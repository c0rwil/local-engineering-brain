#!/usr/bin/env python3
"""Calibrate the h02 grader against disposable historical exports.

python3 -B preflight.py --source-repo /path/to/agi-backend-rust --output preflight.json
"""
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "_shared"))
sys.dont_write_bytecode = True
from preflight_common import Control, Edit, Task, main  # noqa: E402

ENGINE = "src/services/action_engine.rs"
REACTIONS = "src/routes/reactions.rs"
FROZEN = """                    let error_origin = match deps.loaded_session.as_ref() {
                        Some(session) => session.meta.origin.clone(),
                        None => normalize_task_origin(request.origin.as_deref()),
                    };"""
FEEDBACK_TAG = """            // rate most needs.
            if let Some(origin) = origin.as_deref() {
                props.insert("origin".to_string(), json!(origin));
            }
            props
        };"""
FAILS = (
    "first_step_failure_carries_the_declared_origin",
    "first_step_failure_normalizes_origin_like_session_creation",
    "loaded_session_failure_keeps_the_frozen_origin",
)
REACTION_FAILS = (
    "reaction_feedback_event_carries_the_session_origin",
    "reaction_outcome_signals_carry_the_session_origin",
)

CONTROLS = (
    Control("base", "base", "base", "fail",
            "Historical base: failure and feedback records carry no origin.",
            required_failures=FAILS + REACTION_FAILS),
    Control("reference", "reference", "reference", "pass", "Historical fix."),
    Control("request-origin-wins", "mutant", "reference", "fail",
            "Tags failures from the current request even when a session exists.",
            edits=(Edit(ENGINE, FROZEN,
                        "                    let error_origin = normalize_task_origin(request.origin.as_deref());"),),
            required_failures=("loaded_session_failure_keeps_the_frozen_origin",
                               "unstamped_session_failure_stays_untagged")),
    Control("raw-request-origin", "mutant", "reference", "fail",
            "Uses the first-step request origin without session-creation normalization.",
            edits=(Edit(ENGINE, "                        None => normalize_task_origin(request.origin.as_deref()),",
                        "                        None => request.origin.clone(),"),),
            required_failures=("first_step_failure_normalizes_origin_like_session_creation",)),
    Control("failure-events-only", "mutant", "reference", "fail",
            "Tags model-failure events but leaves reaction feedback untagged.",
            edits=(Edit(REACTIONS, FEEDBACK_TAG, "            // rate most needs.\n            props\n        };"),),
            required_failures=REACTION_FAILS),
    Control("feedback-only", "mutant", "reference", "fail",
            "Tags reaction feedback but leaves model-failure events untagged.",
            edits=(Edit(ENGINE, "                        origin: error_origin.as_deref(),",
                        "                        origin: None,"),),
            required_failures=FAILS),
    Control("equivalent-expression", "alternative", "reference", "pass",
            "Legitimate alternative: the same freeze written as map_or_else, and the feedback tag added after the props are built.",
            edits=(
                Edit(ENGINE, FROZEN, """                    let error_origin = deps.loaded_session.as_ref().map_or_else(
                        || normalize_task_origin(request.origin.as_deref()),
                        |session| session.meta.origin.clone(),
                    );"""),
                Edit(REACTIONS, FEEDBACK_TAG, """            // rate most needs.
            props
        };
        let feedback_props = {
            let mut props = feedback_props;
            if let Some(origin) = origin.clone() {
                props.insert("origin".to_string(), json!(origin));
            }
            props
        };"""),
            )),
)

TASK = Task(
    task_id="h02_failure_feedback_origin",
    base_commit="2b690115d49f9e7cf0610ef601c92baf6323edba",
    reference_commit="c67bbf4d3e4d7815f82b5fedb4a5cdf0beef219f",
    grader=HERE / "grade.py",
    preflight=Path(__file__).resolve(),
    private_files=(HERE / "hidden_tests.rs",),
    expected_check_count=9,
    controls=CONTROLS,
    toolchain_command=("rustc", "--version"),
)

if __name__ == "__main__":
    sys.exit(main(TASK))
