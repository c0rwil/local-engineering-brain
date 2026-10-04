#!/usr/bin/env python3
"""Run the corrected v2 task comparison against one pinned Engram release.

Reuses the pilot's isolation and telemetry adapter. Original fixtures/results
are read-only inputs; all new source snapshots and attempts have a separate root.
`--agent codex` runs both arms with the Codex CLI instead of Claude Code (see
real_repo_codex.py); a Codex collection needs its own data and active roots.
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
import real_repo_codex as codex_arm  # noqa: E402
import preflight_real_repo_codex as codex_preflight  # noqa: E402

SUITE = runner.ROOT / "benchmarks/real-repo-v2"
CLAUDE_HELPERS = ("eval_context_quality.py", "capture_eval.py", "preflight_real_repo_runtime.py")
CODEX_HELPERS = ("eval_context_quality.py", "capture_eval.py", "real_repo_codex.py", "preflight_real_repo_codex.py")


def agent_name(args):
    return getattr(args, "agent", "claude")


def validate_runtime(args):
    """Require the agent's passing loopback preflight for these exact controls."""
    if agent_name(args) == "codex":
        codex_preflight.validate_proof(args.data, args.active, args.agent_adapter)
    else:
        runtime_preflight.validate_proof(args.data, args.active)


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
    # The shell-spawn audit reads Claude stream-JSON; Codex traces have another format.
    if record.get("id") and agent_name(args) == "claude":
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


def agent_controls(args):
    """Agent identity, helper scripts, runtime proof and cost cap for the frozen controls."""
    if agent_name(args) == "codex":
        adapter = args.agent_adapter
        identity = {"agent": "codex", "codex_sha256": runner.digest(adapter.executable),
                    "codex_version": codex_arm.version(adapter.executable),
                    "model": adapter.model, "effort": adapter.effort, "codex_hook_trust": adapter.hook_trust,
                    "codex_config_sha256": codex_arm.config_template_sha256(adapter.model, adapter.effort)}
        # Codex exec has no per-run USD cap; only the wall limit applies.
        return identity, CODEX_HELPERS, codex_preflight.PROOF, None
    identity = {"claude_sha256": runner.digest(runner.CLAUDE),
                "claude_version": runner.command([runner.CLAUDE, "--version"]).strip(),
                "model": runner.MODEL, "effort": "high"}
    return identity, CLAUDE_HELPERS, "runtime-preflight.json", args.cost_cap


def controls(args, catalog):
    files = sorted(path for path in SUITE.rglob("*") if path.is_file() and "__pycache__" not in path.parts)
    identity, helpers, proof, cost_cap = agent_controls(args)
    return {
        "suite": "real-repo-v2", "suite_files": {str(path.relative_to(SUITE)): runner.digest(path) for path in files},
        "engram_sha256": runner.digest(runner.ENGRAM), "daemon_sha256": runner.digest(runner.DAEMON),
        **identity, "repetitions": catalog["pilot_repetitions"],
        "agent_limit_seconds": args.agent_seconds, "grading_limit_seconds": args.grader_seconds,
        "cost_cap_usd_estimate": cost_cap,
        "runner_sha256": runner.digest(Path(__file__)),
        "runner_library_sha256": runner.digest(Path(runner.__file__)),
        "metrics_sha256": runner.digest(runner.ROOT / "scripts/real_repo_metrics.py"),
        "helper_sha256": {name: runner.digest(runner.ROOT / "scripts" / name) for name in helpers},
        "runtime_preflight_sha256": runner.digest(args.data / proof),
        "fixture_hashes": {task["id"]: runner.digest(args.data / "fixtures" / task["id"] / "ready.json")
                           for task in catalog["tasks"]},
    }


def scope_limits(args):
    common = [
        "Development tasks previously used to improve retrieval; not held-out generalization evidence.",
        "V2 clarifies grading contracts. Compare Off/Brain within this collection; no causal old-versus-new comparison.",
    ]
    if agent_name(args) == "codex":
        return [*common,
                "Provider prompt caching remains enabled and is not reset. Report all token categories; Codex reports no USD estimate.",
                "Codex exec, single agent (multi-agent disabled), default Codex tools, source-only hooks plus the engram MCP server. "
                "Does not measure learned memories or automatic judging.",
                "No per-run budget cap exists for Codex; the wall limit applies to both arms.",
                ("Brain hooks are trusted in each isolated CODEX_HOME (hooks.state trusted_hash) after the runner verifies every installed handler."
                 if getattr(getattr(args, "agent_adapter", None), "hook_trust", "persisted") == "persisted" else
                 "Brain hooks run with --dangerously-bypass-hook-trust after the runner verifies every installed handler."),
                "Codex and Claude collections are separate experiments; do not compare across agents.",
                "Three repetitions per task are diagnostic, not statistically decisive."]
    return [*common,
            "Provider prompt caching remains enabled and is not reset. Report all token categories; estimated USD is not subscription cash billing.",
            "Single agent, six core tools, source-only hooks. Does not measure learned memories, automatic judging, or MCP tools.",
            "Three repetitions per task are diagnostic, not statistically decisive."]


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
    codex = agent_name(args) == "codex"
    manifest = {
        "schema_version": 1, **{key: value for key, value in expected.items() if key not in ("suite_files", "fixture_hashes")},
        "controls": expected,
        "condition": ("source-only native Codex hooks and the engram MCP server; no learned memories" if codex
                      else "source-only native hooks; no MCP; no learned memories"),
        "controls_sha256": hashlib.sha256(json.dumps(expected, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
        "engram_binary": str(runner.ENGRAM),
        "frozen_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform_dependencies": "same frozen pilot base dependencies in both arms; not a historical lock-exact reinstall",
        "private_dependencies": "Rust registry-only cache; no user Git config; package installation disabled",
        "rustc": runner.command(["rustc", "--version"]).strip(), "python": sys.version,
        "node": runner.command(["node", "--version"]).strip(),
        "pnpm": runner.command(["pnpm", "--version"]).strip(),
        "known_scope_limits": scope_limits(args),
    }
    if codex:
        manifest.update({"codex_binary": str(args.agent_adapter.executable),
                         "codex_auth": f"{codex_arm.AUTH_ENV} environment variable; value not recorded"})
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
    parser.add_argument("--cost-cap", type=float, default=8, help="Claude USD budget cap per run (Codex has none)")
    parser.add_argument("--agent", choices=("claude", "codex"), default="claude",
                        help="coding agent for both arms; use separate --data/--active roots per agent")
    parser.add_argument("--codex", type=Path,
                        help="Codex CLI; default: PATH, ~/.cache/codex-runtimes, app bundles, then Spotlight")
    parser.add_argument("--codex-model", help="pinned Codex model for both arms (required with --agent codex)")
    parser.add_argument("--codex-effort", default="high", help="Codex model_reasoning_effort for both arms")
    parser.add_argument("--codex-hook-trust", choices=("persisted", "bypass"), default="persisted",
                        help="persisted: write hooks.state trust for the verified hooks into each isolated CODEX_HOME; "
                             "bypass: pass the documented --dangerously-bypass-hook-trust flag instead")
    args = parser.parse_args()
    if args.agent == "codex":
        if not args.codex_model:
            parser.error("--codex-model is required with --agent codex")
        try:
            args.agent_adapter = codex_arm.CodexAgent(codex_arm.find_codex(args.codex), args.codex_model,
                                                      args.codex_effort, args.codex_hook_trust)
        except (OSError, ValueError) as error:
            parser.error(str(error))
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
        if agent_name(args) == "codex":
            codex_arm.validate_checkout(args.data / "fixtures" / task["id"] / "work")
    if args.prepare_only:
        return
    if catalog.get("execution_status") != "ready_for_collection":
        raise ValueError("V2 graders must complete preflight before collection")
    if agent_name(args) == "codex":
        args.agent_adapter.require_credentials()
    validate_runtime(args)
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
                validate_runtime(args)
                validate_fixture(task, args.data / "fixtures" / task["id"])
                record = runner.run_trial(task, repetition, arm, args)
                runner.report(args)
                stop_if_unscorable(record)
                count += 1
                if args.limit and count >= args.limit:
                    return


if __name__ == "__main__":
    main()
