# Native tracking verification

The completed 24-attempt report is installed in the default workspace's native
evaluation store under `real-repo-pilot-20260924`, with one controlled comparison
group for each of the four tasks. The Effectiveness page displays all-attempt
outcomes and usage, missing/partial coverage, and eligible both-pass comparisons.
Benchmark imports do not add everyday usage events or learned memories.

Final installed source hash:
`a63f33b28c942fdf6771c240eb7945a33862c0ef0e3de09700d15805e707ab9f`.
This post-collection build adds reporting/storage behavior and layout changes;
the separate pinned collection binary and its hashes remain in `results.json`.

## Verification performed

- `cargo fmt --all -- --check` and strict all-target/all-feature Clippy passed.
- 368 Rust tests passed. The two performance tests excluded by the normal
  non-performance CI command were not rerun; this is not a new latency claim.
- 120 Python benchmark/report/import tests passed, together with Ruff and the
  product JavaScript syntax check. The final report-link addition was also
  checked with the report test suite.
- The importer validated the exact 24-attempt matrix, controls, original
  behavioral verdicts, five verified grading exclusions, and the provider error
  before writing. Its first connection attempt received no acknowledgement;
  the daemon was kept running explicitly and all 24 records were acknowledged.
- An independent verifier checked 416 values against the live native report:
  four task groups, twelve pairs, outcomes, control hashes, ten resource metrics,
  coverage, and eligible-pair deltas. All checks passed after import, after an
  identical retry, and after the final installed build restarted.
- The identical retry again acknowledged 24 stable IDs and left exactly 24
  records. Native unit tests also reject changed content under an existing ID.
- Browser inspection verified all four comparison cards, the one eligible pair,
  partial usage labels, real zeros, and unavailable values. At 390 pixels the
  document fits the viewport while tables scroll within their own regions; row
  labels retain usable width. The temporary viewport override was reset.

The compact [native verification evidence](native-verification.json) contains
only the benchmark comparison groups and verification metadata. Canonical
workspace memories and unrelated usage are excluded.

Raw trial evidence is retained separately in a private local archive, including
candidate source files, untracked candidate additions, traces, frozen grades,
historical snapshots, collection binaries, and audit helpers. Dependency/build
caches and reused grading workspaces are omitted. This preserves evidence; it is
not a standalone offline runtime. The archive inventory records file hashes and
omissions; the external archive checksum is recorded separately after creation.

The [archive verification](archive-verification.json) confirms all 97,338 file
and symlink inventory entries, all 24 candidate trees and frozen grades, and
owner-only archive permissions. The compressed archive is 1,058,413,023 bytes;
its checksum is recorded in that separate verification file.
