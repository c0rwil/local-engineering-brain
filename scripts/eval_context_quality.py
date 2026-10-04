#!/usr/bin/env python3
"""Evaluate source navigation on a frozen SQLite index using an isolated daemon.

Run under caffeinate -dims on macOS. This script never resolves/registers a
repository and never reads existing task prompts or source file contents. It
exports authored queries, source identities, paths, kinds, ranks and counters;
compiled context text remains private to the temporary evaluation process.
"""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import time
import uuid

# Do not create repository-local Python cache files when importing the adapter.
sys.dont_write_bytecode = True
from capture_eval import UnixHTTP, api


SOURCE_KINDS = {"code", "code_dependency", "verification"}
DEFINITION_KINDS = {"Function", "Type", "Interface", "Module"}


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def read_only(path):
    return sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)


def freeze_snapshot(source, snapshot):
    """Use SQLite backup, never a raw copy of a live WAL database."""
    if source.resolve() == snapshot.resolve():
        raise ValueError("snapshot must differ from the source database")
    descriptor = os.open(snapshot, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    os.close(descriptor)
    try:
        with read_only(source) as original, sqlite3.connect(snapshot) as frozen:
            original.backup(frozen)
        snapshot.chmod(0o400)
    except BaseException:
        snapshot.unlink(missing_ok=True)
        raise


def health(socket_path):
    client = UnixHTTP(str(socket_path))
    client.timeout = 2
    client.request("GET", "/api/v1/health")
    response = client.getresponse()
    body = json.loads(response.read())
    client.close()
    if response.status != 200:
        raise RuntimeError(f"health returned HTTP {response.status}")
    return body


def wait_ready(process, socket_path):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"isolated daemon exited with code {process.returncode}")
        try:
            return health(socket_path)
        except (OSError, ValueError, RuntimeError):
            time.sleep(0.05)
    raise TimeoutError("isolated daemon did not become ready within 20 seconds")


def config_text(database, socket_path, vault):
    # JSON strings are valid TOML basic strings for these generated local paths.
    return f"""workspace = "default"
[storage]
db_path = {json.dumps(str(database))}
[daemon]
socket_path = {json.dumps(str(socket_path))}
[web]
enabled = false
[watch]
enabled = false
[jobs]
enabled = false
[vault]
enabled = false
root = {json.dumps(str(vault))}
[judge]
enabled = false
[architect]
auto = "off"
[network]
local_only = true
"""


def isolate_checkout_aliases(database):
    """Keep linked graph scope, but prevent linked-memory live Git discovery."""
    with sqlite3.connect(database) as connection:
        aliases = connection.execute("SELECT count(*) FROM project_aliases").fetchone()[0]
        connection.execute("DELETE FROM project_aliases")
    return aliases


def fixtures(database, cases):
    projects = {}
    definitions = {}
    for case in cases:
        name = case["project"]
        if name not in projects:
            rows = database.execute(
                "SELECT id FROM projects WHERE name=? AND tombstoned_at_ms IS NULL", (name,)
            ).fetchall()
            if len(rows) != 1:
                raise ValueError(f"expected exactly one active indexed project named {name!r}")
            projects[name] = rows[0][0]
        if expected := case.get("expected_path"):
            rows = database.execute(
                "SELECT id FROM nodes WHERE project_id=? AND file_path=? AND label=? "
                "AND kind IN ('Function','Type','Interface','Module') AND tombstoned_at_ms IS NULL",
                (projects[name], expected, case["definition_label"]),
            ).fetchall()
            if not rows:
                raise ValueError(f"expected definition is absent from the snapshot: {case['id']}")
            definitions[case["id"]] = [row[0] for row in rows]
        if case.get("expected_no_source") and "/" in case["query"]:
            present = database.execute(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE project_id=? AND file_path=? "
                "AND tombstoned_at_ms IS NULL)", (projects[name], case["query"])
            ).fetchone()[0]
            if present:
                raise ValueError(f"negative path probe exists in the snapshot: {case['id']}")
    return projects, definitions


def describe_items(database, response, expected_ids):
    selected = []
    source_rank = 0
    for rank, item in enumerate(response.get("items", []), 1):
        source_type = item["source_type"]
        node = database.execute(
            "SELECT n.file_path,n.kind,p.name FROM nodes n JOIN projects p ON p.id=n.project_id "
            "WHERE n.id=?", (item["source_id"],)
        ).fetchone()
        is_source = source_type in SOURCE_KINDS or node is not None
        if is_source:
            source_rank += 1
        selected.append({
            "rank": rank, "source_rank": source_rank if is_source else None,
            "source_type": source_type, "source_id": item["source_id"],
            "path": node[0] if node else None, "indexed_kind": node[1] if node else None,
            "source_project": node[2] if node else None,
            "estimated_tokens": item["tokens"],
            "expected_definition": item["source_id"] in expected_ids,
        })
    return selected


def run_trial(database, socket_path, case, project_id, definition_ids, repeat, budget, mode):
    request = {
        "request_id": f"context_quality_{case['id']}_{repeat}_{mode}_{uuid.uuid4()}",
        "project_id": project_id, "task_text": case["query"],
        "current_paths": case.get("current_paths", []), "token_budget": budget,
        "mode": mode, "repository_root": None, "head": None, "task_id": None,
    }
    result = {"repeat": repeat, "mode": mode, "token_budget": budget}
    started = time.perf_counter()
    try:
        response = api(str(socket_path), "/api/v1/context/compile", request)
        result["api_latency_ms"] = round((time.perf_counter() - started) * 1000, 3)
        selected = describe_items(database, response, definition_ids)
        sources = [item for item in selected if item["source_rank"] is not None]
        top5 = sources[:5]
        expected = case.get("expected_path")
        text = response["stable_prefix"] + response["dynamic_context"]
        metrics = response["metrics"]
        result.update({
            "ok": True, "compile_ms": metrics["compile_ms"],
            "selected_tokens": metrics["selected_tokens"],
            "emitted_bytes": len(text.encode()), "selected_items": selected,
            "source_pointer_count": len(sources),
            "source_estimated_tokens": sum(item["estimated_tokens"] for item in sources),
            "target_path_hit_at5": any(item["path"] == expected and item["source_project"] == case["project"] for item in top5) if expected else None,
            "expected_definition_hit_at5": any(item["expected_definition"] for item in top5) if expected else None,
            "target_path_emitted": expected in text if expected else None,
            "unmatched_filler_source_items": len(sources) if case.get("expected_no_source") else None,
            "budget_pass": metrics["selected_tokens"] <= budget,
            "off_control_pass": not text and not selected and metrics["selected_tokens"] == 0 if mode == "off" else None,
        })
    except Exception as error:
        result.update({
            "ok": False, "error_type": type(error).__name__, "error": str(error),
            "api_latency_ms": round((time.perf_counter() - started) * 1000, 3),
            "budget_pass": False, "off_control_pass": False if mode == "off" else None,
            "target_path_hit_at5": False if case.get("expected_path") else None,
        })
    return result


def summary(results):
    brain = [trial for case in results for trial in case["trials"] if trial["mode"] == "brain"]
    off = [trial for case in results for trial in case["trials"] if trial["mode"] == "off"]
    targets = [trial for trial in brain if trial.get("target_path_hit_at5") is not None]
    negative = [trial for case in results if case.get("expected_no_source") for trial in case["trials"] if trial["mode"] == "brain"]
    successful = [trial for trial in brain if trial["ok"]]
    stable_cases = 0
    for case in results:
        trials = [trial for trial in case["trials"] if trial["mode"] == "brain"]
        signatures = {
            json.dumps([trial.get("selected_items"), trial.get("selected_tokens")], sort_keys=True)
            for trial in trials
        }
        stable_cases += bool(trials) and all(trial["ok"] for trial in trials) and len(signatures) == 1
    return {
        "cases": len(results), "brain_trials": len(brain), "off_trials": len(off),
        "repeat_selection_consistent_cases": stable_cases,
        "failed_brain_trials": sum(not trial["ok"] for trial in brain),
        "target_trials": len(targets), "target_path_hits_at5": sum(bool(trial.get("target_path_hit_at5")) for trial in targets),
        "target_path_hit_rate_at5": sum(bool(trial.get("target_path_hit_at5")) for trial in targets) / len(targets) if targets else None,
        "expected_definition_hits_at5": sum(bool(trial.get("expected_definition_hit_at5")) for trial in targets),
        "negative_trials": len(negative),
        "negative_no_source_passes": sum(trial["ok"] and trial.get("unmatched_filler_source_items") == 0 for trial in negative),
        "unmatched_filler_source_items": sum(trial.get("unmatched_filler_source_items", 0) or 0 for trial in negative),
        "budget_passes": sum(trial["budget_pass"] for trial in brain),
        "off_control_passes": sum(bool(trial.get("off_control_pass")) for trial in off),
        "source_pointers": sum(trial.get("source_pointer_count", 0) for trial in brain),
        "selected_tokens": sum(trial.get("selected_tokens", 0) for trial in brain),
        "api_latency_ms_median_successful": statistics.median(trial["api_latency_ms"] for trial in successful) if successful else None,
        "api_latency_ms_max_including_failures": max((trial["api_latency_ms"] for trial in brain), default=None),
        "compile_ms_median_successful": statistics.median(trial["compile_ms"] for trial in successful) if successful else None,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", type=Path, required=True)
    parser.add_argument("--snapshot", type=Path, required=True)
    parser.add_argument("--freeze-from", type=Path, help="create a new read-only backup; refuses to replace an existing snapshot")
    parser.add_argument("--cases", type=Path, default=Path(__file__).resolve().parents[1] / "tests/fixtures/context_quality_cases.json")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    if not 1 <= args.repeats <= 10:
        parser.error("--repeats must be between 1 and 10")
    if args.freeze_from:
        freeze_snapshot(args.freeze_from, args.snapshot)
    snapshot_hash = sha256(args.snapshot)
    daemon = args.daemon.resolve(strict=True)
    cases_document = json.loads(args.cases.read_text())
    cases = cases_document["cases"]
    if len({case["id"] for case in cases}) != len(cases):
        parser.error("case IDs must be unique")
    with read_only(args.snapshot) as frozen:
        projects, definitions = fixtures(frozen, cases)
        migration = frozen.execute("SELECT max(version) FROM schema_migrations").fetchone()[0]
    result = {
        "schema_version": 2, "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "snapshot": {"sha256": snapshot_hash, "bytes": args.snapshot.stat().st_size, "schema_migration": migration},
        "case_sha256": sha256(args.cases), "daemon_sha256": sha256(daemon),
        "harness_sha256": sha256(Path(__file__)),
        "daemon_release": daemon.parent.parent.name, "repeats": args.repeats,
        "isolation": {"repository_root": None, "head": None, "live_repository_resolution": False, "background_jobs": False, "watch": False, "web": False, "vault": False, "judge": False, "architect": "off"},
        "definitions_verified_in_snapshot": definitions,
        "limitations": [
            "Synthetic source-navigation probes over a real frozen index; not an end-to-end coding or savings benchmark.",
            "Authored queries and expected paths are fixed before comparing binaries; no existing task text or raw context/source contents are exported.",
            "Top five means the first five emitted source items in compiler order, including dependency/verification pointers; it excludes learned-memory items. Path hits require the expected project as well as path.",
            "A path hit does not prove the correct symbol was selected; expected-definition hits are reported separately. Multiple symbols in a correct file remain a path hit.",
            "Unmatched filler is scored only on explicit negative probes; other extra pointers are not automatically labeled irrelevant.",
            "Latency is measured locally with no LLM. All fresh-request trials, including initial cold queries and failed trials, are retained; medians explicitly cover successful calls only.",
            "The same frozen index is copied for each invocation; migrations and context persistence affect only the disposable copy. Repository files and the snapshot are never modified by the evaluation daemon.",
            "Project checkout aliases are removed from the disposable copy before startup so linked-memory retrieval cannot inspect live Git state. Indexed project links remain available to both binaries.",
        ],
    }
    with tempfile.TemporaryDirectory(prefix="ecq-", dir="/tmp") as temp:
        root = Path(temp)
        copied = root / "brain.sqlite"
        shutil.copyfile(args.snapshot, copied)
        copied.chmod(0o600)
        result["isolation"]["checkout_aliases_removed"] = isolate_checkout_aliases(copied)
        socket_path = root / "daemon.sock"
        configuration = config_text(copied, socket_path, root / "vault")
        config_path = root / "config.toml"
        config_path.write_text(configuration)
        config_path.chmod(0o600)
        # Paths differ per run; this hash describes the equivalent tuning.
        result["config_sha256"] = hashlib.sha256(config_text(Path("DATABASE"), Path("SOCKET"), Path("VAULT")).encode()).hexdigest()
        environment = {key: value for key, value in os.environ.items() if not key.startswith("ENGRAM_")}
        with (root / "daemon.stderr").open("wb") as stderr:
            process = subprocess.Popen([str(daemon), "--config", str(config_path)], cwd=root, env=environment, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=stderr)
            try:
                ready = wait_ready(process, socket_path)
                if Path(ready["database"]).resolve() != copied.resolve():
                    raise RuntimeError("daemon health did not match isolated database")
                result["daemon_build_id"] = ready["build_id"]
                with read_only(copied) as database:
                    results = [{**case, "trials": []} for case in cases]
                    for repeat in range(1, args.repeats + 1):
                        for case_result in results:
                            budget = case_result.get("token_budget", cases_document["default_token_budget"])
                            for mode in ("brain", "off"):
                                case_result["trials"].append(run_trial(database, socket_path, case_result, projects[case_result["project"]], definitions.get(case_result["id"], []), repeat, budget, mode))
                    result["results"] = results
                    result["summary"] = summary(results)
            finally:
                if process.poll() is None:
                    try:
                        api(str(socket_path), "/api/v1/admin/shutdown", {})
                        process.wait(timeout=10)
                    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired):
                        process.terminate()
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait(timeout=5)
        result["snapshot_unchanged"] = sha256(args.snapshot) == snapshot_hash
        if not result["snapshot_unchanged"]:
            raise RuntimeError("frozen snapshot changed during evaluation")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result["summary"], indent=2))
    measured = result["summary"]
    if (measured["failed_brain_trials"]
            or measured["budget_passes"] != measured["brain_trials"]
            or measured["off_control_passes"] != measured["off_trials"]):
        sys.exit(1)


if __name__ == "__main__":
    main()
