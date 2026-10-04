"""Adjudications exclude only the exact reviewed ambiguity, preserving evidence."""
from copy import deepcopy
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("real_repo_adjudications", Path(__file__).parents[1] / "scripts/real_repo_adjudications.py")
adjudication = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adjudication)

TRIAL = "r06_onboarding_outage-r1-off"
CHECK = "private timeout contract check"


class AdjudicationTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.grade_path = self.root / "trials" / TRIAL / "grade.json"
        self.grade_path.parent.mkdir(parents=True)
        self.grade = {"status": "fail", "checks": [
            {"name": "safe redirects", "status": "pass", "duration_ms": 3},
            {"name": CHECK, "status": "fail", "failures": ["PRIVATE-SENTINEL"]},
        ], "passed_checks": 1, "total_checks": 2, "log": "PRIVATE-SENTINEL"}
        self.record = {"id": TRIAL, "task_id": "r06_onboarding_outage", "repetition": 1, "arm": "off",
                       "grade": deepcopy(self.grade), "validity_issues": [], "agent_exit_code": 0,
                       "agent_timed_out": False, "grader_exit_code": 1,
                       "metrics": {"total_tokens": 123, "tool_calls": 31, "is_error": False},
                       "agent_wall_seconds": 256.447, "setup_seconds": 1, "grading_seconds": 6.45}
        self.entries = {TRIAL: {"grade_sha256": "", "failed_check": CHECK, "reason": "ambiguous_grader_contract"}}
        self.save_grade()

    def save_grade(self):
        self.grade_path.write_text(json.dumps(self.grade))
        self.entries[TRIAL]["grade_sha256"] = hashlib.sha256(self.grade_path.read_bytes()).hexdigest()

    def save_entries(self, **overrides):
        document = {"schema_version": 1, "adjudications": self.entries, **overrides}
        (self.root / "adjudications.json").write_text(json.dumps(document))

    def apply(self, record=None, entries=None):
        return adjudication.apply_adjudication(self.record if record is None else record, self.root,
                                              self.entries if entries is None else entries)

    def test_valid_exclusion_preserves_input_usage_and_raw_grade(self):
        before, entries = deepcopy(self.record), deepcopy(self.entries)
        raw = self.grade_path.read_bytes()
        derived = self.apply()
        self.assertEqual(derived["grade"], {"status": "infrastructure_error", "issues": ["ambiguous_grader_contract"]})
        self.assertEqual(derived["behavioral_grade"], before["grade"])
        self.assertEqual(derived["validity_issues"], ["ambiguous_grader_contract"])
        for key in ("metrics", "agent_wall_seconds", "setup_seconds", "grading_seconds", "agent_exit_code", "grader_exit_code", "agent_timed_out"):
            self.assertEqual(derived[key], before[key], key)
        self.assertEqual(self.record, before)
        self.assertEqual(self.entries, entries)
        self.assertEqual(self.grade_path.read_bytes(), raw)
        derived["behavioral_grade"]["checks"][0]["name"] = "changed copy"
        derived["metrics"]["total_tokens"] = 999
        self.assertEqual(self.record, before)

    def test_provenance_contains_no_raw_check_error_or_source(self):
        derived = self.apply()
        provenance = derived["adjudication"]
        self.assertEqual(provenance["schema_version"], 1)
        self.assertEqual(provenance["trial_id"], TRIAL)
        self.assertEqual(provenance["grade_sha256"], self.entries[TRIAL]["grade_sha256"])
        self.assertEqual(provenance["failed_check_sha256"], hashlib.sha256(CHECK.encode()).hexdigest())
        self.assertNotIn(CHECK, json.dumps(provenance))
        self.assertNotIn("PRIVATE-SENTINEL", json.dumps(provenance))
        self.assertIn(provenance["reason"], adjudication.EXPLANATIONS)

    def test_idempotent_after_classification_and_preserves_other_issues(self):
        original = deepcopy(self.record["grade"])
        self.record["behavioral_grade"] = original
        self.record["grade"] = {"status": "infrastructure_error", "issues": ["model_drift"]}
        self.record["validity_issues"] = ["model_drift"]
        derived = self.apply()
        self.assertEqual(derived["behavioral_grade"], original)
        self.assertEqual(derived["validity_issues"], ["model_drift", "ambiguous_grader_contract"])
        self.assertEqual(self.apply(derived), derived)

    def test_semantic_grade_ignores_diagnostics_and_check_order(self):
        self.record["grade"]["log"] = "different private path"
        self.record["grade"]["checks"].reverse()
        for check in self.record["grade"]["checks"]:
            check.pop("failures", None)
            check.pop("duration_ms", None)
            check["passed"] = check.pop("status") == "pass"
        self.assertEqual(self.apply()["grade"]["status"], "infrastructure_error")

    def test_stale_saved_grade_hash_rejected(self):
        self.grade_path.write_text(self.grade_path.read_text() + "\n")
        with self.assertRaisesRegex(adjudication.AdjudicationError, "digest"):
            self.apply()

    def test_changed_behavioral_outcome_rejected_even_when_hash_matches(self):
        self.record["grade"]["checks"][0]["status"] = "fail"
        self.record["grade"]["passed_checks"] = 0
        with self.assertRaisesRegex(adjudication.AdjudicationError, "disagrees"):
            self.apply()

    def test_extra_real_failure_cannot_be_excluded(self):
        self.grade["checks"].append({"name": "real regression", "status": "fail"})
        self.grade["total_checks"] = 3
        self.record["grade"] = deepcopy(self.grade)
        self.save_grade()
        with self.assertRaisesRegex(adjudication.AdjudicationError, "sole"):
            self.apply()

    def test_wrong_reviewed_failure_name_rejected(self):
        self.entries[TRIAL]["failed_check"] = "another failure"
        with self.assertRaisesRegex(adjudication.AdjudicationError, "sole"):
            self.apply()

    def test_no_pass_override(self):
        self.grade["status"] = "pass"
        self.grade["checks"][1]["status"] = "pass"
        self.grade["passed_checks"] = 2
        self.record["grade"] = deepcopy(self.grade)
        self.save_grade()
        with self.assertRaises(adjudication.AdjudicationError):
            self.apply()

    def test_unsafe_ids_and_unknown_reason_are_rejected(self):
        for identity in ("../PRIVATE-SENTINEL", "/tmp/PRIVATE-SENTINEL", "a/b", "..", "", None, []):
            with self.subTest(identity=identity):
                record = deepcopy(self.record)
                record["id"] = identity
                with self.assertRaises(adjudication.AdjudicationError) as raised:
                    self.apply(record)
                self.assertNotIn("PRIVATE-SENTINEL", str(raised.exception))
        self.entries[TRIAL]["reason"] = "arbitrary-private-text"
        with self.assertRaisesRegex(adjudication.AdjudicationError, "unsupported"):
            self.apply()

    def test_wrong_record_identity_components_rejected(self):
        for field, value in (("arm", "brain"), ("task_id", "r01_guide_hot_path"), ("repetition", 2), ("repetition", True)):
            with self.subTest(field=field):
                record = deepcopy(self.record)
                record[field] = value
                with self.assertRaisesRegex(adjudication.AdjudicationError, "identity"):
                    self.apply(record)

    def test_malformed_matching_records_are_rejected(self):
        mutations = [
            lambda r: r.update(grade=None),
            lambda r: r.update(validity_issues="not-list"),
            lambda r: r.update(validity_issues=[{}]),
            lambda r: r["grade"].update(checks=None),
            lambda r: r["grade"].update(checks=[None]),
            lambda r: r["grade"]["checks"][0].update(status="unknown"),
            lambda r: r["grade"]["checks"][0].update(passed=False),
            lambda r: r["grade"]["checks"][0].update(passed=1),
            lambda r: r["grade"]["checks"][0].update(name=CHECK),
            lambda r: r["grade"].update(total_checks=True),
        ]
        for change in mutations:
            record = deepcopy(self.record)
            change(record)
            with self.subTest(record=record), self.assertRaises(adjudication.AdjudicationError):
                self.apply(record)
        with self.assertRaises(adjudication.AdjudicationError):
            self.apply([])

    def test_missing_or_invalid_grade_evidence_is_rejected(self):
        self.grade_path.unlink()
        with self.assertRaises(adjudication.AdjudicationError):
            self.apply()
        self.grade_path.write_text("PRIVATE-SENTINEL invalid JSON")
        self.entries[TRIAL]["grade_sha256"] = hashlib.sha256(self.grade_path.read_bytes()).hexdigest()
        with self.assertRaises(adjudication.AdjudicationError) as raised:
            self.apply()
        self.assertNotIn("PRIVATE-SENTINEL", str(raised.exception))

    def test_symlinked_grade_is_rejected(self):
        actual = self.root / "other-grade.json"
        actual.write_bytes(self.grade_path.read_bytes())
        self.grade_path.unlink()
        self.grade_path.symlink_to(actual)
        with self.assertRaisesRegex(adjudication.AdjudicationError, "own regular file"):
            self.apply()

    def test_unmatched_records_are_unchanged_independent_copies(self):
        record = deepcopy(self.record)
        record["id"] = "unreviewed-trial"
        result = self.apply(record)
        self.assertEqual(result, record)
        result["metrics"]["tool_calls"] = 999
        self.assertEqual(record["metrics"]["tool_calls"], 31)
        del record["id"]
        self.assertEqual(self.apply(record), record)

    def test_absent_file_is_empty_and_valid_file_loads(self):
        self.assertEqual(adjudication.load_adjudications(self.root), {})
        self.save_entries()
        self.assertEqual(adjudication.load_adjudications(self.root), self.entries)

    def test_loader_rejects_unsupported_schema_reason_and_ids(self):
        cases = [
            {"schema_version": 2}, {"schema_version": True}, {"adjudications": []},
            {"adjudications": {"../escape": self.entries[TRIAL]}},
            {"adjudications": {TRIAL: {**self.entries[TRIAL], "reason": "unknown"}}},
            {"adjudications": {TRIAL: {**self.entries[TRIAL], "grade_sha256": "bad"}}},
            {"adjudications": {TRIAL: {**self.entries[TRIAL], "extra": "private"}}},
        ]
        for fields in cases:
            with self.subTest(fields=fields):
                self.save_entries(**fields)
                with self.assertRaises(adjudication.AdjudicationError):
                    adjudication.load_adjudications(self.root)

    def test_duplicate_json_keys_are_rejected(self):
        (self.root / "adjudications.json").write_text('{"schema_version":1,"schema_version":1,"adjudications":{}}')
        with self.assertRaises(adjudication.AdjudicationError):
            adjudication.load_adjudications(self.root)


if __name__ == "__main__":
    unittest.main()
