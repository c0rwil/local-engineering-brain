#!/usr/bin/env python3
"""Validate a completed public pilot, then optionally import evaluation records.

This imports benchmark evidence only. It never writes memories or usage events.
The default is a dry run; --apply uses the daemon's Unix HTTP evaluation API.
"""

from __future__ import annotations

import argparse
from collections import Counter
from datetime import datetime, timezone
import hashlib
import http.client
import json
import math
import os
from pathlib import Path
import re
import socket
import sys
import uuid

from real_repo_metrics import RECORD_CLASSIFICATION_REVISION, classify_record

TASKS = ("r01_guide_hot_path", "r02_lean_task_status", "r05_test_env_isolation", "r06_onboarding_outage")
ARMS = ("off", "brain")
EXPECTED_KEYS = {(task, repetition, arm) for task in TASKS for repetition in (1, 2, 3) for arm in ARMS}
IMPORT_NAMESPACE = uuid.UUID("f24f237c-971f-5e16-8809-38fafc5070c0")
IMPORT_VERSION = 1
ENDPOINT = "/api/v1/evaluation/runs"
MAX_REPORT_BYTES = 8 * 1024 * 1024
COUNT_FIELDS = (
    "input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens",
    "processed_input_tokens", "total_tokens", "tool_calls", "num_turns",
)
DELIVERY_FIELDS = ("emission_receipts", "compiled_runs", "compiled_estimated_tokens", "delivered_compiled_estimated_tokens", "source_items")
MANIFEST_TEXT = (
    "condition", "model", "effort", "claude_version", "platform_dependencies",
    "private_dependencies", "rustc", "python", "node", "pnpm",
)
MANIFEST_HASHES = ("engram_sha256", "daemon_sha256", "runner_sha256", "metrics_sha256")
KNOWN_ISSUES = {"model_drift", "no_delivery_receipt", "agent_budget_exhausted", "agent_runtime_error", "ambiguous_grader_contract", "collection_control_drift", "shell_spawn_argument_limit"}
GRADER_FILES = {
    TASKS[0]: {"grade.py", "hidden_tests.rs"},
    TASKS[1]: {"grade.py", "preflight.py", "preflight.json", "v1_sha256.json"},
    TASKS[2]: {"grade.py", "hidden_tests.rs"},
    TASKS[3]: {"grade.py", "onboarding.private.test.ts", "preflight.json"},
}
V2_HASHES = ("controls_sha256", "runner_library_sha256", "claude_sha256", "runtime_preflight_sha256")
GRADER_FILES_V2 = {
    TASKS[0]: {"grade.py", "hidden_tests.rs", "preflight.py", "preflight.json"},
    TASKS[1]: {"grade.py", "preflight.py", "preflight.json", "v1_sha256.json", "v1_semantic_manifest.json"},
    TASKS[2]: {"grade.py", "hidden_tests.rs", "preflight.json"},
    TASKS[3]: {"grade.py", "onboarding.private.test.ts", "preflight.py", "preflight.json"},
}
LEGACY_SUITES = ("real-repo-v1", "real-repo-v2")
# Catalog-selected suites also bind their grader recalibration proof.
SUITE_HASHES = ("grader_preflight_sha256",)
CONTEXT_FIELDS = (
    "read_calls", "ranged_read_calls", "read_results", "read_error_results", "read_result_bytes",
    "delivered_contexts", "pointer_lines", "ranged_pointer_lines", "direct_pointer_lines",
    "neighbor_pointer_lines", "related_use_pointer_lines", "related_use_items",
)
SAFE_TASK = re.compile(r"[a-z0-9][a-z0-9_-]{0,127}")


class ValidationError(ValueError):
    """A public report cannot be faithfully imported."""


class TransportError(RuntimeError):
    """An HTTP failure whose message contains no server response contents."""


def require(condition, message):
    if not condition:
        raise ValidationError(message)


def mapping(value, field):
    require(isinstance(value, dict), f"{field} must be an object")
    return value


def number(value, field, *, integer=False, required=False):
    if value is None and not required:
        return None
    valid = isinstance(value, int) if integer else isinstance(value, (int, float))
    require(valid and not isinstance(value, bool), f"{field} must be a measured nonnegative number")
    require(0 <= value <= (2**53 - 1) and math.isfinite(value), f"{field} is outside the supported numeric range")
    return value


def boolean(value, field, *, required=False):
    require(isinstance(value, bool) or (value is None and not required), f"{field} must be a boolean")
    return value


def digest_value(value, field, length=64):
    require(isinstance(value, str) and re.fullmatch(f"[0-9a-f]{{{length}}}", value) is not None, f"{field} must be a lowercase digest")
    return value


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True, allow_nan=False)


def fingerprint(value):
    return hashlib.sha256(canonical(value).encode()).hexdigest()


def outcome(grade, field="grade"):
    status = mapping(grade, field).get("status")
    require(isinstance(status, str), f"{field}.status is missing")
    status = {"infra": "infrastructure_error", "infra_error": "infrastructure_error"}.get(status, status)
    require(status in {"pass", "fail", "infrastructure_error"}, f"{field}.status is not a classified outcome")
    return status


def exit_code(value, field):
    require(value is None or (type(value) is int and -(2**31) <= value < 2**31), f"{field} must be an integer exit code or null")
    return value


def timestamp_ms(value):
    require(isinstance(value, str), "started_at must be an original timestamp")
    try:
        stamp = datetime.fromisoformat(value)
    except ValueError:
        raise ValidationError("started_at is not an ISO timestamp") from None
    require(stamp.tzinfo is not None, "started_at must include a timezone")
    elapsed = stamp - datetime(1970, 1, 1, tzinfo=timezone.utc)
    result = elapsed.days * 86_400_000 + elapsed.seconds * 1000 + elapsed.microseconds // 1000
    require(0 <= result < 2**63, "started_at is outside the supported range")
    return result


def suite_spec(report):
    """Tasks, repetitions and grader files of the collection.

    v1 and v2 keep their fixed tables. Other suites use the suite block the
    report published from its frozen catalog and manifest: task identities,
    repetitions and the frozen digest of every per-task grader file.
    """
    manifest = report.get("manifest")
    suite = manifest.get("suite", "real-repo-v1") if isinstance(manifest, dict) else "real-repo-v1"
    if suite in LEGACY_SUITES:
        files = GRADER_FILES_V2 if suite == "real-repo-v2" else GRADER_FILES
        return {"suite": suite, "legacy": True, "tasks": TASKS, "repetitions": 3,
                "grader_files": {task: {f"grading/{task}/{name}": None for name in files[task]} for task in TASKS}}
    declared = mapping(report.get("suite"), "suite")
    require(declared.get("suite_id") == suite, "Report suite does not match its manifest")
    tasks = declared.get("task_ids")
    require(isinstance(tasks, list) and tasks and len(set(tasks)) == len(tasks)
            and all(isinstance(task, str) and SAFE_TASK.fullmatch(task) for task in tasks), "Suite task identities are invalid")
    repetitions = declared.get("repetitions")
    require(type(repetitions) is int and 1 <= repetitions <= 100, "Suite repetitions are invalid")
    grader = mapping(declared.get("grader_files"), "suite.grader_files")
    require(set(grader) == set(tasks), "Suite grader files must cover exactly its tasks")
    files = {}
    for task in tasks:
        entries = mapping(grader[task], f"suite.grader_files.{task}")
        require(entries and all(isinstance(path, str) and path.startswith(f"grading/{task}/") and len(Path(path).parts) == 3
                                and ".." not in Path(path).parts for path in entries), "Suite grader file paths are invalid")
        files[task] = {path: digest_value(value, "suite grader digest") for path, value in entries.items()}
    return {"suite": suite, "legacy": False, "tasks": tuple(tasks), "repetitions": repetitions, "grader_files": files,
            "held_out": boolean(declared.get("held_out"), "suite.held_out", required=True),
            "allow_draft_suite": boolean(declared.get("allow_draft_suite"), "suite.allow_draft_suite", required=True)}


def suite_controls(manifest, spec):
    """Controls of a catalog-selected suite: follow-up hashes, grader proof and draft flag."""
    require(type(manifest.get("repetitions")) is int and manifest["repetitions"] == spec["repetitions"],
            "Manifest repetitions differ from the suite")
    controls = {"schema_version": 1, "repetitions": spec["repetitions"], "suite": spec["suite"]}
    for field in (*V2_HASHES, *SUITE_HASHES):
        controls[field] = digest_value(manifest.get(field), f"manifest.{field}")
    draft = manifest.get("allow_draft_suite", False)
    require(draft is True or draft is False, "manifest.allow_draft_suite must be a boolean")
    require(draft == spec["allow_draft_suite"], "Draft-suite flag differs between manifest and suite")
    controls["allow_draft_suite"] = draft
    if draft:
        status = manifest.get("suite_status")
        require(isinstance(status, str) and re.fullmatch(r"[a-z][a-z0-9_]{0,79}", status) is not None,
                "A draft collection must record its catalog status")
        controls["suite_status"] = status
    return controls


def manifest_controls(report, spec=None):
    manifest = mapping(report.get("manifest"), "manifest")
    require(type(manifest.get("schema_version")) is int and manifest["schema_version"] == 1, "Unsupported manifest schema")
    if spec is not None and not spec["legacy"]:
        controls = suite_controls(manifest, spec)
        return common_manifest_controls(manifest, controls)
    require(type(manifest.get("repetitions")) is int and manifest["repetitions"] == 3, "Pilot must contain three repetitions")
    controls = {"schema_version": 1, "repetitions": 3}
    suite = manifest.get("suite", "real-repo-v1")
    require(suite in ("real-repo-v1", "real-repo-v2"), "Unsupported benchmark suite")
    if suite == "real-repo-v2":
        controls["suite"] = suite
        for field in V2_HASHES:
            controls[field] = digest_value(manifest.get(field), f"manifest.{field}")
    return common_manifest_controls(manifest, controls)


def common_manifest_controls(manifest, controls):
    for field in MANIFEST_TEXT:
        value = manifest.get(field)
        require(isinstance(value, str) and 0 < len(value.strip()) <= 4096, f"manifest.{field} is missing or invalid")
        controls[field] = value
    for field in MANIFEST_HASHES:
        controls[field] = digest_value(manifest.get(field), f"manifest.{field}")
    for field in ("agent_limit_seconds", "grading_limit_seconds", "cost_cap_usd_estimate"):
        controls[field] = number(manifest.get(field), f"manifest.{field}", integer=field.endswith("seconds"), required=True)
        require(controls[field] > 0, f"manifest.{field} must be positive")
    return controls


def fixture_controls(report, tasks=TASKS):
    fixtures = report.get("fixtures")
    require(isinstance(fixtures, list) and len(fixtures) == len(tasks),
            "Pilot must contain four fixture controls" if tasks == TASKS else f"Collection must contain {len(tasks)} fixture controls")
    result = {}
    for fixture in fixtures:
        fixture = mapping(fixture, "fixture")
        task = fixture.get("task_id")
        require(isinstance(task, str) and task in tasks and task not in result, "Unexpected or duplicate fixture task")
        control = {field: digest_value(fixture.get(field), f"fixture.{field}", 64 if field == "snapshot_sha256" else 40)
                   for field in ("base_commit", "fixture_head", "snapshot_sha256")}
        locks = mapping(fixture.get("lockfile_sha256"), "fixture.lockfile_sha256")
        control["lockfile_sha256"] = {}
        for name, value in locks.items():
            require(isinstance(name, str) and re.fullmatch(r"[a-zA-Z0-9_./-]{1,180}", name) is not None
                    and not name.startswith("/") and ".." not in Path(name).parts, "Invalid lockfile control name")
            control["lockfile_sha256"][name] = digest_value(value, "lockfile hash")
        result[task] = control
    return result


def context_metrics(context):
    """Published context-delivery and Read counts; missing measurements stay null."""
    context = mapping(context, "context")
    result = {field: number(context.get(field), f"context.{field}", integer=True) for field in CONTEXT_FIELDS}
    result["read_measurement_complete"] = boolean(context.get("read_measurement_complete"), "context.read_measurement_complete", required=True)
    for part, whole in (("ranged_pointer_lines", "pointer_lines"), ("ranged_read_calls", "read_calls"), ("read_results", "read_calls")):
        require(result[part] is None or result[whole] is None or result[part] <= result[whole], f"context.{part} exceeds {whole}")
    return result


def measured_metrics(record):
    raw = mapping(record.get("metrics"), "metrics")
    measured = {field: number(raw.get(field), f"metrics.{field}", integer=True) for field in COUNT_FIELDS}
    measured["total_cost_usd"] = number(raw.get("cost_usd_estimate"), "metrics.cost_usd_estimate")
    measured["agent_wall_seconds"] = number(record.get("agent_wall_seconds"), "agent_wall_seconds", required=True)
    inputs = [measured[field] for field in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")]
    processed = sum(inputs) if all(value is not None for value in inputs) else None
    if measured["processed_input_tokens"] is not None and processed is not None:
        require(measured["processed_input_tokens"] == processed, "processed_input_tokens does not match reported components")
    # Derived totals are independently optional. Reconcile available evidence,
    # but never fill an absent reported measurement from other components.
    reported_input = measured["processed_input_tokens"]
    known_input = reported_input if reported_input is not None else processed
    total = known_input + measured["output_tokens"] if known_input is not None and measured["output_tokens"] is not None else None
    if measured["total_tokens"] is not None and total is not None:
        require(measured["total_tokens"] == total, "total_tokens does not match reported components")
    return measured


def build_payloads(report, benchmark_id, report_path):
    """Return the full planned matrix (24 records for v1/v2) as validated native records, without opening any socket.

    Missing optional measurements remain null. A missing mandatory native wall
    measurement is an error. IDs intentionally exclude outcomes and report time:
    reanalysis under the same label conflicts instead of silently duplicating.
    """
    report = mapping(report, "report")
    require(isinstance(benchmark_id, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,95}", benchmark_id) is not None,
            "benchmark-id must be a short label using letters, digits, dot, underscore or hyphen")
    require(type(report.get("schema_version")) is int and report["schema_version"] == 1, "Unsupported report schema")
    require(report.get("record_classification_revision") == RECORD_CLASSIFICATION_REVISION, "Unsupported classification revision")
    progress = mapping(report.get("progress"), "progress")
    require(progress.get("status") == "complete", "Pilot is not complete")
    spec = suite_spec(report)
    tasks = spec["tasks"]
    expected_keys = {(task, repetition, arm) for task in tasks for repetition in range(1, spec["repetitions"] + 1) for arm in ARMS}
    planned = len(expected_keys)
    for field in ("planned_runs", "started_trials", "coding_runs_started", "completed_records"):
        require(type(progress.get(field)) is int and progress[field] == planned, f"progress.{field} must equal {planned}")
    require(type(progress.get("in_progress_trials")) is int and progress["in_progress_trials"] == 0, "Pilot has active attempts")
    controls = manifest_controls(report, spec)
    fixtures = fixture_controls(report, tasks)
    for field in ("preflight_runs", "infrastructure_attempts"):
        require(isinstance(report.get(field, []), list), f"{field} must be a separate attempt list")
    rows = report.get("records")
    require(isinstance(rows, list) and len(rows) == planned, f"Pilot must contain exactly {planned} scored records")
    keyed = {}
    for row in rows:
        row = mapping(row, "record")
        task, repetition, arm = row.get("task_id"), row.get("repetition"), row.get("arm")
        require(isinstance(task, str) and type(repetition) is int and isinstance(arm, str), "Invalid trial key types")
        key = (task, repetition, arm)
        require(key in expected_keys and key not in keyed, "Unexpected or duplicate scored trial key")
        timestamp_ms(row.get("started_at"))
        keyed[key] = row
    require(set(keyed) == expected_keys, "Pilot matrix is incomplete")
    collection = fingerprint([{"task_id": key[0], "repetition": key[1], "arm": key[2], "started_at": keyed[key].get("started_at")}
                              for key in sorted(keyed)])
    task_controls, payloads = {}, []
    artifact = str(Path(report_path).expanduser().resolve())
    require(Path(artifact).name == "results.json", "Artifact must point to the public results.json report")
    for ordinal, key in enumerate(sorted(keyed), 1):
        task, repetition, arm = key
        row = keyed[key]
        require(row.get("record_classification_revision") == RECORD_CLASSIFICATION_REVISION, "Record classification revision mismatch")
        started = timestamp_ms(row.get("started_at"))
        task_control = {field: digest_value(row.get(field), field) for field in ("prompt_sha256", "composed_prompt_sha256", "snapshot_sha256")}
        require(task_control["snapshot_sha256"] == fixtures[task]["snapshot_sha256"], "Record snapshot differs from fixture")
        grader = mapping(row.get("grader_manifest"), "grader_manifest")
        expected_files = spec["grader_files"][task]
        require(set(grader) == set(expected_files), "Grader control file set is incomplete or unexpected")
        task_control["grader_manifest"] = {name: digest_value(value, "grader digest") for name, value in grader.items()}
        if not spec["legacy"]:
            require(task_control["grader_manifest"] == expected_files, "Trial grader files differ from the frozen suite")
        if task in task_controls:
            require(task_control == task_controls[task], "Frozen task controls changed between trials")
        task_controls[task] = task_control
        status = outcome(row.get("grade"))
        behavior = outcome(row.get("behavioral_grade", row.get("grade")), "behavioral_grade")
        measured = measured_metrics(row)
        raw = row["metrics"]
        flags = {field: boolean(raw.get(field), f"metrics.{field}") for field in ("is_error", "trace_complete", "usage_complete", "tool_calls_complete")}
        timed_out = boolean(row.get("agent_timed_out"), "agent_timed_out", required=True)
        partial = boolean(row.get("usage_may_be_partial"), "usage_may_be_partial", required=True)
        setup_exit = exit_code(row.get("setup_exit_code"), "setup_exit_code")
        agent_exit = exit_code(row.get("agent_exit_code"), "agent_exit_code")
        grader_exit = exit_code(row.get("grader_exit_code"), "grader_exit_code")
        subtype = raw.get("result_subtype")
        require(subtype is None or (isinstance(subtype, str) and re.fullmatch(r"[a-z][a-z0-9_]{0,79}", subtype) is not None), "Invalid result_subtype")
        issues = row.get("validity_issues")
        require(isinstance(issues, list) and all(isinstance(issue, str) and issue in KNOWN_ISSUES for issue in issues)
                and len(issues) == len(set(issues)), "Unknown or duplicate validity issue")
        model_usage = mapping(raw.get("model_usage", {}), "metrics.model_usage")
        models = raw.get("models", [])
        require(isinstance(models, list) and all(isinstance(model, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.\[\]-]{0,127}", model) is not None for model in models)
                and all(isinstance(model, str) for model in model_usage) and models == sorted(model_usage), "Reported model identifiers are invalid or inconsistent")
        if models and models != [controls["model"]]:
            require("model_drift" in issues and status == "infrastructure_error", "Reported model drift must remain unscorable")
        delivery_raw = mapping(row.get("delivery", {}), "delivery")
        delivery = {field: number(delivery_raw.get(field), f"delivery.{field}", integer=True) for field in DELIVERY_FIELDS}
        if status == "pass":
            require(models == [controls["model"]], "A clean completion must have pinned final-model evidence")
            if arm == "brain":
                require(delivery["emission_receipts"] is not None and delivery["emission_receipts"] > 0,
                        "A clean Brain completion must have measured context delivery")
        if arm == "brain" and delivery["emission_receipts"] == 0:
            require(status == "infrastructure_error" and "no_delivery_receipt" in issues, "Absent Brain delivery must remain unscorable")
        classified = classify_record(row)
        require(outcome(classified["grade"]) == status and set(classified.get("validity_issues", [])) == set(issues), "Record has not been consistently classified")
        require(classified.get("usage_may_be_partial", False) == partial, "Partial-usage flag disagrees with classification")
        if set(issues) & {"model_drift", "no_delivery_receipt", "ambiguous_grader_contract", "collection_control_drift", "shell_spawn_argument_limit"} or grader_exit not in (None, 0, 1) or setup_exit not in (None, 0):
            require(status == "infrastructure_error", "Invalid controls or grader infrastructure must remain infrastructure")
        tool_runtime = None
        if "tool_runtime_evidence" in row or "shell_spawn_argument_limit" in issues:
            evidence = mapping(row.get("tool_runtime_evidence"), "tool_runtime_evidence")
            require(set(evidence) == {"schema_version", "trace_sha256", "reason", "shell_tool_calls", "shell_spawn_failure_count"},
                    "Unsupported tool runtime evidence fields")
            require(type(evidence.get("schema_version")) is int and evidence["schema_version"] == 1
                    and evidence.get("reason") == "shell_spawn_argument_limit"
                    and "shell_spawn_argument_limit" in issues and status == "infrastructure_error"
                    and "behavioral_grade" in row, "Tool runtime evidence must preserve the original behavior and unscorable outcome")
            calls = number(evidence.get("shell_tool_calls"), "shell_tool_calls", integer=True, required=True)
            failures = number(evidence.get("shell_spawn_failure_count"), "shell_spawn_failure_count", integer=True, required=True)
            require(0 < failures <= calls and (measured["tool_calls"] is None or calls <= measured["tool_calls"]),
                    "Tool runtime evidence counts are inconsistent")
            tool_runtime = {**evidence, "trace_sha256": digest_value(evidence.get("trace_sha256"), "tool_runtime.trace_sha256")}
        budget = timed_out or (isinstance(subtype, str) and subtype.startswith("error_max_"))
        runtime = tool_runtime is not None or flags["is_error"] is True or agent_exit not in (None, 0) or (isinstance(subtype, str) and subtype.startswith("error_") and not budget)
        if runtime and not budget:
            require(status == "infrastructure_error", "Runtime errors must retain the native infrastructure outcome")
        if "agent_runtime_error" in issues:
            require(status == "infrastructure_error", "Runtime invalidity must remain infrastructure")
        if "agent_budget_exhausted" in issues:
            require(status != "pass" and budget, "Budget invalidity is inconsistent with the recorded attempt")
        require(status != "pass" or (behavior == "pass" and grader_exit in (None, 0) and not runtime and not budget), "Invalid clean completion")
        if runtime or budget:
            require("behavioral_grade" in row, "An interrupted or errored attempt must retain its behavioral verdict")
        adjudication = None
        if "adjudication" in row or "ambiguous_grader_contract" in issues:
            reviewed = mapping(row.get("adjudication"), "adjudication")
            require(type(reviewed.get("schema_version")) is int and reviewed["schema_version"] == 1, "Unsupported adjudication schema")
            require(reviewed.get("trial_id") == f"{task}-r{repetition}-{arm}", "Adjudication trial identity mismatch")
            require(reviewed.get("reason") == "ambiguous_grader_contract" and "ambiguous_grader_contract" in issues
                    and status == "infrastructure_error" and behavior == "fail" and "behavioral_grade" in row,
                    "Adjudication must preserve the failed behavior and unscorable outcome")
            adjudication = {
                "schema_version": 1, "trial_id": reviewed["trial_id"], "reason": "ambiguous_grader_contract",
                **{field: digest_value(reviewed.get(field), f"adjudication.{field}") for field in ("grade_sha256", "failed_check_sha256")},
            }
        model_hash = fingerprint({"collection": collection, **{field: controls[field] for field in ("model", "effort", "claude_version")}})
        config_hash = fingerprint({"import_version": IMPORT_VERSION, "classification": RECORD_CLASSIFICATION_REVISION,
                                   "collection": collection, "manifest": controls, "fixture": fixtures[task], "task": task_control})
        payloads.append({
            "run_id": "eval_" + str(uuid.uuid5(IMPORT_NAMESPACE, canonical([benchmark_id, task, repetition, arm]))),
            "benchmark_id": f"{benchmark_id}/{task}", "task_id": task, "mode": arm,
            "seed": repetition, "ordinal": ordinal, "model_hash": model_hash, "config_hash": config_hash,
            "brain_snapshot_hash": task_control["snapshot_sha256"], "success": status == "pass",
            "timed_out": timed_out, "setup_exit_code": setup_exit, "agent_exit_code": agent_exit,
            "validation_exit_code": grader_exit, "wall_ms": round(measured["agent_wall_seconds"] * 1000),
            "artifact_path": artifact, "created_at_ms": started,
            "metrics": {
                "outcome": status, "behavioral_grade": {"status": behavior}, "reported": measured, **flags,
                "result_subtype": subtype, "agent_timed_out": timed_out, "validity_issues": sorted(issues),
                "budget_exhausted": budget, "runtime_error": runtime, "usage_may_be_partial": partial,
                "record_classification_revision": RECORD_CLASSIFICATION_REVISION,
                "import_schema_version": IMPORT_VERSION, "collection_fingerprint": collection,
                "repetition": repetition, "seed_semantics": "repetition_index_not_rng_seed",
                "cost_semantics": "provider_cli_estimate_not_subscription_cash",
                "reported_models": list(models),
                "delivery": delivery,
            },
        })
        if adjudication is not None:
            payloads[-1]["metrics"]["adjudication"] = adjudication
        if tool_runtime is not None:
            payloads[-1]["metrics"]["tool_runtime_evidence"] = tool_runtime
        if not spec["legacy"]:
            payloads[-1]["metrics"]["suite"] = {field: spec[field] for field in ("held_out", "allow_draft_suite")}
            payloads[-1]["metrics"]["suite"]["suite_id"] = spec["suite"]
            if "context" in row:
                payloads[-1]["metrics"]["context"] = context_metrics(row["context"])
    require(number(progress.get("infrastructure_outcomes"), "progress.infrastructure_outcomes", integer=True, required=True)
            == sum(payload["metrics"]["outcome"] == "infrastructure_error" for payload in payloads), "Infrastructure outcome count disagrees with records")
    # Serialization is part of prevalidation, before any daemon mutation.
    for payload in payloads:
        canonical(payload)
    return payloads


class UnixHTTPConnection(http.client.HTTPConnection):
    def __init__(self, socket_path, timeout=30):
        super().__init__("localhost", timeout=timeout)
        self.socket_path = str(socket_path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            self.sock.settimeout(self.timeout)
            self.sock.connect(self.socket_path)
        except OSError:
            self.sock.close()
            self.sock = None
            raise


def post_record(socket_path, payload):
    """Post one prevalidated record; never include daemon error text in errors."""
    connection = UnixHTTPConnection(socket_path)
    try:
        connection.request("POST", ENDPOINT, canonical(payload).encode(), {"Content-Type": "application/json"})
        response = connection.getresponse()
        body = response.read(65_537)
        if response.status not in (200, 201, 204):
            raise TransportError(f"Evaluation API returned HTTP {response.status}")
        if len(body) > 65_536:
            raise TransportError("Evaluation API response exceeded the size limit")
        if response.status != 204:
            try:
                valid = json.loads(body) is None
            except (ValueError, UnicodeError):
                valid = False
            if not valid:
                raise TransportError("Evaluation API returned an unexpected acknowledgement")
    except (OSError, http.client.HTTPException):
        raise TransportError("Evaluation API transport failed; acknowledgement is uncertain") from None
    finally:
        connection.close()


def load_report(path):
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, "Report contains a duplicate JSON key")
            result[key] = value
        return result

    try:
        with Path(path).open("rb") as handle:
            data = handle.read(MAX_REPORT_BYTES + 1)
        require(len(data) <= MAX_REPORT_BYTES, "Report exceeds the size limit")
        return json.loads(data, object_pairs_hook=unique_object)
    except (OSError, UnicodeError, json.JSONDecodeError):
        raise ValidationError("Could not read a valid public JSON report") from None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--benchmark-id", required=True)
    parser.add_argument("--socket", type=Path, default=Path(os.environ.get("ENGRAM_SOCKET_PATH", "~/.engram/engramd.sock")))
    parser.add_argument("--apply", action="store_true", help="POST validated evaluation records; default only validates")
    args = parser.parse_args(argv)
    try:
        report = load_report(args.report)
        payloads = build_payloads(report, args.benchmark_id, args.report)
    except ValidationError as error:
        print(f"Import rejected before any writes: {error}", file=sys.stderr)
        return 2
    summary = {
        "mode": "apply" if args.apply else "dry_run", "validated_records": len(payloads), "acknowledged_records": 0,
        "outcomes": dict(Counter(payload["metrics"]["outcome"] for payload in payloads)),
        "collection_fingerprint": payloads[0]["metrics"]["collection_fingerprint"],
        "outside_matrix_attempts_not_imported": {field: len(report.get(field, [])) for field in ("preflight_runs", "infrastructure_attempts")},
        "reported_measurement_coverage": {field: sum(payload["metrics"]["reported"][field] is not None for payload in payloads)
                                          for field in payloads[0]["metrics"]["reported"]},
        "partial_tool_call_attempts": sum(payload["metrics"]["tool_calls_complete"] is False for payload in payloads),
        "usage_may_be_partial_attempts": sum(payload["metrics"]["usage_may_be_partial"] for payload in payloads),
        "usage_note": "Reported error-run usage and incomplete tool counts can be lower bounds; coverage is not complete billing.",
    }
    if "suite" in payloads[0]["metrics"]:
        summary["suite"] = payloads[0]["metrics"]["suite"]
    if args.apply:
        for payload in payloads:
            try:
                post_record(args.socket.expanduser(), payload)
            except TransportError as error:
                summary["status"] = "partial_failure"
                summary["error"] = str(error)
                summary["retry"] = "Retry the same report, path and label. A failed acknowledgement may already be stored; identical IDs/payloads are idempotent, conflicting content is rejected."
                print(json.dumps(summary, sort_keys=True))
                return 1
            summary["acknowledged_records"] += 1
    summary["status"] = "complete" if args.apply else "validated_no_writes"
    print(json.dumps(summary, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
