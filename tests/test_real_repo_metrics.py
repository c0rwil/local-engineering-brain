"""Telemetry must preserve unknowns and keep failed runs out of savings claims."""

import importlib.util
from copy import deepcopy
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("real_repo_metrics", Path(__file__).parents[1] / "scripts/real_repo_metrics.py")
metrics = importlib.util.module_from_spec(spec)
spec.loader.exec_module(metrics)


def result(**overrides):
    event = {
        "type": "result", "subtype": "success", "is_error": False,
        "num_turns": 4, "total_cost_usd": 0.125,
        "usage": {"input_tokens": 100, "output_tokens": 20, "cache_creation_input_tokens": 40, "cache_read_input_tokens": 60},
        "modelUsage": {"model-a": {"inputTokens": 100, "costUSD": 0.125}},
        "permission_denials": [],
    }
    event.update(overrides)
    return event


def tool(tool_id, name="Bash"):
    return {"type": "tool_use", "id": tool_id, "name": name, "input": {}}


def assistant(*blocks, **extra):
    return {"type": "assistant", "message": {"model": "model-a", "content": list(blocks), **extra}}


SPAWN_ARGUMENT_LIMIT = (
    "Could not start /bin/zsh: the command line plus environment exceed the OS exec argument limit (E2BIG). "
    "At spawn: command line 1.1MB across 3 args (largest single arg 1.1MB); environment 2.4KB across 42 vars. "
    "The Bash sandbox profile adds 262 filesystem deny paths to every command."
)


def user_result(tool_id, content=SPAWN_ARGUMENT_LIMIT, is_error=True):
    return {"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": tool_id, "content": content, "is_error": is_error},
    ]}}


class TraceMetricsTest(unittest.TestCase):
    def parse(self, *events):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace.jsonl"
            path.write_text("\n".join(event if isinstance(event, str) else json.dumps(event) for event in events))
            return metrics.parse_trace(path)

    def test_cache_tokens_and_unique_tool_ids(self):
        parsed = self.parse(
            {"type": "stream_event", "event": {"type": "content_block_start", "content_block": tool("one")}},
            assistant(tool("one"), {"type": "text", "text": "Planning"}),
            assistant(tool("one"), tool("two", "Read")),
            assistant(tool("three", "Read")),
            {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "one"}]}},
            result(),
        )
        self.assertEqual(parsed["processed_input_tokens"], 200)
        self.assertEqual(parsed["total_tokens"], 220)
        self.assertEqual(parsed["cache_creation_input_tokens"], 40)
        self.assertEqual(parsed["cache_read_input_tokens"], 60)
        self.assertEqual(parsed["tool_calls"], 3)
        self.assertEqual(parsed["counts_by_tool"], {"Bash": 1, "Read": 2})
        self.assertTrue(parsed["tool_calls_complete"])
        self.assertEqual(parsed["model_usage"], {"model-a": {"inputTokens": 100, "costUSD": 0.125}})
        self.assertEqual(parsed["cost_usd_estimate"], 0.125)
        self.assertEqual(parsed["num_turns"], 4)

    def test_missing_final_does_not_sum_intermediate_usage(self):
        parsed = self.parse(assistant(tool("one"), usage={"input_tokens": 777, "output_tokens": 88}))
        for name in (*metrics.TOKEN_FIELDS, "processed_input_tokens", "total_tokens", "cost_usd_estimate", "num_turns"):
            self.assertIsNone(parsed[name], name)
        self.assertEqual(parsed["tool_calls"], 1)
        self.assertFalse(parsed["tool_calls_complete"])
        self.assertFalse(parsed["trace_complete"])
        self.assertIsNone(parsed["permission_denials"])

    def test_result_without_usage_remains_unknown(self):
        parsed = self.parse(result(usage=None))
        self.assertTrue(parsed["trace_complete"])
        self.assertFalse(parsed["usage_complete"])
        self.assertIsNone(parsed["total_tokens"])
        self.assertIsNone(parsed["input_tokens"])
        self.assertEqual(parsed["cost_usd_estimate"], 0.125)

    def test_partial_usage_does_not_assume_missing_cache_is_free(self):
        parsed = self.parse(result(usage={"input_tokens": 10, "output_tokens": 2}))
        self.assertEqual(parsed["input_tokens"], 10)
        self.assertIsNone(parsed["cache_read_input_tokens"])
        self.assertIsNone(parsed["processed_input_tokens"])
        self.assertIsNone(parsed["total_tokens"])

    def test_error_result_permission_denials_and_zero_are_preserved(self):
        usage = {name: 0 for name in metrics.TOKEN_FIELDS}
        denials = [{"tool_name": "Bash", "tool_use_id": "denied"}]
        parsed = self.parse(result(usage=usage, total_cost_usd=0, subtype="error_max_turns", is_error=True, permission_denials=denials))
        self.assertEqual(parsed["total_tokens"], 0)
        self.assertEqual(parsed["cost_usd_estimate"], 0)
        self.assertEqual(parsed["result_subtype"], "error_max_turns")
        self.assertTrue(parsed["is_error"])
        self.assertEqual(parsed["permission_denials"], denials)

    def test_malformed_or_unidentifiable_tools_mark_counts_incomplete(self):
        parsed = self.parse("a non-JSON log line", assistant(tool("one"), {"type": "tool_use", "name": "Read"}), result())
        self.assertEqual(parsed["tool_calls"], 1)
        self.assertFalse(parsed["tool_calls_complete"])
        self.assertEqual(parsed["diagnostics"]["malformed_line_numbers"], [1])
        self.assertEqual(parsed["diagnostics"]["tool_blocks_without_ids"], 1)
        self.assertEqual(parsed["total_tokens"], 220)

    def test_last_result_is_authoritative_without_double_counting(self):
        parsed = self.parse(result(), result(usage={name: 2 for name in metrics.TOKEN_FIELDS}, total_cost_usd=0.2))
        self.assertEqual(parsed["total_tokens"], 8)
        self.assertEqual(parsed["cost_usd_estimate"], 0.2)
        self.assertEqual(parsed["diagnostics"]["result_count"], 2)


    def test_bash_spawn_failures_bind_result_ids_and_deduplicate(self):
        parsed = self.parse(
            {"type": "stream_event", "event": {"type": "content_block_start", "content_block": tool("one")}},
            assistant(tool("one")), user_result("one"), user_result("one"),
            user_result("two", [{"type": "text", "text": SPAWN_ARGUMENT_LIMIT.replace("/bin/zsh", "/bin/bash")}]),
            assistant(tool("two"), tool("read", "Read")), user_result("read"), result(),
        )
        self.assertEqual(parsed["shell_tool_calls"], 2)
        self.assertEqual(parsed["shell_spawn_failure_count"], 2)
        self.assertEqual(parsed["tool_calls"], 3)
        self.assertTrue(parsed["usage_complete"])
        self.assertTrue(parsed["tool_calls_complete"])

    def test_shell_spawn_signature_ignores_quotes_and_ordinary_errors(self):
        cases = (
            assistant({"type": "text", "text": SPAWN_ARGUMENT_LIMIT}),
            user_result("bash", is_error=False),
            user_result("bash", is_error="true"),
            user_result("unmatched"),
            user_result("bash", "Exit code 1\n" + SPAWN_ARGUMENT_LIMIT),
            user_result("bash", "E2BIG while testing an ordinary command"),
            user_result("bash", SPAWN_ARGUMENT_LIMIT.split(" At spawn:")[0]),
            user_result("bash", SPAWN_ARGUMENT_LIMIT.split(" The Bash sandbox")[0]),
            {**user_result("bash"), "type": "assistant"},
        )
        for event in cases:
            with self.subTest(event=event):
                parsed = self.parse(assistant(tool("bash")), event, result())
                self.assertEqual(parsed["shell_tool_calls"], 1)
                self.assertEqual(parsed["shell_spawn_failure_count"], 0)

    def test_conflicting_tool_id_does_not_attribute_spawn_error(self):
        parsed = self.parse(assistant(tool("shared")), assistant(tool("shared", "Read")), user_result("shared"), result())
        self.assertEqual(parsed["shell_spawn_failure_count"], 0)
        self.assertEqual(parsed["diagnostics"]["conflicting_tool_ids"], ["shared"])
        self.assertFalse(parsed["tool_calls_complete"])


def record(task, repetition, arm, grade="pass", tokens=100, calls=10, wall=20, cost=1):
    return {
        "task_id": task, "repetition": repetition, "arm": arm,
        "grade": {"status": grade},
        "metrics": {"total_tokens": tokens, "tool_calls": calls, "cost_usd_estimate": cost},
        "agent_wall_seconds": wall,
    }


class ToolRuntimeChecksTest(unittest.TestCase):
    def trace(self, directory, *events):
        path = Path(directory) / "trace.jsonl"
        # Hash the raw bytes, including CRLF, rather than reconstructed JSON.
        path.write_bytes(("\r\n".join(json.dumps(event) for event in events) + "\r\n").encode())
        return path

    def test_actual_spawn_failure_excludes_pass_without_marking_full_usage_partial(self):
        with tempfile.TemporaryDirectory() as directory:
            path = self.trace(directory, assistant(tool("bash")), user_result("bash"), result())
            trial = record("a", 1, "off")
            trial.update(id="stable-id", record_classification_revision="agent-completion-v2", agent_exit_code=0)
            trial["grade"]["checks"] = [{"name": "behavior", "passed": True}]
            trial["metrics"] = metrics.parse_trace(path)
            before = deepcopy(trial)
            derived = metrics.apply_tool_runtime_checks(trial, path)
            self.assertEqual(trial, before)
            self.assertEqual(derived["id"], "stable-id")
            self.assertEqual(derived["record_classification_revision"], before["record_classification_revision"])
            self.assertEqual(derived["grade"]["status"], "infrastructure_error")
            self.assertEqual(derived["behavioral_grade"], before["grade"])
            self.assertEqual(derived["metrics"], before["metrics"])
            self.assertNotIn("usage_may_be_partial", derived)
            self.assertEqual(derived["tool_runtime_evidence"], {
                "schema_version": 1, "trace_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                "reason": "shell_spawn_argument_limit", "shell_tool_calls": 1, "shell_spawn_failure_count": 1,
            })
            self.assertEqual(metrics.apply_tool_runtime_checks(derived, path), derived)
            classified = metrics.classify_record(derived)
            self.assertEqual(classified["grade"]["status"], "infrastructure_error")
            self.assertNotIn("usage_may_be_partial", classified)
            report = metrics.paired_summary([derived, record("a", 1, "brain")])
            self.assertEqual(report["pair_counts"]["infrastructure"], 1)
            self.assertEqual(report["pair_counts"]["both_pass"], 0)
            usage = report["all_attempts"]["off"]
            self.assertEqual(usage["totals_observed"]["total_tokens"], 220)
            self.assertEqual(usage["measurement_coverage"]["total_tokens"]["partial"], 0)
            derived["behavioral_grade"]["checks"][0]["passed"] = False
            self.assertEqual(trial, before)

    def test_no_trace_failure_returns_unchanged_copy_despite_captured_counter(self):
        with tempfile.TemporaryDirectory() as directory:
            path = self.trace(directory, assistant(tool("bash")), user_result("bash", "Exit code 1\nE2BIG"), result())
            trial = record("a", 1, "off", grade="fail")
            trial.update(id="original-id", record_classification_revision="agent-completion-v1")
            trial["metrics"]["shell_spawn_failure_count"] = 99
            before = deepcopy(trial)
            derived = metrics.apply_tool_runtime_checks(trial, path)
            self.assertEqual(derived, before)
            self.assertIsNot(derived, trial)
            derived["metrics"]["total_tokens"] = 0
            self.assertEqual(trial, before)

    def test_existing_behavioral_grade_validity_and_partial_flag_are_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            path = self.trace(directory, assistant(tool("bash")), user_result("bash"), result())
            trial = record("a", 1, "brain", grade="infrastructure_error")
            trial["behavioral_grade"] = {"status": "fail", "checks": [{"name": "kept", "passed": False}]}
            trial["validity_issues"] = ["model_drift"]
            trial["usage_may_be_partial"] = True
            before = deepcopy(trial)
            derived = metrics.apply_tool_runtime_checks(trial, path)
            self.assertEqual(trial, before)
            self.assertEqual(derived["behavioral_grade"], before["behavioral_grade"])
            self.assertEqual(derived["validity_issues"], ["model_drift", "shell_spawn_argument_limit"])
            self.assertTrue(derived["usage_may_be_partial"])
            self.assertEqual(metrics.apply_tool_runtime_checks(derived, path), derived)


class PairedMetricsTest(unittest.TestCase):
    def test_api_error_with_success_subtype_is_not_a_clean_completion(self):
        off = record("a", 1, "off", tokens=100, cost=2)
        off["metrics"].update({"result_subtype": "success", "is_error": True})
        off["agent_exit_code"] = 1
        before = deepcopy(off)
        derived = metrics.classify_record(off)
        self.assertEqual(off, before)
        self.assertEqual(derived["grade"]["status"], "infrastructure_error")
        self.assertEqual(derived["behavioral_grade"], before["grade"])
        self.assertEqual(derived["validity_issues"], ["agent_runtime_error"])
        self.assertTrue(derived["usage_may_be_partial"])
        self.assertEqual(metrics.classify_record(derived), derived)
        derived["behavioral_grade"]["status"] = "mutated-copy"
        self.assertEqual(off, before)
        report = metrics.paired_summary([off, record("a", 1, "brain", tokens=80)])
        self.assertEqual(report["pair_counts"]["both_pass"], 0)
        self.assertEqual(report["pair_counts"]["brain_only_pass"], 0)
        self.assertEqual(report["pair_counts"]["infrastructure"], 1)
        self.assertEqual(report["pairs"][0]["behavioral_grades"], {"off": "pass", "brain": "pass"})
        self.assertEqual(report["all_attempts"]["off"]["totals_observed"]["total_tokens"], 100)
        self.assertEqual(report["all_attempts"]["off"]["totals_observed"]["cost_usd_estimate"], 2)
        self.assertEqual(report["all_attempts"]["off"]["reported_usage_may_be_partial_attempts"], 1)

    def test_budget_exhaustion_is_failure_even_when_exit_and_error_are_set(self):
        for timed_out, subtype in ((True, "success"), (False, "error_max_turns"), (False, "error_max_budget_usd")):
            with self.subTest(timed_out=timed_out, subtype=subtype):
                trial = record("a", 1, "off")
                trial.update({"agent_timed_out": timed_out, "agent_exit_code": 1})
                trial["metrics"].update({"is_error": True, "result_subtype": subtype})
                derived = metrics.classify_record(trial)
                self.assertEqual(derived["grade"]["status"], "fail")
                self.assertEqual(derived["validity_issues"], ["agent_budget_exhausted"])
                self.assertEqual(derived["behavioral_grade"]["status"], "pass")
                self.assertEqual(metrics.classify_record(derived), derived)

    def test_existing_validity_and_behavioral_grade_are_preserved(self):
        trial = record("a", 1, "brain", grade="infrastructure_error")
        trial["validity_issues"] = ["model_drift", "no_delivery_receipt"]
        trial["behavioral_grade"] = {"status": "pass", "checks": [{"name": "kept"}]}
        trial["agent_exit_code"] = -15
        derived = metrics.classify_record(trial)
        self.assertEqual(derived["validity_issues"], ["model_drift", "no_delivery_receipt", "agent_runtime_error"])
        self.assertEqual(derived["behavioral_grade"], trial["behavioral_grade"])
        self.assertEqual(metrics.classify_record(derived), derived)

    def test_budget_exhaustion_cannot_override_existing_infrastructure(self):
        for reason in ("model_drift", "no_delivery_receipt", "grader_infrastructure"):
            for timed_out, subtype in ((True, "success"), (False, "error_max_turns"), (False, "error_max_budget_usd")):
                with self.subTest(reason=reason, timed_out=timed_out, subtype=subtype):
                    trial = record("a", 1, "off", grade="infrastructure_error")
                    trial["behavioral_grade"] = {"status": "pass" if reason != "grader_infrastructure" else "infra_error"}
                    trial["validity_issues"] = [] if reason == "grader_infrastructure" else [reason]
                    trial.update({"agent_timed_out": timed_out, "agent_exit_code": 1})
                    trial["metrics"].update({"is_error": True, "result_subtype": subtype})
                    before = deepcopy(trial)
                    derived = metrics.classify_record(trial)
                    self.assertEqual(trial, before)
                    self.assertEqual(derived["grade"]["status"], "infrastructure_error")
                    self.assertIn("agent_budget_exhausted", derived["validity_issues"])
                    if reason != "grader_infrastructure":
                        self.assertIn(reason, derived["validity_issues"])
                    self.assertEqual(derived["behavioral_grade"], before["behavioral_grade"])
                    self.assertEqual(metrics.classify_record(derived), derived)
                    summary = metrics.paired_summary([trial, record("a", 1, "brain")])
                    self.assertEqual(summary["pair_counts"]["infrastructure"], 1)
                    self.assertEqual(summary["pair_counts"]["off_only_pass"], 0)
                    self.assertEqual(summary["pair_counts"]["brain_only_pass"], 0)
                    self.assertEqual(summary["pair_counts"]["both_pass"], 0)

    def test_v1_budget_classification_can_recover_preserved_invalidity(self):
        trial = record("a", 1, "off", grade="fail")
        trial["record_classification_revision"] = "agent-completion-v1"
        trial["behavioral_grade"] = {"status": "pass"}
        trial["validity_issues"] = ["model_drift", "agent_budget_exhausted"]
        trial["agent_timed_out"] = True
        derived = metrics.classify_record(trial)
        self.assertEqual(derived["grade"]["status"], "infrastructure_error")
        self.assertEqual(derived["validity_issues"], trial["validity_issues"])
        self.assertEqual(derived["record_classification_revision"], "agent-completion-v2")

    def test_missing_fields_and_grader_failure_are_not_runtime_errors(self):
        for grade in ("pass", "fail"):
            trial = record("a", 1, "off", grade=grade)
            derived = metrics.classify_record(trial)
            self.assertEqual(derived["grade"], trial["grade"])
            self.assertNotIn("agent_runtime_error", derived.get("validity_issues", []))
        unknown = record("a", 1, "off", grade="unknown")
        report = metrics.paired_summary([unknown, record("a", 1, "brain")])
        self.assertEqual(report["pair_counts"]["infrastructure"], 1)
        self.assertEqual(report["pair_counts"]["brain_only_pass"], 0)

    def test_infrastructure_error_spellings_are_normalized(self):
        records = [record("a", n, "off", grade=grade) for n, grade in enumerate(("infra", "infra_error", "infrastructure_error"))]
        report = metrics.paired_summary(records)
        self.assertEqual(report["all_attempts"]["off"]["grade_counts"], {"infra": 3})

    def test_failed_run_reductions_never_become_savings(self):
        records = [
            record("task-a", 1, "off", tokens=100),
            record("task-a", 1, "brain", tokens=80, calls=8, wall=15, cost=0.8),
            record("task-a", 2, "off", tokens=100),
            record("task-a", 2, "brain", grade="fail", tokens=1, calls=1, wall=1, cost=0.01),
            record("task-b", 1, "off", grade="infra_error", tokens=5, cost=0.05),
            record("task-b", 1, "brain", tokens=90, cost=0.9),
            record("task-c", 1, "off", grade="fail", tokens=5, cost=0.05),
            record("task-c", 1, "brain", grade="fail", tokens=2, cost=0.02),
        ]
        report = metrics.paired_summary(records)
        self.assertEqual(report["pair_counts"]["both_pass"], 1)
        self.assertEqual(report["pair_counts"]["off_only_pass"], 1)
        self.assertEqual(report["pair_counts"]["brain_only_pass"], 0)
        self.assertEqual(report["pair_counts"]["infrastructure"], 1)
        self.assertEqual(report["pair_counts"]["neither_pass"], 1)
        total = report["both_pass"]["aggregate"]["metrics"]["total_tokens"]
        self.assertEqual(total["pair_count"], 1)
        self.assertEqual(total["comparison"]["reduction_percent"], 20)
        self.assertEqual(total["comparison"]["off"], 100)
        self.assertEqual(total["comparison"]["brain"], 80)
        self.assertEqual(report["all_attempts"]["brain"]["totals_observed"]["total_tokens"], 173)
        self.assertAlmostEqual(report["all_attempts"]["brain"]["totals_observed"]["cost_usd_estimate"], 1.73)
        self.assertEqual(report["all_attempts"]["off"]["grade_counts"]["infra"], 1)
        for pair in report["pairs"]:
            self.assertEqual("deltas" in pair, pair["status"] == "both_pass")

    def test_missing_measurements_use_matched_denominators(self):
        records = [
            record("a", 1, "off", tokens=100, calls=10),
            record("a", 1, "brain", tokens=None, calls=7),
            record("a", 2, "off", tokens=200, calls=12),
            record("a", 2, "brain", tokens=100, calls=6),
        ]
        report = metrics.paired_summary(records)
        aggregate = report["both_pass"]["aggregate"]
        self.assertEqual(aggregate["pair_count"], 2)
        token_metric = aggregate["metrics"]["total_tokens"]
        self.assertEqual(token_metric["pair_count"], 1)
        self.assertEqual(token_metric["missing_pair_count"], 1)
        self.assertEqual(token_metric["comparison"]["reduction_percent"], 50)
        self.assertEqual(aggregate["metrics"]["tool_calls"]["pair_count"], 2)
        self.assertEqual(report["all_attempts"]["brain"]["measurement_coverage"]["total_tokens"], {"available": 1, "missing": 1, "total": 2, "partial": 0})
        self.assertEqual(report["measurement_coverage"]["complete_pairs_with_both_measurements"]["total_tokens"], 1)

    def test_missing_pairs_duplicates_and_invalid_records_stay_visible(self):
        report = metrics.paired_summary([
            record("a", 1, "off"),
            record("b", 1, "off"), record("b", 1, "off"), record("b", 1, "brain"),
            record("c", 1, "other"),
        ])
        self.assertEqual(report["pair_counts"]["incomplete"], 1)
        self.assertEqual(report["pair_counts"]["ambiguous"], 1)
        self.assertEqual(report["pair_counts"]["both_pass"], 0)
        self.assertEqual(report["all_attempts"]["off"]["attempts"], 3)
        self.assertEqual(report["all_attempts"]["unknown"]["attempts"], 1)
        self.assertEqual(report["measurement_coverage"]["invalid_record_indices"], [4])

    def test_per_task_ratios_use_matched_sums_and_preserve_increases(self):
        report = metrics.paired_summary([
            record("a", 1, "off", tokens=100), record("a", 1, "brain", tokens=200),
            record("a", 2, "off", tokens=900), record("a", 2, "brain", tokens=450),
            record("b", 1, "off", tokens=0), record("b", 1, "brain", tokens=10),
        ])
        tasks = {task["task_id"]: task for task in report["both_pass"]["per_task"]}
        comparison = tasks["a"]["metrics"]["total_tokens"]["comparison"]
        self.assertEqual(comparison["off"], 1000)
        self.assertEqual(comparison["brain"], 650)
        self.assertEqual(comparison["reduction_percent"], 35)
        self.assertEqual(report["pairs"][0]["deltas"]["total_tokens"]["reduction_percent"], -100)
        self.assertIsNone(tasks["b"]["metrics"]["total_tokens"]["comparison"]["reduction_percent"])

    def test_partial_trace_tool_count_does_not_claim_savings(self):
        off = record("a", 1, "off", calls=20)
        brain = record("a", 1, "brain", calls=1)
        brain["metrics"]["tool_calls_complete"] = False
        report = metrics.paired_summary([off, brain])
        comparison = report["both_pass"]["aggregate"]["metrics"]["tool_calls"]
        self.assertIsNone(comparison["comparison"])
        self.assertEqual(comparison["missing_pair_count"], 1)
        self.assertEqual(report["all_attempts"]["brain"]["totals_observed"]["tool_calls"], 1)
        self.assertEqual(report["all_attempts"]["brain"]["measurement_coverage"]["tool_calls"],
                         {"available": 1, "missing": 0, "total": 1, "partial": 1})

    def test_partial_usage_never_claims_savings_even_when_both_outcomes_pass(self):
        usage_fields = (*metrics.TOKEN_FIELDS, "processed_input_tokens", "total_tokens", "cost_usd_estimate")
        for partial_arm in ("off", "brain"):
            with self.subTest(partial_arm=partial_arm):
                off = record("a", 1, "off", tokens=100, cost=1)
                brain = record("a", 1, "brain", tokens=10, cost=.1)
                off["metrics"].update(input_tokens=10, output_tokens=20, cache_creation_input_tokens=10,
                                      cache_read_input_tokens=60, processed_input_tokens=80)
                brain["metrics"].update(input_tokens=1, output_tokens=2, cache_creation_input_tokens=1,
                                        cache_read_input_tokens=6, processed_input_tokens=8)
                partial = off if partial_arm == "off" else brain
                partial["usage_may_be_partial"] = True
                report = metrics.paired_summary([off, brain])
                self.assertEqual(report["pair_counts"]["both_pass"], 1)
                observed = report["all_attempts"][partial_arm]
                for name in usage_fields:
                    comparison = report["both_pass"]["aggregate"]["metrics"][name]
                    self.assertEqual(comparison["pair_count"], 0, name)
                    self.assertEqual(comparison["missing_pair_count"], 1, name)
                    self.assertIsNone(comparison["comparison"], name)
                    self.assertEqual(report["measurement_coverage"]["complete_pairs_with_both_measurements"][name], 0)
                    self.assertEqual(observed["totals_observed"][name], partial["metrics"][name])
                    self.assertEqual(observed["measurement_coverage"][name],
                                     {"available": 1, "missing": 0, "total": 1, "partial": 1})
                for name in ("tool_calls", "agent_wall_seconds"):
                    self.assertEqual(report["both_pass"]["aggregate"]["metrics"][name]["pair_count"], 1)
                    self.assertEqual(observed["measurement_coverage"][name]["partial"], 0)

    def test_partial_usage_does_not_turn_missing_measurements_into_observations(self):
        brain = record("a", 1, "brain", tokens=None, cost=None)
        brain["usage_may_be_partial"] = True
        report = metrics.paired_summary([record("a", 1, "off"), brain])
        for name in ("total_tokens", "cost_usd_estimate"):
            self.assertIsNone(report["all_attempts"]["brain"]["totals_observed"][name])
            self.assertEqual(report["all_attempts"]["brain"]["measurement_coverage"][name],
                             {"available": 0, "missing": 1, "total": 1, "partial": 0})
            self.assertIsNone(report["both_pass"]["aggregate"]["metrics"][name]["comparison"])

    def test_empty_report_does_not_fabricate_zero_cost(self):
        report = metrics.paired_summary([])
        self.assertEqual(report["record_count"], 0)
        self.assertEqual(report["pair_counts"]["both_pass"], 0)
        self.assertIsNone(report["all_attempts"]["off"]["totals_observed"]["cost_usd_estimate"])


if __name__ == "__main__":
    unittest.main()
