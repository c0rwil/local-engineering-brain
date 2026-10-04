# Private Play base-plan lookup grader (h04, draft)

```sh
python3 -B benchmarks/real-repo-heldout-v1/grading/h04_play_base_plan_lookup/grade.py \
  --repo /private/tmp/candidate-grading-copy --output /private/tmp/h04-grade.json
```

Standard library only; Python 3.11 or newer. The grader executes the
candidate's real `RevenueCatService` and `SubscriptionRepository`. The
repository methods that read or write subscription state are replaced by
recording fakes on a subclass of the candidate's class; plan lookups run the
candidate's own repository code against a bounded in-memory query emulation
seeded with two synthetic plans. A fix in the service, at the webhook entry, or
in the repository is therefore graded by behavior. The real `agency.api.config`
package (billing and RevenueCat configuration) is loaded with
`REVENUECAT_BILLING=true`; logging, SQLAlchemy and database models are stubs.
Environment variables are cleared before import; sockets and subprocesses are
denied. Fixture ids, users and transactions are synthetic.

Eight checks plus a module-load check:

| Check | Contract |
| --- | --- |
| `initial_purchase_with_base_plan_grants_mapped_plan` | `sub_plus:monthly` upserts and grants `plan_plus`. |
| `renewal_with_base_plan_grants_mapped_plan` | A renewal of `sub_pro:annual` grants `plan_pro`. |
| `product_change_with_base_plan_switches_plan` | `new_product_id` `sub_pro:annual` switches the row to `plan_pro`. |
| `missing_row_for_mapped_base_plan_keeps_retrying` | Expiration, billing issue and cancellation for an unrecorded row of a mapped product raise (retry). |
| `unmapped_base_plan_still_retries` | Purchase and product change to an unmapped subscription raise, with no grant or plan change. |
| `unmapped_product_missing_row_is_still_acknowledged` | Unrecorded rows of unmapped products are acknowledged. |
| `plain_subscription_ids_still_map` | Ids without a suffix still grant, and still retry for unrecorded rows. |
| `credit_packs_are_unchanged` | A configured pack grants its configured amount; an unknown pack retries. |

The query emulation supports single-model queries, comparisons, `in_`,
`like`/`ilike`, boolean composition, ordering and the SQL functions `lower`,
`upper`, `trim`, `coalesce` and `split_part`. Raw SQL or other functions raise,
which fails the candidate: review such a failure by hand. Exit codes: 0 pass,
1 behavioral failure (including candidate import errors), 2 infrastructure
failure.

## Calibration

Base `ae4dd1d1`, reference `123a6ee6`. Recorded in `preflight.json`:

| Control | Kind | Expected |
| --- | --- | --- |
| `base` | base | The four base-plan cases fail. |
| `reference` | reference | All pass. |
| `lifecycle-only` | mutant | Product change and missing-row cases fail. |
| `missing-row-check-forgotten` | mutant | Only the missing-row case fails. |
| `keeps-base-plan-part` | mutant | Splits but keeps the base plan: the four base-plan cases fail. |
| `repository-normalizes` | alternative | Fix inside the repository lookup: passes. |
| `sql-split-part` | alternative | Repository compares against `split_part` in SQL: passes. |
| `normalize-at-entry` | alternative | Webhook entry rewrites product ids before dispatch: passes. |
