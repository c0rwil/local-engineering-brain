#!/usr/bin/env python3
"""Run the corrected v2 task comparison against one pinned Engram release.

Reuses the pilot's isolation and telemetry adapter. Original fixtures/results
are read-only inputs; all new source snapshots and attempts have a separate root.
"""
import argparse
import datetime
import hashlib
import json
import math
from pathlib import Path
import shutil
import sqlite3
import sys
import time

sys.dont_write_bytecode = True
import run_real_repo_pilot as runner  # noqa: E402
from real_repo_metrics import apply_tool_runtime_checks, classify_record  # noqa: E402
import preflight_real_repo_runtime as runtime_preflight  # noqa: E402

SUITE = runner.ROOT / "benchmarks/real-repo-v2"


def validate_seed_roots(data, active, seeds):
    for output in (data, active):
        if output == seeds or output.is_relative_to(seeds) or seeds.is_relative_to(output):
            raise ValueError("New evidence, active checkout and original fixtures must be separate")


def stop_if_unscorable(record):
    if classify_record(record)["grade"]["status"] not in ("pass", "fail"):
        raise RuntimeError("Unscorable attempt preserved. Inspect the evidence before resuming; do not replace or retry it.")


def refuse_stopped_collection(data):
    if (data / "collection_stop.json").exists():
        raise ValueError("This collection was stopped; preserve it and use a new collection root")


def frozen_inputs_match(args, expected):
    frozen = args.data / "protocol"
    suite = frozen / "suite"
    actual = {str(path.relative_to(suite)): runner.digest(path) for path in suite.rglob("*")
              if path.is_file() and "__pycache__" not in path.parts}
    scripts = {"run_real_repo_followup.py": expected["runner_sha256"],
               "run_real_repo_pilot.py": expected["runner_library_sha256"],
               "real_repo_metrics.py": expected["metrics_sha256"], **expected["helper_sha256"]}
    return (actual == expected["suite_files"]
            and all(runner.digest(frozen / "scripts" / name) == digest for name, digest in scripts.items()))


def validate_trial_controls(record, args, catalog, expected):
    try:
        unchanged = controls(args, catalog) == expected and frozen_inputs_match(args, expected)
    except Exception:
        unchanged = False
    if not unchanged:
        record.setdefault("behavioral_grade", record["grade"])
        issues = record.setdefault("validity_issues", [])
        if "collection_control_drift" not in issues:
            issues.append("collection_control_drift")
        record["grade"] = {"status": "infrastructure_error", "issues": list(issues)}
    if record.get("id"):
        trace = args.data / "trials" / record["id"] / "trace.jsonl"
        if trace.exists():
            record.update(apply_tool_runtime_checks(record, trace))


def validate_fixture(task, fixture):
    ready = json.loads((fixture / "ready.json").read_text())
    if ready["task_id"] != task["id"] or ready["base_commit"] != task["base_commit"]:
        raise ValueError("Frozen fixture does not match the task's historical base")
    if runner.digest(fixture / "snapshot.sqlite") != ready["snapshot_sha256"]:
        raise ValueError("Frozen source snapshot digest changed")
    work = fixture / "work"
    head = runner.command(["git", "-C", work, "rev-parse", "HEAD"]).strip()
    if head != ready["fixture_head"]:
        raise ValueError("Frozen source checkout HEAD changed")
    if runner.command(["git", "-C", work, "rev-list", "--count", "HEAD"]).strip() != "1":
        raise ValueError("Historical fixture must have only one root commit")
    if runner.command(["git", "-C", work, "status", "--porcelain", "--untracked-files=all"]).strip():
        raise ValueError("Historical fixture contains changes or untracked source")
    for name, expected in ready.get("lockfile_sha256", {}).items():
        if Path(name).name != name or runner.digest(work / name) != expected:
            raise ValueError("Frozen dependency lock changed")
    return ready


def prepare_fixture(task, args):
    source = args.seed_fixtures / task["id"]
    original = validate_fixture(task, source)
    destination = args.data / "fixtures" / task["id"]
    if (destination / "ready.json").exists():
        prepared = validate_fixture(task, destination)
        if (prepared.get("engram_sha256") != runner.digest(runner.ENGRAM)
                or prepared.get("daemon_sha256") != runner.digest(runner.DAEMON)):
            raise ValueError("Prepared fixture belongs to another Engram release")
        return prepared
    if destination.exists():
        raise ValueError(f"Incomplete fixture preparation; preserve and inspect {destination}")
    destination.mkdir(parents=True)
    started = time.monotonic()
    work = destination / "work"
    runner.clone(source / "work", work)
    if task["repository"] == "agi-backend-rust":
        runner.clone(source / "cargo-home", destination / "cargo-home")
    seed = destination / "seed"
    seed.mkdir()
    shutil.copy2(source / "snapshot.sqlite", seed / "brain.sqlite")
    config = seed / "config.toml"
    config.write_text(runner.config_text(seed / "brain.sqlite", seed / "d.sock"))
    local_config = work / ".engram/config.toml"
    local_config.write_text(config.read_text())
    index_start = time.monotonic()
    try:
        with runner.daemon(config, seed) as process:
            health = runner.wait_ready(process, seed / "d.sock")
            indexed = json.loads(runner.cli(seed, "index", work, "--json"))
            if indexed["resolved"]["project"]["id"] != original["project_id"]:
                raise ValueError("Reindex changed the frozen project identity")
    finally:
        local_config.unlink(missing_ok=True)
    snapshot = destination / "snapshot.sqlite"
    with sqlite3.connect(seed / "brain.sqlite") as con, sqlite3.connect(snapshot) as backup:
        con.backup(backup)
    con.close()
    backup.close()
    with sqlite3.connect(snapshot) as con:
        con.execute("DELETE FROM project_aliases")
        con.commit()
        con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        counts = {table: con.execute(f"SELECT count(*) FROM {table}").fetchone()[0]
                  for table in ("nodes", "memories", "tasks", "context_runs")}
        schema = con.execute("SELECT max(version) FROM schema_migrations").fetchone()[0]
        aliases = con.execute("SELECT count(*) FROM nodes WHERE search_aliases<>'' AND tombstoned_at_ms IS NULL").fetchone()[0]
    con.close()
    if any(counts[name] for name in ("memories", "tasks", "context_runs")):
        raise ValueError("New snapshot must contain source only")
    result = {**original, "snapshot_sha256": runner.digest(snapshot), "source_counts": counts,
              "index": indexed, "index_seconds": round(time.monotonic() - index_start, 3),
              "setup_seconds": round(time.monotonic() - started, 3), "schema_version": schema,
              "owners_with_aliases": aliases, "engram_sha256": runner.digest(runner.ENGRAM),
              "daemon_sha256": runner.digest(runner.DAEMON), "build_id": health["build_id"],
              "seed_snapshot_sha256": original["snapshot_sha256"],
              "lockfile_sha256": {path.name: runner.digest(path) for path in work.glob("*lock*") if path.is_file()},
              "dependency_seconds": 0, "dependency_preparation": "copy-on-write clone of the frozen pilot base cache"}
    runner.write_json(destination / "ready.json", result)
    validate_fixture(task, source)
    validate_fixture(task, destination)
    return result


def controls(args, catalog):
    files = sorted(path for path in SUITE.rglob("*") if path.is_file() and "__pycache__" not in path.parts)
    return {
        "suite": "real-repo-v2", "suite_files": {str(path.relative_to(SUITE)): runner.digest(path) for path in files},
        "engram_sha256": runner.digest(runner.ENGRAM), "daemon_sha256": runner.digest(runner.DAEMON),
        "claude_sha256": runner.digest(runner.CLAUDE),
        "claude_version": runner.command([runner.CLAUDE, "--version"]).strip(),
        "model": runner.MODEL, "effort": "high", "repetitions": catalog["pilot_repetitions"],
        "agent_limit_seconds": args.agent_seconds, "grading_limit_seconds": args.grader_seconds,
        "cost_cap_usd_estimate": args.cost_cap,
        "runner_sha256": runner.digest(Path(__file__)),
        "runner_library_sha256": runner.digest(Path(runner.__file__)),
        "metrics_sha256": runner.digest(runner.ROOT / "scripts/real_repo_metrics.py"),
        "helper_sha256": {name: runner.digest(runner.ROOT / "scripts" / name)
                          for name in ("eval_context_quality.py", "capture_eval.py", "preflight_real_repo_runtime.py")},
        "runtime_preflight_sha256": runner.digest(args.data / "runtime-preflight.json"),
        "fixture_hashes": {task["id"]: runner.digest(args.data / "fixtures" / task["id"] / "ready.json")
                           for task in catalog["tasks"]},
    }


def freeze_manifest(args, catalog):
    expected = controls(args, catalog)
    path = args.data / "manifest.json"
    if path.exists():
        manifest = json.loads(path.read_text())
        if manifest.get("controls") != expected:
            raise ValueError("Frozen collection controls changed; cannot resume or mix results")
        return manifest
    if (args.data / "trials").exists() or (args.data / "records.jsonl").exists():
        raise ValueError("Cannot establish controls after trials have started")
    manifest = {
        "schema_version": 1, **{key: value for key, value in expected.items() if key not in ("suite_files", "fixture_hashes")},
        "controls": expected, "condition": "source-only native hooks; no MCP; no learned memories",
        "controls_sha256": hashlib.sha256(json.dumps(expected, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
        "engram_binary": str(runner.ENGRAM),
        "frozen_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform_dependencies": "same frozen pilot base dependencies in both arms; not a historical lock-exact reinstall",
        "private_dependencies": "Rust registry-only cache; no user Git config; package installation disabled",
        "rustc": runner.command(["rustc", "--version"]).strip(), "python": sys.version,
        "node": runner.command(["node", "--version"]).strip(),
        "pnpm": runner.command(["pnpm", "--version"]).strip(),
        "known_scope_limits": [
            "Development tasks previously used to improve retrieval; not held-out generalization evidence.",
            "V2 clarifies grading contracts. Compare Off/Brain within this collection; no causal old-versus-new comparison.",
            "Provider prompt caching remains enabled and is not reset. Report all token categories; estimated USD is not subscription cash billing.",
            "Single agent, six core tools, source-only hooks. Does not measure learned memories, automatic judging, or MCP tools.",
            "Three repetitions per task are diagnostic, not statistically decisive.",
        ],
    }
    frozen = args.data / "protocol"
    if frozen.exists():
        raise ValueError("Incomplete protocol freeze; preserve and inspect the existing copy")
    for relative in expected["suite_files"]:
        target = frozen / "suite" / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(SUITE / relative, target)
    for source in (Path(__file__), Path(runner.__file__),
                   *(runner.ROOT / "scripts" / name for name in ("real_repo_metrics.py", *expected["helper_sha256"]))):
        target = frozen / "scripts" / source.name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
    runner.write_json(path, manifest)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data", type=Path, default=Path("/private/tmp/erp26-v3-data"))
    parser.add_argument("--active", type=Path, default=Path("/private/tmp/erp26work-v3-active"))
    parser.add_argument("--seed-fixtures", type=Path, default=Path("/private/tmp/erp26-data/fixtures"))
    parser.add_argument("--engram-release", type=Path, required=True)
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--limit", type=int)
    parser.add_argument("--agent-seconds", type=int, default=1200)
    parser.add_argument("--grader-seconds", type=int, default=1800)
    parser.add_argument("--cost-cap", type=float, default=8)
    args = parser.parse_args()
    runner.validate_paths(args.data, args.active, compact=True)
    args.data, args.active, args.seed_fixtures = args.data.resolve(), args.active.resolve(), args.seed_fixtures.resolve()
    refuse_stopped_collection(args.data)
    args.compact_settings = True
    validate_seed_roots(args.data, args.active, args.seed_fixtures)
    if any(not math.isfinite(value) or value <= 0 for value in (args.agent_seconds, args.grader_seconds, args.cost_cap)) or (args.limit is not None and args.limit <= 0):
        parser.error("Limits must be positive")
    runner.SUITE = SUITE
    runner.ENGRAM = (args.engram_release / "bin/engram").resolve(strict=True)
    runner.DAEMON = (args.engram_release / "bin/engramd").resolve(strict=True)
    args.data.mkdir(mode=0o700, exist_ok=True)
    catalog = json.loads((SUITE / "catalog.json").read_text())
    tasks = catalog["tasks"]
    if {task["id"] for task in tasks} != set(catalog["pilot_task_ids"]) or len(tasks) != 4 or catalog["pilot_repetitions"] != 3:
        raise ValueError("V2 collection requires the four declared tasks and three repetitions")
    for task in tasks:
        print("PREPARE " + task["id"], flush=True)
        prepare_fixture(task, args)
    if args.prepare_only:
        return
    if catalog.get("execution_status") != "ready_for_collection":
        raise ValueError("V2 graders must complete preflight before collection")
    runtime_preflight.validate_proof(args.data, args.active)
    manifest = freeze_manifest(args, catalog)
    # Graders/prompts execute from preserved inputs. Still detect source/control
    # drift after each attempt so final or --limit trials cannot escape validation.
    runner.SUITE = args.data / "protocol/suite"
    args.validate_record = lambda record: validate_trial_controls(record, args, catalog, manifest["controls"])
    done = {record["id"] for record in runner.report(args)}
    count = 0
    for repetition in range(1, catalog["pilot_repetitions"] + 1):
        for index, task in enumerate(tasks):
            arms = ("off", "brain") if (index + repetition) % 2 else ("brain", "off")
            for arm in arms:
                if f'{task["id"]}-r{repetition}-{arm}' in done:
                    continue
                if controls(args, catalog) != manifest["controls"] or not frozen_inputs_match(args, manifest["controls"]):
                    raise ValueError("Collection controls changed between trials")
                refuse_stopped_collection(args.data)
                runtime_preflight.validate_proof(args.data, args.active)
                validate_fixture(task, args.data / "fixtures" / task["id"])
                record = runner.run_trial(task, repetition, arm, args)
                runner.report(args)
                stop_if_unscorable(record)
                count += 1
                if args.limit and count >= args.limit:
                    return


if __name__ == "__main__":
    main()
