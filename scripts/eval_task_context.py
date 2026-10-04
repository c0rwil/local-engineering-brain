#!/usr/bin/env python3
"""Evaluate full-task source coverage on disposable historical repository copies.

No coding model is called. Existing pilot snapshots and checkouts remain intact.
Output contains source metadata and coverage, never compiled context or bodies.
"""

import argparse
import datetime
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import uuid

sys.dont_write_bytecode = True
from capture_eval import api  # noqa: E402
from eval_context_quality import config_text, isolate_checkout_aliases, read_only, sha256, wait_ready  # noqa: E402


def definitions(database, project, components):
    result = []
    for component in components:
        marks = ",".join("?" for _ in component["labels"])
        rows = database.execute(
            f"SELECT id FROM nodes WHERE project_id=? AND file_path=? AND label IN ({marks}) "
            "AND kind IN ('Function','Type','Interface','Module') AND tombstoned_at_ms IS NULL",
            [project, component["path"], *component["labels"]],
        ).fetchall()
        if not rows:
            raise ValueError(f"Expected component not indexed: {component}")
        result.append({**component, "ids": [row[0] for row in rows]})
    return result


def trial(database, socket, project, case, budget, repeat, mode):
    targets = definitions(database, project, case["components"])
    started = time.perf_counter()
    response = api(str(socket), "/api/v1/context/compile", {
        "request_id": str(uuid.uuid4()), "project_id": project,
        "task_text": case["query"], "current_paths": [], "token_budget": budget,
        "mode": mode, "repository_root": None, "head": None, "task_id": None,
    })
    elapsed = (time.perf_counter() - started) * 1000
    selected = []
    for item in response["items"]:
        row = database.execute(
            "SELECT project_id,file_path,kind,label FROM nodes WHERE id=?", (item["source_id"],),
        ).fetchone()
        if row:
            selected.append({"id": item["source_id"], "project_id": row[0], "path": row[1],
                             "kind": row[2], "label": row[3], "tokens": item["tokens"]})
    paths = sorted({target["path"] for target in targets})
    coverage = []
    for target in targets:
        ranks = [rank for rank, item in enumerate(selected, 1) if item["id"] in target["ids"]]
        coverage.append({**target, "rank": min(ranks, default=None)})
    text = response["stable_prefix"] + response["dynamic_context"]
    return {
        "repeat": repeat, "mode": mode, "selected": selected, "components": coverage,
        "required_files": paths,
        "files_at5": [path for path in paths if any(item["project_id"] == project and item["path"] == path for item in selected[:5])],
        "files_in_pack": [path for path in paths if any(item["project_id"] == project and item["path"] == path for item in selected)],
        "selected_tokens": response["metrics"]["selected_tokens"],
        "compile_ms": response["metrics"]["compile_ms"], "api_latency_ms": round(elapsed, 3),
        "budget_pass": response["metrics"]["selected_tokens"] <= budget,
        "off_pass": not text and not response["items"] and response["metrics"]["selected_tokens"] == 0 if mode == "off" else None,
        "negative_pass": not selected if case.get("expected_no_source") and mode == "brain" else None,
    }


def summarize(cases):
    trials = [trial for case in cases for trial in case["trials"] if trial["mode"] == "brain"]
    components = [component for trial in trials for component in trial["components"]]
    return {
        "brain_trials": len(trials), "components": len(components),
        "definitions_at5": sum(component["rank"] is not None and component["rank"] <= 5 for component in components),
        "definitions_in_pack": sum(component["rank"] is not None for component in components),
        "required_files": sum(len(trial["required_files"]) for trial in trials),
        "files_at5": sum(len(trial["files_at5"]) for trial in trials),
        "files_in_pack": sum(len(trial["files_in_pack"]) for trial in trials),
        "selected_tokens": sum(trial["selected_tokens"] for trial in trials),
        "budget_failures": sum(not trial["budget_pass"] for trial in trials),
        "negative_trials": sum(trial["negative_pass"] is not None for trial in trials),
        "negative_failures": sum(trial["negative_pass"] is False for trial in trials),
        "off_failures": sum(trial["off_pass"] is False for case in cases for trial in case["trials"]),
        "latency_ms_median": statistics.median(trial["api_latency_ms"] for trial in trials),
        "latency_ms_max": max(trial["api_latency_ms"] for trial in trials),
        "repeat_consistent_cases": sum(len({json.dumps(trial["selected"], sort_keys=True) for trial in case["trials"] if trial["mode"] == "brain"}) == 1 for case in cases),
    }


def evaluate_task(args, task, budget):
    fixture = args.fixtures / task["task_id"]
    snapshot = fixture / "snapshot.sqlite"
    original_hash = sha256(snapshot)
    ready = json.loads((fixture / "ready.json").read_text())
    if original_hash != ready["snapshot_sha256"]:
        raise ValueError("Original pilot snapshot no longer matches its frozen manifest")
    result = {"task_id": task["task_id"], "snapshot_sha256": original_hash,
              "base_commit": ready["base_commit"], "fixture_head": ready["fixture_head"]}
    with tempfile.TemporaryDirectory(prefix="etc-", dir="/tmp") as directory:
        root = Path(directory)
        copied = root / "brain.sqlite"
        shutil.copyfile(snapshot, copied)
        copied.chmod(0o600)
        isolate_checkout_aliases(copied)
        socket = root / "d.sock"
        config = root / "config.toml"
        config.write_text(config_text(copied, socket, root / "vault"))
        config.chmod(0o600)
        environment = {key: value for key, value in os.environ.items() if not key.startswith("ENGRAM_")}
        with (root / "daemon.log").open("wb") as log:
            process = subprocess.Popen([str(args.daemon), "--config", str(config)], cwd=root,
                                       env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            try:
                healthy = wait_ready(process, socket)
                if Path(healthy["database"]).resolve() != copied.resolve():
                    raise ValueError("Daemon did not open the isolated database")
                result["build_id"] = healthy["build_id"]
                if args.reindex:
                    checkout = root / "work"
                    subprocess.run(["git", "clone", "--quiet", "--no-hardlinks", str(fixture / "work"), str(checkout)], check=True)
                    head = subprocess.check_output(["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True).strip()
                    if head != ready["fixture_head"]:
                        raise ValueError("Disposable checkout does not match frozen fixture HEAD")
                    started = time.perf_counter()
                    indexed = api(str(socket), "/api/v1/projects/index", {
                        "path": str(checkout), "changed_paths": None, "provider": "tree_sitter",
                        "graphify_export": None, "ensure_fresh": True,
                    })
                    if indexed["resolved"]["project"]["id"] != ready["project_id"]:
                        raise ValueError("Reindex changed project identity")
                    result["reindex"] = {"seconds": round(time.perf_counter() - started, 3), **indexed["index"]}
                    isolate_checkout_aliases(copied)
                    # Query requests cannot read source files even accidentally.
                    shutil.rmtree(checkout)
                with read_only(copied) as database:
                    result["schema_version"] = database.execute("SELECT max(version) FROM schema_migrations").fetchone()[0]
                    columns = {row[1] for row in database.execute("PRAGMA table_info(nodes)")}
                    result["owners_with_aliases"] = database.execute("SELECT count(*) FROM nodes WHERE search_aliases<>'' AND tombstoned_at_ms IS NULL").fetchone()[0] if "search_aliases" in columns else 0
                    result["alias_bytes"] = database.execute("SELECT coalesce(sum(length(CAST(search_aliases AS BLOB))),0) FROM nodes WHERE tombstoned_at_ms IS NULL").fetchone()[0] if "search_aliases" in columns else 0
                    cases = [{**case, "trials": []} for case in task["cases"]]
                    for repeat in range(1, args.repeats + 1):
                        for case in cases:
                            for mode in ("brain", "off"):
                                case["trials"].append(trial(database, socket, ready["project_id"], case, budget, repeat, mode))
                    result["cases"] = cases
                    result["summary"] = summarize(cases)
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
        result["original_snapshot_unchanged"] = sha256(snapshot) == original_hash
        if not result["original_snapshot_unchanged"]:
            raise RuntimeError("Original snapshot changed")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", type=Path, required=True)
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--cases", type=Path, default=Path(__file__).resolve().parents[1] / "tests/fixtures/task_context_cases.json")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--reindex", action="store_true")
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    if args.output.exists() or not 1 <= args.repeats <= 10:
        parser.error("Use a new output path and 1–10 repeats")
    args.daemon = args.daemon.resolve(strict=True)
    document = json.loads(args.cases.read_text())
    report = {
        "schema_version": 1, "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "daemon_sha256": sha256(args.daemon), "harness_sha256": sha256(Path(__file__)),
        "helpers_sha256": sha256(Path(__file__).with_name("eval_context_quality.py")),
        "case_sha256": sha256(args.cases), "reindex": args.reindex, "repeats": args.repeats,
        "token_budget": document["default_token_budget"], "limitations": document["purpose"],
        "isolation": "Copied databases and historical checkouts; checkout deleted before queries; no live roots, background jobs, network models or source payload export.",
        "tasks": [],
    }
    for task in document["tasks"]:
        result = evaluate_task(args, task, document["default_token_budget"])
        report["tasks"].append(result)
        print(json.dumps({"task": task["task_id"], **result["summary"]}), flush=True)
    report["summary"] = summarize([case for task in report["tasks"] for case in task["cases"]])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x") as output:
        output.write(json.dumps(report, indent=2) + "\n")
    if any(report["summary"][key] for key in ("budget_failures", "off_failures", "negative_failures")):
        sys.exit(1)


if __name__ == "__main__":
    main()
