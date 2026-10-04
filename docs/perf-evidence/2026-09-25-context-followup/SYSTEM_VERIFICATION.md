# Installed verification, 25 September 2026

The installed default-workspace build is
`58c2d0182e653f236e7ccd2c99f1dd67e78b2fd3623df66d064a0b2590eec395`.
Its source hash and all installed binary hashes match the isolated candidate used
for the final measurements. The daemon reports the same build and opens the
canonical `~/.engram/brain.sqlite` database.

## Validation

- `cargo test --locked --offline --all-targets --all-features`: **399 passed**.
- `cargo clippy --locked --offline --all-targets --all-features -- -D warnings`: passed.
- `cargo fmt --check` and `git diff --check`: passed.
- Four Python task-context accounting tests and scoped Ruff checks: passed.
- Independent review found and resolved cross-project suppression, unavailable
  exact-match fallbacks, premature candidate truncation, extractor identity
  collisions, historical tombstones and recreated-file safety. The truncation
  regression was observed failing before the fix and passing afterward.

The two offline suites each ran Brain and Off controls three times per case.
All budget and negative controls passed; selections were repeat-consistent.
These repeated deterministic compilations are not independent agent trials.

## Storage and live indexes

A consistent SQLite/configuration backup was saved before migration:
`~/.engram/backups/before-context-improvements-20260925T162024Z`.
The backup database is 779,673,600 bytes, passes integrity checking, and has SHA-256
`89578b80683fe2088ea9386dedcace874ce9836e26216680da80c70a0a52abdd`.

The live database passed SQLite integrity checking at schema 16. Search contained
69,811 eligible source rows and exactly 69,811 FTS rows; canonical project keys
matched. There are 643 active owners with field aliases. Five active validated
memories remain. Configuration SHA-256 is unchanged.

Each primary checkout retained its existing project ID and completed a native
extractor refresh:

| Repository | Files parsed | Refresh duration |
| --- | ---: | ---: |
| engram | 189 | 3.620 s |
| agi-backend-rust | 127 | 3.295 s |
| agents | 1,037 | 3.855 s |
| mobile | 1,559 | 9.493 s |
| platform | 672 | 2.045 s |

These are observed one-time refresh durations, not controlled performance
comparisons; [refresh receipts](live-index-refresh.json) retain the scan metadata.
A live `ActionRequest::use_app_guides` query selected `ActionRequest`
from `src/models/api.rs`, stayed within budget at 197 estimated context tokens,
and persisted a delivery signature. See [installed verification](installed-verification.json).

## Services and retained benchmark records

All eleven `engram doctor` checks passed. The dashboard at
`http://127.0.0.1:11436/` returned HTTP 200 and the Engram page. The existing local
judge remains enabled in the default workspace; its loopback `/v1/models`
endpoint returned HTTP 200 and the `engram-judge` model. These are service/configuration
checks, not a new evaluation of the judge's semantic accuracy. The
[service responses](services-verification.json) record status and model metadata.

An independent comparison against the frozen pilot report passed **416 checks**
across all four native comparison groups: **24 attempts, 12 pairs, one pair where
both arms passed**. It checked control hashes, outcomes, measurement coverage,
provider token counts, tool calls, turns, timing and estimated cost. The original
pilot outcomes and evidence were not rewritten or rerun.
The [comparison receipt](native-benchmark-verification.json) records the result.

Detailed local build/test logs, refresh receipts, service responses and the
independent comparison result remain in
`/private/tmp/engram-context-improve-20260925/`.
