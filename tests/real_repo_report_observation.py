"""Observe the v1 and v2 report and import outputs for deterministic synthetic collections.

tests/fixtures/real_repo_report_v1v2.json.gz holds these observations (gzip JSON) as produced by
the v1/v2-only report and importer (commit 67d9e41). The suite-generic versions
must reproduce them byte for byte, apart from the report timestamp and the
analysis-provenance digests of the scripts themselves.

Usage (regenerates the golden; review the diff before committing):
    python3 -B tests/real_repo_report_observation.py --write
"""
import argparse
import contextlib
import gzip
import hashlib
import io
import json
from pathlib import Path
import shutil
import sys
import tempfile

TESTS = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS.parent / "scripts"))
sys.dont_write_bytecode = True
GOLDEN = TESTS / "fixtures/real_repo_report_v1v2.json.gz"
MODEL = "claude-opus-4-8[1m]"
TASKS = ("r01_guide_hot_path", "r02_lean_task_status", "r05_test_env_isolation", "r06_onboarding_outage")
GRADER_FILES = {
    "real-repo-v1": {TASKS[0]: ("grade.py", "hidden_tests.rs"), TASKS[1]: ("grade.py", "preflight.py", "preflight.json", "v1_sha256.json"),
                     TASKS[2]: ("grade.py", "hidden_tests.rs"), TASKS[3]: ("grade.py", "onboarding.private.test.ts", "preflight.json")},
    "real-repo-v2": {TASKS[0]: ("grade.py", "hidden_tests.rs", "preflight.py", "preflight.json"),
                     TASKS[1]: ("grade.py", "preflight.py", "preflight.json", "v1_sha256.json", "v1_semantic_manifest.json"),
                     TASKS[2]: ("grade.py", "hidden_tests.rs", "preflight.json"),
                     TASKS[3]: ("grade.py", "onboarding.private.test.ts", "preflight.py", "preflight.json")},
}


def hexdigest(*parts):
    return hashlib.sha256("/".join(map(str, parts)).encode()).hexdigest()


def trace_events(task_index, repetition, arm, reads=3):
    """A small Claude stream-JSON trace with Read, Grep and Bash calls and their results."""
    events = [{"type": "system", "subtype": "init", "session_id": f"session-{task_index}-{repetition}-{arm}", "cwd": "/private/work"}]
    for read in range(reads + (arm == "off")):
        identity = f"toolu_read_{read}"
        size = 400 * (read + 1) + (900 if arm == "off" else 0)
        payload = {"file_path": f"/private/work/src/file_{read}.rs"}
        if arm == "brain" and read == 0:
            payload.update(offset=1780, limit=40)
        events.append({"type": "assistant", "message": {"model": MODEL, "content": [{"type": "tool_use", "id": identity, "name": "Read", "input": payload}]}})
        events.append({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": identity, "content": "x" * size}]}})
    events.append({"type": "assistant", "message": {"model": MODEL, "content": [{"type": "tool_use", "id": "toolu_grep", "name": "Grep", "input": {"pattern": "PRIVATE"}}]}})
    events.append({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_grep", "content": [{"type": "text", "text": "y" * 50}]}]}})
    events.append({"type": "result", "subtype": "success", "is_error": False, "num_turns": 6})
    return events


def record(suite, task_index, repetition, arm, ordinal):
    task = TASKS[task_index]
    identity = f"{task}-r{repetition}-{arm}"
    base = 1000 * (task_index + 1) + 100 * repetition + (0 if arm == "off" else -150)
    inputs = {"input_tokens": base, "cache_creation_input_tokens": 2 * base, "cache_read_input_tokens": 10 * base}
    output = base // 4
    value = {
        "id": identity, "task_id": task, "repetition": repetition, "arm": arm,
        "started_at": f"2026-09-25T{4 + ordinal // 60:02d}:{ordinal % 60:02d}:00.250000+00:00",
        "grade": {"status": "pass", "checks": [{"name": "behavior_preserved", "passed": True}, {"name": "contract", "status": "pass"}]},
        "agent_wall_seconds": 300.5 + 10 * task_index + repetition + (0 if arm == "off" else -25.25),
        "setup_seconds": 40.0 + task_index, "grading_seconds": 90.0 + repetition,
        "agent_exit_code": 0, "grader_exit_code": 0, "agent_timed_out": False,
        "metrics": {**inputs, "output_tokens": output, "processed_input_tokens": sum(inputs.values()),
                    "total_tokens": sum(inputs.values()) + output, "tool_calls": 20 + task_index - (arm == "brain"),
                    "tool_calls_complete": True, "num_turns": 30 + repetition, "cost_usd_estimate": round(base / 1000, 3),
                    "is_error": False, "trace_complete": True, "usage_complete": True, "result_subtype": "success",
                    "models": [MODEL], "model_usage": {MODEL: {"inputTokens": base, "outputTokens": output, "costUSD": round(base / 1000, 3)}},
                    "counts_by_tool": {"Read": 4, "Grep": 1, "Bash": 15 + task_index - (arm == "brain")},
                    "permission_denials": [], "tool_use_ids": ["PRIVATE"],
                    "diagnostics": {"trace_sha256": "PRIVATE", "result_count": 1, "malformed_line_numbers": [],
                                    "tool_blocks_without_ids": 0, "conflicting_tool_ids": []}},
        "delivery": ({"emission_receipts": 2, "compiled_runs": 3, "compiled_estimated_tokens": 900,
                      "delivered_compiled_estimated_tokens": 600, "source_items": 7,
                      "delivered_run_ids": ["PRIVATE"], "delivered_sessions": ["PRIVATE"]}
                     if arm == "brain" else {"emission_receipts": 0, "compiled_runs": 0}),
        "prompt_sha256": hexdigest("prompt", task), "composed_prompt_sha256": hexdigest("composed", task),
        "snapshot_sha256": hexdigest("snapshot", task),
        "grader_manifest": {f"grading/{task}/{name}": hexdigest("grader", task, name) for name in GRADER_FILES[suite][task]},
        "validity_issues": [], "patch": "PRIVATE", "infrastructure_error": None,
    }
    # Deterministic outcome variety; every case stays importable.
    if (task_index, repetition, arm) == (0, 2, "off"):
        value["grade"] = {"status": "fail", "checks": [{"name": "behavior_preserved", "passed": False}, {"name": "contract", "passed": True}]}
        value["grader_exit_code"] = 1
    if (task_index, repetition, arm) == (1, 1, "brain"):
        value["agent_exit_code"] = 1
        value["metrics"].update(is_error=True, result_subtype="error_during_execution")
    if (task_index, repetition, arm) == (1, 3, "off"):
        value["agent_timed_out"] = True
        value["metrics"]["tool_calls_complete"] = False
        value["grade"] = {"status": "fail", "checks": [{"name": "behavior_preserved", "passed": False}, {"name": "contract", "passed": True}]}
        value["grader_exit_code"] = 1
    if (task_index, repetition, arm) == (2, 2, "brain"):
        value["metrics"]["models"] = ["claude-other"]
        value["metrics"]["model_usage"] = {"claude-other": {"inputTokens": base}}
        value["behavioral_grade"] = value["grade"]
        value["grade"] = {"status": "infrastructure_error", "issues": ["model_drift"]}
        value["validity_issues"] = ["model_drift"]
    if (task_index, repetition, arm) == (3, 1, "off") and suite == "real-repo-v2":
        value["grade"] = {"status": "fail", "checks": [{"name": "behavior_preserved", "passed": True}, {"name": "ambiguous_status", "status": "fail"}]}
        value["grader_exit_code"] = 1
    return value


def build_collection(root, suite, *, complete=True):
    """A synthetic collection root as the runner leaves it, with private sentinels that must not publish."""
    data = root / f"{suite}-{'complete' if complete else 'partial'}"
    data.mkdir(parents=True)
    manifest = {
        "schema_version": 1, "condition": "source-only native hooks; no MCP; no learned memories", "model": MODEL,
        "effort": "high", "claude_version": "2.1.212 (Claude Code)", "engram_sha256": hexdigest("engram"),
        "daemon_sha256": hexdigest("daemon"), "runner_sha256": hexdigest("runner"), "metrics_sha256": hexdigest("metrics"),
        "repetitions": 3, "agent_limit_seconds": 1200, "grading_limit_seconds": 1800, "cost_cap_usd_estimate": 8,
        "platform_dependencies": "same frozen pilot base dependencies in both arms",
        "private_dependencies": "Rust registry-only cache; no user Git config; package installation disabled",
        "rustc": "rustc 1.96.0", "python": "3.14.7", "node": "v26.5.1", "pnpm": "10.18.0",
        "known_scope_limits": ["Synthetic scope limit one.", "Synthetic scope limit two."],
        "engram_binary": "PRIVATE", "controls": {"suite_files": {"PRIVATE": "PRIVATE"}},
    }
    if suite == "real-repo-v2":
        manifest.update(suite=suite, controls_sha256=hexdigest("controls"), runner_library_sha256=hexdigest("library"),
                        claude_sha256=hexdigest("claude"), runtime_preflight_sha256=hexdigest("runtime"))
    (data / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    for index, task in enumerate(TASKS):
        fixture = data / "fixtures" / task
        fixture.mkdir(parents=True)
        (fixture / "ready.json").write_text(json.dumps({
            "task_id": task, "base_commit": hexdigest("base", task)[:40], "fixture_head": hexdigest("head", task)[:40],
            "snapshot_sha256": hexdigest("snapshot", task), "setup_seconds": 10.5 + index, "index_seconds": 2.25,
            "dependency_seconds": 0 if suite == "real-repo-v2" else 60 + index, "private_dependency_seconds": 12.5,
            "source_counts": {"nodes": 1000 + index, "memories": 0, "tasks": 0, "context_runs": 0},
            "removed_paths": [".env"] if index == 0 else [], "lockfile_sha256": {"Cargo.lock": hexdigest("lock", task)},
            "project_id": "PRIVATE", "index": {"root": "PRIVATE"},
        }))
    rows = []
    for repetition in (1, 2, 3):
        for index in range(len(TASKS)):
            for arm in (("off", "brain") if (index + repetition) % 2 else ("brain", "off")):
                rows.append(record(suite, index, repetition, arm, len(rows)))
    if not complete:
        rows = rows[:9]
    for row in rows:
        trial = data / "trials" / row["id"]
        trial.mkdir(parents=True)
        index = TASKS.index(row["task_id"])
        (trial / "trace.jsonl").write_text("".join(json.dumps(event) + "\n" for event in
                                                   trace_events(index, row["repetition"], row["arm"])))
    adjudicated = next((row for row in rows if row["id"] == f"{TASKS[3]}-r1-off"), None)
    if suite == "real-repo-v2" and adjudicated is not None:
        grade = json.dumps(adjudicated["grade"]).encode()
        (data / "trials" / adjudicated["id"] / "grade.json").write_bytes(grade)
        (data / "adjudications.json").write_text(json.dumps({"schema_version": 1, "adjudications": {adjudicated["id"]: {
            "grade_sha256": hashlib.sha256(grade).hexdigest(), "failed_check": "ambiguous_status",
            "reason": "ambiguous_grader_contract"}}}))
    (data / "records.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows))
    separate = record(suite, 0, 1, "off", 90)
    separate.pop("id")
    separate["grade"] = {"status": "infrastructure_error"}
    separate["metrics"].update(total_tokens=None, tool_calls_complete=False)
    (data / "infrastructure_attempts.jsonl").write_text(json.dumps(separate) + "\n")
    smoke = record(suite, 0, 1, "brain", 91)
    smoke.pop("id")
    smoke["task_id"] = "environment_preflight"
    (data / "preflight_runs.jsonl").write_text(json.dumps(smoke) + "\n")
    return data


def normalize(value):
    value = json.loads(json.dumps(value))
    if isinstance(value, dict):
        if "generated_at" in value:
            value["generated_at"] = "<generated_at>"
        if "analysis_provenance" in value:
            value["analysis_provenance"] = {name: "<digest>" for name in value["analysis_provenance"]}
    return value


def observe_report(reporter, data, output):
    progress = reporter.generate_report(data, output)
    return {"progress": progress,
            "results": normalize(json.loads((output / "results.json").read_text())),
            "summary": normalize(json.loads((output / "summary.json").read_text())),
            "readme": (output / "README.md").read_text().splitlines()}


def observe_import(importer, results_path):
    report = importer.load_report(results_path)
    payloads = importer.build_payloads(report, "golden-import", "/public/results.json")
    stdout = io.StringIO()
    with contextlib.redirect_stdout(stdout):
        code = importer.main(["--report", str(results_path), "--benchmark-id", "golden-import"])
    return {"payloads_canonical": [importer.canonical(payload) for payload in payloads], "exit_code": code,
            "dry_run_summary": stdout.getvalue()}


def observe(reporter, importer):
    root = Path(tempfile.mkdtemp(prefix="report-observation-")).resolve()
    try:
        observed = {}
        for suite, complete in (("real-repo-v1", True), ("real-repo-v2", True), ("real-repo-v2", False)):
            data = build_collection(root, suite, complete=complete)
            output = root / "published" / data.name
            observed[data.name] = {"report": observe_report(reporter, data, output)}
            if complete:
                observed[data.name]["import"] = observe_import(importer, output / "results.json")
        return observed
    finally:
        shutil.rmtree(root, ignore_errors=True)


def write_golden(value):
    GOLDEN.parent.mkdir(parents=True, exist_ok=True)
    with GOLDEN.open("wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=0, filename="") as stream:
        stream.write((json.dumps(value, indent=1, sort_keys=True) + "\n").encode())


def read_golden():
    return json.loads(gzip.decompress(GOLDEN.read_bytes()))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--write", action="store_true", help="write the observation to the golden file")
    args = parser.parse_args()
    import import_real_repo_pilot as importer
    import report_real_repo_pilot as reporter
    value = observe(reporter, importer)
    if args.write:
        write_golden(value)
    print(json.dumps({name: {part: (entry["report"]["progress"]["status"] if part == "report" else len(entry[part]["payloads_canonical"]))
                             for part in entry} for name, entry in value.items()}))


if __name__ == "__main__":
    main()
