#!/usr/bin/env python3
"""Historical engineering replay. Run under caffeinate -dims; raw evidence stays private.

This intentionally uses a separate adapter from the synthetic Rust evaluator:
setup, coding, and private grading have independent clocks and timeouts.
"""
import argparse
import contextlib
import datetime
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import sqlite3
import subprocess
import sys
import tarfile
import time
import uuid

sys.dont_write_bytecode = True
from eval_context_quality import wait_ready
from real_repo_metrics import parse_trace, paired_summary

ROOT = Path(__file__).resolve().parents[1]
SUITE = ROOT / "benchmarks/real-repo-v1"
ENGRAM = (Path.home() / ".local/bin/engram").resolve()
DAEMON = (Path.home() / ".local/bin/engramd").resolve()
CLAUDE = Path("/opt/homebrew/bin/claude").resolve()
MODEL = "claude-opus-4-8[1m]"
TOOLS = "Bash,Read,Edit,Write,Glob,Grep"
PLATFORM_DEPS = Path("/tmp/engram-platform-preflight-20260924/node_modules").resolve()
SOURCE_ROOT = Path.home() / "PycharmProjects"


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n")


def digest(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def clean_env():
    env = {k: os.environ[k] for k in ("HOME", "PATH", "USER", "LOGNAME", "TMPDIR", "SHELL", "LANG") if k in os.environ}
    env.update({"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "CARGO_NET_OFFLINE": "true", "CARGO_BUILD_JOBS": "2", "ENGRAM_WORKSPACE": "default", "AWS_EC2_METADATA_DISABLED": "true", "ENVIRONMENT": "test", "AGENT_DATA_SOURCE": "api", "USE_REDIS_SESSIONS": "false", "REQUIRE_REDIS_WHEN_ENABLED": "false", "SENTRY_ENABLED": "false", "BILLING_ENABLED": "false"})
    env.update({"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_SYSTEM": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"})
    return env


def coding_env(work, *, compact=False):
    env = clean_env()
    scratch = work.parent / "scratch"
    env.update({"CLAUDE_CODE_TMPDIR": str(scratch), "TMPDIR": str(scratch), "CARGO_HOME": str(scratch / "cargo-home"), "CARGO_TARGET_DIR": str(work / "target"), "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "core.excludesFile", "GIT_CONFIG_VALUE_0": "/dev/null", "pnpm_config_pm_on_fail": "ignore", "pnpm_config_verify_deps_before_run": "false", "NO_UPDATE_NOTIFIER": "1"})
    if compact:
        # Pin the actual Bash tool shell; zsh's private heredoc prefix can point
        # outside the sandbox's writable scratch even when TMPDIR is isolated.
        env.update({"SHELL": "/bin/bash", "CLAUDE_CODE_SHELL": "/bin/bash"})
    return env


def command(args, cwd=None, timeout=1800, log=None, env=None):
    result = subprocess.run(list(map(str, args)), cwd=cwd, env=env or clean_env(), text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout)
    if log:
        log.write_text(result.stdout)
    if result.returncode:
        raise RuntimeError(f"command failed ({result.returncode}): {args[0]} {args[1:3]}; log={log}")
    return result.stdout


def clone(source, dest):
    if dest.exists() or dest.is_symlink():
        raise RuntimeError(f"Refusing to overwrite {dest}")
    # APFS copy-on-write preserves isolation without multiplying build-cache storage.
    command(["/bin/cp", "-cR", source, dest])


def config_text(db, sock):
    return f'''workspace = "default"
[storage]
vector_backend = "disabled"
db_path = {json.dumps(str(db))}
[daemon]
socket_path = {json.dumps(str(sock))}
[web]
enabled = false
[watch]
enabled = false
[jobs]
enabled = false
[vault]
enabled = false
auto_refresh = false
root = {json.dumps(str(db.parent / "vault"))}
[judge]
enabled = false
[architect]
auto = "off"
[synthesis]
auto = false
cross_project_hypotheses = false
[brain]
auto_learn = false
conservative_promotion = true
raw_transcript_retention_days = 0
[links]
auto_scan = false
[context]
max_injected_tokens = 2000
stable_prefix_tokens = 300
include_linked_projects = false
semantic_retrieval = false
[structural]
provider = "tree_sitter"
git_history_commits = 1
[embeddings]
provider = "disabled"
model = ""
allow_remote = false
[sync]
central_url = ""
every_secs = 0
push = false
[security]
custom_redaction_patterns = []
[network]
local_only = true
'''


@contextlib.contextmanager
def daemon(config, state):
    with (state / "daemon.log").open("a") as log:
        process = subprocess.Popen([str(DAEMON), "--config", str(config)], env=clean_env(), stdout=log, stderr=log, start_new_session=True)
        try:
            wait_ready(process, state / "d.sock")
            yield process
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(15)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()


def cli(state, *args):
    return command([ENGRAM, "--workspace", "default", "--db", state / "brain.sqlite", "--socket", state / "d.sock", *args])


def prepare(task, data):
    folder = data / "fixtures" / task["id"]
    ready = folder / "ready.json"
    if ready.exists():
        return json.loads(ready.read_text())
    folder.mkdir(parents=True, exist_ok=True)
    work = folder / "work"
    if work.exists():
        raise RuntimeError(f"Incomplete preparation at {folder}; inspect before removing it")
    started = time.monotonic()
    work.mkdir()
    archive = folder / "base.tar"
    with archive.open("wb") as out:
        subprocess.run(["git", "-C", str(SOURCE_ROOT / task["repository"]), "archive", task["base_commit"]], stdout=out, check=True)
    with tarfile.open(archive) as tree:
        tree.extractall(work, filter="data")
    archive.unlink()
    # Never retain a tracked environment secret or links reaching outside the fixture.
    removed = []
    for path in work.rglob("*"):
        if path.is_symlink() and not path.resolve().is_relative_to(work.resolve()):
            removed.append(str(path.relative_to(work)))
            path.unlink()
        elif path.is_file() and path.name == ".env":
            removed.append(str(path.relative_to(work)))
            path.unlink()
    (work / ".engram").mkdir(exist_ok=True)
    project_id = "prj_" + str(uuid.uuid5(uuid.NAMESPACE_URL, "engram-real-repo-v1/" + task["id"]))
    (work / ".engram/project.toml").write_text(f'project_id = "{project_id}"\nname = "{task["repository"]}"\n')
    # One root commit only, with no future objects, remote, hooks, or user identity.
    command(["git", "init", "-q", work])
    command(["git", "-C", work, "config", "core.hooksPath", "/dev/null"])
    command(["git", "-C", work, "add", "-A"])
    command(["git", "-C", work, "-c", "user.name=Benchmark", "-c", "user.email=benchmark@invalid", "commit", "-qm", "Historical source fixture"])
    state = folder / "seed"
    state.mkdir()
    config = state / "config.toml"
    config.write_text(config_text(state / "brain.sqlite", state / "d.sock"))
    (work / ".engram/config.toml").write_text(config.read_text())
    index_start = time.monotonic()
    with daemon(config, state):
        index = json.loads(cli(state, "index", work, "--json"))
    snapshot = folder / "snapshot.sqlite"
    with sqlite3.connect(state / "brain.sqlite") as source, sqlite3.connect(snapshot) as target:
        source.backup(target)
    with sqlite3.connect(snapshot) as con:
        con.execute("DELETE FROM project_aliases")
        con.commit()
        con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        counts = {table: con.execute(f"SELECT count(*) FROM {table}").fetchone()[0] for table in ("nodes", "memories", "tasks", "context_runs")}
    source.close()
    target.close()
    con.close()
    if counts["memories"] or counts["tasks"] or counts["context_runs"]:
        raise RuntimeError("source-only snapshot contains episode/context data")
    index_seconds = time.monotonic() - index_start
    (work / ".engram/config.toml").unlink()
    deps_start = time.monotonic()
    if task["repository"] == "agi-backend-rust":
        # Build only the BASE here; never copy the grader/reference target cache.
        command(["cargo", "test", "--offline", "--locked", "--no-run"], cwd=work, timeout=1800, log=folder / "dependencies.log")
    elif task["repository"] == "platform":
        clone(PLATFORM_DEPS, work / "node_modules")
    info = {"task_id": task["id"], "base_commit": task["base_commit"], "project_id": project_id,
            "fixture_head": command(["git", "-C", work, "rev-parse", "HEAD"]).strip(),
            "snapshot_sha256": digest(snapshot), "source_counts": counts,
            "index_seconds": round(index_seconds, 3), "dependency_seconds": round(time.monotonic() - deps_start, 3),
            "setup_seconds": round(time.monotonic() - started, 3), "removed_paths": removed,
            "index": index, "rustc": command(["rustc", "--version"]).strip(),
            "lockfile_sha256": {p.name: digest(p) for p in work.glob("*lock*") if p.is_file()}}
    write_json(ready, info)
    return info


def prepare_private_dependencies(task, data, active):
    if task["repository"] != "agi-backend-rust":
        return
    fixture = data / "fixtures" / task["id"]
    ready = json.loads((fixture / "ready.json").read_text())
    if ready.get("cargo_home_scope") and (fixture / "cargo-home/registry").is_dir():
        return
    validate_paths(data, active)
    home = fixture / "cargo-home"
    if home.exists():
        raise RuntimeError(f"incomplete private dependency preparation: {home}")
    home.mkdir()
    clone(Path.home() / ".cargo/registry", home / "registry")
    if active.exists():
        shutil.rmtree(active)
    active.mkdir(mode=0o700)
    (active / ".pilot-owned").write_text("real-repo-v1\n")
    clone(fixture / "work", active / "work")
    (active / "scratch").mkdir()
    clone(home, active / "scratch/cargo-home")
    started = time.monotonic()
    command(["cargo", "test", "--offline", "--locked", "--no-run"], cwd=active / "work", env=coding_env(active / "work"), log=fixture / "private-dependencies.log")
    shutil.rmtree(fixture / "work/target")
    clone(active / "work/target", fixture / "work/target")
    ready["private_dependency_seconds"] = round(time.monotonic() - started, 3)
    ready["cargo_home_scope"] = "private registry-only copy; no global config or git checkouts"
    write_json(fixture / "ready.json", ready)


def settings(work, data, active, state, arm, *, compact=False):
    private = [data.resolve(), ROOT.resolve(), SOURCE_ROOT.resolve(), Path.home() / ".engram", Path.home() / ".codex", Path.home() / ".claude"]
    if compact:
        validate_paths(data, active, compact=True)
        if sys.platform != "darwin":
            raise ValueError("Compact sandbox glob policy is verified only on macOS")
        # On macOS the sandbox runtime compiles these directly to regex. A
        # trailing '/**' is stripped by that runtime, so use an unbroken '**'.
        temp_globs = ["/private/tmp/engram-**", "/private/tmp/erp26-**"]
        patterns = ("claude-*", "erp26work-*")
    else:
        temp_globs = []
        private += [p.resolve() for p in Path("/private/tmp").glob("engram-*")]
        patterns = ("claude-*", "erp26-*")
    private += [p.resolve() for pattern in patterns for p in Path("/private/tmp").glob(pattern) if p.resolve() != active.resolve() and not active.resolve().is_relative_to(p.resolve())]
    private += [state.resolve()]
    # Claude's cwd helper remains in /tmp/claude-UID even with a private TMPDIR.
    # Permit only new cwd-* files there; protect every existing child explicitly.
    scratch_roots = [p for p in private if p.name.startswith("claude-") and p.is_dir()]
    old_scratch = [child for p in scratch_roots for child in p.iterdir()]
    if any(p.name.startswith("cwd-") for p in old_scratch):
        raise RuntimeError("existing Claude cwd bookkeeping detected; cannot safely allow scratch glob")
    denies = [f"{tool}(/{p}/**)" for p in private if p not in scratch_roots for tool in ("Read", "Edit", "Write", "Glob", "Grep")]
    denies += [f"{tool}(/{p}/**)" for p in scratch_roots for tool in ("Read", "Glob", "Grep")]
    for child in old_scratch:
        for tool in ("Edit", "Write"):
            denies.extend([f"{tool}(/{child})", f"{tool}(/{child}/**)"])
    denies += [f"{tool}(//Users/corwil/**)" for tool in ("Read", "Edit", "Write", "Glob", "Grep")]
    if compact:
        # Native tool globs need both top-level files and directory descendants.
        denies += [f"{tool}(//private/tmp/{prefix}{suffix})"
                   for prefix in ("engram-", "erp26-") for suffix in ("*", "*/**")
                   for tool in ("Read", "Edit", "Write", "Glob", "Grep")]
    denies += [f"{tool}(/{work}/.engram/**)" for tool in ("Edit", "Write")]
    value = {"disableAllHooks": arm == "off", "autoMemoryEnabled": False,
             "claudeMdExcludes": ["/Users/corwil/**", str(data / "**")],
             "permissions": {"deny": denies},
             "sandbox": {"enabled": True, "failIfUnavailable": True, "autoAllowBashIfSandboxed": True,
                         "allowUnsandboxedCommands": False,
                         "filesystem": {"denyRead": [*map(str, private), *temp_globs],
                                        "allowRead": [str(p / "cwd-*") for p in scratch_roots],
                                        "denyWrite": [str(work / ".engram"), *map(str, old_scratch)],
                                        "allowWrite": [str(work), str(active / "scratch"), *[str(p / "cwd-*") for p in scratch_roots]]},
                         "network": {"allowedDomains": [], "allowLocalBinding": True}}}
    if arm == "brain":
        hook_command = " ".join(map(shlex.quote, map(str, [ENGRAM, "--workspace", "default", "--db", state / "brain.sqlite", "--socket", state / "d.sock"])))
        events = {"SessionStart": ("session-start", 15), "UserPromptSubmit": ("prompt", 30), "PreToolUse": ("pretool", 10), "PostToolUse": ("bash", 15), "PostToolUseFailure": ("bash", 15), "Stop": ("stop", 15), "SessionEnd": ("session-end", 15)}
        value["hooks"] = {}
        for event, (subcommand, timeout) in events.items():
            entry = {"hooks": [{"type": "command", "command": hook_command + " hook " + subcommand, "timeout": timeout}]}
            if event == "PreToolUse":
                entry["matcher"] = "Read|Edit|Write|MultiEdit"
            value["hooks"][event] = [entry]
    return value


def receipt_summary(state):
    with contextlib.closing(sqlite3.connect(state / "brain.sqlite")) as con:
        compiled, tokens = con.execute("SELECT count(*), coalesce(sum(selected_tokens),0) FROM context_runs").fetchone()
        delivered = con.execute("SELECT DISTINCT c.id,c.selected_tokens,t.session_id FROM usage_events u JOIN context_runs c ON c.id=u.context_run_id JOIN tasks t ON t.id=u.source_id WHERE u.source_type='task' AND u.usage_kind='context_served' AND c.mode='brain' AND length(c.stable_prefix || c.dynamic_context)>0").fetchall()
        return {"emission_receipts": len(delivered), "compiled_runs": compiled,
                "compiled_estimated_tokens": tokens, "delivered_run_ids": [r[0] for r in delivered],
                "delivered_sessions": list({r[2] for r in delivered}),
                "delivered_compiled_estimated_tokens": sum(r[1] for r in delivered),
                "source_items": con.execute("SELECT count(*) FROM context_items WHERE source_type IN ('code','code_dependency','code_use','verification')").fetchone()[0]}


def agent_command(settings_path, cost):
    return [str(CLAUDE), "-p", "--model", MODEL, "--effort", "high", "--max-budget-usd", str(cost),
            "--output-format", "stream-json", "--verbose", "--include-hook-events", "--no-session-persistence",
            "--setting-sources", "", "--settings", str(settings_path), "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
            "--disable-slash-commands", "--no-chrome", "--permission-mode", "dontAsk", "--tools", TOOLS, "--allowedTools", TOOLS]


def execute_agent(work, settings_path, prompt, trace, stderr, wall, cost, *, compact=False):
    started = time.monotonic()
    timed_out = False
    agent_env = coding_env(work, compact=compact)
    (work.parent / "scratch").mkdir(exist_ok=True)
    with trace.open("w") as out, stderr.open("w") as err:
        proc = subprocess.Popen(agent_command(settings_path, cost), cwd=work, env=agent_env, stdin=subprocess.PIPE, stdout=out, stderr=err, text=True, start_new_session=True)
        try:
            proc.communicate(prompt, timeout=wall)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(proc.pid, signal.SIGTERM)
            try:
                proc.wait(10)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
    return {"agent_wall_seconds": round(time.monotonic() - started, 3), "agent_exit_code": proc.returncode, "agent_timed_out": timed_out}


class ClaudeAgent:
    """Claude Code headless adapter: the original pilot behavior.

    run_trial uses an adapter for agent-specific setup, execution, telemetry
    and model checks; scripts/real_repo_codex.py provides the Codex one.
    """

    name = "claude"
    model = MODEL

    def limits(self, args):
        return {"max_budget_usd_estimate": args.cost_cap, "model": MODEL, "effort": "high"}

    def prepare(self, *, trial, work, state, arm, args):
        settings_path = state / "settings.json"
        write_json(settings_path, settings(work, args.data, args.active, state, arm,
                                           compact=getattr(args, "compact_settings", False)))
        shutil.copy2(settings_path, trial / "settings.json")
        return {"trial": trial, "work": work, "settings_path": settings_path}

    def execute(self, prepared, prompt, args):
        trial = prepared["trial"]
        return execute_agent(prepared["work"], prepared["settings_path"], prompt, trial / "trace.jsonl", trial / "stderr.log",
                             args.agent_seconds, args.cost_cap, compact=getattr(args, "compact_settings", False))

    def metrics(self, prepared):
        return parse_trace(prepared["trial"] / "trace.jsonl")

    def record_flags(self, metrics):
        return {}

    def models(self, metrics):
        return list(metrics.get("model_usage", {}))

    def preserve(self, prepared):
        """Claude's private state is the Engram state directory, cloned by run_trial."""


CLAUDE_AGENT = ClaudeAgent()


def run_trial(task, repetition, arm, args):
    agent = getattr(args, "agent_adapter", None) or CLAUDE_AGENT
    ident = f'{task["id"]}-r{repetition}-{arm}'
    trial = args.data / "trials" / ident
    if trial.exists():
        raise RuntimeError(f"Trial already exists; refusing a silent retry: {ident}")
    trial.mkdir(parents=True)
    record = {"id": ident, "task_id": task["id"], "repetition": repetition, "arm": arm,
              "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(), "grade": {"status": "infrastructure_error"}}
    work, state = args.active / "work", args.active / "state"
    if args.active.exists():
        shutil.rmtree(args.active)
    args.active.mkdir(mode=0o700)
    (args.active / ".pilot-owned").write_text("real-repo-v1\n")
    setup_start = time.monotonic()
    prepared = None
    try:
        fixture = args.data / "fixtures" / task["id"]
        clone(fixture / "work", work)
        state.mkdir()
        (args.active / "scratch").mkdir()
        if task["repository"] == "agi-backend-rust":
            clone(fixture / "cargo-home", args.active / "scratch/cargo-home")
        config = state / "config.toml"
        config.write_text(config_text(state / "brain.sqlite", state / "d.sock"))
        (work / ".engram/config.toml").write_text(config.read_text())
        prepared = agent.prepare(trial=trial, work=work, state=state, arm=arm, args=args)
        record["prompt_sha256"] = digest(SUITE / task["prompt_file"])
        record["snapshot_sha256"] = digest(fixture / "snapshot.sqlite")
        record["grader_manifest"] = {str(p.relative_to(SUITE)): digest(p) for p in sorted((SUITE / "grading" / task["id"]).glob("*")) if p.is_file() and p.suffix in (".py", ".rs", ".ts", ".json")}
        prompt = (SUITE / task["prompt_file"]).read_text() + "\n\nImplement the change in this disposable repository and add focused local regression tests. Work only in this checkout. Do not deploy, contact remote services, install dependencies, or access other checkouts. Dependencies are prepared where available; network is disabled. Use offline tests and local test doubles. The checkout has intentionally limited Git history. Do not run broad integration suites requiring external databases. Leave the completed patch in the working tree.\n"
        if task["repository"] == "agents":
            prompt += "The existing pytest conftest requires unavailable development services. Use standard-library isolated tests or syntax checks without importing that conftest.\n"
        record["composed_prompt_sha256"] = hashlib.sha256(prompt.encode()).hexdigest()
        (trial / "prompt.txt").write_text(prompt)
        write_json(trial / "limits.json", {"agent_seconds": args.agent_seconds, "grader_seconds": args.grader_seconds, **agent.limits(args)})
        manager = contextlib.nullcontext()
        if arm == "brain":
            shutil.copy2(fixture / "snapshot.sqlite", state / "brain.sqlite")
            manager = daemon(config, state)
        with manager:
            if arm == "brain":
                cli(state, "status", work, "--json")
            # Relocation rebuild happens before timing, identically in both arms.
            if task["repository"] == "agi-backend-rust":
                command(["cargo", "test", "--offline", "--locked", "--no-run"], cwd=work, env=coding_env(work, compact=getattr(args, "compact_settings", False)), log=trial / "warmup.log")
            record["setup_seconds"] = round(time.monotonic() - setup_start, 3)
            print(f"START {ident}", flush=True)
            record.update(agent.execute(prepared, prompt, args))
            record["metrics"] = agent.metrics(prepared)
            record.update(agent.record_flags(record["metrics"]))
            record["delivery"] = receipt_summary(state) if arm == "brain" else {"emission_receipts": 0, "compiled_runs": 0}
        # Store complete private state and patch before injecting any grader code.
        clone(work, trial / "candidate")
        clone(state, trial / "state")
        agent.preserve(prepared)
        (trial / "patch.diff").write_text(command(["git", "-C", work, "diff", "HEAD"]))
        grade_copy = args.data / "grade-work"
        if grade_copy.exists():
            shutil.rmtree(grade_copy)
        clone(work, grade_copy)
        grade_start = time.monotonic()
        grade_command = [sys.executable, str(SUITE / "grading" / task["id"] / "grade.py"), "--repo", str(grade_copy), "--output", str(trial / "grade.json")]
        with (trial / "grader.log").open("w") as log:
            graded = subprocess.run(grade_command, env=clean_env(), stdout=log, stderr=log, timeout=args.grader_seconds)
        record["grading_seconds"] = round(time.monotonic() - grade_start, 3)
        record["grade"] = json.loads((trial / "grade.json").read_text())
        record["grader_exit_code"] = graded.returncode
        models = agent.models(record["metrics"])
        record["validity_issues"] = []
        if models and models != [agent.model]:
            record["validity_issues"].append("model_drift")
        if arm == "brain" and record["delivery"]["emission_receipts"] == 0:
            record["validity_issues"].append("no_delivery_receipt")
        if record["validity_issues"]:
            record["behavioral_grade"] = record["grade"]
            record["grade"] = {"status": "infrastructure_error", "issues": record["validity_issues"]}
    except Exception as error:
        record["infrastructure_error"] = repr(error)
        if prepared is not None and (trial / "trace.jsonl").exists():
            record["metrics"] = agent.metrics(prepared)
        if prepared is not None:
            # Keep agent-private evidence of the failed attempt when possible.
            with contextlib.suppress(Exception):
                agent.preserve(prepared)
    # Follow-up collections can verify frozen controls after coding and grading,
    # before either canonical record is persisted. The original pilot has no callback.
    validator = getattr(args, "validate_record", None)
    if validator is not None:
        validator(record)
    write_json(trial / "record.json", record)
    with (args.data / "records.jsonl").open("a") as out:
        out.write(json.dumps(record) + "\n")
    print(f"END {ident} grade={record['grade']['status']} seconds={record.get('agent_wall_seconds')} tools={record.get('metrics', {}).get('tool_calls')}", flush=True)
    return record


def report(args):
    records = [json.loads(line) for line in (args.data / "records.jsonl").read_text().splitlines()] if (args.data / "records.jsonl").exists() else []
    result = paired_summary(records)
    write_json(args.data / "summary.json", result)
    return records


def validate_paths(data, active, *, compact=False):
    for path in (data, active):
        prefix = "erp26work-" if compact and path == active else "erp26-"
        if path.is_symlink() or path.resolve().parent != Path("/private/tmp") or not path.name.startswith(prefix):
            raise ValueError(f"benchmark roots must be separate non-symlink /private/tmp/{prefix}* directories")
    if data.resolve() == active.resolve():
        raise ValueError("data and active must differ")
    if active.exists() and not (active / ".pilot-owned").is_file():
        raise ValueError("refusing to clear an active directory without the pilot ownership marker")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--data", type=Path, default=Path("/private/tmp/erp26-data"))
    p.add_argument("--active", type=Path, default=Path("/private/tmp/erp26-active"))
    p.add_argument("--prepare-only", action="store_true")
    p.add_argument("--limit", type=int)
    p.add_argument("--tasks", nargs="*")
    p.add_argument("--agent-seconds", type=int, default=1200)
    p.add_argument("--grader-seconds", type=int, default=1800)
    p.add_argument("--cost-cap", type=float, default=8)
    args = p.parse_args()
    validate_paths(args.data, args.active)
    args.data, args.active = args.data.resolve(), args.active.resolve()
    if args.data == args.active or args.data.is_relative_to(args.active) or args.active.is_relative_to(args.data):
        p.error("data and active must be separate sibling directories")
    args.data.mkdir(parents=True, exist_ok=True, mode=0o700)
    catalog = json.loads((SUITE / "catalog.json").read_text())
    tasks = [t for t in catalog["tasks"] if t["id"] in (args.tasks or catalog["pilot_task_ids"])]
    for task in tasks:
        print("PREPARE " + task["id"], flush=True)
        prepare(task, args.data)
        prepare_private_dependencies(task, args.data, args.active)
    if args.prepare_only:
        return
    done = {r["id"] for r in report(args)}
    count = 0
    for repetition in range(1, catalog["pilot_repetitions"] + 1):
        for index, task in enumerate(tasks):
            arms = ["off", "brain"] if (index + repetition) % 2 else ["brain", "off"]
            for arm in arms:
                if f'{task["id"]}-r{repetition}-{arm}' in done:
                    continue
                run_trial(task, repetition, arm, args)
                report(args)
                count += 1
                if args.limit and count >= args.limit:
                    return


if __name__ == "__main__":
    main()
