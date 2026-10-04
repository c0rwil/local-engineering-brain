"""Workflow diagnostics preserve uncertainty and never emit trace payloads."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
SCRIPT = Path(__file__).parents[1] / "scripts/real_repo_diagnostics.py"
spec = importlib.util.spec_from_file_location("real_repo_diagnostics", SCRIPT)
diagnostics = importlib.util.module_from_spec(spec)
spec.loader.exec_module(diagnostics)

T0 = "2026-09-24T12:00:00Z"
T1 = "2026-09-24T12:00:01Z"
T3 = "2026-09-24T12:00:03Z"


def tool(identity, name="Read", **inputs):
    return {"type": "tool_use", "id": identity, "name": name, "input": inputs}


def assistant(*blocks, timestamp=None):
    event = {"type": "assistant", "message": {"content": list(blocks)}}
    if timestamp is not None:
        event["timestamp"] = timestamp
    return event


def response(identity, **extra):
    return {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": identity, **extra}]}}


def result(**extra):
    return {"type": "result", "subtype": "success", "is_error": False, **extra}


def write_trace(path, events):
    path.write_text("\n".join(event if isinstance(event, str) else json.dumps(event) for event in events), encoding="utf-8")


class DiagnosticsTest(unittest.TestCase):
    def parse(self, *events, **options):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.jsonl"
            write_trace(path, events)
            return diagnostics.parse_trace(path, **options)

    def test_deduplicates_stream_and_full_requests_and_results(self):
        parsed = self.parse(
            {"type": "system", "subtype": "init", "cwd": "/fixture"},
            assistant(tool("a", "Read"), tool("b", "Grep"), tool("c", "Bash"), timestamp=T1),
            {"type": "stream_event", "timestamp": T3, "event": {"type": "content_block_start", "content_block": tool("d", "Edit")}},
            assistant(tool("d", "Edit", file_path="src/./a.py"), timestamp=T3),
            assistant(tool("d", "Edit", file_path="src/a.py"), tool("e", "Glob"), timestamp=T3),
            response("a"), response("b"), response("c"), response("d"), response("d", is_error=False), response("e"), result(),
        )
        self.assertEqual(parsed["tool_calls"], 5)
        self.assertEqual(parsed["tool_calls_by_name"], {"Bash": 1, "Edit": 1, "Glob": 1, "Grep": 1, "Read": 1})
        self.assertEqual(parsed["search_read_calls_total"], 3)
        self.assertEqual(parsed["search_read_calls_before_first_edit"], 2)
        self.assertEqual(parsed["first_edit"], {"observed": True, "tool_ordinal": 4, "elapsed_seconds": None, "elapsed_basis": None,
                                               "since_first_timestamped_event_seconds": 2})
        self.assertEqual(parsed["tool_results_observed"], 5)
        self.assertEqual(parsed["edited_unique_file_count"], 1)
        for key in ("tool_calls_complete", "tool_errors_complete", "edited_file_count_complete"):
            self.assertTrue(parsed[key], key)

    def test_failed_first_edit_is_request_but_not_confirmed_file(self):
        parsed = self.parse(
            assistant(tool("a", "Edit", file_path="/fixture/failed.py"), tool("b", "Read")),
            response("a", is_error=True), response("a", is_error=True), response("b"), result(),
        )
        self.assertEqual(parsed["first_edit"]["tool_ordinal"], 1)
        self.assertEqual(parsed["search_read_calls_before_first_edit"], 0)
        self.assertEqual(parsed["edited_unique_file_count"], 0)
        self.assertEqual(parsed["tool_error_responses"], 1)
        self.assertTrue(parsed["edited_file_count_complete"])

    def test_unique_paths_across_edit_tools_are_lexical_and_confirmed(self):
        parsed = self.parse(
            {"type": "system", "subtype": "init", "cwd": "/fixture"},
            assistant(tool("a", "Edit", file_path="src/a.py"), tool("b", "Write", file_path="/fixture/src/../src/a.py"),
                      tool("c", "MultiEdit", edits=[{"file_path": "src/b.py"}, {"file_path": "src/a.py"}]),
                      tool("d", "NotebookEdit", notebook_path="/fixture/n.ipynb")),
            response("a"), response("b"), response("c"), response("d"), result(),
        )
        self.assertEqual(parsed["edited_unique_file_count"], 3)
        self.assertTrue(parsed["edited_file_count_complete"])

    def test_bash_payloads_do_not_infer_searches_edits_or_error_flags(self):
        parsed = self.parse(
            assistant(tool("a", "Bash", command="rg foo source; sed -i backup source")),
            response("a", content="Error: exit status 1; changed source"), result(),
        )
        self.assertEqual(parsed["search_read_calls_total"], 0)
        self.assertIsNone(parsed["search_read_calls_before_first_edit"])
        self.assertFalse(parsed["first_edit"]["observed"])
        self.assertEqual(parsed["edited_unique_file_count"], 0)
        self.assertEqual(parsed["tool_error_responses"], 0)

    def test_explicit_dispatch_and_timestamped_init(self):
        for events, options, basis in (
            ([assistant(tool("a", "Edit", file_path="/a"), timestamp=T3)], {"agent_started_at": T0}, "agent_started_at"),
            ([{"type": "system", "subtype": "init", "timestamp": T0}, assistant(tool("a", "Edit", file_path="/a"), timestamp=T3)], {}, "timestamped_system_init"),
        ):
            with self.subTest(basis=basis):
                parsed = self.parse(*events, response("a"), result(), **options)
                self.assertEqual(parsed["first_edit"]["elapsed_seconds"], 3)
                self.assertEqual(parsed["first_edit"]["elapsed_basis"], basis)

    def test_missing_timestamps_do_not_become_zero_elapsed(self):
        parsed = self.parse(assistant(tool("a", "Edit", file_path="/a")), response("a"), result(), agent_started_at=T0)
        self.assertIsNone(parsed["first_edit"]["elapsed_seconds"])
        self.assertIsNone(parsed["first_edit"]["since_first_timestamped_event_seconds"])

    def test_unreliable_timestamps_disable_elapsed(self):
        fixtures = (
            (assistant(tool("a", "Read"), timestamp=T3), assistant(tool("b", "Edit", file_path="/a"), timestamp=T1)),
            (assistant(tool("a", "Read"), timestamp="2026-09-24T12:00:00"), assistant(tool("b", "Edit", file_path="/a"), timestamp=T3)),
            (assistant(tool("a", "Read"), timestamp=123), assistant(tool("b", "Edit", file_path="/a"), timestamp=T3)),
        )
        for events in fixtures:
            with self.subTest(events=events):
                parsed = self.parse(*events, result(), agent_started_at=T0)
                self.assertIsNone(parsed["first_edit"]["elapsed_seconds"])
                self.assertIsNone(parsed["first_edit"]["since_first_timestamped_event_seconds"])

    def test_invalid_explicit_dispatch_does_not_fall_back_to_init(self):
        parsed = self.parse({"type": "system", "subtype": "init", "timestamp": T0},
                            assistant(tool("a", "Edit", file_path="/a"), timestamp=T3), result(), agent_started_at="bad")
        self.assertIsNone(parsed["first_edit"]["elapsed_seconds"])
        self.assertTrue(parsed["coverage"]["invalid_supplied_dispatch_timestamp"])
        self.assertEqual(parsed["first_edit"]["since_first_timestamped_event_seconds"], 3)

    def test_dispatch_after_edit_does_not_report_negative_time(self):
        parsed = self.parse(assistant(tool("a", "Edit", file_path="/a"), timestamp=T1), result(), agent_started_at=T3)
        self.assertIsNone(parsed["first_edit"]["elapsed_seconds"])

    def test_incomplete_and_corrupt_trace_preserve_observed_counts(self):
        parsed = self.parse("not-json", "[]", assistant(tool("a"), {"type": "tool_use", "name": "Read"}, tool("b", "Edit", file_path="/a")))
        self.assertEqual(parsed["tool_calls"], 2)
        self.assertFalse(parsed["tool_calls_complete"])
        self.assertFalse(parsed["tool_errors_complete"])
        self.assertFalse(parsed["edited_file_count_complete"])
        self.assertIsNone(parsed["first_edit"]["tool_ordinal"])
        self.assertIsNone(parsed["search_read_calls_before_first_edit"])
        self.assertEqual(parsed["coverage"]["malformed_lines"], 2)
        self.assertEqual(parsed["coverage"]["tool_blocks_without_ids"], 1)
        self.assertFalse(parsed["final_result"]["present"])

    def test_duplicate_identity_conflicts_do_not_confirm_edit(self):
        parsed = self.parse(assistant(tool("a", "Edit", file_path="/a")), assistant(tool("a", "Edit", file_path="/b")),
                            response("a"), result())
        self.assertEqual(parsed["edited_unique_file_count"], 0)
        self.assertFalse(parsed["edited_file_count_complete"])
        self.assertEqual(parsed["coverage"]["edit_path_conflicts"], 1)
        renamed = self.parse(assistant(tool("a", "Read")), assistant(tool("a", "Edit", file_path="/a")), response("a"), result())
        self.assertEqual(renamed["coverage"]["tool_name_conflicts"], 1)
        self.assertFalse(renamed["tool_calls_complete"])

    def test_results_missing_conflicting_or_unmatched_mark_coverage(self):
        parsed = self.parse(
            assistant(tool("a", "Edit", file_path="/a"), tool("b", "Write", file_path="/b"), tool("c", "Edit")),
            response("a"), response("a", is_error=True), response("b", is_error="false"), response("orphan"),
            response(None, is_error=True), result(),
        )
        self.assertEqual(parsed["edited_unique_file_count"], 0)
        self.assertEqual(parsed["tool_error_responses"], 1)
        self.assertEqual(parsed["coverage"]["unidentified_tool_error_responses"], 1)
        self.assertEqual(parsed["coverage"]["conflicting_tool_results"], 1)
        self.assertEqual(parsed["coverage"]["unknown_tool_result_statuses"], 1)
        self.assertEqual(parsed["coverage"]["unmatched_tool_result_ids"], 1)
        self.assertEqual(parsed["coverage"]["edit_requests_without_result"], 1)
        self.assertEqual(parsed["coverage"]["edit_requests_without_path"], 1)
        for key in ("tool_calls_complete", "tool_errors_complete", "edited_file_count_complete"):
            self.assertFalse(parsed[key], key)

    def test_relative_path_without_cwd_is_counted_with_incomplete_coverage(self):
        parsed = self.parse(assistant(tool("a", "Edit", file_path="src/a.py")), response("a"), result())
        self.assertEqual(parsed["edited_unique_file_count"], 1)
        self.assertFalse(parsed["edited_file_count_complete"])
        self.assertEqual(parsed["coverage"]["relative_edit_paths_without_cwd"], 1)

    def test_provider_errors_do_not_count_as_tool_errors(self):
        parsed = self.parse(result(is_error=True, terminal_reason="api_error", api_error_status=None))
        self.assertTrue(parsed["final_result"]["provider_error"])
        self.assertEqual(parsed["final_result"]["classification"], "provider_error")
        self.assertEqual(parsed["tool_error_responses"], 0)
        self.assertEqual(self.parse(result(api_error_status=429))["final_result"]["classification"], "provider_error")

    def test_final_error_classification_uses_last_result_only(self):
        cases = (
            ({"subtype": "error_max_turns", "is_error": True}, "agent_limit", False),
            ({"subtype": "error_max_budget_usd", "is_error": True}, "agent_limit", False),
            ({"subtype": "unrecognized", "is_error": True}, "other_error", None),
            ({"subtype": []}, "unknown", None),
            ({}, "successful_result", False),
        )
        for fields, expected, provider in cases:
            with self.subTest(expected=expected):
                final = self.parse(result(is_error=True, terminal_reason="api_error"), result(**fields))["final_result"]
                self.assertEqual(final["classification"], expected)
                self.assertIs(final["provider_error"], provider)
                self.assertEqual(final["result_count"], 2)

    def test_only_approved_metadata_and_counts_leave_parser(self):
        sentinel = "PRIVATE_PAYLOAD_SENTINEL"
        parsed = self.parse(
            {"type": "system", "subtype": "init", "cwd": "/" + sentinel},
            assistant({"type": "thinking", "thinking": sentinel}, {"type": "text", "text": sentinel},
                      tool(sentinel, "Edit", file_path=sentinel, old_string=sentinel, new_string=sentinel)),
            response(sentinel, content=sentinel), result(errors=[sentinel], result=sentinel),
        )
        self.assertNotIn(sentinel, json.dumps(parsed))
        self.assertEqual(parsed["edited_unique_file_count"], 1)


class SummaryAndCliTest(unittest.TestCase):
    def test_diagnose_ignores_setup_start_and_other_record_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.jsonl"
            write_trace(path, [assistant(tool("a", "Edit", file_path="/a"), timestamp=T3), response("a"), result()])
            row = diagnostics.diagnose_trial({"id": "t-r1-off", "task_id": "t", "arm": "off", "repetition": 1,
                                             "started_at": T0, "prompt": "PRIVATE_PAYLOAD", "metrics": {"total_tokens": 9}, "grade": {"status": "pass"}}, path)
            self.assertIsNone(row["workflow"]["first_edit"]["elapsed_seconds"])
            self.assertEqual(set(row), {"trial_id", "task_id", "arm", "repetition", "workflow"})
            self.assertNotIn("PRIVATE_PAYLOAD", json.dumps(row))

    def test_summary_keeps_missing_traces_in_distribution_denominators(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.jsonl"
            write_trace(path, [assistant(tool("a", "Read"), tool("b", "Edit", file_path="/a")), response("a"), response("b"), result()])
            row = diagnostics.diagnose_trial({"id": "t-r1-off", "task_id": "t", "arm": "off", "repetition": 1}, path)
            summary = diagnostics.summarize_trials([row, {"task_id": "t", "arm": "off", "workflow": None},
                                                   {"task_id": "u", "arm": "brain", "workflow": None}])
            off = summary["by_arm"]["off"]
            self.assertEqual(off["trials"], 2)
            self.assertEqual(off["traces_available"], 1)
            self.assertEqual(off["tool_calls_observed"], 2)
            self.assertEqual(off["edited_unique_files_per_trial_sum"], 1)
            self.assertEqual(off["distributions"]["first_edit_tool_ordinal"], {"available": 1, "missing": 1, "mean": 2, "median": 2})
            self.assertEqual(off["distributions"]["first_edit_elapsed_seconds"]["missing"], 2)
            self.assertEqual([entry["task_id"] for entry in summary["by_task"]], ["t", "u"])

    def cli(self, directory, *extra):
        return subprocess.run([sys.executable, str(SCRIPT), "--data", str(directory), *map(str, extra)], text=True, capture_output=True, check=False)

    def test_cli_reads_completed_records_only_and_does_not_modify_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            data = Path(directory)
            (data / "records.jsonl").write_text(json.dumps({"id": "t-r1-off", "task_id": "t", "arm": "off"}) + "\n")
            trial = data / "trials" / "t-r1-off"
            trial.mkdir(parents=True)
            write_trace(trial / "trace.jsonl", [assistant(tool("a", "Read", file_path="PRIVATE_PAYLOAD")), response("a"), result()])
            (data / "infrastructure_attempts.jsonl").write_text("PRIVATE_PAYLOAD\n")
            unfinished = data / "trials" / "t-r1-brain"
            unfinished.mkdir()
            (unfinished / "trace.jsonl").write_text("unfinished")
            before = {path: path.read_bytes() for path in data.rglob("*") if path.is_file()}
            derived = data / "derived.json"
            process = self.cli(data, "--output", derived)
            self.assertEqual(process.returncode, 0, process.stderr)
            report = json.loads(derived.read_text())
            self.assertEqual(len(report["trials"]), 1)
            self.assertNotIn("PRIVATE_PAYLOAD", derived.read_text())
            for path, content in before.items():
                self.assertEqual(path.read_bytes(), content)
            refusal = self.cli(data, "--output", data / "records.jsonl")
            self.assertEqual(refusal.returncode, 2)
            self.assertEqual((data / "records.jsonl").read_bytes(), before[data / "records.jsonl"])

    def test_cli_missing_trace_reports_coverage_and_invalid_identity_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            data = Path(directory)
            records = data / "records.jsonl"
            records.write_text(json.dumps({"id": "missing", "task_id": "t", "arm": "off"}))
            process = self.cli(data)
            self.assertEqual(process.returncode, 0, process.stderr)
            self.assertTrue(json.loads(process.stdout)["trials"][0]["trace_unavailable"])
            records.write_text(json.dumps({"id": "../../PRIVATE_PAYLOAD"}))
            process = self.cli(data)
            self.assertEqual(process.returncode, 2)
            self.assertNotIn("PRIVATE_PAYLOAD", process.stderr)
            self.assertEqual(process.stdout, "")


if __name__ == "__main__":
    unittest.main()
