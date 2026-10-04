#!/usr/bin/env python3
"""Read-only workflow diagnostics for native Claude traces; no source or payload output.

These observations explain workflow, not correctness or token savings. Shell
commands are opaque. First edit means the first native edit request; edited
files require a non-error tool result. Missing dispatch timestamps stay unknown.
"""
from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from datetime import datetime
import json
from pathlib import Path
import posixpath
import re
import statistics
import sys
from typing import Any

SEARCH_READ_TOOLS = frozenset({"Read", "Grep", "Glob"})
EDIT_TOOLS = frozenset({"Edit", "Write", "MultiEdit", "NotebookEdit"})
SAFE_NAME = re.compile(r"[A-Za-z0-9_.:-]{1,128}\Z")


def _label(value: Any) -> str:
    return value if isinstance(value, str) and SAFE_NAME.fullmatch(value) else "unknown"


def _timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str):
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return parsed if parsed.tzinfo is not None and parsed.utcoffset() is not None else None
    except ValueError:
        return None


def _paths(block: dict, cwd: str | None) -> tuple[set[str], bool]:
    """Normalize lexically only. Never resolve symlinks or read candidate files."""
    value = block.get("input")
    if not isinstance(value, dict):
        return set(), False
    paths = []
    for key in ("file_path", "notebook_path"):
        if isinstance(value.get(key), str) and value[key].strip():
            paths.append(value[key])
    if block.get("name") == "MultiEdit" and isinstance(value.get("edits"), list):
        for edit in value["edits"]:
            if isinstance(edit, dict) and isinstance(edit.get("file_path"), str) and edit["file_path"].strip():
                paths.append(edit["file_path"])
    relative_without_cwd = any(not posixpath.isabs(path) and cwd is None for path in paths)
    return {posixpath.normpath(posixpath.join(cwd, path) if cwd and not posixpath.isabs(path) else path) for path in paths}, relative_without_cwd


def _final_error(final: dict | None, count: int) -> dict:
    if final is None:
        return {"present": False, "result_count": count, "is_error": None, "provider_error": None, "classification": "unknown", "api_error_status": None}
    is_error = final.get("is_error") if isinstance(final.get("is_error"), bool) else None
    status = final.get("api_error_status")
    status = status if isinstance(status, int) and not isinstance(status, bool) and 400 <= status <= 599 else None
    subtype, terminal = final.get("subtype"), final.get("terminal_reason")
    provider_error = terminal == "api_error" or subtype == "api_error" or status is not None
    if provider_error:
        classification = "provider_error"
    elif subtype in ("error_max_turns", "error_max_budget_usd", "error_max_structured_output_retries"):
        classification = "agent_limit"
    elif is_error is True:
        classification = "other_error"
    elif is_error is False and subtype == "success":
        classification = "successful_result"
    else:
        classification = "unknown"
    return {"present": True, "result_count": count, "is_error": is_error,
            "provider_error": True if provider_error else False if classification in {"agent_limit", "successful_result"} else None,
            "classification": classification, "api_error_status": status}


def parse_trace(path: str | Path, *, agent_started_at: str | None = None) -> dict:
    """Return payload-free observations; optional start must be real dispatch time.

    Do not pass record.started_at: the pilot sets it before fixture setup. Native
    traces usually lack a dispatch timestamp. In that case elapsed_seconds is
    null and only the explicitly named first-observed-event offset is available.
    """
    tools, results = {}, defaultdict(set)
    malformed, missing_ids, missing_result_ids, unidentified_errors = 0, 0, 0, 0
    name_conflicts, path_conflicts = set(), set()
    relative_paths_without_cwd = set()
    invalid_timestamps, backwards_timestamps = 0, 0
    first_timestamp = previous_timestamp = init_timestamp = None
    cwd = None
    final, final_count = None, 0

    def add_tool(block, timestamp):
        nonlocal missing_ids
        if not isinstance(block, dict) or block.get("type") != "tool_use":
            return
        identity = block.get("id")
        if not isinstance(identity, str) or not identity:
            missing_ids += 1
            return
        name = _label(block.get("name"))
        if identity not in tools:
            tools[identity] = {"name": name, "ordinal": len(tools) + 1, "timestamp": timestamp, "paths": set()}
        tool = tools[identity]
        if tool["name"] != name:
            name_conflicts.add(identity)
        if tool["timestamp"] is None and timestamp is not None:
            tool["timestamp"] = timestamp
        if name in EDIT_TOOLS:
            paths, unresolved = _paths(block, cwd)
            if unresolved:
                relative_paths_without_cwd.add(identity)
            if paths:
                if tool["paths"] and tool["paths"] != paths:
                    path_conflicts.add(identity)
                elif not tool["paths"]:
                    tool["paths"] = paths

    with Path(path).open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            if not line.strip():
                continue
            try:
                event = json.loads(line)
                if not isinstance(event, dict):
                    raise ValueError("event must be an object")
            except (ValueError, TypeError):
                malformed += 1
                continue
            timestamp = _timestamp(event.get("timestamp"))
            if "timestamp" in event and timestamp is None:
                invalid_timestamps += 1
            if timestamp is not None:
                first_timestamp = first_timestamp or timestamp
                if previous_timestamp is not None and timestamp < previous_timestamp:
                    backwards_timestamps += 1
                previous_timestamp = timestamp
            if event.get("type") == "system" and event.get("subtype") == "init":
                candidate = event.get("cwd")
                if isinstance(candidate, str) and posixpath.isabs(candidate):
                    cwd = candidate
                init_timestamp = timestamp
            if event.get("type") == "result":
                final, final_count = event, final_count + 1
            message = event.get("message")
            if event.get("type") in {"assistant", "user"} and isinstance(message, dict):
                blocks = message.get("content")
                for block in blocks if isinstance(blocks, list) else []:
                    if not isinstance(block, dict):
                        continue
                    if event["type"] == "assistant":
                        add_tool(block, timestamp)
                    elif block.get("type") == "tool_result":
                        identity = block.get("tool_use_id")
                        is_error = block.get("is_error", False)
                        if not isinstance(identity, str) or not identity:
                            missing_result_ids += 1
                            unidentified_errors += is_error is True
                        else:
                            results[identity].add(is_error if isinstance(is_error, bool) else None)
            if event.get("type") == "stream_event":
                partial = event.get("event")
                if isinstance(partial, dict) and partial.get("type") == "content_block_start":
                    add_tool(partial.get("content_block"), timestamp)

    first_edit = next((tool for tool in tools.values() if tool["name"] in EDIT_TOOLS), None)
    ordered = not (malformed or missing_ids or name_conflicts)
    chronology = not (invalid_timestamps or backwards_timestamps)
    explicit_start = _timestamp(agent_started_at)
    origin = explicit_start if agent_started_at is not None else init_timestamp
    basis = "agent_started_at" if explicit_start else "timestamped_system_init" if init_timestamp and agent_started_at is None else None
    edit_timestamp = first_edit["timestamp"] if first_edit else None
    timing_valid = ordered and chronology and edit_timestamp is not None
    elapsed = (edit_timestamp - origin).total_seconds() if timing_valid and origin is not None and origin <= edit_timestamp else None
    first_observed_offset = (edit_timestamp - first_timestamp).total_seconds() if timing_valid and first_timestamp is not None and first_timestamp <= edit_timestamp else None
    edit_requests = {identity: tool for identity, tool in tools.items() if tool["name"] in EDIT_TOOLS}
    confirmed_files = set()
    for identity, tool in edit_requests.items():
        if results.get(identity) == {False} and identity not in name_conflicts | path_conflicts:
            confirmed_files.update(tool["paths"])
    edit_missing_result = sum(identity not in results for identity in edit_requests)
    edit_missing_path = sum(not tool["paths"] for tool in edit_requests.values())
    conflicting_results = sum(len(values) > 1 for values in results.values())
    unknown_result_status = sum(None in values for values in results.values())
    unmatched_results = len(set(results) - set(tools))
    complete = bool(final_count) and ordered and not unmatched_results
    return {
        "schema_version": 1,
        "scope": "explanatory_native_tool_workflow_only",
        "tool_calls": len(tools),
        "tool_calls_by_name": dict(sorted(Counter(tool["name"] for tool in tools.values()).items())),
        "tool_calls_complete": complete,
        "search_read_calls_total": sum(tool["name"] in SEARCH_READ_TOOLS for tool in tools.values()),
        "search_read_calls_before_first_edit": sum(tool["name"] in SEARCH_READ_TOOLS and tool["ordinal"] < first_edit["ordinal"] for tool in tools.values()) if first_edit and ordered else None,
        "first_edit": {"observed": first_edit is not None, "tool_ordinal": first_edit["ordinal"] if first_edit and ordered else None,
                       "elapsed_seconds": elapsed, "elapsed_basis": basis if elapsed is not None else None,
                       "since_first_timestamped_event_seconds": first_observed_offset},
        "tool_error_responses": sum(True in values for values in results.values()),
        "tool_results_observed": len(results),
        "tool_errors_complete": complete and not (missing_result_ids or conflicting_results or unknown_result_status) and set(tools) <= set(results),
        "edited_unique_file_count": len(confirmed_files),
        "edited_file_count_complete": complete and not (edit_missing_result or edit_missing_path or path_conflicts or relative_paths_without_cwd or missing_result_ids or conflicting_results or unknown_result_status),
        "final_result": _final_error(final, final_count),
        "coverage": {"malformed_lines": malformed, "tool_blocks_without_ids": missing_ids, "tool_name_conflicts": len(name_conflicts),
                     "edit_path_conflicts": len(path_conflicts), "relative_edit_paths_without_cwd": len(relative_paths_without_cwd),
                     "tool_results_without_ids": missing_result_ids, "unidentified_tool_error_responses": unidentified_errors,
                     "unmatched_tool_result_ids": unmatched_results, "conflicting_tool_results": conflicting_results,
                     "unknown_tool_result_statuses": unknown_result_status, "edit_requests_without_result": edit_missing_result,
                     "edit_requests_without_path": edit_missing_path, "invalid_timestamps": invalid_timestamps,
                     "backwards_timestamps": backwards_timestamps, "dispatch_timestamp_available": origin is not None,
                     "invalid_supplied_dispatch_timestamp": agent_started_at is not None and explicit_start is None},
    }


def diagnose_trial(record: dict, trace: str | Path, *, agent_started_at: str | None = None) -> dict:
    """Attach only safe grouping labels; ignore prompts, paths, tokens and grades."""
    repetition = record.get("repetition")
    repetition = repetition if isinstance(repetition, int) and not isinstance(repetition, bool) else None
    return {"trial_id": _label(record.get("id")), "task_id": _label(record.get("task_id")),
            "repetition": repetition, "arm": record.get("arm") if record.get("arm") in ("off", "brain") else "unknown",
            "workflow": parse_trace(trace, agent_started_at=agent_started_at)}


def _distribution(values: list[float | int], total: int) -> dict:
    return {"available": len(values), "missing": total - len(values),
            "mean": statistics.mean(values) if values else None, "median": statistics.median(values) if values else None}


def _group(trials: list[dict]) -> dict:
    workflows = [trial["workflow"] for trial in trials if isinstance(trial.get("workflow"), dict)]
    counts = Counter()
    for workflow in workflows:
        counts.update(workflow["tool_calls_by_name"])
    metrics = {
        "search_read_calls_before_first_edit": [w["search_read_calls_before_first_edit"] for w in workflows],
        "first_edit_tool_ordinal": [w["first_edit"]["tool_ordinal"] for w in workflows],
        "first_edit_elapsed_seconds": [w["first_edit"]["elapsed_seconds"] for w in workflows],
        "first_edit_since_first_timestamped_event_seconds": [w["first_edit"]["since_first_timestamped_event_seconds"] for w in workflows],
    }
    return {"trials": len(trials), "traces_available": len(workflows), "tool_calls_by_name_observed": dict(sorted(counts.items())),
            "tool_calls_observed": sum(w["tool_calls"] for w in workflows), "complete_tool_count_traces": sum(w["tool_calls_complete"] for w in workflows),
            "tool_error_responses_observed": sum(w["tool_error_responses"] for w in workflows),
            "complete_tool_error_traces": sum(w["tool_errors_complete"] for w in workflows),
            "edited_unique_files_per_trial_sum": sum(w["edited_unique_file_count"] for w in workflows),
            "complete_edited_file_count_traces": sum(w["edited_file_count_complete"] for w in workflows),
            "final_result_classifications": dict(sorted(Counter(w["final_result"]["classification"] for w in workflows).items())),
            "distributions": {name: _distribution([value for value in values if value is not None], len(trials)) for name, values in metrics.items()}}


def summarize_trials(trials: list[dict]) -> dict:
    """Aggregate counts/distributions by arm and task; no savings calculation."""
    arms, tasks = defaultdict(list), defaultdict(lambda: defaultdict(list))
    for trial in trials:
        arm = trial.get("arm") if trial.get("arm") in ("off", "brain") else "unknown"
        arms[arm].append(trial)
        tasks[_label(trial.get("task_id"))][arm].append(trial)
    return {"schema_version": 1, "scope": "explanatory_diagnostics_not_correctness_or_token_outcomes",
            "by_arm": {arm: _group(group) for arm, group in sorted(arms.items())},
            "by_task": [{"task_id": task, "by_arm": {arm: _group(group) for arm, group in sorted(groups.items())}} for task, groups in sorted(tasks.items())],
            "limitations": ["Bash searches and edits are opaque; native tool names define these categories.",
                            "First edit is the first edit request, which can fail; edited file counts require non-error results.",
                            "First observed timestamp excludes unobserved dispatch/initial response time and is labeled separately.",
                            "File identities are normalized lexically, without symlink resolution; only counts are emitted.",
                            "Errors count explicit tool-result error flags, not exit codes inferred from arbitrary output."]}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data", required=True, type=Path)
    parser.add_argument("--output", type=Path, help="Write a new derived JSON file; existing paths are refused. Omit to print JSON")
    args = parser.parse_args()
    try:
        records = [json.loads(line) for line in (args.data / "records.jsonl").read_text().splitlines() if line.strip()]
        trials = []
        for record in records:
            identity = record.get("id")
            if not isinstance(identity, str) or not SAFE_NAME.fullmatch(identity) or identity in {".", ".."}:
                raise ValueError("invalid trial identity")
            try:
                trial = diagnose_trial(record, args.data / "trials" / identity / "trace.jsonl")
            except OSError:
                trial = {"trial_id": _label(identity), "task_id": _label(record.get("task_id")),
                         "arm": record.get("arm") if record.get("arm") in ("off", "brain") else "unknown",
                         "workflow": None, "trace_unavailable": True}
            trials.append(trial)
        report = {"trials": trials, "summary": summarize_trials(trials)}
        payload = json.dumps(report, indent=2) + "\n"
        if args.output:
            with args.output.open("x", encoding="utf-8") as stream:
                stream.write(payload)
        else:
            print(payload, end="")
        return 0
    except (OSError, ValueError, TypeError, AttributeError):
        print("Could not read pilot records or write diagnostics; check the supplied locations and JSON format.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
