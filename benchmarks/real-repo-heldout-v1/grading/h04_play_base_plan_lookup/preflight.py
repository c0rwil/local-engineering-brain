#!/usr/bin/env python3
"""Calibrate the h04 grader against disposable historical exports.

python3 -B preflight.py --source-repo /path/to/agents --output preflight.json
"""
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "_shared"))
sys.dont_write_bytecode = True
from preflight_common import Control, Edit, Task, main  # noqa: E402

SERVICE = "src/agency/api/services/revenuecat_service.py"
REPOSITORY = "src/agency/api/repositories/subscription_repository.py"
LOOKUP = """        return self.db.query(Plan).filter(Plan.play_product_id == play_product_id).one_or_none()"""
CHANGE_SITE = "plan = repo.get_plan_by_play_product_id(play_subscription_id(new_product_id))"
MISSING_SITE = "and repo.get_plan_by_play_product_id(play_subscription_id(product_id)) is None"
GRANTS = (
    "initial_purchase_with_base_plan_grants_mapped_plan",
    "renewal_with_base_plan_grants_mapped_plan",
    "product_change_with_base_plan_switches_plan",
    "missing_row_for_mapped_base_plan_keeps_retrying",
)

CONTROLS = (
    Control("base", "base", "base", "fail", "Historical base: exact-match lookups at all three sites.",
            required_failures=GRANTS),
    Control("reference", "reference", "reference", "pass", "Historical fix."),
    Control("lifecycle-only", "mutant", "reference", "fail",
            "Strips the base plan only where purchases and renewals resolve their plan.",
            edits=(Edit(SERVICE, CHANGE_SITE, "plan = repo.get_plan_by_play_product_id(new_product_id)"),
                   Edit(SERVICE, MISSING_SITE, "and repo.get_plan_by_play_product_id(product_id) is None")),
            required_failures=GRANTS[2:]),
    Control("missing-row-check-forgotten", "mutant", "reference", "fail",
            "Fixes grants and plan changes but not the permanent-missing-row decision.",
            edits=(Edit(SERVICE, MISSING_SITE, "and repo.get_plan_by_play_product_id(product_id) is None"),),
            required_failures=GRANTS[3:]),
    Control("keeps-base-plan-part", "mutant", "reference", "fail",
            "Splits on the separator but keeps the base-plan part instead of the subscription id.",
            edits=(Edit(SERVICE, 'return product_id.split(":", 1)[0]', 'return product_id.split(":")[-1]'),),
            required_failures=GRANTS),
    Control("repository-normalizes", "alternative", "base", "pass",
            "Legitimate alternative: the repository lookup drops the base plan for every caller.",
            edits=(Edit(REPOSITORY, LOOKUP, """        subscription_id = play_product_id.split(":", 1)[0] if play_product_id else play_product_id
        return self.db.query(Plan).filter(Plan.play_product_id == subscription_id).one_or_none()"""),)),
    Control("sql-split-part", "alternative", "base", "pass",
            "Legitimate alternative: the repository compares against split_part of the incoming id in SQL.",
            edits=(Edit(REPOSITORY, LOOKUP, """        return (
            self.db.query(Plan)
            .filter(Plan.play_product_id == func.split_part(play_product_id, ":", 1))
            .one_or_none()
        )"""),)),
    Control("normalize-at-entry", "alternative", "base", "pass",
            "Legitimate alternative: the webhook entry point rewrites product ids before dispatch.",
            edits=(Edit(SERVICE, """        event_id = event.get("id", "unknown")
""", """        event_id = event.get("id", "unknown")
        event = {
            **event,
            **{
                key: event[key].split(":", 1)[0]
                for key in ("product_id", "new_product_id")
                if isinstance(event.get(key), str)
            },
        }
"""),)),
)

TASK = Task(
    task_id="h04_play_base_plan_lookup",
    base_commit="ae4dd1d1dee3d449a30b9f91aee713ef3e012a9f",
    reference_commit="123a6ee6ed9c969591d8f21181b404a8458cc7ce",
    grader=HERE / "grade.py",
    preflight=Path(__file__).resolve(),
    private_files=(),
    expected_check_count=9,
    controls=CONTROLS,
    grader_timeout_seconds=600,
)

if __name__ == "__main__":
    sys.exit(main(TASK))
