"""The held-out suite stays a draft with calibration evidence that matches its graders."""
import hashlib
import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
SUITE = ROOT / "benchmarks/real-repo-heldout-v1"
SHARED_BY_REPOSITORY = {
    "agi-backend-rust": ("preflight_common.py", "rust_grader.py"),
    "agents": ("preflight_common.py", "py_harness.py"),
}


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class HeldoutCatalogTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.catalog = json.loads((SUITE / "catalog.json").read_text())
        cls.tasks = cls.catalog["tasks"]

    def test_suite_is_a_draft_awaiting_operator_review(self):
        self.assertEqual(self.catalog["suite_id"], "real-repo-heldout-v1")
        self.assertEqual(self.catalog["execution_status"], "draft_requires_operator_review")
        self.assertEqual(self.catalog["status"], "draft_requires_operator_review")
        self.assertFalse(self.catalog["executable_runner_config"])
        self.assertIn("None", self.catalog["result_claims"])

    def test_every_task_has_prompt_grader_and_evidence(self):
        self.assertGreaterEqual(len(self.tasks), 4)
        for task in self.tasks:
            with self.subTest(task=task["id"]):
                directory = SUITE / "grading" / task["id"]
                for path in (SUITE / task["prompt_file"], SUITE / task["grader"]["entrypoint"],
                             SUITE / task["grader"]["preflight_evidence"], directory / "README.md",
                             directory / "preflight.py"):
                    self.assertTrue(path.is_file(), path)
                self.assertRegex(task["base_commit"], r"^[0-9a-f]{40}$")
                self.assertRegex(task["reference_fix_commit"], r"^[0-9a-f]{40}$")

    def test_recorded_calibration_matches_catalog_and_current_graders(self):
        for task in self.tasks:
            with self.subTest(task=task["id"]):
                directory = SUITE / "grading" / task["id"]
                report = json.loads((SUITE / task["grader"]["preflight_evidence"]).read_text())
                self.assertEqual(report["task_id"], task["id"])
                self.assertEqual(report["base_commit"], task["base_commit"])
                self.assertEqual(report["reference_fix_commit"], task["reference_fix_commit"])
                self.assertEqual(report["status"], "pass")
                runs = [run["fixture"] for run in report["runs"]]
                self.assertEqual(runs, task["grader"]["calibration_controls"])
                self.assertTrue(all(run["calibrated"] for run in report["runs"]))
                self.assertEqual(report["expected_check_count"], task["grader"]["private_check_count"] + 1)
                by_name = {run["fixture"]: run for run in report["runs"]}
                self.assertEqual(by_name["reference"]["exit_code"], 0)
                self.assertEqual(by_name["base"]["exit_code"], 1)
                self.assertTrue(by_name["base"]["failed_checks"])
                self.assertTrue(any(run["kind"] == "mutant" for run in report["runs"]))
                # Calibration is evidence only for the grader files it ran.
                self.assertEqual(report["grader_sha256"], sha256(directory / "grade.py"))
                self.assertEqual(report["preflight_sha256"], sha256(directory / "preflight.py"))
                for name, digest in report["private_files_sha256"].items():
                    self.assertEqual(digest, sha256(directory / name), name)
                for name in SHARED_BY_REPOSITORY[task["repository"]]:
                    self.assertEqual(report["shared_sha256"].get(name), sha256(SUITE / "grading/_shared" / name), name)

    def test_prompts_carry_no_reference_or_grader_details(self):
        v1 = json.loads((ROOT / "benchmarks/real-repo-v1/catalog.json").read_text())
        v2 = json.loads((ROOT / "benchmarks/real-repo-v2/catalog.json").read_text())
        earlier = {task[key] for catalog in (v1, v2) for task in catalog["tasks"]
                   for key in ("base_commit", "reference_fix_commit")}
        for task in self.tasks:
            with self.subTest(task=task["id"]):
                prompt = (SUITE / task["prompt_file"]).read_text()
                for commit in (task["base_commit"], task["reference_fix_commit"]):
                    self.assertNotIn(commit[:7], prompt)
                for path in task["evaluator_source_paths"]:
                    self.assertNotIn(path, prompt)
                self.assertNotRegex(prompt, re.compile(r"hidden|grader|preflight|reference fix", re.IGNORECASE))
                self.assertNotIn(task["reference_fix_commit"], earlier)
                self.assertNotIn(task["base_commit"], earlier)


if __name__ == "__main__":
    unittest.main()
