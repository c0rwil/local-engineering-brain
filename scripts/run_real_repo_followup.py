#!/usr/bin/env python3
"""Run a frozen Off/Brain collection of one real-repository suite against one pinned Engram release.

The suite is selected by its catalog path (--catalog; default real-repo-v2).
Reuses the pilot's isolation and telemetry adapter. Original fixtures/results
are read-only inputs; all new source snapshots and attempts have a separate root.
real-repo-v2 clones the original pilot's seed fixtures. Other suites build theirs
from the operator's repositories (read-only git archive) with --prepare-only and
must pass preflight_real_repo_graders.py against those fixtures before collection.
A draft suite is collected only with --allow-draft-suite, which the frozen
manifest records. `--agent codex` runs both arms with the Codex CLI instead of
Claude Code (see real_repo_codex.py); a Codex collection needs its own data and
active roots.
"""
import argparse
import contextlib
import datetime
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tarfile
import time
import uuid

sys.dont_write_bytecode = True
import run_real_repo_pilot as runner  # noqa: E402
from real_repo_metrics import apply_tool_runtime_checks, classify_record  # noqa: E402
import preflight_real_repo_runtime as runtime_preflight  # noqa: E402
import real_repo_codex as codex_arm  # noqa: E402
import preflight_real_repo_codex as codex_preflight  # noqa: E402
import preflight_real_repo_graders as grader_preflight  # noqa: E402

SUITE = runner.ROOT / "benchmarks/real-repo-v2"
V2_SUITE_ID = "real-repo-v2"
READY_STATUS = "ready_for_collection"
DRAFT_STATUS = "draft_requires_operator_review"
DEFAULT_REPETITIONS = 3
DEPENDENCY_BUILD_SECONDS = 3600
CLAUDE_HELPERS = ("eval_context_quality.py", "capture_eval.py", "preflight_real_repo_runtime.py")
CODEX_HELPERS = ("eval_context_quality.py", "capture_eval.py", "real_repo_codex.py", "preflight_real_repo_codex.py")
# A suite whose fixtures are built from local repositories also freezes its grader preflight driver.
LOCAL_FIXTURE_HELPERS = ("preflight_real_repo_graders.py",)
SUITE_DEFAULTS = {
    V2_SUITE_ID: {"data": "/private/tmp/erp26-v3-data", "active": "/private/tmp/erp26work-v3-active",
                  "seed_fixtures": "/private/tmp/erp26-data/fixtures"},
    "real-repo-heldout-v1": {"data": "/private/tmp/erp26-heldout-data",
                             "active": "/private/tmp/erp26work-heldout-active"},
}
SCOPE_COMMON = {
    V2_SUITE_ID: [
        "Development tasks previously used to improve retrieval; not held-out generalization evidence.",
        "V2 clarifies grading contracts. Compare Off/Brain within this collection; no causal old-versus-new comparison.",
    ],
    "real-repo-heldout-v1": [
        "Held-out historical tasks for the line-ranged pointer and related-use changes. They count as held out "
        "only after the operator confirms that none informed Engram development.",
        "Fixtures are rebuilt from the operator's repositories at the catalog base commits, and the graders are "
        "recalibrated against them. Compare Off/Brain within this collection only.",
    ],
}
GENERIC_SCOPE = ["Historical replay tasks. Compare Off/Brain within this collection only."]
SEED_DEPENDENCY_NOTES = {
    V2_SUITE_ID: "same frozen pilot base dependencies in both arms; not a historical lock-exact reinstall",
}
LOCAL_DEPENDENCY_NOTE = ("Rust dependencies built offline and locked from each base Cargo.lock before timing, "
                         "identical in both arms; agents-repository tasks have no installed Python dependencies")


class Suite:
    """One benchmark suite, selected by its catalog path; its directory is what gets frozen."""

    def __init__(self, catalog_path):
        self.catalog_path = Path(catalog_path).resolve(strict=True)
        self.root = self.catalog_path.parent
        self.catalog = json.loads(self.catalog_path.read_text())
        self.id = self.catalog["suite_id"]
        self.defaults = SUITE_DEFAULTS.get(self.id, {})

    @property
    def draft(self):
        return self.catalog.get("execution_status") == DRAFT_STATUS


_V2 = None


def suite_of(args):
    """The collection's suite; real-repo-v2 when none was selected."""
    global _V2
    suite = getattr(args, "suite", None)
    if suite is not None:
        return suite
    if _V2 is None:
        _V2 = Suite(SUITE / "catalog.json")
    return _V2


def local_fixtures(args):
    """True when fixtures are built from local repositories instead of cloned from earlier seeds."""
    return getattr(args, "seed_fixtures", None) is None and "seed_fixtures" not in suite_of(args).defaults


def repetitions(catalog):
    return catalog.get("pilot_repetitions", DEFAULT_REPETITIONS)


def schedule(tasks, count):
    """Planned attempts in collection order; arm order alternates within each planned pair."""
    planned = []
    for repetition in range(1, count + 1):
        for index, task in enumerate(tasks):
            arms = ("off", "brain") if (index + repetition) % 2 else ("brain", "off")
            planned.extend((repetition, task, arm) for arm in arms)
    return planned


def agent_name(args):
    return getattr(args, "agent", "claude")


def validate_runtime(args):
    """Require the agent's passing loopback preflight for these exact controls."""
    if agent_name(args) == "codex":
        codex_preflight.validate_proof(args.data, args.active, args.agent_adapter)
    else:
        runtime_preflight.validate_proof(args.data, args.active)
    if local_fixtures(args):
        # Fixtures built here must have recalibrated graders before any attempt.
        suite = suite_of(args)
        grader_preflight.validate_proof(args.data, suite.root, suite.id, suite.catalog["tasks"])


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


def source_only_snapshot(database, snapshot):
    """Back up an indexed database without project aliases; refuse anything but source."""
    with sqlite3.connect(database) as con, sqlite3.connect(snapshot) as backup:
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
    return counts, schema, aliases


def current_release(prepared):
    return (prepared.get("engram_sha256") == runner.digest(runner.ENGRAM)
            and prepared.get("daemon_sha256") == runner.digest(runner.DAEMON))


def ensure_release(prepared):
    if not current_release(prepared):
        raise ValueError("Prepared fixture belongs to another Engram release; for fixtures built from local "
                         "repositories, refresh the snapshot with --prepare-only --refresh-snapshots")
    return prepared


def refresh_snapshot(task, args):
    """Re-index a prepared fixture with the pinned release, keeping its work tree and dependency build.

    The earlier snapshot, seed state and ready.json move to superseded/ as
    evidence. Returns the earlier ready.json digest, or None when the fixture
    already belongs to this release.
    """
    destination = args.data / "fixtures" / task["id"]
    ready = validate_fixture(task, destination)
    if current_release(ready):
        return None
    previous = runner.digest(destination / "ready.json")
    archive = destination / "superseded" / (time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()) + "-"
                                            + str(ready.get("build_id") or "unknown")[:12])
    archive.mkdir(parents=True)
    for name in ("snapshot.sqlite", "seed", "ready.json"):
        if (destination / name).exists():
            (destination / name).rename(archive / name)
    started = time.monotonic()
    snapshot = index_source_snapshot(destination, destination / "work", ready["project_id"])
    refresh = {"refreshed_at": now(), "previous_ready_sha256": previous, "previous_snapshot_sha256": ready["snapshot_sha256"],
               "previous_build_id": ready.get("build_id"), "previous_engram_sha256": ready.get("engram_sha256"),
               "superseded": str(archive.relative_to(destination))}
    refreshed = {**ready, "snapshot_sha256": runner.digest(destination / "snapshot.sqlite"), **snapshot,
                 "index_seconds": round(time.monotonic() - started, 3),
                 "engram_sha256": runner.digest(runner.ENGRAM), "daemon_sha256": runner.digest(runner.DAEMON),
                 "snapshot_refreshes": [*ready.get("snapshot_refreshes", []), refresh]}
    runner.write_json(destination / "ready.json", refreshed)
    validate_fixture(task, destination)
    print(f"REFRESHED {task['id']} index={refreshed['index_seconds']}s build={str(snapshot['build_id'])[:12]}", flush=True)
    return previous


def grader_proof_state(args):
    """Whether the grader proof is valid for the fixtures as they are now."""
    suite = suite_of(args)
    if not (args.data / grader_preflight.PROOF).exists():
        return "absent"
    try:
        grader_preflight.validate_proof(args.data, suite.root, suite.id, suite.catalog["tasks"])
    except (OSError, ValueError, KeyError) as error:
        return f"invalid: {error}"
    return "valid"


def rebind_grader_proof(args, refreshed):
    """Point a valid grader proof at refreshed fixtures whose work trees did not change.

    Grading runs on the fixture work tree, which a snapshot refresh leaves
    untouched (validated HEAD, clean tree and lockfiles). Only the entries of
    refreshed fixtures change, each only if the proof was bound to the exact
    ready.json that was refreshed. The earlier proof is kept beside it.
    """
    path = args.data / grader_preflight.PROOF
    proof = json.loads(path.read_text())
    for task_id, previous in refreshed.items():
        if proof["fixtures"].get(task_id) != previous:
            raise ValueError(f"The grader proof was not bound to {task_id}'s refreshed fixture; rerun the grader preflight")
    shutil.copy2(path, args.data / f"grader-preflight.superseded-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}.json")
    current = {task_id: runner.digest(args.data / "fixtures" / task_id / "ready.json") for task_id in refreshed}
    proof["fixtures"].update(current)
    proof.setdefault("rebinds", []).append({
        "at": now(), "reason": "snapshot refresh for another Engram release; work trees, lockfiles and dependency builds unchanged",
        "engram_sha256": runner.digest(runner.ENGRAM), "daemon_sha256": runner.digest(runner.DAEMON),
        "tasks": {task_id: {"from_ready_sha256": previous, "to_ready_sha256": current[task_id]}
                  for task_id, previous in sorted(refreshed.items())}})
    runner.write_json(path, proof)
    suite = suite_of(args)
    grader_preflight.validate_proof(args.data, suite.root, suite.id, suite.catalog["tasks"])


def prepare_fixture(task, args):
    if local_fixtures(args):
        return prepare_local_fixture(task, args)
    source = args.seed_fixtures / task["id"]
    original = validate_fixture(task, source)
    destination = args.data / "fixtures" / task["id"]
    if (destination / "ready.json").exists():
        return ensure_release(validate_fixture(task, destination))
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
    counts, schema, aliases = source_only_snapshot(seed / "brain.sqlite", snapshot)
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


def source_git_env():
    return grader_preflight.source_git_env()


def export_base(repository, commit, work, log):
    """Extract a historical tree with `git archive`; the operator repository is only read."""
    runner.command(["git", "-C", repository, "cat-file", "-e", f"{commit}^{{commit}}"], env=source_git_env())
    work.mkdir()
    with log.open("w") as errors:
        archive = subprocess.Popen(["git", "-C", str(repository), "archive", "--format=tar", commit],
                                   env=source_git_env(), stdout=subprocess.PIPE, stderr=errors)
        try:
            with tarfile.open(fileobj=archive.stdout, mode="r|") as tree:
                tree.extractall(work, filter="data")
        finally:
            archive.stdout.close()
            archive.wait()
    if archive.returncode:
        raise RuntimeError(f"git archive failed ({archive.returncode}) for {commit}; log={log}")


def sanitize_tree(work):
    """Never keep a tracked environment secret or a link reaching outside the fixture."""
    removed = []
    root = work.resolve()
    for path in sorted(work.rglob("*")):
        if path.is_symlink() and not path.resolve().is_relative_to(root):
            removed.append(str(path.relative_to(work)))
            path.unlink()
        elif path.is_file() and not path.is_symlink() and path.name == ".env":
            removed.append(str(path.relative_to(work)))
            path.unlink()
    return removed


def refresh_mtimes(work):
    """Archives carry commit-time mtimes, so a cargo target built from a newer tree
    could treat these sources as unchanged. Give every file a fresh mtime before
    the fixture is committed and built."""
    now = time.time()
    touched = 0
    for directory, _, files in os.walk(work):
        for name in files:
            path = Path(directory) / name
            if not path.is_symlink():
                os.utime(path, (now, now))
                touched += 1
    return touched


def commit_fixture(work, task, suite_id):
    """One root commit with the pinned project identity; no remote, hooks or user identity."""
    (work / ".engram").mkdir(exist_ok=True)
    project_id = "prj_" + str(uuid.uuid5(uuid.NAMESPACE_URL, f"engram-{suite_id}/{task['id']}"))
    (work / ".engram/project.toml").write_text(f'project_id = "{project_id}"\nname = "{task["repository"]}"\n')
    runner.command(["git", "init", "-q", work])
    runner.command(["git", "-C", work, "config", "core.hooksPath", "/dev/null"])
    # Force: tracked files matching the tree's own ignore rules still belong to the base.
    runner.command(["git", "-C", work, "add", "-A", "--force"])
    runner.command(["git", "-C", work, "-c", "user.name=Benchmark", "-c", "user.email=benchmark@invalid",
                    "commit", "-qm", "Historical source fixture"])
    return project_id


def tree_entries(listing):
    entries = {}
    for record in listing.split(b"\0"):
        if record:
            meta, _, name = record.partition(b"\t")
            entries[name.decode()] = meta.decode()
    return entries


def verify_fixture_tree(repository, commit, work, removed):
    """The fixture commit must hold the base tree exactly, plus only its project identity."""
    expected = tree_entries(grader_preflight.source_git(repository, "ls-tree", "-r", "-z", commit))
    for path in removed:
        expected.pop(path, None)
    actual = tree_entries(subprocess.run(["git", "-C", str(work), "ls-tree", "-r", "-z", "HEAD"], env=runner.clean_env(),
                                         capture_output=True, check=True).stdout)
    actual.pop(".engram/project.toml", None)
    if actual != expected:
        differing = sorted(set(actual.items()) ^ set(expected.items()))
        raise ValueError(f"Fixture tree differs from {commit[:12]} at {len(differing)} entries, e.g. {differing[:3]}")
    return len(actual)


def index_source_snapshot(destination, work, project_id):
    """Index the fixture with the pinned release in an isolated daemon; keep a source-only snapshot."""
    seed = destination / "seed"
    seed.mkdir()
    config = seed / "config.toml"
    config.write_text(runner.config_text(seed / "brain.sqlite", seed / "d.sock"))
    local_config = work / ".engram/config.toml"
    local_config.write_text(config.read_text())
    try:
        with runner.daemon(config, seed) as process:
            health = runner.wait_ready(process, seed / "d.sock")
            indexed = json.loads(runner.cli(seed, "index", work, "--json"))
    finally:
        local_config.unlink(missing_ok=True)
    if indexed["resolved"]["project"]["id"] != project_id:
        raise ValueError("Index resolved a project other than the fixture's pinned identity")
    counts, schema, aliases = source_only_snapshot(seed / "brain.sqlite", destination / "snapshot.sqlite")
    return {"index": indexed, "build_id": health["build_id"], "source_counts": counts, "schema_version": schema,
            "owners_with_aliases": aliases}


def shared_build(task, args):
    """An already prepared fixture of the same repository and base, whose build cache can seed this one."""
    for other in suite_of(args).catalog["tasks"]:
        fixture = args.data / "fixtures" / other["id"]
        if (other["id"] != task["id"] and other["repository"] == task["repository"]
                and other["base_commit"] == task["base_commit"] and (fixture / "ready.json").is_file()
                and (fixture / "work/target").is_dir()):
            return fixture
    return None


def prepare_rust_dependencies(task, args, destination):
    """Private registry-only cargo home, and a base build at the trials' own absolute paths."""
    reuse = shared_build(task, args)
    home = destination / "cargo-home"
    home.mkdir()
    runner.clone(Path.home() / ".cargo/registry", home / "registry")
    active = args.active
    if active.exists():
        runner.validate_paths(args.data, active, compact=True)
        shutil.rmtree(active)
    active.mkdir(mode=0o700)
    (active / ".pilot-owned").write_text("fixture-preparation\n")
    try:
        runner.clone(destination / "work", active / "work")
        (active / "scratch").mkdir()
        runner.clone(home, active / "scratch/cargo-home")
        if reuse is not None:
            runner.clone(reuse / "work/target", active / "work/target")
        env = {**runner.coding_env(active / "work", compact=True), "CARGO_BUILD_JOBS": str(args.build_jobs)}
        started = time.monotonic()
        runner.command(["cargo", "test", "--offline", "--locked", "--no-run"], cwd=active / "work", env=env,
                       timeout=DEPENDENCY_BUILD_SECONDS, log=destination / "dependencies.log")
        seconds = round(time.monotonic() - started, 3)
        runner.clone(active / "work/target", destination / "work/target")
    finally:
        shutil.rmtree(active, ignore_errors=True)
    return {"dependency_seconds": seconds, "build_jobs": args.build_jobs, "build_active_root": str(active),
            "dependency_reuse": reuse.name if reuse is not None else None,
            "dependency_preparation": ("offline locked base build in the trial's active paths"
                                       + (f", seeded from {reuse.name}'s build" if reuse is not None else "")),
            "cargo_home_scope": "private registry-only copy; no global config or git checkouts"}


def prepare_local_fixture(task, args):
    """Build a fixture from the operator's repository without modifying it."""
    destination = args.data / "fixtures" / task["id"]
    if (destination / "ready.json").exists():
        return ensure_release(validate_fixture(task, destination))
    if destination.exists():
        raise ValueError(f"Incomplete fixture preparation; preserve and inspect {destination}")
    repository = (args.source_root / task["repository"]).resolve(strict=True)
    destination.mkdir(parents=True)
    started = time.monotonic()
    work = destination / "work"
    export_base(repository, task["base_commit"], work, destination / "archive.log")
    removed = sanitize_tree(work)
    refreshed = refresh_mtimes(work)
    project_id = commit_fixture(work, task, suite_of(args).id)
    tracked = verify_fixture_tree(repository, task["base_commit"], work, removed)
    export_seconds = round(time.monotonic() - started, 3)
    index_start = time.monotonic()
    snapshot = index_source_snapshot(destination, work, project_id)
    index_seconds = round(time.monotonic() - index_start, 3)
    dependencies = {"dependency_seconds": 0, "dependency_preparation": "none; no dependencies are installed"}
    if task["repository"] == "agi-backend-rust":
        dependencies = prepare_rust_dependencies(task, args, destination)
    result = {"task_id": task["id"], "base_commit": task["base_commit"], "repository": task["repository"],
              "project_id": project_id, "fixture_head": runner.command(["git", "-C", work, "rev-parse", "HEAD"]).strip(),
              "fixture_source": "operator repository via read-only git archive", "source_repository": str(repository),
              "snapshot_sha256": runner.digest(destination / "snapshot.sqlite"), **snapshot,
              "tracked_files": tracked, "removed_paths": removed, "refreshed_files": refreshed,
              "export_seconds": export_seconds, "index_seconds": index_seconds, **dependencies,
              "setup_seconds": round(time.monotonic() - started, 3),
              "engram_sha256": runner.digest(runner.ENGRAM), "daemon_sha256": runner.digest(runner.DAEMON),
              "rustc": runner.command(["rustc", "--version"]).strip(),
              "lockfile_sha256": {path.name: runner.digest(path) for path in work.glob("*lock*") if path.is_file()}}
    runner.write_json(destination / "ready.json", result)
    validate_fixture(task, destination)
    print(f"PREPARED {task['id']} setup={result['setup_seconds']}s export={export_seconds}s "
          f"index={index_seconds}s dependencies={result['dependency_seconds']}s", flush=True)
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
    suite = suite_of(args)
    files = sorted(path for path in suite.root.rglob("*") if path.is_file() and "__pycache__" not in path.parts)
    identity, helpers, proof, cost_cap = agent_controls(args)
    local = local_fixtures(args)
    value = {
        "suite": suite.id, "suite_files": {str(path.relative_to(suite.root)): runner.digest(path) for path in files},
        "engram_sha256": runner.digest(runner.ENGRAM), "daemon_sha256": runner.digest(runner.DAEMON),
        **identity, "repetitions": repetitions(catalog),
        "agent_limit_seconds": args.agent_seconds, "grading_limit_seconds": args.grader_seconds,
        "cost_cap_usd_estimate": cost_cap,
        "runner_sha256": runner.digest(Path(__file__)),
        "runner_library_sha256": runner.digest(Path(runner.__file__)),
        "metrics_sha256": runner.digest(runner.ROOT / "scripts/real_repo_metrics.py"),
        "helper_sha256": {name: runner.digest(runner.ROOT / "scripts" / name)
                          for name in (*helpers, *(LOCAL_FIXTURE_HELPERS if local else ()))},
        "runtime_preflight_sha256": runner.digest(args.data / proof),
        "fixture_hashes": {task["id"]: runner.digest(args.data / "fixtures" / task["id"] / "ready.json")
                           for task in catalog["tasks"]},
    }
    if local:
        value.update({"fixture_source": "operator repositories at the catalog base commits (read-only git archive)",
                      "grader_preflight_sha256": runner.digest(args.data / grader_preflight.PROOF),
                      "grader_environment": grader_preflight.grader_environment(args.data)})
    if getattr(args, "allow_draft_suite", False):
        value.update({"suite_status": catalog.get("execution_status"), "allow_draft_suite": True})
    return value


def scope_limits(args):
    common = list(SCOPE_COMMON.get(suite_of(args).id, GENERIC_SCOPE))
    if getattr(args, "allow_draft_suite", False):
        common.append(f"Collected from a suite still marked {DRAFT_STATUS} (--allow-draft-suite), before the "
                      "operator review its README requires.")
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


def dependency_note(args):
    if local_fixtures(args):
        return LOCAL_DEPENDENCY_NOTE
    return SEED_DEPENDENCY_NOTES.get(suite_of(args).id, "same frozen seed-fixture dependencies in both arms")


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
        "platform_dependencies": dependency_note(args),
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
    suite_root = suite_of(args).root
    for relative in expected["suite_files"]:
        target = frozen / "suite" / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(suite_root / relative, target)
    for source in (Path(__file__), Path(runner.__file__),
                   *(runner.ROOT / "scripts" / name for name in ("real_repo_metrics.py", *expected["helper_sha256"]))):
        target = frozen / "scripts" / source.name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
    runner.write_json(path, manifest)
    return manifest


def validate_suite_tasks(catalog):
    ids = [task["id"] for task in catalog["tasks"]]
    if not ids or len(ids) != len(set(ids)):
        raise ValueError("A suite needs unique task ids")
    if "pilot_task_ids" in catalog and set(ids) != set(catalog["pilot_task_ids"]):
        raise ValueError("Suite tasks differ from the catalog's declared task ids")
    for task in catalog["tasks"]:
        missing = {"repository", "base_commit", "prompt_file"} - set(task)
        if missing:
            raise ValueError(f"Task {task['id']} lacks {sorted(missing)}")


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")


def progress(args, **fields):
    """Record detached-collection progress atomically; a no-op without --progress."""
    path = getattr(args, "progress", None)
    if path is None:
        return
    current = {}
    with contextlib.suppress(OSError, ValueError):
        current = json.loads(path.read_text())
    current.update(fields, pid=os.getpid(), updated_at=now())
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(current, indent=2, default=str) + "\n")
    temporary.replace(path)


@contextlib.contextmanager
def collector_lock(args):
    """One collector per data root while a detached collection runs (taken with --progress)."""
    if getattr(args, "progress", None) is None:
        yield
        return
    handle = (args.data / "collector.lock").open("a")
    try:
        try:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError("Another collector holds this data root's lock") from None
        yield
    finally:
        handle.close()


def completed_records(args):
    path = args.data / "records.jsonl"
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def check_collection(args, catalog, planned):
    """Everything a launch validates, without freezing, writing or running anything."""
    expected = controls(args, catalog)
    path = args.data / "manifest.json"
    if path.exists():
        if json.loads(path.read_text()).get("controls") != expected:
            raise ValueError("Frozen collection controls changed; cannot resume or mix results")
        if not frozen_inputs_match(args, expected):
            raise ValueError("Frozen protocol copy differs from its manifest")
    elif (args.data / "trials").exists() or (args.data / "records.jsonl").exists():
        raise ValueError("Cannot establish controls after trials have started")
    done = {record["id"] for record in completed_records(args)}
    ids = [f'{task["id"]}-r{repetition}-{arm}' for repetition, task, arm in planned]
    interrupted = sorted(path.name for path in (args.data / "trials").glob("*")
                         if path.name not in done) if (args.data / "trials").exists() else []
    if interrupted:
        raise ValueError(f"Interrupted attempts need inspection before resuming: {interrupted}")
    print(json.dumps({"status": "ready", "suite": suite_of(args).id, "planned": len(ids),
                      "completed": len(done & set(ids)), "manifest": "frozen" if path.exists() else "frozen at launch",
                      "allow_draft_suite": bool(getattr(args, "allow_draft_suite", False))}), flush=True)


def collect(args, suite, catalog, tasks):
    planned = schedule(tasks, repetitions(catalog))
    progress(args, state="preparing", suite=suite.id, catalog=str(suite.catalog_path), agent=agent_name(args),
             data=str(args.data), active=str(args.active), planned=len(planned), started_at=now())
    refresh = getattr(args, "refresh_snapshots", False)
    if refresh:
        if any((args.data / name).exists() for name in ("manifest.json", "trials", "records.jsonl")):
            raise ValueError("Snapshots cannot be refreshed after the collection has started")
        proof_before = grader_proof_state(args)
    refreshed = {}
    for task in tasks:
        print("PREPARE " + task["id"], flush=True)
        if getattr(args, "check_only", False) and not (args.data / "fixtures" / task["id"] / "ready.json").exists():
            raise ValueError(f"Fixture {task['id']} is not prepared; run --prepare-only first")
        if refresh and (args.data / "fixtures" / task["id"] / "ready.json").exists():
            previous = refresh_snapshot(task, args)
            if previous is not None:
                refreshed[task["id"]] = previous
        prepare_fixture(task, args)
        if agent_name(args) == "codex":
            codex_arm.validate_checkout(args.data / "fixtures" / task["id"] / "work")
    if refresh:
        if not refreshed:
            print(f"GRADER-PROOF unchanged ({proof_before}); no snapshot needed a refresh", flush=True)
        elif proof_before == "valid":
            rebind_grader_proof(args, refreshed)
            print(f"GRADER-PROOF rebound for {len(refreshed)} refreshed fixture(s): {', '.join(sorted(refreshed))}", flush=True)
        else:
            print(f"GRADER-PROOF not rebound ({proof_before}); run preflight_real_repo_graders.py before collection", flush=True)
    if args.prepare_only:
        progress(args, state="prepared")
        return
    if catalog.get("execution_status") != READY_STATUS and not (suite.draft and args.allow_draft_suite):
        if suite.id == V2_SUITE_ID:
            raise ValueError("V2 graders must complete preflight before collection")
        raise ValueError(f"{suite.id} is {catalog.get('execution_status')}, not {READY_STATUS}")
    if agent_name(args) == "codex":
        args.agent_adapter.require_credentials()
    validate_runtime(args)
    if getattr(args, "check_only", False):
        check_collection(args, catalog, planned)
        return
    manifest = freeze_manifest(args, catalog)
    # Graders/prompts execute from preserved inputs. Still detect source/control
    # drift after each attempt so final or --limit trials cannot escape validation.
    runner.SUITE = args.data / "protocol/suite"
    args.validate_record = lambda record: validate_trial_controls(record, args, catalog, manifest["controls"])
    done = {record["id"] for record in runner.report(args)}
    progress(args, state="collecting", controls_sha256=manifest["controls_sha256"], completed=len(done))
    count = 0
    for repetition, task, arm in planned:
        if f'{task["id"]}-r{repetition}-{arm}' in done:
            continue
        if controls(args, catalog) != manifest["controls"] or not frozen_inputs_match(args, manifest["controls"]):
            raise ValueError("Collection controls changed between trials")
        refuse_stopped_collection(args.data)
        validate_runtime(args)
        validate_fixture(task, args.data / "fixtures" / task["id"])
        progress(args, current=f'{task["id"]}-r{repetition}-{arm}', current_started_at=now())
        record = runner.run_trial(task, repetition, arm, args)
        records = runner.report(args)
        progress(args, current=None, completed=len(records),
                 last={"id": record["id"], "grade": record["grade"]["status"],
                       "agent_wall_seconds": record.get("agent_wall_seconds")})
        stop_if_unscorable(record)
        count += 1
        if args.limit and count >= args.limit:
            progress(args, state="limit_reached")
            return
    progress(args, state="completed")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalog", type=Path, default=SUITE / "catalog.json",
                        help="suite catalog.json; its directory is the suite that is frozen (default: real-repo-v2)")
    parser.add_argument("--allow-draft-suite", action="store_true",
                        help=f"collect a suite still marked {DRAFT_STATUS}; the frozen manifest records this")
    for name, role in (("data", "new evidence root"), ("active", "active trial root")):
        defaults = "; ".join(f"{suite} {roots[name]}" for suite, roots in SUITE_DEFAULTS.items())
        parser.add_argument(f"--{name}", type=Path, help=f"{role} (defaults: {defaults})")
    parser.add_argument("--seed-fixtures", type=Path,
                        help="earlier prepared fixtures to clone (real-repo-v2 default: /private/tmp/erp26-data/fixtures); "
                             "other suites build fixtures from --source-root")
    parser.add_argument("--source-root", type=Path, default=runner.SOURCE_ROOT,
                        help="directory holding the operator repositories; only read, with git archive")
    parser.add_argument("--build-jobs", type=int, default=4, help="cargo jobs for fixture dependency builds")
    parser.add_argument("--engram-release", type=Path, required=True)
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--refresh-snapshots", action="store_true",
                        help="with --prepare-only: re-index prepared local fixtures whose snapshot another Engram release built, "
                             "keeping work trees and dependency builds; earlier snapshots move to superseded/, and a grader "
                             "proof valid before the refresh is rebound to the refreshed fixtures")
    parser.add_argument("--check-only", action="store_true",
                        help="validate fixtures, preflight proofs and frozen controls, then exit without any attempt")
    parser.add_argument("--progress", type=Path, help="progress JSON for a detached collection; also takes the collector lock")
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
    args = parser.parse_args(argv)
    try:
        suite = Suite(args.catalog)
    except (OSError, ValueError, KeyError) as error:
        parser.error(f"unreadable suite catalog {args.catalog}: {error}")
    args.suite = suite
    for name in ("data", "active", "seed_fixtures"):
        if getattr(args, name) is None and suite.defaults.get(name):
            setattr(args, name, Path(suite.defaults[name]))
    if args.data is None or args.active is None:
        parser.error(f"{suite.id} has no default roots; pass --data and --active")
    if args.allow_draft_suite and not suite.draft:
        parser.error("--allow-draft-suite applies only to a suite marked " + DRAFT_STATUS)
    if args.prepare_only and args.check_only:
        parser.error("--prepare-only and --check-only are exclusive")
    if args.refresh_snapshots and not args.prepare_only:
        parser.error("--refresh-snapshots requires --prepare-only")
    if args.refresh_snapshots and (args.seed_fixtures is not None or "seed_fixtures" in suite.defaults):
        parser.error("--refresh-snapshots applies to fixtures built from local repositories; "
                     "prepare seed-fixture collections into a new data root")
    if args.agent == "codex":
        if not args.codex_model:
            parser.error("--codex-model is required with --agent codex")
        try:
            args.agent_adapter = codex_arm.CodexAgent(codex_arm.find_codex(args.codex), args.codex_model,
                                                      args.codex_effort, args.codex_hook_trust)
        except (OSError, ValueError) as error:
            parser.error(str(error))
    runner.validate_paths(args.data, args.active, compact=True)
    args.data, args.active = args.data.resolve(), args.active.resolve()
    if args.seed_fixtures is not None:
        args.seed_fixtures = args.seed_fixtures.resolve()
    args.source_root = args.source_root.resolve()
    refuse_stopped_collection(args.data)
    args.compact_settings = True
    validate_seed_roots(args.data, args.active, args.seed_fixtures if args.seed_fixtures is not None else args.source_root)
    if any(not math.isfinite(value) or value <= 0 for value in (args.agent_seconds, args.grader_seconds, args.cost_cap)) \
            or (args.limit is not None and args.limit <= 0) or args.build_jobs <= 0:
        parser.error("Limits must be positive")
    runner.SUITE = suite.root
    runner.ENGRAM = (args.engram_release / "bin/engram").resolve(strict=True)
    runner.DAEMON = (args.engram_release / "bin/engramd").resolve(strict=True)
    args.data.mkdir(mode=0o700, exist_ok=True)
    catalog = suite.catalog
    tasks = catalog["tasks"]
    if suite.id == V2_SUITE_ID:
        if {task["id"] for task in tasks} != set(catalog["pilot_task_ids"]) or len(tasks) != 4 or catalog["pilot_repetitions"] != 3:
            raise ValueError("V2 collection requires the four declared tasks and three repetitions")
    else:
        validate_suite_tasks(catalog)
    if suite.draft and not args.allow_draft_suite and not args.prepare_only:
        raise ValueError(f"{suite.id} is {DRAFT_STATUS}; collecting it requires --allow-draft-suite, "
                         "which the frozen manifest records")
    if local_fixtures(args):
        # Collection grading uses the same grader settings as the grader preflight.
        args.grader_env = grader_preflight.grader_environment(args.data)
    if args.progress is not None:
        args.progress = args.progress.resolve()
    with collector_lock(args):
        try:
            collect(args, suite, catalog, tasks)
        except BaseException as error:
            progress(args, state="stopped", error=f"{type(error).__name__}: {error}", current=None)
            raise


if __name__ == "__main__":
    main()
