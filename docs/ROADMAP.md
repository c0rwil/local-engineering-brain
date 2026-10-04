# Engram roadmap

Updated 2026-10-04. This roadmap uses the
[roadmap.sh AI Engineer roadmap](https://roadmap.sh/ai-engineer) as a checklist and
maps it onto Engram's thesis (spec §0): a local software agent should improve
through verified experience, and Engram should be able to prove when that
experience improves engineering outcomes.

## Where the loop stands

Engram's value depends on four steps: capture work, validate lessons, deliver
context and prove the effect. Measured on 2026-10-04:

| Step | State | Source |
|---|---|---|
| Capture | Claude Code hooks capture continuously. Codex has never reached Engram: its six hooks are installed but were never trusted in Codex, so there are 0 Codex tasks and 0 Codex usage samples. Until a restart on 2026-10-04, the daemon could not read repositories under `~/Documents`, because it inherited the macOS privacy permissions of the process that auto-started it. One repository is registered as two projects (`brain` and `engram`). | live database, `engram_project_status` |
| Validate | The judge recorded 1,557 decisions for 140 reviews: 11 accepted, 0 rejected, 1,541 deferred. 1,470 deferrals never reached the model; 1,040 cite missing independently verified evidence. Unchanged reviews were re-judged up to 129 times. | `lesson_judgments` |
| Deliver | Works for Claude Code: agi-backend-rust received 1,814 context runs in seven days, 1,608 of them with lessons. | `engram effectiveness --days 7` |
| Prove | The v2 real-repository follow-up (24 of 24 runs, none unscorable) found no improvement: Off 9/12 clean passes, Engram 8/12. On both-pass pairs Engram used 15.5% fewer tool calls but 17.2% more provider tokens, because agents followed file-level pointers with whole-file Reads. | [follow-up findings](perf-evidence/2026-09-25-real-repo-followup/FINDINGS.md) |

## How the roadmap.sh topics map to Engram

| roadmap.sh section | Role for Engram |
|---|---|
| Context Engineering: context sources, memory systems, RAG and dynamic filters, compaction, long-context processing, state and historical context, context failure modes, context evaluation, context security | The product surface. Engram is a context layer for coding agents. |
| LLM Evaluations (deterministic, model-based and human evals; metrics), Regression Testing, LLM Observability (tracing, cost and latency) | How Engram proves or disproves its value and catches regressions. |
| AI Safety and Ethics: prompt injection, security and privacy, adversarial testing, constraining inputs and outputs | Guardrails for injected lessons and for the judge. |
| Prompt Engineering (structured output), Choosing the Right Model, Embeddings | Judge reliability and optional semantic retrieval. |
| Development Tools (Claude Code, Codex, Cursor), Model Context Protocol, multi-agent context sharing | Where Engram reaches agents. |

Not adopted: multimodal AI; hosted vector databases (a SQLite sidecar is enough
for the current 74 memories); agent frameworks such as LangChain or LlamaIndex;
and fine-tuning, which stays a longer-term goal (spec §4.3) until enough
validated trajectories exist.

## Workstreams

Each workstream lists its roadmap.sh topics, the work, and the measurement that
closes it.

### 0. Capture plumbing

Topics: Development Tools, MCP host and client.

- `engram doctor` reports Codex hooks that are installed but not trusted, daemon
  privacy-permission failures per registered repository, and registered projects
  that share one repository identity.
- Errors caused by macOS privacy denials name the cause and the remedy instead of
  a generic I/O error.
- Operator actions: trust the Engram hooks in Codex with `/hooks`; restart the
  daemon from a terminal that can read the repositories.

Closed when Codex sessions produce tasks and `codex:` usage samples, and doctor
passes for every registered repository.

### 1. Evidence and judge

Topics: model-based, deterministic and human evals; structured output; choosing
the right model.

- Constrain verdicts with the JSON schema and classify malformed output as a
  model failure, not a configuration error.
- Fix false path-containment failures and stop re-judging unchanged reviews.
- Make test, build and lint runs that hooks actually observe eligible evidence,
  without accepting agent narration.
- Calibrate the local judge on the labeled evaluation set and spot-check a
  sample of real decisions by hand.

Closed when pending reviews resolve to accept or reject at a measured precision,
with no format errors and no repeat judgments of unchanged reviews.

### 2. Context efficiency

Topics: context failure modes, long-context processing, context evaluation, RAG
and dynamic filters, embeddings.

- Line-ranged pointers (`path:start-end`) so agents read spans instead of whole
  files. This targets the mechanism behind the v2 token increase.
- Related-use cues that point at other users of a definition being changed.
- Delivered-and-used attribution and Read-volume metrics in Effectiveness.
- A local embedding provider behind the existing `EmbeddingProvider` seam, enabled
  only after the frozen retrieval probes show a gain.

Closed when the frozen retrieval probes hold or improve while emitted and
followed context shrinks, and a paired run shows equal or better correctness
with lower Read text and cache-read tokens.

### 3. Proof

Topics: LLM evaluations, regression testing, LLM observability.

- A hermetic retrieval regression test in CI.
- A Codex arm in the real-repository runner.
- A held-out task suite. The v1 and v2 tasks informed development, so they
  cannot show generalization.

Closed when a paired matrix on held-out tasks, for both Claude Code and Codex,
reports correctness first and every token category.

### 4. Safety

Topics: prompt injection, security and privacy, adversarial testing,
constraining inputs and outputs, context security.

- Detect instruction-like and exfiltration content in lesson proposals and route
  it to human review.
- Keep injected context framed as data, and have the judge treat packet content
  as data.
- Adversarial tests through the real promotion and rendering paths, plus
  adversarial cases for the judge evaluation.

Closed when the adversarial suite runs in CI and no planted instruction is
promoted.

### 5. Reach

Topics: Development Tools, MCP, multi-agent context sharing, context isolation.

- Attribute Codex guardian sub-sessions to their parent session.
- Cursor hooks and MCP registration as an explicit opt-in.

Closed when each supported agent has a verified install path and sub-sessions
create no duplicate tasks or usage.

## Order

Workstreams 0 and 1 come first: without capture and validation there is nothing
to deliver or measure. Workstream 2 targets the one cost mechanism the v2
benchmark measured. Workstream 3 decides whether the changes help. Workstreams 4
and 5 proceed in parallel because they do not depend on those measurements.

## Progress on 2026-10-04

All six workstreams have a first implementation. None is closed: each closing
measurement above still needs live data or a paired agent run.

| Workstream | Landed | Measured | Open |
|---|---|---|---|
| 0. Capture | Doctor checks `codex_hooks`, `checkout_access` and `project_identity`; specific macOS privacy errors; `engram daemon restart`; identity backfill for metadata-pinned projects | The Codex trust rule reproduces stored hashes and Codex's own `hooks/list` (CLI 0.155.0-alpha.2.6) | Hooks must be trusted in Codex by the operator. No safe project merge exists, because `project_aliases.path` is unique |
| 1. Evidence and judge | JSON-schema verdicts; malformed output is a runtime error; containment across every registered checkout; one reason per cause; stable dedupe (judge policy v2); contradictions mark memories suspect; hook-observed verification becomes evidence; abbreviated revisions count in verified evidence | The old hook recorded 0 of 1,879 observed verification runs. Judge accuracy 14/15 on synthetic cases, a smoke test only. A same-snapshot replay gives accurate reasons, but still only 2 of 135 pending reviews pass every gate | 97.8% of observed agent test commands hide their exit status behind pipes. Existing reviews cannot be backfilled. Agent-reported evidence stays eligible, a policy decision |
| 2. Context efficiency | Line-ranged pointers, checked against indexed digests; related-use cue; delivered-and-used and Read-volume metrics; optional local embeddings, off by default | Frozen probes: direct selections unchanged, 231 of 252 pointers ranged. Required pilot components in the pack rose from 13/35 to 19/35, all from related uses. Packs grew 13–20% in tokens | Agent-level token effect needs a paired run. Python call sites are not indexed. Embeddings are unmeasured |
| 3. Proof | Hermetic retrieval regression test in CI; Codex arm with persisted hook trust; draft held-out suite of 5 tasks | The gate fails under six deliberate breakages. Codex preflight passes 18 of 18 checks. Held-out calibration: 36 of 36 controls as expected | The held-out suite needs operator review and a collection adapter. The Codex arm needs `CODEX_API_KEY` |
| 4. Safety | Injection screen holds flagged proposals for a human; framing escapes; doctor screen of validated memories; adversarial tests and judge cases | 0 of 12 validated memories and 1 of 2,013 repository paragraphs flagged | Pattern-based: paraphrases and other languages pass |
| 5. Reach | Codex sub-sessions attributed to the parent session; Cursor hooks and MCP registration as an opt-in, with an installer self-check | Verified against documentation, source and synthetic payloads only | No live Cursor or Codex run yet. Cursor reports no token usage |

### Review before install

Two read-only reviews of the merged branches found seven confirmed hook
defects and an upgrade race; all were fixed with regression tests before
installation.

- A failed test could be masked by a later passing lint, format or build run,
  closing the episode as success.
- A verification earlier in a `;`-joined command could hide its failure.
- `cd X; cmd` could record evidence on the wrong project.
- A failure the daemon refused to record left an earlier pass standing.
- Concurrent user- and project-level hooks recorded one test run twice. This
  was found on the live installation and fixed with a per-call claim.
- During an upgrade, a hook could start a new daemon that migrated the schema
  under the still-draining old one. Daemons now take a single-instance lock,
  answer 503 while draining, and `install.sh` waits for the old process.

Build 82b5df1d is installed at schema 19. On the live system, an unpiped test
run is recorded once as `test_result` with its exit status and source digests,
and source pointers arrive as checked `path:start-end` ranges. `engram doctor`
passes except for the duplicate `brain`/`engram` registration and the Codex
hooks that still await trust.

### Second and third passes (2026-10-04, later)

| Workstream | Added | Measured or found |
|---|---|---|
| 0. Capture | `engram project merge` (dry run, backup, one transaction, audit event, retired-id redirect); pinned non-Git folders resolve in subdirectories | A review on a faithful copy found no lost rows. It confirmed nine defects, all fixed before release. The live merge of `engram` into `brain` waits for the operator. Codex 0.155 reports no exit status to hooks at all; reading it from the rollout transcript is in progress |
| 1. Evidence and judge | Status notes when a verification's exit status is hidden, with a ledger of recordable runs; judge acceptance of unindexed files through hook digests and of immaterial revision differences; a judge Git watchdog | The note fires live in Claude Code. A replay still passes 2 of 135 pending reviews: existing evidence predates hook digests, and 45 of 73 stale reviews touched files that really changed |
| 2. Context efficiency | Python call sites (extractor v3) with query rewrites and a unique-name rule; line-ranged, task-ranked file briefs with a size line for large files | Pilot tasks: required definitions in their file brief's first five rose from 45 to 87 of 105, and listed first from 6 to 51. Source-map selections were unchanged |
| 3. Proof | Held-out collection runner, report and import; a login check before every attempt; inspected resume after infrastructure stops | The held-out collection stopped after one attempt because the Claude Code login had expired before the agent started. That attempt is kept as an infrastructure attempt |
| 4. Safety | A loopback Host and Origin guard on `engram-proxy`; agent-written task prompts screened before the judge | No false positives on the backup's prompts |
| Maintenance | Job-row retention; recurring vault exports; hidden projects skipped by automatic exports | 30 of 123 live job rows are past retention |
