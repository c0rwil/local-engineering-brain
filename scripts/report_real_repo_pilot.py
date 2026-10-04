#!/usr/bin/env python3
"""Publish aggregate pilot evidence without copying private traces or patches."""

from __future__ import annotations

import argparse
from collections import Counter
from datetime import datetime, timezone
import hashlib
import json
import math
from pathlib import Path
import re

from real_repo_adjudications import AdjudicationError, EXPLANATIONS, apply_adjudication, load_adjudications
from real_repo_metrics import MEASURES, RECORD_CLASSIFICATION_REVISION, apply_tool_runtime_checks, classify_record, paired_summary

TASK_NAMES = {
    "r01_guide_hot_path": "Skip task guides in the action hot path",
    "r02_lean_task_status": "Lean phone-task status polling",
    "r05_test_env_isolation": "Isolate tests from developer environment files",
    "r06_onboarding_outage": "Bound onboarding outage handling",
}
MANIFEST_KEYS = (
    "schema_version", "condition", "model", "effort", "claude_version",
    "engram_sha256", "daemon_sha256", "runner_sha256", "metrics_sha256",
    "repetitions", "agent_limit_seconds", "grading_limit_seconds",
    "cost_cap_usd_estimate", "platform_dependencies", "private_dependencies", "rustc", "python", "node", "pnpm",
    "known_scope_limits",
    "suite", "controls_sha256", "runner_library_sha256", "claude_sha256", "runtime_preflight_sha256",
)
MODEL_NUMBERS = (
    "inputTokens", "outputTokens", "cacheReadInputTokens", "cacheCreationInputTokens",
    "webSearchRequests", "costUSD", "contextWindow", "maxOutputTokens",
)
DELIVERY_NUMBERS = (
    "emission_receipts", "compiled_runs", "compiled_estimated_tokens",
    "delivered_compiled_estimated_tokens", "source_items",
)


def number(value):
    return value if isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) and value >= 0 else None


def grade_status(value):
    if value in {"infra", "infra_error", "infrastructure_error"}:
        return "infra"
    return value if value in {"pass", "fail"} else "unknown"


def redact_grade(grade):
    grade = grade if isinstance(grade, dict) else {}
    checks = []
    for check in grade.get("checks", []):
        if not isinstance(check, dict) or not isinstance(check.get("name"), str):
            continue
        passed = check.get("passed")
        if not isinstance(passed, bool):
            passed = {"pass": True, "fail": False}.get(check.get("status"))
        checks.append({"name": check["name"], "passed": passed})
    return {"status": grade_status(grade.get("status")), "checks": checks}


def redact_record(record):
    record = classify_record(record)
    if not isinstance(record.get("task_id"), str) or not record["task_id"]:
        raise ValueError("Missing task identifier")
    raw_metrics = record.get("metrics") or {}
    metrics = {name: number(raw_metrics.get(name)) for name in MEASURES if name != "agent_wall_seconds"}
    for name in ("trace_complete", "usage_complete", "tool_calls_complete", "is_error"):
        value = raw_metrics.get(name)
        metrics[name] = value if isinstance(value, bool) else None
    metrics["num_turns"] = number(raw_metrics.get("num_turns"))
    metrics["result_subtype"] = raw_metrics.get("result_subtype")
    metrics["counts_by_tool"] = {name: number(count) for name, count in (raw_metrics.get("counts_by_tool") or {}).items() if number(count) is not None}
    metrics["model_usage"] = {
        model: {name: number(values.get(name)) for name in MODEL_NUMBERS if number(values.get(name)) is not None}
        for model, values in (raw_metrics.get("model_usage") or {}).items() if isinstance(values, dict)
    }
    metrics["models"] = sorted(metrics["model_usage"])
    denials = raw_metrics.get("permission_denials")
    metrics["permission_denial_count"] = len(denials) if isinstance(denials, list) else None
    diagnostics = raw_metrics.get("diagnostics") or {}
    metrics["diagnostics"] = {
        "result_count": number(diagnostics.get("result_count")),
        "malformed_line_count": len(diagnostics.get("malformed_line_numbers", [])),
        "tool_blocks_without_ids": number(diagnostics.get("tool_blocks_without_ids")),
        "conflicting_tool_id_count": len(diagnostics.get("conflicting_tool_ids", [])),
    }
    result = {
        "task_id": record.get("task_id"), "repetition": record.get("repetition"), "arm": record.get("arm"),
        "grade": redact_grade(record.get("grade")), "metrics": metrics,
        "record_classification_revision": record["record_classification_revision"],
        "usage_may_be_partial": record.get("usage_may_be_partial") is True,
        "delivery": {name: number((record.get("delivery") or {}).get(name)) for name in DELIVERY_NUMBERS},
    }
    for name in ("agent_wall_seconds", "setup_seconds", "grading_seconds"):
        result[name] = number(record.get(name))
    for name in ("setup_exit_code", "agent_exit_code", "grader_exit_code", "started_at", "agent_timed_out", "prompt_sha256", "composed_prompt_sha256", "snapshot_sha256"):
        if name in record:
            result[name] = record[name]
    result["validity_issues"] = [value for value in record.get("validity_issues", []) if value in {"model_drift", "no_delivery_receipt", "agent_budget_exhausted", "agent_runtime_error", "ambiguous_grader_contract", "collection_control_drift", "shell_spawn_argument_limit"}]
    if "tool_runtime_evidence" in record:
        result["tool_runtime_evidence"] = {key: record["tool_runtime_evidence"][key] for key in (
            "schema_version", "trace_sha256", "reason", "shell_tool_calls", "shell_spawn_failure_count"
        )}
    if "adjudication" in record:
        result["adjudication"] = {key: record["adjudication"][key] for key in (
            "schema_version", "trial_id", "reason", "grade_sha256", "failed_check_sha256"
        )}
    if "behavioral_grade" in record:
        result["behavioral_grade"] = redact_grade(record["behavioral_grade"])
    # Only publish the known, repository-relative grader paths and SHA-256 values.
    prefix = f"grading/{record['task_id']}/"
    result["grader_manifest"] = {
        path: digest for path, digest in (record.get("grader_manifest") or {}).items()
        if isinstance(path, str) and path.startswith(prefix)
        and len(Path(path).parts) == 3 and ".." not in Path(path).parts
        and isinstance(digest, str) and len(digest) == 64
        and all(character in "0123456789abcdef" for character in digest)
    }
    return result


def redact_fixture(fixture, suite=None):
    result = {name: fixture.get(name) for name in ("task_id", "base_commit", "fixture_head", "snapshot_sha256")}
    for name in ("index_seconds", "dependency_seconds", "private_dependency_seconds", "setup_seconds"):
        result[name] = number(fixture.get(name))
    result["source_counts"] = {name: number((fixture.get("source_counts") or {}).get(name)) for name in ("nodes", "memories", "tasks", "context_runs")}
    result["removed_path_count"] = len(fixture.get("removed_paths", []))
    result["lockfile_sha256"] = fixture.get("lockfile_sha256", {})
    if suite == "real-repo-v2":
        # V2 replaces setup/index/dependency timings, but carries this field
        # forward from the seed pilot when cloning its prepared caches.
        result["inherited_seed_private_dependency_seconds"] = result.pop("private_dependency_seconds")
    return result


def sums_with_coverage(rows, names):
    result = {}
    for name in names:
        values = [value for row in rows if (value := number(row.get(name))) is not None]
        result[name] = {"observed_total": sum(values) if values else None, "available": len(values), "total": len(rows), "partial": len(values) != len(rows)}
    return result


def format_number(value, *, decimals=0):
    return "unavailable" if value is None else f"{value:,.{decimals}f}"


def markdown(summary, manifest, fixtures, artifacts=()):
    progress = summary["progress"]
    followup = manifest.get("suite") == "real-repo-v2"
    lines = [
        "# Follow-up real-repository task benchmark" if manifest.get("suite") == "real-repo-v2" else "# Real-repository task pilot", "",
        f"**{progress['status'].replace('_', ' ').capitalize()}: {progress['completed_records']}/{progress['planned_runs']} trial records completed.** "
        f"{progress['started_trials']} trial attempts started; {progress['coding_runs_started']} coding runs started; "
        f"{progress['in_progress_trials']} attempts remain active; {progress.get('interrupted_trials', 0)} were interrupted without a final record. "
        f"Completed outcomes include {progress['infrastructure_outcomes']} infrastructure/unscorable outcomes.", "",
        f"Pinned agent: `{manifest.get('model', 'unavailable')}`, effort `{manifest.get('effort', 'unavailable')}`, "
        f"Claude Code `{manifest.get('claude_version', 'unavailable')}`. "
        f"Protocol: {manifest.get('repetitions', 'unavailable')} repetitions per task and arm, Engram off versus source-only automatic hooks.", "",
        "All tables below use completed records only. Started or interrupted runs without final records can have additional unmeasured resource use. "
        "Token totals are provider input + cache creation + cache reads + output; they are processed tokens, not an estimate from text length.", "",
        "## Correctness and observed resources", "",
        "Clean validated completions require behavioral acceptance, valid controls and grading, and no explicit agent runtime error or exhausted budget. "
        "A patch can pass the behavioral grader while its agent run fails; the behavioral grade is retained separately. "
        "Success fractions use completed attempts as the denominator, including behavioral failures and infrastructure/unscorable outcomes. "
        "Each cell shows off → brain. Resources include unsuccessful attempts; partial measurements are marked.", "",
        "| Task | Behavioral passes (off; brain) | Clean validated completions (off; brain) | Provider tokens | Agent tool calls | Agent seconds |",
        "|---|---|---|---:|---:|---:|",
    ]
    for task in summary["per_task"]:
        arms = task["all_attempts"]

        def cell(name, decimals=0):
            values = []
            for arm in ("off", "brain"):
                block = arms[arm]
                coverage = block["measurement_coverage"][name]
                partial = coverage["missing"] > 0 or coverage.get("partial", 0) > 0
                values.append(format_number(block["totals_observed"][name], decimals=decimals) + (" (partial)" if partial else ""))
            return " → ".join(values)

        success = "; ".join(f"{arms[arm]['grade_counts'].get('pass', 0)}/{arms[arm]['attempts']}" for arm in ("off", "brain"))
        behavioral = "; ".join(f"{task['behavioral_grade_counts'][arm].get('pass', 0)}/{arms[arm]['attempts']}" for arm in ("off", "brain"))
        lines.append(f"| {TASK_NAMES.get(task['task_id'], task['task_id'])} | {behavioral} | {success} | {cell('total_tokens')} | {cell('tool_calls')} | {cell('agent_wall_seconds', 1)} |")
    lines += ["", "| Arm | Clean passes | Failures | Infrastructure / unscorable | Unknown | Attempts |", "|---|---:|---:|---:|---:|---:|"]
    for arm in ("off", "brain"):
        block = summary["all_attempts"][arm]
        counts = block["grade_counts"]
        lines.append(f"| {arm} | {counts.get('pass', 0)} | {counts.get('fail', 0)} | {counts.get('infra', 0)} | {counts.get('unknown', 0)} | {block['attempts']} |")
    lines += ["", "Unscorable attempts remain in resource totals but cannot establish a correctness win. Differences in overall clean-completion fractions alone are not an effectiveness estimate.", "",
              "| All completed attempts | Provider tokens | Agent tool calls | Agent seconds | Reported USD estimate |", "|---|---:|---:|---:|---:|"]
    for arm in ("off", "brain"):
        block = summary["all_attempts"][arm]
        values = []
        for name, decimals in (("total_tokens", 0), ("tool_calls", 0), ("agent_wall_seconds", 1), ("cost_usd_estimate", 3)):
            cov = block["measurement_coverage"][name]
            values.append(format_number(block["totals_observed"][name], decimals=decimals) + (" (partial)" if cov.get("partial", 0) else "") + f" [{cov['available']}/{cov['total']} reported]")
        lines.append(f"| {arm} | " + " | ".join(values) + " |")
    lines += [
        "", "Provider token categories remain separate because cache reads, cache writes, ordinary input and output have different prices. These are all-attempt totals, including reported usage from interrupted runs.", "",
        "| Arm | Ordinary input | Cache creation | Cache reads | Output |",
        "|---|---:|---:|---:|---:|",
    ]
    for arm in ("off", "brain"):
        block = summary["all_attempts"][arm]
        cells = []
        for name in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"):
            coverage = block["measurement_coverage"][name]
            cells.append(format_number(block["totals_observed"][name]) + (" (partial)" if coverage.get("partial", 0) else "") + f" [{coverage['available']}/{coverage['total']} reported]")
        lines.append(f"| {arm} | " + " | ".join(cells) + " |")
    counts = summary["pair_counts"]
    lines += [
        "", "## Paired results", "",
        f"Complete pairs: {counts['complete_pairs']}; both pass: {counts['both_pass']}; "
        f"only off passes: {counts['off_only_pass']}; only brain passes: {counts['brain_only_pass']}; "
        f"neither passes: {counts['neither_pass']}; infrastructure/unscorable: {counts['infrastructure']}; "
        f"incomplete: {counts['incomplete']}; ambiguous duplicates: {counts['ambiguous']}.", "",
        "Pairs with an infrastructure or unknown outcome are unscorable and do not count as a correctness win for either arm. "
        "Reductions below compare only pairs where both arms have clean validated completions and both measurements are available. "
        "Positive percentages mean fewer resources with Engram; negative percentages mean more. "
        "Percentages use sums over matched pairs, not the average of percentages. A cheaper failed run is never counted as savings.", "",
        "| Both-pass comparison | Token reduction | Tool-call reduction | Agent-time reduction |",
        "|---|---:|---:|---:|",
    ]
    comparisons = [{"task_id": "All tasks", **summary["both_pass"]["aggregate"]}, *summary["both_pass"]["per_task"]]
    for comparison in comparisons:
        cells = []
        for name in ("total_tokens", "tool_calls", "agent_wall_seconds"):
            metric = comparison["metrics"][name]
            delta = metric["comparison"]
            percent = delta["reduction_percent"] if delta is not None else None
            cells.append((f"{percent:+.1f}%" if percent is not None else "undefined") + f" (n={metric['pair_count']})")
        lines.append(f"| {TASK_NAMES.get(comparison['task_id'], comparison['task_id'])} | " + " | ".join(cells) + " |")
    lines += [
        "", "## All-attempt resource per clean validated completion", "",
        "These quotients charge each arm's observed failed and infrastructure attempts to its clean validated completions. "
        "They are descriptive, not paired savings estimates; no successes means undefined. Partial coverage means the numerator is only observed usage.", "",
        "| Arm | Tokens / success | Tool calls / success | Agent seconds / success | USD estimate / success |",
        "|---|---:|---:|---:|---:|",
    ]
    for arm in ("off", "brain"):
        cells = []
        for name, decimals in (("total_tokens", 0), ("tool_calls", 1), ("agent_wall_seconds", 1), ("cost_usd_estimate", 3)):
            value = summary["resource_per_validated_success"][arm][name]
            text = "undefined" if value["value"] is None else format_number(value["value"], decimals=decimals)
            cells.append(text + (" (partial coverage)" if value["partial"] else ""))
        lines.append(f"| {arm} | " + " | ".join(cells) + " |")
    if followup:
        setup_description = ("Collection fixture setup measures cloning the prepared seed caches and reindexing/snapshot work. "
                             "The index/snapshot portion overlaps setup and must not be added to it. "
                             "Inherited seed private-cache preparation belongs to the earlier pilot, not this collection.")
        setup_header = "| Task | Collection clone/reindex seconds | Index/snapshot seconds | New dependency preparation seconds | Inherited seed private-cache seconds | Indexed source nodes |"
        private_dependency_field = "inherited_seed_private_dependency_seconds"
    else:
        setup_description = "Original fixture setup includes dependency preparation and indexing/snapshot work; these columns overlap and must not be added together. Additional private dependency preparation is recorded separately."
        setup_header = "| Task | Original setup seconds | Index/snapshot seconds | Original dependency seconds | Additional private dependency seconds | Indexed source nodes |"
        private_dependency_field = "private_dependency_seconds"
    lines += ["", "## Setup, indexing, and delivery", "", setup_description, "", setup_header, "|---|---:|---:|---:|---:|---:|"]
    for fixture in fixtures:
        lines.append(f"| {TASK_NAMES.get(fixture['task_id'], fixture['task_id'])} | {format_number(fixture['setup_seconds'], decimals=1)} | {format_number(fixture['index_seconds'], decimals=1)} | {format_number(fixture['dependency_seconds'], decimals=1)} | {format_number(fixture[private_dependency_field], decimals=1)} | {format_number(fixture['source_counts']['nodes'])} |")
    fixture_overhead = summary["overhead"]["fixtures"]
    setup_label = "Measured collection fixture setup" if followup else "Observed one-time fixture setup"
    private_label = "Inherited seed private-cache preparation (earlier pilot)" if followup else "Additional private dependency preparation"
    private_overhead = fixture_overhead[private_dependency_field]
    lines += ["", f"{setup_label}: {format_number(fixture_overhead['setup_seconds']['observed_total'], decimals=1)} seconds; "
              f"index/snapshot portion: {format_number(fixture_overhead['index_seconds']['observed_total'], decimals=1)} seconds. "
              f"{private_label}: {format_number(private_overhead['observed_total'], decimals=1)} seconds "
              f"[{private_overhead['available']}/{private_overhead['total']} fixtures recorded].", ""]
    for arm in ("off", "brain"):
        overhead = summary["overhead"]["trials"][arm]
        delivery = summary["delivery"][arm]
        lines.append(f"- {arm}: {format_number(overhead['setup_seconds']['observed_total'], decimals=1)} seconds of per-trial setup, "
                     f"{format_number(overhead['grading_seconds']['observed_total'], decimals=1)} seconds of grading; "
                     f"{format_number(delivery['emission_receipts']['observed_total'])} delivered contexts and "
                     f"{format_number(delivery['compiled_runs']['observed_total'])} context compilations.")
    lines += [
        "", "Agent wall time includes hook processing; fixture setup, per-trial preparation, and private grading are reported separately. "
        "Agent tool-call counts do not include internal Engram hook/daemon operations. Context estimates and delivery counts are separate from provider token usage.",
    ]
    for field, title in (("infrastructure_attempts", "Separate infrastructure attempts"), ("preflight_runs", "Runtime, authentication, isolation, and toolchain preflights")):
        lines += [
            "", f"## {title}", "",
            f"{summary[field]['attempts']} recorded attempts are outside the planned matrix. "
            "Their observed resource use is retained separately and is not assigned to the paired comparisons. Missing final provider telemetry is unknown, not zero.", "",
            "| Arm | Attempts | Observed provider tokens | Observed tool calls | Agent seconds | Reported USD estimate |",
            "|---|---:|---:|---:|---:|---:|",
        ]
        for arm, block in summary[field]["by_arm"].items():
            if not block["attempts"]:
                continue
            cells = []
            for name, decimals in (("total_tokens", 0), ("tool_calls", 0), ("agent_wall_seconds", 1), ("cost_usd_estimate", 3)):
                cov = block["measurement_coverage"][name]
                cells.append(format_number(block["totals_observed"][name], decimals=decimals) + (" (partial)" if cov.get("partial", 0) else "") + f" [{cov['available']}/{cov['total']} reported]")
            lines.append(f"| {arm} | {block['attempts']} | " + " | ".join(cells) + " |")
    lines += [
        "", "## Scope and limitations", "",
        "- This is a four-task, three-repetition diagnostic, not a general productivity estimate or a statistically conclusive trial.",
        "- The model and effort are pinned. Model drift or missing brain delivery is classified as infrastructure failure rather than a successful Engram trial.",
        "- Claude-reported USD figures are usage estimates under a subscription, not incremental cash charges or billing receipts.",
        "- Platform dependencies use the prepared installed cache and differ from the historical lockfile; both arms use the same prepared environment.",
        "- Engram uses source-only historical indexes with automatic hooks, no accumulated learned memories, and no MCP tools. This does not measure the full second-brain configuration.",
        "- Provider cache reads and cache creation are included in processed-token totals. Cache pricing differs, so processed-token reduction and estimated USD reduction need not agree.",
        f"- Derived completion classification uses revision `{RECORD_CLASSIFICATION_REVISION}`. Raw records and behavioral grades are preserved; runtime errors are infrastructure outcomes and explicit budget exhaustion is failure unless infrastructure already invalidates the trial.",
        "- Manifest runner/metrics hashes identify frozen collection-start sources. The separate analysis_provenance hashes identify the scripts used to generate this report; post-collection classification does not replace raw measurements.",
        "- Reported usage from error or interrupted runs may be partial even when final usage fields are present. Measurement coverage counts reported values, not complete error-run billing. All reported values remain in all-attempt totals; none of these runs contributes to both-pass savings.",
        "- Incomplete tool traces retain their observed call counts as lower bounds in all-attempt totals, with partial coverage marked. They are excluded from paired tool-call deltas.",
        "- Private source, prompts, patches, tool inputs, raw grader output, session identifiers, and trace identifiers are excluded from these artifacts.",
    ]
    for limit in manifest.get("known_scope_limits", []):
        lines.append(f"- {limit}")
    for reason, count in summary["adjudication_counts"].items():
        lines.append(f"- Post-collection adjudication ({count} attempt{'s' if count != 1 else ''}): {EXPLANATIONS[reason]} Original frozen grades are unchanged. See the grading audit for the reviewed ambiguity.")
    if progress["status"] == "aborted":
        lines += ["", "**Collection aborted: the generated sandbox prevented shell commands from starting (E2BIG).** "
                  "Raw grades and usage are preserved, but affected attempts are unscorable and excluded from savings. "
                  "This collection will not be resumed or mixed with a repaired protocol."]
    elif progress["status"] != "complete":
        lines += ["", "This report is a progress snapshot. The planned matrix is not complete; do not treat the current results as the final pilot outcome."]
    if summary["input_warnings"]:
        lines += ["", "Input warnings: " + "; ".join(summary["input_warnings"]) + "."]
    lines += ["", "Machine-readable evidence: [redacted results](results.json) and [aggregate summary](summary.json)."]
    if artifacts:
        lines += ["", "Additional evidence: " + "; ".join(f"[{title}]({filename})" for filename, title in artifacts) + "."]
    lines.append("")
    return "\n".join(lines)


def generate_report(data: Path, output: Path):
    manifest_raw = json.loads((data / "manifest.json").read_text())
    manifest = {name: manifest_raw[name] for name in MANIFEST_KEYS if name in manifest_raw}
    fixtures = [redact_fixture(json.loads(path.read_text()), manifest.get("suite")) for path in sorted((data / "fixtures").glob("*/ready.json"))]
    warnings = []
    adjudications = load_adjudications(data)
    stopped = False
    stop_path = data / "collection_stop.json"
    if stop_path.exists():
        stop = json.loads(stop_path.read_text())
        if (type(stop.get("schema_version")) is not int or stop["schema_version"] != 1
                or stop.get("reason") != "shell_spawn_argument_limit"
                or stop.get("controls_sha256") != manifest.get("controls_sha256")):
            raise AdjudicationError("Collection stop evidence does not match the frozen protocol")
        stopped = True

    def read_records(name):
        records = []
        path = data / name
        if path.exists():
            for n, line in enumerate(path.read_text().splitlines(), 1):
                if not line.strip():
                    continue
                try:
                    record = json.loads(line)
                    # Only this publication's verified adapter can create provenance.
                    record.pop("adjudication", None)
                    if name == "records.jsonl":
                        record = apply_adjudication(record, data, adjudications)
                        identity = record.get("id")
                        safe_identity = (isinstance(identity, str)
                                         and re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,127}", identity)
                                         and identity == f"{record.get('task_id')}-r{record.get('repetition')}-{record.get('arm')}")
                        verified_runtime = False
                        if safe_identity:
                            trace = data.resolve() / "trials" / identity / "trace.jsonl"
                            if trace.is_file() and trace.resolve() == trace:
                                # Only fresh trace inspection may supply this provenance.
                                record.pop("tool_runtime_evidence", None)
                                record = apply_tool_runtime_checks(record, trace)
                                verified_runtime = "tool_runtime_evidence" in record
                        if stopped and not verified_runtime:
                            raise AdjudicationError("An E2BIG-aborted attempt requires its own verifiable shell-failure trace")
                    records.append(redact_record(record))
                except AdjudicationError:
                    # A stale exclusion must stop publication, never hide a trial.
                    raise
                except (ValueError, TypeError, AttributeError):
                    warnings.append(f"{name} line {n} could not be read")
        return records

    records = read_records("records.jsonl")
    applied = Counter(r["adjudication"]["trial_id"] for r in records if "adjudication" in r)
    if applied != Counter({identity: 1 for identity in adjudications}):
        raise AdjudicationError("Each configured adjudication must match exactly one retained trial.")
    infrastructure_records = read_records("infrastructure_attempts.jsonl")
    preflight_records = read_records("preflight_runs.jsonl")
    repetitions = manifest.get("repetitions", 3)
    task_ids = sorted(set(TASK_NAMES) | {f["task_id"] for f in fixtures} | {r["task_id"] for r in records})
    expected_keys = {(task_id, repetition, arm) for task_id in TASK_NAMES for repetition in range(1, repetitions + 1) for arm in ("off", "brain")}
    planned = len(expected_keys)
    trial_dirs = [path for path in (data / "trials").glob("*") if path.is_dir()]
    started = max(len(trial_dirs), len(records))
    agent_started = max(sum((path / "trace.jsonl").exists() for path in trial_dirs), sum(r.get("agent_wall_seconds") is not None for r in records))
    finished = len(records)
    summary = paired_summary(records)
    summary["adjudication_counts"] = dict(Counter(r["adjudication"]["reason"] for r in records if "adjudication" in r))
    actual_keys = {(r["task_id"], r["repetition"], r["arm"]) for r in records}
    complete = finished == planned and actual_keys == expected_keys and not warnings and summary["pair_counts"]["complete_pairs"] == planned // 2 and not summary["measurement_coverage"]["invalid_record_indices"]
    summary["progress"] = {
        "status": "aborted" if stopped else "complete" if complete else "in_progress" if started else "not_started",
        "planned_runs": planned, "started_trials": started, "coding_runs_started": agent_started,
        "completed_records": finished, "in_progress_trials": 0 if stopped else max(0, started - finished),
        "infrastructure_outcomes": sum(r["grade"]["status"] == "infra" for r in records),
    }
    if stopped:
        summary["progress"]["interrupted_trials"] = max(0, started - finished)
    summary["per_task"] = [{"task_id": task_id, "all_attempts": paired_summary([r for r in records if r["task_id"] == task_id])["all_attempts"]} for task_id in task_ids]
    for task in summary["per_task"]:
        task["behavioral_grade_counts"] = {
            arm: dict(Counter(record.get("behavioral_grade", record["grade"])["status"]
                              for record in records if record["task_id"] == task["task_id"] and record["arm"] == arm))
            for arm in ("off", "brain")
        }
    summary["resource_per_validated_success"] = {}
    private_dependency_field = "inherited_seed_private_dependency_seconds" if manifest.get("suite") == "real-repo-v2" else "private_dependency_seconds"
    summary["overhead"] = {"fixtures": sums_with_coverage(fixtures, ("setup_seconds", "index_seconds", "dependency_seconds", private_dependency_field)), "trials": {}}
    summary["delivery"] = {}
    summary["infrastructure_attempts"] = {
        "attempts": len(infrastructure_records),
        "by_arm": paired_summary(infrastructure_records)["all_attempts"],
        "excluded_from_matrix": True,
    }
    summary["preflight_runs"] = {
        "attempts": len(preflight_records),
        "by_arm": paired_summary(preflight_records)["all_attempts"],
        "excluded_from_matrix": True,
    }
    for arm in ("off", "brain"):
        block = summary["all_attempts"][arm]
        successes = block["grade_counts"].get("pass", 0)
        summary["resource_per_validated_success"][arm] = {
            name: {"value": total / successes if total is not None and successes else None, "validated_successes": successes,
                   "observed_total": total, "partial": block["measurement_coverage"][name]["missing"] > 0 or block["measurement_coverage"][name].get("partial", 0) > 0,
                   "measurement_coverage": block["measurement_coverage"][name]}
            for name, total in block["totals_observed"].items()
        }
        arm_records = [r for r in records if r["arm"] == arm]
        summary["overhead"]["trials"][arm] = sums_with_coverage(arm_records, ("setup_seconds", "grading_seconds"))
        summary["delivery"][arm] = sums_with_coverage([r["delivery"] for r in arm_records], DELIVERY_NUMBERS)
    summary["input_warnings"] = warnings
    generated_at = datetime.now(timezone.utc).isoformat()
    result = {"schema_version": 1, "record_classification_revision": RECORD_CLASSIFICATION_REVISION, "generated_at": generated_at, "progress": summary["progress"], "manifest": manifest, "fixtures": fixtures, "records": records, "infrastructure_attempts": infrastructure_records, "preflight_runs": preflight_records}
    result["analysis_provenance"] = {
        path.name: hashlib.sha256(path.read_bytes()).hexdigest()
        for path in (Path(__file__), Path(__file__).with_name("real_repo_metrics.py"), Path(__file__).with_name("real_repo_diagnostics.py"), Path(__file__).with_name("real_repo_adjudications.py"))
    }
    summary["analysis_provenance"] = result["analysis_provenance"]
    summary["generated_at"] = generated_at
    output.mkdir(parents=True, exist_ok=True)
    for name, value in (("results.json", result), ("summary.json", summary)):
        (output / name).write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")
    artifacts = [(filename, title) for filename, title in (
        ("FINDINGS.md", "findings and interpretation"),
        ("CONTEXT_AUDIT.md", "context delivery audit"),
        ("GRADING_AUDIT.md", "grading audit and uncovered cases"),
        ("COLLECTION_AUDIT.md", "collection controls audit"),
        ("NEXT_ITERATION.md", "measured follow-up tasks"),
        ("workflow.json", "native-tool workflow diagnostics"),
    ) if (output / filename).is_file()]
    (output / "README.md").write_text(markdown(summary, manifest, fixtures, artifacts))
    return summary["progress"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    print(json.dumps(generate_report(args.data, args.output)))


if __name__ == "__main__":
    main()
