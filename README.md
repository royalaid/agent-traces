# agent-traces

Read coding-agent sessions across Claude Code, Codex, T3 Code, OpenCode, and
Grok Build with one native executable. SQLite is bundled. Running the executable
does not require Python, ripgrep, Cargo, or a separately installed SQLite.

All eleven commands are implemented. Windows unit, schema, and Python parity
checks pass; the Linux musl executable cross-builds. Follow the native Windows,
macOS ARM64/x64, and Linux [CI results](https://github.com/royalaid/agent-traces/actions/workflows/ci.yml).
Python remains the regression oracle. See
[verification and measurements](docs/verification-2026-09-26.md).
Download native binaries from the [v0.1.0 release](https://github.com/royalaid/agent-traces/releases/tag/v0.1.0).
This package is not published to crates.io.

## Install a release

Authenticate GitHub CLI with access to `royalaid/agent-traces`. Download the archive
and its `.sha256` file for your host with:

```sh
gh release download v0.1.0 --repo royalaid/agent-traces --pattern 'ARCHIVE_NAME*' --dir downloads
```

| Host | Archive name |
| --- | --- |
| Windows x64 | `agent-traces-x86_64-pc-windows-msvc.zip` |
| macOS Apple Silicon | `agent-traces-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `agent-traces-x86_64-apple-darwin.tar.gz` |
| Linux x64, including WSL | `agent-traces-x86_64-unknown-linux-musl.tar.gz` |

Replace `ARCHIVE_NAME` with the exact table entry. Verify the archive against its
SHA-256 file before extracting: `Get-FileHash -Algorithm SHA256` on Windows,
`shasum -a 256 -c ARCHIVE_NAME.sha256` on macOS, or
`sha256sum -c ARCHIVE_NAME.sha256` on Linux, from the download directory.
Extract with `Expand-Archive` on Windows or `tar -xzf` on Unix. Copy the executable
inside the extracted directory into a directory on PATH, then verify
`agent-traces --version` and `agent-traces where`. Run it on the host that owns
the stores; Windows and WSL are separate hosts.

## Build from source

Clone this repository, enter its directory, and install a Rust toolchain and the
platform C compiler. Run:

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

`--since` and `--until` select sessions by recorded activity: `--since` keeps a
session whose last timestamped event is at or after the cutoff, and `--until`
one that started at or before it. File modification time is only a prefilter,
because harnesses append undated metadata to old sessions. `failures` also
scores only the events inside that window, so a session resumed this week does
not carry last month's errors into this week's ranking. Pass `--all-events` to
score whole sessions. A score ranks candidates for reading; it is not a failure
rate. Skipped records are reported on stderr with their store path, and an
unterminated final record is named as such, since it usually means the file was
being written while it was read.

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
T3 Code reads orchestrator v2 threads from `~/.t3/userdata/statev2.sqlite` and
adds v1 threads from `state.sqlite` whose IDs are absent from v2. A v2 row takes
precedence, including a deleted row. Machines with only v1 keep using the v1
store. Provider session IDs resolve to their T3 thread, including IDs from
previous provider threads in v2. `where` reports each store separately.

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
workflow verifies native release executables and builds archives with SHA-256
checksums. Publication attaches those verified archives to a versioned GitHub
release; the workflow itself has read-only repository permissions.
Its targets are Windows x64, macOS ARM64 and x64, and Linux x64 musl.
Linux ARM64 is a later matrix addition requiring a native runner or a tested
cross-toolchain. Rust target details come from the
[official platform list](https://doc.rust-lang.org/rustc/platform-support.html).

## License

MIT. See [LICENSE](LICENSE).
