# Context improvements from the real-task pilot

The follow-up improves required-file coverage in initial source packs from **6/22 to 12/22** across four historical tasks and their reordered requirements. Exact component coverage rises from **0/32 to 10/32**. These are development diagnostics, not new coding-agent outcomes or provider savings.

## Changes

- Collect identifiers and paths across a bounded 64 KiB prompt before selecting prose. Preserve short repeated terms and hyphenated compounds. Suppress weak identifier fragments only after a precise pointer is actually selected, within the same project; preserve graph neighbors and independent relevance.
- Index bounded Rust named fields as aliases of their owner types. Qualify impl methods and repair ownership. Refresh older native indexes even if Git content is unchanged. Unchanged v1 Rust files retain node identities and memory anchors; edited or recreated files conservatively retire old v1 symbol identities. Historical tombstones cannot be rebound to new owners.
- Persist a delivery signature with each compiled output. Source-map order does not affect it; displayed content, identities, revision metadata, lesson validity, facts, task and workspace do. Keep only the latest successful delivery as the comparison baseline. Failed output retries, empty/removal/reset and later restoration remain eligible. Hooks recompile identical inputs to notice database changes.

Field aliases cover Rust named struct fields only, up to 128 fields and 16 KiB per owner. Other languages keep existing symbol extraction. Packs retain at most one pointer per file; multiple required definitions in one file cannot all be directly represented.

## Measurements

| Diagnostic | Before | After |
| --- | ---: | ---: |
| Required files anywhere in full-task packs | 6/22 | 12/22 |
| Required files in first five pointers | 3/22 | 4/22 |
| Exact required components anywhere in packs | 0/32 | 10/32 |
| Exact components in first five pointers | 0/32 | 4/32 |
| Estimated context tokens, eight full-task variants (one repeat) | 2,749 | 2,808 |
| Navigation intended file at five | 16/16 | 16/16 |
| Navigation exact definition at five | 14/16 | 14/16 |
| Navigation estimated tokens, 54 Brain compilations | 10,359 | 10,569 |
| Navigation API latency median | 4.931 ms | 6.880 ms |
| Historical suite API latency median | 5.715 ms | 7.466 ms |

Full-task coverage counts the original task text and a reordered-sentence variant once each. Each case also ran three times; repeats had identical selections and are not independent samples. Three positive field/method probes and one absent-field negative are reported separately in raw historical-suite results. Historical-task requests retained the same 1,600-token budget and use no current-file hints. Navigation preserves the prior 18-case suite, including its budget settings and one unrelated `README.md` current-file hint. Neither suite makes model calls or measures provider usage.

| Task / variant | Before files | After files | Before definitions | After definitions |
| --- | ---: | ---: | ---: | ---: |
| r01 / full_task | 1/3 | 2/3 | 0/5 | 2/5 |
| r01 / requirements_reordered | 2/3 | 2/3 | 0/5 | 2/5 |
| r02 / full_task | 1/3 | 1/3 | 0/3 | 0/3 |
| r02 / requirements_reordered | 1/3 | 1/3 | 0/3 | 0/3 |
| r05 / full_task | 0/2 | 1/2 | 0/4 | 1/4 |
| r05 / requirements_reordered | 1/2 | 1/2 | 0/4 | 1/4 |
| r06 / full_task | 0/3 | 2/3 | 0/4 | 2/4 |
| r06 / requirements_reordered | 0/3 | 2/3 | 0/4 | 2/4 |

Missed components, including consumer/lock-site coverage, remain visible in the raw reports. The task probes are authored from the completed pilot, exclude the runner's administrative wrapper and tool-history file hints, and were used during development. They are not held-out evidence. Better navigation does not establish a causal task success improvement.

The original pilot audit found 12 order-only deliveries containing 4,335 estimated text tokens. The new hook regression tests demonstrate order-only suppression and changed-content delivery. Historical runs lack the new persisted signatures, so that old text total is an opportunity estimate, not a newly measured provider saving. Removing the input-only shortcut adds a compilation per hook; compiler/API timings here do not measure the complete hook path.

An initial candidate increased navigation text and lost a status-polling file. That candidate was rejected; the final refinement restored bounded compound/prose relevance and removed unnecessary fragment expansion. Intermediate diagnostics remain in the private working evidence directory. No original pilot prompts, graders, results or imported records were rewritten.

## Reproduction and provenance

- Before build: `a63f33b28c942fdf6771c240eb7945a33862c0ef0e3de09700d15805e707ab9f`.
- Installed after build: `58c2d0182e653f236e7ccd2c99f1dd67e78b2fd3623df66d064a0b2590eec395`.
- Frozen navigation snapshot: `fe1114b8fe2cced0e3d43b085aa3da7cbab5e4e6af1faca8617740b09f30befc`; both runs verified it unchanged.
- [Navigation before](navigation-before.json), [navigation after](navigation-after.json), [task before](tasks-before.json), [task after](tasks-after.json), [summary](summary.json).
- Harnesses: [eval_context_quality.py](../../../scripts/eval_context_quality.py), [eval_task_context.py](../../../scripts/eval_task_context.py). Cases: [navigation](../../../tests/fixtures/context_quality_cases.json), [historical tasks](../../../tests/fixtures/task_context_cases.json). Reports record binary, case, helper and harness hashes.

The navigation suite copies the same schema-14 snapshot; each binary migrates only its own copy. The task suite copies each original snapshot and clones its one-commit historical checkout, verifies the frozen HEAD, refreshes the isolated index, removes checkout aliases, then deletes the checkout before retrieval. Native v1 sees already-current files; v2 pays a one-time reindex. All original snapshots are hash-checked before and after. Jobs, watcher, web, judge and architect are disabled. Index setup time and alias size are recorded separately from query latency.

```sh
python3 -B scripts/eval_task_context.py \
  --daemon /path/to/versioned/engramd \
  --fixtures /private/path/to/frozen-pilot/fixtures \
  --cases tests/fixtures/task_context_cases.json \
  --reindex --repeats 3 --output /tmp/new-task-context-result.json
```

Run each binary with identical fixtures/cases and a new output filename. Provider token/tool-call savings require a new controlled agent evaluation with corrected grading contracts.

## Validation and installation

The full Rust suite, targeted final retrieval/index upgrades, strict all-target/all-feature Clippy, formatting, and four Python coverage-accounting tests passed. Independent review covered scoping, budget fallbacks, false suppression, upgrade identity collisions, tombstone/recreated-file safety and benchmark accounting. A consistent database/configuration backup was saved before migration.

The candidate was installed in the default workspace, live repositories refreshed, and schema 16 plus SQLite/FTS integrity verified. Configuration is unchanged. The live field-owner smoke check returned `ActionRequest` from `src/models/api.rs`. See [installed verification](installed-verification.json) and [system verification](SYSTEM_VERIFICATION.md) for native benchmark preservation and local service checks.
