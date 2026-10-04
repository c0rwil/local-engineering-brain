# Real repository engineering benchmarks

Status: `pilot_completed_with_grading_limitations`. All 24 pilot attempts are
complete; see the [results](../../docs/perf-evidence/2026-09-24-real-repo-pilot/README.md).
Four pairs favor Brain on correctness, one favors Off, one has both passing, and
six are unscorable. The single both-pass pair used 34.1% fewer processed tokens
and 22.7% fewer tool calls with Brain, with 1.3% more agent time. This is a small
diagnostic, not evidence of general savings. Five grader-contract ambiguities and
one provider error remain in the evidence; repair and preflight the contracts
before another collection. The remaining five tasks are specifications awaiting
preflight.

The executable adapter is [`scripts/run_real_repo_pilot.py`](../../scripts/run_real_repo_pilot.py).
It consumes this catalog directly; this is not an `engram eval run` configuration.

The goal is to measure completed engineering work: correctness, regressions,
provider tokens, actual tool invocations and elapsed agent time. Source-navigation
scores alone cannot answer that question.

## Start with four tasks

Use `r01_guide_hot_path`, `r02_lean_task_status`, `r05_test_env_isolation` and
`r06_onboarding_outage` first. They cover backend behavior, response efficiency,
a small control change and frontend recovery. Three repetitions of each task,
with one Off and one Brain arm per repetition, make 24 agent runs. A repetition
label is not a deterministic model seed unless the chosen runtime supports and
records one. Treat this pilot as a diagnostic, not a statistically decisive result.

The remaining tasks test cancellation, async delivery, state ownership and broader
features. Graduate to them after the pilot's collection and grading work reliably.
Keep development tasks used to tune Engram separate from a frozen held-out task
set when making broader effectiveness claims.

`catalog.json` holds evaluator metadata, exact Git references and acceptance
criteria. `prompts/` contains only the requests given to the working agents. Do
not put the catalog, reference patches or hidden graders in an agent workspace.

| Task | Repository | What a correct solution must preserve | Round |
| --- | --- | --- | --- |
| [Disable task guides in the action hot path](prompts/r01_guide_hot_path.md) | agi-backend-rust | App guides, defaults, policy restrictions and request conversion; disabled retrieval performs no work | Pilot |
| [Make status polling lean](prompts/r02_lean_task_status.md) | agents | Cursor/history opt-in, total count, old contracts and device wire format; default skips payload loading | Pilot |
| [Acknowledge expired device results](prompts/r03_expired_device_result.md) | agents | Successful delivery, ownership and correlation; no duplicate or recreated request | Extension |
| [Report honest cancellation causes](prompts/r04_honest_cancellation.md) | agents | Every lifecycle path, remote cancellation and the narrower phone-submitted terminal contract | Extension |
| [Isolate tests from developer environment](prompts/r05_test_env_isolation.md) | agi-backend-rust | Normal local startup and original failure diagnostics | Pilot control |
| [Bound onboarding outage handling](prompts/r06_onboarding_outage.md) | platform | Fail-closed gates, usable welcome page, safe return URLs and five-second deadline | Pilot |
| [Fix sign-out and routine-registration races](prompts/r07_signout_routine_race.md) | mobile | Disconnect-once, local settings, structured cancellation and correct missed occurrences | Advanced |
| [Make direct-phone context durable](prompts/r08_direct_phone_context.md) | mobile | Abort propagation, per-turn context, tenant isolation and bounded outbox retries | Advanced |
| [Wire organization runtime defaults](prompts/r09_org_runtime_defaults.md) | platform | Inherit/override/opt-out distinctions, scoped cache updates and server-only credentials | Extension |

## Make the historical replay fair

1. Export each task's base tree into a disposable fixture. Do not reset a real
   checkout. Use a fresh Git repository or ancestor-only history: the agent must
   not find the completed fix in later commits, branches, reflogs or remotes.
2. Export instructions and documentation from that same base. Index only those
   sources into a new isolated Engram database with pinned project identity.
   The current canonical brain contains future code and lessons and is unsuitable
   for these replays. Any linked repository must also have a declared historical
   revision; otherwise omit the link in both fixtures.
3. For the initial comparison, use a source-only brain. Freeze its exact database
   before the pair, give each Brain run a fresh copy, and disable background
   learning, judge, watchers, synthesis, linked-project retrieval and external
   refresh. Off runs have no active brain. Remap checkout aliases to the trial
   fixture; do not retain aliases to real working repositories.
4. In Off, remove all Engram injection and access, including inherited hooks,
   MCP configuration and proxy injection. In Brain, exercise the native delivery
   pipeline and verify delivery receipts. Pin the same coding model, effort,
   prompt, non-Engram tools, dependency state and limits. Restrict both agents to
   the fixture and local test doubles during task execution.
5. Keep grading outside the editable task tree. Build behavioral tests from the
   listed contract, not a required text patch. Before scoring, prove that the
   base fails a meaningful new check, the historical fix passes, and a plausible
   incomplete fix fails a preservation check. If the reference commit needs
   additional prerequisite changes, record and resolve that before admitting it.
6. Use a fresh session and identical frozen inputs for every repetition. Alternate
   arm order, isolate build caches or give them equivalent warmed contents, and
   retain failed, timed-out and infrastructure-error attempts. Do not replace a
   failed attempt silently with a successful retry.

These are completed fixes replayed as engineering requests. They are not new bugs
to apply to today's repositories. After this suite, prospectively freeze real
unsolved tickets before implementation to reduce replay-selection bias.

## Run the pilot

From the Engram repository on the prepared macOS host:

```sh
caffeinate -dims python3 scripts/run_real_repo_pilot.py --prepare-only
caffeinate -dims python3 scripts/run_real_repo_pilot.py
```

Preparation exports each historical tree, creates a synthetic one-commit Git
repository with no remote or future objects, indexes a separate source-only
snapshot, and prepares dependencies. Rust base builds and relocation warmups
run before agent timing. Source snapshots contain no memories, tasks, or compiled
context. The real repositories and canonical Engram database are not fixtures.

The default run schedules 24 trials and alternates arm order. Rerunning the same
command resumes by skipping every ID already recorded in `records.jsonl`,
including failed and infrastructure-error attempts. It does not silently retry
them. An existing trial directory without a completed record refuses execution;
inspect and preserve the interrupted attempt rather than replacing its evidence.
An incomplete fixture preparation similarly refuses to overwrite its work tree.

The first guide-task Off attempt was aborted when the sandbox blocked access to
global Git configuration. Its evidence is preserved in the data directory's
`infrastructure_attempts.jsonl`, outside the 24-run scored matrix. The scored
comparison restarted after full tool preflight with the corrected environment;
the scored limits and task prompts did not change. The matrix summary reads
`records.jsonl`, so report this earlier infrastructure attempt and its observed
resource use separately instead of dropping it from the account of the work.

Default private paths are `/private/tmp/erp26-data` for fixtures, trial evidence
and `summary.json`, and `/private/tmp/erp26-active` for the current disposable
workspace. Custom `--data` and `--active` paths must be distinct non-symlink
`/private/tmp/erp26-*` roots. The runner refuses to clear an existing active
directory without its ownership marker. Keep these paths private: trial records
include source patches, prompts, model traces, settings, state and grader output.

`--tasks` selects catalog IDs and `--limit` caps the number of new trials in that
invocation. Agent and grader limits are independent: `--agent-seconds` defaults
to 1200, `--grader-seconds` to 1800, and `--cost-cap` to the CLI's $8 estimated
budget per agent run. Setup commands have their own timeouts, generally 1800
seconds. Preparation, per-trial setup, agent execution and grading are recorded
separately; the grader cannot consume the agent's wall-clock allowance.

Both arms use Claude Opus 4.8 with the `claude-opus-4-8[1m]` route, high effort,
a fresh session, and the single-agent tool set `Bash,Read,Edit,Write,Glob,Grep`.
Resolved Engram, daemon and Claude executable paths are pinned for the runner
process. Auto-memory, inherited settings, slash commands, browser integration
and MCP access are disabled. Brain adds native Engram hooks; Off disables all
hooks. This is a source-navigation comparison, not a learned-memory or MCP-agent
comparison. Native-tool permission rules and the Bash sandbox protect the
original checkouts, private graders, canonical state and earlier trial artifacts.
External network domains are denied and local test-server binding is enabled.

The coding environment ignores global and system Git configuration and global
exclude rules. Each Rust arm receives a private `CARGO_HOME` copied from the
prepared registry cache, with no inherited Cargo credentials or Git checkout
cache. Both arms keep identical prepared dependencies. For the installed pnpm
11 runner, `pnpm_config_pm_on_fail=ignore` and
`pnpm_config_verify_deps_before_run=false` prevent automatic package-manager
downloads or dependency reinstallations during the task.

## Grader calibration and runtime

| Pilot grader | Private checks | Calibration evidence |
| --- | ---: | --- |
| Guide hot path | 14 | [Reference/base/mutant reports](grading/r01_guide_hot_path/preflight/) |
| Lean task status | 23 | [Preflight report](grading/r02_lean_task_status/preflight.json) |
| Test environment isolation | 5 | [Reference/base/mutant reports](grading/r05_test_env_isolation/preflight/) |
| Onboarding outage | 26 | [Preflight report](grading/r06_onboarding_outage/preflight.json) |

All four reference implementations pass. The base implementations and plausible
incomplete fixes fail their relevant checks. These are grader calibration
results, not coding-agent benchmark outcomes. Each grader accepts
`--repo PATH --output JSON` and runs after the agent exits, against a separate
grading copy. Detailed commands and mutation descriptions are in each grader's
README. Private tests and reference patches never enter an agent's workspace or
Engram index.

Rust calibration used Homebrew Rust **1.96.0**, despite the historical package's
1.91.1 minimum. Builds are offline and locked; loopback services are mocks.
The Python grader uses the standard library and isolated service doubles, with
no development `conftest.py`, database, embeddings, network or subprocess access.
Agents working on that task use syntax checks or their own isolated tests.

Platform calibration uses Node v26.5.1, Vitest 4.1.5 and MSW 2.13.5. Its prepared
`node_modules` is a private clone of the installed dependency tree, shared as an
identical starting point across arms. It is **not an exact installation of the
historical lockfile**; the grader records both lockfile hashes. Preserve this
runtime across trials, or recalibrate every control before changing it.

The frozen graders have these acceptance boundaries:

- Lean-status cursors that cannot be converted to an integer must return invalid
  parameters. The reference accepts numeric strings and coerces fractions and
  booleans; strict rejection of the latter is not scored.
- The lean-status grader compares v1 fixture bytes and checks a new v2 directory
  if added. However, the agent-visible historical v1 README explicitly permits
  optional added fields. A byte-only failure therefore needs per-trial review
  for a grader-contract ambiguity, preserving the raw result and any independent
  behavioral failures. See the [grading audit](../../docs/perf-evidence/2026-09-24-real-repo-pilot/GRADING_AUDIT.md).
- Onboarding's malformed-response checks cover invalid JSON syntax/non-JSON
  bodies. Schema-invalid but syntactically valid JSON profiles are not covered
  by this reference contract. The deadline check observes actual fetch aborts
  with 500 ms scheduling tolerance beyond five seconds.

## Read the measurements

[`scripts/real_repo_metrics.py`](../../scripts/real_repo_metrics.py) parses the
provider's stream-JSON trace and produces paired summaries. These are its current
measurements:

| Measure | Definition |
| --- | --- |
| Correct task completion | Agent produces a patch that passes hidden behavior and preservation checks; self-reported success is insufficient. |
| Provider usage | Reported input, output, cache-read and cache-write tokens, preserving provider semantics. No bytes/4 estimates or missing-as-zero values. |
| Tool use | Unique model-issued `tool_use` IDs, deduplicated across complete and partial stream events, with counts by tool name. These count requests, including denied requests, not inferred shell subcommands. Hooks are not model tool calls; this pilot has no MCP or delegated-agent calls. Turns are reported separately. |
| Agent time | From task dispatch to agent completion/timeout. Fixture setup, indexing and external grading time appear separately. |
| Context delivery | Durable task-linked `context_served` receipts, delivered run/session IDs, compiled-run counts and estimated selected tokens. A compiled record alone does not prove delivery. A Brain run without a receipt is invalid. Separate file-shape hook text can exceed what the compiled-token estimate describes; provider usage measures the actual agent bill. |
| Setup cost | Fixture/index/dependency preparation and per-trial setup seconds, separate from agent time. Automatic learning and judge calls are disabled. |

Processed input is the sum of provider input, cache-creation input and cache-read
input; total tokens add output. Missing final usage stays unknown, never zero.
Missing tool IDs or malformed/conflicting events mark tool-count coverage as
incomplete. The report preserves provider model usage, result/error state,
permission denials and trace diagnostics. It does not currently calculate time
to first relevant edit, semantic exploration categories, or amortized setup cost.

Publish the paired outcome table (both pass, Brain-only pass, Off-only pass,
neither passes). Report all-attempt resource totals with success rates, and
same-success paired deltas with their sample count. The current summary retains
observed costs for failed and infrastructure attempts and computes reductions
only for pairs where both arms pass and both measurements are present. Aggregate
percentages are ratios of matched sums, not averages of task percentages. A
lower token total from an agent giving up is not a savings result. The reported
USD value is the CLI's estimate, not a statement about subscription billing.

Provider prompt caching remains enabled and is not reset between trials. The
aborted first Off attempt may have warmed the guide-task prompt prefix. Report
cache-creation and cache-read tokens separately and retain arm order when
interpreting the paired results; this pilot is not a controlled cold-cache
comparison. A client-reported cost reduction does not establish cash savings.

Pre-register a pilot decision rule: investigate any Brain-only correctness
regression; consider a task promising when correctness is preserved and measured
token or exploration cost improves consistently across repetitions. Do not turn
that screening rule into a significance claim from three repetitions.

## Import completed evidence into Engram

[`scripts/import_real_repo_pilot.py`](../../scripts/import_real_repo_pilot.py)
imports the public `results.json` into Engram's evaluation store. It does not add
live usage events or memories. The default dry run validates all 24 scored trials,
frozen controls, evidence hashes, classified outcomes, and measured agent time
before any write:

```sh
python3 -B scripts/import_real_repo_pilot.py \
  --report docs/perf-evidence/2026-09-24-real-repo-pilot/results.json \
  --benchmark-id real-repo-pilot-20260924
```

After the full report validates, apply those same records to the chosen daemon:

```sh
python3 -B scripts/import_real_repo_pilot.py \
  --report docs/perf-evidence/2026-09-24-real-repo-pilot/results.json \
  --benchmark-id real-repo-pilot-20260924 \
  --socket "$HOME/.engram/engramd.sock" --apply
```

Without `--socket`, the importer uses `ENGRAM_SOCKET_PATH` or
`~/.engram/engramd.sock`. It writes only to `/api/v1/evaluation/runs`. Separate
preflight and aborted infrastructure attempts remain in the public report,
outside the 24-trial import. Every scored pass, failure, and infrastructure
outcome retains its reported resources and original behavioral verdict. Missing
optional measurements remain unknown; missing mandatory agent time rejects the
import. Error-run usage and incomplete tool counts can be partial even when
numeric values exist. CLI cost estimates are not subscription cash charges.

Reviewed grader ambiguities are verified against the saved grade and its sole
failed check before publication. The importer preserves the resulting compact
adjudication hashes, original behavioral failure, and unscorable outcome; it
does not turn that trial into a pass or an ordinary correctness loss.

Stable run IDs make identical retries idempotent. A reused ID with changed
content is rejected rather than overwritten. If an apply stops partway through,
it reports the acknowledged count; a failed acknowledgement may already be
stored. Retry the same report, path, and label. Regenerating only the report's
publication timestamp does not change imported records, while changed controls
partition comparisons and conflicting reanalysis requires explicit resolution.

## Test memory separately from source navigation

Once the source-only comparison works, add a separate condition with independently
supported knowledge that existed before the target task. Compare Off, indexed
sources, and indexed sources plus lessons using the same grading contracts. This
separates navigation benefits from learned engineering knowledge. Include a
misleading initial file scope or a follow-up intent change in a separately labeled
variant, delivered through the actual client hook events in both arms.

Never seed a lesson that restates the held-out fix. A small local judge can record
an advisory review, but deterministic tests decide the scored correctness result.

The five extension/advanced tasks still require behavioral graders, dependency
preparation and calibration before execution. Mobile preparation must stay
outside timed agent work. Store private raw traces outside source control;
publish redacted aggregate metrics, fixture hashes, validators and exact
configuration with the completed pilot results.
