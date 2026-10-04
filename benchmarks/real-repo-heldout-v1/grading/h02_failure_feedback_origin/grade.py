#!/usr/bin/env python3
"""Private behavioral grader for h02. Run only against an expendable grading copy."""
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "_shared"))
sys.dont_write_bytecode = True
from rust_grader import RustTask, run  # noqa: E402

TASK = RustTask(
    task_id="h02_failure_feedback_origin",
    private_tests=HERE / "hidden_tests.rs",
    test_target="http_contract",
    module="engram_heldout_failure_origin",
    append_to="tests/http_contract.rs",
    cases=(
        "first_step_failure_carries_the_declared_origin",
        "first_step_failure_normalizes_origin_like_session_creation",
        "loaded_session_failure_keeps_the_frozen_origin",
        "unstamped_session_failure_stays_untagged",
        "reaction_feedback_event_carries_the_session_origin",
        "reaction_outcome_signals_carry_the_session_origin",
        "reaction_records_stay_untagged_without_origin",
        "successful_steps_keep_their_origin_stamp",
    ),
    case_timeout_seconds=120,
)

if __name__ == "__main__":
    sys.exit(run(TASK))
