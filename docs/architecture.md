# Architecture

One Cargo package exposes a library for tests and one `agent-traces` executable.
`main.rs` owns Clap parsing and the legacy/v1 output boundary. `core.rs` owns
cross-store discovery, T3 merging, resolution, and guarded artifact writes.
`Context` captures invocation-local configuration, raw discovered identities,
coverage, diagnostics, and protected source paths.

Each harness implements `StoreAdapter` under `src/adapters/`. Its interface covers
homes, sessions, resolution, events, children, and store descriptions. The five
adapters are Claude, Codex, T3, OpenCode, and Grok. Hermes remains unsupported.
`storage/jsonl.rs` streams records; `storage/sqlite.rs` owns read-only SQLite
opening and existing-WAL enforcement. Rusqlite bundles SQLite into the binary.

Legacy sessions retain serde_json values to preserve missing-key versus null
semantics. Events have typed common fields. `search.rs` ports the exact reference
substring BM25 and ranking rules rather than substituting a tokenizer or scoring
crate. It performs file prefiltering in-process with regex. `render.rs` and
`commands.rs` implement transcript, handoff, file-touch, and failure output.
`contract.rs` projects legacy data into the versioned schema without changing
the legacy format. `platform.rs` uses native process-query APIs for liveness.

Output and caches are outside session stores. Artifact writes use a temporary
file in the destination directory followed by rename, with source-root and
observed-source guards. SQLite uses read-only open flags and normal WAL reader
coordination; it never changes journal mode or requests immutable/nolock reads.
No runtime command shells out to Python, rg, SQLite, or another executable.

The test-only Python reference is frozen. Synthetic fixtures and private captures
feed both CLIs through subprocesses, compare observable outputs, and check that
fixture stores remain unchanged. CI prepares native platform validation; release
archives are a separate manual workflow without publication permissions.
