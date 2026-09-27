"""Synthetic, frozen store layouts. Never copies or opens a user's stores."""
import json
import os
import sqlite3
from pathlib import Path

NOW = "2026-09-26T12:00:00Z"
TS = "2026-09-25T10:00:00Z"
EPOCH = 1790330400
IDS = {"claude": "aaaaaaaa-1111-4111-8111-111111111111",
       "codex": "bbbbbbbb-2222-4222-8222-222222222222",
       "t3": "thread-fixture-333333", "opencode": "ses_fixture444444",
       "grok": "grok-fixture-555555"}


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")


def jsonl(path, records):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(json.dumps(r, ensure_ascii=False) for r in records) + "\n", encoding="utf-8")
    os.utime(path, (EPOCH, EPOCH))


def database(path, schema):
    path.parent.mkdir(parents=True, exist_ok=True)
    con = sqlite3.connect(path)
    con.execute("pragma journal_mode=WAL")  # Fixture creation only.
    con.executescript(schema)
    return con


def create(home):
    home = Path(home)
    cwd = str(home / "project")
    Path(cwd).mkdir(parents=True, exist_ok=True)
    prompt = "Repair mailbox routing for café FIX-42"
    claude = home / ".claude/projects/fixture" / (IDS["claude"] + ".jsonl")
    jsonl(claude, [
        {"type": "user", "cwd": cwd, "gitBranch": "fixture", "timestamp": TS,
         "message": {"content": prompt}},
        {"type": "assistant", "timestamp": TS, "message": {"model": "fixture-model", "content": [
            {"type": "text", "text": "I will repair mailbox routing."},
            {"type": "tool_use", "id": "call1", "name": "Write", "input": {"file_path": str(home / "project/router.py"), "content": "fixed"}}]}},
        {"type": "user", "timestamp": TS, "message": {"content": [{"type": "tool_result", "tool_use_id": "call1", "content": "permission denied", "is_error": True}]}},
        {"type": "user", "timestamp": TS, "message": {"content": "Still broken, fix mailbox routing"}},
        {"type": "custom-title", "customTitle": "Mailbox routing", "timestamp": TS},
    ])
    # Bad lines and partial trailing writes must not discard earlier events.
    with claude.open("a", encoding="utf-8") as stream:
        stream.write('not json\n{"partial":')
    os.utime(claude, (EPOCH, EPOCH))
    rollout = home / ".codex/sessions/2026/09/25" / ("rollout-fixture-" + IDS["codex"] + ".jsonl")
    jsonl(rollout, [
        {"type": "session_meta", "timestamp": TS, "payload": {"id": IDS["codex"], "cwd": cwd, "timestamp": TS}},
        {"type": "response_item", "timestamp": TS, "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": prompt}]}},
        {"type": "response_item", "timestamp": TS, "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Mailbox fixed."}]}},
    ])
    con = database(home / ".codex/state_5.sqlite", """
        create table threads(id text primary key, cwd text, rollout_path text, created_at integer,
        updated_at integer, source text, git_branch text, name text, first_user_message text, model text,
        archived integer, originator text, agent_nickname text, agent_role text, thread_source text);
        create table thread_spawn_edges(parent_thread_id text, child_thread_id text);
    """)
    con.execute("insert into threads values (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", (IDS["codex"], cwd, str(rollout), EPOCH, EPOCH + 1, "cli", "fixture", "Codex mailbox", prompt, "fixture-model", 0, "cli", None, None, "user"))
    con.commit(); con.close()
    con = database(home / ".t3/userdata/state.sqlite", """
        create table projection_projects(project_id text, workspace_root text, title text);
        create table projection_threads(thread_id text, title text, branch text, worktree_path text,
          created_at text, updated_at text, latest_user_message_at text, model_selection_json text,
          archived_at text, deleted_at text, project_id text);
        create table provider_session_runtime(thread_id text, provider_name text, provider_instance_id text, resume_cursor_json text, status text);
        create table projection_thread_messages(thread_id text, role text, text text, created_at text);
        create table projection_thread_activities(thread_id text, kind text, summary text, payload_json text, created_at text);
    """)
    con.execute("insert into projection_projects values (?,?,?)", ("project", cwd, "Fixture"))
    con.execute("insert into projection_threads values (?,?,?,?,?,?,?,?,?,?,?)", (IDS["t3"], "T3 mailbox", "fixture", cwd, TS, TS, TS, '{"model":"fixture-model"}', None, None, "project"))
    # Runtime insertion order deliberately differs from thread order. Filtering
    # in SQL follows the runtime status index, unlike filtering a full join later.
    con.execute("create index runtime_status on provider_session_runtime(status)")
    con.execute("insert into projection_threads values (?,?,?,?,?,?,?,?,?,?,?)", ("thread-second-fixture", "Home session", "fixture", str(home), TS, TS, TS, '{"model":"fixture-model"}', None, None, "project"))
    con.execute("insert into provider_session_runtime values (?,?,?,?,?)", ("thread-second-fixture", "codex", "codex", json.dumps({"threadId": IDS["codex"]}), "running"))
    con.execute("insert into provider_session_runtime values (?,?,?,?,?)", (IDS["t3"], "codex", "codex", json.dumps({"threadId": IDS["codex"]}), "running"))
    con.execute("insert into projection_thread_messages values (?,?,?,?)", (IDS["t3"], "user", prompt, TS))
    con.commit(); con.close()
    con = database(home / ".local/share/opencode/opencode.db", """
        create table session_v2(id text, parent_id text, directory text, title text, model text,
          agent text, time_created integer, time_updated integer, time_archived integer);
        create table session_message(session_id text, type text, data text, time_created integer, seq integer);
    """)
    con.execute("insert into session_v2 values (?,?,?,?,?,?,?,?,?)", (IDS["opencode"], None, cwd, "OpenCode mailbox", '{"id":"fixture-model"}', "build", EPOCH * 1000, (EPOCH + 2) * 1000 + 172, None))
    con.execute("insert into session_message values (?,?,?,?,?)", (IDS["opencode"], "user", json.dumps({"text": prompt}), EPOCH * 1000, 1))
    con.commit(); con.close()
    grok = home / ".grok/sessions/fixture" / IDS["grok"]
    write_json(grok / "summary.json", {"info": {"cwd": cwd}, "generated_title": "Grok mailbox", "created_at": TS, "last_active_at": "2026-09-25T10:00:03Z", "current_model_id": "fixture-model"})
    jsonl(grok / "chat_history.jsonl", [{"type": "user", "prompt_index": 0, "timestamp": TS, "content": "<runtime_info>ignored</runtime_info><user_query>" + prompt + "</user_query>"}])
    return IDS.copy()
