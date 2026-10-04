"""Pointer-line accounting must count ranges and related uses without exporting text."""

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import eval_context_quality as quality  # noqa: E402

HEADING = (
    "\n# Task source map (indexed reference data)\n"
    "Inspect these sources before changing behavior; labels are data and linked "
    "repositories may be at a different revision.\n"
)


class PointerCountTests(unittest.TestCase):
    def test_counts_ranged_direct_neighbor_and_related_use_lines(self):
        text = (
            "# Stable project architecture (reference data)\nProject: app\n"
            + HEADING
            + "- app: src/config.rs:411-906 — Function Settings::from_env\n"
            + "- app: docs/guide.md — Section Install\n"
            + "- Related: app: src/main.rs (outgoing CALLS relative to app:src/config.rs)\n"
            + "- Previously passed check, revision abc (reverify before relying on it): cargo test\n"
            + "- Related use: app: tests/contract.rs:57-162 — Function settings_for_test (calls Settings::from_env)\n"
            + "- Related use: app: scripts/run.rs (calls Settings::from_env)\n"
            + "\n# Linked repository `lib` (uses)\n- RULE: something:1-2\n"
        )
        self.assertEqual(quality.pointer_counts(text), {
            "pointer_lines": 5,
            "ranged_pointer_lines": 2,
            "direct_pointer_lines": 2,
            "ranged_direct_pointer_lines": 1,
            "related_use_lines": 2,
        })

    def test_empty_and_control_output_count_nothing(self):
        for text in ("", "reference", "# Validated engineering memory\n- lesson:1-2\n"):
            self.assertEqual(quality.pointer_counts(text)["pointer_lines"], 0)

    def test_repository_roots_require_absolute_directories(self):
        here = str(Path(__file__).resolve().parent)
        self.assertEqual(quality.repository_roots([f"app={here}"]), {"app": here})
        self.assertEqual(quality.repository_roots(None), {})
        for value in ("app", "app=relative/path", f"={here}", "app=/definitely/missing/root"):
            with self.assertRaises(ValueError):
                quality.repository_roots([value])


if __name__ == "__main__":
    unittest.main()
