"""Controls for immutable follow-up collection and historical fixture reuse."""
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import run_real_repo_followup as followup  # noqa: E402


class FollowupControlsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="engram-followup-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.args = SimpleNamespace(data=self.root)
        self.controls = {"model": "fixed-model", "suite_files": {"prompt.md": "hash"}}

    def test_manifest_resume_accepts_identical_controls_without_rewriting(self):
        manifest = {"controls": self.controls, "frozen_at": "original-time"}
        path = self.root / "manifest.json"
        original = json.dumps(manifest, indent=1) + "\n"
        path.write_text(original)
        with mock.patch.object(followup, "controls", return_value=self.controls):
            self.assertEqual(followup.freeze_manifest(self.args, {}), manifest)
        self.assertEqual(path.read_text(), original)

    def test_manifest_resume_rejects_changed_model_prompt_or_fixture(self):
        path = self.root / "manifest.json"
        path.write_text(json.dumps({"controls": self.controls}))
        for changed in ({**self.controls, "model": "changed"},
                        {**self.controls, "suite_files": {"prompt.md": "different"}},
                        {**self.controls, "fixture_hashes": {"task": "different"}}):
            with self.subTest(controls=changed), mock.patch.object(followup, "controls", return_value=changed):
                with self.assertRaisesRegex(ValueError, "controls changed"):
                    followup.freeze_manifest(self.args, {})

    def test_manifest_cannot_be_created_after_attempts_start(self):
        (self.root / "trials").mkdir()
        with mock.patch.object(followup, "controls", return_value=self.controls):
            with self.assertRaisesRegex(ValueError, "after trials"):
                followup.freeze_manifest(self.args, {})

    def test_source_fixtures_cannot_overlap_active_or_evidence_roots(self):
        data, active, seeds = (self.root / name for name in ("data", "active", "seeds"))
        followup.validate_seed_roots(data, active, seeds)
        for forbidden in (active, active / "fixtures", data, data / "fixtures", self.root):
            with self.subTest(seeds=forbidden), self.assertRaisesRegex(ValueError, "must be separate"):
                followup.validate_seed_roots(data, active, forbidden)

    def test_daemon_changes_invalidate_prepared_fixture_even_with_same_cli(self):
        destination = self.root / "fixtures" / "synthetic"
        destination.mkdir(parents=True)
        (destination / "ready.json").write_text("{}")
        self.args.seed_fixtures = self.root / "seeds"
        for daemon in (None, "old-daemon"):
            prepared = {"engram_sha256": "same-cli", "daemon_sha256": daemon}
            with mock.patch.object(followup, "validate_fixture", side_effect=[{}, prepared]), \
                 mock.patch.object(followup.runner, "digest", side_effect=["same-cli", "new-daemon"]):
                with self.assertRaisesRegex(ValueError, "another Engram release"):
                    followup.prepare_fixture({"id": "synthetic"}, self.args)

    def test_infrastructure_results_stop_collection_after_preservation(self):
        for status in ("infra", "infrastructure_error", "unknown"):
            with mock.patch.object(followup, "classify_record", return_value={"grade": {"status": status}}):
                with self.assertRaisesRegex(RuntimeError, "attempt preserved"):
                    followup.stop_if_unscorable({})
        for status in ("pass", "fail"):
            with mock.patch.object(followup, "classify_record", return_value={"grade": {"status": status}}):
                followup.stop_if_unscorable({})

    def test_stopped_collection_cannot_resume(self):
        followup.refuse_stopped_collection(self.root)
        (self.root / "collection_stop.json").write_text('{"resume_allowed": false}')
        with self.assertRaisesRegex(ValueError, "stopped"):
            followup.refuse_stopped_collection(self.root)

    def test_trace_runtime_audit_runs_before_record_persistence(self):
        record = {"id": "trial", "grade": {"status": "pass"}, "metrics": {"total_tokens": 123}}
        trace = self.root / "trials/trial/trace.jsonl"
        trace.parent.mkdir(parents=True)
        trace.write_text("synthetic trace")
        changed = {**record, "grade": {"status": "infrastructure_error"}, "behavioral_grade": record["grade"]}
        with mock.patch.object(followup, "controls", return_value=self.controls), \
             mock.patch.object(followup, "frozen_inputs_match", return_value=True), \
             mock.patch.object(followup, "apply_tool_runtime_checks", return_value=changed) as audit:
            followup.validate_trial_controls(record, self.args, {}, self.controls)
        audit.assert_called_once_with(record, trace)
        self.assertEqual(record["grade"]["status"], "infrastructure_error")
        self.assertEqual(record["metrics"]["total_tokens"], 123)

    def test_mid_trial_drift_preserves_behavior_and_usage_but_excludes_success(self):
        record = {"grade": {"status": "pass"}, "metrics": {"total_tokens": 123}}
        with mock.patch.object(followup, "controls", return_value={"changed": True}):
            followup.validate_trial_controls(record, self.args, {}, self.controls)
        self.assertEqual(record["behavioral_grade"], {"status": "pass"})
        self.assertEqual(record["metrics"]["total_tokens"], 123)
        self.assertEqual(record["grade"]["status"], "infrastructure_error")
        self.assertIn("collection_control_drift", record["validity_issues"])
        with self.assertRaisesRegex(RuntimeError, "attempt preserved"):
            followup.stop_if_unscorable(record)

    def test_control_validation_failure_is_unscorable(self):
        record = {"grade": {"status": "fail"}}
        with mock.patch.object(followup, "controls", side_effect=OSError("deleted control")):
            followup.validate_trial_controls(record, self.args, {}, self.controls)
        self.assertEqual(record["grade"]["status"], "infrastructure_error")

    def test_changed_frozen_grader_is_unscorable_even_if_public_sources_match(self):
        record = {"grade": {"status": "pass"}}
        with mock.patch.object(followup, "controls", return_value=self.controls), \
             mock.patch.object(followup, "frozen_inputs_match", return_value=False):
            followup.validate_trial_controls(record, self.args, {}, self.controls)
        self.assertEqual(record["grade"]["status"], "infrastructure_error")

    def test_unchanged_inputs_leave_the_result_intact(self):
        record = {"grade": {"status": "pass"}, "metrics": {"total_tokens": 123}}
        with mock.patch.object(followup, "controls", return_value=self.controls), \
             mock.patch.object(followup, "frozen_inputs_match", return_value=True):
            followup.validate_trial_controls(record, self.args, {}, self.controls)
        self.assertEqual(record, {"grade": {"status": "pass"}, "metrics": {"total_tokens": 123}})

    def test_shared_adapter_validates_before_both_records_are_written(self):
        args = SimpleNamespace(data=self.root / "data", active=self.root / "active")
        args.data.mkdir()

        def mutate_during_trial(_source, _destination):
            self.controls["model"] = "changed-during-trial"
            raise RuntimeError("synthetic setup failure")

        original = dict(self.controls)
        args.validate_record = lambda record: followup.validate_trial_controls(record, args, {}, original)
        with mock.patch.object(followup.runner, "clone", side_effect=mutate_during_trial), \
             mock.patch.object(followup, "controls", side_effect=lambda *_: dict(self.controls)):
            result = followup.runner.run_trial({"id": "synthetic", "repository": "synthetic"}, 1, "off", args)
        stored = json.loads((args.data / "trials/synthetic-r1-off/record.json").read_text())
        ledger = json.loads((args.data / "records.jsonl").read_text())
        self.assertEqual(stored, result)
        self.assertEqual(ledger, result)
        self.assertIn("collection_control_drift", stored["validity_issues"])


class FollowupFixtureTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="engram-followup-fixture-")
        self.addCleanup(self.temp.cleanup)
        self.fixture = Path(self.temp.name)
        self.work = self.fixture / "work"
        self.work.mkdir()
        self.file = self.work / "Cargo.lock"
        self.file.write_text("frozen dependency lock\n")
        self.git("init", "-q")
        self.git("add", "Cargo.lock")
        self.git("-c", "user.name=Benchmark", "-c", "user.email=benchmark@invalid", "commit", "-qm", "Base")
        (self.fixture / "snapshot.sqlite").write_bytes(b"synthetic snapshot bytes")
        self.task = {"id": "synthetic", "base_commit": "a" * 40}
        self.ready = {"task_id": "synthetic", "base_commit": self.task["base_commit"],
                      "fixture_head": self.git("rev-parse", "HEAD").strip(),
                      "snapshot_sha256": followup.runner.digest(self.fixture / "snapshot.sqlite"),
                      "lockfile_sha256": {"Cargo.lock": followup.runner.digest(self.file)}}
        self.save()

    def git(self, *arguments):
        return followup.runner.command(["git", "-C", self.work, *arguments])

    def save(self):
        (self.fixture / "ready.json").write_text(json.dumps(self.ready))

    def test_accepts_unchanged_fixture_and_legacy_without_lock_hash(self):
        self.assertEqual(followup.validate_fixture(self.task, self.fixture), self.ready)
        del self.ready["lockfile_sha256"]
        self.save()
        self.assertEqual(followup.validate_fixture(self.task, self.fixture), self.ready)

    def test_rejects_modified_snapshot(self):
        (self.fixture / "snapshot.sqlite").write_bytes(b"changed snapshot")
        with self.assertRaisesRegex(ValueError, "snapshot digest"):
            followup.validate_fixture(self.task, self.fixture)

    def test_rejects_dirty_and_untracked_source(self):
        self.file.write_text("modified lock\n")
        with self.assertRaisesRegex(ValueError, "changes or untracked"):
            followup.validate_fixture(self.task, self.fixture)
        self.git("checkout", "--", "Cargo.lock")
        (self.work / "future-solution.txt").write_text("must not enter the fixture")
        with self.assertRaisesRegex(ValueError, "changes or untracked"):
            followup.validate_fixture(self.task, self.fixture)

    def test_rejects_extra_git_history_even_if_head_is_recorded(self):
        self.git("-c", "user.name=Benchmark", "-c", "user.email=benchmark@invalid", "commit", "--allow-empty", "-qm", "Future")
        self.ready["fixture_head"] = self.git("rev-parse", "HEAD").strip()
        self.save()
        with self.assertRaisesRegex(ValueError, "one root commit"):
            followup.validate_fixture(self.task, self.fixture)


if __name__ == "__main__":
    unittest.main()
