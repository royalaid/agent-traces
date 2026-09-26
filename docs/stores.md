# Trace stores on this machine

Surveyed 2026-09-23. `agent-traces where` shows the live state; this file records the formats and the traps. Open every SQLite store with `sqlite3 'file:/abs/path?mode=ro'` (sees WAL content, cannot write). A plain `cp` of a live db plus its `-wal` has produced a corrupt copy.

## Claude Code

- **Homes.** `CLAUDE_CONFIG_DIR` picks the home, and the launchers disagree: bare `claude` and `cc` use `~/.claude-personal`; `ccw` and `claude-work` use `~/.claude`; `~/.claude-sneakpeek/claudesp/config` is an old trial install. T3's `claudeAgent_personal` instance writes to `~/.claude-personal`. 475 sessions from before September exist as byte-identical copies in both homes (a home split copied `projects/`); later sessions are disjoint. The CLI lists each session once, from the fuller copy, and tags the other home.
- **Transcript.** `<home>/projects/<cwd-slug>/<session-uuid>.jsonl`. The slug replaces `/` and `.` with `-`, so it is lossy and starts with `-` (prefix paths with `./` or `--` in shell commands). The real `cwd` is on nearly every record.
- **Records.** `user`, `assistant` (one record per content block: `text`, `tool_use`, `thinking`), `system` (`compact_boundary`), `attachment` (hook output), `ai-title` / `custom-title`, `last-prompt` (`leafUuid`), `file-history-snapshot`, `cost-state`, `pr-link`, and more. There is no `summary` record type.
- **Real prompts.** `promptSource` is `typed`, `sdk`, or `system` on newer records. Injected text arrives as user records: `<task-notification>`, `<local-command-*>`, `<system-reminder>` blocks, "Another Claude session sent a message", `isMeta`, `isCompactSummary`.
- **Subagents.** `<session-uuid>/subagents/agent-<agentId>.jsonl` plus `.meta.json` (`name`, `description`, `agentType`, `toolUseId`). Workflow-tool agents sit one level deeper in `subagents/workflows/wf_<id>/agent-<agentId>.jsonl` beside a `journal.jsonl`; together they outnumber top-level sessions. Records carry the parent's `sessionId` and `isSidechain: true`.
- **Compaction and resume.** Compaction writes `compact_boundary` plus an `isCompactSummary` user record in the same file; resume appends to the same file. Edits and resends create branches (several records sharing a `parentUuid`), so file order is not a single thread.
- **Liveness.** `<home>/sessions/<pid>.json` maps a running pid to `sessionId` and `cwd`.
- **Retention.** `cleanupPeriodDays` is 99999 in both homes; the oldest surviving transcripts date from 2026-04-26, so earlier sessions are gone.
- **Traps.** `history.jsonl` logs only REPL-typed prompts (SDK, T3, and subagent prompts are absent) and its `paste-cache` pointers rot. `~/.claude.json` `projects[cwd].lastSessionId` gives the latest session for an exact cwd. Lone UTF-16 surrogates from truncated tool output can make a line unparseable; parse per line.

## Codex

- **Homes.** `~/.codex` (personal, default) and `~/.codex-work` (set by the `ccw` / `codex-work` launchers and T3's `codex_work` instance). `~/.codex-t3/work` is a symlink farm onto `~/.codex`. T3's `codex_work` threads from before its `homePath` change live in `~/.codex`.
- **Index of record.** `state_5.sqlite` table `threads`: `id`, `rollout_path`, `cwd`, `title` (the first user message unless renamed via `name`), `first_user_message`, `created_at`/`updated_at` (epoch seconds), `archived`, `source`, `thread_source`, `originator`, `model`, `git_branch`. Parent links: `thread_spawn_edges` for persona subagents; guardian and `agent_job` children only record `session_meta.payload.parent_thread_id` in their own rollout.
- **Rollout.** `sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl` and flat `archived_sessions/` (archiving is not age-based; search both, or trust `threads.rollout_path`). Line 0 is `session_meta`. Content is `response_item` payloads: `message` (roles `user`, `assistant` with `phase` `commentary`/`final_answer`, `developer`), `function_call` / `custom_tool_call` (code mode: JavaScript calling `tools.exec_command({cmd: …})`, `tools.apply_patch`) and their `*_output`, `reasoning` (encrypted). `event_msg` carries `turn_aborted`; `compacted` marks compaction.
- **Real prompts.** Every session injects boilerplate as `user` messages starting with `<` (`<environment_context>`, `<recommended_plugins>`, `<skill>`) or `# AGENTS.md instructions`; a raw grep for common words matches almost every rollout.
- **Workers.** `codex exec` delegated workers have `originator: codex_exec`, `source: exec`, usually a `worktrees/<slug>` cwd, one templated prompt, and a `=== FINAL REPORT ===` ending.
- **Traps.** `history.jsonl` stopped in April and `session_index.jsonl` lags by weeks. `thread_history_1.sqlite` `thread_items` is a camelCase cache of the same content (`userMessage`, `agentMessage`). `session_meta.payload.session_id` can name an earlier rollout. A forked subagent may carry its parent's history only inside a `compacted` record's `replacement_history`; the CLI does not search that copy.

## T3 Code

- **Store.** `~/.t3/userdata/state.sqlite`, event-sourced. `~/.t3/userdata-v2` is an empty abandoned schema and `~/.t3/dev` a stale dev copy.
- **Tables.** `projection_threads` (`thread_id`, `title`, `branch`, `worktree_path`, `model_selection_json.instanceId`), `projection_projects.workspace_root`, `projection_thread_messages` (full `text`, role `user`/`assistant`), `projection_thread_activities` (`tool.completed` payloads with tool input and result), `projection_turns` (checkpoint git refs `refs/t3/checkpoints/<base64 thread>/turn/<n>`).
- **Provider mapping.** `provider_session_runtime.resume_cursor_json`: Claude `resume`, Codex `threadId`, Grok and OpenCode `sessionId`. `projection_thread_sessions.provider_session_id` is always empty. The instance's home comes from `~/.t3/userdata/settings.json` `providerInstances.<id>.config.homePath` (empty means the provider default), and it has changed over time, so search every home by id.
- **Worktrees.** `~/.t3/worktrees/<repo>/<name>`; the provider records that path as its cwd, which changes the Claude slug.
- **Traps.** No sub-thread concept; subagents exist only inside the provider transcript. `clerk-tokens.json` and `cloud-auth-token.json` sit beside the db; name `state.sqlite` explicitly rather than globbing the directory.

## OpenCode

- **Store.** `~/.local/share/opencode/opencode.db`. `session_v2` (`id` `ses_…`, `parent_id`, `directory`, `title`, `model` JSON, `time_created`/`time_updated` epoch ms) and `session_message` (`session_id`, `seq`, `type` `user`/`assistant`/`synthetic`/`system`/`compaction`, `data` JSON). Assistant `data.content` blocks: `text`, `reasoning`, `tool` (`name`, `state.input`, `state.content`, `state.status`).
- **Traps.** The `storage/` JSON tree stopped in April and the v1 `session`/`message`/`part` tables stopped on 2026-09-18; both are superseded (v1 sessions were migrated into `session_v2`). Skill text arrives under `data.skills`, separate from the prompt `data.text`.

## Grok Build

- **Store.** `~/.grok/sessions/<urlencoded cwd>/<session-id>/`: `summary.json` (`info.cwd`, `generated_title`, `current_model_id`, `head_branch`, `created_at`, `last_active_at`, `request_id` starting `t3-` when driven by T3) and `chat_history.jsonl` (`type` `system`/`user`/`assistant`/`tool_result`/`reasoning`). `prompt_history.jsonl` per cwd directory lists typed prompts.
- **Real prompts.** `type == "user"` with a non-null `prompt_index`; the text sits inside `<user_query>`. Other user records are injected rules, skills, and reminders.
- **Traps.** `session_search.sqlite` indexes a small stale fraction of sessions. Subagents run inline as `spawn_subagent` tool calls. `chat_history.jsonl` has no per-record timestamps; `events.jsonl` has timing.

## Stores without an adapter

- **Cursor** (last used May 2026). IDE chats: `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`, table `cursorDiskKV`, keys `composerData:<id>` (session) and `bubbleId:<id>:<bubble>` (messages); trust the embedded `createdAt`, not file mtimes. cursor-agent CLI: `~/.cursor/projects/*/agent-transcripts/*.jsonl`.
- **Gemini CLI** (last used May 2026). `~/.gemini/tmp/<slug-or-hash>/chats/session-*.json`; `~/.gemini/projects.json` maps registered cwds. Antigravity `.pb` files are undecoded.
- **Hermes.** `~/.hermes/state.db`: `sessions` (`cwd`, `title`, `model`, `parent_session_id`, `origin_json.imported_from` for sessions imported from Claude or Codex) and `messages` (`role`, `content`, `tool_calls`), with `messages_fts`. `~/.hermes/sessions/` is empty and `~/.hermes/hermes-agent/` is the app build. Few local rows; most use appears to be on a remote gateway.
- **Claude desktop.** `~/Library/Application Support/Claude/claude-code-sessions` and `local-agent-mode-sessions` hold metadata with a `cliSessionId` pointer into a Claude home; no message bodies.
- **context-mode.** `<home>/context-mode/sessions/*.db` per harness home: `session_meta`, `session_events` (`user_prompt` in full, `turn_end.last_assistant_message` truncated, decisions, findings). Session ids equal the harness ids; shard filenames are not unique across homes. The Codex sidecar stopped writing on 2026-09-03.
- **Empty or irrelevant.** Codex desktop app support dir (Chromium profile), Conductor (`session_messages` empty), CodexBar (quota only), Zed `threads.db` (empty), Warp (no local store).
