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
from real_repo_context import DELIVERY_FIELDS as CONTEXT_DELIVERY_FIELDS, READ_FIELDS, share, trial_context
from real_repo_metrics import (MEASURES, RECORD_CLASSIFICATION_REVISION, TOKEN_FIELDS, _delta, apply_tool_runtime_checks,
                               classify_record, paired_summary)

# v1 manifests carry no suite; v1 and v2 keep their fixed task tables and wording.
LEGACY_SUITES = (None, "real-repo-v1", "real-repo-v2")
GRADER_SUFFIXES = (".py", ".rs", ".ts", ".json")
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
    "agent", "codex_version", "codex_sha256", "codex_hook_trust", "codex_config_sha256", "codex_auth",
    # Catalog-selected suites (absent from v1/v2 manifests).
    "allow_draft_suite", "suite_status", "fixture_source", "grader_preflight_sha256",
)
CONTEXT_MEASURES = (*READ_FIELDS, *CONTEXT_DELIVERY_FIELDS)
PAIRED_CONTEXT_MEASURES = ("read_calls", "ranged_read_calls", "read_result_bytes")
SAFE_LABEL = re.compile(r"[a-z0-9][a-z0-9_-]{0,127}")
COUNT_WORDS = {1: "one", 2: "two", 3: "three", 4: "four", 5: "five", 6: "six", 7: "seven", 8: "eight", 9: "nine", 10: "ten"}
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


def suite_definition(data, manifest_raw):
    """The planned matrix and public task metadata of a catalog-selected suite.

    v1 and v2 collections return None and keep their fixed task tables. Other
    suites are read from the frozen protocol catalog and the manifest's suite
    file digests, so the published grader file set is the one actually frozen.
    """
    suite_id = manifest_raw.get("suite")
    if suite_id in LEGACY_SUITES:
        return None
    catalog = json.loads((data / "protocol/suite/catalog.json").read_text())
    if catalog.get("suite_id") != suite_id:
        raise ValueError("The frozen protocol catalog does not match the manifest suite")
    selected = catalog.get("pilot_task_ids") or [task["id"] for task in catalog["tasks"]]
    tasks = [task for task in catalog["tasks"] if task["id"] in selected]
    if not tasks or any(not isinstance(task.get("id"), str) or not SAFE_LABEL.fullmatch(task["id"]) for task in tasks):
        raise ValueError("The frozen protocol catalog has no valid task identities")
    suite_files = (manifest_raw.get("controls") or {}).get("suite_files") or {}
    repetitions = manifest_raw.get("repetitions", 3)
    return {
        "suite_id": suite_id,
        "held_out": all(str(task.get("selection", "")).startswith("heldout") for task in tasks),
        "catalog_status": catalog.get("execution_status"),
        "allow_draft_suite": manifest_raw.get("allow_draft_suite") is True,
        "repetitions": repetitions,
        "planned_runs": len(tasks) * repetitions * 2,
        "task_ids": [task["id"] for task in tasks],
        "task_titles": {task["id"]: str(task.get("title", task["id"]))[:200] for task in tasks},
        "task_exercises": {task["id"]: [value for value in task.get("exercises", []) if isinstance(value, str) and SAFE_LABEL.fullmatch(value)]
                           for task in tasks},
        "grader_files": {task["id"]: {path: digest for path, digest in sorted(suite_files.items())
                                      if path.startswith(f"grading/{task['id']}/") and len(Path(path).parts) == 3
                                      and Path(path).suffix in GRADER_SUFFIXES and isinstance(digest, str)
                                      and re.fullmatch(r"[0-9a-f]{64}", digest)}
                         for task in tasks},
    }


INSPECTION_DECISIONS = ("continue_in_matrix", "separate_infrastructure_attempt")


def read_inspections(data):
    """Recorded decisions on stopping attempts (launch_real_repo_collection.py resume); empty when none were made."""
    path = data / "inspections.jsonl"
    if not path.is_file():
        return []
    result = []
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        entry = json.loads(line)
        identity, decision = entry.get("attempt_id"), entry.get("decision")
        if not isinstance(identity, str) or not SAFE_LABEL.fullmatch(identity) or decision not in INSPECTION_DECISIONS:
            raise ValueError("An inspection ledger entry is invalid")
        inspected = entry.get("inspected_at")
        result.append({"attempt_id": identity, "decision": decision, "reason": " ".join(str(entry.get("reason", "")).split())[:500],
                       "inspected_at": inspected if isinstance(inspected, str) and len(inspected) <= 40 else None})
    return result


def redact_context(context):
    """Only the numeric context measurements and their completeness flag."""
    result = {name: number(context.get(name)) for name in CONTEXT_MEASURES}
    result["read_measurement_complete"] = context.get("read_measurement_complete") is True
    return result


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


def redact_record(record, context=None):
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
    # Claude reports models in modelUsage; Codex records them from turn context.
    metrics["models"] = sorted(metrics["model_usage"]) or sorted(
        model for model in (raw_metrics.get("models") or []) if isinstance(model, str))
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
    if context is not None:
        result["context"] = redact_context(context)
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
    elif suite not in LEGACY_SUITES:
        # Fixtures built from the operator's repositories: no inherited caches.
        result.pop("private_dependency_seconds")
        result["export_seconds"] = number(fixture.get("export_seconds"))
        reuse = fixture.get("dependency_reuse")
        result["dependency_reuse"] = reuse if isinstance(reuse, str) and SAFE_LABEL.fullmatch(reuse) else None
        build = fixture.get("build_id")
        result["engram_build_id"] = build if isinstance(build, str) and re.fullmatch(r"[0-9a-f]{12,64}", build) else None
        result["snapshot_schema_version"] = number(fixture.get("schema_version"))
        result["snapshot_refreshes"] = len(fixture.get("snapshot_refreshes", []))
    return result


def context_summary(records):
    """Per-arm context totals, the brain arm's ranged-pointer share, and both-pass paired Read deltas."""
    by_arm = {}
    for arm in ("off", "brain"):
        contexts = [record["context"] for record in records if record["arm"] == arm and "context" in record]
        totals = sums_with_coverage(contexts, CONTEXT_MEASURES)
        complete_reads = [context for context in contexts if context["read_measurement_complete"]]
        by_arm[arm] = {
            "attempts": len(contexts), "read_complete_attempts": len(complete_reads), "totals": totals,
            "ranged_pointer_share": share(totals["ranged_pointer_lines"]["observed_total"], totals["pointer_lines"]["observed_total"]),
            "read_bytes_per_call": share(sum(c["read_result_bytes"] for c in complete_reads) if complete_reads else None,
                                         sum(c["read_calls"] for c in complete_reads) if complete_reads else None),
        }
    contexts = {(record["task_id"], record["repetition"], record["arm"]): record.get("context") for record in records}
    paired = {}
    for pair in paired_summary(records)["pairs"]:
        if pair["status"] != "both_pass":
            continue
        off, brain = (contexts.get((pair["task_id"], pair["repetition"], arm)) for arm in ("off", "brain"))
        for name in PAIRED_CONTEXT_MEASURES:
            valid = (off is not None and brain is not None and off["read_measurement_complete"] and brain["read_measurement_complete"]
                     and off[name] is not None and brain[name] is not None)
            for key in ("All tasks", pair["task_id"]):
                entry = paired.setdefault(key, {}).setdefault(name, {"pairs": [], "missing_pair_count": 0})
                if valid:
                    entry["pairs"].append((off[name], brain[name]))
                else:
                    entry["missing_pair_count"] += 1
    both_pass = []
    for key, metrics in paired.items():
        both_pass.append({"task_id": key, "metrics": {
            name: {"pair_count": len(value["pairs"]), "missing_pair_count": value["missing_pair_count"],
                   "comparison": _delta(sum(p[0] for p in value["pairs"]), sum(p[1] for p in value["pairs"])) if value["pairs"] else None}
            for name, value in metrics.items()}})
    both_pass.sort(key=lambda item: (item["task_id"] != "All tasks", item["task_id"]))
    return {"by_arm": by_arm, "both_pass": both_pass,
            "definitions": {
                "delivered_contexts": "Brain contexts confirmed delivered by hook receipts.",
                "pointer_lines": "Source-map pointer lines in delivered contexts; ranged ones name a line span.",
                "related_use_items": "Delivered context items of type code_use (related-use cues).",
                "read_result_bytes": "UTF-8 bytes of text returned by native Read requests; Bash file reads are not counted.",
                "ranged_read_calls": "Read requests that named an offset or limit."}}


def sums_with_coverage(rows, names):
    result = {}
    for name in names:
        values = [value for row in rows if (value := number(row.get(name))) is not None]
        result[name] = {"observed_total": sum(values) if values else None, "available": len(values), "total": len(rows), "partial": len(values) != len(rows)}
    return result


def format_number(value, *, decimals=0):
    return "unavailable" if value is None else f"{value:,.{decimals}f}"


def suite_title(suite):
    if suite["held_out"]:
        return "# Held-out real-repository task benchmark"
    return f"# Real-repository task benchmark: {suite['suite_id']}"


def suite_statement(suite):
    """What kind of suite this is, and whether it was collected as a draft."""
    count = COUNT_WORDS.get(len(suite["task_ids"]), str(len(suite["task_ids"])))
    if suite["held_out"]:
        text = (f"**Held-out suite `{suite['suite_id']}`.** Its {count} historical tasks were chosen outside the development "
                "tasks that informed Engram, to test whether its context changes generalize.")
    else:
        text = f"**Suite `{suite['suite_id']}`** with {count} historical tasks."
    if suite["allow_draft_suite"]:
        text += (f" It was collected while its catalog was still `{suite['catalog_status']}`, with `--allow-draft-suite`: "
                 "the operator review in its README, including confirmation that no task informed Engram development, "
                 "was still pending, so the held-out status is provisional.")
    else:
        text += f" Catalog status at collection: `{suite['catalog_status']}`."
    return text


def format_share(value):
    return "unavailable" if value is None else f"{100 * value:.1f}%"


def context_markdown(summary, task_name):
    """Context delivered per arm and the paired Read comparison for the line-range hypothesis."""
    context = summary["context_delivery"]
    lines = [
        "", "## Context delivery and reads", "",
        "Delivered contexts are Brain contexts confirmed by hook receipts. Pointer lines are their source-map entries; "
        "a ranged pointer names a line span. Related-use items are delivered `code_use` items. Read counts native Read "
        "requests over all completed attempts, those that named an offset or limit, and the UTF-8 bytes of text they "
        "returned; file reads through Bash are not counted. Off attempts have no Engram context by design.", "",
        "| Arm | Delivered contexts | Pointer lines | Ranged pointers | Related-use pointers (items) | Read calls | Ranged Read calls | Read text bytes |",
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]

    def total(block, name):
        coverage = block["totals"][name]
        return format_number(coverage["observed_total"]) + (" (partial)" if coverage["partial"] else "")

    arms = context["by_arm"]
    for arm in ("off", "brain"):
        block = arms[arm]
        ranged = total(block, "ranged_pointer_lines") + f" ({format_share(block['ranged_pointer_share'])})"
        related = total(block, "related_use_pointer_lines") + f" ({total(block, 'related_use_items')})"
        lines.append(f"| {arm} | {total(block, 'delivered_contexts')} | {total(block, 'pointer_lines')} | {ranged} | {related} | "
                     f"{total(block, 'read_calls')} | {total(block, 'ranged_read_calls')} | {total(block, 'read_result_bytes')} |")
    lines += [
        "", f"Read totals are complete for {arms['off']['read_complete_attempts']}/{arms['off']['attempts']} off and "
        f"{arms['brain']['read_complete_attempts']}/{arms['brain']['attempts']} brain attempts; partial cells include the others as observed.", "",
        "The line-range hypothesis is fewer Read bytes and fewer cache-read tokens with Engram at equal or better correctness. "
        "The comparison uses the both-pass pairs above, with complete Read measurements in both arms; positive percentages "
        "mean less with Engram. Correctness is in the paired results: a reduction only counts where both arms passed.", "",
        "| Both-pass comparison | Read calls | Ranged Read calls | Read text bytes | Cache-read tokens |",
        "|---|---:|---:|---:|---:|",
    ]
    cache = {"All tasks": summary["both_pass"]["aggregate"]["metrics"]["cache_read_input_tokens"],
             **{item["task_id"]: item["metrics"]["cache_read_input_tokens"] for item in summary["both_pass"]["per_task"]}}

    def reduction(metric):
        if metric is None:
            return "undefined (n=0)"
        percent = metric["comparison"]["reduction_percent"] if metric["comparison"] is not None else None
        return (f"{percent:+.1f}%" if percent is not None else "undefined") + f" (n={metric['pair_count']})"

    for comparison in context["both_pass"] or [{"task_id": "All tasks", "metrics": {}}]:
        cells = [reduction(comparison["metrics"].get(name)) for name in PAIRED_CONTEXT_MEASURES]
        lines.append(f"| {task_name(comparison['task_id'])} | " + " | ".join([*cells, reduction(cache.get(comparison["task_id"]))]) + " |")
    return lines


def markdown(summary, manifest, fixtures, artifacts=(), suite=None):
    progress = summary["progress"]
    followup = manifest.get("suite") == "real-repo-v2"
    codex = manifest.get("agent") == "codex"
    agent_line = (f"Codex CLI `{manifest.get('codex_version', 'unavailable')}`. " if codex
                  else f"Claude Code `{manifest.get('claude_version', 'unavailable')}`. ")
    treatment = "source-only automatic hooks and the engram MCP server" if codex else "source-only automatic hooks"
    task_name = (lambda task_id: suite["task_titles"].get(task_id, task_id)) if suite else (lambda task_id: TASK_NAMES.get(task_id, task_id))
    title = suite_title(suite) if suite else ("# Follow-up real-repository task benchmark" if manifest.get("suite") == "real-repo-v2" else "# Real-repository task pilot")
    lines = [
        title, "",
        *([suite_statement(suite), ""] if suite else []),
        f"**{progress['status'].replace('_', ' ').capitalize()}: {progress['completed_records']}/{progress['planned_runs']} trial records completed.** "
        f"{progress['started_trials']} trial attempts started; {progress['coding_runs_started']} coding runs started; "
        f"{progress['in_progress_trials']} attempts remain active; {progress.get('interrupted_trials', 0)} were interrupted without a final record. "
        f"Completed outcomes include {progress['infrastructure_outcomes']} infrastructure/unscorable outcomes.", "",
        f"Pinned agent: `{manifest.get('model', 'unavailable')}`, effort `{manifest.get('effort', 'unavailable')}`, "
        + agent_line +
        f"Protocol: {manifest.get('repetitions', 'unavailable')} repetitions per task and arm, Engram off versus {treatment}.", "",
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
        lines.append(f"| {task_name(task['task_id'])} | {behavioral} | {success} | {cell('total_tokens')} | {cell('tool_calls')} | {cell('agent_wall_seconds', 1)} |")
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
        "| Arm | Ordinary input | Cache creation | Cache reads | Output |" + (" Reasoning (within output) |" if codex else ""),
        "|---|---:|---:|---:|---:|" + ("---:|" if codex else ""),
    ]
    categories = ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens")
    for arm in ("off", "brain"):
        block = summary["all_attempts"][arm]
        cells = []
        for name in (*categories, "reasoning_output_tokens") if codex else categories:
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
        lines.append(f"| {task_name(comparison['task_id'])} | " + " | ".join(cells) + " |")
    if suite:
        lines += [
            "", "The same both-pass pairs by provider token category. Cache reads dominate processed tokens and are priced "
            "differently, so they are compared separately.", "",
            "| Both-pass comparison | Ordinary input | Cache creation | Cache reads | Output |",
            "|---|---:|---:|---:|---:|",
        ]
        for comparison in comparisons:
            cells = []
            for name in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"):
                metric = comparison["metrics"][name]
                percent = metric["comparison"]["reduction_percent"] if metric["comparison"] is not None else None
                cells.append((f"{percent:+.1f}%" if percent is not None else "undefined") + f" (n={metric['pair_count']})")
            lines.append(f"| {task_name(comparison['task_id'])} | " + " | ".join(cells) + " |")
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
    if suite:
        setup_description = ("Fixtures were exported from the operator's repositories at each base commit with `git archive`, "
                             "committed as one-commit repositories, built offline and locked where they have Rust dependencies, "
                             "and indexed by the pinned release. Export, index/snapshot and dependency-build seconds are parts "
                             "of fixture setup and must not be added to it. A snapshot refreshed for a new release replaces "
                             "the index/snapshot time only.")
        setup_header = "| Task | Fixture setup seconds | Index/snapshot seconds | Dependency build seconds | Export seconds | Indexed source nodes |"
        private_dependency_field = "export_seconds"
    elif followup:
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
        lines.append(f"| {task_name(fixture['task_id'])} | {format_number(fixture['setup_seconds'], decimals=1)} | {format_number(fixture['index_seconds'], decimals=1)} | {format_number(fixture['dependency_seconds'], decimals=1)} | {format_number(fixture[private_dependency_field], decimals=1)} | {format_number(fixture['source_counts']['nodes'])} |")
    fixture_overhead = summary["overhead"]["fixtures"]
    setup_label = "Measured fixture preparation" if suite else "Measured collection fixture setup" if followup else "Observed one-time fixture setup"
    private_label = ("Export from the operator's repositories" if suite else "Inherited seed private-cache preparation (earlier pilot)"
                     if followup else "Additional private dependency preparation")
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
    if suite:
        lines += context_markdown(summary, task_name)
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
        if field == "infrastructure_attempts" and summary.get("inspections"):
            lines += ["", "Recorded inspections of attempts the collector stopped on:", ""]
            for entry in summary["inspections"]:
                outcome = ("kept here as a separate infrastructure attempt, outside the paired matrix; its slot ran again"
                           if entry["decision"] == "separate_infrastructure_attempt"
                           else "kept in the paired matrix as an unscorable outcome; collection continued with the next planned attempt")
                lines.append(f"- `{entry['attempt_id']}` ({entry['inspected_at'] or 'time unrecorded'}): {outcome}. Reason: {entry['reason']}")
    if suite:
        tasks = COUNT_WORDS.get(len(suite["task_ids"]), str(len(suite["task_ids"])))
        repetitions = COUNT_WORDS.get(suite["repetitions"], str(suite["repetitions"]))
        size_line = f"- This is a {tasks}-task, {repetitions}-repetition diagnostic, not a general productivity estimate or a statistically conclusive trial."
        dependency_line = f"- Dependencies: {manifest.get('platform_dependencies', 'unavailable')}. Both arms use the same prepared environment."
    else:
        size_line = "- This is a four-task, three-repetition diagnostic, not a general productivity estimate or a statistically conclusive trial."
        dependency_line = "- Platform dependencies use the prepared installed cache and differ from the historical lockfile; both arms use the same prepared environment."
    lines += [
        "", "## Scope and limitations", "",
        size_line,
        "- The model and effort are pinned. Model drift or missing brain delivery is classified as infrastructure failure rather than a successful Engram trial.",
        ("- Codex reports no USD estimate, so USD columns are unavailable; Codex input includes cached input and output includes reasoning, mapped as described in real_repo_metrics.parse_codex_trace." if codex
         else "- Claude-reported USD figures are usage estimates under a subscription, not incremental cash charges or billing receipts."),
        dependency_line,
        ("- Engram uses source-only historical indexes with automatic Codex hooks and the engram MCP server, and no accumulated learned memories. This does not measure the full second-brain configuration." if codex
         else "- Engram uses source-only historical indexes with automatic hooks, no accumulated learned memories, and no MCP tools. This does not measure the full second-brain configuration."),
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
    suite = suite_definition(data, manifest_raw)
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
                    context = None
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
                        if suite and safe_identity:
                            trial = data.resolve() / "trials" / identity
                            if trial.is_dir() and trial.resolve() == trial:
                                context = trial_context(trial, record.get("arm"))
                    records.append(redact_record(record, context))
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
    planned_tasks = suite["task_ids"] if suite else TASK_NAMES
    task_ids = sorted(set(planned_tasks) | {f["task_id"] for f in fixtures} | {r["task_id"] for r in records})
    expected_keys = {(task_id, repetition, arm) for task_id in planned_tasks for repetition in range(1, repetitions + 1) for arm in ("off", "brain")}
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
    private_dependency_field = ("export_seconds" if suite else "inherited_seed_private_dependency_seconds"
                                if manifest.get("suite") == "real-repo-v2" else "private_dependency_seconds")
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
    if suite:
        summary["suite"] = suite
        summary["context_delivery"] = context_summary(records)
    inspections = read_inspections(data)
    if inspections:
        summary["inspections"] = inspections
    summary["input_warnings"] = warnings
    generated_at = datetime.now(timezone.utc).isoformat()
    result = {"schema_version": 1, "record_classification_revision": RECORD_CLASSIFICATION_REVISION, "generated_at": generated_at, "progress": summary["progress"], "manifest": manifest, "fixtures": fixtures, "records": records, "infrastructure_attempts": infrastructure_records, "preflight_runs": preflight_records}
    analysis_scripts = ["report_real_repo_pilot.py", "real_repo_metrics.py", "real_repo_diagnostics.py", "real_repo_adjudications.py"]
    if suite:
        result["suite"] = suite
        analysis_scripts += ["real_repo_context.py", "eval_context_quality.py"]
    if inspections:
        result["inspections"] = inspections
    result["analysis_provenance"] = {
        name: hashlib.sha256(Path(__file__).with_name(name).read_bytes()).hexdigest() for name in analysis_scripts
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
    (output / "README.md").write_text(markdown(summary, manifest, fixtures, artifacts, suite))
    return summary["progress"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    print(json.dumps(generate_report(args.data, args.output)))


if __name__ == "__main__":
    main()
