# Real repository held-out suite v1 (draft)

Status: `draft_requires_operator_review`. No coding agent has run against this
suite, and it makes no effectiveness claim. The grader calibration below shows
only that each grader rejects the historical base and plausible incomplete
fixes and accepts the historical fix and alternative correct fixes.

The [2026-09-25 follow-up findings](../../docs/perf-evidence/2026-09-25-real-repo-followup/FINDINGS.md)
proposed two changes: line-ranged source pointers and a related-use coverage
cue. The v1 and v2 tasks informed earlier development, so they cannot show
whether such changes generalize. This suite supplies held-out historical replays
with the same structure as [v2](../real-repo-v2/README.md): a catalog, agent
prompts, and private graders with recorded calibration.

| Task | Repository | Base → fix | Exercises | A correct solution must |
| --- | --- | --- | --- | --- |
| [h01 effect-check enforce only](prompts/h01_effect_check_enforce_only.md) | agi-backend-rust | `f566a1c2` → `c24e931d` | Line-ranged pointers | Silence the note in shadow and off, both in the model request and in stored prompts, while keeping enforce delivery and the shadow LoopGuard screen evidence |
| [h02 failure and feedback origin](prompts/h02_failure_feedback_origin.md) | agi-backend-rust | `2b690115` → `c67bbf4d` | Related-use cue; line-ranged pointers | Tag model-failure events and reaction records with the frozen origin, normalize first-step origins as session creation does, and leave origin-less sessions untagged |
| [h03 scheme-less navigation](prompts/h03_schemeless_navigation.md) | agents | `9adccea2` → `d3ab3e02` | Related-use cue | Default bare hosts to https on all six navigation paths and leave schemed targets unchanged |
| [h04 Play base-plan lookup](prompts/h04_play_base_plan_lookup.md) | agents | `ae4dd1d1` → `123a6ee6` | Related-use cue | Resolve suffixed product ids at all three plan lookups and keep packs, plain ids and the unmapped behavior |
| [h05 multi-byte truncation](prompts/h05_multibyte_truncation.md) | agi-backend-rust | `f566a1c2` → `dee1f322` | Small control | Count limits in characters for every field and transcript entry, with ASCII output unchanged |

In h01 and h02 the fix sits in `src/services/action_engine.rs`, which is
800,915 and 666,541 bytes at the two bases. That is far above the 256 KB Read
limit noted in the follow-up. In h01 the fix site is near line 1,787, and the
existing contract test that encodes the old behavior is in a 545 KB test file.
Three tasks need a fix at several sites: h02 has two emitters in two files, h03
has six navigation paths, and h04 has three callers of one lookup. h05 is a
small control in a 19 KB file. h01 and h05 share a base commit.

`catalog.json` holds evaluator-only metadata, exact commits, acceptance criteria
and review notes. `prompts/` holds only the requests given to coding agents. Do
not copy the catalog, graders, preflight evidence, mutations or reference
commits into an agent workspace or an Engram index.

## Selection

Candidates came from the operator repositories' main-branch history:
`agi-backend-rust` `origin/main` (its main checkout's working tree is stale, so
only Git objects were used) and `agents` `origin/main`. A task needed a
behavioral contract that hidden tests could check offline, roughly one to five
source files, tests that run without network, secrets or databases, and no later
revert on main as of 2026-10-04. No v1 or v2 task, including the related v1
variant, was eligible. h03 changes nine files: six one-line call sites, a helper,
its export and a test. It exceeds the usual size because breadth is the point.

Candidates that were not chosen include:

- the `whoami` org-key fix, because its contract is a SQL join that needs a
  database;
- several large LoopGuard attribution changes, because they rewrite wording and
  their contracts depend on wording;
- `platform` and `mobile` tasks. The prepared platform dependency tree under
  `/tmp/engram-platform-preflight-20260924` is partially purged; for example,
  `node_modules/vitest` points to a missing target. Mobile needs Android tooling.
  Neither could be calibrated here.

The purge also affects the frozen v2 onboarding task's prepared dependencies.
Rebuild that tree before any v2 regrading.

The drafting agent read each reference diff to build its grader. The operator
must confirm that none of these fixes or code areas informed Engram tuning
before the suite is treated as held out.

## Graders

Every grader accepts `--repo DISPOSABLE_GRADING_COPY --output RESULT_JSON`. It
exits with 0 for pass, 1 for a behavioral or compile failure, and 2 for an
infrastructure failure. It refuses paths under `~/PycharmProjects`. Each task
directory contains a README, `grade.py`, `preflight.py`, `preflight.json` and,
for the Rust tasks, `hidden_tests.rs`. The `_shared/` code is grading
infrastructure:

- `rust_grader.py` injects private tests and touches every source file, because
  historical archives keep commit-time mtimes and a shared target would
  otherwise serve stale builds. It removes a root `.env`, builds offline and
  locked under a file lock, and runs each case in a fresh process with one test
  thread.
- `py_harness.py` loads the candidate's real modules with package
  initializers and third-party packages stubbed. It clears the environment and
  denies sockets and subprocesses. It uses only the standard library and needs no
  `DATABASE_URL`.
- `preflight_common.py` exports base and reference with `git archive`, which
  only reads the source repository. It derives each control by exact anchored
  edits, runs every control, and records outcomes, edit diffs and file hashes.

Rust graders use `ENGRAM_RUST_GRADER_TARGET`, by default
`/tmp/engram-heldout-rust-target`, and `ENGRAM_RUST_GRADER_JOBS`, by default 2.
Collection grading and the grader preflight both set them to the collection's
`grader-target` directory and four jobs.

## Calibration

Run on 2026-10-04 with Homebrew Rust 1.96.0 and Python 3.14.7. Rust builds ran
under `nice -n 10` with four jobs and a private target directory. Each
`preflight.json` records every run's checks and exit codes.

| Task | Hidden checks | Controls | Outcome |
| --- | ---: | --- | --- |
| h01 | 6 + compile | base, reference, 4 mutants, 1 alternative | All 7 controls as expected; base fails the 3 note checks |
| h02 | 8 + compile | base, reference, 4 mutants, 1 alternative | All 7 as expected; base fails 5 tagging checks |
| h03 | 6 + load | base, reference, 4 mutants, 2 alternatives | All 8 as expected; base fails all 6 paths |
| h04 | 8 + load | base, reference, 3 mutants, 3 alternatives | All 8 as expected; base fails 4 base-plan checks |
| h05 | 5 + compile | base, reference, 3 mutants, 1 alternative | All 6 as expected; base fails 4 multi-byte checks (3 by panic) |

The mutants stand for plausible incomplete fixes: fixing one emitter or path,
silencing only one mode, losing normalization or trimming, or keeping byte
budgets. Alternatives are different correct implementations, such as a fix
inside the repository rather than the service, a different URL helper, or
normalization at the route. The task READMEs describe each control.

The h01 shadow-scoring check changed twice before the recorded run. First it
read LoopGuard's trace stamp from mock GCS uploads, but in this scenario the
service uploaded no new trace after the first step, so the check failed every
control. It now reads the `loopguard_shadow` INFO log line and per-step screen
fingerprints. A later version also asserted the note's absence and so failed the
base for the wrong reason; that assertion was removed. Only the final run is
recorded. The other graders' recorded runs were their first full runs; h03 and
h04 each had one earlier partial debug run of base and reference only, written
outside the suite.

[`tests/test_real_repo_heldout_catalog.py`](../../tests/test_real_repo_heldout_catalog.py)
keeps the suite a draft and checks that each recorded report matches the
catalog and the current grader, preflight, private-test and shared-module
hashes. Any grader edit therefore needs a new calibration run. It also checks
that prompts carry no commit, evaluator path or grader vocabulary.

## Operator review before collection

1. Confirm that the tasks are held out: none of the fixes or their code areas
   informed Engram tuning.
2. Review the prompts. The h03 and h04 prompts name the categories of paths
   and events, which makes the contracts gradable but the related sites easier
   to find. h02 names the two record families. No prompt names a file,
   function or commit.
3. Review grader observables. h01 relies on an internal log line and the
   `screen_fingerprint` field. h03 drives `_execute_actions` and the router
   handler directly. The Rust graders reuse helpers in `tests/http_contract.rs`,
   so a candidate that renames them fails to compile with the private tests.
4. Decide the agents-repo environment. The v1/v2 runner prepares no Python
   dependencies, so coding agents can only run syntax checks or tests they
   write themselves. The Python graders are standard-library only: a
   candidate that imports a third-party package missing from the stubs fails
   to load, and the h04 query emulation rejects SQL functions outside its small
   set.
5. Collect only after this review, or record that it is still pending. While
   `execution_status` is `draft_requires_operator_review`, the collector runs
   this suite only with `--allow-draft-suite`, and the frozen manifest and its
   scope limits record that flag.

## Collection

[`run_real_repo_followup.py`](../../scripts/run_real_repo_followup.py) collects
any suite selected by its catalog path; without `--catalog` it still runs v2,
whose manifest and output are unchanged. For this suite it builds its own
fixtures and requires a passing grader preflight against them.

The default protocol matches v2: five tasks, three repetitions and both arms,
30 attempts in alternating arm order. Both arms use Claude Code with
`claude-opus-4-8[1m]` at high effort, six core tools, 1,200 seconds of coding,
1,800 seconds of grading and an $8 usage-estimate cap. Brain adds source-only
automatic hooks from the pinned installed release, with no MCP tools and no
learned memories.

1. Prepare the fixtures. Each base commit is exported with `git archive` from
   the operator's repository, which is only read. The export becomes a
   one-commit repository with a pinned project identity. Tracked `.env` files
   are dropped, every file gets a fresh mtime, and the commit must match the
   base tree exactly. Rust tasks get a registry-only cargo home and an offline,
   locked build at the trials' own paths; h05 starts from h01's build because
   they share a base. The pinned release then indexes a source-only snapshot.
2. Recalibrate the graders against those fixtures with
   [`preflight_real_repo_graders.py`](../../scripts/preflight_real_repo_graders.py).
   The base control is the prepared fixture itself. The reference control is
   the fixture plus the historical fix. Every control must match its recorded
   `preflight.json` run: the same exit code, the same failed checks and the
   same check count. The collector refuses to start without this proof, or if
   a fixture or grading file changed after it.
3. Run the model-free Claude runtime preflight against a loopback stub.
4. Launch the detached collection with
   [`launch_real_repo_collection.py`](../../scripts/launch_real_repo_collection.py).
   It reruns every check in the foreground with `--check-only`, then starts the
   collector in a new session under `nohup` and `caffeinate -dims`. The
   collector holds `collector.lock`, writes `progress.json` around every attempt
   and appends to `collector.log`. Running `start` again resumes: completed
   attempts are skipped and frozen controls are checked again. An interrupted
   attempt blocks the resume until it is inspected. `status` reports progress.

```sh
RELEASE=~/.local/share/engram/releases/82b5df1dbe45dd72b1d113909ec81a549242f0a846fa00ff4202cb9b2e12aa9a
caffeinate -dims nice -n 10 python3 scripts/run_real_repo_followup.py \
  --catalog benchmarks/real-repo-heldout-v1/catalog.json \
  --data /private/tmp/erp26-heldout-data --active /private/tmp/erp26work-heldout-active \
  --engram-release "$RELEASE" --prepare-only --build-jobs 4
caffeinate -dims nice -n 10 python3 scripts/preflight_real_repo_graders.py \
  --catalog benchmarks/real-repo-heldout-v1/catalog.json --data /private/tmp/erp26-heldout-data
python3 scripts/preflight_real_repo_runtime.py --data /private/tmp/erp26-heldout-data \
  --active /private/tmp/erp26work-heldout-active --engram-release "$RELEASE"
python3 scripts/launch_real_repo_collection.py start \
  --catalog benchmarks/real-repo-heldout-v1/catalog.json --allow-draft-suite \
  --data /private/tmp/erp26-heldout-data --active /private/tmp/erp26work-heldout-active \
  --engram-release ~/.local/share/engram/releases/82b5df1dbe45dd72b1d113909ec81a549242f0a846fa00ff4202cb9b2e12aa9a
python3 scripts/launch_real_repo_collection.py status --data /private/tmp/erp26-heldout-data --tail 20
```

The runtime proof's identity includes the coding environment, so the launcher
gives the collector the `PATH`, `HOME`, `USER`, `LOGNAME` and `LANG` recorded in
that proof. It also includes the checkout path, the runner scripts and the
Claude Code binary. To launch from another checkout, or after any of them
changed, rerun the runtime preflight there with `--supersede`. That keeps the
earlier evidence as `runtime-preflight.superseded-<time>` and clears the active
root it left; it is refused once the collection has started. The grader proof
binds file contents, not paths, and stays valid while the fixtures, grading
files and grader preflight script are unchanged.

### Claude session isolation

The v2 collection paused at 19/24 because its frozen sandbox policy listed
every entry of the shared `/private/tmp/claude-501`. Each Claude Code session
adds and removes files there, so any operator session or subagent changed the
policy identity. The compact policy no longer enumerates that directory.

Each trial now sets `CLAUDE_CODE_TMPDIR` to a short collection-specific alias,
`/private/tmp/e26t-<hash of the active root>`. The alias is a symlink to
`scratch/claude-tmp` inside the trial's own active root. The short path keeps
Claude Code under its 44-byte limit for the sandbox temp directory. Above that
limit, Claude Code puts that directory, and with it the Bash cwd files, in the
shared `/tmp/claude-<uid>`; the v2 scratch path was too long. The trial's
session files, Bash cwd files and sandboxed `TMPDIR` therefore stay inside its
own writable scratch, which the next trial clears. Trials also run with
`--no-session-persistence`, so they write no transcripts.

Isolation is at least as strict as before. Every `/private/tmp/claude-*` root
is denied to the Read, Edit, Write, Glob and Grep tools and to sandboxed reads
by static rules, which match roots that appear later as well. Sandboxed
commands can write only the checkout and the trial scratch, besides the
sandbox runtime's built-in device and log paths. The earlier
exceptions for `cwd-*` files in the shared directory, and the refusal to start
when such a file existed, are gone. The runtime preflight checks the new
layout through the real CLI: `TMPDIR` lies inside the trial, a temporary file
can be created there, and a fresh `claude-*` canary root and the shared
directory can be neither listed nor written. It also checks that nothing named
after the run's session or checkout appeared in the shared directory.

Other collections' active roots, `/private/tmp/erp26work-*`, are still denied
by name: each attempt's policy lists every such root that exists when the
attempt starts. The frozen identity leaves that list out. Before this change,
creating or removing such a root, as the harness tests and runtime preflights
do, would have stopped a running collection.

## Rerun calibration

```sh
export ENGRAM_RUST_GRADER_TARGET=/private/tmp/engram-heldout-rust-target ENGRAM_RUST_GRADER_JOBS=4
nice -n 10 python3 -B benchmarks/real-repo-heldout-v1/grading/h01_effect_check_enforce_only/preflight.py \
  --source-repo /path/to/agi-backend-rust \
  --output benchmarks/real-repo-heldout-v1/grading/h01_effect_check_enforce_only/preflight.json
python3 -B benchmarks/real-repo-heldout-v1/grading/h03_schemeless_navigation/preflight.py \
  --source-repo /path/to/agents \
  --output benchmarks/real-repo-heldout-v1/grading/h03_schemeless_navigation/preflight.json
```

h02 and h05 use the `agi-backend-rust` source; h04 uses `agents`. Run Rust
calibrations one at a time. Fixture roots default to new `/private/tmp`
directories and contain the exported historical sources; delete them after
review.
