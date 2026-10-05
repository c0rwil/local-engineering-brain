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

### Fourth pass (2026-10-04, late)

| Workstream | Added | Measured or found |
|---|---|---|
| 0. Capture | Codex verification results read from the calling thread's rollout transcript; the rollout's directory attributes Codex commands to the right project | Codex 0.155 sends no exit status to hooks. In local rollouts, 56% of Codex commands ran outside the session directory |
| 1. Evidence and judge | `engram review reverify` (operator-run, disposable worktree, strict command screen); `engram review expire` and `reopen` with a terminal `expired` status; episodes record their checkouts, so one repository's run cannot void another's pass | Of the 135 pending reviews, only 27 have runnable recorded commands, and only 5 could become judge-eligible if those pass. Expiry would close 114 once they age past 14 days. A review of the new code found no correctness defects; two screen gaps (`make test deploy` and code-loading environment variables) are fixed |
| 2. Context efficiency | File briefs refresh once per task when their sources change | About 1 KB per re-brief, at most one per file per task |

The held-out benchmark (Claude arm) is prepared and checks the Claude Code login before every attempt. Collection resumes after the operator signs in again.

### Fifth pass (2026-10-04, night)

This pass started from context this session received from its own hooks: rules
and source pointers from three company services, while the task was Engram's
own code.

| Workstream | Added | Measured or found |
|---|---|---|
| 0. Capture | The operator merged the duplicate `engram` project into `brain` | `engram doctor` passes `project_identity`; only the untrusted Codex hooks warn |
| 2. Context failure modes | Links applied under the pre-2026-09-16 hostname rule are retracted when no evidence outside documentation supports them; shell-mode turns and background-task notices no longer replace the task's request | In the seven days before, linked repositories supplied 92% of the context injected into Engram's own sessions (1.68M of 1.84M tokens). The three links rested on a handoff note and a deploy README. Links whose host still appears in configuration, such as an app build file, stay. A `claude auth login` turn and a test-run notice each became the task text, steering retrieval to API-key and `run_task` code in other repositories |
| 4. Safety | Injection screen v2: Latin-lookalike folding and "ignore previous instructions" in nine languages | No new flags on 90,131 paragraphs from six repositories or on 989,453 localized macOS UI strings. Recall on real attacks is unmeasured |
| 3. Proof | Held-out collection resumed after the operator signed in again | Running; results follow in the held-out findings |

Build f27bb96a is installed. A re-index of Engram's checkout retracted its three
links, and dry-run scans of the four company repositories find no stale links.
The next hook delivery in this session carried only the brief of the file being
read.

Effectiveness now reports which project supplied each project's selected
context (`selected_by_origin`). Over the seven days before this pass, linked
repositories supplied most of it: `platform` 84% (source pointers into the API
service it calls), `mobile` 62%, `agents` 59%, the backend 40%.

Next for context efficiency, each needing a measurement before it ships:

- **Linked lessons.** A lesson travels to a linked repository's sessions when
  any of its terms appears in the task text (claim coverage above 0). Five of
  the backend's validated lessons, including the history of one merged pull
  request, reached 60–90% of all `agents` and `mobile` runs. In the seeded
  calibration tickets, relevant lessons cover 0.13–0.30 of their terms and
  irrelevant ones up to 0.21, so no coverage threshold separates them. A floor
  of 0.10 would remove about 60% of linked-lesson tokens but still pass
  repository-internal history on generic words such as "PR" and "staging".
  The retrieval probes disable linked projects, so none of these gates can be
  checked today. First step: labeled cross-repo probes sampled from live
  injections. Candidates to evaluate: marking lessons as cross-repo or
  repository-internal at validation, a coverage floor, and requiring the
  lesson to name the consuming repository or a shared interface.
- **Repeated delivery.** Hooks resend the whole pack whenever it changes, so
  items the session already holds are delivered again. In the one session with
  full attribution, 82% of delivered tokens repeated earlier deliveries, 96%
  for linked lessons. Hooks now reset delivery after compaction (`SessionStart`
  with source `compact` or `clear`), which a once-per-session rule needs.
  It still needs a paired run, because repetition may also act as a reminder.
