"""Collection requires intact, matching evidence of actual runtime checks."""
import json
import contextlib
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
    def test_control_drift_during_successful_tools_cannot_publish_passing_proof(self):
        def local_provider(calls):
            received = [{"is_error": False, "content": "RUNTIME_ALLOWED"}]
            received += [{"is_error": True, "content": "permission denied"} for _ in calls[1:-2]]
            received += [{"is_error": False, "content": marker}
                         for marker in ("RUNTIME_BASH_OK", "OVERRIDE_REMAINS_SANDBOXED")]
            return SimpleNamespace(server_port=12345, shutdown=lambda: None, server_close=lambda: None), received

        with tempfile.TemporaryDirectory() as root:
            data, active = Path(root) / "data", Path(root) / "active"
            with (mock.patch.object(preflight.runner, "validate_paths"),
                  mock.patch.object(preflight, "identity", side_effect=[{"code": "before"}, {"code": "after"}]),
                  mock.patch.object(preflight.runner, "settings", return_value={}),
                  mock.patch.object(preflight.runner, "coding_env", return_value={}),
                  mock.patch.object(preflight.runner, "daemon", return_value=contextlib.nullcontext()),
                  mock.patch.object(preflight.runner, "cli"),
                  mock.patch.object(preflight.runner, "agent_command", return_value=["synthetic-cli"]),
                  mock.patch.object(preflight, "scripted_provider", side_effect=local_provider),
                  mock.patch.object(preflight.subprocess, "run", return_value=SimpleNamespace(returncode=0)) as run):
                with self.assertRaisesRegex(ValueError, "controls changed during tool execution"):
                    preflight.run_preflight(data, active)
            self.assertEqual(run.call_count, 2)
            self.assertEqual(data.stat().st_mode & 0o777, 0o700)
            self.assertFalse((data / "runtime-preflight.json").exists())
            for arm in ("off", "brain"):
                self.assertTrue(json.loads((data / f"runtime-preflight/{arm}.checks.json").read_text())["passed"])


if __name__ == "__main__":
    unittest.main()
