# Parity gates

`tests/reference/agent_traces.py` is the frozen Python oracle. Do not edit it to
make a Rust discrepancy disappear. `tests/parity.py` loads it with `runpy`, sets
its home, cache, output and T3 constants to a scratch fixture root, and freezes
the clock. `TZ=UTC` fixes rendering timezone. `PYTHONUTF8=1` enables the Python
reference's UTF-8 file mode, an explicit baseline safety correction for Windows
where its legacy default encoding can corrupt Unicode handoff output. The
reference source remains unchanged. The subprocess environment removes configured provider homes and
self IDs so no command reaches a real store. Reference liveness probes are
replaced with an empty process inventory, never `os.kill(pid, 0)` on Windows.

`tests/fixtures/corpus.py` builds synthetic stores from the documented layouts.
All SQLite databases enter WAL mode during fixture creation, before either CLI
opens them. Tests compare hashes of database and transcript content before and
after execution. WAL coordination files are excluded because ordinary read-only
SQLite can manage shared memory; this is not permission to write application data.

The corpus covers five adapters, T3/provider merging, Unicode, corrupt JSONL,
an incomplete trailing record, tool calls, errors, user corrections, no-match
exits, and handoff artifact content. The runner checks legacy ls/find/resolve
JSON and show/tree/live/touched/failures text. List ordering, ID values, hit
ordering, missing keys and nulls remain significant. Only the scratch root and
equivalent RFC3339 spellings are normalized. Object member order is immaterial.
stderr is diagnostic rather than a byte-level compatibility promise.

Run a focused case with `--case resolve`, or validate the Python fixture alone
with `--reference-only`. A run fails on the first malformed JSON and reports
every ordinary output difference. `--schema` additionally checks all six v1
commands against the checked-in Draft 2020-12 schema using optional jsonschema.
The executable is also run with an empty PATH to catch accidental interpreter
or ripgrep runtime dependencies.

This fixture suite is not a claim of real-store parity. Before promotion, select
known inactive sessions and capture them into a new private directory:

```sh
python tests/fixtures/capture.py --home /home/user --output /private/parity-corpus --id SESSION_ID --id OTHER_ID
python tests/parity.py --snapshot /private/parity-corpus --query SEARCH_TERM --binary target/release/agent-traces
```

The capture helper opens source SQLite with mode=ro, verifies existing WAL,
backs up Codex and OpenCode using SQLite's backup API, and narrows discovery rows
in the scratch copy. T3 can be gigabytes, so it copies the selected thread,
project, runtime, message and activity rows with their original table schemas
under one read-only transaction. It copies only selected transcript files, rewrites Codex
rollout paths in the scratch database, and checks source transcript hashes and
timestamps for concurrent edits. A source change invalidates the capture.
Database backups are individually consistent, not a global transaction.
Physical backup files can retain unselected private database content; keep the
entire corpus private and outside git. The manifest records source transcript
hashes, host, capture time and selected IDs. Real-corpus mode compares listing,
search, resolve, and the last three rendered turns per selected session.
Preserve redacted counterexamples as new fixtures.
Measure cold and warm listing, broad and narrow search, live, resolve, and large
show on each available host. Linux/macOS CI cannot establish runtime parity on
the user's actual Linux/macOS stores.

Native Windows, macOS and Linux CI must pass format, clippy, unit tests and parity
before a binary is promoted. Release archive generation is manual and read-only
with respect to repository hosting. Publication, signing and tag pushes require
separate authorization. Both macOS architectures use native runners: `macos-15` for ARM64 and
`macos-15-intel` for x64, in CI and release smoke tests. Runner labels follow
the [GitHub runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

## Deliberate differences and boundaries

Windows process probes use query-only APIs and compare recorded FILETIME start
values to detect PID reuse. UTF-8 output is independent of the Windows code page.
Search uses an in-process prefilter, avoiding the reference's Windows command-line
length limit. SQLite databases must already use WAL; other modes are reported,
never converted. Malformed records produce diagnostics and v1 partial results.
Legacy stderr is not byte-identical and legacy JSON object key order is not an
interface guarantee.

The legacy parser accepts nonstandard JSON NaN/Infinity and lone surrogates that
serde_json rejects. Rust reports those records as malformed; it does not silently
coerce their contents. These nonstandard JSON extensions did not appear in the passing real-store
golden cases. Negative count flags retain Python slicing behavior and have
explicit golden cases.
