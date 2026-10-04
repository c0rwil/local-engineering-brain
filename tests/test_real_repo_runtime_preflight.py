"""Collection requires intact, matching evidence of actual runtime checks."""
import json
import contextlib
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import preflight_real_repo_runtime as preflight


class RuntimePreflightProofTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.data = Path(self.temp.name)
        self.active = self.data / "active"
        self.identity = {"policy": "fixed"}
        directory = self.data / "runtime-preflight"
        directory.mkdir()
        self.proof = {"schema_version": 1, "status": "pass", "identity": self.identity,
                      "arms": ["off", "brain"], "artifacts": {}}
        canary = directory / "private-canary.txt"
        canary.write_text("synthetic canary")
        self.proof["artifacts"][str(canary.relative_to(self.data))] = preflight.runner.digest(canary)
        for arm in self.proof["arms"]:
            for suffix in ("checks.json", "trace.jsonl", "tool-results.json", "stderr.log", "settings.json"):
                path = directory / f"{arm}.{suffix}"
                path.write_text(json.dumps({"passed": True, "checks": dict.fromkeys(preflight.CHECK_NAMES, True)})
                                if suffix == "checks.json" else "synthetic evidence")
                self.proof["artifacts"][str(path.relative_to(self.data))] = preflight.runner.digest(path)
        self.save()

    def save(self):
        (self.data / "runtime-preflight.json").write_text(json.dumps(self.proof))

    def validate(self):
        with mock.patch.object(preflight, "identity", return_value=self.identity):
            return preflight.validate_proof(self.data, self.active)

    def test_accepts_matching_complete_proof(self):
        self.assertEqual(self.validate(), self.proof)

    def test_absent_or_changed_identity_blocks_collection(self):
        self.proof["identity"] = {"policy": "old"}
        self.save()
        with self.assertRaisesRegex(ValueError, "differs"):
            self.validate()
        (self.data / "runtime-preflight.json").unlink()
        with self.assertRaises(OSError):
            self.validate()

    def test_changed_trace_blocks_collection(self):
        (self.data / "runtime-preflight/off.trace.jsonl").write_text("changed")
        with self.assertRaisesRegex(ValueError, "artifact differs"):
            self.validate()

    def test_missing_execution_artifact_cannot_be_a_pass(self):
        del self.proof["artifacts"]["runtime-preflight/off.trace.jsonl"]
        self.save()
        with self.assertRaisesRegex(ValueError, "exact tool execution"):
            self.validate()

    def test_missing_arm_or_extra_artifact_cannot_be_a_pass(self):
        self.proof["arms"] = ["off"]
        self.save()
        with self.assertRaises(ValueError):
            self.validate()
        self.proof["arms"] = ["off", "brain"]
        self.proof["artifacts"]["unknown.json"] = "a" * 64
        self.save()
        with self.assertRaisesRegex(ValueError, "exact tool execution"):
            self.validate()

    def test_failed_isolation_check_is_not_a_pass_even_with_updated_digest(self):
        relative = "runtime-preflight/brain.checks.json"
        path = self.data / relative
        result = json.loads(path.read_text())
        result["checks"]["sandboxed_bash"] = False
        path.write_text(json.dumps(result))
        self.proof["artifacts"][relative] = preflight.runner.digest(path)
        self.save()
        with self.assertRaisesRegex(ValueError, "every isolation check"):
            self.validate()

    def test_cli_rejects_leaf_symlinks_before_resolving_them(self):
        for option, prefix in (("data", "erp26-"), ("active", "erp26work-")):
            with self.subTest(option=option), tempfile.TemporaryDirectory(prefix=prefix, dir="/private/tmp") as target:
                alias = self.data / (option + "-alias")
                alias.symlink_to(Path(target), target_is_directory=True)
                data = alias if option == "data" else Path("/private/tmp/erp26-runtime-cli-unit-data")
                active = alias if option == "active" else Path("/private/tmp/erp26work-runtime-cli-unit-active")
                argv = ["preflight", "--data", str(data), "--active", str(active), "--engram-release", "/nonexistent"]
                with mock.patch.object(sys, "argv", argv), mock.patch.object(preflight, "run_preflight") as run:
                    with self.assertRaises(ValueError):
                        preflight.main()
                    run.assert_not_called()

class RuntimePreflightExecutionTests(unittest.TestCase):
    def run_synthetic_preflight(self, root, bash_output="RUNTIME_PRIVATE_TMP_OK\nRUNTIME_BASH_OK", claude_creates_private=True,
                                identities=({"code": "before"}, {"code": "before"})):
        def local_provider(calls):
            received = [{"is_error": False, "content": "RUNTIME_ALLOWED"}]
            received += [{"is_error": True, "content": "permission denied"} for _ in calls[1:-2]]
            received += [{"is_error": False, "content": marker} for marker in (bash_output, "OVERRIDE_REMAINS_SANDBOXED")]
            return SimpleNamespace(server_port=12345, shutdown=lambda: None, server_close=lambda: None), received

        data, active = Path(root) / "data", Path(root) / "active"

        def claude(*_args, **_kwargs):
            # Claude Code creates claude-UID inside CLAUDE_CODE_TMPDIR on first use.
            if claude_creates_private:
                (preflight.runner.claude_tmp_target(active) / f"claude-{os.getuid()}").mkdir(parents=True, exist_ok=True)
            return SimpleNamespace(returncode=0)

        shared = Path(root) / "shared-claude-tmp"
        shared.mkdir()
        with (mock.patch.object(preflight.runner, "validate_paths"),
              mock.patch.object(preflight, "identity", side_effect=list(identities)),
              mock.patch.object(preflight, "SHARED_CLAUDE_TMP", shared),
              mock.patch.object(preflight.runner, "settings", return_value={}),
              mock.patch.object(preflight.runner, "coding_env", return_value={}),
              mock.patch.object(preflight.runner, "prepare_claude_tmp",
                                side_effect=lambda path: preflight.runner.claude_tmp_target(path).mkdir(parents=True, exist_ok=True)),
              mock.patch.object(preflight.runner, "release_claude_tmp"),
              mock.patch.object(preflight.runner, "daemon", return_value=contextlib.nullcontext()),
              mock.patch.object(preflight.runner, "cli"),
              mock.patch.object(preflight.runner, "agent_command", return_value=["synthetic-cli"]),
              mock.patch.object(preflight, "scripted_provider", side_effect=local_provider),
              mock.patch.object(preflight.subprocess, "run", side_effect=claude) as run):
            try:
                return preflight.run_preflight(data, active), run, data
            except (ValueError, RuntimeError) as error:
                return error, run, data

    def test_control_drift_during_successful_tools_cannot_publish_passing_proof(self):
        with tempfile.TemporaryDirectory() as root:
            error, run, data = self.run_synthetic_preflight(root, identities=({"code": "before"}, {"code": "after"}))
            self.assertIsInstance(error, ValueError)
            self.assertIn("controls changed during tool execution", str(error))
            self.assertEqual(run.call_count, 2)
            self.assertEqual(data.stat().st_mode & 0o777, 0o700)
            self.assertFalse((data / "runtime-preflight.json").exists())
            for arm in ("off", "brain"):
                self.assertTrue(json.loads((data / f"runtime-preflight/{arm}.checks.json").read_text())["passed"])

    def test_claude_temp_outside_the_trial_fails_the_preflight(self):
        for bash_output, creates in (("RUNTIME_BASH_OK", True), ("RUNTIME_PRIVATE_TMP_OK\nRUNTIME_BASH_OK", False)):
            with self.subTest(bash_output=bash_output, creates=creates), tempfile.TemporaryDirectory() as root:
                error, run, data = self.run_synthetic_preflight(root, bash_output=bash_output, claude_creates_private=creates)
                self.assertIsInstance(error, RuntimeError)
                checks = json.loads((data / "runtime-preflight/off.checks.json").read_text())
                self.assertFalse(checks["checks"]["private_claude_tmp"])
                self.assertFalse((data / "runtime-preflight.json").exists())

    def test_passing_preflight_records_private_claude_temp_evidence(self):
        with tempfile.TemporaryDirectory() as root:
            proof, _, data = self.run_synthetic_preflight(root)
            self.assertEqual(proof["status"], "pass")
            for arm in ("off", "brain"):
                checks = json.loads((data / f"runtime-preflight/{arm}.checks.json").read_text())
                self.assertEqual(set(checks["checks"]), preflight.CHECK_NAMES)
                self.assertEqual(checks["claude_tmp"]["shared_entries_for_this_run"], [])
                self.assertIs(checks["claude_tmp"]["shared_write_probe_created"], False)


class SupersedeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.data, self.active = Path(self.temp.name) / "data", Path(self.temp.name) / "active"
        (self.data / "runtime-preflight").mkdir(parents=True)
        (self.data / "runtime-preflight/off.trace.jsonl").write_text("earlier evidence")
        (self.data / "runtime-preflight.json").write_text('{"status": "pass"}')
        (self.active / "work").mkdir(parents=True)
        (self.active / ".pilot-owned").write_text("runtime-preflight\n")

    def test_earlier_evidence_is_kept_and_the_preflight_active_root_cleared(self):
        preflight.supersede(self.data, self.active)
        archived = list(self.data.glob("runtime-preflight.superseded-*"))
        self.assertEqual(len(archived), 1)
        self.assertEqual((archived[0] / "runtime-preflight/off.trace.jsonl").read_text(), "earlier evidence")
        self.assertTrue((archived[0] / "runtime-preflight.json").is_file())
        self.assertFalse((self.data / "runtime-preflight").exists())
        self.assertFalse(self.active.exists())

    def test_supersede_is_refused_after_attempts_or_for_a_trial_active_root(self):
        (self.active / ".pilot-owned").write_text("real-repo-v1\n")
        with self.assertRaisesRegex(ValueError, "not left by a runtime preflight"):
            preflight.supersede(self.data, self.active)
        (self.active / ".pilot-owned").write_text("runtime-preflight\n")
        for started in ("manifest.json", "records.jsonl"):
            (self.data / started).write_text("{}")
            with self.subTest(started=started), self.assertRaisesRegex(ValueError, "after the collection has started"):
                preflight.supersede(self.data, self.active)
            (self.data / started).unlink()
        self.assertTrue((self.data / "runtime-preflight.json").is_file())
        self.assertTrue(self.active.is_dir())


class RuntimeIdentityTests(unittest.TestCase):
    def test_identity_ignores_other_roots_while_attempt_policies_deny_them(self):
        data = Path("/private/tmp/erp26-identity-unit-data")
        active = Path("/private/tmp/erp26work-identity-unit-active")
        before = preflight.identity(data, active)
        with tempfile.TemporaryDirectory(prefix="erp26work-identity-unit-sibling-", dir="/private/tmp") as sibling, \
             tempfile.TemporaryDirectory(prefix="claude-identity-unit-", dir="/private/tmp") as session:
            (Path(session) / "cwd-1234").write_text("another session")
            self.assertEqual(preflight.identity(data, active), before)
            executed = preflight.runner.settings(active / "work", data, active, active / "state", "off", compact=True)
            self.assertIn(str(Path(sibling).resolve()), executed["sandbox"]["filesystem"]["denyRead"])
            self.assertIn(f"Read(/{Path(sibling).resolve()}/**)", executed["permissions"]["deny"])
        frozen = before["settings"]["off"]
        # Only the trial's own Engram state; no inventory of other roots.
        self.assertEqual([path for path in frozen["sandbox"]["filesystem"]["denyRead"] if "erp26work-" in path],
                         [str(active / "state")])
        self.assertIn("/private/tmp/claude-**", frozen["sandbox"]["filesystem"]["denyRead"])


class SharedClaudeTempTests(unittest.TestCase):
    def test_only_entries_of_this_run_count_as_shared_use(self):
        with tempfile.TemporaryDirectory() as root:
            shared = Path(root)
            work = Path("/private/tmp/erp26work-unit_active/work")
            for name in ("cache-break-state-other-session.json", "-Users-someone-project", "bash-edit-diff",
                         "cache-break-state-trial-session.json", "-private-tmp-erp26work-unit-active-work"):
                (shared / name).mkdir()
            with mock.patch.object(preflight, "SHARED_CLAUDE_TMP", shared):
                self.assertEqual(preflight.shared_claude_tmp_traces(work, {"trial-session"}),
                                 ["-private-tmp-erp26work-unit-active-work", "cache-break-state-trial-session.json"])
                self.assertEqual(preflight.shared_claude_tmp_traces(work, set()), ["-private-tmp-erp26work-unit-active-work"])

    def test_session_ids_come_from_the_stream_trace(self):
        with tempfile.TemporaryDirectory() as root:
            trace = Path(root) / "trace.jsonl"
            trace.write_text('{"type": "system", "session_id": "one"}\nnot json\n{"type": "result", "session_id": "two"}\n')
            self.assertEqual(preflight.session_ids(trace), {"one", "two"})


if __name__ == "__main__":
    unittest.main()
