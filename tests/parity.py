#!/usr/bin/env python3
"""Run reference and Rust on disposable fixtures; requires no Python packages."""
import argparse
from contextlib import nullcontext
import difflib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("corpus", ROOT / "tests/fixtures/corpus.py")
corpus = importlib.util.module_from_spec(spec)
spec.loader.exec_module(corpus)


def environment(home):
    env = dict(os.environ)
    for key in ("CLAUDE_CONFIG_DIR", "CODEX_HOME", "XDG_DATA_HOME", "CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "CODEX_SESSION_ID"):
        env.pop(key, None)
    env.update(AGENT_TRACES_HOME=str(home), HOME=str(home), USERPROFILE=str(home),
               AGENT_TRACES_CACHE=str(home / "cache"), AGENT_TRACES_OUT=str(home / "out"),
               AGENT_TRACES_NOW=corpus.NOW, AGENT_TRACES_HOST_ID="fixture-host",
               AGENT_TRACES_PROCESS_SNAPSHOT=str(home / "processes.json"),
               PYTHONIOENCODING="utf-8", PYTHONUTF8="1", TZ="UTC")
    return env


def reference(home, args):
    # Load a frozen reference without editing its source or invoking __main__.
    mod = runpy.run_path(str(ROOT / "tests/reference/agent_traces.py"), run_name="parity_reference")
    globals_ = mod["main"].__globals__
    globals_.update(HOME=str(home), T3_DB=str(home / ".t3/userdata/state.sqlite"),
                    CACHE_DIR=str(home / "cache"), OUT_DIR=str(home / "out"))
    class FrozenDatetime(datetime):
        @classmethod
        def now(cls, tz=None):
            value = datetime.fromisoformat(os.environ.get("AGENT_TRACES_NOW", corpus.NOW).replace("Z", "+00:00"))
            return value.astimezone(tz) if tz else value.replace(tzinfo=None)
    globals_["datetime"] = FrozenDatetime
    def no_process_access(*_):
        raise ProcessLookupError("fixture inventory contains no process")
    # Never call os.kill(pid, 0), including on Windows where its meaning differs.
    os.kill = no_process_access
    globals_["pid_matches"] = lambda _: False
    return mod["main"](args)


def normalize(value, home, field=None):
    if isinstance(value, dict):
        return {k: normalize(v, home, k) for k, v in value.items()}
    if isinstance(value, list):
        return [normalize(v, home) for v in value]
    if isinstance(value, str):
        value = value.replace(str(home), "<HOME>")
        # Only equivalent timestamp spelling; no dropped keys or reordered lists.
        if field in {"started", "updated", "ts", "observed_at", "process_started", "last_activity"} and len(value) >= 20 and value[4:5] == "-" and "T" in value:
            try:
                return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(timezone.utc).isoformat()
            except ValueError:
                pass
    return value


def snapshot(home):
    return {str(p.relative_to(home)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in home.rglob("*") if p.is_file()
            and p.relative_to(home).parts[0].startswith(".")
            and not p.name.endswith(("-shm", "-wal"))}


def invoke(cmd, env):
    return subprocess.run(cmd, env=env, capture_output=True, text=True, encoding="utf-8", timeout=60)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary", type=Path)
    ap.add_argument("--reference-only", action="store_true")
    ap.add_argument("--schema", action="store_true", help="validate v1 with optional jsonschema dev package")
    ap.add_argument("--case", help="run cases whose command contains this substring")
    ap.add_argument("--snapshot", type=Path, help="private corpus from tests/fixtures/capture.py")
    ap.add_argument("--query", default="mailbox", help="search term for a private snapshot")
    opts = ap.parse_args()
    binary = (opts.binary or ROOT / "target/debug" / ("agent-traces.exe" if os.name == "nt" else "agent-traces")).resolve()
    resource = nullcontext(str(opts.snapshot.resolve())) if opts.snapshot else tempfile.TemporaryDirectory(prefix="agent-traces-parity-")
    with resource as temp:
        home = Path(temp)
        manifest = json.loads((home / "snapshot.json").read_text(encoding="utf-8")) if opts.snapshot else None
        ids = {r["harness"]: r["id"] for r in manifest["ids"]} if manifest else corpus.create(home)
        identity = ids.get("claude") or next(iter(ids.values()))
        (home / "processes.json").write_text("[]", encoding="utf-8")
        env = environment(home)
        if manifest:
            env["AGENT_TRACES_NOW"] = manifest["captured_at"]
        before = snapshot(home)
        cases = [(["where"], False), (["ls", "--all", "--json"], True), (["ls", "--all", "--subagents", "--json"], True),
                 (["find", "mailbox", "--json", "--include-self"], True),
                 (["find", "mailbox", "routing", "--json", "--include-self", "--sort", "oldest"], True),
                 (["find", "absent-needle-xyz", "--json"], True),
                 (["live"], False), (["me"], False),
                 (["touched", "router.py", "--since", "2026-09-01"], False),
                 (["failures", "--min", "1", "--since", "2026-09-01"], False)]
        cases += [
            (["find", "mailbox", "--include-self", "--hits", "-1"], False),
            (["find", "mailbox", "--include-self", "--hits", "-100"], False),
            (["touched", "router.py", "--since", "2026-09-01", "--limit", "-1"], False),
            (["touched", "router.py", "--since", "2026-09-01", "--limit", "-100"], False),
            (["failures", "--min", "-1", "--since", "2026-09-01", "--limit", "-1", "--json"], True),
            (["failures", "--min", "-1", "--since", "2026-09-01", "--limit", "-100"], False),
            (["failures", "--min", "-1", "--since", "2026-09-01", "--json"], True),
            (["show", identity, "--tail", "-1", "--full-stdout"], False),
            (["show", identity, "--tail", "-1", "--grep", "mailbox", "--full-stdout"], False),
        ]
        for ident in ids.values():
            cases += [(["resolve", ident, "--json"], True),
                      (["show", ident, "--full-stdout", "--results"], False),
                      (["tree", ident], False)]
        cases += [(["resolve", "no-such-session", "--json"], True),
                  (["handoff", identity, "--output", str(home / "handoff.md")], False)]
        if manifest:
            cases = [(["where"], False), (["ls", "--all", "--json"], True),
                     (["find", opts.query, "--since", "2000-01-01", "--include-self", "--json"], True)]
            for row in manifest["ids"]:
                cases.extend([(["resolve", row["id"], "--json"], True),
                              (["show", row["id"], "--tail", "3", "--full-stdout", "--results"], False)])
        failed = checked = 0
        for args, is_json in cases:
            label = " ".join(args)
            if opts.case and opts.case not in label:
                continue
            checked += 1
            case_env = dict(env, CLAUDE_CODE_SESSION_ID=identity) if args[0] == "me" else env
            expected = invoke([sys.executable, str(Path(__file__).resolve()), "--reference", str(home), *args], case_env)
            if "Traceback" in expected.stderr:
                raise RuntimeError(expected.stderr)
            if manifest and args[0] == "find" and (expected.returncode != 0 or not expected.stdout.strip()):
                raise AssertionError("choose --query that matches the private corpus; an empty search does not verify ranking")
            if opts.reference_only:
                if not manifest and args[0] == "ls" and len(expected.stdout.splitlines()) != 4:
                    raise AssertionError("fixture listing must have four sessions after T3/provider merge")
                print("REFERENCE", label, "exit", expected.returncode)
                continue
            artifact = (home / "handoff.md").read_text(encoding="utf-8") if args[0] == "handoff" else None
            if artifact is not None:
                (home / "handoff.md").unlink()
            actual = invoke([str(binary), *args], case_env)
            if is_json:
                left = [normalize(json.loads(s), home) for s in expected.stdout.splitlines()]
                right = [normalize(json.loads(s), home) for s in actual.stdout.splitlines()]
                left, right = [json.dumps(v, indent=2, ensure_ascii=False, sort_keys=True) for v in (left, right)]
            elif artifact is not None:
                left = normalize(artifact, home)
                right = normalize((home / "handoff.md").read_text(encoding="utf-8"), home)
            else:
                left, right = normalize(expected.stdout, home), normalize(actual.stdout, home)
            if left != right or expected.returncode != actual.returncode:
                failed += 1
                print("FAIL", label, "exit", expected.returncode, actual.returncode)
                print("".join(difflib.unified_diff(left.splitlines(True), right.splitlines(True), fromfile="python", tofile="rust")))
                print(actual.stderr)
            else:
                print("PASS", label)
        if not opts.reference_only:
            # CLI and stores work with no Python, rg, or other programs on PATH.
            clean_env = dict(env, PATH="")
            for smoke_args, _ in cases:
                smoke_env = dict(clean_env, CLAUDE_CODE_SESSION_ID=identity) if smoke_args[0] == "me" else clean_env
                smoke = invoke([str(binary), *smoke_args], smoke_env)
                if smoke.returncode not in (0, 1):
                    raise AssertionError("self-contained executable smoke failed: " + repr(smoke_args) + smoke.stderr)
            if opts.schema:
                import jsonschema
                schema = json.loads((ROOT / "schemas/cli-v1.schema.json").read_text(encoding="utf-8"))
                identity = next(iter(ids.values()))
                for args in (["ls", "--all"], ["find", opts.query], ["live"], ["resolve", identity], ["me"], ["handoff", identity]):
                    result = invoke([str(binary), *args, "--json", "--schema-version", "1"], dict(env, CLAUDE_CODE_SESSION_ID=identity))
                    envelope = json.loads(result.stdout)
                    jsonschema.Draft202012Validator(schema, format_checker=jsonschema.FormatChecker()).validate(envelope)
                    expected_exits = {"ok": 0, "not_found": 1, "error": 2, "partial": 3, "ambiguous": 4}
                    if result.returncode != expected_exits[envelope["status"]]:
                        raise AssertionError("v1 exit/status mismatch: " + args[0])
                    if not manifest:
                        if envelope["status"] not in ("ok", "partial"):
                            raise AssertionError("known fixture must return useful data: " + args[0])
                        if envelope["status"] == "partial":
                            codes = {d["code"] for d in envelope["diagnostics"]}
                            if not codes or codes - {"malformed_record"}:
                                raise AssertionError("unexpected fixture partial failure: " + repr(codes))
                        data = envelope["result"]
                        if args[0] == "live":
                            observations = data["observations"]
                            if envelope["status"] != "ok" or len(observations) != 2:
                                raise AssertionError("fixture has exactly two running T3 observations")
                            observed = {(o["identity"]["id"], o["evidence"], o["observed_status"]) for o in observations}
                            expected = {(i, "t3_runtime", "running") for i in [ids["t3"], "thread-second-fixture"]}
                            if observed != expected:
                                raise AssertionError("wrong live identities/evidence/status")
                        elif args[0] == "me":
                            if data["identifiers"] != [{"environment_variable": "CLAUDE_CODE_SESSION_ID", "id": identity, "resolved": True}]:
                                raise AssertionError("me did not resolve the supplied environment identity")
                            if [s["identity"]["id"] for s in data["sessions"]] != [identity]:
                                raise AssertionError("me returned the wrong session")
                        elif args[0] == "ls" and data["total"] != 4:
                            raise AssertionError("v1 fixture listing must have four merged sessions")
                with tempfile.TemporaryDirectory(prefix="agent-traces-empty-") as empty:
                    empty_home = Path(empty)
                    (empty_home / "processes.json").write_text("[]", encoding="utf-8")
                    empty_env = environment(empty_home)
                    for command in (["ls", "--all"], ["live"], ["me"], ["resolve", "absent-session"]):
                        result = invoke([str(binary), *command, "--json", "--schema-version", "1"], empty_env)
                        envelope = json.loads(result.stdout)
                        jsonschema.Draft202012Validator(schema, format_checker=jsonschema.FormatChecker()).validate(envelope)
                        expected = (0, "ok") if command[0] == "ls" else (1, "not_found")
                        if (result.returncode, envelope["status"]) != expected or envelope["complete"] is not True:
                            raise AssertionError("empty-store status/exit regression: " + command[0])
                    empty_env.pop("AGENT_TRACES_HOST_ID")
                    result = invoke([str(binary), "ls", "--json", "--schema-version", "1"], empty_env)
                    if result.returncode != 2 or result.stdout:
                        raise AssertionError("v1 must reject a missing host ID before emitting an envelope")
        if before != snapshot(home):
            raise AssertionError("command mutated fixture store content")
        print(f"{checked} cases, {failed} failures")
        return bool(failed)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--reference":
        sys.exit(reference(Path(sys.argv[2]), sys.argv[3:]))
    sys.exit(main())
