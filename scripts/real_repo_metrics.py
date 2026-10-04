"""Provider telemetry and paired task comparisons for real-repository trials.

No token counts are inferred from transcript text. Cost is the CLI's reported
USD estimate. Paired reductions use successful runs on both arms only.
Claude Code stream-JSON traces and Codex exec/rollout transcripts map onto one
record schema; see parse_trace and parse_codex_trace.
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
# Reported separately: a subset of output tokens (Codex). Claude traces omit it.
REASONING_FIELD = "reasoning_output_tokens"
MEASURES = (
    *TOKEN_FIELDS,
    REASONING_FIELD,
    "processed_input_tokens",
    "total_tokens",
    "tool_calls",
    "agent_wall_seconds",
    "cost_usd_estimate",
)
# Provider usage measures: excluded from paired savings when usage may be partial.
USAGE_MEASURES = frozenset({*TOKEN_FIELDS, REASONING_FIELD, "processed_input_tokens", "total_tokens", "cost_usd_estimate"})
RECORD_CLASSIFICATION_REVISION = "agent-completion-v2"

# Codex rollout counters, cumulative per session file. Codex input includes
# cached input, and output includes reasoning output.
CODEX_COUNTERS = (
    "input_tokens",
    "cached_input_tokens",
    "cache_write_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
)
CODEX_CORE_COUNTERS = ("input_tokens", "cached_input_tokens", "output_tokens")
# Model-issued tool requests in rollout `response_item` rows.
CODEX_TOOL_CALL_TYPES = frozenset({
    "function_call", "custom_tool_call", "local_shell_call", "web_search_call",
    "tool_search_call", "image_generation_call", "computer_call",
})
CODEX_SHELL_TOOLS = frozenset({"shell", "shell_command", "exec_command", "exec", "local_shell_call"})


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


def _codex_counters(usage: Any) -> dict[str, int]:
    """Known non-negative integer counters from one cumulative usage object."""
    if not isinstance(usage, dict):
        return {}
    known = {name: _number(usage.get(name), integer=True) for name in CODEX_COUNTERS}
    return {name: value for name, value in known.items() if value is not None}


def _codex_counter_delta(previous: dict[str, int], current: dict[str, int]):
    """Turn one cumulative row into an additive increment, as src/usage_capture.rs does.

    A decrease in any known counter is a reset: it becomes the new baseline and
    contributes no usage for the gap. Counters absent from a row keep their
    previous value. Identical rows contribute nothing.
    """
    if any(name in previous and current[name] < previous[name] for name in current):
        previous.clear()
        previous.update(current)
        return "reset", None
    delta = {name: current[name] - previous.get(name, 0) for name in current}
    previous.update(current)
    if any(value > 0 for value in delta.values()):
        return "increment", delta
    return "unchanged", None


def codex_thread_kind(payload: dict) -> str:
    """Classify a rollout's leading session_meta like src/usage_capture.rs.

    Returns `root`, `spawned` (a worker another thread started) or
    `internal:<label>` for Codex-internal sub-sessions such as the guardian
    reviewer, review, compact or memory consolidation. Every kind's usage is
    real usage; the label only explains where it came from.
    """
    thread_source = payload.get("thread_source")
    if thread_source == "guardian_review":
        return "internal:guardian"
    source = payload.get("source") if isinstance(payload.get("source"), dict) else {}
    if "subagent" in source:
        subagent = source["subagent"]
        if isinstance(subagent, dict) and "thread_spawn" in subagent:
            return "spawned"
        if isinstance(subagent, dict):
            other = subagent.get("other")
            # serde_json maps iterate in sorted key order.
            return "internal:" + (other if isinstance(other, str) else min(subagent, default="subagent"))
        return "internal:" + (subagent if isinstance(subagent, str) else "subagent")
    if "internal" in source:
        internal = source["internal"]
        return "internal:" + (internal if isinstance(internal, str) else "internal")
    if thread_source == "subagent":
        return "spawned"
    if thread_source == "memory_consolidation":
        return "internal:memory_consolidation"
    return "root"


def _jsonl_rows(path: Path, digest):
    """Yield (line_number, row or None) for a JSONL file, hashing raw bytes."""
    with path.open("rb") as stream:
        for line_number, raw_line in enumerate(stream, 1):
            digest.update(raw_line)
            line = raw_line.decode("utf-8", errors="replace")
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except (ValueError, TypeError):
                yield line_number, None
                continue
            yield line_number, row if isinstance(row, dict) else None


def parse_codex_trace(events_path: str | Path, session_paths: list[str | Path] | tuple = ()) -> dict[str, Any]:
    """Parse one `codex exec --json` event log and the run's rollout session files.

    Provider usage comes from rollout `token_count` rows, which are cumulative per
    session file: only increments are summed and a decrease starts a new
    baseline without inventing usage, as in src/usage_capture.rs. Codex input
    includes cached input and output includes reasoning output, so the shared
    fields are mapped to keep their Claude meaning: input_tokens is uncached
    input (input - cached - cache_write), cache_read is cached input,
    cache_creation is reported cache writes, processed input is Codex input and
    total is input + output. Reasoning output is reported separately and is
    already inside output. Tool calls are model-issued function, custom and
    shell calls deduplicated by call_id. Codex reports no cost estimate.
    Missing files raise OSError; unknown usage stays None, never zero.
    """
    events_digest = hashlib.sha256()
    malformed_lines = []
    final_events = []
    event_usage = None
    error_messages = []
    thread_ids = []
    for line_number, event in _jsonl_rows(Path(events_path), events_digest):
        if event is None:
            malformed_lines.append(line_number)
            continue
        kind = event.get("type")
        if kind == "thread.started" and isinstance(event.get("thread_id"), str):
            thread_ids.append(event["thread_id"])
        elif kind in ("turn.completed", "turn.failed"):
            final_events.append(kind)
            if kind == "turn.completed" and isinstance(event.get("usage"), dict):
                event_usage = {name: _number(value, integer=True) for name, value in event["usage"].items()}
            if kind == "turn.failed":
                error = event.get("error")
                message = error.get("message") if isinstance(error, dict) else None
                error_messages.append(message[:200] if isinstance(message, str) else "turn failed")
        elif kind == "error":
            message = event.get("message")
            error_messages.append(message[:200] if isinstance(message, str) else "error")

    totals: dict[str, int] = {}
    # A counter absent from a middle row is still covered by the next row's
    # delta; it is unknown only if a session's final row lacks it.
    unreported_at_end = Counter()
    increments = unchanged = resets = token_rows = malformed_session_lines = 0
    complete_sessions = 0
    session_digests = {}
    tools: dict[str, str] = {}
    conflicting_tool_ids = set()
    missing_tool_ids = 0
    models = set()
    providers = set()
    session_kinds = Counter()
    operations = Counter()
    task_completions = 0
    record_usage: dict[str, dict[str, int]] = {}
    for session_path in map(Path, session_paths):
        digest = hashlib.sha256()
        previous: dict[str, int] = {}
        last_row: dict[str, int] = {}
        saw_usage = classified = False
        for _, row in _jsonl_rows(session_path, digest):
            if row is None:
                malformed_session_lines += 1
                continue
            kind = row.get("type")
            payload = row.get("payload") if isinstance(row.get("payload"), dict) else {}
            payload_type = payload.get("type")
            if kind == "session_meta":
                if isinstance(payload.get("model_provider"), str):
                    providers.add(payload["model_provider"])
                if not classified:
                    session_kinds[codex_thread_kind(payload)] += 1
                    classified = True
            elif kind == "turn_context":
                if isinstance(payload.get("model"), str) and payload["model"]:
                    models.add(payload["model"])
            elif kind == "event_msg" and payload_type == "token_count":
                info = payload.get("info")
                current = _codex_counters(info.get("total_token_usage") if isinstance(info, dict) else None)
                if not current:
                    continue
                token_rows += 1
                saw_usage = True
                last_row = current
                outcome, delta = _codex_counter_delta(previous, current)
                if outcome == "reset":
                    resets += 1
                elif outcome == "unchanged":
                    unchanged += 1
                else:
                    increments += 1
                    for name, value in delta.items():
                        totals[name] = totals.get(name, 0) + value
            elif kind == "event_msg" and payload_type == "task_complete":
                task_completions += 1
            elif kind == "event_msg" and payload_type == "item_completed":
                item = payload.get("item")
                if isinstance(item, dict) and isinstance(item.get("type"), str):
                    operations[item["type"]] += 1
            elif kind == "token_usage_record":
                response_id = payload.get("response_id")
                usage = _codex_counters(payload.get("usage"))
                if isinstance(response_id, str) and usage:
                    record_usage[response_id] = usage
            elif kind == "response_item" and payload_type in CODEX_TOOL_CALL_TYPES:
                call_id = payload.get("call_id") or payload.get("id")
                if not isinstance(call_id, str) or not call_id:
                    missing_tool_ids += 1
                    continue
                name = payload.get("name") if payload_type in ("function_call", "custom_tool_call") else payload_type
                name = name if isinstance(name, str) and name else "<unknown>"
                if call_id in tools and tools[call_id] != name:
                    conflicting_tool_ids.add(call_id)
                else:
                    tools[call_id] = name
        session_digests[session_path.name] = digest.hexdigest()
        if saw_usage:
            unreported_at_end.update(name for name in CODEX_COUNTERS if name not in last_row)
            if all(name in last_row for name in CODEX_CORE_COUNTERS):
                complete_sessions += 1

    def known(name):
        return totals.get(name, 0) if token_rows and not unreported_at_end[name] else None

    raw = {name: known(name) for name in CODEX_COUNTERS}
    raw_input, cached, cache_write, output = (raw["input_tokens"], raw["cached_input_tokens"],
                                              raw["cache_write_input_tokens"], raw["output_tokens"])
    uncached = None
    if raw_input is not None and cached is not None:
        uncached = raw_input - cached - (cache_write or 0)
        uncached = uncached if uncached >= 0 else None
    total = raw_input + output if raw_input is not None and output is not None else None
    sessions = len(session_digests)
    trace_complete = bool(final_events)
    usage_complete = (bool(sessions) and complete_sessions == sessions and not resets
                      and not malformed_session_lines and all(raw[name] is not None for name in CODEX_CORE_COUNTERS))
    tool_complete = (trace_complete and bool(sessions) and not malformed_lines and not malformed_session_lines
                     and not missing_tool_ids and not conflicting_tool_ids)
    record_totals = None
    if record_usage:
        record_totals = {name: sum(usage.get(name, 0) for usage in record_usage.values()) for name in CODEX_CORE_COUNTERS}
    last = final_events[-1] if final_events else None
    return {
        "agent": "codex",
        "input_tokens": uncached,
        "output_tokens": output,
        "cache_creation_input_tokens": cache_write,
        "cache_read_input_tokens": cached,
        REASONING_FIELD: raw["reasoning_output_tokens"],
        "processed_input_tokens": raw_input,
        "total_tokens": total,
        "provider_usage": raw,
        "cost_usd_estimate": None,
        "model_usage": None,
        "models": sorted(models),
        "providers": sorted(providers),
        "tool_calls": len(tools),
        "shell_tool_calls": sum(name in CODEX_SHELL_TOOLS for name in tools.values()),
        "shell_spawn_failure_count": None,
        "counts_by_tool": dict(sorted(Counter(tools.values()).items())),
        "operation_counts": dict(sorted(operations.items())),
        "tool_use_ids": sorted(tools),
        "tool_calls_complete": tool_complete,
        "num_turns": increments if token_rows else None,
        "result_subtype": {"turn.completed": "success", "turn.failed": "error"}.get(last),
        # Retried stream errors followed by a completed turn are kept as
        # diagnostics; the final turn event decides the outcome.
        "is_error": (last == "turn.failed") if trace_complete else (True if error_messages else None),
        "error_messages": error_messages,
        "permission_denials": None,
        "trace_complete": trace_complete,
        "usage_complete": usage_complete,
        "diagnostics": {
            "trace_sha256": events_digest.hexdigest(),
            "session_sha256": session_digests,
            "session_files": sessions,
            "session_kinds": dict(sorted(session_kinds.items())),
            "thread_ids": thread_ids,
            "result_count": len(final_events),
            "task_completions": task_completions,
            "malformed_line_numbers": malformed_lines,
            "malformed_session_lines": malformed_session_lines,
            "tool_blocks_without_ids": missing_tool_ids,
            "conflicting_tool_ids": sorted(conflicting_tool_ids),
            "token_count_rows": token_rows,
            "usage_increments": increments,
            "unchanged_usage_rows": unchanged,
            "counter_resets": resets,
            "event_usage": event_usage,
            "usage_record_totals": record_totals,
            "usage_sources_agree": (None if record_totals is None or not usage_complete
                                    else all(record_totals[name] == raw[name] for name in CODEX_CORE_COUNTERS)),
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
    if complete_only and record.get("usage_may_be_partial") is True and name in USAGE_MEASURES:
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
            or (name in USAGE_MEASURES and record.get("usage_may_be_partial") is True)
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
