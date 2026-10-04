"""Shared private behavioral grader for the agi-backend-rust held-out tasks.

Run only against an expendable grading copy. A task's grade.py supplies a
RustTask; this module injects the task's private tests, builds offline and
locked, and runs every case in a fresh process. Exit codes: 0 pass,
1 behavioral or compile failure, 2 infrastructure failure.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
import fcntl
import json
import os
from pathlib import Path
import subprocess
import time

# Grading copies must be disposable. Never inject private tests into the
# operator's checkouts or worktrees.
PROTECTED_ROOTS = (Path.home() / "PycharmProjects",)
MARKER = "\n// ENGRAM_HELDOUT_PRIVATE_GRADER_START\n"
SKIPPED_DIRECTORIES = {".git", "target", "node_modules"}
DEFAULT_TARGET = "/tmp/engram-heldout-rust-target"


@dataclass(frozen=True)
class RustTask:
    task_id: str
    private_tests: Path
    test_target: str
    module: str
    cases: tuple[str, ...]
    # Existing integration-test source to extend (its helpers are reused).
    # None writes tests/<test_target>.rs as a new private test crate.
    append_to: str | None = None
    case_timeout_seconds: int = 120
    extra_env: dict[str, str] = field(default_factory=dict)


def refuse_protected(repo: Path) -> None:
    for root in PROTECTED_ROOTS:
        root = root.resolve()
        if repo == root or root in repo.parents:
            raise RuntimeError(f"Refusing to grade inside {root}; supply a disposable grading copy")


def refresh_mtimes(repo: Path) -> int:
    """Historical archives carry old commit-time mtimes. Cargo compares mtimes,
    so a reused target directory could otherwise keep stale artifacts built from
    a different fixture with the same package identity."""
    touched = 0
    for directory, subdirectories, files in os.walk(repo):
        subdirectories[:] = [name for name in subdirectories if name not in SKIPPED_DIRECTORIES]
        for name in files:
            path = Path(directory) / name
            if path.is_file() and not path.is_symlink():
                path.touch()
                touched += 1
    return touched


def inject(repo: Path, task: RustTask) -> Path:
    fixture = task.private_tests.read_text()
    if task.append_to is None:
        destination = repo / "tests" / f"{task.test_target}.rs"
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(fixture)
        return destination
    destination = repo / task.append_to
    original = destination.read_text().split(MARKER)[0]
    destination.write_text(original + MARKER + fixture)
    return destination


def grading_env(task: RustTask) -> dict[str, str]:
    env = {key: os.environ[key] for key in ("PATH", "HOME", "TMPDIR", "CARGO_HOME", "RUSTUP_HOME") if key in os.environ}
    env.update({
        "ENVIRONMENT": "test",
        "AGENT_DATA_SOURCE": "api",
        "USE_REDIS_SESSIONS": "false",
        "REQUIRE_REDIS_WHEN_ENABLED": "false",
        "SENTRY_ENABLED": "false",
        "BILLING_ENABLED": "false",
        "AWS_EC2_METADATA_DISABLED": "true",
        "CARGO_NET_OFFLINE": "true",
        "CARGO_BUILD_JOBS": os.environ.get("ENGRAM_RUST_GRADER_JOBS", "2"),
        "CARGO_TARGET_DIR": os.environ.get("ENGRAM_RUST_GRADER_TARGET", DEFAULT_TARGET),
        "RUST_BACKTRACE": "0",
    })
    env.update(task.extra_env)
    return env


def run(task: RustTask, argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=f"Private behavioral grader for {task.task_id}")
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args(argv)
    repo, output = args.repo.resolve(), args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    logdir = output.with_suffix(".logs")
    logdir.mkdir(exist_ok=True)
    result: dict = {"task": task.task_id, "status": "infra", "checks": [], "logs": str(logdir)}
    started = time.monotonic()
    try:
        refuse_protected(repo)
        if not (repo / "Cargo.toml").is_file():
            raise RuntimeError("Grading copy has no Cargo.toml")
        env = grading_env(task)
        target_dir = Path(env["CARGO_TARGET_DIR"])
        target_dir.mkdir(parents=True, exist_ok=True)
        # Serialize build and execution: another grader sharing the target
        # directory must not replace this candidate's executable mid-run.
        lock = (target_dir / ".engram-heldout-grader.lock").open("a")
        fcntl.flock(lock, fcntl.LOCK_EX)
        # A developer .env must never change what the private tests exercise.
        removed_env = []
        if (repo / ".env").is_file():
            (repo / ".env").unlink()
            removed_env.append(".env")
        result["removed_env_files"] = removed_env
        result["refreshed_files"] = refresh_mtimes(repo)
        inject(repo, task)
        command = ["cargo", "test", "--offline", "--locked", "--test", task.test_target, "--no-run",
                   "--message-format=json"]
        build = subprocess.run(command, cwd=repo, env=env, text=True, capture_output=True, timeout=1800)
        (logdir / "build.stdout.jsonl").write_text(build.stdout)
        (logdir / "build.stderr.log").write_text(build.stderr)
        result["compiler"] = subprocess.check_output(["rustc", "--version"], env=env, text=True).strip()
        result["build_exit_code"] = build.returncode
        artifacts, diagnostics = [], []
        for line in build.stdout.splitlines():
            try:
                item = json.loads(line)
            except json.JSONDecodeError:
                continue
            if (item.get("reason") == "compiler-artifact" and item.get("executable")
                    and item.get("profile", {}).get("test") and item.get("target", {}).get("name") == task.test_target):
                artifacts.append(item["executable"])
            if item.get("reason") == "compiler-message" and item.get("message", {}).get("level") == "error":
                diagnostics.append(item["message"].get("rendered") or item["message"]["message"])
        if build.returncode:
            result["status"] = "fail" if diagnostics else "infra"
            result["checks"].append({"name": "compiles_with_behavioral_tests", "passed": False,
                                     "errors": diagnostics[:20]})
        elif len(artifacts) != 1:
            raise RuntimeError(f"Expected one {task.test_target} test executable, got {len(artifacts)}")
        else:
            result["checks"].append({"name": "compiles_with_behavioral_tests", "passed": True})
            for name in task.cases:
                case_started = time.monotonic()
                try:
                    case = subprocess.run([artifacts[0], f"{task.module}::{name}", "--exact", "--nocapture",
                                           "--test-threads=1"], cwd=repo, env=env, text=True, capture_output=True,
                                          timeout=task.case_timeout_seconds)
                    (logdir / f"{name}.log").write_text(case.stdout + case.stderr)
                    passed = case.returncode == 0 and "1 passed" in case.stdout
                    check = {"name": name, "passed": passed, "exit_code": case.returncode}
                except subprocess.TimeoutExpired as timeout:
                    (logdir / f"{name}.log").write_text(f"timed out after {task.case_timeout_seconds}s\n{timeout}")
                    check = {"name": name, "passed": False, "exit_code": None, "timed_out": True}
                check["seconds"] = round(time.monotonic() - case_started, 3)
                result["checks"].append(check)
            result["status"] = "pass" if all(check["passed"] for check in result["checks"]) else "fail"
    except (OSError, RuntimeError, subprocess.TimeoutExpired, subprocess.CalledProcessError) as error:
        result["error"] = str(error)
    result["elapsed_seconds"] = round(time.monotonic() - started, 3)
    output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"status": result["status"], "output": str(output)}))
    return {"pass": 0, "fail": 1, "infra": 2}[result["status"]]
