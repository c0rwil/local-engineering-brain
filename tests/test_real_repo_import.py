"""The native importer preserves benchmark denominators, privacy and controls."""

from contextlib import redirect_stderr, redirect_stdout
from copy import deepcopy
import importlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch
import uuid

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "scripts"))
importer = importlib.import_module("import_real_repo_pilot")


def complete_report():
    manifest = {field: "frozen-control" for field in importer.MANIFEST_TEXT}
    manifest.update({field: "a" * 64 for field in importer.MANIFEST_HASHES})
    manifest.update(schema_version=1, model="claude-pinned[1m]", effort="high", repetitions=3,
                    agent_limit_seconds=1200, grading_limit_seconds=1800, cost_cap_usd_estimate=8)
    fixtures, records = [], []
    for task in importer.TASKS:
        fixtures.append({"task_id": task, "base_commit": "b" * 40, "fixture_head": "c" * 40,
                         "snapshot_sha256": "d" * 64, "lockfile_sha256": {}})
        for repetition in (1, 2, 3):
            for arm in importer.ARMS:
                records.append({
                    "task_id": task, "repetition": repetition, "arm": arm,
                    "record_classification_revision": importer.RECORD_CLASSIFICATION_REVISION,
                    "started_at": f"2026-09-25T04:{len(records):02d}:00.123456+00:00",
                    "grade": {"status": "pass", "checks": [{"name": "behavior", "passed": True}]},
                    "metrics": {"input_tokens": 10, "output_tokens": 20, "cache_creation_input_tokens": 30,
                                "cache_read_input_tokens": 40, "processed_input_tokens": 80, "total_tokens": 100,
                                "tool_calls": 5, "num_turns": 6, "cost_usd_estimate": .15,
                                "is_error": False, "trace_complete": True, "usage_complete": True,
                                "tool_calls_complete": True, "result_subtype": "success",
                                "models": [manifest["model"]], "model_usage": {manifest["model"]: {"inputTokens": 11}}},
                    "agent_wall_seconds": 10.125, "agent_exit_code": 0, "grader_exit_code": 0,
                    "agent_timed_out": False, "usage_may_be_partial": False, "validity_issues": [],
                    "delivery": {"emission_receipts": 1 if arm == "brain" else 0},
                    "prompt_sha256": "e" * 64, "composed_prompt_sha256": "f" * 64, "snapshot_sha256": "d" * 64,
                    "grader_manifest": {f"grading/{task}/{name}": "1" * 64 for name in importer.GRADER_FILES[task]},
                })
    return {
        "schema_version": 1, "record_classification_revision": importer.RECORD_CLASSIFICATION_REVISION,
        "generated_at": "2026-09-26T00:00:00+00:00", "manifest": manifest, "fixtures": fixtures, "records": records,
        "progress": {"status": "complete", "planned_runs": 24, "started_trials": 24, "coding_runs_started": 24,
                     "completed_records": 24, "in_progress_trials": 0, "infrastructure_outcomes": 0},
        "preflight_runs": [], "infrastructure_attempts": [],
    }


def payloads(report=None, label="pilot-2026"):
    return importer.build_payloads(complete_report() if report is None else report, label, "/public/results.json")


class RealRepoImportTest(unittest.TestCase):
    def test_tool_spawn_failures_require_evidence_and_remain_unscorable(self):
        report = complete_report()
        row = report["records"][0]
        row["behavioral_grade"] = deepcopy(row["grade"])
        row["grade"] = {"status": "infra", "checks": []}
        row["validity_issues"] = ["shell_spawn_argument_limit"]
        row["tool_runtime_evidence"] = {"schema_version": 1, "trace_sha256": "9" * 64,
            "reason": "shell_spawn_argument_limit", "shell_tool_calls": 3, "shell_spawn_failure_count": 3}
        report["progress"]["infrastructure_outcomes"] = 1
        result = payloads(report)
        affected = next(item for item in result if item["task_id"] == row["task_id"] and item["mode"] == row["arm"] and item["seed"] == 1)
        self.assertFalse(affected["success"])
        self.assertTrue(affected["metrics"]["runtime_error"])
        self.assertFalse(affected["metrics"]["usage_may_be_partial"])
        self.assertEqual(affected["metrics"]["tool_runtime_evidence"], row["tool_runtime_evidence"])
        for field, value in (("shell_spawn_failure_count", 0), ("shell_spawn_failure_count", 4),
                             ("shell_tool_calls", 6), ("trace_sha256", "invalid"), ("reason", "unknown")):
            invalid = deepcopy(report)
            invalid["records"][0]["tool_runtime_evidence"][field] = value
            with self.subTest(field=field, value=value), self.assertRaises(importer.ValidationError):
                payloads(invalid)
        for mutation in ("remove_evidence", "mark_pass", "remove_reason"):
            invalid = deepcopy(report)
            target = invalid["records"][0]
            if mutation == "remove_evidence":
                del target["tool_runtime_evidence"]
            elif mutation == "mark_pass":
                target["grade"]["status"] = "pass"
            else:
                target["validity_issues"] = []
            with self.subTest(mutation=mutation), self.assertRaises(importer.ValidationError):
                payloads(invalid)

    def test_v2_requires_and_preserves_its_own_protocol_and_grader_controls(self):
        report = complete_report()
        report["manifest"].update(suite="real-repo-v2", **{field: "2" * 64 for field in importer.V2_HASHES})
        for row in report["records"]:
            row["grader_manifest"] = {f"grading/{row['task_id']}/{name}": "3" * 64
                                      for name in importer.GRADER_FILES_V2[row["task_id"]]}
        before = payloads(report, "followup")
        self.assertEqual(len(before), 24)
        changed = deepcopy(report)
        changed["manifest"]["controls_sha256"] = "4" * 64
        self.assertNotEqual(before[0]["config_hash"], payloads(changed, "followup")[0]["config_hash"])
        for missing in importer.V2_HASHES:
            invalid = deepcopy(report)
            del invalid["manifest"][missing]
            with self.subTest(missing=missing), self.assertRaises(importer.ValidationError):
                payloads(invalid, "followup")
        invalid = deepcopy(report)
        del invalid["records"][0]["grader_manifest"]["grading/r01_guide_hot_path/preflight.py"]
        with self.assertRaises(importer.ValidationError):
            payloads(invalid, "followup")

    def test_full_matrix_native_shape_and_stable_controls(self):
        rows = payloads()
        self.assertEqual(len(rows), 24)
        self.assertEqual(len({row["run_id"] for row in rows}), 24)
        for row in rows:
            self.assertEqual(uuid.UUID(row["run_id"].removeprefix("eval_")).version, 5)
            self.assertEqual(row["benchmark_id"], "pilot-2026/" + row["task_id"])
            self.assertEqual(row["seed"], row["metrics"]["repetition"])
            self.assertEqual(row["metrics"]["seed_semantics"], "repetition_index_not_rng_seed")
            self.assertEqual(row["wall_ms"], 10125)
            self.assertEqual(row["metrics"]["reported"]["total_cost_usd"], .15)
            self.assertEqual(row["metrics"]["reported"]["processed_input_tokens"], 80)
            self.assertEqual(row["brain_snapshot_hash"], "d" * 64)
        first_off = next(row for row in rows if row["task_id"] == importer.TASKS[0] and row["seed"] == 1 and row["mode"] == "off")
        self.assertEqual(first_off["created_at_ms"], 1790308800123)
        for task in importer.TASKS:
            controlled = [row for row in rows if row["task_id"] == task]
            self.assertEqual(len({row["config_hash"] for row in controlled}), 1)

    def test_matrix_and_completion_fail_closed(self):
        for mutation in (
            lambda report: report["records"].pop(),
            lambda report: report["records"].append(deepcopy(report["records"][0])),
            lambda report: report["records"].__setitem__(1, deepcopy(report["records"][0])),
            lambda report: report["records"][0].update(repetition=True),
            lambda report: report["records"][0].update(arm="unknown"),
            lambda report: report["progress"].update(status="in_progress"),
            lambda report: report["progress"].update(in_progress_trials=1),
            lambda report: report["manifest"].update(repetitions=4),
        ):
            report = complete_report()
            mutation(report)
            with self.assertRaises(importer.ValidationError):
                payloads(report)

    def test_hashes_and_frozen_task_controls_are_required(self):
        for mutation in (
            lambda report: report["records"][0].pop("grader_manifest"),
            lambda report: report["records"][0].update(prompt_sha256="missing"),
            lambda report: report["records"][0].update(composed_prompt_sha256="2" * 64),
            lambda report: report["records"][0].update(snapshot_sha256="2" * 64),
            lambda report: report["manifest"].pop("runner_sha256"),
            lambda report: report["fixtures"][0].pop("base_commit"),
        ):
            report = complete_report()
            mutation(report)
            with self.assertRaises(importer.ValidationError):
                payloads(report)

    def test_reimport_ignores_report_timestamp_and_record_order(self):
        report = complete_report()
        old = payloads(report)
        report["generated_at"] = "2029-01-01T00:00:00Z"
        report["records"].reverse()
        report["analysis_provenance"] = {"reporter.py": "new-formatting-code"}
        self.assertEqual(old, payloads(report))

    def test_changed_controls_partition_comparisons_but_ids_remain_conflict_safe(self):
        before = payloads()
        for mutation in (
            lambda report: report["manifest"].update(effort="medium"),
            lambda report: report["manifest"].update(python="next-version"),
            lambda report: report["manifest"].update(runner_sha256="2" * 64),
            lambda report: report["records"][0].update(started_at="2026-09-25T05:00:00+00:00"),
            lambda report: [row.update(prompt_sha256="3" * 64) for row in report["records"]],
        ):
            report = complete_report()
            mutation(report)
            after = payloads(report)
            self.assertEqual([row["run_id"] for row in before], [row["run_id"] for row in after])
            self.assertNotEqual(before[0]["config_hash"], after[0]["config_hash"])
        self.assertNotEqual(before[0]["run_id"], payloads(label="new-independent-pilot")[0]["run_id"])

    def test_privacy_whitelist_does_not_copy_nested_private_data(self):
        report = complete_report()
        report["secret"] = "PRIVATE-SENTINEL"
        report["manifest"]["credentials"] = "PRIVATE-SENTINEL"
        report["infrastructure_attempts"].append({"trace": "PRIVATE-SENTINEL"})
        for row in report["records"]:
            row.update(prompt="PRIVATE-SENTINEL", source="PRIVATE-SENTINEL", error="PRIVATE-SENTINEL", session_id="PRIVATE-SENTINEL")
            row["metrics"].update(tool_inputs="PRIVATE-SENTINEL", tool_use_ids=["PRIVATE-SENTINEL"])
            row["grade"]["checks"][0].update(name="PRIVATE-SENTINEL", error="PRIVATE-SENTINEL")
            row["metrics"]["model_usage"]["claude-pinned[1m]"]["error"] = "PRIVATE-SENTINEL"
        serialized = json.dumps(payloads(report))
        self.assertNotIn("PRIVATE-SENTINEL", serialized)
        self.assertNotIn("tool_use_ids", serialized)

    def test_runtime_error_behavioral_pass_and_functional_failures_are_preserved(self):
        report = complete_report()
        runtime, failed = report["records"][:2]
        runtime.update(grade={"status": "infra"}, behavioral_grade={"status": "pass"},
                       validity_issues=["agent_runtime_error"], usage_may_be_partial=True, agent_exit_code=1)
        runtime["metrics"]["is_error"] = True
        failed.update(grade={"status": "fail"}, grader_exit_code=1)
        report["progress"]["infrastructure_outcomes"] = 1
        rows = payloads(report)
        by_arm = {row["mode"]: row for row in rows if row["task_id"] == importer.TASKS[0] and row["seed"] == 1}
        self.assertEqual(by_arm["off"]["metrics"]["outcome"], "infrastructure_error")
        self.assertEqual(by_arm["off"]["metrics"]["behavioral_grade"], {"status": "pass"})
        self.assertEqual(by_arm["off"]["metrics"]["reported"]["total_tokens"], 100)
        self.assertEqual(by_arm["brain"]["metrics"]["outcome"], "fail")
        self.assertFalse(by_arm["off"]["success"])
        self.assertFalse(by_arm["brain"]["success"])
        runtime["grade"]["status"] = "pass"
        with self.assertRaises(importer.ValidationError):
            payloads(report)

    def test_budget_failure_and_infrastructure_precedence(self):
        for infrastructure in (False, True):
            report = complete_report()
            row = report["records"][0]
            row.update(grade={"status": "infra" if infrastructure else "fail"}, behavioral_grade={"status": "pass"},
                       agent_timed_out=True, agent_exit_code=1, usage_may_be_partial=True,
                       validity_issues=["agent_budget_exhausted"] + (["no_delivery_receipt"] if infrastructure else []))
            row["metrics"].update(is_error=True, result_subtype="error_max_turns")
            report["progress"]["infrastructure_outcomes"] = int(infrastructure)
            result = next(value for value in payloads(report) if value["mode"] == "off" and value["seed"] == 1 and value["task_id"] == importer.TASKS[0])
            self.assertEqual(result["metrics"]["outcome"], "infrastructure_error" if infrastructure else "fail")
            self.assertTrue(result["metrics"]["budget_exhausted"])

    def test_actual_reported_model_drift_is_not_silently_replaced(self):
        report = complete_report()
        row = report["records"][0]
        row["metrics"].update(models=["other-model"], model_usage={"other-model": {}})
        with self.assertRaises(importer.ValidationError):
            payloads(report)
        row.update(grade={"status": "infra"}, behavioral_grade={"status": "pass"}, validity_issues=["model_drift"])
        report["progress"]["infrastructure_outcomes"] = 1
        result = next(value for value in payloads(report) if value["mode"] == "off" and value["seed"] == 1 and value["task_id"] == importer.TASKS[0])
        self.assertEqual(result["metrics"]["reported_models"], ["other-model"])
        self.assertEqual(result["metrics"]["outcome"], "infrastructure_error")

    def test_clean_pass_requires_model_and_brain_delivery_evidence(self):
        for missing in ("model", "delivery", "zero_delivery"):
            report = complete_report()
            row = report["records"][1]
            if missing == "model":
                row["metrics"].update(models=[], model_usage={})
            else:
                row["delivery"]["emission_receipts"] = 0 if missing == "zero_delivery" else None
            with self.assertRaises(importer.ValidationError):
                payloads(report)
            row.update(grade={"status": "infra"}, behavioral_grade={"status": "pass"},
                       validity_issues=["no_delivery_receipt"] if missing == "zero_delivery" else [])
            report["progress"]["infrastructure_outcomes"] = 1
            self.assertEqual(len(payloads(report)), 24)

    def test_brain_delivery_evidence_must_be_numeric_and_zero_invalidates_failures(self):
        for value in (True, -1, 1.5, "1"):
            report = complete_report()
            report["records"][1]["delivery"]["emission_receipts"] = value
            with self.assertRaises(importer.ValidationError):
                payloads(report)
        report = complete_report()
        row = report["records"][1]
        row.update(grade={"status": "fail"}, grader_exit_code=1)
        row["delivery"]["emission_receipts"] = 0
        with self.assertRaises(importer.ValidationError):
            payloads(report)

    def test_setup_exit_code_is_preserved_and_nonzero_requires_infrastructure(self):
        for value in (None, 0, 7, -9):
            report = complete_report()
            row = report["records"][0]
            row["setup_exit_code"] = value
            if value not in (None, 0):
                for status in ("pass", "fail"):
                    row["grade"]["status"] = status
                    with self.assertRaises(importer.ValidationError):
                        payloads(report)
                row.update(grade={"status": "infra"}, behavioral_grade={"status": "pass"})
                report["progress"]["infrastructure_outcomes"] = 1
            imported = next(value for value in payloads(report) if value["mode"] == "off" and value["seed"] == 1 and value["task_id"] == importer.TASKS[0])
            self.assertEqual(imported["setup_exit_code"], value)
            if value not in (None, 0):
                self.assertEqual(imported["metrics"]["outcome"], "infrastructure_error")
                self.assertFalse(imported["success"])
        for invalid in (True, "1", 1.0):
            report = complete_report()
            report["records"][0]["setup_exit_code"] = invalid
            with self.assertRaises(importer.ValidationError):
                payloads(report)

    def test_missing_optional_measurements_stay_null_and_wall_is_mandatory(self):
        report = complete_report()
        raw = report["records"][0]["metrics"]
        for field in (*importer.COUNT_FIELDS, "cost_usd_estimate"):
            raw.pop(field)
        raw.update(trace_complete=False, usage_complete=False, tool_calls_complete=False)
        measured = next(row for row in payloads(report) if row["mode"] == "off" and row["seed"] == 1 and row["task_id"] == importer.TASKS[0])["metrics"]["reported"]
        self.assertTrue(all(value is None for name, value in measured.items() if name != "agent_wall_seconds"))
        for value in (None, True, "10", -1, float("nan"), float("inf")):
            report["records"][0]["agent_wall_seconds"] = value
            with self.assertRaises(importer.ValidationError):
                payloads(report)

    def test_optional_aggregate_tokens_are_independently_nullable(self):
        for missing in (("total_tokens",), ("processed_input_tokens",), ("total_tokens", "processed_input_tokens")):
            for remove in (False, True):
                report = complete_report()
                raw = report["records"][0]["metrics"]
                for field in missing:
                    if remove:
                        raw.pop(field)
                    else:
                        raw[field] = None
                measured = next(row for row in payloads(report) if row["mode"] == "off" and row["seed"] == 1 and row["task_id"] == importer.TASKS[0])["metrics"]["reported"]
                for field in missing:
                    self.assertIsNone(measured[field])
                self.assertEqual(measured["input_tokens"], 10)
                self.assertEqual(measured["output_tokens"], 20)
                self.assertEqual(measured["cache_creation_input_tokens"], 30)
                self.assertEqual(measured["cache_read_input_tokens"], 40)

    def test_partial_components_do_not_erase_totals_and_known_sums_are_checked(self):
        report = complete_report()
        raw = report["records"][0]["metrics"]
        raw["cache_read_input_tokens"] = None
        self.assertEqual(importer.measured_metrics(report["records"][0])["total_tokens"], 100)
        raw["total_tokens"] = 99
        with self.assertRaises(importer.ValidationError):
            importer.measured_metrics(report["records"][0])
        raw["processed_input_tokens"] = None
        self.assertEqual(importer.measured_metrics(report["records"][0])["total_tokens"], 99)
        raw["cache_read_input_tokens"] = 40
        with self.assertRaises(importer.ValidationError):
            importer.measured_metrics(report["records"][0])
        raw["total_tokens"] = None
        raw["processed_input_tokens"] = 79
        with self.assertRaises(importer.ValidationError):
            importer.measured_metrics(report["records"][0])

    def test_ambiguous_contract_is_unscorable_and_partial_tool_counts_are_kept(self):
        report = complete_report()
        row = report["records"][0]
        row.update(grade={"status": "infra"}, behavioral_grade={"status": "fail"},
                   grader_exit_code=1, validity_issues=["ambiguous_grader_contract"],
                   adjudication={"schema_version": 1, "trial_id": f"{row['task_id']}-r1-off", "reason": "ambiguous_grader_contract",
                                 "grade_sha256": "3" * 64, "failed_check_sha256": "4" * 64, "error": "PRIVATE-SENTINEL"})
        row["metrics"]["tool_calls_complete"] = False
        report["progress"]["infrastructure_outcomes"] = 1
        result = next(value for value in payloads(report) if value["mode"] == "off" and value["seed"] == 1 and value["task_id"] == importer.TASKS[0])
        self.assertEqual(result["metrics"]["outcome"], "infrastructure_error")
        self.assertEqual(result["metrics"]["behavioral_grade"], {"status": "fail"})
        self.assertEqual(result["metrics"]["validity_issues"], ["ambiguous_grader_contract"])
        self.assertEqual(result["metrics"]["reported"]["tool_calls"], 5)
        self.assertFalse(result["metrics"]["tool_calls_complete"])
        self.assertEqual(result["metrics"]["adjudication"]["grade_sha256"], "3" * 64)
        self.assertNotIn("PRIVATE-SENTINEL", json.dumps(result))
        row["adjudication"]["trial_id"] = f"{row['task_id']}-r2-off"
        with self.assertRaises(importer.ValidationError):
            payloads(report)
        row["adjudication"]["trial_id"] = f"{row['task_id']}-r1-off"
        row["grade"]["status"] = "fail"
        with self.assertRaises(importer.ValidationError):
            payloads(report)

    def test_invalid_measurements_are_rejected_without_coercion(self):
        for field, value in (("input_tokens", True), ("tool_calls", 1.5), ("output_tokens", "20"),
                             ("total_tokens", 99), ("cache_read_input_tokens", -1), ("cost_usd_estimate", float("inf"))):
            report = complete_report()
            report["records"][0]["metrics"][field] = value
            with self.assertRaises(importer.ValidationError):
                payloads(report)

    def invoke(self, report, *arguments):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "results.json"
            source.write_text(json.dumps(report))
            output, error = io.StringIO(), io.StringIO()
            with redirect_stdout(output), redirect_stderr(error):
                code = importer.main(["--report", str(source), "--benchmark-id", "unit-pilot", *arguments])
            return code, output.getvalue(), error.getvalue()

    def test_dry_run_never_opens_a_socket(self):
        with patch.object(importer, "post_record") as post, patch.object(importer.socket, "socket") as connect:
            code, output, error = self.invoke(complete_report())
        self.assertEqual(code, 0, error)
        self.assertEqual(json.loads(output)["status"], "validated_no_writes")
        post.assert_not_called()
        connect.assert_not_called()

    def test_apply_prevalidates_every_record_before_first_write(self):
        report = complete_report()
        report["records"][-1]["agent_wall_seconds"] = None
        with patch.object(importer, "post_record") as post:
            code, _, error = self.invoke(report, "--apply")
        self.assertEqual(code, 2)
        self.assertIn("before any writes", error)
        post.assert_not_called()

    def test_partial_failure_counts_acknowledgements_and_retry_is_explicit(self):
        with patch.object(importer, "post_record", side_effect=[None, None, importer.TransportError("HTTP 409")]) as post:
            code, output, _ = self.invoke(complete_report(), "--apply", "--socket", "/tmp/never-opened.sock")
        summary = json.loads(output)
        self.assertEqual(code, 1)
        self.assertEqual(post.call_count, 3)
        self.assertEqual(summary["acknowledged_records"], 2)
        self.assertIn("conflicting content is rejected", summary["retry"])

    def test_unix_adapter_sends_only_evaluation_post_and_closes(self):
        connection = MagicMock()
        response = connection.getresponse.return_value
        response.status = 200
        response.read.return_value = b"null"
        payload = payloads()[0]
        with patch.object(importer, "UnixHTTPConnection", return_value=connection):
            importer.post_record("/tmp/test.sock", payload)
        args = connection.request.call_args.args
        self.assertEqual(args[:2], ("POST", "/api/v1/evaluation/runs"))
        self.assertEqual(json.loads(args[2]), payload)
        connection.close.assert_called_once()

    def test_adapter_errors_redact_response_and_transport_details(self):
        for status, body in ((409, b"PRIVATE-SENTINEL"), (200, b"PRIVATE-SENTINEL"), (200, b"x" * 65537)):
            connection = MagicMock()
            response = connection.getresponse.return_value
            response.status, response.read.return_value = status, body
            with patch.object(importer, "UnixHTTPConnection", return_value=connection):
                with self.assertRaises(importer.TransportError) as caught:
                    importer.post_record("/tmp/test.sock", {})
            self.assertNotIn("PRIVATE-SENTINEL", str(caught.exception))
            connection.close.assert_called_once()
        connection = MagicMock()
        connection.request.side_effect = OSError("PRIVATE-SENTINEL")
        with patch.object(importer, "UnixHTTPConnection", return_value=connection):
            with self.assertRaises(importer.TransportError) as caught:
                importer.post_record("/tmp/test.sock", {})
        self.assertNotIn("PRIVATE-SENTINEL", str(caught.exception))
        connection.close.assert_called_once()

    def test_unix_connection_uses_local_socket_and_closes_on_connect_error(self):
        sock = MagicMock()
        with patch.object(importer.socket, "socket", return_value=sock) as factory:
            connection = importer.UnixHTTPConnection("/tmp/test.sock")
            connection.connect()
            factory.assert_called_once_with(importer.socket.AF_UNIX, importer.socket.SOCK_STREAM)
            sock.connect.assert_called_once_with("/tmp/test.sock")
            sock.connect.side_effect = OSError("connection refused")
            with self.assertRaises(OSError):
                connection.connect()
        sock.close.assert_called_once()
        self.assertIsNone(connection.sock)

    def test_json_duplicate_keys_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "results.json"
            path.write_text('{"records": [], "records": []}')
            with self.assertRaises(importer.ValidationError):
                importer.load_report(path)


if __name__ == "__main__":
    unittest.main()
