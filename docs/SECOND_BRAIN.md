# Engineering memory and measured effectiveness

Engram combines indexed repository references, cross-repository product knowledge,
task episodes, and evidence-backed lessons. Each serves a different purpose:

- A task source map points to relevant symbols, files, immediate dependencies and
  historical verification commands. It is bounded by the context budget. Inspect
  the referenced sources; an index is not proof that code behaves as described.
- Validated lessons retain their task and evidence provenance. Changes can make
  them stale. A judge cannot substitute its own assertion for source evidence.
- Product bundles connect documented capabilities, services, flows and contracts
  across explicitly connected repositories. Undocumented relationships remain gaps.

This is an inspectable engineering memory, not a guarantee that every indexed
repository is understood completely.

## Keep the task context useful

Native hooks retain the latest task prompt and a bounded set of files actually
used in each repository. A changed task or file scope can receive new context
without creating another episode. Reordering the same source pointers is suppressed;
changed source revisions, lessons, facts, or task scope remain deliverable. Each
hook checks current context even when the prompt and Git state are unchanged.
Only the latest successful pack is the suppression baseline, so removal followed
by restoration can deliver again. A failed delivery can retry. Relative tool paths use the tool's working directory,
so work across repositories stays attributed to the correct project.
Native hooks acknowledge context only after successfully emitting it. Skipped,
empty or identical compilations do not become task-delivery observations.

Source selection combines exact file and symbol matches with a local full-text
index. Code identifiers and paths are collected across a bounded 64 KiB of the
prompt before selecting prose terms. Rust named struct fields are searchable
through their owning type, and Rust impl methods have qualified names. Field
aliases are bounded to 128 fields and 16 KiB per owner; other languages retain
their existing symbol indexing. Packs still include at most one pointer per file.
Known edit paths do not exclude task-relevant files elsewhere in the
repository or explicitly linked repositories. Direct matches precede graph
neighbors; unresolved references do not become asserted dependencies. Source
pointers omit opaque IDs and long call expressions from prompt text while keeping
their durable provenance. Indexed references still require checking the live file.

Ordinary learned lessons need a task or structural relevance signal. Confidence
alone does not make a lesson relevant. Validated project preferences remain
available as standing conventions. Relevant source pointers reserve a bounded
share of the context budget, so a large memory collection cannot crowd them out.

`engram_recall` sends the compiled text once and returns a small structured
`context_run_id` handle with item count and token estimates. Detailed scoring and
provenance are available through `engram_explain_context`, rather than repeated in
every recall. CLI and daemon inspection still expose the full audit record.

The repeatable source-navigation diagnostic compares binaries against the same
frozen index and authored queries. It reports intended-file and definition hits,
irrelevant-query filler, context size, latency, budget compliance and zero-injection
controls. It runs on a disposable database with live checkout resolution disabled:

```sh
caffeinate -dims python3 -B scripts/eval_context_quality.py \
  --daemon /path/to/engramd --snapshot /path/to/frozen.sqlite \
  --cases tests/fixtures/context_quality_cases.json --repeats 3 \
  --output /tmp/context-quality.json
```

Use `--freeze-from /path/to/brain.sqlite` once to create a new consistent, read-only
snapshot. Reuse that snapshot for both binaries. These probes measure retrieval;
provider cost and engineering outcomes still require paired agent tasks.
See the [measured context comparison](perf-evidence/2026-09-24-context-quality/README.md)
for the recorded inputs, results and remaining definition misses.
The [real-repository task pilot](perf-evidence/2026-09-24-real-repo-pilot/README.md)
measures completed engineering attempts with private behavioral graders, actual
provider telemetry and native tool counts. Its progress marker identifies whether
the planned matrix is complete. The accompanying
[context audit](perf-evidence/2026-09-24-real-repo-pilot/CONTEXT_AUDIT.md) examines
which source pointers were delivered and repeated; retrieval scores alone do not
establish task-level savings. The
[follow-up improvements](perf-evidence/2026-09-25-context-followup/README.md)
compare the fixes against the prior binary using frozen source-navigation probes
and full historical task descriptions. `scripts/eval_task_context.py --reindex`
updates only a disposable database and historical checkout, then deletes that
checkout before measuring retrieval. Native extractor upgrades also refresh
unchanged files once when an existing repository is next indexed. The v1-to-v2
Rust upgrade preserves symbol identities and lesson anchors in unchanged files.
If a Rust file changed since its v1 scan, its old symbol identities are retired
conservatively during that first upgrade; attached lessons require fresh evidence.

## See what is working

Open **Effectiveness** in the local web application, or run:

```sh
engram effectiveness --days 7
engram effectiveness --days 30 --json
engram judge status
```

Reports include compiled contexts by mode, selected source kinds, contexts containing
validated memories, task/context usage attribution, active lessons and pending reviews.
Compilation does not prove the coding agent received, followed or benefited from a reference.

### Delivered context use and Read volume

Native hooks also record which repository files each agent tool call reads or edits.
A call is identified by the agent's session and tool-use IDs; calls without them are
not recorded. Each record keeps the tool name, an action class, repository-relative
paths and, for Read calls, whether `offset`, `limit` or `pages` was supplied, the size
of the text the tool returned and whether the agent's size limit rejected or truncated
it. File content, shell commands and error text are never stored. The pre-tool hook
records a touch before compiling any context that touch triggers. Records follow the
`[jobs] context_run_retention_days` window.

Each repository report (`context_use` in `--json`) compares delivered file pointers
with later reads and edits in the same agent session:

- **Precision**: pointed files read or edited after delivery, divided by pointed files
  not already touched when the context was compiled. Already-touched files are counted
  separately; a context compiled for the file being read cannot claim that read.
- **Coverage**: of the files first read or edited after a context for their repository
  was delivered in the session, the share that an earlier context had pointed to. A
  pointer from a linked repository's context also counts.
- **First-touch distance**: observed tool calls from delivery to the first touch,
  including the touching call.
- **Read volume**: ranged-read share, mean result bytes, size-limit hits and failures.
  `brain` covers calls made after a context was delivered in that session and
  repository. `unattributed` covers calls made before; it is not a control group.

These are associations, not causation: an agent may have found a pointed file without
the pointer. Contexts without a later observed tool call, contexts linked to no agent
session, and every context delivered before tool observation existed are reported as
unobserved and stay out of precision. Pointers resolve through their indexed node to a
file path, so line ranges do not affect attribution; pointers that cannot be resolved
are counted separately. Only Read and edit tools with explicit file arguments count as
touches. Shell commands are not parsed for paths, so shell reads, including most Codex
reads, are not attributed and do not appear as Read calls. Files created during a
session count as touched although no earlier context could point to them. Failed reads
still count as touches. Calls outside tracked repositories are not observed, so
first-touch distances can undercount. Result bytes measure the text content the tool
reported, not provider tokens; image, PDF and unknown results have no size.

Only sessions running updated hooks are observed: earlier sessions stored no touched
paths, so historical contexts stay unobserved. Hooks send observations to
`POST /api/v1/effectiveness/tool`. Another harness can send one per call and phase
(`pre`, `post` or `failure`); repeats of the same call merge, keeping the earliest
time:

```json
{
  "project_id": "prj_REPLACE_WITH_REAL_UUID",
  "session_id": "agent-session-id",
  "tool_use_id": "agent-tool-call-id",
  "source": "my_harness",
  "tool_name": "Read",
  "phase": "post",
  "paths": ["src/lib.rs"],
  "ranged": true,
  "result_bytes": 4096,
  "limit_hit": false
}
```

The usage ledger records reported input/output/cache tokens, tool calls and cost.
Missing values stay unknown. Coverage appears beside totals; partial observations
are not the full cost of a session. Cache token categories remain separate because
providers define input tokens differently. Context-size token estimates are separate
from provider token measurements. Local inference cost is unknown unless reported.

Hook installation adds automatic usage capture for supported Claude and Codex
transcript records, plus identifiable completed tool calls. Stop/SessionEnd reads
bounded transcript data and persists usage numbers, not transcript text. Unsupported
or absent usage remains missing. Existing sessions need the refreshed hooks loaded
by their coding client. Run the appropriate installer after upgrading:

```sh
engram hooks install --global
engram hooks codex --install
```

Whole-session provider totals are omitted when a session spans multiple repository
episodes; Engram cannot truthfully divide that spend between repositories. Per-tool
measurements remain scoped to the target repository. Historical transcript totals
are never retrospectively attributed to the latest context bundle or a brain/off mode.
SessionStart establishes a baseline of existing transcript records. If that event
was missed, the first usable Stop establishes the baseline without counting old
records; subsequent new records are eligible. Ambiguous intervals are skipped.
Files above 32 MiB and lines above 2 MiB are not parsed for usage.

Other harnesses can send one immutable, additive observation per response through
`engram_record_usage`, `POST /api/v1/effectiveness/usage`, or `engram usage sample.json`:

```json
{
  "sample_id": "response-unique-id",
  "project_id": "prj_REPLACE_WITH_REAL_UUID",
  "source": "my_harness",
  "metrics": {"input_tokens": 1420, "output_tokens": 230, "tool_calls": 3}
}
```

Use stable IDs and include task/context/session IDs when actually known. Repeating
an identical sample is safe; changing an existing sample is rejected. Do not combine
cumulative session totals with their individual responses. Do not count the same
provider response through both a custom harness and the transcript adapter.

Judge calls appear separately under `internal_judge`, including provider usage
returned with a malformed verdict. This makes the cost of maintaining the brain
visible alongside coding-agent usage.

Daily totals cannot establish savings. The benchmark section compares only matching
off/brain task-seed pairs with identical model, configuration and brain snapshot.
The view separates all-attempt outcomes and observed resource totals from comparisons
where both arms completed successfully. Failed runs remain in the denominator;
infrastructure errors and ambiguous grades cannot award either arm a correctness win.
Missing metrics remain unmeasured. Partial usage stays in observed totals but cannot
enter paired savings. Duplicate or incomplete pairs are excluded from paired deltas
and retained in attempt counts. Repetition labels need not be deterministic model seeds.

The [real-task importer](../benchmarks/real-repo-v1/README.md) validates a completed
24-run report before writing evaluation records through the daemon. It preserves
original behavioral verdicts, reviewed exclusions, control hashes and measurement
coverage. These records appear in unfiltered Effectiveness reports, separately from
everyday usage samples and learned memories. Identical retries are idempotent; changed
evidence under an existing run ID is rejected. These historical replays are diagnostic
evidence; use representative tasks and repeat trials before drawing broad conclusions.

## Automatic evidence review

The background judge examines pending proposals from completed episodes. It receives
a redacted, bounded packet of the original observations, source revisions, task and
related memories. It can accept a supported lesson, reject it, or defer when uncertain.
Only eligible original evidence can promote a lesson. Exact citations and a fresh
source digest are checked again immediately before applying the decision. Touched
files are checked against the live checkout, with bounded reads, rather than relying
only on an index refresh. Changed or unavailable sources cause deferral.

Touched paths can be relative to the checkout or absolute. The judge reads a path only
inside a registered checkout of the project, including linked worktrees, or of an
explicitly linked repository. Symlinked spellings, such as `/tmp` and `/private/tmp`,
resolve to the same checkout. Any other path blocks automatic review. Relative paths
use the checkout the task worked in: the one named by absolute paths, else a checkout
whose HEAD matches the recorded task revision, else the checkout holding the recorded
files, preferring the primary checkout. Abbreviated revisions of at least seven
hexadecimal digits match. Each file must match its indexed content. Deferral reasons
distinguish a missing file (deleted, renamed or not a path), a checkout or file the
daemon may not read (including macOS privacy refusals; `engram doctor` shows the fix),
another I/O failure, a file that differs from the index and a file without an index
anchor.

Evidence must also have observed the indexed version of every touched file. It
qualifies when it was recorded after that file was indexed, or when a native hook
recorded the file's SHA-256 beside the verification and it equals the indexed
digest. A commit and the watcher's re-index therefore keep a hook-observed run of
the same bytes eligible. Evidence reported by agents carries no hook digests and
must be newer than every touched file's indexed version. The hook records only
runs whose exit status the client reported; see the user guide on
[verification evidence from hooks](USER_GUIDE.md#verification-evidence-from-hooks).

Hook provenance (a hook `source` label and `source_digests`) is attached only
through the daemon's hook evidence endpoint, which the CLI and MCP do not use;
the generic evidence endpoint removes those keys from client-supplied metadata.
This protects against accidental forgery, for example `engram task evidence
--metadata` copying a hook record. It is not a security boundary: any process
running as the same user can reach the daemon socket and its hook endpoint.

Every decision, citation, attempt, error and reported usage is audited. A review is
judged again only when its proposal, task or evidence changes, or when a source check
reaches a different result. Index scans, memory recency and unrelated commits do not
start another review. Unchanged deferred proposals are not repeatedly billed; attach
new evidence or revise the proposal to trigger another review. Runtime errors and
results invalidated by a source change during inference have a bounded retry count.
Malformed model output counts as a runtime error, not a decision.

If the judge names related memories that conflict with a supported or uncertain
proposal, it marks those active memories suspect and leaves the proposal pending.
Suspect memories are not injected, and nothing is deleted. The judge does not decide
that proposal again; a person reviews it in **Knowledge**. Accepting it with the
contradiction checked marks the old memory contradicted. Any other decision returns
the quarantined memories to active.
Human review remains available. Neither automatic nor human lesson approval implies
the user accepted the task. Previously quarantined memories need fresh evidence and
a new proposal; they are not silently revalidated.
Graceful daemon shutdown lets an in-flight inference finish and records its audit
and reported usage; it does not start the remaining items in that batch.

To use an existing local OpenAI-compatible model, add this to the applicable config:

```toml
[judge]
enabled = true
provider = "local"
endpoint = "http://127.0.0.1:8010/v1/chat/completions"
model = "YOUR_RUNNING_MODEL_ID"
batch_size = 2
interval_seconds = 300
```

The runtime must already be running. Engram does not download or launch a model.
The adapter disables redirects, enforces loopback and bounds request/response size
and time. A model's semantic judgment can still be wrong; inspect audit samples and
test on held-out supported/unsupported lessons before trusting a new model widely.
Proposals whose text or evidence matches prompt-injection patterns are held for
human review and never sent to the model; see [SECURITY.md](SECURITY.md).

Requests carry the verdict JSON schema as an OpenAI-compatible `response_format` of
type `json_schema`. The schema allows only the packet's evidence and memory IDs, so a
runtime that enforces it cannot produce unknown fields, empty claims or invented IDs.
Engram still parses and validates every answer strictly. For a runtime without schema
support, set `structured_output = "json_object"` under `[judge]`.

A small Apple Silicon runtime can serve the local judge separately from a coding
model. The tested MTPLX configuration uses a 4-bit Qwen3.5-4B model on port 8011:

```sh
mtplx pull Youssofal/Qwen3.5-4B-MTPLX-Optimized-Speed
MTPLX_SESSION_BANK_MAX_BYTES=512M \
MTPLX_SESSION_BANK_PER_SESSION_BYTES=256M \
MTPLX_SESSION_BANK_MAX_ENTRIES=2 \
MTPLX_REQUEST_LOG_JSONL=off MTPLX_FLIGHT_RECORDER=off MTPLX_FLIGHT_TEXT=off \
mtplx serve --model Youssofal/Qwen3.5-4B-MTPLX-Optimized-Speed \
  --host 127.0.0.1 --port 8011 --model-id engram-judge \
  --reasoning off --no-stats-footer --ssd-session-cache off \
  --max-tokens 1200 --context-window 16384 --fan-mode default
```

Install MTPLX's `server` extra in its runtime environment: JSON-constrained output
requires `llguidance`. Point `[judge]` at port 8011 and model `engram-judge`, then
restart the Engram daemon. The cache limits above bound retained prompt snapshots,
not total process memory. MTPLX can print completion previews to stdout; avoid
retaining stdout for a persistent service when those previews are not needed.

Before changing model, runtime or prompt, run the synthetic evaluation:

```sh
python3 scripts/eval_local_judge.py \
  --endpoint http://127.0.0.1:8011/v1/chat/completions \
  --model engram-judge --output /tmp/engram-local-judge-eval.json
```

The harness makes no brain writes. Its exit status fails for incorrect decisions,
malformed outputs and runtime errors, and its report keeps raw decisions separate
from citation validation. It uses the production schema; `--response-format json_object`
reproduces unconstrained output. See the [measured local setup and limitations](perf-evidence/2026-09-24-local-judge/README.md)
and the [structured-output comparison](perf-evidence/2026-10-04-judge/README.md).

TypeSafe's Jev uses a hosted typed-decision API. The optional adapter asks atomic
questions about reuse, scope, compatibility and support by exact evidence excerpts.
It preserves the original claim and defers unless every gate and at least one source
meet the configured threshold. The threshold is a policy, not measured accuracy.

```toml
[network]
local_only = false

[judge]
enabled = true
provider = "typesafe"
allow_remote = true
endpoint = "https://api.typesafe.ai/v1/systemone"
model = "jev-1.13.0"
acceptance_threshold = 0.98
```

Set `TYPESAFE_API_KEY` in the daemon environment before starting it. Redaction removes
recognized secrets, but packets can still contain proprietary source and observations.
Remote review is explicit opt-in. Restart the daemon after configuration changes and
use `engram judge run` to queue a pass. See TypeSafe's [API reference](https://docs.typesafe.ai/api)
and [model catalog](https://docs.typesafe.ai/models) for current service details.

## Workspaces

All existing repositories remain in **default**, using the existing state paths.
Separate workspaces have isolated databases, sockets, derived state and vaults:

```sh
engram workspace list
engram workspace show
engram --workspace client-a index /path/to/repository
engram --workspace client-a effectiveness --json
```

Selection order is `--workspace`, `ENGRAM_WORKSPACE`, project `.engram/config.toml`,
then global configuration; absence means `default`. Named workspace profiles live
at `~/.engram/workspaces/NAME/config.toml`. Names use lowercase letters, digits,
hyphens or underscores and begin with a letter or digit.

To select a workspace consistently for a repository's hooks and agent tools:

```toml
# .engram/config.toml
workspace = "client-a"
```

Start MCP/proxy with the same workspace (for example `engram-mcp --workspace client-a`);
a new process also resolves workspace configuration from its working repository.
A long-running MCP connection serves its selected workspace. Set `ENGRAM_WORKSPACE`
for all related processes when a session should stay in one workspace.

Named workspaces do not inherit the default database, socket or vault. Explicit paths
must stay within the named workspace. Their web listener is disabled unless configured
with its own port, because two daemons cannot share the default listener:

```toml
# ~/.engram/workspaces/client-a/config.toml
[web]
enabled = true
bind = "127.0.0.1"
port = 11446
```

Workspace separation is a knowledge boundary, not an OS security sandbox. References
do not cross databases. The same filesystem repository can still be indexed in more
than one workspace when deliberately selected.
