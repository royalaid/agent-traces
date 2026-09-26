# Implementation interfaces

The parent owns src/main.rs, lib.rs, context.rs, model.rs, util.rs, adapters/mod.rs,
Cargo.toml, orchestration/core.rs, integration and final verification.

Session is serde_json::Value, preserving legacy absent/null fields. Event is the
shared typed structure in model.rs. Implement the StoreAdapter trait exactly as
provided in adapters/mod.rs. Unit structs Claude, Codex, T3, OpenCode, Grok
implement it. Context is passed explicitly, and provides home/cache/out/now,
self_ids, diagnostic/cover methods, t3_settings, expand, tilde, and skipped Cell.
Use Context paths, not a new global HOME. AGENT_TRACES_HOME and AGENT_TRACES_NOW
are explicit fixture/test overrides, also useful for alternate roots.

Owned module files can be added to lib.rs/mod.rs TEMPORARILY in isolated worktrees
for testing, but do not deliver those shared declarations; parent integrates.
Read reference tests/reference/agent_traces.py and docs/stores.md. Output semantics
are reference-driven. Do not change schema or silently fix algorithm quirks.
Never mutate live stores. mode=ro and existing WAL required; never set journal_mode.

JSONL worker owns storage/jsonl.rs and exports:
records(ctx,path,start,needles) -> Box<dyn Iterator<Item=(usize,Value)> + '_>
where ctx:&Context, path:&Path, start:usize, needles:Option<&[String]>.
Raw JSONL filtering counts skipped lines through ctx.skipped for ranking parity.
Also tail_lines(ctx,path,nbytes)->Vec<Value>. Functions report parse issues.

SQLite worker owns storage/sqlite.rs and exports:
open(ctx:&Context,harness:&str,path:&Path)->anyhow::Result<rusqlite::Connection>.
Use read-only flags and verify existing WAL. Report diagnostics and coverage.
Short statements, no changing journal, checkpoints, migrations, or data writes.

Core (parent) will expose core::adapters()->Vec<Box<dyn StoreAdapter>>,
core::adapter(name)->Result<Box<dyn StoreAdapter>>, core::resolve_any(ctx,id),
core::pick_one(ctx,id,prefer_provider), core::merge_t3(rows), and
core::gather(ctx,&Filters). Filters fields: harness:Option<String>,
cwd:Option<String>,since:Option<DateTime<Utc>>,until:Option<DateTime<Utc>>,
subagents:bool. All operations return anyhow::Result where appropriate.

Rendering worker exports render::fmt_row(ctx,row)->String,
render::header(ctx,row)->String, render::tool_summary(name,input)->String,
render::touched_paths(name,input)->Vec<String>, render::render_event(e,full,results)->String,
render::turns_of(events:&[Event])->Vec<Vec<Event>>.
Commands worker defines render command options/functions in commands.rs;
coordinate signatures with parent. Search worker creates search.rs API documented
in its delivery, using core::gather and Event stream. v1 projection parent owns.
