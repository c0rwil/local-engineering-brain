"""Published pilot evidence excludes private artifacts and keeps denominators."""

import importlib
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "scripts"))
reporter = importlib.import_module("report_real_repo_pilot")


def record(task, arm, status="pass", tokens=100, repetition=1):
    return {
        "task_id": task, "repetition": repetition, "arm": arm,
        "grade": {"status": status, "checks": [{"name": "behavior_preserved", "status": "pass", "error": "PRIVATE-SENTINEL"}], "error": "PRIVATE-SENTINEL"},
        "agent_wall_seconds": 10, "setup_seconds": 2, "grading_seconds": 3,
        "metrics": {"total_tokens": tokens, "tool_calls": 5, "tool_calls_complete": True,
                    "cost_usd_estimate": 0.2 if tokens is not None else None,
                    "tool_use_ids": ["PRIVATE-SENTINEL"], "permission_denials": [{"tool_input": "PRIVATE-SENTINEL"}],
                    "model_usage": {"pinned-model": {"inputTokens": 10, "secret": "PRIVATE-SENTINEL"}}},
        "delivery": {"emission_receipts": 1 if arm == "brain" else 0, "compiled_runs": 2 if arm == "brain" else 0,
                     "delivered_run_ids": ["PRIVATE-SENTINEL"], "delivered_sessions": ["PRIVATE-SENTINEL"]},
        "patch": "PRIVATE-SENTINEL", "source": "PRIVATE-SENTINEL", "infrastructure_error": "PRIVATE-SENTINEL",
    }


class RealRepoReportTest(unittest.TestCase):
    def publish(self, records=(), infrastructure=(), *, preflights=(), repetitions=3, trials=(), private_dependency_seconds=None, dependency_seconds=6, suite=None, adjudicate=None, stale_adjudication=False, traces=None, stopped=False, manifest_extra=None):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        data, output = root / "data", root / "report"
        data.mkdir()
        (data / "manifest.json").write_text(json.dumps({
            **({"suite": suite} if suite is not None else {}),
            "model": "pinned-model", "effort": "high", "repetitions": repetitions,
            "pnpm": "10.18.0", "private_dependencies": "Prepared private dependency cache",
            "known_scope_limits": ["Provider caching remains enabled and is not reset between runs."],
            "engram_binary": "PRIVATE-SENTINEL", "smoke_evidence": "PRIVATE-SENTINEL",
            **(manifest_extra or {}),
        }))
        (data / "records.jsonl").write_text("".join(json.dumps(value) + "\n" for value in records))
        (data / "infrastructure_attempts.jsonl").write_text("".join(json.dumps(value) + "\n" for value in infrastructure))
        (data / "preflight_runs.jsonl").write_text("".join(json.dumps(value) + "\n" for value in preflights))
        for task in reporter.TASK_NAMES:
            fixture = data / "fixtures" / task
            fixture.mkdir(parents=True)
            (fixture / "ready.json").write_text(json.dumps({
                "task_id": task, "setup_seconds": 10, "index_seconds": 2, "dependency_seconds": dependency_seconds,
                "private_dependency_seconds": private_dependency_seconds,
                "source_counts": {"nodes": 100, "memories": 0, "tasks": 0, "context_runs": 0},
                "project_id": "PRIVATE-SENTINEL", "index": {"root": "PRIVATE-SENTINEL"},
            }))
        for trial in trials:
            folder = data / "trials" / trial
            folder.mkdir(parents=True)
            (folder / "trace.jsonl").write_text("PRIVATE-SENTINEL")
        for identity, events in (traces or {}).items():
            folder = data / "trials" / identity
            folder.mkdir(parents=True, exist_ok=True)
            (folder / "trace.jsonl").write_text("".join(json.dumps(event) + "\n" for event in events))
        if stopped:
            (data / "collection_stop.json").write_text(json.dumps({
                "schema_version": 1, "reason": "shell_spawn_argument_limit", "controls_sha256": None,
            }))
        if adjudicate is not None:
            trial = data / "trials" / adjudicate["id"]
            trial.mkdir(parents=True, exist_ok=True)
            grade_bytes = json.dumps(adjudicate["grade"]).encode()
            (trial / "grade.json").write_bytes(grade_bytes)
            (data / "adjudications.json").write_text(json.dumps({
                "schema_version": 1, "adjudications": {adjudicate["id"]: {
                    "grade_sha256": "0" * 64 if stale_adjudication else hashlib.sha256(grade_bytes).hexdigest(),
                    "failed_check": "ambiguous_status", "reason": "ambiguous_grader_contract",
                }},
            }))
        progress = reporter.generate_report(data, output)
        summary = json.loads((output / "summary.json").read_text())
        results = json.loads((output / "results.json").read_text())
        readme = (output / "README.md").read_text()
        return progress, summary, results, readme, output

    def test_shell_spawn_error_is_evidence_bound_and_aborted_collection_has_no_savings(self):
        rows, traces = [], {}
        for arm in ("off", "brain"):
            value = record("r01_guide_hot_path", arm)
            value["id"] = f"r01_guide_hot_path-r1-{arm}"
            rows.append(value)
            traces[value["id"]] = [
                {"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "shell", "name": "Bash", "input": {"command": "PRIVATE-SENTINEL"}}]}},
                {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "shell", "is_error": True,
                    "content": "Could not start /bin/zsh: the command line plus environment exceed the OS exec argument limit (E2BIG). At spawn: command line 1.2MB. The Bash sandbox profile adds 264 filesystem deny paths to every command."}]}},
            ]
        progress, summary, results, readme, output = self.publish(rows, traces=traces, stopped=True, trials=["interrupted"])
        self.assertEqual(progress["status"], "aborted")
        self.assertEqual(progress["in_progress_trials"], 0)
        self.assertEqual(progress["interrupted_trials"], 1)
        self.assertEqual(progress["infrastructure_outcomes"], 2)
        self.assertEqual(summary["pair_counts"]["both_pass"], 0)
        self.assertEqual(summary["pair_counts"]["infrastructure"], 1)
        for row in results["records"]:
            self.assertEqual(row["behavioral_grade"]["status"], "pass")
            self.assertEqual(row["metrics"]["total_tokens"], 100)
            self.assertFalse(row["usage_may_be_partial"])
            self.assertEqual(row["tool_runtime_evidence"]["shell_spawn_failure_count"], 1)
            self.assertEqual(len(row["tool_runtime_evidence"]["trace_sha256"]), 64)
        self.assertIn("Collection aborted", readme)
        for path in output.iterdir():
            self.assertNotIn("PRIVATE-SENTINEL", path.read_text())

    def test_aborted_report_refuses_missing_mismatched_or_unverified_runtime_trace(self):
        for identity in (None, "../outside", "r02_lean_task_status-r1-off", "r01_guide_hot_path-r1-off"):
            row = record("r01_guide_hot_path", "off")
            if identity is not None:
                row["id"] = identity
            with self.subTest(identity=identity), self.assertRaises(reporter.AdjudicationError):
                self.publish([row], stopped=True)
        row = record("r01_guide_hot_path", "off")
        row["id"] = "r01_guide_hot_path-r1-off"
        row["tool_runtime_evidence"] = {"schema_version": 1, "trace_sha256": "0" * 64,
            "reason": "shell_spawn_argument_limit", "shell_tool_calls": 1, "shell_spawn_failure_count": 1}
        with self.assertRaises(reporter.AdjudicationError):
            self.publish([row], stopped=True, traces={row["id"]: []})

    def test_private_payloads_are_not_published(self):
        _, _, results, _, output = self.publish([record("r01_guide_hot_path", "brain")], trials=["active"])
        for path in output.iterdir():
            self.assertNotIn("PRIVATE-SENTINEL", path.read_text())
        clean = results["records"][0]
        self.assertEqual(clean["grade"]["checks"], [{"name": "behavior_preserved", "passed": True}])
        self.assertEqual(clean["metrics"]["permission_denial_count"], 1)
        self.assertNotIn("tool_use_ids", clean["metrics"])
        self.assertNotIn("delivered_sessions", clean["delivery"])
        self.assertEqual(results["manifest"]["pnpm"], "10.18.0")
        self.assertEqual(results["manifest"]["private_dependencies"], "Prepared private dependency cache")
        self.assertIn("Provider caching remains enabled and is not reset between runs.", (output / "README.md").read_text())

    def test_codex_collection_reports_its_agent_models_and_reasoning(self):
        rows = []
        for arm in ("off", "brain"):
            value = record("r01_guide_hot_path", arm)
            value["metrics"].update({"model_usage": None, "models": ["pinned-model"], "reasoning_output_tokens": 10,
                                     "cost_usd_estimate": None})
            rows.append(value)
        _, summary, results, readme, output = self.publish(rows, suite="real-repo-v2", manifest_extra={
            "agent": "codex", "codex_version": "codex-cli 0.0.0", "codex_hook_trust": "bypass"})
        self.assertEqual(results["manifest"]["agent"], "codex")
        for row in results["records"]:
            self.assertEqual(row["metrics"]["models"], ["pinned-model"])
            self.assertEqual(row["metrics"]["reasoning_output_tokens"], 10)
        self.assertEqual(summary["all_attempts"]["brain"]["totals_observed"]["reasoning_output_tokens"], 10)
        for phrase in ("Codex CLI `codex-cli 0.0.0`", "Reasoning (within output)", "Codex reports no USD estimate",
                       "source-only automatic hooks and the engram MCP server"):
            self.assertIn(phrase, readme)
        self.assertNotIn("Claude Code `", readme)
        self.assertNotIn("Claude-reported USD", readme)
        for path in output.iterdir():
            self.assertNotIn("PRIVATE-SENTINEL", path.read_text())

    def test_claude_report_header_is_unchanged(self):
        _, _, _, readme, _ = self.publish([record("r01_guide_hot_path", "off")])
        self.assertIn("Claude Code `unavailable`. Protocol: 3 repetitions per task and arm, Engram off versus source-only automatic hooks.", readme)
        self.assertNotIn("Reasoning (within output)", readme)

    def test_failed_attempts_count_in_resource_per_success_but_not_savings(self):
        records = [
            record("r01_guide_hot_path", "off", tokens=100), record("r01_guide_hot_path", "brain", status="fail", tokens=10),
            record("r02_lean_task_status", "off", tokens=100), record("r02_lean_task_status", "brain", tokens=80),
        ]
        progress, summary, _, readme, _ = self.publish(records)
        self.assertEqual(progress["completed_records"], 4)
        self.assertEqual(progress["planned_runs"], 24)
        self.assertEqual(progress["status"], "in_progress")
        self.assertEqual(summary["resource_per_validated_success"]["brain"]["total_tokens"]["value"], 90)
        self.assertEqual(summary["both_pass"]["aggregate"]["metrics"]["total_tokens"]["comparison"]["reduction_percent"], 20)
        self.assertEqual(summary["pair_counts"]["off_only_pass"], 1)
        self.assertIn("planned matrix is not complete", readme)

    def test_explicit_setup_exit_survives_publication(self):
        value = record("r01_guide_hot_path", "off", status="infrastructure_error")
        value["setup_exit_code"] = 2
        _, summary, results, _, _ = self.publish([value])
        self.assertEqual(results["records"][0]["setup_exit_code"], 2)
        self.assertEqual(summary["all_attempts"]["off"]["grade_counts"], {"infra": 1})

    def test_public_grader_hashes_and_analysis_provenance(self):
        value = record("r01_guide_hot_path", "brain")
        public_path = "grading/r01_guide_hot_path/grade.py"
        value["grader_manifest"] = {
            public_path: "a" * 64,
            "/private/source.py": "b" * 64,
            "grading/r01_guide_hot_path/../secret.py": "c" * 64,
            "grading/r01_guide_hot_path/invalid.py": "PRIVATE-SENTINEL",
        }
        _, summary, results, readme, _ = self.publish([value])
        self.assertEqual(results["records"][0]["grader_manifest"], {public_path: "a" * 64})
        self.assertEqual(set(results["analysis_provenance"]), {"report_real_repo_pilot.py", "real_repo_metrics.py", "real_repo_diagnostics.py", "real_repo_adjudications.py"})
        self.assertTrue(all(len(digest) == 64 for digest in results["analysis_provenance"].values()))
        self.assertEqual(summary["analysis_provenance"], results["analysis_provenance"])
        self.assertIn("Cache creation | Cache reads | Output", readme)

    def test_runtime_classification_precedes_all_report_tables_and_redaction(self):
        off = record("r01_guide_hot_path", "off", tokens=100)
        off["agent_exit_code"] = 1
        off["metrics"].update({"result_subtype": "success", "is_error": True})
        brain = record("r01_guide_hot_path", "brain", tokens=80)
        progress, summary, results, readme, _ = self.publish([off, brain])
        self.assertEqual(off["grade"]["status"], "pass")
        self.assertEqual(progress["infrastructure_outcomes"], 1)
        self.assertEqual(summary["pair_counts"]["infrastructure"], 1)
        self.assertEqual(summary["pair_counts"]["brain_only_pass"], 0)
        self.assertEqual(summary["all_attempts"]["off"]["grade_counts"], {"infra": 1})
        self.assertEqual(summary["per_task"][0]["behavioral_grade_counts"]["off"], {"pass": 1})
        self.assertEqual(summary["all_attempts"]["off"]["totals_observed"]["total_tokens"], 100)
        self.assertIsNone(summary["resource_per_validated_success"]["off"]["total_tokens"]["value"])
        published = results["records"][0]
        self.assertEqual(published["behavioral_grade"]["status"], "pass")
        self.assertEqual(published["grade"]["status"], "infra")
        self.assertEqual(published["validity_issues"], ["agent_runtime_error"])
        self.assertTrue(published["usage_may_be_partial"])
        self.assertEqual(results["record_classification_revision"], reporter.RECORD_CLASSIFICATION_REVISION)
        self.assertEqual(summary["record_classification_revision"], reporter.RECORD_CLASSIFICATION_REVISION)
        self.assertIn("Clean validated completions", readme)
        self.assertIn("Behavioral passes (off; brain)", readme)
        self.assertIn("infrastructure/unscorable: 1", readme)
        self.assertIn("usage from error or interrupted runs may be partial", readme)
        self.assertIn("Measurement coverage counts reported values, not complete error-run billing", readme)

    def test_adjudication_keeps_frozen_grade_and_usage_without_awarding_pair_win(self):
        off = record("r06_onboarding_outage", "off", status="fail", tokens=100)
        off["id"] = "r06_onboarding_outage-r1-off"
        off["grade"]["checks"] = [{"name": "ambiguous_status", "status": "fail"}]
        brain = record("r06_onboarding_outage", "brain", tokens=80)
        progress, summary, results, readme, _ = self.publish([off, brain], adjudicate=off)
        self.assertEqual(progress["infrastructure_outcomes"], 1)
        self.assertEqual(summary["pair_counts"]["infrastructure"], 1)
        self.assertEqual(summary["pair_counts"]["brain_only_pass"], 0)
        self.assertEqual(summary["pair_counts"]["both_pass"], 0)
        self.assertEqual(summary["all_attempts"]["off"]["totals_observed"]["total_tokens"], 100)
        self.assertEqual(results["records"][0]["behavioral_grade"]["status"], "fail")
        self.assertEqual(results["records"][0]["grade"]["status"], "infra")
        self.assertEqual(results["records"][0]["validity_issues"], ["ambiguous_grader_contract"])
        self.assertEqual(set(results["records"][0]["adjudication"]), {
            "schema_version", "trial_id", "reason", "grade_sha256", "failed_check_sha256",
        })
        self.assertEqual(summary["adjudication_counts"], {"ambiguous_grader_contract": 1})
        self.assertIn("Original frozen grades are unchanged", readme)
        self.assertEqual(off["grade"]["status"], "fail")

    def test_stale_adjudication_aborts_instead_of_dropping_record(self):
        off = record("r06_onboarding_outage", "off", status="fail")
        off["id"] = "r06_onboarding_outage-r1-off"
        off["grade"]["checks"] = [{"name": "ambiguous_status", "status": "fail"}]
        with self.assertRaises(reporter.AdjudicationError):
            self.publish([off], adjudicate=off, stale_adjudication=True)

    def test_unmatched_or_duplicate_adjudication_aborts_publication(self):
        off = record("r06_onboarding_outage", "off", status="fail")
        off["id"] = "r06_onboarding_outage-r1-off"
        off["grade"]["checks"] = [{"name": "ambiguous_status", "status": "fail"}]
        for records in ([], [off, off]):
            with self.subTest(records=len(records)), self.assertRaises(reporter.AdjudicationError):
                self.publish(records, adjudicate=off)

    def test_unverified_provenance_cannot_enter_published_artifacts(self):
        value = record("r01_guide_hot_path", "off")
        value["adjudication"] = {key: "PRIVATE-SENTINEL" for key in (
            "schema_version", "trial_id", "reason", "grade_sha256", "failed_check_sha256",
        )}
        _, _, results, _, output = self.publish([value], preflights=[value])
        self.assertNotIn("adjudication", results["records"][0])
        self.assertNotIn("adjudication", results["preflight_runs"][0])
        for path in output.iterdir():
            self.assertNotIn("PRIVATE-SENTINEL", path.read_text())

    def test_report_keeps_infrastructure_precedence_during_timeout(self):
        off = record("r01_guide_hot_path", "off", status="infrastructure_error")
        off["behavioral_grade"] = {"status": "pass"}
        off["validity_issues"] = ["no_delivery_receipt"]
        off["agent_timed_out"] = True
        _, summary, results, readme, _ = self.publish([off, record("r01_guide_hot_path", "brain")])
        self.assertEqual(summary["pair_counts"]["infrastructure"], 1)
        self.assertEqual(summary["pair_counts"]["brain_only_pass"], 0)
        self.assertEqual(results["records"][0]["grade"]["status"], "infra")
        self.assertEqual(results["records"][0]["behavioral_grade"]["status"], "pass")
        self.assertEqual(results["record_classification_revision"], "agent-completion-v2")
        self.assertIn("unless infrastructure already invalidates the trial", readme)

    def test_infrastructure_attempts_stay_outside_the_matrix_and_unknown_stays_unknown(self):
        interrupted = record("r01_guide_hot_path", "off", status="infrastructure_error", tokens=None)
        interrupted["metrics"]["tool_calls_complete"] = False
        progress, summary, results, readme, _ = self.publish(infrastructure=[interrupted])
        self.assertEqual(progress["completed_records"], 0)
        self.assertEqual(progress["status"], "not_started")
        self.assertEqual(summary["pair_counts"]["observed_pairs"], 0)
        separate = summary["infrastructure_attempts"]["by_arm"]["off"]
        self.assertEqual(separate["grade_counts"], {"infra": 1})
        self.assertIsNone(separate["totals_observed"]["total_tokens"])
        self.assertEqual(separate["measurement_coverage"]["total_tokens"]["missing"], 1)
        self.assertEqual(len(results["infrastructure_attempts"]), 1)
        self.assertIn("outside the planned matrix", readme)

    def test_zero_success_and_partial_coverage_are_explicit(self):
        records = [record("r01_guide_hot_path", "off", status="fail", tokens=None), record("r01_guide_hot_path", "brain", tokens=None)]
        _, summary, _, readme, _ = self.publish(records)
        failed = summary["resource_per_validated_success"]["off"]["total_tokens"]
        self.assertIsNone(failed["value"])
        self.assertTrue(failed["partial"])
        self.assertIn("undefined (partial coverage)", readme)
        self.assertEqual(summary["overhead"]["fixtures"]["setup_seconds"]["observed_total"], 40)
        self.assertEqual(summary["overhead"]["fixtures"]["index_seconds"]["observed_total"], 8)

    def test_preflight_costs_and_private_dependency_time_are_separate(self):
        smokes = [record("environment_preflight", "off", tokens=40), record("environment_preflight", "brain", tokens=None)]
        smokes[1]["metrics"]["tool_calls_complete"] = False
        progress, summary, results, readme, _ = self.publish(preflights=smokes, private_dependency_seconds=12)
        self.assertEqual(progress["completed_records"], 0)
        self.assertEqual(summary["pair_counts"]["observed_pairs"], 0)
        self.assertEqual(summary["preflight_runs"]["attempts"], 2)
        self.assertEqual(summary["preflight_runs"]["by_arm"]["off"]["totals_observed"]["total_tokens"], 40)
        self.assertIsNone(summary["preflight_runs"]["by_arm"]["brain"]["totals_observed"]["total_tokens"])
        self.assertEqual(summary["preflight_runs"]["by_arm"]["brain"]["measurement_coverage"]["total_tokens"]["missing"], 1)
        self.assertEqual(len(results["preflight_runs"]), 2)
        self.assertEqual(summary["overhead"]["fixtures"]["private_dependency_seconds"]["observed_total"], 48)
        self.assertNotIn("inherited_seed_private_dependency_seconds", results["fixtures"][0])
        self.assertNotIn("inherited_seed_private_dependency_seconds", summary["overhead"]["fixtures"])
        self.assertIn("Original setup seconds", readme)
        self.assertIn("Additional private dependency preparation: 48.0 seconds", readme)
        self.assertIn("Runtime, authentication, isolation, and toolchain preflights", readme)

    def test_followup_separates_current_setup_from_inherited_seed_cache_preparation(self):
        _, summary, results, readme, _ = self.publish(
            suite="real-repo-v2", dependency_seconds=0, private_dependency_seconds=12)
        fixture = results["fixtures"][0]
        overhead = summary["overhead"]["fixtures"]
        self.assertEqual(fixture["setup_seconds"], 10)
        self.assertEqual(fixture["dependency_seconds"], 0)
        self.assertEqual(fixture["inherited_seed_private_dependency_seconds"], 12)
        self.assertNotIn("private_dependency_seconds", fixture)
        self.assertEqual(overhead["setup_seconds"]["observed_total"], 40)
        self.assertEqual(overhead["dependency_seconds"]["observed_total"], 0)
        self.assertEqual(overhead["inherited_seed_private_dependency_seconds"]["observed_total"], 48)
        self.assertNotIn("private_dependency_seconds", overhead)
        self.assertIn("Collection clone/reindex seconds", readme)
        self.assertIn("Measured collection fixture setup: 40.0 seconds", readme)
        self.assertIn("Inherited seed private-cache preparation (earlier pilot): 48.0 seconds", readme)
        self.assertIn("belongs to the earlier pilot, not this collection", readme)
        self.assertNotIn("Original setup seconds", readme)
        self.assertNotIn("Additional private dependency preparation:", readme)

    def test_followup_missing_seed_cache_measurements_remain_unknown(self):
        _, summary, results, readme, _ = self.publish(suite="real-repo-v2", dependency_seconds=0)
        self.assertIsNone(results["fixtures"][0]["inherited_seed_private_dependency_seconds"])
        self.assertEqual(summary["overhead"]["fixtures"]["inherited_seed_private_dependency_seconds"],
                         {"observed_total": None, "available": 0, "total": 4, "partial": True})
        self.assertIn("Inherited seed private-cache preparation (earlier pilot): unavailable seconds [0/4 fixtures recorded]", readme)

    def test_complete_requires_all_expected_pairs_not_just_record_count(self):
        records = [record(task, arm, repetition=repetition) for task in reporter.TASK_NAMES for repetition in (1, 2, 3) for arm in ("off", "brain")]
        progress, _, _, readme, _ = self.publish(records)
        self.assertEqual(progress["status"], "complete")
        self.assertNotIn("planned matrix is not complete", readme)
        records[-1] = records[-2]
        progress, summary, _, _, _ = self.publish(records)
        self.assertEqual(progress["completed_records"], 24)
        self.assertEqual(progress["status"], "in_progress")
        self.assertEqual(summary["pair_counts"]["ambiguous"], 1)

    def test_unexpected_repetitions_cannot_substitute_for_planned_runs(self):
        records = [record(task, arm, repetition=repetition) for task in reporter.TASK_NAMES for repetition in (1, 2, 4) for arm in ("off", "brain")]
        progress, _, _, _, _ = self.publish(records)
        self.assertEqual(progress["completed_records"], 24)
        self.assertEqual(progress["status"], "in_progress")


if __name__ == "__main__":
    unittest.main()
