# Real repository engineering benchmark v2

This suite defines a new 24-run comparison of Off and source-only Engram using
the installed context improvements. Four historical tasks run three times per
arm. The executable adapter is
[`run_real_repo_followup.py`](../../scripts/run_real_repo_followup.py); its frozen
manifest records the actual binaries, model route, task inputs and source
snapshots used for collection. `catalog.json` records preparation status.

V2 clarifies the contracts that made some v1 outcomes unscorable. It does not
rescore v1, reuse its agent attempts, or establish improvement over the old Brain
binary. V1 prompts, graders, traces and published results remain frozen. These
four tasks have informed development, so this is a diagnostic comparison;
general effectiveness claims require a separate held-out suite.

| Task | Preserved behavior and v2 clarification |
| --- | --- |
| [r01: guide hot path](prompts/r01_guide_hot_path.md) | Disabled task guides skip retrieval, deferred work and cache pickup; app guides, defaults, master switch and session conversion remain correct. The grader retains the enforcing policy context used by production and accepts enforcement during request normalization or resolution. |
| [r02: lean status polling](prompts/r02_lean_task_status.md) | Summary-only polling avoids history loading; explicit history and cursors preserve total counts and device payloads. V1 JSON compatibility permits additive optional fields and formatting changes. Cursor conversion and false flag handling are explicit. |
| [r05: test environment isolation](prompts/r05_test_env_isolation.md) | Test mode does not reload developer dotenv values; normal startup still loads them; all three shared-lock sites recover after poisoning without hiding the original failure. The prompt and behavioral grader are unchanged from v1 and receive fresh calibration. |
| [r06: onboarding outages](prompts/r06_onboarding_outage.md) | Gates fail closed and welcome remains usable, safe destinations survive, and completed users continue. The five-second deadline includes response-body consumption; header timeouts may use 503 or 504, and interrupted body failures may also use 502. Outcome checks are independent. |

The catalog and private graders contain evaluator-only information. Agents receive
only their task prompt, the same execution wrapper, and the historical base tree.
Do not copy reference commits, grader fixtures, preflight mutations, previous
attempts or this catalog into an agent workspace.

## Freeze and calibrate before collection

Each task uses the same historical base and reference revisions as v1. Fresh
calibration must reject the base and plausible incomplete fixes, accept the
reference, and accept legitimate alternate implementations of the clarified
contract. Each grader directory describes its controls and records new
`preflight.json` evidence; old preflight output is not new calibration.

Review the prompts, graders and complete calibration reports before changing
`execution_status` to `ready_for_collection`. Freeze the suite files and runner
manifest before the first agent invocation. Do not change acceptance rules,
test counts or environment during collection. A newly discovered grader defect
is reported with the saved evidence; any revised collection requires a new
version or separately frozen experiment, not silent rescoring.

Preparation exports only historical base sources into synthetic one-commit Git
repositories with no remotes or future objects. Source-only Engram snapshots
contain no memories, lessons, tasks or previous compiled context. Every Brain
attempt receives a fresh copy; learning, watchers, synthesis, background refresh
and linked-project retrieval are disabled. Every Off attempt has no Engram
injection or access. The canonical brain and current working repositories must
not be used as replay inputs.

Both arms use the same pinned model route and effort, prompt, tools, dependencies
and limits. Sessions and task workspaces are fresh. Dependencies and Rust build
warmups happen before agent timing. Grading runs afterward in a separate
disposable copy; agents cannot read or edit private graders. Both arms are
restricted to their fixture and local test doubles, with no deployments, live
services or external network calls during coding. Prepared platform dependencies
are identical across arms but are not a lockfile-exact historical installation;
record both dependency-lock hashes and rerun calibration if the runtime changes.

Run `--prepare-only` first, review the prepared inputs and manifest, then run the
new adapter with separate v2 data and active roots. The adapter's `--help` defines
the invocation and path defaults. Resuming must verify frozen controls and skip
all completed IDs, including failures and infrastructure outcomes. Preserve an
interrupted attempt; do not overwrite its directory or silently replace it.
After a completed unscorable or provider-error attempt, the runner stops for
inspection. Resuming preserves that attempt and continues to the next planned
ID; it does not retry or replace it.

## Compare completed work

The schedule has four tasks, three repetitions, and two arms: 24 agent attempts.
Alternate arm order within the planned pairs. Repetition labels are not model
seeds. Three repetitions support diagnosis, not a statistical significance claim.
Judge correctness from the frozen behavioral checks, retain every attempt, and
report provider/runtime errors separately from behavioral failures.

Report both-pass, Brain-only pass, Off-only pass, both-fail and unscorable pairs.
Compare token, tool and time costs only where both arms pass and their required
measurements are complete. Report all attempts' resource use separately so failed
or unscorable work remains visible. Investigate every Brain-only correctness
regression; repeated correctness-preserving reductions are promising signals,
not general savings estimates.

Keep provider input, output, cache-creation and cache-read tokens separate, along
with actual tool invocations and agent wall time. Record preparation, per-trial
setup and grading time separately. Distinguish compiled Engram context from
context confirmed delivered by native hook receipts; missing Brain receipts
invalidate the treatment. The provider controls its prompt cache, so this is not
a cold-cache comparison. Claude Max client cost estimates are not subscription
cash charges.

Publish a new report with the frozen suite/runtime hashes and aggregate results.
Retain private prompts, patches, traces, grader logs, calibration evidence and
any aborted infrastructure attempts. Do not infer a causal v1-to-v2 improvement
from a comparison whose task contracts and collection dates changed.
