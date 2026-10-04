# Context quality comparison — 2026-09-24

On 18 fixed source-navigation probes across five real indexed repositories, the
updated compiler found all 16 intended files within its first five source pointers.
It used 59.8% fewer estimated context tokens and reduced median local API latency
from 96.519 ms to 4.721 ms. This is retrieval evidence, not demonstrated savings on
completed engineering tasks.

| Measurement | Before | After |
| --- | ---: | ---: |
| Intended file in first five pointers | 14/16 cases; 42/48 trials | 16/16 cases; 48/48 trials |
| Expected definition in first five pointers | 7/16 cases; 21/48 trials | 14/16 cases; 42/48 trials |
| Negative probes returning no source filler | 1/2 cases; 3/6 trials | 2/2 cases; 6/6 trials |
| Filler pointers across negative trials | 36 | 0 |
| Estimated context tokens across 54 brain trials | 25,794 | 10,359 |
| Source pointers across 54 brain trials | 507 | 240 |
| Median API latency | 96.519 ms | 4.721 ms |
| Maximum API latency, including cold requests | 751.210 ms | 43.425 ms |
| Median compiler-reported latency | 88.5 ms | 1 ms |
| Brain requests within token budget | 54/54 | 54/54 |
| Off controls with exactly zero injection | 54/54 | 54/54 |
| API failures | 0 | 0 |
| Cases with identical selections across repeats | 18/18 | 18/18 |

The remaining definition misses are `request_fingerprint` and the expected
`cancelled` definition for the `loopguard_enforcer` query. Their intended files
are present. A correct file pointer does not guarantee selection of the correct
symbol or a correct engineering decision.

## Changes evaluated

- Combine exact paths and symbols with scoped full-text source search; current
  edit paths no longer exclude other relevant files. Exclude navigation words
  and generic path fragments from lexical matching.
- Prefer direct sources before graph neighbors. Traverse indexed adjacency,
  deduplicate files, reuse prepared statements and bound labels and output.
- Reserve context space for relevant sources. Ordinary lessons need a relevance
  signal; validated project preferences remain standing conventions.
- Avoid linked-checkout Git scans when no active, validated memory has commit
  bounds that require checking.

Separately verified by tests: latest-prompt and recent-file delivery, budget-aware
refresh, identical-text suppression, atomic receipts, native `cmd` routing,
successful-emission acknowledgement, and a compact MCP result that sends context
text only once. These behaviors are not scored by the source-navigation probes.

## Method and reproduction

Raw results: [before.json](before.json), [after.json](after.json).
Harness: [eval_context_quality.py](../../../scripts/eval_context_quality.py).
Cases: [context_quality_cases.json](../../../tests/fixtures/context_quality_cases.json).

Each optimized binary ran sequentially under `caffeinate -dims`, with three fresh
brain requests and three off controls per case. The default budget was 1,600
estimated tokens. All trials, including cold requests, remain in the report.
No model calls were made. The five repositories were Engram, agents, platform,
mobile and agi-backend-rust.

The same read-only SQLite backup was copied into a private disposable directory
for each invocation. Both copies had their 43 checkout aliases removed before
startup, preventing linked-memory retrieval from reading live Git state. Indexed
project links remained available. Jobs, watchers, web, vault, judge and architect
were disabled. Requests supplied no checkout root, head or task attribution.
Migrations and context persistence affected only each disposable copy; the frozen
snapshot hash was unchanged. Earlier exploratory timings that still allowed live
checkout resolution are not used in this comparison.

```sh
caffeinate -dims python3 -B scripts/eval_context_quality.py \
  --daemon /path/to/optimized/engramd \
  --snapshot /path/to/frozen.sqlite \
  --cases tests/fixtures/context_quality_cases.json \
  --repeats 3 --output /tmp/context-quality.json
```

Use the same snapshot and cases for both binaries. `--freeze-from` creates a new
consistent SQLite backup and refuses to replace an existing snapshot.

- Before build: `fee50762a2dfa3c16c6de16752d2f6874e884e7435a14f2d9379472ad6fcee47`.
- After build: `0a88b7ab849429107cd6f73de627dfa07e384539e411598a5758409e334a79fb`.
- Snapshot SHA-256: `fe1114b8fe2cced0e3d43b085aa3da7cbab5e4e6af1faca8617740b09f30befc`.
- Cases SHA-256: `0e489bed06e1c095907c62481fe26b7775826eb767fc30416fe819a841db1ffa`.
- Harness SHA-256: `b9f110d950eefe4fc4a13c459a2da248d252005ef59d98aca61d2f5fcc3838fe`.

## Limits and validation

Queries are authored diagnostics, mostly symbol navigation with several natural
language and misleading-path cases. Repeated identical cases are not independent
samples. The corpus is a frozen local index, not a held-out benchmark. Source
pointers require inspecting current files; the index does not prove behavior.
Token sizes use the compiler's estimate, not provider billing tokens. This run
does not establish fewer tool calls, lower model bills, better task success, or
live hook latency. Those require paired off/brain agent tasks with measured usage.

Final validation passed formatting, strict all-target/all-feature clippy and all
358 tests, including both unchanged performance limits, migration synchronization,
scope isolation, delivery attribution and linked-repository commit validity.

The after build was installed locally. Post-install diagnostics passed, including
SQLite integrity and schema version 15. The source index contained all 69,165
eligible nodes. Existing configuration was unchanged and the local `engram-judge`
endpoint responded. A live `validate_verdict` lookup returned its exact definition
using 84 estimated tokens; that individual smoke check is not a latency benchmark.
A consistent pre-migration database/configuration backup was retained at
`~/.engram/backups/before-context-quality-20260925T024000Z`.
