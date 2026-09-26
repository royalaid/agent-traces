#!/usr/bin/env python3
"""agent-traces: find, read, and hand off coding-agent sessions on this machine.

Covers Claude Code (every config dir), Codex (every CODEX_HOME), T3 Code
threads (mapped to their provider sessions), OpenCode, and Grok Build.
Read-only: SQLite stores are opened with mode=ro, JSONL is parsed line by
line and tolerates corrupt lines.

Subcommands:
  where               every store, its size, freshness, and index of record
  ls                  list sessions (default: last 7 days, top-level only)
  find TERM...        search real user prompts (or --in assistant|tools|all)
  show ID             render one session; large output goes to a file
  resolve ID          which harness/file owns an id or id prefix
  touched PATH        which sessions wrote a file, oldest first
  failures            rank recent sessions by errors, aborts, and user corrections
  tree ID             subagents / child sessions of a session
  handoff ID          write a handoff draft for continuing a session elsewhere
  live                sessions running right now
  me                  identify the session this command runs inside

IDs can be full or a prefix of 6+ characters: Claude session UUIDs, Codex
thread ids, T3 thread ids, OpenCode ses_ ids, Grok session ids.
"""
import argparse
import glob
import json
import math
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import urllib.parse
from datetime import datetime, timedelta, timezone

HOME = os.path.expanduser("~")
CACHE_DIR = os.environ.get("AGENT_TRACES_CACHE", os.path.join(HOME, ".cache", "agent-traces"))
OUT_DIR = os.environ.get(
    "AGENT_TRACES_OUT", os.path.join(os.environ.get("TMPDIR", "/tmp").rstrip("/"), "agent-traces")
)
SHOW_BUDGET = 20000  # characters printed by `show` before spilling to a file
STALE_DAYS = 14

try:
    sys.stdout.reconfigure(errors="replace")
    sys.stderr.reconfigure(errors="replace")
except Exception:
    pass


# ---------------------------------------------------------------- utilities

def note(msg):
    print(msg, file=sys.stderr)


def tilde(path):
    if not path:
        return ""
    return "~" + path[len(HOME):] if path.startswith(HOME) else path


def parse_ts(value):
    """ISO string, epoch seconds, or epoch ms -> aware UTC datetime (or None)."""
    if value is None or value == "":
        return None
    if isinstance(value, (int, float)):
        v = float(value)
        if v > 1e12:
            v /= 1000.0
        try:
            return datetime.fromtimestamp(v, tz=timezone.utc)
        except (OverflowError, OSError, ValueError):
            return None
    s = str(value).strip()
    if re.fullmatch(r"\d{10,13}(\.\d+)?", s):
        return parse_ts(float(s))
    try:
        dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def fmt_ts(dt):
    return dt.astimezone().strftime("%Y-%m-%d %H:%M") if dt else "????-??-?? ??:??"


def parse_since(text):
    if not text:
        return None
    m = re.fullmatch(r"(\d+)([mhdw])", text.strip())
    if m:
        n, unit = int(m.group(1)), m.group(2)
        delta = {"m": timedelta(minutes=n), "h": timedelta(hours=n),
                 "d": timedelta(days=n), "w": timedelta(weeks=n)}[unit]
        return datetime.now(timezone.utc) - delta
    dt = parse_ts(text)
    if dt is None:
        raise SystemExit("cannot parse time %r; use 7d, 36h, 90m, or 2026-09-01" % text)
    if len(text.strip()) == 10:  # bare date means local midnight
        dt = datetime.fromisoformat(text.strip()).astimezone()
    return dt


def one_line(text, n):
    text = re.sub(r"\s+", " ", text or "").strip()
    return text if len(text) <= n else text[: n - 1] + "\u2026"


def clip(text, n):
    text = (text or "").strip()
    if len(text) <= n:
        return text
    return text[:n].rstrip() + "\n[\u2026 %d more chars]" % (len(text) - n)


_SECRET_PATTERNS = [
    re.compile(r"sk-(?:ant-)?[A-Za-z0-9_\-]{20,}"),
    re.compile(r"gh[pousr]_[A-Za-z0-9]{30,}"),
    re.compile(r"glpat[-][A-Za-z0-9_\-]{20,}"),  # [-] keeps the literal token prefix out of leak audits
    re.compile(r"xox[abprs]-[A-Za-z0-9\-]{10,}"),
    re.compile(r"AKIA[0-9A-Z]{16}"),
    re.compile(r"(?i)(bearer\s+)[A-Za-z0-9._\-]{20,}"),
    re.compile(r"eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}"),
    re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"),
]


def redact(text):
    if not text:
        return text
    for pat in _SECRET_PATTERNS:
        text = pat.sub(lambda m: (m.group(1) if m.groups() else "") + "<redacted>", text)
    return text


def ro_connect(path):
    con = sqlite3.connect("file:%s?mode=ro" % urllib.parse.quote(path), uri=True, timeout=10)
    con.row_factory = sqlite3.Row
    return con


def table_exists(con, name):
    return con.execute("select 1 from sqlite_master where type='table' and name=?", (name,)).fetchone() is not None


SKIPPED = [0]  # lines passed over by a needle filter, for search statistics


def jsonl(path, start=0, needles=None):
    """Yield (line_no, obj) for every parseable line; corrupt lines are skipped.
    With needles (lowercase bytes), lines containing none of them are not parsed."""
    try:
        f = open(path, "rb")
    except OSError:
        return
    with f:
        for i, raw in enumerate(f):
            if i < start:
                continue
            if needles and not any(n in raw.lower() for n in needles):
                SKIPPED[0] += 1
                continue
            try:
                yield i, json.loads(raw.decode("utf-8", "replace"), strict=False)
            except ValueError:
                continue


def tail_lines(path, nbytes=262144):
    try:
        size = os.path.getsize(path)
        with open(path, "rb") as f:
            f.seek(max(0, size - nbytes))
            data = f.read().decode("utf-8", "replace").splitlines()
    except OSError:
        return []
    if size > nbytes and data:
        data = data[1:]
    out = []
    for line in data:
        try:
            out.append(json.loads(line, strict=False))
        except ValueError:
            continue
    return out


def files_containing(term, files):
    """Subset of files containing term (case-insensitive, literal). Uses rg when present."""
    files = [f for f in files if os.path.exists(f)]
    if not files:
        return set()
    rg = shutil.which("rg")
    if rg:
        hits = set()
        for i in range(0, len(files), 400):
            chunk = files[i:i + 400]
            proc = subprocess.run([rg, "-l", "-i", "-F", "--no-messages", "-e", term, "--"] + chunk,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            if proc.returncode not in (0, 1):
                break  # fall back to the Python scan below
            hits.update(p for p in proc.stdout.decode("utf-8", "replace").splitlines() if p)
        else:
            return hits
    needle = term.lower().encode()
    hits = set()
    for f in files:
        try:
            with open(f, "rb") as fh:
                for raw in fh:
                    if needle in raw.lower():
                        hits.add(f)
                        break
        except OSError:
            pass
    return hits


def load_cache(name):
    try:
        with open(os.path.join(CACHE_DIR, name + ".json")) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def save_cache(name, data):
    try:
        os.makedirs(CACHE_DIR, exist_ok=True)
        tmp = os.path.join(CACHE_DIR, name + ".json.tmp")
        with open(tmp, "w") as f:
            json.dump(data, f)
        os.replace(tmp, os.path.join(CACHE_DIR, name + ".json"))
    except OSError:
        pass


def realpath_key(path):
    try:
        return os.path.realpath(path)
    except OSError:
        return path


def newest_mtime(paths):
    best = None
    for p in paths:
        try:
            m = os.path.getmtime(p)
        except OSError:
            continue
        best = m if best is None or m > best else best
    return parse_ts(best) if best else None


def self_ids():
    ids = set()
    for key, value in os.environ.items():
        if key in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "CODEX_SESSION_ID") and value:
            ids.add(value)
    return ids


def t3_settings():
    try:
        with open(os.path.join(HOME, ".t3", "userdata", "settings.json")) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def t3_home_paths(driver):
    out = []
    for inst in (t3_settings().get("providerInstances") or {}).values():
        if inst.get("driver") == driver:
            home = ((inst.get("config") or {}).get("homePath") or "").strip()
            if home:
                out.append(os.path.expanduser(home))
    return out


# ------------------------------------------------------------ event model
# An event is a dict: {ts, role, text, name, input, error, kind}
# role: prompt | command | assistant | tool | result | notice | system


def ev(role, text="", ts=None, **kw):
    d = {"role": role, "text": text or "", "ts": ts}
    d.update(kw)
    return d


_JS_TOOL_RE = re.compile(r"tools\.([A-Za-z_$][\w$]*)\(")
_JS_CMD_RE = re.compile(r"""\b(?:cmd|command)\s*:\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`[^`]*`)""")


def code_mode_summary(src):
    """Codex code mode wraps tool calls in JavaScript; name the inner tools and first command."""
    names = []
    for n in _JS_TOOL_RE.findall(src):
        if n not in names:
            names.append(n)
    m = _JS_CMD_RE.search(src)
    cmd = ""
    if m:
        cmd = m.group(1)[1:-1].encode("utf-8", "replace").decode("unicode_escape", "replace")
    return one_line(("[%s] " % ",".join(names[:4]) if names else "") + (cmd or src), 160)


def tool_summary(name, inp):
    """One-line description of a tool call from its input."""
    if isinstance(inp, str) and "tools." in inp:
        return code_mode_summary(inp)
    if isinstance(inp, str):
        try:
            inp = json.loads(inp)
        except ValueError:
            return one_line(inp, 160)
    if not isinstance(inp, dict):
        return one_line(json.dumps(inp), 160) if inp is not None else ""
    for key in ("command", "cmd", "file_path", "path", "filePath", "pattern", "query", "url",
                "description", "prompt", "code", "input"):
        if key in inp and inp[key]:
            val = inp[key]
            if isinstance(val, list):
                val = " ".join(str(v) for v in val)
            label = "" if key in ("command", "cmd", "code", "input") else key + "="
            extra = ""
            if key == "description" and inp.get("subagent_type"):
                extra = " [%s]" % inp["subagent_type"]
            return one_line(label + str(val), 160) + extra
    return one_line(json.dumps(inp, ensure_ascii=False), 160)


def touched_paths(name, inp):
    """File paths a tool call wrote, for handoffs."""
    out = []
    if isinstance(inp, str):
        try:
            inp = json.loads(inp)
        except ValueError:
            inp = {"input": inp}
    if not isinstance(inp, dict):
        return out
    lname = (name or "").lower()
    if lname in ("write", "edit", "multiedit", "notebookedit", "str_replace_based_edit_tool", "create_file",
                 "edit_file", "write_file", "apply_patch", "patch"):
        for key in ("file_path", "path", "filePath", "notebook_path"):
            if inp.get(key):
                out.append(str(inp[key]))
    patch = inp.get("input") or inp.get("patch") or inp.get("code") or ""
    if isinstance(patch, str) and "*** " in patch:
        for p in re.findall(r"\*\*\* (?:Add|Update|Delete) File: ([^\n\\\"'`]+)", patch):
            if p.strip() not in out:
                out.append(p.strip())
    return out


# ------------------------------------------------------------ Claude Code

_CLAUDE_NOTICE_PREFIXES = (
    "<task-notification>", "<local-command-stdout>", "<local-command-stderr>", "<local-command-caveat>",
    "<bash-stdout>", "<bash-stderr>", "Caveat: The messages below", "[Request interrupted",
    "Another Claude session sent a message", "[SYSTEM NOTIFICATION", "<user-prompt-submit-hook>",
    "<system-reminder>", "<cross-session-message", "<agent-message",
)
_REMINDER_RE = re.compile(r"<(system-reminder|context_window_protection|user-prompt-submit-hook)>[\s\S]*?</\1>")


def classify_claude_text(text, rec):
    """Return (role, text) for user-side text in a Claude record."""
    raw = text or ""
    text = _REMINDER_RE.sub("", raw).strip()
    if not text:
        return "notice", one_line(raw, 200)
    if rec.get("isCompactSummary") or text.startswith("This session is being continued from a previous"):
        return "system", "[compaction summary] " + text
    if text.startswith("<command-name>") or "<command-name>" in text[:200]:
        name = re.search(r"<command-name>(.*?)</command-name>", text, re.S)
        args = re.search(r"<command-args>(.*?)</command-args>", text, re.S)
        cmd = (name.group(1).strip() if name else "/?")
        if not cmd.startswith("/"):
            cmd = "/" + cmd
        return "command", (cmd + " " + (args.group(1).strip() if args else "")).strip()
    if text.startswith("<bash-input>"):
        m = re.search(r"<bash-input>(.*?)</bash-input>", text, re.S)
        return "command", "!" + (m.group(1).strip() if m else "")
    if text.startswith(_CLAUDE_NOTICE_PREFIXES) or rec.get("promptSource") == "system" or rec.get("isMeta"):
        return "notice", text
    return "prompt", text


def claude_events(path, needles=None):
    seen = set()
    for _, rec in jsonl(path, needles=needles):
        uid = rec.get("uuid")
        if uid:
            if uid in seen:
                continue
            seen.add(uid)
        ts = parse_ts(rec.get("timestamp"))
        typ = rec.get("type")
        if typ == "user":
            content = (rec.get("message") or {}).get("content")
            if isinstance(content, str):
                role, text = classify_claude_text(content, rec)
                yield ev(role, text, ts)
                continue
            if not isinstance(content, list):
                continue
            texts = []
            for block in content:
                if not isinstance(block, dict):
                    continue
                btype = block.get("type")
                if btype == "tool_result":
                    body = block.get("content")
                    if isinstance(body, list):
                        body = "\n".join(b.get("text", "") for b in body if isinstance(b, dict) and b.get("type") == "text")
                    yield ev("result", str(body or ""), ts, error=bool(block.get("is_error")),
                             tool_use_id=block.get("tool_use_id"))
                elif btype == "text":
                    texts.append(block.get("text", ""))
                elif btype == "image":
                    texts.append("[image]")
            if texts:
                role, text = classify_claude_text("\n".join(texts), rec)
                yield ev(role, text, ts)
        elif typ == "assistant":
            msg = rec.get("message") or {}
            for block in msg.get("content") or []:
                if not isinstance(block, dict):
                    continue
                if block.get("type") == "text" and block.get("text", "").strip():
                    yield ev("assistant", block["text"], ts, model=msg.get("model"))
                elif block.get("type") == "tool_use":
                    yield ev("tool", "", ts, name=block.get("name"), input=block.get("input"), id=block.get("id"))
            if rec.get("isApiErrorMessage") or rec.get("error"):
                yield ev("system", "[api error] " + one_line(json.dumps(rec.get("error") or ""), 300), ts, error=True)
        elif typ == "system":
            sub = rec.get("subtype") or ""
            if sub == "compact_boundary":
                meta = rec.get("compactMetadata") or {}
                yield ev("system", "[compacted: %s, %s tokens before]" % (meta.get("trigger"), meta.get("preTokens")), ts)
            elif rec.get("level") == "error":
                yield ev("system", "[error] " + one_line(rec.get("content") or sub, 300), ts, error=True)


def claude_homes():
    cands = [os.environ.get("CLAUDE_CONFIG_DIR", ""), os.path.join(HOME, ".claude")]
    cands += [p for p in glob.glob(os.path.join(HOME, ".claude*")) if os.path.isdir(p)]
    cands += [os.path.dirname(p) for p in glob.glob(os.path.join(HOME, ".claude*", "*", "config", "projects"))]
    cands += t3_home_paths("claudeAgent")
    seen, out = set(), []
    for c in cands:
        if c and os.path.isdir(os.path.join(c, "projects")):
            key = realpath_key(c)
            if key not in seen:
                seen.add(key)
                out.append(c)
    return out


class Claude:
    name = "claude"

    def __init__(self):
        self.homes = claude_homes()
        self._cache = None

    def top_files(self, home):
        return glob.glob(os.path.join(glob.escape(home), "projects", "*", "*.jsonl"))

    def sub_files(self, home):
        base = os.path.join(glob.escape(home), "projects", "*", "*", "subagents")
        # Agent tool subagents, plus Workflow tool agents one level deeper
        return glob.glob(os.path.join(base, "agent-*.jsonl")) + glob.glob(os.path.join(base, "workflows", "*", "agent-*.jsonl"))

    def cache(self):
        if self._cache is None:
            self._cache = load_cache("claude-meta")
        return self._cache

    def meta(self, path, home, sub=False):
        try:
            st = os.stat(path)
        except OSError:
            return None
        c = self.cache().get(path)
        if c and c.get("size") == st.st_size and c.get("mtime") == st.st_mtime:
            m = c["meta"]
        else:
            m = self._extract(path, st)
            self.cache()[path] = {"size": st.st_size, "mtime": st.st_mtime, "meta": m}
            self._dirty = True
        if sub:
            sid = os.path.basename(path.split(os.sep + "subagents" + os.sep)[0])
            workflow = os.path.basename(os.path.dirname(path)) if (os.sep + "workflows" + os.sep) in path else None
            agent = os.path.basename(path)[len("agent-"):-len(".jsonl")]
            info = {}
            try:
                with open(path[: -len(".jsonl")] + ".meta.json") as f:
                    info = json.load(f)
            except (OSError, ValueError):
                pass
            title = info.get("name") or info.get("description") or m.get("title") or ""
            if info.get("description") and info.get("name"):
                title = "%s: %s" % (info["name"], info["description"])
            if workflow and not (info.get("name") or info.get("description")):
                title = "%s: %s" % (workflow, one_line(m.get("first_prompt") or "", 90))
            return self._row(m, home, path, agent, parent=sid, kind="workflow-agent" if workflow else "subagent",
                             title=title, agent_type=info.get("agentType"))
        sid = os.path.basename(path)[:-len(".jsonl")]
        return self._row(m, home, path, sid)

    def _row(self, m, home, path, sid, parent=None, kind="main", title=None, agent_type=None):
        return {
            "harness": "claude", "home": tilde(home), "id": sid, "parent": parent, "kind": kind,
            "cwd": m.get("cwd"), "branch": m.get("branch"), "title": title if title is not None else m.get("title"),
            "first_prompt": m.get("first_prompt"), "started": m.get("started"), "updated": m.get("updated"),
            "model": m.get("model"), "entrypoint": m.get("entrypoint"), "path": path, "agent_type": agent_type,
        }

    def _extract(self, path, st):
        m = {"cwd": None, "branch": None, "title": None, "first_prompt": None, "started": None,
             "updated": None, "model": None, "entrypoint": None}
        ai_title = custom = None
        for i, rec in jsonl(path):
            if not m["cwd"] and rec.get("cwd"):
                m["cwd"], m["branch"] = rec.get("cwd"), rec.get("gitBranch")
                m["entrypoint"] = rec.get("entrypoint")
            if not m["started"] and rec.get("timestamp"):
                m["started"] = rec["timestamp"]
            t = rec.get("type")
            if t == "ai-title":
                ai_title = rec.get("aiTitle")
            elif t == "custom-title":
                custom = rec.get("customTitle")
            elif t == "assistant" and not m["model"]:
                m["model"] = (rec.get("message") or {}).get("model")
            elif t == "user" and not m["first_prompt"]:
                content = (rec.get("message") or {}).get("content")
                if isinstance(content, list):
                    content = "\n".join(b.get("text", "") for b in content
                                        if isinstance(b, dict) and b.get("type") == "text")
                if isinstance(content, str) and content:
                    role, text = classify_claude_text(content, rec)
                    if role in ("prompt", "command"):
                        m["first_prompt"] = text[:600]
            if i > 400 or (m["cwd"] and m["first_prompt"] and m["model"] and (ai_title or custom)):
                break
        for rec in tail_lines(path):
            if rec.get("timestamp"):
                m["updated"] = rec["timestamp"]
            if rec.get("type") == "custom-title":
                custom = rec.get("customTitle")
            elif rec.get("type") == "ai-title" and not ai_title:
                ai_title = rec.get("aiTitle")
        m["title"] = custom or ai_title
        m["updated"] = m["updated"] or datetime.fromtimestamp(st.st_mtime, tz=timezone.utc).isoformat()
        return m

    def flush(self):
        if getattr(self, "_dirty", False):
            save_cache("claude-meta", self.cache())
            self._dirty = False

    @staticmethod
    def dedupe(rows):
        """Homes can hold byte-identical copies of a session (a home split copies projects/).
        Keep the copy with the most data and list the others under `copies`."""
        best = {}
        for r in rows:
            key = (r.get("parent") or "", r["id"])
            try:
                rank = (os.path.getsize(r["path"]), os.path.getmtime(r["path"]))
            except OSError:
                rank = (0, 0)
            cur = best.get(key)
            if cur is None:
                best[key] = (rank, r, [])
            elif rank > cur[0]:
                best[key] = (rank, r, cur[2] + [cur[1]["home"]])
            else:
                cur[2].append(r["home"])
        out = []
        for rank, r, others in best.values():
            if others:
                r["copies"] = sorted(set(others))
            out.append(r)
        return out

    def sessions(self, since=None, subagents=False):
        cutoff = since.timestamp() if since else None
        rows = []
        for home in self.homes:
            files = self.top_files(home) + (self.sub_files(home) if subagents else [])
            for f in files:
                try:
                    if cutoff and os.path.getmtime(f) < cutoff:
                        continue
                except OSError:
                    continue
                row = self.meta(f, home, sub="/subagents/" in f)
                if row:
                    rows.append(row)
        self.flush()
        return self.dedupe(rows)

    def resolve(self, ident):
        out = []
        for home in self.homes:
            pat_top = os.path.join(glob.escape(home), "projects", "*", glob.escape(ident) + "*.jsonl")
            pat_sub = os.path.join(glob.escape(home), "projects", "*", "*", "subagents",
                                   "agent-" + glob.escape(ident) + "*.jsonl")
            pat_wf = os.path.join(glob.escape(home), "projects", "*", "*", "subagents", "workflows", "*",
                                  "agent-" + glob.escape(ident) + "*.jsonl")
            for f in glob.glob(pat_top):
                out.append(self.meta(f, home))
            for f in glob.glob(pat_sub) + glob.glob(pat_wf):
                out.append(self.meta(f, home, sub=True))
        self.flush()
        return self.dedupe([r for r in out if r])

    def events(self, row, needles=None):
        return claude_events(row["path"], needles)

    def children(self, row):
        if row["kind"] != "main":
            return []
        d = row["path"][:-len(".jsonl")]
        home = os.path.expanduser(row["home"])
        out = []
        for f in sorted(glob.glob(os.path.join(glob.escape(d), "subagents", "agent-*.jsonl")) +
                        glob.glob(os.path.join(glob.escape(d), "subagents", "workflows", "*", "agent-*.jsonl"))):
            r = self.meta(f, home, sub=True)
            if r:
                out.append(r)
        self.flush()
        return self.dedupe(out)

    def where(self):
        out = []
        names = {}
        for home in self.homes:
            for f in self.top_files(home):
                names.setdefault(os.path.basename(f), set()).add(home)
        shared = sum(1 for v in names.values() if len(v) > 1)
        for home in self.homes:
            files = self.top_files(home)
            out.append({
                "harness": "claude", "path": tilde(home), "sessions": len(files),
                "newest": newest_mtime(files), "index": "projects/<cwd-slug>/<session>.jsonl (no index; scan files)",
                "notes": "history.jsonl is a typed-prompt log only (misses SDK/T3/subagent prompts)"
                         + ("; %d sessions exist as copies in more than one home (listed once)" % shared if shared else ""),
            })
        return out


# ------------------------------------------------------------------ Codex

_CODEX_BOILERPLATE = ("<", "# AGENTS.md instructions", "# Context from my IDE setup")


def codex_homes():
    cands = [os.environ.get("CODEX_HOME", ""), os.path.join(HOME, ".codex")]
    cands += [p for p in glob.glob(os.path.join(HOME, ".codex*")) if os.path.isdir(p)]
    cands += [p for p in glob.glob(os.path.join(HOME, ".codex*", "*")) if os.path.isdir(p)]
    cands += t3_home_paths("codex")
    seen, out = set(), []
    for c in cands:
        if not c:
            continue
        has_rollouts = bool(glob.glob(os.path.join(glob.escape(c), "sessions", "20[0-9][0-9]")))
        if not (os.path.exists(os.path.join(c, "state_5.sqlite")) or has_rollouts):
            continue
        key = realpath_key(os.path.join(c, "state_5.sqlite")
                           if os.path.exists(os.path.join(c, "state_5.sqlite")) else os.path.join(c, "sessions"))
        if key not in seen:
            seen.add(key)
            out.append(c)
    return out


def codex_payload_text(payload):
    parts = []
    for block in payload.get("content") or []:
        if isinstance(block, dict) and block.get("text"):
            parts.append(block["text"])
    return "\n".join(parts)


def codex_events(path, needles=None):
    for _, rec in jsonl(path, needles=needles):
        ts = parse_ts(rec.get("timestamp"))
        typ = rec.get("type")
        p = rec.get("payload") or {}
        if typ == "response_item":
            ptype = p.get("type")
            if ptype == "message":
                role = p.get("role")
                text = codex_payload_text(p)
                if role == "user":
                    stripped = text.lstrip()
                    if not stripped:
                        continue
                    if stripped.startswith(_CODEX_BOILERPLATE):
                        yield ev("notice", text, ts)
                    else:
                        yield ev("prompt", text, ts)
                elif role == "assistant" and text.strip():
                    yield ev("assistant", text, ts, phase=p.get("phase"))
            elif ptype in ("function_call", "custom_tool_call", "local_shell_call", "web_search_call", "tool_search_call"):
                inp = p.get("arguments") if ptype == "function_call" else p.get("input", p.get("action"))
                yield ev("tool", "", ts, name=p.get("name") or ptype, input=inp, id=p.get("call_id"))
            elif ptype in ("function_call_output", "custom_tool_call_output", "local_shell_call_output"):
                out = p.get("output")
                if isinstance(out, dict):
                    out = out.get("output") or out.get("content") or json.dumps(out)
                elif isinstance(out, list):
                    out = "\n".join(b.get("text", "") for b in out if isinstance(b, dict))
                text = str(out or "")
                err = bool(re.search(r'"exit_code":\s*[1-9]|Process exited with code [1-9]|^error', text[:400]))
                yield ev("result", text, ts, error=err, tool_use_id=p.get("call_id"))
        elif typ == "event_msg":
            ptype = p.get("type")
            if ptype == "turn_aborted":
                yield ev("system", "[turn aborted: %s]" % p.get("reason"), ts, error=True)
            elif ptype in ("error", "stream_error"):
                yield ev("system", "[error] " + one_line(p.get("message") or json.dumps(p), 300), ts, error=True)
        elif typ == "compacted":
            yield ev("system", "[compacted]", ts)


class Codex:
    name = "codex"

    def __init__(self):
        self.homes = codex_homes()

    def _rows(self, home, where="", params=()):
        db = os.path.join(home, "state_5.sqlite")
        if not os.path.exists(db):
            return []
        try:
            con = ro_connect(db)
            rows = con.execute("select * from threads " + where, params).fetchall()
            parents = {}
            if table_exists(con, "thread_spawn_edges"):
                parents = {r["child_thread_id"]: r["parent_thread_id"]
                           for r in con.execute("select parent_thread_id, child_thread_id from thread_spawn_edges")}
            con.close()
        except sqlite3.Error as e:
            note("codex: cannot read %s: %s" % (tilde(db), e))
            return []
        out = []
        for r in rows:
            keys = r.keys()
            source = r["source"] if "source" in keys else ""
            tsrc = r["thread_source"] if "thread_source" in keys else None
            parent = parents.get(r["id"])
            is_sub = bool(parent) or (tsrc not in (None, "", "user")) or ("subagent" in str(source))
            name = r["name"] if "name" in keys else None
            if not name and is_sub and "agent_nickname" in keys and r["agent_nickname"]:
                role = r["agent_role"] if "agent_role" in keys and r["agent_role"] else "subagent"
                name = "%s (%s)" % (r["agent_nickname"], role)
            out.append({
                "harness": "codex", "home": tilde(home), "id": r["id"], "parent": parent,
                "kind": "subagent" if is_sub else ("worker" if source == "exec" else "main"),
                "cwd": r["cwd"], "branch": r["git_branch"] if "git_branch" in keys else None,
                "title": name or None, "first_prompt": (r["first_user_message"] or "")[:600]
                if "first_user_message" in keys else None,
                "started": parse_ts(r["created_at"]).isoformat() if r["created_at"] else None,
                "updated": parse_ts(r["updated_at"]).isoformat() if r["updated_at"] else None,
                "model": r["model"] if "model" in keys else None, "path": r["rollout_path"],
                "archived": bool(r["archived"]) if "archived" in keys else False,
                "originator": r["originator"] if "originator" in keys else None,
                "nickname": r["agent_nickname"] if "agent_nickname" in keys else None,
            })
        return out

    def sessions(self, since=None, subagents=False):
        for home in self.homes:
            if since:
                rows = self._rows(home, "where updated_at >= ?", (int(since.timestamp()),))
            else:
                rows = self._rows(home)
            for r in rows:
                if r["kind"] == "subagent" and not subagents:
                    continue
                yield r

    def resolve(self, ident):
        out = []
        for home in self.homes:
            rows = self._rows(home, "where id like ?", (ident + "%",))
            if not rows:  # rollouts the index missed
                for f in glob.glob(os.path.join(glob.escape(home), "sessions", "*", "*", "*", "rollout-*" + glob.escape(ident) + "*.jsonl")) + \
                        glob.glob(os.path.join(glob.escape(home), "archived_sessions", "rollout-*" + glob.escape(ident) + "*.jsonl")):
                    rows.append(self._row_from_rollout(home, f))
            out += rows
        return [r for r in out if r]

    def _row_from_rollout(self, home, path):
        meta = {}
        for _, rec in jsonl(path):
            if rec.get("type") == "session_meta":
                meta = rec.get("payload") or {}
            break
        return {"harness": "codex", "home": tilde(home), "id": meta.get("id") or os.path.basename(path)[-41:-6],
                "parent": meta.get("parent_thread_id"), "kind": "unindexed", "cwd": meta.get("cwd"),
                "title": None, "first_prompt": None, "started": meta.get("timestamp"), "updated": None,
                "model": None, "path": path, "originator": meta.get("originator")}

    def events(self, row, needles=None):
        return codex_events(row["path"], needles)

    def children(self, row):
        home = os.path.expanduser(row["home"])
        kids = {r["id"]: r for r in self._rows(home) if r["parent"] == row["id"]}
        # guardian / agent_job children only record the parent inside their own session_meta
        started = parse_ts(row.get("started"))
        for r in self._rows(home, "where created_at >= ?", (int(started.timestamp()) if started else 0,)):
            if r["id"] in kids or r["id"] == row["id"] or not r["path"] or not os.path.exists(r["path"]):
                continue
            for _, rec in jsonl(r["path"]):
                if rec.get("type") == "session_meta":
                    p = rec.get("payload") or {}
                    src = p.get("source") if isinstance(p.get("source"), dict) else {}
                    spawn = ((src.get("subagent") or {}).get("thread_spawn") or {}) if isinstance(src.get("subagent"), dict) else {}
                    if p.get("parent_thread_id") == row["id"] or spawn.get("parent_thread_id") == row["id"]:
                        r["kind"] = "subagent"
                        r["parent"] = row["id"]
                        kids[r["id"]] = r
                break
        return sorted(kids.values(), key=lambda r: r.get("started") or "")

    def where(self):
        out = []
        for home in self.homes:
            db = os.path.join(home, "state_5.sqlite")
            n = newest = None
            try:
                con = ro_connect(db)
                n, newest = con.execute("select count(*), max(updated_at) from threads").fetchone()
                con.close()
            except sqlite3.Error:
                pass
            notes = []
            if os.path.islink(os.path.join(home, "sessions")):
                notes.append("sessions/ is a symlink to %s" % tilde(os.path.realpath(os.path.join(home, "sessions"))))
            for idx in ("session_index.jsonl", "history.jsonl"):
                ip = os.path.join(home, idx)
                if os.path.exists(ip) and newest:
                    lag = parse_ts(newest) - parse_ts(os.path.getmtime(ip))
                    if lag > timedelta(days=1):
                        notes.append("%s is stale by %d days; do not search it" % (idx, lag.days))
            out.append({"harness": "codex", "path": tilde(home), "sessions": n, "newest": parse_ts(newest),
                        "index": "state_5.sqlite threads (sessions/ + archived_sessions/ rollouts)",
                        "notes": "; ".join(notes)})
        return out


# ---------------------------------------------------------------- T3 Code

T3_DB = os.path.join(HOME, ".t3", "userdata", "state.sqlite")
T3_DRIVER_HARNESS = {"claudeAgent": "claude", "codex": "codex", "grok": "grok", "opencode": "opencode",
                     "cursor": "cursor"}


def t3_provider_id(driver, cursor, thread_id):
    try:
        cur = json.loads(cursor) if cursor else {}
    except ValueError:
        return None
    if not isinstance(cur, dict):
        return None
    if driver == "claudeAgent":
        return cur.get("resume")
    if driver == "codex":
        return cur.get("threadId")
    for key in ("sessionId", "sessionID", "session_id", "resume", "threadId"):
        val = cur.get(key)
        if val and val != thread_id:
            return val
    return None


class T3:
    name = "t3"

    def __init__(self):
        self.db = T3_DB if os.path.exists(T3_DB) else None
        self.homes = [os.path.dirname(T3_DB)] if self.db else []
        self.instances = t3_settings().get("providerInstances") or {}

    def _query(self, where="", params=()):
        if not self.db:
            return []
        try:
            con = ro_connect(self.db)
            rows = con.execute(
                """select t.thread_id, t.title, t.branch, t.worktree_path, t.created_at, t.updated_at,
                          t.latest_user_message_at, t.model_selection_json, t.archived_at, t.deleted_at,
                          p.workspace_root, p.title as project,
                          r.provider_name, r.provider_instance_id, r.resume_cursor_json, r.status
                   from projection_threads t
                   left join projection_projects p on p.project_id = t.project_id
                   left join provider_session_runtime r on r.thread_id = t.thread_id
                   where t.deleted_at is null """ + where, params).fetchall()
            con.close()
        except sqlite3.Error as e:
            note("t3: cannot read %s: %s" % (tilde(self.db), e))
            return []
        out = []
        for r in rows:
            try:
                sel = json.loads(r["model_selection_json"] or "{}")
            except ValueError:
                sel = {}
            inst = r["provider_instance_id"] or sel.get("instanceId")
            driver = (self.instances.get(inst) or {}).get("driver") or r["provider_name"] or inst
            pid = t3_provider_id(driver, r["resume_cursor_json"], r["thread_id"])
            out.append({
                "harness": "t3", "home": tilde(os.path.dirname(T3_DB)), "id": r["thread_id"], "parent": None,
                "kind": "main", "cwd": r["worktree_path"] or r["workspace_root"], "branch": r["branch"],
                "title": r["title"], "first_prompt": None, "started": r["created_at"],
                "updated": r["latest_user_message_at"] or r["updated_at"], "model": sel.get("model"),
                "path": "%s#thread=%s" % (tilde(T3_DB), r["thread_id"]), "provider_instance": inst,
                "provider": T3_DRIVER_HARNESS.get(driver, driver), "provider_id": pid, "status": r["status"],
                "archived": bool(r["archived_at"]),
            })
        return out

    def sessions(self, since=None, subagents=False):
        if since:
            return self._query("and coalesce(t.latest_user_message_at, t.updated_at) >= ?",
                               (since.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S"),))
        return self._query()

    def resolve(self, ident):
        rows = self._query("and t.thread_id like ?", (ident + "%",))
        rows += [r for r in self._query("and r.resume_cursor_json like ?", ("%" + ident + "%",))
                 if r["provider_id"] and r["provider_id"].startswith(ident)]
        seen, out = set(), []
        for r in rows:
            if r["id"] not in seen:
                seen.add(r["id"])
                out.append(r)
        return out

    def events(self, row, needles=None):
        con = ro_connect(self.db)
        msgs = con.execute("select role, text, created_at from projection_thread_messages where thread_id=?",
                           (row["id"],)).fetchall()
        acts = con.execute("""select kind, summary, payload_json, created_at from projection_thread_activities
                              where thread_id=? and kind in ('tool.completed','runtime.error','tool.denied',
                              'context-compaction','runtime.warning')""", (row["id"],)).fetchall()
        con.close()
        items = []
        for m in msgs:
            role = "prompt" if m["role"] == "user" else "assistant"
            items.append((m["created_at"] or "", ev(role, m["text"] or "", parse_ts(m["created_at"]))))
        for a in acts:
            ts = parse_ts(a["created_at"])
            if a["kind"] == "tool.completed":
                name, inp = a["summary"], None
                try:
                    data = (json.loads(a["payload_json"] or "{}").get("data") or {})
                    name = data.get("toolName") or name
                    inp = data.get("input")
                except (ValueError, AttributeError):
                    pass
                items.append((a["created_at"] or "", ev("tool", "", ts, name=name, input=inp if inp is not None else a["summary"])))
            else:
                items.append((a["created_at"] or "", ev("system", "[%s] %s" % (a["kind"], one_line(a["summary"], 200)), ts,
                                                         error=a["kind"] in ("runtime.error", "tool.denied"))))
        items.sort(key=lambda x: x[0])
        return (e for _, e in items)

    def children(self, row):
        return []

    def where(self):
        if not self.db:
            return []
        n = newest = None
        try:
            con = ro_connect(self.db)
            n, newest = con.execute("select count(*), max(updated_at) from projection_threads where deleted_at is null").fetchone()
            con.close()
        except sqlite3.Error:
            pass
        insts = ", ".join("%s->%s" % (k, ((v.get("config") or {}).get("homePath") or "default"))
                          for k, v in self.instances.items())
        notes = "provider instances: " + insts
        stale = [tilde(p) for p in (os.path.join(HOME, ".t3", "userdata-v2"), os.path.join(HOME, ".t3", "dev")) if os.path.isdir(p)]
        if stale:
            notes += "; ignore %s (dead or dev stores)" % ", ".join(stale)
        return [{"harness": "t3", "path": tilde(self.db), "sessions": n, "newest": parse_ts(newest),
                 "index": "projection_threads + provider_session_runtime.resume_cursor_json", "notes": notes}]


# --------------------------------------------------------------- OpenCode

def opencode_db():
    base = os.environ.get("XDG_DATA_HOME") or os.path.join(HOME, ".local", "share")
    p = os.path.join(base, "opencode", "opencode.db")
    return p if os.path.exists(p) else None


class OpenCode:
    name = "opencode"

    def __init__(self):
        self.db = opencode_db()
        self.homes = [os.path.dirname(self.db)] if self.db else []

    def _query(self, where="", params=()):
        if not self.db:
            return []
        try:
            con = ro_connect(self.db)
            if not table_exists(con, "session_v2"):
                con.close()
                return []
            rows = con.execute("select id, parent_id, directory, title, model, agent, time_created, time_updated, "
                               "time_archived from session_v2 " + where, params).fetchall()
            con.close()
        except sqlite3.Error as e:
            note("opencode: cannot read %s: %s" % (tilde(self.db), e))
            return []
        out = []
        for r in rows:
            try:
                model = (json.loads(r["model"] or "{}") or {}).get("id")
            except (ValueError, AttributeError):
                model = None
            out.append({
                "harness": "opencode", "home": tilde(os.path.dirname(self.db)), "id": r["id"],
                "parent": r["parent_id"], "kind": "subagent" if r["parent_id"] else "main",
                "cwd": r["directory"], "branch": None, "title": r["title"], "first_prompt": None,
                "started": parse_ts(r["time_created"]).isoformat() if r["time_created"] else None,
                "updated": parse_ts(r["time_updated"]).isoformat() if r["time_updated"] else None,
                "model": model, "path": "%s#session=%s" % (tilde(self.db), r["id"]),
                "archived": bool(r["time_archived"]),
            })
        return out

    def sessions(self, since=None, subagents=False):
        where, params = [], []
        if since:
            where.append("time_updated >= ?")
            params.append(int(since.timestamp() * 1000))
        if not subagents:
            where.append("parent_id is null")
        return self._query(("where " + " and ".join(where)) if where else "", tuple(params))

    def resolve(self, ident):
        return self._query("where id like ?", (ident + "%",))

    def events(self, row, needles=None):
        con = ro_connect(self.db)
        rows = con.execute("select type, data, time_created from session_message where session_id=? order by seq",
                           (row["id"],)).fetchall()
        con.close()
        for r in rows:
            ts = parse_ts(r["time_created"])
            try:
                d = json.loads(r["data"] or "{}")
            except ValueError:
                continue
            t = r["type"]
            if t == "user":
                yield ev("prompt", d.get("text") or "", ts)
            elif t == "assistant":
                for b in d.get("content") or []:
                    if not isinstance(b, dict):
                        continue
                    if b.get("type") == "text" and (b.get("text") or "").strip():
                        yield ev("assistant", b["text"], ts, model=(d.get("model") or {}).get("id"))
                    elif b.get("type") == "tool":
                        st = b.get("state") or {}
                        yield ev("tool", "", ts, name=b.get("name"), input=st.get("input"), id=b.get("id"))
                        body = st.get("content")
                        if isinstance(body, list):
                            body = "\n".join(x.get("text", "") for x in body if isinstance(x, dict))
                        if body or st.get("error"):
                            yield ev("result", str(body or st.get("error") or ""), ts,
                                     error=st.get("status") == "error" or bool(st.get("error")))
            elif t == "compaction":
                yield ev("system", "[compacted]", ts)
            elif t in ("synthetic", "system"):
                yield ev("notice", d.get("text") or "", ts)

    def children(self, row):
        return self._query("where parent_id = ?", (row["id"],))

    def where(self):
        if not self.db:
            return []
        n = newest = None
        try:
            con = ro_connect(self.db)
            n, newest = con.execute("select count(*), max(time_updated) from session_v2").fetchone()
            con.close()
        except sqlite3.Error:
            pass
        return [{"harness": "opencode", "path": tilde(self.db), "sessions": n, "newest": parse_ts(newest),
                 "index": "session_v2 + session_message (not the storage/ json tree, not v1 tables)", "notes": ""}]


# ------------------------------------------------------------------- Grok

def grok_user_text(content):
    if isinstance(content, list):
        content = "\n".join(b.get("text", "") for b in content if isinstance(b, dict))
    text = content or ""
    m = re.search(r"<user_query>\s*([\s\S]*?)\s*(?:</user_query>|$)", text)
    if m:
        text = m.group(1)
    text = re.sub(r"<runtime_info>[\s\S]*?</runtime_info>", "", text)
    return _REMINDER_RE.sub("", text).strip()


class Grok:
    """Grok Build: ~/.grok/sessions/<urlencoded cwd>/<session-id>/{summary.json,chat_history.jsonl}.
    session_search.sqlite is a stale partial index; do not use it."""
    name = "grok"

    def __init__(self):
        root = os.path.join(HOME, ".grok", "sessions")
        self.root = root if os.path.isdir(root) else None
        self.homes = [os.path.dirname(root)] if self.root else []

    def _dirs(self, ident=None):
        if not self.root:
            return []
        pat = glob.escape(ident) + "*" if ident else "*"
        return [d for d in glob.glob(os.path.join(glob.escape(self.root), "*", pat))
                if os.path.isdir(d) and os.path.exists(os.path.join(d, "chat_history.jsonl"))]

    def _row(self, d):
        s = {}
        try:
            with open(os.path.join(d, "summary.json")) as f:
                s = json.load(f)
        except (OSError, ValueError):
            pass
        hist = os.path.join(d, "chat_history.jsonl")
        mt = newest_mtime([hist])
        first = None
        if not (s.get("generated_title") or s.get("session_summary")):
            for i, rec in jsonl(hist):
                if rec.get("type") == "user" and rec.get("prompt_index") is not None:
                    first = grok_user_text(rec.get("content"))[:600] or None
                    if first:
                        break
                if i > 300:
                    break
        return {"harness": "grok", "home": tilde(os.path.dirname(self.root)), "id": os.path.basename(d),
                "parent": None, "kind": "main",
                "cwd": (s.get("info") or {}).get("cwd") or urllib.parse.unquote(os.path.basename(os.path.dirname(d))),
                "branch": s.get("head_branch"), "title": s.get("generated_title") or s.get("session_summary"),
                "first_prompt": first, "started": s.get("created_at") or (mt.isoformat() if mt else None),
                "updated": s.get("last_active_at") or s.get("updated_at") or (mt.isoformat() if mt else None),
                "model": s.get("current_model_id"), "path": hist,
                "originator": "t3" if str(s.get("request_id") or "").startswith("t3-") else None}

    def sessions(self, since=None, subagents=False):
        cutoff = since.timestamp() if since else None
        for d in self._dirs():
            try:
                if cutoff and os.path.getmtime(os.path.join(d, "chat_history.jsonl")) < cutoff:
                    continue
            except OSError:
                continue
            yield self._row(d)

    def resolve(self, ident):
        return [self._row(d) for d in self._dirs(ident)]

    def events(self, row, needles=None):
        for _, rec in jsonl(row["path"], needles=needles):
            t = rec.get("type")
            ts = parse_ts(rec.get("timestamp") or rec.get("created_at"))
            if t == "user":
                if rec.get("prompt_index") is None:
                    continue  # injected rules, skills, and reminders
                text = grok_user_text(rec.get("content"))
                if text:
                    yield ev("prompt", text, ts)
            elif t == "assistant":
                content = rec.get("content")
                if isinstance(content, list):
                    content = "\n".join(b.get("text", "") for b in content if isinstance(b, dict))
                if content and content.strip():
                    yield ev("assistant", content, ts, model=rec.get("model_id"))
                for tc in rec.get("tool_calls") or []:
                    fn = tc.get("function") if isinstance(tc.get("function"), dict) else tc
                    yield ev("tool", "", ts, name=fn.get("name"), input=fn.get("arguments"), id=tc.get("id"))
            elif t == "tool_result":
                body = rec.get("content")
                if isinstance(body, list):
                    body = "\n".join(b.get("text", "") for b in body if isinstance(b, dict))
                body = str(body or "")
                yield ev("result", body, ts, error=body.lstrip().startswith('{"error"'), tool_use_id=rec.get("tool_call_id"))

    def children(self, row):
        return []

    def where(self):
        if not self.root:
            return []
        dirs = self._dirs()
        return [{"harness": "grok", "path": tilde(self.root), "sessions": len(dirs),
                 "newest": newest_mtime([os.path.join(d, "chat_history.jsonl") for d in dirs]),
                 "index": "sessions/<urlencoded cwd>/<id>/summary.json + chat_history.jsonl",
                 "notes": "session_search.sqlite is stale; subagents run inline (spawn_subagent tool calls)"}]


ADAPTERS = [Claude, Codex, T3, OpenCode, Grok]
PASSIVE_STORES = [
    ("cursor", "~/Library/Application Support/Cursor/User/globalStorage/state.vscdb",
     "cursorDiskKV composerData:*/bubbleId:*; no adapter"),
    ("cursor-agent", "~/.cursor/projects/*/agent-transcripts", "jsonl; no adapter"),
    ("gemini", "~/.gemini/tmp/*/chats", "session-*.json; no adapter"),
    ("hermes", "~/.hermes/state.db", "sessions + messages tables; few local rows; ignore ~/.hermes/sessions and hermes-agent/"),
    ("context-mode", "~/.claude*/context-mode/sessions, ~/.codex/context-mode/sessions",
     "sidecar index of prompts/decisions, not transcripts; session ids match the harness"),
]


def adapters(names=None):
    out = []
    for cls in ADAPTERS:
        if names and cls.name not in names:
            continue
        out.append(cls())
    return out


# --------------------------------------------------------------- gathering

def cwd_match(row_cwd, want):
    if not want:
        return True
    if not row_cwd:
        return False
    if want in (".", "here"):
        here = os.getcwd()
        return row_cwd == here or row_cwd.startswith(here + os.sep)
    want = os.path.expanduser(want)
    return want.lower() in row_cwd.lower()


def merge_t3(rows):
    """Fold each T3 thread into its provider session row; keep unmatched T3 rows."""
    by_key = {(r["harness"], r["id"]): r for r in rows if r["harness"] != "t3"}
    out = [r for r in rows if r["harness"] != "t3"]
    for r in rows:
        if r["harness"] != "t3":
            continue
        target = by_key.get((r.get("provider"), r.get("provider_id")))
        if target is not None:
            target["t3_thread"] = r["id"]
            target["t3_title"] = r["title"]
            target["provider_instance"] = r.get("provider_instance")
        else:
            out.append(r)
    return out


def gather(args, subagents=False, since=None):
    names = set(args.harness.split(",")) if getattr(args, "harness", None) else None
    rows = []
    for ad in adapters(names):
        for r in ad.sessions(since=since, subagents=subagents):
            if cwd_match(r.get("cwd"), getattr(args, "cwd", None)):
                rows.append(r)
    if not names or "t3" in names:
        rows = merge_t3(rows)
    until = parse_since(args.until) if getattr(args, "until", None) else None
    if until:
        rows = [r for r in rows if (parse_ts(r.get("started")) or datetime.now(timezone.utc)) <= until]
    return rows


def resolve_any(ident):
    ident = ident.strip()
    if os.path.exists(os.path.expanduser(ident)):
        p = os.path.realpath(os.path.expanduser(ident))
        base = os.path.basename(p)
        m = re.search(r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})", base)
        if base.startswith("agent-"):
            ident = base[len("agent-"):].split(".")[0]
        elif m:
            ident = m.group(1)
    if len(ident) < 6:
        raise SystemExit("id prefix too short; give at least 6 characters")
    rows = []
    for ad in adapters():
        try:
            rows += ad.resolve(ident)
        except sqlite3.Error as e:
            note("%s: %s" % (ad.name, e))
    return rows


_ADAPTER_CACHE = {}


def adapter_for(row):
    name = row["harness"]
    if name not in _ADAPTER_CACHE:
        cls = next((c for c in ADAPTERS if c.name == name), None)
        if cls is None:
            raise SystemExit("no adapter for %s" % name)
        _ADAPTER_CACHE[name] = cls()
    return _ADAPTER_CACHE[name]


def pick_one(ident, prefer_provider=True):
    rows = resolve_any(ident)
    if not rows:
        note("no session matches %r in: %s" % (ident, ", ".join(scope_summary())))
        sys.exit(1)
    if prefer_provider:
        rows = merge_t3(rows)  # a T3 thread and its provider session are one conversation
    if len(rows) > 1:
        note("AMBIGUOUS: %r matches %d sessions. Ask the user which one, or, if you cannot ask, open your answer with "
             "every candidate and the one you assumed; then rerun with the full id:" % (ident, len(rows)))
        for r in rows:
            print(fmt_row(r))
        sys.exit(2)
    row = rows[0]
    if row["harness"] == "t3" and prefer_provider and row.get("provider_id"):
        prov = [r for r in resolve_any(row["provider_id"]) if r["harness"] == row.get("provider")]
        if len(prov) == 1:
            prov[0]["t3_thread"], prov[0]["t3_title"] = row["id"], row["title"]
            prov[0]["provider_instance"] = row.get("provider_instance")
            return prov[0]
        note("T3 thread %s: provider transcript %s:%s not found; showing the T3 projection" % (
            row["id"], row.get("provider"), row.get("provider_id")))
    return row


def scope_summary():
    out = []
    for ad in adapters():
        out.append("%s(%s)" % (ad.name, ", ".join(tilde(h) for h in ad.homes) or "absent"))
    return out


# ---------------------------------------------------------------- output

def label(row):
    return row.get("t3_title") or row.get("title") or one_line(row.get("first_prompt") or "", 90) or "(untitled)"


def fmt_row(r, width=90):
    upd = parse_ts(r.get("updated")) or parse_ts(r.get("started"))
    tags = []
    if r.get("kind") and r["kind"] != "main":
        tags.append(r["kind"])
    if r.get("parent"):
        tags.append("parent=" + str(r["parent"]))
    if r.get("t3_thread"):
        tags.append("t3=" + r["t3_thread"])
    if r["harness"] == "t3" and r.get("provider"):
        tags.append("provider=%s:%s" % (r.get("provider_instance") or r["provider"], r.get("provider_id") or "?"))
    if r.get("archived"):
        tags.append("archived")
    if r.get("copies"):
        tags.append("also in " + ", ".join(r["copies"]))
    if r.get("id") in self_ids():
        tags.append("THIS SESSION")
    title = one_line(r.get("t3_title") or label(r), width)
    line = "%s  %-8s %-18s %s\n    %s  %s" % (fmt_ts(upd), r["harness"], r.get("home") or "", r["id"],
                                             tilde(r.get("cwd") or "?"), title)
    if tags:
        line += "  [" + ", ".join(tags) + "]"
    return line


def row_json(r):
    d = dict(r)
    for k in ("started", "updated"):
        dt = parse_ts(d.get(k))
        d[k] = dt.isoformat() if dt else None
    return json.dumps(d, ensure_ascii=False, default=str)


def cmd_where(args):
    print("Stores on this machine (newest = latest activity, local time):\n")
    for ad in adapters():
        for w in ad.where():
            newest = w.get("newest")
            age = (datetime.now(timezone.utc) - newest).days if newest else None
            status = "LIVE" if age is not None and age <= STALE_DAYS else ("STALE" if newest else "?")
            print("%-8s %-5s %-44s sessions=%-5s newest=%s" % (w["harness"], status, w["path"], w.get("sessions"),
                                                               fmt_ts(newest) if newest else "?"))
            print("         index: %s" % w["index"])
            if w.get("notes"):
                print("         notes: %s" % w["notes"])
    print("\nNot adapted (read by hand if needed):")
    for name, path, what in PASSIVE_STORES:
        print("  %-12s %s  (%s)" % (name, path, what))
    ids = self_ids()
    if ids:
        print("\nThis process runs inside: %s" % ", ".join(sorted(ids)))


def cmd_ls(args):
    since = None if args.all else parse_since(args.since or "7d")
    rows = gather(args, subagents=args.subagents, since=since)
    rows.sort(key=lambda r: parse_ts(r.get("updated")) or parse_ts(r.get("started")) or datetime.min.replace(tzinfo=timezone.utc),
              reverse=True)
    shown = rows[: args.limit]
    for r in shown:
        print(row_json(r) if args.json else fmt_row(r))
    if not args.json:
        note("\n%d of %d sessions%s. searched: %s" % (len(shown), len(rows),
             "" if args.all else " updated since %s" % fmt_ts(since), "; ".join(scope_summary())))


SCOPES = {"prompts": ("prompt", "command"), "assistant": ("assistant",), "tools": ("tool",),
          "results": ("result",), "all": ("prompt", "command", "assistant", "tool", "result", "system")}


def event_text(e):
    if e["role"] == "tool":
        inp = e.get("input")
        return "%s %s" % (e.get("name") or "", inp if isinstance(inp, str) else json.dumps(inp, ensure_ascii=False))
    return e.get("text") or ""


def snippet(text, terms, width=220):
    low = text.lower()
    pos = min([p for p in (low.find(t.lower()) for t in terms) if p >= 0] or [0])
    start = max(0, pos - width // 3)
    s = one_line(text[start:start + width], width)
    return ("\u2026" if start else "") + s


def cmd_find(args):
    terms = [t for t in args.terms if t]
    if not terms:
        raise SystemExit("give at least one search term")
    roles = SCOPES[args.scope]
    since = parse_since(args.since) if args.since else None
    rows = gather(args, subagents=args.subagents, since=since)
    mine = self_ids()
    if not args.include_self:
        rows = [r for r in rows if r["id"] not in mine and r.get("parent") not in mine and r.get("provider_id") not in mine]
    by_harness = {}
    for r in rows:
        by_harness.setdefault(r["harness"], []).append(r)
    candidates = []
    for h, rs in by_harness.items():
        if h in ("claude", "codex", "grok"):
            files = [r["path"] for r in rs if r.get("path") and os.path.exists(r["path"])]
            for term in sorted(terms, key=len, reverse=True):  # every term must occur in the file
                files = sorted(files_containing(term, files))
                if not files:
                    break
            keep = set(files)
            candidates += [r for r in rs if r.get("path") in keep]
        elif h == "t3":
            candidates += t3_prefilter(rs, terms, roles)
        elif h == "opencode":
            candidates += opencode_prefilter(rs, terms, roles)
    # Pass 1: term statistics over every in-scope message of the candidate sessions.
    lows = [t.lower() for t in terms]
    needles = [t.encode("utf-8") for t in lows]
    per_session, n_docs, total_len = [], 0, 0
    df = dict.fromkeys(lows, 0)
    SKIPPED[0] = 0
    for r in candidates:
        ad = adapter_for(r)
        docs, covered = [], set()
        first = one_line(r.get("first_prompt") or "", 80).lower()
        try:
            for e in ad.events(r, needles=needles):
                if e["role"] not in roles:
                    continue
                low = event_text(e).lower()
                is_first = bool(first) and e["role"] in ("prompt", "command") and one_line(low, 80) == first
                length = low.count(" ") + 1
                n_docs += 1
                total_len += length
                tf = {t: low.count(t) for t in lows}
                present = [t for t in lows if tf[t]]
                if not present:
                    continue
                for t in present:
                    df[t] += 1
                covered.update(present)
                docs.append((e, tf, length, is_first))
        except (sqlite3.Error, OSError) as ex:
            note("skip %s %s: %s" % (r["harness"], r["id"], ex))
            continue
        if docs and len(covered) == len(lows):  # every term occurs somewhere in scope
            per_session.append((r, docs))
    # Pass 2: BM25 per message, weighted by role; a session scores by its best message,
    # plus a small bonus for breadth and a boost when the title carries the terms.
    avgdl = (total_len / n_docs) if n_docs else 1.0
    n_docs += SKIPPED[0]  # unparsed lines held none of the terms
    idf = {t: math.log(1 + (n_docs - df[t] + 0.5) / (df[t] + 0.5)) for t in lows}
    role_w = {"prompt": 1.5, "command": 1.5, "assistant": 1.0, "tool": 0.8, "result": 0.5, "system": 0.5}

    def bm25(tf, length):
        return sum(idf[t] * tf[t] * 2.2 / (tf[t] + 1.2 * (0.25 + 0.75 * length / avgdl)) for t in lows if tf[t])

    results = []
    for r, docs in per_session:
        scored = []
        for e, tf, length, is_first in docs:
            sc = bm25(tf, length) * role_w.get(e["role"], 1.0) * (1.2 if is_first else 1.0)
            scored.append((sc, e, is_first, all(tf[t] for t in lows)))
        scored.sort(key=lambda x: x[0], reverse=True)
        title = (label(r) + " " + (r.get("t3_title") or "")).lower()
        ttf = {t: title.count(t) for t in lows}
        title_score = 2.0 * bm25(ttf, max(1, len(title.split()))) if any(ttf.values()) else 0.0
        score = scored[0][0] + 0.5 * math.log(1 + len(scored)) + title_score
        full = [x for x in scored if x[3]]
        r["_match"] = "title" if all(ttf.values()) else ("first prompt" if any(x[2] and x[3] for x in scored) else "")
        r["_score"] = round(score, 2)
        hits = [x[1] for x in (full or scored)]  # best messages first; prefer those carrying every term
        results.append((r, hits, score))
    epoch = datetime.min.replace(tzinfo=timezone.utc)
    if args.sort == "oldest":  # provenance: who touched it first
        results.sort(key=lambda x: min((parse_ts(e.get("ts")) or epoch) for e in x[1]) if x[1] else epoch)
    elif args.sort == "recent":
        rank = {"title": 2, "first prompt": 1, "": 0}
        results.sort(key=lambda x: (rank[x[0]["_match"]], parse_ts(x[0].get("updated")) or epoch), reverse=True)
    else:
        results.sort(key=lambda x: (x[2], parse_ts(x[0].get("updated")) or epoch), reverse=True)
    results = [(r, hits) for r, hits, _ in results]
    for r, hits in results[: args.limit]:
        if args.json:
            d = json.loads(row_json(r))
            d.pop("_match", None)
            d["hits"] = [{"ts": e["ts"].isoformat() if e.get("ts") else None, "role": e["role"],
                          "snippet": redact(snippet(event_text(e), terms))} for e in hits[:5]]
            print(json.dumps(d, ensure_ascii=False))
            continue
        print(fmt_row(r) + "  <score %.1f%s>" % (r["_score"], ", match in " + r["_match"] if r["_match"] else ""))
        for e in hits[: args.hits]:
            when = fmt_ts(e.get("ts"))[5:] if e.get("ts") else " " * 11
            print("      %s %-9s %s" % (when, e["role"], redact(snippet(event_text(e), terms))))
        if len(hits) > args.hits:
            print("      (+%d more hits)" % (len(hits) - args.hits))
    note("\n%d matching sessions (%d shown); scope=%s; sort=%s; %s. searched: %s" % (
        len(results), min(len(results), args.limit), args.scope, args.sort,
        "current session excluded" if not args.include_self else "current session included",
        "; ".join(scope_summary())))
    if not results:
        note("no hits. Try --in all, fewer or shorter terms, --subagents, or check `where` for a store this tool does not adapt.")
        sys.exit(1)


_WRITE_TOOLS = ("write", "edit", "multiedit", "notebookedit", "apply_patch", "create_file", "edit_file",
                "write_file", "str_replace_based_edit_tool", "patch")
_READ_ONLY_CMD = re.compile(r"^\s*(?:ls|cat|head|tail|stat|wc|grep|rg|find|file|git (?:status|log|diff|show)|open)\b")


def classify_touch(e, needle):
    name = (e.get("name") or "").lower()
    text = event_text(e)
    if name in _WRITE_TOOLS or any(p.endswith(needle) for p in touched_paths(e.get("name"), e.get("input"))):
        return "write"
    summary = tool_summary(e.get("name"), e.get("input"))
    if re.search(r"(?:>|\btee\b|--out(?:put)?\b|\bmv\b|\bcp\b|\brm\b|build|render|generate)", text):
        return "write?"
    if _READ_ONLY_CMD.search(summary) or name in ("read", "grep", "glob", "read_file", "view", "search"):
        return "read"
    return "mention"


def cmd_touched(args):
    """Every tool call, in any session, whose input names the file; oldest first."""
    target = os.path.abspath(os.path.expanduser(args.path))
    needle = os.path.basename(target) if not args.exact else target
    if os.path.exists(target):
        st = os.stat(target)
        print("file: %s\n  mtime %s (the last writer should end near this time)" % (tilde(target), fmt_ts(parse_ts(st.st_mtime))))
    else:
        print("file: %s (not on disk now)" % tilde(target))
    rows = gather(args, subagents=True, since=parse_since(args.since) if args.since else None)
    mine = self_ids()
    if not args.include_self:
        rows = [r for r in rows if r["id"] not in mine and r.get("parent") not in mine and r.get("provider_id") not in mine]
    cands = []
    for h in ("claude", "codex", "grok"):
        rs = [r for r in rows if r["harness"] == h]
        hit = files_containing(needle, [r["path"] for r in rs if r.get("path")])
        cands += [r for r in rs if r.get("path") in hit]
    cands += t3_prefilter([r for r in rows if r["harness"] == "t3"], [needle])
    cands += opencode_prefilter([r for r in rows if r["harness"] == "opencode"], [needle])
    calls = []
    for r in cands:
        for e in adapter_for(r).events(r):
            if e["role"] == "tool" and needle in event_text(e):
                kind = classify_touch(e, needle)
                if kind in ("read", "mention") and not args.all:
                    continue
                calls.append((parse_ts(e.get("ts")) or parse_ts(r.get("started")), kind, r, e))
    calls.sort(key=lambda x: x[0] or datetime.min.replace(tzinfo=timezone.utc))
    for ts, kind, r, e in calls[-args.limit:]:
        approx = "" if e.get("ts") else "~"
        print("%s%s  %-7s %-8s %s  %s\n    %s" % (approx, fmt_ts(ts), kind, r["harness"], r["id"], one_line(label(r), 60),
                                             redact(tool_summary(e.get("name"), e.get("input")))))
    if calls:
        first, last = calls[0], calls[-1]
        print("\nfirst write-like call: %s %s %s (%s)" % (first[2]["harness"], first[2]["id"], fmt_ts(first[0]), first[1]))
        print("last write-like call:  %s %s %s (%s)" % (last[2]["harness"], last[2]["id"], fmt_ts(last[0]), last[1]))
        if first[2]["id"] != last[2]["id"]:
            print("the creator and the last writer differ; report both")
    note("\n%d %s calls naming %s; oldest first; '~' marks a session start time used where the transcript has no "
         "per-call timestamp. Confirm the last writer against the mtime and `git log -- <path>`." % (
             len(calls), "tool" if args.all else "write-like", needle))
    if not calls:
        sys.exit(1)


def t3_prefilter(rows, terms, roles=SCOPES["all"]):
    t = T3()
    if not t.db:
        return []
    con = ro_connect(t.db)
    msg_roles = [r for r, want in (("user", ("prompt", "command")), ("assistant", ("assistant",)))
                 if any(x in roles for x in want)]
    ids = set()
    params = tuple("%" + x + "%" for x in terms)
    if msg_roles:
        where = " and ".join(["text like ?"] * len(terms))
        ids |= {r[0] for r in con.execute("select distinct thread_id from projection_thread_messages where role in (%s) and %s"
                                          % (",".join("?" * len(msg_roles)), where), tuple(msg_roles) + params)}
    if any(x in roles for x in ("tool", "result")):
        where = " and ".join(["payload_json like ?"] * len(terms))
        ids |= {r[0] for r in con.execute("select distinct thread_id from projection_thread_activities where "
                                          "kind='tool.completed' and " + where, params)}
    con.close()
    return [r for r in rows if r["id"] in ids]


def opencode_prefilter(rows, terms, roles=SCOPES["all"]):
    o = OpenCode()
    if not o.db:
        return []
    types = ["user"] if set(roles) <= {"prompt", "command"} else ["user", "assistant", "synthetic", "system"]
    con = ro_connect(o.db)
    ids = None
    for x in terms:  # terms may sit in different messages of one session
        got = {r[0] for r in con.execute("select distinct session_id from session_message where type in (%s) and data like ?"
                                         % ",".join("?" * len(types)), tuple(types) + ("%" + x + "%",))}
        ids = got if ids is None else ids & got
    con.close()
    return [r for r in rows if r["id"] in (ids or set())]


_CORRECTION_RE = re.compile(
    r"^\s*(?:no\b|nope\b|stop\b|wait\b|hold on|that'?s (?:not|wrong)|you (?:didn'?t|did not|missed|forgot|never)|"
    r"wrong\b|why did you|don'?t\b|undo\b|revert\b|this is (?:wrong|broken)|it (?:still )?(?:doesn'?t|does not) work)",
    re.I)


def cmd_failures(args):
    """Rank sessions by errors, aborted turns, compactions, and user corrections."""
    since = parse_since(args.since or "3d")
    rows = gather(args, subagents=args.subagents, since=since)
    mine = self_ids()
    if not args.include_self:
        rows = [r for r in rows if r["id"] not in mine and r.get("parent") not in mine and r.get("provider_id") not in mine]
    scored = []
    for r in rows:
        try:
            events = [e for e in adapter_for(r).events(r) if e["role"] != "notice"]
        except (sqlite3.Error, OSError) as ex:
            note("skip %s %s: %s" % (r["harness"], r["id"], ex))
            continue
        tool_err = [e for e in events if e["role"] == "result" and e.get("error")]
        sys_err = [e for e in events if e["role"] == "system" and e.get("error")]
        compactions = [e for e in events if e["role"] == "system" and "compact" in (e.get("text") or "")]
        prompts = [e for e in events if e["role"] in ("prompt", "command")]
        corrections = [e for e in prompts[1:] if _CORRECTION_RE.search(e.get("text") or "")]
        score = len(tool_err) + 3 * len(sys_err) + 2 * len(corrections) + len(compactions)
        if score < args.min:
            continue
        first_bad = (sys_err + tool_err + corrections)
        first_bad.sort(key=lambda e: e.get("ts") or datetime.min.replace(tzinfo=timezone.utc))
        scored.append((score, r, tool_err, sys_err, corrections, compactions, len(prompts), first_bad))
    scored.sort(key=lambda x: x[0], reverse=True)
    for score, r, te, se, co, cp, n_prompts, bad in scored[: args.limit]:
        if args.json:
            d = json.loads(row_json(r))
            d.update({"score": score, "tool_errors": len(te), "system_errors": len(se), "corrections": len(co),
                      "compactions": len(cp), "prompts": n_prompts,
                      "samples": [redact(one_line(e.get("text") or "", 240)) for e in bad[:3]]})
            print(json.dumps(d, ensure_ascii=False))
            continue
        print(fmt_row(r))
        print("      score %d: %d tool errors, %d api/abort/runtime errors, %d user corrections, %d compactions, %d prompts"
              % (score, len(te), len(se), len(co), len(cp), n_prompts))
        for e in bad[:3]:
            kind = "correction" if e in co else ("system" if e in se else "tool error")
            when = fmt_ts(e.get("ts"))[5:] if e.get("ts") else " " * 11
            print("      %s %-10s %s" % (when, kind, redact(one_line(e.get("text") or "", 200))))
    note("\n%d of %d sessions since %s scored >= %d (tool error 1, api/abort/runtime error 3, user correction 2, "
         "compaction 1). Scores find candidates; read the turns around the first problem with `show ID --grep`."
         % (len(scored), len(rows), fmt_ts(since), args.min))


def turns_of(events):
    """Split events into turns; each turn starts at a prompt or command."""
    turns, cur = [], []
    for e in events:
        if e["role"] in ("prompt", "command") and cur:
            turns.append(cur)
            cur = []
        cur.append(e)
    if cur:
        turns.append(cur)
    return turns


def render_event(e, full=False, results=False):
    ts = fmt_ts(e.get("ts"))[11:] if e.get("ts") else "     "
    role = e["role"]
    if role in ("prompt", "command"):
        return "\n## %s USER%s\n%s" % (ts, " (command)" if role == "command" else "",
                                        redact(e["text"] if full else clip(e["text"], 4000)))
    if role == "assistant":
        phase = " (%s)" % e["phase"] if e.get("phase") else ""
        return "\n%s ASSISTANT%s:\n%s" % (ts, phase, redact(e["text"] if full else clip(e["text"], 2500)))
    if role == "tool":
        return "%s   tool %s: %s" % (ts, e.get("name"), redact(tool_summary(e.get("name"), e.get("input"))))
    if role == "result":
        if e.get("error"):
            return "%s   ! tool error: %s" % (ts, redact(one_line(e["text"], 300)))
        if results:
            return "%s   -> %s" % (ts, redact(one_line(e["text"], 300)))
        return None
    if role == "system":
        return "%s   [system] %s" % (ts, redact(one_line(e["text"], 300)))
    return None


def header(row):
    lines = ["# %s session %s" % (row["harness"], row["id"])]
    lines.append("- title: %s" % one_line(label(row), 160))
    if row.get("t3_thread"):
        lines.append("- T3 thread: %s (%s)" % (row["t3_thread"], row.get("t3_title") or ""))
    for key in ("home", "path", "cwd", "branch", "model", "parent", "provider_instance", "provider", "provider_id", "originator"):
        if row.get(key):
            lines.append("- %s: %s" % (key, tilde(str(row[key]))))
    st, up = parse_ts(row.get("started")), parse_ts(row.get("updated"))
    lines.append("- time: %s -> %s (local %s)" % (fmt_ts(st), fmt_ts(up), datetime.now().astimezone().strftime("%Z")))
    return "\n".join(lines)


def parse_range(spec, n):
    if not spec:
        return None
    out = set()
    for part in spec.split(","):
        part = part.strip()
        if "-" in part[1:]:
            a, b = part.split("-", 1) if not part.startswith("-") else ("-" + part[1:].split("-", 1)[0], part[1:].split("-", 1)[1])
            a, b = int(a), int(b)
        else:
            a = b = int(part)
        a = a + n + 1 if a < 0 else a
        b = b + n + 1 if b < 0 else b
        out.update(range(max(1, a), min(n, b) + 1))
    return out


def cmd_show(args):
    row = pick_one(args.id, prefer_provider=not args.t3)
    ad = adapter_for(row)
    events = list(ad.events(row))
    if not args.notices:
        events = [e for e in events if e["role"] != "notice"]
    turns = turns_of(events)
    n = len(turns)
    sel = parse_range(args.turns, n)
    if args.tail:
        sel = set(range(max(1, n - args.tail + 1), n + 1))
    if args.grep:
        gs = [g.lower() for g in args.grep]
        sel = {i for i, t in enumerate(turns, 1)
               if all(any(g in event_text(e).lower() for e in t) for g in gs)} & (sel or set(range(1, n + 1)))
    out = [header(row), "- turns: %d%s" % (n, "" if sel is None else " (showing %s)" % compress(sorted(sel)))]
    for i, turn in enumerate(turns, 1):
        if sel is not None and i not in sel:
            continue
        out.append("\n---- turn %d/%d ----" % (i, n))
        for e in turn:
            if e["role"] == "tool" and not args.tools:
                continue
            line = render_event(e, full=args.full, results=args.results)
            if line:
                out.append(line)
    text = "\n".join(out) + "\n"
    if args.output:
        write_out(args.output, text)
        return
    if len(text) <= SHOW_BUDGET or args.full_stdout:
        sys.stdout.write(text)
        return
    path = os.path.join(OUT_DIR, "%s-%s.md" % (row["harness"], row["id"]))
    write_out(path, text, quiet=True)
    index = [header(row), "- turns: %d" % n,
             "\nOutput is %d chars, over the %d budget. Full render: %s" % (len(text), SHOW_BUDGET, path),
             "Read it in slices, or narrow with --turns 3-5, --tail 2, or --grep TEXT.\n", "Turn index:"]
    for i, turn in enumerate(turns, 1):
        first = turn[0]
        kind = "user" if first["role"] in ("prompt", "command") else first["role"]
        tools = sum(1 for e in turn if e["role"] == "tool")
        errs = sum(1 for e in turn if e.get("error"))
        index.append("  %3d %s %-9s %s%s" % (i, fmt_ts(first.get("ts"))[5:], kind, redact(one_line(event_text(first), 110)),
                                              "  (%d tools%s)" % (tools, ", %d errors" % errs if errs else "") if tools else ""))
    last_asst = [e for e in turns[-1] if e["role"] == "assistant"] if turns else []
    if last_asst:
        index.append("\nLast assistant message:\n" + redact(clip(last_asst[-1]["text"], 3000)))
    sys.stdout.write("\n".join(index) + "\n")


def compress(nums):
    if not nums:
        return "none"
    out, start, prev = [], nums[0], nums[0]
    for x in nums[1:] + [None]:
        if x is not None and x == prev + 1:
            prev = x
            continue
        out.append(str(start) if start == prev else "%d-%d" % (start, prev))
        if x is not None:
            start = prev = x
    return ",".join(out)


def write_out(path, text, quiet=False):
    path = os.path.expanduser(path)
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)
    if not quiet:
        print("wrote %s (%d chars)" % (path, len(text)))


def cmd_resolve(args):
    rows = resolve_any(args.id)
    if not rows:
        note("no match for %r. searched: %s" % (args.id, "; ".join(scope_summary())))
        sys.exit(1)
    rows = merge_t3(rows)
    distinct = len(rows)
    if distinct > 1 and not args.json:
        print("AMBIGUOUS: %d different sessions match %r. Ask the user which one they mean. If you cannot ask,"
              " open your answer with every candidate below and the one you assumed.\n" % (distinct, args.id))
    for r in rows:
        print(row_json(r) if args.json else fmt_row(r))
        if not args.json:
            print("    path: %s" % tilde(r.get("path") or ""))
            if r["harness"] == "t3" and r.get("provider_id"):
                prov = [p for p in resolve_any(r["provider_id"]) if p["harness"] == r.get("provider")]
                for p in prov:
                    print("    provider transcript: %s" % tilde(p.get("path") or ""))
                if not prov:
                    print("    provider transcript: not found for %s id %s" % (r.get("provider"), r["provider_id"]))


def cmd_tree(args):
    row = pick_one(args.id)
    print(fmt_row(row))
    kids = adapter_for(row).children(row)
    for k in kids:
        print("  " + fmt_row(k).replace("\n", "\n  "))
    if not kids:
        note("no child sessions recorded")


def cmd_handoff(args):
    row = pick_one(args.id)
    ad = adapter_for(row)
    events = [e for e in ad.events(row) if e["role"] != "notice"]
    turns = turns_of(events)
    prompts = [e for e in events if e["role"] in ("prompt", "command")]
    assistant = [e for e in events if e["role"] == "assistant"]
    tools = [e for e in events if e["role"] == "tool"]
    errors = [e for e in events if e.get("error")]
    files, commands, todos = [], [], None
    base = row.get("cwd") or ""
    for e in tools:
        for p in touched_paths(e.get("name"), e.get("input")):
            p = os.path.normpath(os.path.join(base, os.path.expanduser(p))) if base else p
            if p not in files:
                files.append(p)
        if (e.get("name") or "").lower() in ("bash", "shell", "exec_command", "local_shell", "exec", "execute",
                                             "shell_command", "run_terminal_cmd", "run_command"):
            commands.append(e)
        if (e.get("name") or "") in ("TodoWrite", "todowrite", "update_plan"):
            todos = e.get("input")
    alltext = "\n".join(event_text(e) for e in events)
    tickets = sorted(set(re.findall(r"\b[A-Z]{2,6}-\d{1,5}\b", alltext)), key=lambda s: (s.split("-")[0], int(s.split("-")[1])))
    urls = sorted(set(re.findall(r"https://(?:gitlab\.com|github\.com)/[^\s)\"'>\]]+/(?:-/)?(?:merge_requests|pull)/\d+", alltext)))
    lines = [header(row).replace("# ", "# Handoff draft: ", 1), "",
             "> Draft extracted from a transcript by agent-traces. It records what the session said and did,",
             "> not what is true now. Verify branch, files, and ticket state before acting on it.", "",
             "## Goal (first request)", redact(clip(prompts[0]["text"], 3000)) if prompts else "(no user prompt found)", "",
             "## All user requests (%d)" % len(prompts)]
    for i, p in enumerate(prompts, 1):
        full = i > len(prompts) - 3
        lines.append("%d. [%s] %s" % (i, fmt_ts(p.get("ts")), redact(clip(p["text"], 1500) if full else one_line(p["text"], 300))))
    lines += ["", "## Where it ended (last assistant message; from the transcript, unverified)",
              redact(clip(assistant[-1]["text"], 6000)) if assistant else "(no assistant text)", "",
              "## Current state checks (fill in before handing off)",
              "For each claim above about branches, commits, files, tickets, MRs, or tests, run a live check and record",
              "`verified: <command> -> <result>`, or leave the claim under 'from the transcript, unverified'.", ""]
    if len(assistant) > 1:
        lines.append("## Earlier assistant conclusions (last 4, truncated)")
        for e in assistant[-5:-1]:
            lines.append("- [%s] %s" % (fmt_ts(e.get("ts")), redact(one_line(e["text"], 400))))
        lines.append("")
    if todos:
        lines += ["## Last todo / plan state", "```", redact(clip(json.dumps(todos, indent=1, ensure_ascii=False), 3000)), "```", ""]
    lines.append("## Files written or edited (%d)" % len(files))
    lines += ["- %s" % tilde(f) for f in files[:80]] or ["(none recorded)"]
    lines += ["", "## Commands run (last 25 of %d)" % len(commands)]
    lines += ["- %s" % redact(tool_summary(e.get("name"), e.get("input"))) for e in commands[-25:]] or ["(none)"]
    lines += ["", "## Errors and interruptions (last 15 of %d)" % len(errors)]
    lines += ["- [%s] %s" % (fmt_ts(e.get("ts")), redact(one_line(e.get("text") or "", 240))) for e in errors[-15:]] or ["(none)"]
    lines += ["", "## Tickets and merge requests mentioned",
              ", ".join(tickets[:60]) or "(none)"] + ["- " + u for u in urls[:20]]
    kids = ad.children(row)
    if kids:
        lines += ["", "## Child sessions (%d)" % len(kids)]
        lines += ["- %s %s: %s" % (k["harness"], k["id"], one_line(label(k), 120)) for k in kids[:30]]
    lines += ["", "## Size", "%d turns, %d tool calls, %d errors" % (len(turns), len(tools), len(errors))]
    text = "\n".join(lines) + "\n"
    path = args.output or os.path.join(OUT_DIR, "handoff-%s-%s.md" % (row["harness"], row["id"][:12]))
    write_out(path, text)


def pid_matches(d):
    """True unless the pid's start time contradicts the procStart the session recorded."""
    rec = d.get("procStart")
    if not rec:
        return True
    try:
        out = subprocess.run(["ps", "-p", str(d["pid"]), "-o", "lstart="], stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, env=dict(os.environ, TZ="UTC")).stdout.decode().strip()
    except OSError:
        return True
    return not out or " ".join(out.split()) == " ".join(rec.split())


def cmd_live(args):
    mine = self_ids()
    found = False
    count = others = 0
    for home in claude_homes():
        for f in glob.glob(os.path.join(glob.escape(home), "sessions", "*.json")):
            try:
                with open(f) as fh:
                    d = json.load(fh)
                os.kill(int(d.get("pid")), 0)
            except (OSError, ValueError, TypeError):
                continue
            if not pid_matches(d):
                continue  # the pid was reused by another process
            sid = d.get("sessionId")
            rows = Claude().resolve(sid) if sid else []
            rows = [r for r in rows if r["kind"] == "main"]
            title = label(rows[0]) if rows else ""
            tag = "  [THIS SESSION]" if sid in mine else ""
            last_prompt, last_ts = "", None
            if rows:
                last_ts = parse_ts(rows[0].get("updated"))
                for rec in reversed(tail_lines(rows[0]["path"], 1048576)):
                    if rec.get("type") == "user" and not rec.get("isSidechain"):
                        content = (rec.get("message") or {}).get("content")
                        if isinstance(content, list):
                            content = "\n".join(b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text")
                        if isinstance(content, str) and content:
                            role, text = classify_claude_text(content, rec)
                            if role in ("prompt", "command"):
                                last_prompt = text
                                break
            idle = ""
            if last_ts and datetime.now(timezone.utc) - last_ts > timedelta(hours=12):
                idle = "  IDLE %dd" % (datetime.now(timezone.utc) - last_ts).days
            print("claude  %-18s pid=%-6s %s  %s  last activity %s%s\n    %s  %s%s" % (
                tilde(home), d.get("pid"), sid, d.get("entrypoint") or d.get("kind") or "", fmt_ts(last_ts), idle,
                tilde(d.get("cwd") or ""), one_line(title, 90), tag))
            if last_prompt:
                print("    last prompt: %s" % redact(one_line(last_prompt, 160)))
            found = True
            count += 1
            others += 0 if sid in mine else 1
    t = T3()
    if t.db:
        for r in t._query("and r.status = 'running'"):
            tag = "  [THIS SESSION]" if r.get("provider_id") in mine else ""
            print("t3      %-18s %s  -> %s:%s\n    %s  %s%s" % ("", r["id"], r.get("provider_instance"), r.get("provider_id"),
                                                               tilde(r.get("cwd") or ""), one_line(r.get("title") or "", 90), tag))
            found = True
    print("\n%d live Claude Code sessions, %d besides this one. T3 rows above repeat Claude or Codex sessions "
          "under their T3 thread ids." % (count, others))
    note("claude: live pid in <config>/sessions/<pid>.json; t3: provider_session_runtime.status='running'. "
         "Codex, OpenCode, and Grok have no liveness record; use `ls --since 30m`.")
    if not found:
        sys.exit(1)


def cmd_me(args):
    ids = self_ids()
    if not ids:
        note("no session id in the environment (CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID unset)")
        sys.exit(1)
    for i in sorted(ids):
        for r in resolve_any(i):
            print(fmt_row(r))
            print("    path: %s" % tilde(r.get("path") or ""))


def main(argv=None):
    ap = argparse.ArgumentParser(prog="agent-traces", description=__doc__.split("\n\n")[0],
                                 formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__.split("\n\n", 1)[1])
    sub = ap.add_subparsers(dest="cmd")

    def filters(p, since_default=None):
        p.add_argument("--harness", help="comma list: claude,codex,t3,opencode,grok")
        p.add_argument("--cwd", help="substring of the session cwd; '.' means this directory or below")
        p.add_argument("--since", default=since_default, help="7d, 36h, 90m, or 2026-09-01")
        p.add_argument("--until", help="same formats as --since")
        p.add_argument("--subagents", action="store_true", help="include subagent / child sessions")
        p.add_argument("--json", action="store_true", help="one JSON object per line")

    p = sub.add_parser("where", help="list every trace store and its freshness")
    p.set_defaults(fn=cmd_where)
    p = sub.add_parser("ls", help="list sessions")
    filters(p)
    p.add_argument("--all", action="store_true", help="no time window (default window: 7d)")
    p.add_argument("--limit", type=int, default=30)
    p.set_defaults(fn=cmd_ls)
    p = sub.add_parser("find", help="search sessions")
    p.add_argument("terms", nargs="+", help="all terms must appear in one message (case-insensitive)")
    p.add_argument("--in", dest="scope", choices=sorted(SCOPES), default="prompts",
                   help="prompts (default), assistant, tools (calls), results (tool output), all")
    filters(p)
    p.add_argument("--limit", type=int, default=15)
    p.add_argument("--hits", type=int, default=3, help="snippets per session")
    p.add_argument("--sort", choices=("relevance", "recent", "oldest"), default="relevance",
                   help="relevance: BM25 over messages, prompts and titles weighted up; recent: title > first prompt "
                        "> newest; oldest: earliest hit first (provenance)")
    p.add_argument("--include-self", action="store_true")
    p.set_defaults(fn=cmd_find)
    p = sub.add_parser("touched", help="which sessions wrote (or read) a file, oldest first")
    p.add_argument("path")
    p.add_argument("--all", action="store_true", help="include reads and plain mentions, not only write-like calls")
    p.add_argument("--exact", action="store_true", help="match the full path instead of the basename")
    filters(p)
    p.add_argument("--limit", type=int, default=25)
    p.add_argument("--include-self", action="store_true")
    p.set_defaults(fn=cmd_touched)
    p = sub.add_parser("failures", help="rank sessions by errors, aborted turns, and user corrections")
    filters(p)
    p.add_argument("--limit", type=int, default=10)
    p.add_argument("--min", type=int, default=3, help="minimum score to list")
    p.add_argument("--include-self", action="store_true")
    p.set_defaults(fn=cmd_failures)
    p = sub.add_parser("show", help="render a session transcript")
    p.add_argument("id")
    p.add_argument("--turns", help="e.g. 1-3,7 or -2--1 (negative counts from the end)")
    p.add_argument("--tail", type=int, help="last N turns")
    p.add_argument("--grep", nargs="+", help="only turns containing all of these texts")
    p.add_argument("--no-tools", dest="tools", action="store_false", help="hide tool call lines")
    p.add_argument("--results", action="store_true", help="show one-line tool results")
    p.add_argument("--notices", action="store_true", help="include injected notices and boilerplate")
    p.add_argument("--full", action="store_true", help="do not truncate messages")
    p.add_argument("--full-stdout", action="store_true", help="print even when over the size budget")
    p.add_argument("--t3", action="store_true", help="render the T3 projection instead of the provider transcript")
    p.add_argument("-o", "--output", help="write to this file instead of stdout")
    p.set_defaults(fn=cmd_show)
    p = sub.add_parser("resolve", help="find which store owns an id")
    p.add_argument("id")
    p.add_argument("--json", action="store_true")
    p.set_defaults(fn=cmd_resolve)
    p = sub.add_parser("tree", help="child sessions of a session")
    p.add_argument("id")
    p.set_defaults(fn=cmd_tree)
    p = sub.add_parser("handoff", help="write a handoff draft for a session")
    p.add_argument("id")
    p.add_argument("-o", "--output")
    p.set_defaults(fn=cmd_handoff)
    p = sub.add_parser("live", help="sessions running now")
    p.set_defaults(fn=cmd_live)
    p = sub.add_parser("me", help="identify the current session")
    p.set_defaults(fn=cmd_me)
    args = ap.parse_args(argv)
    if not getattr(args, "fn", None):
        ap.print_help()
        return 2
    try:
        args.fn(args)
    except BrokenPipeError:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
