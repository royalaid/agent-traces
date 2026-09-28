#!/usr/bin/env python3
"""Capture explicitly selected inactive sessions into a private parity snapshot.

Source SQLite connections are mode=ro and must already use WAL. Backups and
path rewrites happen only in a newly created destination outside the repository.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
import platform
from pathlib import Path
import runpy
import shutil
import sqlite3

ROOT = Path(__file__).resolve().parents[2]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--home", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--id", action="append", required=True)
    opts = ap.parse_args()
    source, dest = opts.home.resolve(), opts.output.resolve()
    if dest.is_relative_to(source / ".codex") or dest.is_relative_to(source / ".claude") or dest.is_relative_to(ROOT):
        ap.error("snapshot destination must be outside the repository and source stores")
    if dest.exists():
        ap.error("snapshot destination must not exist")
    ref = runpy.run_path(str(ROOT / "tests/reference/agent_traces.py"), run_name="snapshot_reference")
    ns = ref["resolve_any"].__globals__
    ns.update(HOME=str(source), T3_DB=str(source / ".t3/userdata/state.sqlite"))
    # Discovery has a metadata cache; never write it to the source home.
    ns["load_cache"] = lambda _: {}
    ns["save_cache"] = lambda *_: None
    original_ro = ns["ro_connect"]
    def readonly_wal(path):
        con = original_ro(path)
        if con.execute("pragma journal_mode").fetchone()[0].lower() != "wal":
            con.close()
            raise RuntimeError("source is not WAL: " + str(path))
        return con
    ns["ro_connect"] = readonly_wal
    rows = []
    for ident in opts.id:
        matches = ref["resolve_any"](ident)
        if not matches:
            raise RuntimeError("no match for " + ident)
        rows.extend(matches)
    rows = list({(r["harness"], r["id"], r["home"]): r for r in rows}.values())
    for row in rows:
        store = str(row["home"])
        store_root = source / store[2:] if store.startswith(("~/", "~\\")) else Path(store)
        if dest.is_relative_to(store_root.resolve()):
            ap.error("snapshot destination is inside a source store")
    # Liveness is not inferred here. Callers must choose known inactive sessions.
    dest.mkdir(parents=True)
    stamps = {}
    copies = {}
    def local(path):
        value = str(path)
        if value.startswith("\\\\?\\"):
            value = value[4:]
        return source / value[2:] if value.startswith(("~/", "~\\")) else Path(value)
    def copy_file(path):
        path = local(path).resolve()
        rel = path.relative_to(source)
        target = dest / rel
        st = path.stat()
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(path, target)
        if hashlib.sha256(target.read_bytes()).hexdigest() != digest:
            raise RuntimeError("source changed during capture: " + str(path))
        stamps[str(path)] = (st.st_size, st.st_mtime_ns, digest)
        return target
    for row in rows:
        harness = row["harness"]
        if harness in ("claude", "codex", "grok") and row.get("path"):
            target = copy_file(row["path"])
            copies[row["path"]] = str(target)
            if harness == "grok":
                summary = local(row["path"]).with_name("summary.json")
                if summary.exists():
                    copy_file(summary)
        if harness == "codex":
            db = local(row["home"]) / "state_5.sqlite"
        elif harness == "t3":
            db = source / ".t3/userdata/state.sqlite"
        elif harness == "opencode":
            db = local(row["home"]) / "opencode.db"
        else:
            continue
        key = str(db.resolve())
        if key in copies:
            continue
        target = dest / db.resolve().relative_to(source)
        target.parent.mkdir(parents=True, exist_ok=True)
        with readonly_wal(str(db)) as src, sqlite3.connect(target) as out:
            ids = [r["id"] for r in rows if r["harness"] == harness]
            placeholders = ','.join('?' for _ in ids)
            if harness == "t3":
                # T3 can be gigabytes. Copy one coherent selected relational slice,
                # including original full table shapes, under a single read snapshot.
                src.execute("begin")
                selected = {
                    "projection_threads": f"thread_id in ({placeholders})",
                    "projection_projects": f"project_id in (select project_id from projection_threads where thread_id in ({placeholders}))",
                    "provider_session_runtime": f"thread_id in ({placeholders})",
                    "projection_thread_messages": f"thread_id in ({placeholders})",
                    "projection_thread_activities": f"thread_id in ({placeholders})",
                }
                for table, condition in selected.items():
                    schema = src.execute("select sql from sqlite_master where type='table' and name=?", (table,)).fetchone()
                    if not schema:
                        raise RuntimeError("missing T3 table: " + table)
                    out.execute(schema[0])
                    for record in src.execute(f"select * from {table} where {condition}", ids):
                        out.execute(f"insert into {table} values ({','.join('?' for _ in record)})", tuple(record))
                out.commit()
                src.rollback()
            else:
                src.backup(out)
            out.execute("pragma journal_mode=WAL")
            table, column = {"codex": ("threads", "id"), "t3": ("projection_threads", "thread_id"), "opencode": ("session_v2", "id")}[harness]
            out.execute(f"delete from {table} where {column} not in ({','.join('?' for _ in ids)})", ids)
        copies[key] = str(target)
    # All captured rollouts now exist, including rows encountered after a backup.
    for key, value in copies.items():
        if key.endswith("state_5.sqlite"):
            with sqlite3.connect(value) as con:
                for original, target in copies.items():
                    if original.endswith(".jsonl"):
                        con.execute("update threads set rollout_path=? where rollout_path=?", (target, original))
    settings = source / ".t3/userdata/settings.json"
    if settings.exists():
        config = json.loads(settings.read_text(encoding="utf-8"))
        for instance in (config.get("providerInstances") or {}).values():
            cfg = instance.get("config") or {}
            if cfg.get("homePath"):
                cfg["homePath"] = str(dest / local(cfg["homePath"]).resolve().relative_to(source))
        target = dest / ".t3/userdata/settings.json"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(config), encoding="utf-8")
    for path, (size, mtime, digest) in stamps.items():
        current = Path(path).stat()
        if (size, mtime) != (current.st_size, current.st_mtime_ns) or hashlib.sha256(Path(path).read_bytes()).hexdigest() != digest:
            raise RuntimeError("source changed; discard snapshot: " + path)
    manifest = {"captured_at": datetime.now(timezone.utc).isoformat(), "source_host": platform.node(),
                "ids": [{"harness": r["harness"], "id": r["id"]} for r in rows], "transcripts": stamps,
                "note": "Private selected corpus; SQLite backups are consistent individually, not a global transaction. T3 uses a selected logical snapshot under one read transaction.",
                "t3_tables": ["projection_threads", "projection_projects", "provider_session_runtime", "projection_thread_messages", "projection_thread_activities"]}
    (dest / "snapshot.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(dest)


if __name__ == "__main__":
    main()
