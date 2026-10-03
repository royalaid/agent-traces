# Rust port verification, 2026-09-26

This report records local verification before the first push. Current native
platform results are in [GitHub CI](https://github.com/royalaid/agent-traces/actions/workflows/ci.yml).

Host: a native Windows 11 desktop. Checkout: the `port/rust` branch.
The separate repository avoids adding a dependency lifecycle to the prose-only
skills repository. The skill edits are on the skills
repository branch `plan/agent-traces-rust`. Nothing was pushed or published.

## Scope and recommendation

Use the full Rust implementation for the Windows mailbox integration. All eleven
commands and the five existing adapters are implemented. Python is retained as
the frozen development oracle, without a runtime fallback. The six v1 commands
are ls, find, live, resolve, me, and handoff; legacy ls/find/resolve JSON Lines
remain available. Hermes still has no adapter.

Python was already adequate for a roughly 60-second inventory poll. The full
port follows the user's decision for portability, a self-contained executable,
and speed. Speedups are workload-dependent; broad historical search should not
be part of the periodic polling path. See [architecture](architecture.md),
[contract](cli-contract.md), and [parity boundaries](parity.md).

## Verification boundaries

SQLite opens use read-only flags, verify the existing WAL mode, and never change
journal mode. Tests exercise committed WAL visibility, rejected writes and
non-WAL stores, malformed records and cache reloads, alias retention after
merging/filtering, artifact source guards, and atomic replacement failure.
The multi-row live fixture first reproduced home-path spelling and T3 SQL-filter
ordering regressions, then passed after their correction.
Process queries never signal a process. Windows FILETIME start records are
compared exactly to reject reused PIDs; unknown formats remain unverified.

Native macOS ARM64 and x64 CI is prepared, with matching release smoke runners.
No remote Mac was used, per the user's instruction. Linux musl cross-build and
static linkage are verified; native Linux execution remains pending CI. WSL
startup failed with HCS_E_CONNECTION_TIMEOUT during the study and was not repaired.
CI jobs cannot run until a separately authorized push. Publication, signing,
and native-store benchmarks on the other hosts remain outside this local run.

The skills router index was regenerated and its check passes. Runtime links in
`.agents`, `.claude`, and `.hermes` resolve to the edited skill source. The global
scan also reports 22 skills installed nowhere and five other managed skill-copy
differences; none concern agent-traces.

## Measurement method

One warmup followed by three fresh subprocesses per engine and case; elapsed
wall time includes startup and captured output. Caches are warm, separate per
engine; OS caches are not flushed. Stores remain live and the desktop remains
in normal use, so these are comparative observations, not p95 guarantees.
Both implementations read the same existing WAL stores.

The Python source stays unchanged. UTF-8 mode avoids its Windows encoding error.
For broad search only, diagnostic in-memory instrumentation reduces rg batches
from 400 to 100 paths because the original fails with WinError206. For live only,
the diagnostic variant replaces its unsafe Windows signal probe with query-only
APIs. Rust has no such wrappers or runtime subprocess dependencies.

The initial Python profiles attributed about 72% of broad search to JSONL scanning
and parsing, 0.290s to SQLite calls, and only 2.6ms to BM25 scoring within a 15.833s
profile. Default listing, resolve and live were dominated by directory discovery.
Interpreter-only startup was 23ms; CLI help including imports/parser was 96ms.
Cumulative profile figures are nested and cannot be summed. The full baseline
and 48 timing runs remain in the skills repository's
`docs/research/2026-09-26-agent-traces-performance.md` and
`docs/agent-traces/performance-data.json`.

V1 initially required 10.74s for a full inventory. Isolated profiling on 2034 frozen
rows measured 452ms for identity projection and 2773ms for full session projection;
JSON serialization took 5ms. Repeated path canonicalization in sorting and repeated
alias scans were the bottleneck. Invocation-local canonical-path caching and
relationship indexes preserve output while avoiding repeated work. Fresh-context
projection fell to 317ms and identity projection to 7ms, with identical projected
JSON bytes on that frozen inventory.

## Final verification and artifacts

- `cargo fmt --all -- --check`, strict Clippy, and all 53 Rust tests pass.
- Final Windows release passes 36 synthetic Python/Rust golden cases, plus 17
  private captured-store cases, with zero differences. Every command is also
  exercised with an empty PATH. Fixture database/transcript contents stay unchanged.
- All six v1 command envelopes pass Draft 2020-12 validation and date-time formats;
  tests also assert statuses, exits, aliases, counts, missing-host and empty-store
  behavior. Full live v1 inventory and live observations validate successfully.
- The reference Python source is unchanged. The final full live inventory check
  matched all fields across 2038 rows; final timed workloads also match ordered
  identities. Installed live output exactly matches the safe Python reference.
  Counts grow as this host records more sessions during development.
- Windows executable: 5791744 bytes, statically linked MSVC runtime, bundled SQLite.
  Its only PE imports are Windows system libraries. Linux x64 musl executable:
  5450632 bytes, no ELF interpreter or dynamic program header.
- The installed `%USERPROFILE%\.local\bin\agent-traces.exe` matches the build
  SHA-256, resolves through PATH, reports `agent-traces 0.1.0`, and passes `where`.
  [Build hashes and imports](build-evidence.json) record exact artifacts.

After optimization, the actual full v1 inventory median is 1.173s for
2038 rows, versus 10.738s before, approximately
9.2x faster. The final response returned status `ok`,
`complete: true`, and no diagnostics. V1 live median is 0.543s with
4 observations and the same complete status. JSON schema validation ran
after, and is not included in, the CLI elapsed time. Inventory occupies about
2.0% of a 60-second interval in wall time, not measured CPU utilization.
[Raw v1 timing evidence](v1-performance-data.json) includes all observations.

The private real-store capture remains outside git at
`%TEMP%\agent-traces-private-parity-20260926`.
It is about 148MiB and contains five selected harness sessions plus linked aliases.
Do not publish it. Measurement scripts and intermediate evidence remain in
`%TEMP%\agent-traces-rust-study`.

## Final paired command timings

Median seconds; speedup is Python divided by Rust.

| Workload | Python | Rust | Speedup | Python / Rust peak working set MiB |
| --- | ---: | ---: | ---: | ---: |
| Full legacy inventory | 1.071 | 0.816 | 1.31x | 53.8 / 77.3 |
| find hermes --since 7d | 1.308 | 0.994 | 1.32x | 30.8 / 13.6 |
| find hermes, all history | 21.898 | 11.248 | 1.95x | 78.6 / 77.5 |
| Known Codex ID resolve | 0.319 | 0.435 | 0.73x | 26.4 / 10.1 |
| Show 366 MB transcript | 4.653 | 2.088 | 2.23x | 130.4 / 177.1 |
| Live, safe reference probe | 0.359 | 0.462 | 0.78x | 29.6 / 11.2 |

[Raw paired measurements](benchmark-data.json) preserve all 36 timed runs,
binary/reference hashes and identity checks. Live stores can change between
the sequential subprocesses; frozen-corpus tests establish exact ordering parity.
The broad Python search uses the explicitly described batch-size correction.

Rust is faster for content scans and large rendering, while known-ID resolution
and live probing can be slower despite using less memory. Listing and rendering
also use more peak memory in Rust on this corpus. Those are remaining optimization
opportunities, not reasons to retain a dual-language runtime. The chosen full port
delivers the requested native executable and stable machine interface.
