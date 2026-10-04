#!/usr/bin/env python3
"""Calibrate the h05 grader against disposable historical exports.

python3 -B preflight.py --source-repo /path/to/agi-backend-rust --output preflight.json
"""
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "_shared"))
sys.dont_write_bytecode = True
from preflight_common import Control, Edit, Task, main  # noqa: E402

FILE = "src/services/human_response.rs"
BASE_TRUNCATE = """fn truncate(value: &str, max_chars: usize) -> String {
    if value.len() <= max_chars {
        return value.to_string();
    }
    let suffix = "... [truncated]";
    if max_chars <= suffix.len() {
        return suffix[..max_chars].to_string();
    }
    format!("{}{}", value[..max_chars - suffix.len()].trim_end(), suffix)
}"""
REFERENCE_TRUNCATE_END = """    let head: String = value.chars().take(max_chars - suffix_len).collect();
    format!("{}{}", head.trim_end(), SUFFIX)
}
"""
BYTE_TRUNCATE = """
fn truncate_bytes(value: &str, max_chars: usize) -> String {
    if value.len() <= max_chars {
        return value.to_string();
    }
    let suffix = "... [truncated]";
    if max_chars <= suffix.len() {
        return suffix[..max_chars].to_string();
    }
    format!("{}{}", value[..max_chars - suffix.len()].trim_end(), suffix)
}
"""
ALL = (
    "multibyte_task_prompt_cut_inside_a_character_does_not_panic",
    "multibyte_context_fields_and_question_truncate_by_characters",
    "multibyte_transcript_entries_truncate_by_characters",
    "multibyte_text_within_the_character_limit_is_kept_whole",
)

CONTROLS = (
    Control("base", "base", "base", "fail", "Historical base: byte-indexed truncation.",
            required_failures=ALL),
    Control("reference", "reference", "reference", "pass", "Historical fix."),
    Control("char-count-only", "mutant", "base", "fail",
            "Counts characters for the limit check but still slices by byte index.",
            edits=(Edit(FILE, "    if value.len() <= max_chars {\n        return value.to_string();",
                        "    if value.chars().count() <= max_chars {\n        return value.to_string();"),),
            required_failures=ALL[:3]),
    Control("byte-budget-at-char-boundary", "mutant", "base", "fail",
            "Avoids the panic by backing a byte budget off to a character boundary.",
            edits=(Edit(FILE, '    format!("{}{}", value[..max_chars - suffix.len()].trim_end(), suffix)\n}',
                        '    let mut cut = max_chars - suffix.len();\n'
                        '    while !value.is_char_boundary(cut) {\n        cut -= 1;\n    }\n'
                        '    format!("{}{}", value[..cut].trim_end(), suffix)\n}'),),
            required_failures=ALL),
    Control("prompt-fields-only", "mutant", "reference", "fail",
            "Fixes the prompt fields but keeps byte truncation for transcript history.",
            edits=(
                Edit(FILE, REFERENCE_TRUNCATE_END, REFERENCE_TRUNCATE_END + BYTE_TRUNCATE),
                Edit(FILE, "truncate(&prompt, MAX_HISTORY_FIELD_CHARS)", "truncate_bytes(&prompt, MAX_HISTORY_FIELD_CHARS)"),
                Edit(FILE, "truncate(&question, MAX_HISTORY_FIELD_CHARS)", "truncate_bytes(&question, MAX_HISTORY_FIELD_CHARS)"),
                Edit(FILE, "truncate(&action, MAX_HISTORY_FIELD_CHARS)", "truncate_bytes(&action, MAX_HISTORY_FIELD_CHARS)"),
                Edit(FILE, "truncate(&raw_output, MAX_HISTORY_FIELD_CHARS)", "truncate_bytes(&raw_output, MAX_HISTORY_FIELD_CHARS)"),
                Edit(FILE, "line = truncate(&line, MAX_TRANSCRIPT_CHARS);", "line = truncate_bytes(&line, MAX_TRANSCRIPT_CHARS);"),
            ),
            required_failures=(ALL[2],)),
    Control("char-indices-cut", "alternative", "base", "pass",
            "Legitimate alternative: finds the byte cut with char_indices instead of collecting chars.",
            edits=(Edit(FILE, BASE_TRUNCATE, """fn truncate(value: &str, max_chars: usize) -> String {
    const SUFFIX: &str = "... [truncated]";
    if value.chars().nth(max_chars).is_none() {
        return value.to_string();
    }
    if max_chars <= SUFFIX.len() {
        return SUFFIX[..max_chars].to_string();
    }
    let cut = value
        .char_indices()
        .nth(max_chars - SUFFIX.len())
        .map_or(value.len(), |(index, _)| index);
    format!("{}{}", value[..cut].trim_end(), SUFFIX)
}"""),)),
)

TASK = Task(
    task_id="h05_multibyte_truncation",
    base_commit="f566a1c24ddfc600fb21c0304680068158d50f35",
    reference_commit="dee1f322658ee319fbe0e3c64d67b89781cd46db",
    grader=HERE / "grade.py",
    preflight=Path(__file__).resolve(),
    private_files=(HERE / "hidden_tests.rs",),
    expected_check_count=6,
    controls=CONTROLS,
    toolchain_command=("rustc", "--version"),
)

if __name__ == "__main__":
    sys.exit(main(TASK))
