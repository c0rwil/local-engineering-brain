# Review expiry on the pending review backlog, 4 October 2026

Scope: `engram review expire` (`src/expiry.rs`) and migration 23. This records
how much of the pending lesson-review backlog the operator-run expiry would
close. All numbers are aggregates. The database holds proprietary claims,
commands and paths; none of them is committed.

## Method

- Input: a copy of the newest backup, `pre-roadmap-20261004.sqlite` (written at
  07:04 local time, schema 16), in a scratch directory. The live database and
  daemon were not used.
- An isolated daemon from this branch opened the copy through its own socket,
  with `engramd --config` disabling jobs, the watcher, the browser, the judge
  and the vault. Opening the copy migrated it to schema 23.
- Dry runs through `POST /api/v1/reviews/expire` at 14, 7 and 0 days, at about
  17:00 local time. Planning read the registered checkouts with local-only Git
  and bounded file reads, as the judge's live source check and the
  re-verification plan do. Nothing ran.
- The copy was then expired at 0 days and one review reopened, to check the
  write path on real rows. The copy was deleted afterwards.

## Migration

| | Before (schema 16) | After (schema 23) |
| --- | ---: | ---: |
| Pending reviews | 135 | 135 |
| Accepted reviews | 11 | 11 |
| Judgment audit rows | 1,589 | 1,589 |
| Foreign key violations | | 0 |

## Results

The backlog is young: 96 of the 135 pending reviews were created on 28 and 29
September, 5 to 6 days before the run. None has changed since it was created.

| Threshold | Considered | Would expire | Kept pending |
| --- | ---: | ---: | ---: |
| 14 days (default) | 2 | 2 | 0 |
| 7 days | 16 | 10 | 6 |
| Any age (`0d`) | 135 | 114 | 21 |

At 14 days, 2 of the 135 would expire now, both held by
`no_verified_evidence`. The 96 reviews from 28 and 29 September reach 14 days
on 12 and 13 October.

At any age, by gate class:

| Gate class | Reviews |
| --- | ---: |
| `no_verified_evidence` | 84 |
| `unanchored_files` | 30 |
| `revision_changed` | 0 |

Reviews kept pending at any age:

| Reason | Reviews |
| --- | ---: |
| Another gate (outside checkouts 3, differs from the index 3, task revision 3, evidence revision 2, missing file 2) | 13 |
| `no_verified_evidence`, but a runnable re-verification could clear it | 6 |
| No gate: the judge would ask its model | 2 |

The judge's gates match the earlier re-verification plan
(`2026-10-04-reverify`): 90 `no_verified_evidence`, 30 unanchored files
without a hook digest, and 15 others. All 30 unanchored reviews and 84 of the
90 without verified evidence would expire. For the other 6, a runnable
re-verification could still clear the gate.

None of the five revision gates (3 task, 2 evidence) is backed by a known
content change: the comparison found no touched file whose content at the
recorded revision differs from its live content. They stay pending as
`other_gate`.

The 114 expirable reviews spread over 6 projects (30, 27, 25, 24, 6 and 2).

Expiring the copy at any age changed 114 reviews to `expired`, kept all 1,589
judgment rows, and recorded 114 expiries (84 `no_verified_evidence`, 30
`unanchored_files`). Reopening one returned it to pending with its 2 judgment
rows and an `expired, reopened` history.

## Reading

With the default 14 days, expiry clears almost nothing today because the
backlog is less than two weeks old. Gates that only new verification work
clears hold 114 of the 135 reviews. If those gates do not change, the default
run would close most of the backlog from 12 and 13 October, when the reviews
from 28 and 29 September reach 14 days.
