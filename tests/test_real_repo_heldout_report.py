"""Catalog-selected suites: held-out reporting, context-delivery metrics and native import.

Hermetic: a synthetic collection root with stream-JSON traces and Engram state
databases. v1/v2 byte identity is covered by tests/real_repo_report_observation.py.
"""
from contextlib import closing, redirect_stdout
from copy import deepcopy
import hashlib
import io
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

TESTS = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS.parent / "scripts"))
sys.path.insert(0, str(TESTS))
sys.dont_write_bytecode = True
import import_real_repo_pilot as importer  # noqa: E402
import real_repo_context as context  # noqa: E402
import real_repo_report_observation as observation  # noqa: E402
import report_real_repo_pilot as reporter  # noqa: E402

SUITE_ID = "synthetic-heldout-v1"
MODEL = "claude-opus-4-8[1m]"
TASKS = ("h1_large_file_fix", "h2_related_sites")
SECRET = "PRIVATE-SENTINEL"
HEADING = "# Task source map (indexed reference data)\n"


def sha(*parts):
    return hashlib.sha256("/".join(map(str, parts)).encode()).hexdigest()


def trace(arm, reads):
    events = [{"type": "system", "subtype": "init", "session_id": f"{SECRET}-session", "cwd": f"/private/{SECRET}"}]
    for index, (size, ranged) in enumerate(reads):
        identity = f"toolu_{arm}_{index}"
        payload = {"file_path": f"/private/{SECRET}/src/engine.rs", **({"offset": 1700, "limit": 120} if ranged else {})}
        events.append({"type": "assistant", "message": {"model": MODEL, "content": [
            {"type": "tool_use", "id": identity, "name": "Read", "input": payload}]}})
        events.append({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": identity, "content": [{"type": "text", "text": SECRET[:1] * size}]}]}})
    events.append({"type": "assistant", "message": {"model": MODEL, "content": [
        {"type": "tool_use", "id": f"toolu_{arm}_edit", "name": "Edit", "input": {"file_path": f"/private/{SECRET}"}}]}})
    events.append({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": f"toolu_{arm}_edit", "content": "ok"}]}})
    events.append({"type": "result", "subtype": "success", "is_error": False})
    return events


def brain_state(path, contexts):
    """An Engram state database holding delivered (receipt-confirmed) and compiled-only contexts."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with closing(sqlite3.connect(path)) as con:
        con.executescript("""
            CREATE TABLE context_runs(id TEXT, mode TEXT, selected_tokens INTEGER, stable_prefix TEXT, dynamic_context TEXT);
            CREATE TABLE context_items(context_run_id TEXT, ordinal INTEGER, source_type TEXT, source_id TEXT);
            CREATE TABLE usage_events(source_type TEXT, source_id TEXT, usage_kind TEXT, context_run_id TEXT);
            CREATE TABLE tasks(id TEXT, session_id TEXT);
            INSERT INTO tasks VALUES ('task-1', 'session-1');
        """)
        for index, (delivered, text, items) in enumerate(contexts):
            run = f"run-{index}"
            con.execute("INSERT INTO context_runs VALUES (?, 'brain', 100, ?, ?)", (run, f"{SECRET} prefix\n", text))
            for ordinal, source_type in enumerate(items):
                con.execute("INSERT INTO context_items VALUES (?, ?, ?, ?)", (run, ordinal, source_type, SECRET))
            con.execute("INSERT INTO usage_events VALUES ('task', 'task-1', ?, ?)",
                        ("context_served" if delivered else "context_compiled", run))
        con.commit()


SOURCE_MAP = (HEADING + "Inspect these sources first.\n"
              f"- agi-backend-rust: src/services/engine.rs:1780-1820 — Function {SECRET}\n"
              f"- agi-backend-rust: tests/contract.rs:40-90 — Function {SECRET}\n"
              f"- agi-backend-rust: src/models.rs — Type {SECRET}\n"
              f"- Related use: agi-backend-rust: src/routes/reactions.rs:12-30 — Function {SECRET} (calls emit)\n"
              f"- Related: agi-backend-rust: src/lib.rs — Module {SECRET}\n"
              "\n# Something else\n- not a pointer\n")


def record(task, repetition, arm, ordinal, status="pass"):
    base = 10_000 * (TASKS.index(task) + 1)
    cache_read = 40 * base if arm == "off" else 30 * base
    value = {
        "id": f"{task}-r{repetition}-{arm}", "task_id": task, "repetition": repetition, "arm": arm,
        "started_at": f"2026-10-05T01:{ordinal:02d}:00.000000+00:00",
        "grade": {"status": status, "checks": [{"name": "behavior", "passed": status == "pass"}], "error": SECRET},
        "agent_wall_seconds": 600.0 - (60 if arm == "brain" else 0), "setup_seconds": 45.0, "grading_seconds": 80.0,
        "agent_exit_code": 0, "grader_exit_code": 0 if status == "pass" else 1, "agent_timed_out": False,
        "metrics": {"input_tokens": base, "cache_creation_input_tokens": 2 * base, "cache_read_input_tokens": cache_read,
                    "output_tokens": base // 2, "processed_input_tokens": 3 * base + cache_read,
                    "total_tokens": 3 * base + cache_read + base // 2, "tool_calls": 30, "tool_calls_complete": True,
                    "num_turns": 20, "cost_usd_estimate": 1.5, "is_error": False, "trace_complete": True,
                    "usage_complete": True, "result_subtype": "success", "models": [MODEL],
                    "model_usage": {MODEL: {"inputTokens": base}}, "counts_by_tool": {"Read": 4}, "permission_denials": []},
        "delivery": {"emission_receipts": 2 if arm == "brain" else 0, "compiled_runs": 3 if arm == "brain" else 0,
                     "delivered_sessions": [SECRET]},
        "prompt_sha256": sha("prompt", task), "composed_prompt_sha256": sha("composed", task),
        "snapshot_sha256": sha("snapshot", task),
        "grader_manifest": {f"grading/{task}/{name}": sha("grader", task, name) for name in ("grade.py", "preflight.json", "preflight.py")},
        "validity_issues": [], "patch": SECRET,
    }
    if status == "fail":
        value["metrics"]["result_subtype"] = "success"
    return value


def build_heldout(root, *, draft=True, complete=True):
    data = root / "data"
    data.mkdir(parents=True)
    tasks = [{"id": TASKS[0], "title": "Fix inside a very large file", "selection": "heldout", "exercises": ["line_ranged_pointers"],
              "repository": "agi-backend-rust", "base_commit": "a" * 40, "prompt_file": "prompts/h1.md"},
             {"id": TASKS[1], "title": "Reach every related emitter", "selection": "heldout_control", "exercises": ["related_use_cue"],
              "repository": "agi-backend-rust", "base_commit": "b" * 40, "prompt_file": "prompts/h2.md"}]
    status = "draft_requires_operator_review" if draft else "ready_for_collection"
    catalog = {"suite_id": SUITE_ID, "status": status, "execution_status": status, "tasks": tasks}
    (data / "protocol/suite").mkdir(parents=True)
    (data / "protocol/suite/catalog.json").write_text(json.dumps(catalog))
    suite_files = {"catalog.json": sha("catalog"), "README.md": sha("readme"), "grading/_shared/preflight_common.py": sha("shared")}
    for task in TASKS:
        suite_files.update({f"grading/{task}/{name}": sha("grader", task, name) for name in ("grade.py", "preflight.json", "preflight.py")})
        suite_files[f"grading/{task}/README.md"] = sha("task readme", task)
    manifest = {
        "schema_version": 1, "suite": SUITE_ID, "condition": "source-only native hooks; no MCP; no learned memories",
        "model": MODEL, "effort": "high", "claude_version": "2.1.212 (Claude Code)", "repetitions": 3,
        "agent_limit_seconds": 1200, "grading_limit_seconds": 1800, "cost_cap_usd_estimate": 8,
        "platform_dependencies": "Rust dependencies built offline and locked from each base Cargo.lock before timing",
        "private_dependencies": "Rust registry-only cache; no user Git config; package installation disabled",
        "rustc": "rustc 1.96.0", "python": "3.14.7", "node": "v26.5.1", "pnpm": "10.18.0",
        "known_scope_limits": ["Held-out only after the operator confirms no task informed Engram development."],
        "fixture_source": "operator repositories at the catalog base commits (read-only git archive)",
        "controls": {"suite_files": suite_files}, "engram_binary": f"/private/{SECRET}",
        **{field: sha(field) for field in ("engram_sha256", "daemon_sha256", "runner_sha256", "metrics_sha256",
                                            "controls_sha256", "runner_library_sha256", "claude_sha256",
                                            "runtime_preflight_sha256", "grader_preflight_sha256")},
    }
    if draft:
        manifest.update(allow_draft_suite=True, suite_status=status)
    (data / "manifest.json").write_text(json.dumps(manifest))
    for index, task in enumerate(TASKS):
        fixture = data / "fixtures" / task
        fixture.mkdir(parents=True)
        (fixture / "ready.json").write_text(json.dumps({
            "task_id": task, "base_commit": tasks[index]["base_commit"], "fixture_head": sha("head", task)[:40],
            "snapshot_sha256": sha("snapshot", task), "setup_seconds": 120.0, "export_seconds": 0.4, "index_seconds": 3.0,
            "dependency_seconds": 90.0 if index == 0 else 9.0, "dependency_reuse": None if index == 0 else TASKS[0],
            "build_id": "82b5df1dbe45dd72b1d113909ec81a549242f0a846fa00ff4202cb9b2e12aa9a", "schema_version": 19,
            "source_counts": {"nodes": 22000, "memories": 0, "tasks": 0, "context_runs": 0}, "removed_paths": [],
            "lockfile_sha256": {"Cargo.lock": sha("lock", task)}, "snapshot_refreshes": [{"previous_build_id": "old"}],
            "source_repository": f"/Users/{SECRET}/PycharmProjects/agi-backend-rust", "project_id": SECRET}))
    rows = []
    for repetition in (1, 2, 3):
        for index, task in enumerate(TASKS):
            for arm in (("off", "brain") if (index + repetition) % 2 else ("brain", "off")):
                status = "fail" if (task, repetition, arm) == (TASKS[1], 3, "off") else "pass"
                row = record(task, repetition, arm, len(rows), status)
                if (task, repetition, arm) == (TASKS[0], 2, "brain"):
                    row["agent_exit_code"] = 1
                    row["metrics"]["is_error"] = True
                rows.append(row)
                trial = data / "trials" / row["id"]
                trial.mkdir(parents=True)
                reads = [(30_000, False)] * 4 if arm == "off" else [(2_000, True), (2_000, True), (8_000, False)]
                (trial / "trace.jsonl").write_text("".join(json.dumps(event) + "\n" for event in trace(arm, reads)))
                if arm == "brain":
                    brain_state(trial / "state/brain.sqlite", [
                        (True, SOURCE_MAP, ["code", "code", "code", "code_use", "code_dependency"]),
                        (True, f"{SECRET} without a source map", []),
                        (False, SOURCE_MAP, ["code_use", "code_use"]),   # compiled, never delivered
                    ])
                else:
                    (trial / "state").mkdir()
    if not complete:
        rows = rows[:5]
    (data / "records.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows))
    return data


class ContextMeasurementTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_read_measurements_count_requests_ranges_and_returned_bytes(self):
        path = self.root / "trace.jsonl"
        events = trace("brain", [(2_000, True), (8_000, False)])
        events.insert(3, {"type": "stream_event", "event": {"type": "content_block_start", "content_block":
                          {"type": "tool_use", "id": "toolu_brain_0", "name": "Read", "input": {}}}})
        path.write_text("".join(json.dumps(event) + "\n" for event in events))
        measured = context.read_measurements(path)
        self.assertEqual((measured["read_calls"], measured["ranged_read_calls"], measured["read_results"],
                          measured["read_result_bytes"]), (2, 1, 2, 10_000))
        self.assertTrue(measured["read_measurement_complete"])

    def test_missing_or_errored_results_make_the_read_measurement_incomplete(self):
        path = self.root / "trace.jsonl"
        events = trace("off", [(100, False), (100, False)])
        events[4]["message"]["content"][0]["is_error"] = True   # second Read result
        events.append({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "lost", "name": "Read", "input": {}}]}})
        path.write_text("".join(json.dumps(event) + "\n" for event in events) + "not json\n")
        measured = context.read_measurements(path)
        self.assertEqual((measured["read_calls"], measured["read_results"], measured["read_error_results"],
                          measured["read_result_bytes"]), (3, 2, 1, 100))
        self.assertFalse(measured["read_measurement_complete"])

    def test_only_receipt_confirmed_contexts_are_classified(self):
        database = self.root / "state/brain.sqlite"
        brain_state(database, [(True, SOURCE_MAP, ["code", "code_use"]), (False, SOURCE_MAP, ["code_use"] * 5)])
        measured = context.delivered_context(database)
        self.assertEqual(measured, {"delivered_contexts": 1, "pointer_lines": 5, "ranged_pointer_lines": 3,
                                    "direct_pointer_lines": 3, "neighbor_pointer_lines": 1,
                                    "related_use_pointer_lines": 1, "related_use_items": 1})

    def test_rows_still_in_the_write_ahead_log_are_read_without_touching_the_evidence(self):
        live = self.root / "live/brain.sqlite"
        brain_state(live, [])
        writer = sqlite3.connect(live)
        self.addCleanup(writer.close)
        writer.execute("PRAGMA journal_mode=WAL")
        writer.execute("PRAGMA wal_autocheckpoint=0")
        writer.execute("INSERT INTO context_runs VALUES ('wal-run', 'brain', 1, '', ?)", (SOURCE_MAP,))
        writer.execute("INSERT INTO context_items VALUES ('wal-run', 0, 'code_use', 'x')")
        writer.execute("INSERT INTO usage_events VALUES ('task', 'task-1', 'context_served', 'wal-run')")
        writer.commit()
        evidence = self.root / "trial/state"
        evidence.mkdir(parents=True)
        for suffix in ("", "-wal"):
            (evidence / f"brain.sqlite{suffix}").write_bytes(Path(f"{live}{suffix}").read_bytes())
        before = {path.name: path.read_bytes() for path in evidence.iterdir()}
        measured = context.delivered_context(evidence / "brain.sqlite")
        self.assertEqual((measured["delivered_contexts"], measured["pointer_lines"], measured["related_use_items"]), (1, 5, 1))
        self.assertEqual({path.name: path.read_bytes() for path in evidence.iterdir()}, before)

    def test_missing_evidence_stays_null_and_off_has_no_engram_context(self):
        trial = self.root / "trial"
        trial.mkdir()
        brain = context.trial_context(trial, "brain")
        self.assertIsNone(brain["read_calls"])
        self.assertIsNone(brain["delivered_contexts"])
        self.assertFalse(brain["read_measurement_complete"])
        off = context.trial_context(trial, "off")
        self.assertEqual(off["delivered_contexts"], 0)
        self.assertEqual(off["related_use_items"], 0)


class HeldoutReportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def publish(self, **options):
        data = build_heldout(self.root, **options)
        output = self.root / "published"
        progress = reporter.generate_report(data, output)
        return (progress, json.loads((output / "results.json").read_text()), json.loads((output / "summary.json").read_text()),
                (output / "README.md").read_text(), output)

    def test_report_states_the_held_out_suite_and_the_draft_flag(self):
        progress, results, summary, readme, output = self.publish()
        self.assertEqual((progress["status"], progress["planned_runs"], progress["completed_records"]), ("complete", 12, 12))
        self.assertTrue(readme.startswith("# Held-out real-repository task benchmark\n"))
        self.assertIn("**Held-out suite `synthetic-heldout-v1`.** Its two historical tasks", readme)
        self.assertIn("with `--allow-draft-suite`", readme)
        self.assertIn("held-out status is provisional", readme)
        self.assertEqual(results["suite"]["suite_id"], SUITE_ID)
        self.assertIs(results["suite"]["held_out"], True)
        self.assertIs(results["suite"]["allow_draft_suite"], True)
        self.assertIs(results["manifest"]["allow_draft_suite"], True)
        self.assertEqual(results["manifest"]["suite_status"], "draft_requires_operator_review")
        self.assertEqual(results["suite"]["task_titles"][TASKS[0]], "Fix inside a very large file")
        self.assertEqual(set(results["suite"]["grader_files"][TASKS[0]]),
                         {f"grading/{TASKS[0]}/{name}" for name in ("grade.py", "preflight.json", "preflight.py")})
        self.assertIn("Fix inside a very large file", readme)
        self.assertIn("This is a two-task, three-repetition diagnostic", readme)
        self.assertIn("- Dependencies: Rust dependencies built offline and locked", readme)
        self.assertIn("- Held-out only after the operator confirms", readme)
        self.assertNotIn("four-task", readme)
        self.assertIn("real_repo_context.py", results["analysis_provenance"])
        for path in output.iterdir():
            self.assertNotIn(SECRET, path.read_text(), path.name)

    def test_failures_and_infrastructure_stay_in_denominators_with_paired_semantics(self):
        _, results, summary, readme, _ = self.publish()
        self.assertEqual(summary["all_attempts"]["off"]["grade_counts"], {"pass": 5, "fail": 1})
        self.assertEqual(summary["all_attempts"]["brain"]["grade_counts"], {"pass": 5, "infra": 1})
        self.assertEqual(summary["pair_counts"]["both_pass"], 4)
        self.assertEqual(summary["pair_counts"]["infrastructure"], 1)
        self.assertEqual(summary["pair_counts"]["brain_only_pass"], 1)
        self.assertIn("| off | 5 | 1 | 0 | 0 | 6 |", readme)
        self.assertIn("| brain | 5 | 0 | 1 | 0 | 6 |", readme)
        cache = summary["both_pass"]["aggregate"]["metrics"]["cache_read_input_tokens"]
        self.assertEqual(cache["pair_count"], 4)
        self.assertAlmostEqual(cache["comparison"]["reduction_percent"], 25.0)
        self.assertIn("| Both-pass comparison | Ordinary input | Cache creation | Cache reads | Output |", readme)
        self.assertIn("+25.0% (n=4)", readme)
        # Failed and infrastructure attempts keep their resources in all-attempt totals.
        self.assertEqual(summary["all_attempts"]["brain"]["totals_observed"]["cache_read_input_tokens"],
                         sum(row["metrics"]["cache_read_input_tokens"] for row in results["records"] if row["arm"] == "brain"))

    def test_context_delivery_per_arm_and_both_pass_read_comparison(self):
        _, results, summary, readme, _ = self.publish()
        delivery = summary["context_delivery"]["by_arm"]
        brain, off = delivery["brain"]["totals"], delivery["off"]["totals"]
        self.assertEqual(brain["delivered_contexts"]["observed_total"], 12)
        self.assertEqual(brain["pointer_lines"]["observed_total"], 30)
        self.assertEqual(brain["ranged_pointer_lines"]["observed_total"], 18)
        self.assertAlmostEqual(delivery["brain"]["ranged_pointer_share"], 0.6)
        self.assertEqual(brain["related_use_items"]["observed_total"], 6)
        self.assertEqual(brain["related_use_pointer_lines"]["observed_total"], 6)
        self.assertEqual(off["delivered_contexts"]["observed_total"], 0)
        self.assertEqual(off["read_result_bytes"]["observed_total"], 6 * 4 * 30_000)
        self.assertEqual(brain["read_result_bytes"]["observed_total"], 6 * 12_000)
        self.assertEqual(brain["ranged_read_calls"]["observed_total"], 12)
        self.assertEqual(delivery["brain"]["read_bytes_per_call"], 4_000)
        both = {item["task_id"]: item["metrics"] for item in summary["context_delivery"]["both_pass"]}
        self.assertEqual(both["All tasks"]["read_result_bytes"]["pair_count"], 4)
        self.assertAlmostEqual(both["All tasks"]["read_result_bytes"]["comparison"]["reduction_percent"], 90.0)
        self.assertAlmostEqual(both["All tasks"]["read_calls"]["comparison"]["reduction_percent"], 25.0)
        self.assertIn("## Context delivery and reads", readme)
        self.assertIn("| brain | 12 | 30 | 18 (60.0%) | 6 (6) | 18 | 12 | 72,000 |", readme)
        self.assertIn("| off | 0 | 0 | 0 (unavailable) | 0 (0) | 24 | 0 | 720,000 |", readme)
        self.assertIn("fewer Read bytes and fewer cache-read tokens", readme)
        # Read calls, ranged Read calls (none in off: no percentage), Read bytes, cache reads.
        self.assertIn("| All tasks | +25.0% (n=4) | undefined (n=4) | +90.0% (n=4) | +25.0% (n=4) |", readme)
        row = next(row for row in results["records"] if row["arm"] == "brain")
        self.assertEqual(set(row["context"]), {*reporter.CONTEXT_MEASURES, "read_measurement_complete"})

    def test_ready_suite_is_not_reported_as_a_draft_and_partial_matrix_is_in_progress(self):
        progress, results, _, readme, _ = self.publish(draft=False, complete=False)
        self.assertEqual(progress["status"], "in_progress")
        self.assertIs(results["suite"]["allow_draft_suite"], False)
        self.assertNotIn("--allow-draft-suite", readme)
        self.assertIn("Catalog status at collection: `ready_for_collection`", readme)
        self.assertIn("planned matrix is not complete", readme)

    def test_frozen_catalog_must_match_the_manifest_suite(self):
        data = build_heldout(self.root)
        catalog = json.loads((data / "protocol/suite/catalog.json").read_text())
        (data / "protocol/suite/catalog.json").write_text(json.dumps({**catalog, "suite_id": "other"}))
        with self.assertRaisesRegex(ValueError, "does not match the manifest suite"):
            reporter.generate_report(data, self.root / "published")


class HeldoutImportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        data = build_heldout(root)
        reporter.generate_report(data, root / "published")
        self.path = root / "published/results.json"
        self.report = importer.load_report(self.path)

    def payloads(self, report=None):
        return importer.build_payloads(self.report if report is None else report, "heldout-2026", "/public/results.json")

    def test_full_matrix_imports_with_suite_and_context_metrics(self):
        rows = self.payloads()
        self.assertEqual(len(rows), 12)
        self.assertEqual({row["task_id"] for row in rows}, set(TASKS))
        for row in rows:
            self.assertEqual(row["metrics"]["suite"], {"suite_id": SUITE_ID, "held_out": True, "allow_draft_suite": True})
            self.assertIn("read_result_bytes", row["metrics"]["context"])
        brain = next(row for row in rows if row["mode"] == "brain" and row["success"])
        self.assertEqual(brain["metrics"]["context"]["ranged_pointer_lines"], 3)
        self.assertEqual(brain["metrics"]["context"]["read_result_bytes"], 12_000)
        stdout = io.StringIO()
        with redirect_stdout(stdout):
            code = importer.main(["--report", str(self.path), "--benchmark-id", "heldout-2026"])
        self.assertEqual(code, 0)
        summary = json.loads(stdout.getvalue())
        self.assertEqual((summary["validated_records"], summary["suite"]["suite_id"]), (12, SUITE_ID))
        self.assertEqual(summary["outcomes"], {"pass": 10, "fail": 1, "infrastructure_error": 1})

    def test_controls_bind_the_draft_flag_and_grader_proof(self):
        before = self.payloads()
        for field, value in (("grader_preflight_sha256", "9" * 64), ("allow_draft_suite", False)):
            changed = deepcopy(self.report)
            changed["manifest"][field] = value
            if field == "allow_draft_suite":
                with self.assertRaisesRegex(importer.ValidationError, "Draft-suite flag differs"):
                    self.payloads(changed)
                changed["suite"]["allow_draft_suite"] = False
                del changed["manifest"]["suite_status"]
            self.assertNotEqual(self.payloads(changed)[0]["config_hash"], before[0]["config_hash"])
        missing = deepcopy(self.report)
        del missing["manifest"]["grader_preflight_sha256"]
        with self.assertRaises(importer.ValidationError):
            self.payloads(missing)

    def test_grader_files_must_be_the_frozen_suite_files(self):
        for mutation in ("digest", "missing", "suite_block"):
            changed = deepcopy(self.report)
            if mutation == "digest":
                path = f"grading/{TASKS[0]}/grade.py"
                for row in changed["records"]:
                    if row["task_id"] == TASKS[0]:
                        row["grader_manifest"][path] = "0" * 64
            elif mutation == "missing":
                for row in changed["records"]:
                    row["grader_manifest"].pop(f"grading/{row['task_id']}/preflight.py")
            else:
                del changed["suite"]
            with self.subTest(mutation=mutation), self.assertRaises(importer.ValidationError):
                self.payloads(changed)

    def test_matrix_must_match_the_suite_plan(self):
        for mutation in (lambda report: report["records"].pop(),
                         lambda report: report["suite"]["task_ids"].append("h3_unplanned"),
                         lambda report: report["suite"].update(repetitions=2),
                         lambda report: report["records"][0]["context"].update(ranged_read_calls=99)):
            changed = deepcopy(self.report)
            mutation(changed)
            with self.subTest(mutation=mutation), self.assertRaises(importer.ValidationError):
                self.payloads(changed)


class LegacyByteIdentityTests(unittest.TestCase):
    def test_v1_and_v2_report_and_import_outputs_are_unchanged(self):
        golden = observation.read_golden()
        observed = json.loads(json.dumps(observation.observe(reporter, importer), sort_keys=True))
        self.assertEqual(set(observed), set(golden))
        for name in golden:
            for part in golden[name]:
                for key in golden[name][part]:
                    with self.subTest(collection=name, part=part, key=key):
                        self.assertEqual(observed[name][part][key], golden[name][part][key])


if __name__ == "__main__":
    unittest.main()
