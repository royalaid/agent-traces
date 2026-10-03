# Agent-traces CLI v1 contract

Implemented by the Rust CLI. The frozen Python reference supplies legacy
behavior and does not implement v1.

Read [cli-v1.schema.json](../schemas/cli-v1.schema.json) for exact fields and
[cli-v1.examples.json](../schemas/cli-v1.examples.json) for synthetic envelopes covering
all six commands, empty results, ambiguity, partial coverage, and errors.
The schema's HTTPS identifier is a proposed identifier, not a deployed service;
validation uses the checked-in file and local references.

## Activation and framing

Preserve existing arguments, text output, exits, and legacy `--json`.
Legacy `ls --json`, `find --json`, and `resolve --json` remain JSON Lines.
Use `--json --schema-version 1` for ls, find, live, resolve, me,
and handoff. It emits exactly one UTF-8 JSON object followed by a newline,
including for empty results. This explicitly selected format is a document,
not JSON Lines of session rows. Emit no banner, Markdown, progress, or
confirmation on stdout. Human diagnostics may also appear on stderr.

Machine handoff defaults to stdout with no artifact write. An explicit
`--output` writes the same envelope atomically to that path, leaves stdout
empty, and keeps the documented exit code. It never writes into a store.
Legacy handoff continues to produce its Markdown file.

An incomplete or unparsable envelope is a transport failure; consumers never
treat it as an empty successful scan. Buffer the envelope before emitting it.
Fatal failures before argument parsing or serialization may produce only
stderr and a nonzero exit. Consumers check both exit status and payload.

## Identity, time, and paths

A session identity is the tuple `host_id, store_id, harness, id`.
IDs are opaque strings, not necessarily UUIDs. The caller supplies a stable
host identifier through `AGENT_TRACES_HOST_ID`; absent it, v1
fails before scanning with invalid_argument on stderr and exit 2. This
preflight failure emits no envelope because a valid host_id is unavailable.
Choose different host IDs for Windows and WSL
store namespaces even when they share physical hardware.

Store IDs are explicit configured labels when provided, otherwise the
canonical absolute store root (JSONL) or database path (SQLite). They are
stable while that path is stable. Moving a store requires a configured label
to preserve identity. Case comparison follows the host filesystem; retain
the observed spelling in emitted paths. The schema permits future harness
and session-kind strings without changing the shape.

Source paths are absolute native paths. Database row selectors are separate
strings, such as `thread=<id>`, rather than appended to a fake filename.
`home` is absolute. Treat cwd and source paths as host-local opaque values;
do not rewrite a Windows path into a WSL path. Null means unknown, not empty.
Every schema field is required; nullable fields are emitted as null.

Timestamps are UTC RFC3339 with a trailing Z, six fractional digits, and
microsecond precision. Parse legacy source timestamps by the reference
rules, then normalize. Unknown or invalid timestamps are null. The schema
checks date-time form, the UTC suffix, and exactly six fractional digits.

Aliases preserve duplicate sources, parent references, T3 thread references,
and provider references with complete identities. A T3 thread and provider
session can describe one conversation but retain distinct identities.
A provider absent from discovery has no invented store identity or alias;
retain its raw provider/provider_id in attributes. Resolve all known store
identities before forming aliases. Keep a nullable legacy parent ID for
display; use a parent alias for routing when its store is known.

## Status, coverage, and exits

| Exit | Status | Meaning |
| --- | --- | --- |
| 0 | ok | Requested operation completed; ls may have zero rows |
| 1 | not_found | Complete search found no match, no live observation, or no resolvable self |
| 2 | error | Invalid arguments or fatal processing failure; result is null |
| 3 | partial | Usable result with unreadable/busy stores or skipped relevant corrupt records |
| 4 | ambiguous | Complete resolve found multiple candidates, or handoff could not select one |

The result shape is selected by command. Handoff with no matching session
uses status not_found, complete=true, result=null, a not_found diagnostic,
and exit 1. Handoff ambiguity uses status error,
exit 4, result null, and an ambiguous_identity diagnostic because no handoff
exists. Its diagnostic message directs the caller to resolve for candidates.
Resolve ambiguity uses status ambiguous and its candidate-bearing result.
Exit 2 therefore ordinarily accompanies error; exit 4 is its one documented
handoff exception.

`complete=false` applies to partial and error only. Coverage lists each
selected configured/discovered store and requested capability, including
absent or unsupported capabilities. Absent stores and unsupported liveness
are normal coverage outcomes; they do not imply partial failure. Unreadable
or busy selected stores, or skipped malformed records that could affect the
answer, do. Fatal failure takes precedence over partial; partial takes
precedence over not_found and ambiguous. Partial resolve may still have
resolution ambiguous or not_found, but neither conclusion is exhaustive.
Partial handoff may have result=null when incomplete coverage prevents
selection of a unique session or no usable extraction is available. It
still carries diagnostics, complete=false, status=partial, and exit 3;
it does not report an exhaustive no-match or ambiguity outcome. When
selection is ambiguous, retry the unavailable store and rerun resolve to
inspect candidates before requesting a handoff. A usable partial extraction
retains the full handoff result shape and its unverified designation.

A complete scan is complete for the reported capability and selected stores,
not proof that all sessions on the machine are discoverable. In particular,
unsupported live coverage never means that no sessions run in that harness.
Empty results with partial coverage exit 3.

Live SQLite reads use ordinary read-only connections and SQLite's normal
read coordination, as authorized. Existing WAL mode is required; all four
local database stores checked during this investigation already use WAL.
A non-WAL live store is skipped without changing its journal mode. Report
unsupported_storage_mode in both the diagnostic code and coverage status,
set complete=false and status=partial, and exit 3. This is distinct from an
unsupported harness capability. Never bypass coordination with nolock or
claim a changing database is immutable.

Diagnostic codes and coverage states are enumerated in the schema. Use
stable codes for branching, human messages for explanations, and retryable
only for a plausible transient failure. Preserve diagnostic information in
the envelope even when stderr is redirected.

## Command semantics and order

All tie breaks use identity tuple fields in ascending Unicode code-point
order. Canonical JSON object key order is not a contract.

- ls: default seven-day window, limit 30. Order updated descending, falling
  back to started, with unknown timestamps last. total counts matches before
  the limit; truncated is total greater than emitted count.
- find: default prompts scope, no date window, limit 15. Preserve the Python
  candidate filtering, substring term counts, skipped-line corpus statistics,
  role weights, title boost, and self exclusions under
  agent-traces-python-bm25-v1. All terms must occur somewhere across scoped
  messages of a session. Rank relevance by unrounded score descending then
  updated descending. Emit the score rounded to two decimals using Python's
  reference rounding semantics. recent uses title-match, first-prompt-match,
  body-match order then updated descending; oldest uses earliest selected-hit
  timestamp ascending, with unknown first as in Python. Apply the identity
  tie break last. Return up to five hits per session, preserving Python
  full-term-message preference and message order for tied scores.
- resolve: preserve prefix and path resolution, minimum six-character ID
  prefixes, and known T3/provider merging. Candidates sort by identity.
  resolution is resolved for one candidate, ambiguous for two or more,
  not_found for none. Complete envelope status agrees with this result.
- live: order by identity, evidence, then PID, null PID last. Keep separate
  Claude PID and T3 runtime observations. A PID without verified start time
  has lower confidence than a verified identity. T3 running is
  provider_reported, not OS process proof. Permission failures produce
  unverifiable/unknown observations where identity is known and diagnostics.
  Query processes without signals or termination permissions.
- me: identifiers sort by environment variable and ID; sessions sort by
  identity and deduplicate identical identity tuples. Retain unresolved
  identifiers with resolved=false. Return not_found if no sessions resolve.
- handoff: select a unique session using provider preference. Preserve
  chronological request order and extracted command/error order. Preserve
  first-seen file order, Python ticket ordering, lexical PR URL order, and
  identity order for children. Counts describe all extracted items before
  truncation; the truncation flags explain omitted items. Per-text clipping
  is recorded in each extract. last_plan is rendered text, not arbitrary
  unredacted tool JSON. All content is explicitly unverified.

Negative limits are invalid in v1. Limit zero is valid and returns no rows
with total and truncated still describing the full query. Other option
defaults remain the Python reference defaults. Limits select output rows,
not a promise to stop scanning early.

## Privacy and trust

Apply the current reference redaction rules to every free-text field:
metadata titles and first prompts, hits, last prompts, extracted handoff
text, and diagnostic messages. Call this frozen rule set
agent-traces-redact-v1; changes to its behavior require a documented contract
revision. Redaction is best effort, not a guarantee that every secret is
removed. Identity IDs and native paths remain useful routing data and can
themselves be private. Store envelopes locally under caller-controlled
permissions. Do not embed raw provider cursor JSON, environment dumps, or
unfiltered tool inputs. The handoff verification constant is always
unverified; extraction does not verify repository, ticket, or test state.

Legacy JSON currently leaves row metadata unredacted while redacting find
snippets. Preserve that legacy behavior for drop-in compatibility; the
explicit v1 mode adopts the stated broader rule.

## Versioning and adoption

Pin `schema_version="1.0"` and reject unknown major versions. The schema
uses closed objects so accidental new fields fail producer validation.
Changes to field shapes, framing, required keys, ranking semantics, exits,
redaction, or null rules require a reviewed version revision and fixtures.
For v1.0, adding a field requires updating the schema and negotiating a new
minor version rather than silently emitting it to pinned consumers.

Implement v1 in Rust alongside legacy compatibility. Keep Python as the
reference for existing behavior; do not require a production Python v1
implementation as a prerequisite. A test-only projector over reference
rows/events supports comparisons of inherited semantics, while independent
fixtures check newly specified v1 behavior. Compare v1 semantic outputs after
only documented nondeterminism normalization:
observed_at and configured host/root mappings for frozen fixtures. Do not
normalize away ordering, missing fields, ambiguity, coverage failures,
redaction, or score differences. A parser accepting the output is not
sufficient evidence of compatibility.

## Legacy inventory and known differences

Audit basis: scripts/agent_traces.py on the planning branch before any port.

| Area | Current behavior | Source lines |
| --- | --- | --- |
| Session JSON | Copies adapter fields; started/updated normalized to ISO; default=str; no schema tag | 1392-1397 |
| Shared fields | harness, home, id, parent, kind, cwd, title, first_prompt, started, updated, model, path | 522-528, 767-782, 930-939, 1045-1053, 1167-1175 |
| Extra fields | Claude entrypoint/agent_type/copies; Codex archived/originator/nickname; T3 provider_instance/provider/provider_id/status; Grok originator | Same adapter sections; 575-600 |
| Missing fields | Codex unindexed fallback lacks branch and archived | 811-820 |
| T3 merge | Provider row gains t3_thread, t3_title, provider_instance | 1263-1277 |
| ls | JSONL; zero rows exit 0; discovery-order ties | 1420-1430 |
| find | JSONL; _score retained, _match removed, five hits regardless of --hits; no matches exit 1 | 1452-1566 |
| Search terms | Terms may span messages, despite help claiming one message | 1510, 2057 |
| resolve | JSONL; multiple matches exit 0; no matches exit 1 | 1860-1877 |
| live | Text only; Claude PID and T3 running; no observations exit 1 | 1967-2022 |
| me | Text only; IDs present but unresolved can exit 0 with no output | 2025-2033 |
| handoff | Markdown artifact by default; unique-session selection exits 2 on ambiguity | 1329-1351, 1892-1951 |
| Errors | Many adapter failures become stderr plus empty rows; no completeness field | 748-760, 905-921, 1028-1038 |
| Paths | Homes often tilde-shortened; SQLite path values contain # selectors | Adapter row constructors |
| Time | Existing offsets remain offsets; precision varies | 65-86, 1392-1397 |

None of these documented legacy differences authorizes production changes
during the planning phase.
