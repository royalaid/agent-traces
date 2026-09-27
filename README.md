# agent-traces

Read coding-agent sessions across Claude Code, Codex, T3 Code, OpenCode, and
Grok Build with one native executable. SQLite is bundled. Running the executable
does not require Python, ripgrep, Cargo, or a separately installed SQLite.

All eleven commands are implemented. Windows unit, schema, and Python parity
checks pass; the Linux musl executable cross-builds. Follow the native Windows,
macOS ARM64/x64, and Linux [CI results](https://github.com/royalaid/agent-traces/actions/workflows/ci.yml).
Python remains the regression oracle. See
[verification and measurements](docs/verification-2026-09-26.md). No published
binaries or crates.io package are available yet.

## Build and install

Install a Rust toolchain and the platform C compiler, then run:

```sh
cargo build --locked --release
cargo install --locked --path .
agent-traces --help
```

The release executable is `target/release/agent-traces` on Unix and
`target/release/agent-traces.exe` on Windows. Copy that file to a directory on
PATH. macOS binaries use the operating system libraries; "self-contained" does
not mean a fully static macOS executable. Windows release archives select the
static MSVC runtime. Linux release archives target musl.

## Commands

| Command | Purpose |
| --- | --- |
| `where` | Locate stores and describe coverage |
| `ls` | List sessions with date, harness, and cwd filters |
| `find QUERY` | Rank matching sessions with BM25 |
| `touched PATH` | Locate recorded file edits |
| `failures` | Rank errors and user corrections |
| `show ID` | Render a transcript or selected turns |
| `resolve ID` | Resolve full IDs and prefixes |
| `tree ID` | List recorded child sessions |
| `handoff ID` | Extract a handoff draft |
| `live` | Query available liveness evidence |
| `me` | Resolve the caller's environment session IDs |

Legacy `ls --json`, `find --json`, and `resolve --json` emit JSON Lines.
Mailbox consumers should use `--json --schema-version 1` and set
`AGENT_TRACES_HOST_ID` to a stable host namespace. The versioned envelope applies
to ls, find, live, resolve, me, and handoff. See [the contract](docs/cli-contract.md)
and [schema](schemas/cli-v1.schema.json). Check exit status and coverage before
treating an empty result as a complete scan.

Stores are opened read-only using their existing WAL configuration. The program
does not switch journal mode, checkpoint, migrate, or modify store content.
Read-only SQLite access can take transient read locks and use WAL shared-memory
coordination. Generated caches and handoff output belong outside session stores.
Hermes has no adapter in this release.

## Verify

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python tests/parity.py
```

Python is a development-only reference test dependency. Optional schema checks
use `python -m pip install jsonschema==4.26.0`, then
`python tests/parity.py --schema`. See [parity scope](docs/parity.md).

CI is configured for native Windows, macOS ARM64/x64, and Linux verification. The manual release
workflow builds archives and SHA-256 checksums; it does not publish them.
Its targets are Windows x64, macOS ARM64 and x64, and Linux x64 musl.
Linux ARM64 is a later matrix addition requiring a native runner or a tested
cross-toolchain. Rust target details come from the
[official platform list](https://doc.rust-lang.org/rustc/platform-support.html).
