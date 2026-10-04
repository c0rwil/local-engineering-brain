"""Shared calibration driver for the held-out graders.

Exports the historical base and reference trees with ``git archive`` (read-only
on the source repository), derives each control by exact anchored edits, runs
the task's grade.py on every control and records the actual outcome. Every
control runs; the report is "pass" only when all of them behave as expected.
Calibration is grader evidence, never a coding-agent outcome.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
import datetime
import difflib
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time

SUITE_ID = "real-repo-heldout-v1"
SHARED = Path(__file__).resolve().parent


@dataclass(frozen=True)
class Edit:
    path: str
    before: str
    after: str


@dataclass(frozen=True)
class Control:
    name: str
    kind: str  # base | reference | mutant | alternative
    source: str  # base | reference
    expect: str  # pass | fail
    description: str
    edits: tuple[Edit, ...] = ()
    new_files: tuple[tuple[str, str], ...] = ()
    # Checks that must fail. With exact_failures, no other check may fail.
    required_failures: tuple[str, ...] = ()
    exact_failures: bool = True


@dataclass
class Task:
    task_id: str
    base_commit: str
    reference_commit: str
    grader: Path
    preflight: Path
    private_files: tuple[Path, ...]
    expected_check_count: int
    controls: tuple[Control, ...]
    toolchain_command: tuple[str, ...] | None = None
    grader_env: dict[str, str] = field(default_factory=dict)
    grader_timeout_seconds: int = 3600


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def export(source_repo: Path, revision: str, destination: Path) -> None:
    destination.mkdir(parents=True)
    archive = subprocess.check_output(["git", "-C", str(source_repo), "archive", revision])
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        tar.extractall(destination, filter="data")


def replace_exactly_once(path: Path, before: str, after: str) -> None:
    text = path.read_text()
    count = text.count(before)
    if count != 1:
        raise RuntimeError(f"Control anchor must occur exactly once in {path} (found {count}): {before[:120]!r}")
    path.write_text(text.replace(before, after))


def prepare(root: Path, control: Control) -> tuple[Path, str]:
    destination = root / control.name
    if control.name not in ("base", "reference"):
        shutil.copytree(root / control.source, destination, symlinks=True)
    patch = []
    touched = []
    for edit in control.edits:
        target = destination / edit.path
        before = target.read_text()
        replace_exactly_once(target, edit.before, edit.after)
        touched.append((edit.path, before))
    for relative, content in control.new_files:
        target = destination / relative
        before = target.read_text() if target.exists() else ""
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)
        touched.append((relative, before))
    seen = set()
    for relative, before in touched:
        if relative in seen:
            continue
        seen.add(relative)
        original = (root / control.source / relative)
        original_text = original.read_text() if original.exists() else ""
        patch.append("".join(difflib.unified_diff(
            original_text.splitlines(True), (destination / relative).read_text().splitlines(True),
            fromfile=f"{control.source}/{relative}", tofile=f"{control.name}/{relative}")))
    return destination, "".join(patch)


def evaluate(control: Control, task: Task, exit_code: int, result: dict) -> tuple[bool, list[str]]:
    reasons = []
    checks = result.get("checks", [])
    failed = {check["name"] for check in checks if not check.get("passed")}
    if len(checks) != task.expected_check_count:
        reasons.append(f"expected {task.expected_check_count} checks, got {len(checks)}")
    if control.expect == "pass":
        if exit_code != 0 or result.get("status") != "pass":
            reasons.append(f"expected pass, got status={result.get('status')} exit={exit_code}")
        if failed:
            reasons.append(f"unexpected failures: {sorted(failed)}")
    else:
        if exit_code != 1 or result.get("status") != "fail":
            reasons.append(f"expected behavioral fail, got status={result.get('status')} exit={exit_code}")
        missing = set(control.required_failures) - failed
        if missing:
            reasons.append(f"required failures did not fail: {sorted(missing)}")
        if control.exact_failures and failed - set(control.required_failures):
            reasons.append(f"additional failures: {sorted(failed - set(control.required_failures))}")
    return not reasons, reasons


def main(task: Task, argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=f"Calibrate the {task.task_id} grader")
    parser.add_argument("--source-repo", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--root", type=Path, help="Fixture root (default: a new /private/tmp directory)")
    parser.add_argument("--only", nargs="*", help="Run only these controls (report is then incomplete)")
    args = parser.parse_args(argv)
    if args.root:
        args.root.mkdir(parents=True, exist_ok=False)
        root = args.root.resolve()
    else:
        root = Path(tempfile.mkdtemp(prefix=f"engram-heldout-{task.task_id}-", dir="/private/tmp"))
    export(args.source_repo, task.base_commit, root / "base")
    export(args.source_repo, task.reference_commit, root / "reference")
    selected = [control for control in task.controls if not args.only or control.name in args.only]
    report = {
        "suite_id": SUITE_ID,
        "task_id": task.task_id,
        "base_commit": task.base_commit,
        "reference_fix_commit": task.reference_commit,
        "generated_at": datetime.datetime.now(datetime.UTC).isoformat(timespec="seconds"),
        "python": sys.version.split()[0],
        "fixture_root": str(root),
        "grader_sha256": sha256(task.grader),
        "private_files_sha256": {path.name: sha256(path) for path in task.private_files},
        "preflight_sha256": sha256(task.preflight),
        "shared_sha256": {path.name: sha256(path) for path in sorted(SHARED.glob("*.py"))},
        "expected_check_count": task.expected_check_count,
        "controls": [{"name": c.name, "kind": c.kind, "source": c.source, "expect": c.expect,
                      "description": c.description, "required_failures": list(c.required_failures),
                      "exact_failures": c.exact_failures} for c in selected],
        "mutations": {},
        "runs": [],
        "status": "incomplete",
    }
    if task.toolchain_command:
        report["toolchain"] = subprocess.check_output(list(task.toolchain_command), text=True).strip()
    env = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1", **task.grader_env}
    for control in selected:
        directory, patch = prepare(root, control)
        if patch:
            report["mutations"][control.name] = patch
        result_path = root / f"{control.name}-result.json"
        print(f"Calibrating {task.task_id}: {control.name}", flush=True)
        started = time.monotonic()
        run = subprocess.run([sys.executable, "-B", str(task.grader), "--repo", str(directory),
                              "--output", str(result_path)], text=True, capture_output=True,
                             timeout=task.grader_timeout_seconds, env=env)
        (root / f"{control.name}.log").write_text(run.stdout + run.stderr)
        try:
            result = json.loads(result_path.read_text())
        except (OSError, json.JSONDecodeError) as error:
            result = {"status": "infra", "checks": [], "error": f"grader wrote no result: {error}"}
        calibrated, reasons = evaluate(control, task, run.returncode, result)
        report["runs"].append({
            "fixture": control.name, "kind": control.kind, "expect": control.expect,
            "expected_failure_checks": list(control.required_failures),
            "exit_code": run.returncode, "calibrated": calibrated, "reasons": reasons,
            "failed_checks": sorted(c["name"] for c in result.get("checks", []) if not c.get("passed")),
            "driver_seconds": round(time.monotonic() - started, 3),
            "result": result,
        })
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    complete = len(report["runs"]) == len(task.controls)
    all_calibrated = all(run["calibrated"] for run in report["runs"])
    report["status"] = "pass" if complete and all_calibrated else ("fail" if not all_calibrated else "incomplete")
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "output": str(args.output), "fixture_root": str(root)}))
    return 0 if report["status"] == "pass" else 1
