#!/usr/bin/env python3
"""Private behavioral grader for h01. Run only against an expendable grading copy."""
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "_shared"))
sys.dont_write_bytecode = True
from rust_grader import RustTask, run  # noqa: E402

TASK = RustTask(
    task_id="h01_effect_check_enforce_only",
    private_tests=HERE / "hidden_tests.rs",
    test_target="http_contract",
    module="engram_heldout_effect_check",
    append_to="tests/http_contract.rs",
    cases=(
        "shadow_mode_sends_no_effect_check_note_to_the_model",
        "off_mode_sends_no_effect_check_note_to_the_model",
        "non_enforcing_modes_record_no_effect_check_note_in_the_step",
        "enforce_mode_still_delivers_the_note_for_a_dead_tap",
        "enforce_mode_stays_silent_when_the_screen_moved",
        "shadow_mode_still_scores_unchanged_screens",
    ),
    case_timeout_seconds=120,
)

if __name__ == "__main__":
    sys.exit(run(TASK))
