"""Provider telemetry and paired task comparisons for real-repository trials.

No token counts are inferred from transcript text. Cost is the CLI's reported
USD estimate. Paired reductions use successful runs on both arms only.
"""

from __future__ import annotations

from collections import Counter, defaultdict
from copy import deepcopy
import hashlib
import json
import math
from pathlib import Path
from typing import Any

TOKEN_FIELDS = (
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
)
MEASURES = (
    *TOKEN_FIELDS,
    "processed_input_tokens",
    "total_tokens",
    "tool_calls",
    "agent_wall_seconds",
    "cost_usd_estimate",
)
RECORD_CLASSIFICATION_REVISION = "agent-completion-v2"


def _number(value: Any, *, integer: bool = False):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    if not math.isfinite(value) or value < 0 or (integer and not isinstance(value, int)):
        return None
    return value


def _shell_spawn_argument_limit(content: Any) -> bool:
    """Recognize the observed sandbox launch diagnostic, not command output."""
    if isinstance(content, list):
        content = "\n".join(block["text"] for block in content if isinstance(block, dict)
                            and block.get("type") == "text" and isinstance(block.get("text"), str))
    return (isinstance(content, str) and content.startswith("Could not start ")
            and ": the command line plus environment exceed the OS exec argument limit (E2BIG). At spawn:" in content
            and "The Bash sandbox profile adds " in content
            and "filesystem deny paths to every command." in content)


def parse_trace(path: str | Path) -> dict[str, Any]:
    """Parse one Claude stream-JSON log; absent final usage stays unknown.

    Tool calls are requested tool_use blocks, deduplicated by ID across assistant
    messages and optional partial-stream events. Missing IDs and malformed lines
    make the observed tool count incomplete. Missing files raise OSError.
    """
    final = None
    result_count = 0
    malformed_lines = []
    missing_tool_ids = 0
    conflicting_tool_ids = set()
    tools = {}
    models = set()
    failed_shell_ids = set()
    trace_digest = hashlib.sha256()

    def add_tool(block):
        nonlocal missing_tool_ids
        if not isinstance(block, dict) or block.get("type") != "tool_use":
            return
        tool_id, name = block.get("id"), block.get("name")
        if not isinstance(tool_id, str) or not tool_id:
            missing_tool_ids += 1
            return
        name = name if isinstance(name, str) and name else "<unknown>"
        if tool_id in tools and tools[tool_id] != name:
            conflicting_tool_ids.add(tool_id)
        else:
            tools[tool_id] = name

    with Path(path).open("rb") as trace:
        for line_number, raw_line in enumerate(trace, 1):
            trace_digest.update(raw_line)
            line = raw_line.decode("utf-8", errors="replace")
            if not line.strip():
                continue
            try:
                event = json.loads(line)
                if not isinstance(event, dict):
                    raise ValueError("Expected an event object")
            except (ValueError, TypeError):
                malformed_lines.append(line_number)
                continue
            if event.get("type") == "result":
                final = event
                result_count += 1
            if event.get("type") == "assistant":
                message = event.get("message")
                if isinstance(message, dict):
                    if isinstance(message.get("model"), str):
                        models.add(message["model"])
                    content = message.get("content", [])
                    if isinstance(content, list):
                        for block in content:
                            add_tool(block)
            if event.get("type") == "stream_event":
                partial = event.get("event")
                if isinstance(partial, dict) and partial.get("type") == "content_block_start":
                    add_tool(partial.get("content_block"))
            if event.get("type") == "user":
                message = event.get("message")
                content = message.get("content", []) if isinstance(message, dict) else []
                if isinstance(content, list):
                    for block in content:
                        if (isinstance(block, dict) and block.get("type") == "tool_result"
                                and block.get("is_error") is True
                                and isinstance(block.get("tool_use_id"), str)
                                and _shell_spawn_argument_limit(block.get("content"))):
                            failed_shell_ids.add(block["tool_use_id"])

    final = final or {}
    usage = final.get("usage")
    usage = usage if isinstance(usage, dict) else {}
    tokens = {name: _number(usage.get(name), integer=True) for name in TOKEN_FIELDS}
    inputs = [tokens[name] for name in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")]
    processed_input = sum(inputs) if all(value is not None for value in inputs) else None
    total = processed_input + tokens["output_tokens"] if processed_input is not None and tokens["output_tokens"] is not None else None
    model_usage = final.get("modelUsage")
    model_usage = model_usage if isinstance(model_usage, dict) else None
    if model_usage is not None:
        models.update(model_usage)
    denials = final.get("permission_denials")
    tool_complete = bool(result_count) and not (malformed_lines or missing_tool_ids or conflicting_tool_ids)
    return {
        **tokens,
        "processed_input_tokens": processed_input,
        "total_tokens": total,
        "cost_usd_estimate": _number(final.get("total_cost_usd")),
        "model_usage": model_usage,
        "models": sorted(models),
        "tool_calls": len(tools),
        "shell_tool_calls": sum(name == "Bash" for name in tools.values()),
        "shell_spawn_failure_count": sum(tools.get(tool_id) == "Bash" and tool_id not in conflicting_tool_ids
                                         for tool_id in failed_shell_ids),
        "counts_by_tool": dict(sorted(Counter(tools.values()).items())),
        "tool_use_ids": sorted(tools),
        "tool_calls_complete": tool_complete,
        "num_turns": _number(final.get("num_turns"), integer=True),
        "result_subtype": final.get("subtype"),
        "is_error": final.get("is_error") if isinstance(final.get("is_error"), bool) else None,
        "permission_denials": denials if isinstance(denials, list) else None,
        "trace_complete": bool(result_count),
        "usage_complete": all(value is not None for value in tokens.values()),
        "diagnostics": {
            "trace_sha256": trace_digest.hexdigest(),
            "result_count": result_count,
            "malformed_line_numbers": malformed_lines,
            "tool_blocks_without_ids": missing_tool_ids,
            "conflicting_tool_ids": sorted(conflicting_tool_ids),
        },
    }


def apply_tool_runtime_checks(record: dict[str, Any], trace_path: str | Path) -> dict[str, Any]:
    """Exclude observed Bash launch failures without changing captured usage.

    The trace is the authority; captured metric counters alone are not evidence.
    No failure returns an unchanged deep copy, including classification revision
    and IDs. Call before completion classification. Missing traces raise OSError.
    """
    if not isinstance(record, dict):
        raise TypeError("Record must be an object")
    result = deepcopy(record)
    parsed = parse_trace(trace_path)
    if not parsed["shell_spawn_failure_count"]:
        return result
    reason = "shell_spawn_argument_limit"
    if "behavioral_grade" not in result:
        result["behavioral_grade"] = deepcopy(result.get("grade"))
    issues = list(result.get("validity_issues") or [])
    if reason not in issues:
        issues.append(reason)
    result["validity_issues"] = issues
    result["grade"] = {"status": "infrastructure_error", "issues": list(issues)}
    result["tool_runtime_evidence"] = {
        "schema_version": 1, "trace_sha256": parsed["diagnostics"]["trace_sha256"],
        "reason": reason, "shell_tool_calls": parsed["shell_tool_calls"],
        "shell_spawn_failure_count": parsed["shell_spawn_failure_count"],
    }
    # A failed tool spawn does not imply incomplete final provider usage.
    return result


def classify_record(record: dict[str, Any]) -> dict[str, Any]:
    """Derive completion eligibility without changing captured trial evidence.

    Behavioral correctness alone cannot make a runtime error a clean completion.
    Explicit budget exhaustion is a failed attempt unless infrastructure already
    invalidates the trial. Missing telemetry alone does not create errors.
    """
    if not isinstance(record, dict):
        raise TypeError("Record must be an object")
    result = deepcopy(record)
    result["record_classification_revision"] = RECORD_CLASSIFICATION_REVISION
    metrics = result.get("metrics")
    metrics = metrics if isinstance(metrics, dict) else {}
    subtype = metrics.get("result_subtype")
    budget_exhausted = result.get("agent_timed_out") is True or (isinstance(subtype, str) and subtype.startswith("error_max_"))
    exit_code = result.get("agent_exit_code")
    nonzero_exit = isinstance(exit_code, int) and not isinstance(exit_code, bool) and exit_code != 0
    runtime_error = metrics.get("is_error") is True or nonzero_exit
    if budget_exhausted or runtime_error:
        existing_infrastructure = _grade(result) == "infra" or any(
            issue in {"model_drift", "no_delivery_receipt"}
            for issue in (result.get("validity_issues") or [])
        )
        if "behavioral_grade" not in result:
            result["behavioral_grade"] = deepcopy(result.get("grade"))
        issue = "agent_budget_exhausted" if budget_exhausted else "agent_runtime_error"
        issues = list(result.get("validity_issues") or [])
        if issue not in issues:
            issues.append(issue)
        result["validity_issues"] = issues
        status = "fail" if budget_exhausted and not existing_infrastructure else "infrastructure_error"
        result["grade"] = {"status": status, "issues": list(issues)}
        result["usage_may_be_partial"] = True
    return result


def _grade(record):
    grade = record.get("grade")
    value = grade.get("status") if isinstance(grade, dict) else None
    return "infra" if value in {"infra", "infra_error", "infrastructure_error"} else value if value in {"pass", "fail"} else "unknown"


def _measure(record, name, *, complete_only=True):
    if name == "agent_wall_seconds":
        return _number(record.get(name))
    metrics = record.get("metrics")
    if not isinstance(metrics, dict):
        return None
    if complete_only and name == "tool_calls" and metrics.get("tool_calls_complete") is False:
        return None
    # Keep lower bounds in all-attempt totals, but never treat them as savings.
    if (complete_only and record.get("usage_may_be_partial") is True
            and name in {*TOKEN_FIELDS, "processed_input_tokens", "total_tokens", "cost_usd_estimate"}):
        return None
    return _number(metrics.get(name), integer=name != "cost_usd_estimate")


def _all_attempts(records):
    coverage, totals = {}, {}
    for name in MEASURES:
        reported = [record for record in records if _measure(record, name, complete_only=False) is not None]
        known = [_measure(record, name, complete_only=False) for record in reported]
        totals[name] = sum(known) if known else None
        partial = sum(
            (name == "tool_calls" and (record.get("metrics") or {}).get("tool_calls_complete") is False)
            or (name in {*TOKEN_FIELDS, "processed_input_tokens", "total_tokens", "cost_usd_estimate"}
                and record.get("usage_may_be_partial") is True)
            for record in reported
        )
        coverage[name] = {"available": len(known), "missing": len(records) - len(known), "total": len(records), "partial": partial}
    return {
        "attempts": len(records),
        "grade_counts": dict(Counter(_grade(record) for record in records)),
        "behavioral_grade_counts": dict(Counter(_grade({"grade": record.get("behavioral_grade", record.get("grade"))}) for record in records)),
        "reported_usage_may_be_partial_attempts": sum(record.get("usage_may_be_partial") is True for record in records),
        "totals_observed": totals,
        "measurement_coverage": coverage,
    }


def _delta(off, brain):
    delta = brain - off
    return {
        "off": off,
        "brain": brain,
        "delta": delta,
        "delta_percent": 100 * delta / off if off else None,
        "reduction": -delta,
        "reduction_percent": -100 * delta / off if off else None,
    }


def _aggregate_success_pairs(pairs):
    metrics = {}
    for name in MEASURES:
        measured = [pair["deltas"][name] for pair in pairs if pair["deltas"][name] is not None]
        metrics[name] = {
            "pair_count": len(measured),
            "missing_pair_count": len(pairs) - len(measured),
            "comparison": _delta(sum(p["off"] for p in measured), sum(p["brain"] for p in measured)) if measured else None,
        }
    return {"pair_count": len(pairs), "metrics": metrics}


def paired_summary(records: list[dict[str, Any]]) -> dict[str, Any]:
    """Compare off/brain records sharing task_id and repetition.

    Records are classified for clean completion before aggregation.
    All attempts retain observed cost, including failures and infrastructure
    errors. A duplicated arm makes its pair ambiguous and excludes that pair
    from reductions. Percentages are ratios of matched sums, not means of
    individual percentages; a zero off denominator has no percentage.
    """
    groups = defaultdict(lambda: defaultdict(list))
    invalid_records = []
    by_arm = {arm: [] for arm in ("off", "brain", "unknown")}
    for index, record in enumerate(records):
        if not isinstance(record, dict):
            raise TypeError(f"Record {index} must be an object")
        record = classify_record(record)
        arm = record.get("arm")
        by_arm[arm if arm in {"off", "brain"} else "unknown"].append(record)
        task_id, repetition = record.get("task_id"), record.get("repetition")
        valid_repetition = isinstance(repetition, (str, int)) and not isinstance(repetition, bool)
        if arm not in {"off", "brain"} or not isinstance(task_id, str) or not task_id or not valid_repetition:
            invalid_records.append(index)
            continue
        groups[(task_id, repetition)][arm].append(record)

    counts = {name: 0 for name in ("complete_pairs", "both_pass", "off_only_pass", "brain_only_pass", "neither_pass", "infrastructure", "incomplete", "ambiguous")}
    pairs = []
    successful_by_task = defaultdict(list)
    complete_metric_coverage = {name: 0 for name in MEASURES}
    for (task_id, repetition), arms in sorted(groups.items(), key=lambda item: (item[0][0], str(item[0][1]))):
        pair = {"task_id": task_id, "repetition": repetition, "arm_attempt_counts": {arm: len(arms.get(arm, [])) for arm in ("off", "brain")}}
        if any(len(attempts) > 1 for attempts in arms.values()):
            pair["status"] = "ambiguous"
        elif not all(arm in arms for arm in ("off", "brain")):
            pair["status"] = "incomplete"
        else:
            counts["complete_pairs"] += 1
            off, brain = arms["off"][0], arms["brain"][0]
            off_grade, brain_grade = _grade(off), _grade(brain)
            pair["grades"] = {"off": off_grade, "brain": brain_grade}
            pair["behavioral_grades"] = {arm: _grade({"grade": rec.get("behavioral_grade", rec.get("grade"))}) for arm, rec in (("off", off), ("brain", brain))}
            if any(grade in {"infra", "unknown"} for grade in (off_grade, brain_grade)):
                pair["status"] = "infrastructure"
            elif off_grade == brain_grade == "pass":
                pair["status"] = "both_pass"
            elif off_grade == "pass":
                pair["status"] = "off_only_pass"
            elif brain_grade == "pass":
                pair["status"] = "brain_only_pass"
            else:
                pair["status"] = "neither_pass"
            measured = {}
            for name in MEASURES:
                left, right = _measure(off, name), _measure(brain, name)
                valid = left is not None and right is not None
                complete_metric_coverage[name] += int(valid)
                measured[name] = _delta(left, right) if valid else None
            if pair["status"] == "both_pass":
                pair["deltas"] = measured
                successful_by_task[task_id].append(pair)
        counts[pair["status"]] += 1
        pairs.append(pair)

    successful = [pair for pair in pairs if pair["status"] == "both_pass"]
    return {
        "schema_version": 1,
        "record_classification_revision": RECORD_CLASSIFICATION_REVISION,
        "record_count": len(records),
        "all_attempts": {arm: _all_attempts(attempts) for arm, attempts in by_arm.items()},
        "pair_counts": {"observed_pairs": len(pairs), **counts},
        "pairs": pairs,
        "both_pass": {
            "aggregate": _aggregate_success_pairs(successful),
            "per_task": [{"task_id": task_id, **_aggregate_success_pairs(task_pairs)} for task_id, task_pairs in sorted(successful_by_task.items())],
        },
        "measurement_coverage": {
            "complete_pairs": counts["complete_pairs"],
            "both_pass_pairs": len(successful),
            "complete_pairs_with_both_measurements": complete_metric_coverage,
            "invalid_record_indices": invalid_records,
        },
        "interpretation": "All-attempt totals include observed failed-run and infrastructure-run costs. Reductions use only both-pass pairs with both measurements present and not marked partial; missing measurements are not zero and failed-run cost reductions are not savings.",
    }
