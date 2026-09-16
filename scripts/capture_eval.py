#!/usr/bin/env python3
"""Read-only capture metrics plus local-daemon replay; never reads transcripts.

Use a WAL-safe `engram backup create` snapshot for reproducible capture metrics.
Replay calls the production context compiler and therefore records context usage.
No task text, evidence artifacts, credentials, or repository source are exported.
"""
import argparse
import datetime
import hashlib
import http.client
import json
from pathlib import Path
import re
import socket
import sqlite3
import subprocess
import uuid


def ratio(numerator, denominator):
    return numerator / denominator if denominator else None


def capture_metrics(db):
    db.row_factory = sqlite3.Row
    sessions = {}
    for task in db.execute("SELECT id, project_id, session_id, status, accepted_by_user, started_at_ms FROM tasks"):
        key = task["session_id"] or task["id"]
        group = sessions.setdefault(key, {"tasks": [], "memories": set(), "started_at_ms": task["started_at_ms"]})
        group["tasks"].append(dict(task))
        group["started_at_ms"] = min(group["started_at_ms"], task["started_at_ms"])
        group["memories"].update(row[0] for row in db.execute("SELECT memory_id FROM memory_sources WHERE task_id=?", (task["id"],)))
    memories = {row["id"]: dict(row) for row in db.execute("SELECT id, epistemic_state, status, known_from_ms FROM memories")}
    first_validation = {}
    creation_sources = {}
    for row in db.execute("SELECT payload_json, created_at_ms, event_type FROM events WHERE event_type IN ('MEMORY_STATE_CHANGED', 'LEARNING_DISTILLED') ORDER BY created_at_ms"):
        event = json.loads(row[0])["data"]
        if row[2] == "LEARNING_DISTILLED" and not event["reused_existing"]:
            creation_sources.setdefault(event["memory_id"], set(event["source_task_ids"]))
        elif row[2] == "MEMORY_STATE_CHANGED" and event["epistemic_state"] == "validated":
            first_validation.setdefault(event["memory_id"], row[1])
    def summarize(tasks):
        closed = [t for t in tasks if t["status"] != "open"]
        successes = [t for t in tasks if t["status"] == "success"]
        accepted = sum(t["accepted_by_user"] == 1 for t in closed)
        known = sum(t["accepted_by_user"] is not None for t in closed)
        abandoned = sum(t["status"] == "abandoned" for t in tasks)
        return {"tasks": len(tasks), "closed": len(closed), "successes": len(successes),
                "accepted_closed": accepted, "closed_with_acceptance_recorded": known,
                "accepted_share_of_closed": ratio(accepted, len(closed)),
                "acceptance_recorded_share_of_closed": ratio(known, len(closed)),
                "successes_without_acceptance": sum(t["accepted_by_user"] is None for t in successes),
                "abandoned": abandoned, "abandoned_task_rate": ratio(abandoned, len(tasks))}
    per_session = []
    for key, group in sorted(sessions.items()):
        sourced = group["memories"]
        task_ids = {task["id"] for task in group["tasks"]}
        ids = {memory_id for memory_id in sourced if creation_sources.get(memory_id, set()) & task_ids}
        validated = sum(memories[m]["epistemic_state"] == "validated" for m in ids)
        times = [first_validation[m] for m in ids if m in first_validation]
        per_session.append({"session_hash": hashlib.sha256(key.encode()).hexdigest()[:16],
            "grouping": "episode" if key.startswith("tsk_") else "session",
            **summarize(group["tasks"]), "sourced_memories": len(sourced),
            "sourced_memories_without_creation_provenance": len(sourced - creation_sources.keys()),
            "memories_created": len(ids), "validated_memories": validated,
            "candidate_to_validated_conversion": ratio(validated, len(ids)),
            "time_to_first_validated_memory_ms": max(0, min(times)-group["started_at_ms"]) if times else None})
    all_tasks = [task for group in sessions.values() for task in group["tasks"]]
    validated = sum(m["epistemic_state"] == "validated" for m in memories.values())
    return {"aggregate": {**summarize(all_tasks), "memories": len(memories), "validated_memories": validated,
            "candidate_to_validated_conversion": ratio(validated, len(memories))},
            "sessions": per_session,
            "limitations": ["Tasks without session_id are reported as separate episodes, not inferred sessions.",
                "Session creation attribution uses source tasks in the first non-reused LEARNING_DISTILLED event; later reuse receives no creation credit, including overlapping sessions. Directly created memories without this provenance are reported separately.",
                "A memory initially distilled from multiple sessions contributes to each source session; session counts must not be summed as unique memories. Current validated state is separate from first validation time.",
                "Unknown acceptance is not treated as approval. Unlabeled brief relevance is not scored."]}


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=120)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def api(path, route, payload):
    client = UnixHTTP(path)
    client.request("POST", route, json.dumps(payload), {"Content-Type": "application/json"})
    response = client.getresponse()
    body = json.loads(response.read())
    client.close()
    if response.status != 200:
        raise RuntimeError(f"local API {route} failed with HTTP {response.status}")
    return body


def replay(db, path, cases, projects):
    results = []
    for case in cases:
        name = projects[case["project"]]
        project = db.execute("SELECT id FROM projects WHERE name=? AND tombstoned_at_ms IS NULL", (name,)).fetchone()
        if project is None:
            raise ValueError(f"missing registered replay project: {case['project']}")
        aliases = [Path(row[0]) for row in db.execute("SELECT path FROM project_aliases WHERE project_id=? ORDER BY observed_at_ms, path", (project[0],))]
        def usable_checkout(checkout):
            if not checkout.is_dir():
                return False
            probe = subprocess.run(["git", "-C", str(checkout), "rev-parse", "--is-bare-repository"], capture_output=True, timeout=5, check=False)
            return probe.stdout.strip() != b"true"
        aliases = sorted((p for p in aliases if usable_checkout(p)), key=lambda p: p.joinpath(".git").is_file())
        if not aliases:
            raise ValueError(f"no existing checkout for replay project {case['project']}")
        resolved = api(path, "/api/v1/projects/resolve", {"path": str(aliases[0])})
        database_path = db.execute("PRAGMA database_list").fetchone()[2]
        if Path(resolved["database"]).resolve() != Path(database_path).resolve():
            raise ValueError("Replay requires --db to be the database served by --socket; use an isolated daemon for snapshot replay")
        request = {"request_id": "capture_replay_" + str(uuid.uuid4()), "project_id": project[0],
            "task_text": case["query"], "current_paths": [], "token_budget": 2000, "mode": "brain",
            "repository_root": resolved["root"], "head": (resolved.get("git") or {}).get("head")}
        body = api(path, "/api/v1/context/compile", request)
        memory_ids = [item["source_id"] for item in body["items"] if item["source_type"] == "memory"]
        claims = [db.execute("SELECT claim FROM memories WHERE id=?", (m,)).fetchone()[0] for m in memory_ids]
        retrieved = [m for m, claim in zip(memory_ids, claims) if all(re.search(pattern, claim, re.I) for pattern in case["expected_patterns"])]
        emitted = body["stable_prefix"] + body["dynamic_context"]
        matches = []
        for memory_id, claim in zip(memory_ids, claims):
            if memory_id not in retrieved:
                continue
            compact = db.execute("SELECT compact_text FROM memory_compact_injections WHERE memory_id=?", (memory_id,)).fetchone()
            # Check the bytes actually delivered: a compact line may truncate the lesson.
            delivered = claim if claim in emitted else (compact[0] if compact and compact[0] in emitted else "")
            if delivered and all(re.search(pattern, delivered, re.I) for pattern in case["expected_patterns"]):
                matches.append(memory_id)
        results.append({"case": case["id"], "received": bool(matches), "matching_memory_ids": matches, "retrieved_matching_memory_ids": retrieved,
            "injected_memory_count": len(memory_ids), "selected_tokens": body["metrics"]["selected_tokens"],
            "injected_bytes": len((body["stable_prefix"]+body["dynamic_context"]).encode())})
    return {"received": sum(r["received"] for r in results), "cases": len(results), "results": results}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--replay", type=Path)
    parser.add_argument("--socket")
    parser.add_argument("--project", action="append", default=[], help="replay alias=registered project name")
    parser.add_argument("--brief-labels", type=Path, help="JSON array of emitted brief items with relevant:true/false")
    args = parser.parse_args()
    db = sqlite3.connect(args.db.resolve().as_uri() + "?mode=ro", uri=True)
    db.execute("BEGIN")  # One read snapshot keeps all capture denominators consistent.
    result = {"generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(), **capture_metrics(db)}
    db.rollback()  # Replay must see the same current memory rows as the live compiler.
    labels = json.loads(args.brief_labels.read_text()) if args.brief_labels else []
    if any(type(item.get("relevant")) is not bool for item in labels):
        raise ValueError("every brief label must have relevant:true or false")
    false_positives = sum(not item["relevant"] for item in labels)
    result["brief_relevance"] = {"labeled_items": len(labels), "false_positives": false_positives if labels else None,
        "false_positive_rate": ratio(false_positives, len(labels))}
    if args.replay:
        if not args.socket:
            parser.error("--replay requires --socket for production context compilation")
        result["replay"] = replay(db, args.socket, json.loads(args.replay.read_text()), dict(p.split("=",1) for p in args.project))
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: result[key] for key in ("aggregate", "brief_relevance", "replay") if key in result}, indent=2))


if __name__ == "__main__":
    main()
