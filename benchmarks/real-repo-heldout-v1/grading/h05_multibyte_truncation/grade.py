#!/usr/bin/env python3
"""Private behavioral grader for h05. Run only against an expendable grading copy."""
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "_shared"))
sys.dont_write_bytecode = True
from rust_grader import RustTask, run  # noqa: E402

TASK = RustTask(
    task_id="h05_multibyte_truncation",
    private_tests=HERE / "hidden_tests.rs",
    test_target="engram_heldout_h05",
    module="engram_heldout_truncation",
    cases=(
        "multibyte_task_prompt_cut_inside_a_character_does_not_panic",
        "multibyte_context_fields_and_question_truncate_by_characters",
        "multibyte_transcript_entries_truncate_by_characters",
        "ascii_fields_truncate_exactly_as_before",
        "multibyte_text_within_the_character_limit_is_kept_whole",
    ),
    case_timeout_seconds=60,
)

if __name__ == "__main__":
    sys.exit(run(TASK))
