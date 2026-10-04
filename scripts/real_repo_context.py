#!/usr/bin/env python3
"""Context-delivery and Read measurements for real-repository trials; counts only.

Two kinds of trial evidence are measured, and no text from either is published:

- The Claude stream-JSON trace: native Read requests, how many named a line
  range (offset or limit), and the UTF-8 bytes of the text Read returned.
- The Brain arm's preserved Engram state: the contexts confirmed delivered by
  hook receipts (as runner.receipt_summary counts them), classified line by line
  as source-map pointers: ranged or not, direct, neighbor or related use. Related-
  use items are delivered context items of type code_use.

A measurement that the evidence cannot support stays null instead of zero. Off
attempts have no Engram context by design, so their delivery counts are zero.
"""
from __future__ import annotations

import contextlib
import json
from pathlib import Path
import shutil
import sqlite3
import sys
import tempfile

sys.dont_write_bytecode = True
from eval_context_quality import pointer_counts  # noqa: E402

SCHEMA_VERSION = 1
READ_FIELDS = ("read_calls", "ranged_read_calls", "read_results", "read_error_results", "read_result_bytes")
DELIVERY_FIELDS = ("delivered_contexts", "pointer_lines", "ranged_pointer_lines", "direct_pointer_lines",
                   "neighbor_pointer_lines", "related_use_pointer_lines", "related_use_items")
# The same acknowledged, non-empty Brain contexts that runner.receipt_summary counts.
DELIVERED_RUNS = ("SELECT DISTINCT c.id, c.stable_prefix, c.dynamic_context FROM usage_events u "
                  "JOIN context_runs c ON c.id=u.context_run_id JOIN tasks t ON t.id=u.source_id "
                  "WHERE u.source_type='task' AND u.usage_kind='context_served' AND c.mode='brain' "
                  "AND length(c.stable_prefix || c.dynamic_context)>0")


def _text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(block["text"] for block in content
                       if isinstance(block, dict) and block.get("type") == "text" and isinstance(block.get("text"), str))
    return None


def read_measurements(trace):
    """Count Read requests and the bytes of text they returned, from a Claude trace."""
    calls, ranged, results = {}, 0, {}
    malformed = 0
    with Path(trace).open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            if not line.strip():
                continue
            try:
                event = json.loads(line)
            except ValueError:
                malformed += 1
                continue
            if not isinstance(event, dict):
                malformed += 1
                continue
            message = event.get("message")
            blocks = message.get("content") if isinstance(message, dict) else None
            if event.get("type") == "stream_event":
                partial = event.get("event")
                blocks = [partial.get("content_block")] if isinstance(partial, dict) and partial.get("type") == "content_block_start" else None
            for block in blocks if isinstance(blocks, list) else []:
                if not isinstance(block, dict):
                    continue
                identity = block.get("id") if block.get("type") == "tool_use" else block.get("tool_use_id")
                if block.get("type") == "tool_use" and block.get("name") == "Read" and isinstance(identity, str):
                    if identity not in calls:
                        payload = block.get("input") if isinstance(block.get("input"), dict) else {}
                        calls[identity] = payload.get("offset") is not None or payload.get("limit") is not None
                        ranged += calls[identity]
                elif block.get("type") == "tool_result" and isinstance(identity, str):
                    results.setdefault(identity, []).append(block)
    read_results = {identity: blocks for identity, blocks in results.items() if identity in calls}
    errors = sum(any(block.get("is_error") is True for block in blocks) for blocks in read_results.values())
    sizes = [_text(block.get("content")) for blocks in read_results.values() for block in blocks if block.get("is_error") is not True]
    complete = (not malformed and set(read_results) == set(calls)
                and all(len(blocks) == 1 for blocks in read_results.values()) and all(text is not None for text in sizes))
    return {"read_calls": len(calls), "ranged_read_calls": ranged, "read_results": len(read_results),
            "read_error_results": errors,
            "read_result_bytes": sum(len(text.encode()) for text in sizes if text is not None),
            "read_measurement_complete": complete}


@contextlib.contextmanager
def evidence_database(database):
    """Open preserved state without writing to it, including rows still in its write-ahead log.

    A cleanly stopped daemon leaves no WAL, and the file is read in place as
    immutable. Otherwise the database and its WAL are copied to a temporary
    directory first, because an immutable read would ignore the WAL.
    """
    database = Path(database).resolve()
    wal = database.with_name(database.name + "-wal")
    if not wal.is_file() or wal.stat().st_size == 0:
        with contextlib.closing(sqlite3.connect(f"file:{database}?mode=ro&immutable=1", uri=True)) as con:
            yield con
        return
    with tempfile.TemporaryDirectory(prefix="engram-context-evidence-") as scratch:
        copy = Path(scratch) / database.name
        shutil.copyfile(database, copy)
        shutil.copyfile(wal, copy.with_name(copy.name + "-wal"))
        with contextlib.closing(sqlite3.connect(copy)) as con:
            yield con


def delivered_context(database):
    """Classify the pointers in contexts that hook receipts confirm were delivered."""
    with evidence_database(database) as con:
        runs = con.execute(DELIVERED_RUNS).fetchall()
        counts = {"delivered_contexts": len(runs), "pointer_lines": 0, "ranged_pointer_lines": 0, "direct_pointer_lines": 0,
                  "neighbor_pointer_lines": 0, "related_use_pointer_lines": 0}
        for _, prefix, dynamic in runs:
            found = pointer_counts(f"{prefix or ''}\n{dynamic or ''}")
            counts["pointer_lines"] += found["pointer_lines"]
            counts["ranged_pointer_lines"] += found["ranged_pointer_lines"]
            counts["direct_pointer_lines"] += found["direct_pointer_lines"]
            counts["related_use_pointer_lines"] += found["related_use_lines"]
            counts["neighbor_pointer_lines"] += found["pointer_lines"] - found["direct_pointer_lines"] - found["related_use_lines"]
        identities = [run[0] for run in runs]
        counts["related_use_items"] = sum(
            con.execute("SELECT count(*) FROM context_items WHERE context_run_id=? AND source_type='code_use'",
                        (identity,)).fetchone()[0] for identity in identities)
    return counts


def trial_context(trial, arm):
    """All context measurements for one preserved trial directory; null where evidence is missing."""
    result = {"schema_version": SCHEMA_VERSION, **dict.fromkeys(READ_FIELDS), "read_measurement_complete": False,
              **dict.fromkeys(DELIVERY_FIELDS)}
    trace = trial / "trace.jsonl"
    if trace.is_file():
        result.update(read_measurements(trace))
    if arm == "off":
        result.update(dict.fromkeys(DELIVERY_FIELDS, 0))
    elif (trial / "state/brain.sqlite").is_file():
        try:
            result.update(delivered_context(trial / "state/brain.sqlite"))
        except sqlite3.Error:
            pass
    return result


def share(numerator, denominator):
    return numerator / denominator if numerator is not None and denominator else None
