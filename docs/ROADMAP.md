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
