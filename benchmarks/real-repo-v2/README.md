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

## Codex arm

`--agent codex` runs both arms with the Codex CLI (`codex exec`) instead of
Claude Code; the default remains `claude`. The schedule, fixtures, prompts,
graders, wall limits and classification are the same. A Codex collection is a
separate experiment: use separate data and active roots, and compare Codex Off
with Codex Brain only, never with a Claude collection. No Codex collection has
been run yet.

Each attempt gets a fresh `CODEX_HOME` and a private `HOME` for the Codex
process, so user configuration, credential files, skills, memories, hooks and
MCP servers do not leak in. Tool commands keep the real `HOME` so toolchains
resolve as in the Claude arm. The adapter is
[`real_repo_codex.py`](../../scripts/real_repo_codex.py). Both arms share one
generated `config.toml`:

- the pinned model and reasoning effort, `approval_policy = "never"`, no login
  shell, web search disabled and history persistence off;
- identical feature flags, with memories, multi-agent, apps, plugins, browser
  and computer use, image generation, shell snapshots, skill search and
  unbounded connection retries disabled (bounded retries make a provider outage
  an unscorable `turn.failed` rather than a wall-clock failure);
- a permission profile that mirrors the Claude sandbox: evidence, fixtures, the
  Engram checkout, source repositories, `~/.engram`, `~/.codex`, `~/.claude`,
  this trial's Engram state and Codex homes, and other or later trial roots are
  unreadable; writes are limited to the checkout and the trial scratch
  directory; the checkout's `.engram` is read-only; network access is off.

The arms differ only in the treatment. Brain enables Codex hooks, installed
with `engram hooks codex --install` into that attempt's `CODEX_HOME`, and
registers `engram-mcp` with the attempt's database and socket and no daemon
autostart. Off disables hooks and has no MCP servers. Unlike the Claude v2
collection, the Brain arm therefore includes MCP tools.

Codex runs hooks only after they are trusted. The runner verifies that every
installed handler runs the pinned Engram binary. It then writes the same
record that `/hooks` review stores, `[hooks.state."<key>"] trusted_hash`, into
that attempt's isolated `config.toml`. The key and hash rule matches
`src/codex_trust.rs`, which was checked against the pinned Codex binary's
`hooks/list`. Trust is never written for a home outside a runner-owned active
root, including `~/.codex`. `--codex-hook-trust bypass` instead passes the
documented automation flag `--dangerously-bypass-hook-trust`. A checkout
containing `.codex` is refused, because project hooks or configuration would
bypass these controls.

Credentials come only from the `CODEX_API_KEY` environment variable, which
Codex documents for `codex exec`. The runner passes it to the Codex process,
excludes it from tool commands, and never records it. The isolated homes do
not support a ChatGPT sign-in; using one would need a separate, reviewed
operator procedure.

Telemetry comes from each attempt's rollout files in `CODEX_HOME/sessions`
and the `--json` event stream. `token_count` rows are cumulative per session
file. As in `src/usage_capture.rs`, only increments are summed, identical rows
add nothing, and a decrease starts a new baseline without inventing usage.
A reset or missing counter marks usage as partial, so it never enters paired
savings. Codex input includes cached input and output includes reasoning. The
record keeps the shared meaning: `input_tokens` is uncached input,
`cache_read_input_tokens` is cached input, processed input is Codex input,
total is input plus output, and `reasoning_output_tokens` is reported
separately. Tool calls are model-issued function, custom and shell calls,
deduplicated by `call_id`. Codex reports no USD estimate and `codex exec` has
no budget cap, so only the wall limit applies.

A hook that runs while the arm's daemon is down loses its record and starts a
replacement daemon on the trial's state. To keep any hook process that
outlives Codex from doing so, the runner waits up to 45 seconds for the
remaining processes in Codex's process group, then stops any stragglers,
before it stops the daemon. Agent wall time ends when Codex exits. The wait
is recorded separately (`agent_hook_drain_seconds`,
`agent_hook_stragglers_stopped`).

Before collection, run the model-free preflight
[`preflight_real_repo_codex.py`](../../scripts/preflight_real_repo_codex.py).
It builds each arm's exact Codex home and drives the pinned binary against a
scripted loopback endpoint; no model is called and no credentials are read.
It checks feature flags, MCP servers, the hook-trust record, and the absence
of user skills from the model-visible prompt input. Through Codex's real exec
tool, it checks reads, writes, loopback TCP and Unix sockets, and the tool
environment. It also checks Brain hook delivery and the daemon receipt, that
the parser recovers the scripted usage exactly, and that no hook restarts the
arm's daemon after the session. With persisted trust no
bypass flag is passed, so Brain hook delivery shows that Codex accepted the
trust entries. The runner refuses to collect without a passing proof for the
same binaries, model, effort, trust mode and scripts.

```sh
python3 scripts/run_real_repo_followup.py --agent codex --codex-model MODEL \
  --data /private/tmp/erp26-codex-data --active /private/tmp/erp26work-codex-active \
  --engram-release RELEASE --prepare-only
python3 scripts/preflight_real_repo_codex.py --codex-model MODEL \
  --data /private/tmp/erp26-codex-data --active /private/tmp/erp26work-codex-active \
  --engram-release RELEASE
CODEX_API_KEY=... caffeinate -dims python3 scripts/run_real_repo_followup.py \
  --agent codex --codex-model MODEL \
  --data /private/tmp/erp26-codex-data --active /private/tmp/erp26work-codex-active \
  --engram-release RELEASE
```

The Codex CLI is found on `PATH`, in `~/.cache/codex-runtimes`, in the Codex or
ChatGPT app bundle (`Contents/Resources/codex`), or through Spotlight; `--codex`
overrides it. The manifest records the binary hash and version, model, effort,
hook-trust mode and a digest of the generated configuration.
