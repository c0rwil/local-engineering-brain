#!/usr/bin/env python3
"""Calibrate the h01 grader against disposable historical exports.

python3 -B preflight.py --source-repo /path/to/agi-backend-rust --output preflight.json
"""
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "_shared"))
sys.dont_write_bytecode = True
from preflight_common import Control, Edit, Task, main  # noqa: E402

ENGINE = "src/services/action_engine.rs"
BASE_SCREEN = """        let screen = self
            .screen_evidence(deps.loaded_session.as_ref(), request.image.as_deref())
            .await
            .unwrap_or_default();
        let last_effect = screen.effect.clone();
        if let Some(note) = last_effect.as_ref().and_then(effect_check_note) {"""
BASE_NOTE = "        if let Some(note) = last_effect.as_ref().and_then(effect_check_note) {"

CONTROLS = (
    Control("base", "base", "base", "fail", "Historical base: the note is delivered in every mode.",
            required_failures=("shadow_mode_sends_no_effect_check_note_to_the_model",
                               "off_mode_sends_no_effect_check_note_to_the_model",
                               "non_enforcing_modes_record_no_effect_check_note_in_the_step")),
    Control("reference", "reference", "reference", "pass", "Historical fix."),
    Control("gate-evidence-on-enforce", "mutant", "base", "fail",
            "Silences the note by skipping the screen evidence outside enforce, which also blinds shadow scoring.",
            edits=(Edit(ENGINE, BASE_SCREEN, """        let screen = if self.settings.loopguard_mode == "enforce" {
            self.screen_evidence(deps.loaded_session.as_ref(), request.image.as_deref())
                .await
                .unwrap_or_default()
        } else {
            Default::default()
        };
        let last_effect = screen.effect.clone();
        if let Some(note) = last_effect.as_ref().and_then(effect_check_note) {"""),),
            required_failures=("shadow_mode_still_scores_unchanged_screens",)),
    Control("blind-loopguard-outside-enforce", "mutant", "base", "fail",
            "Keeps fingerprinting screens but hides the effect reading from shadow LoopGuard as well as the note.",
            edits=(Edit(ENGINE, "        let last_effect = screen.effect.clone();\n" + BASE_NOTE,
                        "        let last_effect = screen\n            .effect\n            .clone()\n"
                        "            .filter(|_| self.settings.loopguard_mode == \"enforce\");\n" + BASE_NOTE),),
            required_failures=("shadow_mode_still_scores_unchanged_screens",)),
    Control("silence-shadow-only", "mutant", "base", "fail",
            "Suppresses the note only for shadow, so off still steers the model.",
            edits=(Edit(ENGINE, BASE_NOTE, """        if let Some(note) = last_effect
            .as_ref()
            .filter(|_| self.settings.loopguard_mode != "shadow")
            .and_then(effect_check_note)
        {"""),),
            required_failures=("off_mode_sends_no_effect_check_note_to_the_model",
                               "non_enforcing_modes_record_no_effect_check_note_in_the_step")),
    Control("drop-note-everywhere", "mutant", "base", "fail",
            "Removes the note in every mode, including enforce.",
            edits=(Edit(ENGINE, BASE_NOTE,
                        "        if let Some(note) = last_effect.as_ref().filter(|_| false).and_then(effect_check_note) {"),),
            required_failures=("enforce_mode_still_delivers_the_note_for_a_dead_tap",)),
    Control("mode-flag-before-note", "alternative", "base", "pass",
            "Legitimate alternative: decides delivery once from the configured mode, case-insensitively.",
            edits=(Edit(ENGINE, BASE_NOTE, """        let deliver_effect_note = self.settings.loopguard_mode.eq_ignore_ascii_case("enforce");
        let effect_note = if deliver_effect_note {
            last_effect.as_ref().and_then(effect_check_note)
        } else {
            None
        };
        if let Some(note) = effect_note {"""),)),
)

TASK = Task(
    task_id="h01_effect_check_enforce_only",
    base_commit="f566a1c24ddfc600fb21c0304680068158d50f35",
    reference_commit="c24e931dc23321ec5612679c8a7e38490b7e0e47",
    grader=HERE / "grade.py",
    preflight=Path(__file__).resolve(),
    private_files=(HERE / "hidden_tests.rs",),
    expected_check_count=7,
    controls=CONTROLS,
    toolchain_command=("rustc", "--version"),
)

if __name__ == "__main__":
    sys.exit(main(TASK))
