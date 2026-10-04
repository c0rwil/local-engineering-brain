# Repaired follow-up collection controls

Collection is complete: 24/24 attempts, with zero infrastructure or unscorable outcomes. This is a new frozen collection with a repaired runner. The [earlier four-attempt collection](../2026-09-25-real-repo-followup-aborted/README.md) was stopped after every attempted Bash call failed before execution. Its raw evidence is archived and verified; its preliminary savings interpretation is withdrawn. Its resource use remains in the separate infrastructure-attempt section, outside this new 24-attempt matrix.

## Frozen comparison

Installed Engram build `58c2d0182e653f236e7ccd2c99f1dd67e78b2fd3623df66d064a0b2590eec395`; Claude Code 2.1.212; `claude-opus-4-8[1m]`; high effort. Four historical repository tasks × three repetitions × Off/Brain give 24 planned coding attempts. Each has 1,200 seconds for coding, 1,800 seconds for private grading and an $8 provider-reported usage-estimate cap. Subscription estimates are not incremental cash charges.

Protocol fingerprint: `ec058a5fc01c073f47dec35a1657e0a24234f76bac9eed92aeb9397f917370ab`. Runtime proof: `06941c30f47d538e5b8e1e160210154a2413266acb14c08545e014f0e287e30b`. The protocol preserves and checks the prompts, graders, collector and helper sources, binary hashes, fixture metadata, and runtime proof before and after each attempt. Completed attempts are not silently replaced.

All four fresh snapshots contain source only: schema 16, zero memories, tasks and compiled contexts. Historical commits and prepared dependency caches are fixed. Fixture setup, agent coding, and private grading have separate clocks. The runtime uses `/bin/bash`, bounded macOS sandbox patterns, protected sibling checkouts, and fail-closed sandbox settings.

## Acceptance and runtime checks

The unchanged v2 graders passed all 50 calibration controls: 18 valid implementations accepted and 32 base or deliberately broken implementations rejected. Behavioral check counts are 14 guide hot-path checks plus compilation, 33 lean-status checks, five environment-isolation checks plus compilation, and 31 onboarding-outage checks.

The [runtime verification](runtime-verification.json) drives actual CLI tools through a scripted local provider, with no live model calls. Both modes pass all seven named checks, including shell execution, heredocs and toolchains; active reads and writes; denied evidence, state, sibling, repository, alias and symlink reads; a 320-path stress case; and an attempt to disable sandboxing. Proof validation requires matching code, binaries, environment, settings, named checks and hashed execution artifacts. A before/after identity check rejects changes during the probe.

All 160 benchmark-tooling tests passed, along with scoped Python lint and diff checks. Independent review checked the compact policy, proof drift, symlink validation, post-attempt control verification, runtime exclusions and native import. Original v1 native evidence still passes its 416-value verification.

Detected Bash E2BIG launch failures now preserve the behavioral verdict and full reported usage while marking the attempt unscorable and stopping collection. Publication rechecks the raw trace and binds exclusions to its SHA-256. Aborted reports cannot regain savings through missing or substituted traces. A collection stop marker prevents resuming the stopped protocol.

## Operational pause at 19/24

The collector stopped itself on 2026-09-25 at 22:47 (local time) after recording 19 attempts. The stop happened before attempt 20 began, when the runtime proof check found a changed identity. The frozen sandbox policy lists every direct child of `/private/tmp/claude-501` by name. Claude Code keeps one `cache-break-state-<session>.json` file there for each session. When an operator opened a new Claude Code session, one name was added. Other sessions had removed three names present at preflight. Every other identity field matched: code, binaries, suite, fixtures and runtime artifacts. Post-attempt control checks do not depend on that directory, so none of the 19 records was affected.

Collection resumed on 2026-09-26 under the original proof, with no edits to records, prompts, graders, collector code or the proof. The three proof-time names were recreated as empty placeholders. The operator session's own bookkeeping file was kept absent while the collector ran. The collector was launched with the preflight's exact `PATH`, `LANG`, `HOME`, `USER` and `LOGNAME`. The standard proof check then passed before each of the remaining five attempts. The last attempt finished at 17:10 UTC, and the placeholders were removed afterward. The remaining five attempts therefore ran with byte-identical sandbox settings.

This fragility belongs to the protocol, not the product. A future collection should keep the per-session Claude bookkeeping names out of the frozen identity while still protecting them from the agent.

## Interpretation

Correctness and all-attempt resources come first. Savings require both members of a matched pair to finish cleanly, pass the private grader, and provide complete relevant measurements. Cache reads, cache creation, ordinary input and output remain separate. Agent wall time includes hook processing; internal hook operations are not counted as agent tool calls.

These development tasks informed retrieval improvements; they are not held-out generalization evidence. This compares the installed build with Off under the repaired protocol. It does not isolate improvement over the old Brain build, and it does not measure learned memories, the local judge, linked repositories or MCP tools. Provider caching and normal host background load are not reset; order alternates and three repetitions per task remain diagnostic.

## Native import and archive

All 24 records were imported into Engram's native evaluation view as `real-repo-followup-20260925` (collection fingerprint `ca18e0c9ed4e6707749f6bbfada1601a9f73b5f1ae40290b474300ce35eab1dc`). An independent comparison of the live native view with this report passed all 416 checks: four task groups, 12 pairs and 8 both-pass pairs. Re-importing was idempotent, leaving every comparison group byte-identical. The original pilot's native evidence still passes its own 416 checks.

The private evidence archive is at `~/.engram/benchmarks/2026-09-25-real-repo-followup/evidence.tar.gz`. It is 1,099,140,295 bytes, SHA-256 `81aaca0d056112f3fd4899e447c5d2973937f56df1dee99b7aa6b9ef805f2b89`, owner-only. An independent verifier read it without extracting it or executing archived code. It confirmed all 105,899 inventory entries, the 24-record matrix, 11 runtime-proof artifacts and 38 diagnostic record bindings, and recomputed volume coverage from the archived traces. Dependency and build caches are omitted (69 entries); this is evidence preservation, not an offline runtime. The archived copy of this document predates this section.
