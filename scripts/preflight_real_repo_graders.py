#!/usr/bin/env python3
"""Recalibrate a suite's private graders against the collection's prepared fixtures.

Each task's base control is the prepared fixture itself: a clone of its work
tree without the build cache. The reference control is that fixture with the
historical fix applied from the operator's repository (a read-only `git diff`).
Mutants and alternatives use the task's own anchored edits from preflight.py.
Graders run as collection grading runs them: a clean environment plus the
collection's grader settings. Every outcome must match the recorded calibration
in preflight.json: the same exit code, the same failed checks and the same check
count. Only a complete run with every control matched writes a passing
grader-preflight.json. run_real_repo_followup.py requires that proof before it
collects a suite whose fixtures it built from local repositories.
"""
import argparse
import datetime
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import time

sys.dont_write_bytecode = True
import run_real_repo_pilot as runner  # noqa: E402

SCHEMA_VERSION = 1
PROOF = "grader-preflight.json"
OUTPUT = "grader-preflight"
GRADER_JOBS = "4"


def grader_environment(data):
    """Settings the held-out Rust graders read; the same in this preflight and in collection grading."""
    return {"ENGRAM_RUST_GRADER_TARGET": str(data / "grader-target"), "ENGRAM_RUST_GRADER_JOBS": GRADER_JOBS}


def grading_digests(suite_root):
    """Graders, shared grading code, control definitions and recorded calibration."""
    grading = suite_root / "grading"
    return {str(path.relative_to(suite_root)): runner.digest(path) for path in sorted(grading.rglob("*"))
            if path.is_file() and "__pycache__" not in path.parts}


def source_git_env():
    # Read-only use of the operator's repository: no optional locks, no user configuration.
    return {**runner.clean_env(), "GIT_OPTIONAL_LOCKS": "0"}


def source_git(repository, *arguments):
    return subprocess.run(["git", "-C", str(repository), *arguments], env=source_git_env(),
                          capture_output=True, check=True).stdout


def shared_module(suite_root):
    """Load the suite's preflight_common once, so task definitions bind to the same module."""
    path = (suite_root / "grading/_shared/preflight_common.py").resolve()
    loaded = sys.modules.get("preflight_common")
    if loaded is not None and Path(loaded.__file__).resolve() == path:
        return loaded
    spec = importlib.util.spec_from_file_location("preflight_common", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules["preflight_common"] = module
    spec.loader.exec_module(module)
    return module


def task_definition(suite_root, task):
    common = shared_module(suite_root)
    path = suite_root / "grading" / task["id"] / "preflight.py"
    spec = importlib.util.spec_from_file_location(f"grader_preflight_{task['id']}", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    definition = module.TASK
    if (definition.task_id, definition.base_commit, definition.reference_commit) != (
            task["id"], task["base_commit"], task["reference_fix_commit"]):
        raise ValueError(f"{path} does not define the catalog's task and commits")
    return definition, common


def clone_without_build_cache(source, destination):
    destination.mkdir()
    for child in sorted(source.iterdir()):
        if child.name != "target":
            runner.clone(child, destination / child.name)


def apply_reference(repository, task, tree):
    """Apply the historical fix to a fixture copy and verify every changed path."""
    base, reference = task["base_commit"], task["reference_fix_commit"]
    patch = source_git(repository, "diff", "--binary", "--full-index", "--no-renames", "--no-ext-diff",
                       "--no-textconv", base, reference)
    subprocess.run(["git", "apply", "--binary", "--whitespace=nowarn", "-"], cwd=tree, input=patch,
                   env=runner.clean_env(), capture_output=True, check=True)
    fields = source_git(repository, "diff", "--name-status", "-z", "--no-renames", base, reference).split(b"\0")
    changes = list(zip(fields[0::2], fields[1::2]))
    for status, name in changes:
        relative = name.decode()
        target = tree / relative
        if status == b"D":
            if target.exists() or target.is_symlink():
                raise RuntimeError(f"Reference control still contains deleted {relative}")
        elif target.read_bytes() != source_git(repository, "cat-file", "blob", f"{reference}:{relative}"):
            raise RuntimeError(f"Reference control differs from the historical fix at {relative}")
    return len(changes)


def matches_recorded(recorded, exit_code, failed, checks, calibrated):
    return (recorded is not None and calibrated and recorded.get("calibrated") is True
            and recorded.get("exit_code") == exit_code and recorded.get("failed_checks") == failed
            and len(recorded.get("result", {}).get("checks", [])) == checks)


def run_task(task, args):
    """Grade every control of one task against its prepared fixture; return the report."""
    definition, common = task_definition(args.suite_root, task)
    fixture = args.data / "fixtures" / task["id"]
    ready = json.loads((fixture / "ready.json").read_text())
    if ready["task_id"] != task["id"] or ready["base_commit"] != task["base_commit"]:
        raise ValueError(f"Prepared fixture {fixture} does not match the catalog task")
    root = args.data / OUTPUT / task["id"]
    root.mkdir(parents=True)
    clone_without_build_cache(fixture / "work", root / "base")
    clone_without_build_cache(fixture / "work", root / "reference")
    changed = apply_reference(args.source_root / task["repository"], task, root / "reference")
    recorded = json.loads((args.suite_root / task["grader"]["preflight_evidence"]).read_text())
    recorded_runs = {run["fixture"]: run for run in recorded["runs"]}
    environment = {**runner.clean_env(), "PYTHONDONTWRITEBYTECODE": "1", **grader_environment(args.data),
                   **definition.grader_env}
    report = {"schema_version": SCHEMA_VERSION, "suite": args.suite_id, "task_id": task["id"],
              "base_commit": task["base_commit"], "reference_fix_commit": task["reference_fix_commit"],
              "fixture_ready_sha256": runner.digest(fixture / "ready.json"),
              "recorded_calibration_sha256": runner.digest(args.suite_root / task["grader"]["preflight_evidence"]),
              "reference_changed_files": changed, "grader_environment": grader_environment(args.data),
              "expected_check_count": definition.expected_check_count, "runs": [], "status": "incomplete"}
    for control in definition.controls:
        directory, patch = common.prepare(root, control)
        output = root / f"{control.name}-result.json"
        started = time.monotonic()
        try:
            completed = subprocess.run([sys.executable, str(definition.grader), "--repo", str(directory),
                                        "--output", str(output)], env=environment, text=True,
                                       capture_output=True, timeout=definition.grader_timeout_seconds)
            exit_code, log = completed.returncode, completed.stdout + completed.stderr
        except subprocess.TimeoutExpired as error:
            exit_code, log = None, f"grader timed out after {definition.grader_timeout_seconds}s: {error}"
        (root / f"{control.name}.log").write_text(log)
        try:
            result = json.loads(output.read_text())
        except (OSError, json.JSONDecodeError) as error:
            result = {"status": "infra", "checks": [], "error": f"grader wrote no result: {error}"}
        calibrated, reasons = common.evaluate(control, definition, exit_code, result)
        failed = sorted(check["name"] for check in result.get("checks", []) if not check.get("passed"))
        matched = matches_recorded(recorded_runs.get(control.name), exit_code, failed,
                                   len(result.get("checks", [])), calibrated)
        report["runs"].append({"fixture": control.name, "kind": control.kind, "expect": control.expect,
                               "exit_code": exit_code, "failed_checks": failed, "calibrated": calibrated,
                               "reasons": reasons, "matches_recorded": matched, "edit_diff": patch,
                               "seconds": round(time.monotonic() - started, 3), "result": result})
        print(f"GRADER {task['id']} {control.name} exit={exit_code} calibrated={calibrated} matches={matched}",
              flush=True)
    complete = [run["fixture"] for run in report["runs"]] == [run["fixture"] for run in recorded["runs"]]
    report["status"] = "pass" if complete and all(run["matches_recorded"] for run in report["runs"]) else "fail"
    runner.write_json(root / "report.json", report)
    return report


def validate_proof(data, suite_root, suite_id, tasks):
    """Require a passing grader preflight for exactly these fixtures, graders and settings."""
    if not (data / PROOF).is_file():
        raise ValueError(f"Grader preflight is absent; run preflight_real_repo_graders.py for {data}")
    proof = json.loads((data / PROOF).read_text())
    fixtures = {task["id"]: runner.digest(data / "fixtures" / task["id"] / "ready.json") for task in tasks}
    if (proof.get("schema_version") != SCHEMA_VERSION or proof.get("status") != "pass"
            or proof.get("suite") != suite_id or proof.get("driver_sha256") != runner.digest(Path(__file__))
            or proof.get("grading_files") != grading_digests(suite_root) or proof.get("fixtures") != fixtures
            or proof.get("grader_environment") != grader_environment(data) or set(proof.get("tasks", {})) != set(fixtures)):
        raise ValueError("Grader preflight is absent, failed, or differs from the prepared fixtures and graders")
    for task_id, summary in proof["tasks"].items():
        report = data / OUTPUT / task_id / "report.json"
        if runner.digest(report) != summary.get("report_sha256") or summary.get("matched") != summary.get("controls"):
            raise ValueError("Grader preflight report differs from its proof")
        if json.loads(report.read_text()).get("status") != "pass":
            raise ValueError("Grader preflight report did not pass")
    return proof


def run_preflight(args, tasks):
    if (args.data / "manifest.json").exists() or (args.data / "trials").exists():
        raise ValueError("Grader preflight must precede the frozen manifest and every attempt")
    if (args.data / OUTPUT).exists() or (args.data / PROOF).exists():
        raise ValueError("Preserve prior grader preflight evidence: move grader-preflight/ and "
                         "grader-preflight.json aside before rerunning")
    for task in tasks:
        if not (args.data / "fixtures" / task["id"] / "ready.json").is_file():
            raise ValueError(f"Fixture {task['id']} is not prepared; run the collector with --prepare-only first")
    started = time.monotonic()
    reports = [run_task(task, args) for task in tasks]
    toolchain = {"python": sys.version.split()[0]}
    if any(task["repository"] == "agi-backend-rust" for task in tasks):
        toolchain["rustc"] = runner.command(["rustc", "--version"]).strip()
    proof = {"schema_version": SCHEMA_VERSION, "status": "pass" if all(r["status"] == "pass" for r in reports) else "fail",
             "suite": args.suite_id, "driver_sha256": runner.digest(Path(__file__)),
             "grading_files": grading_digests(args.suite_root),
             "fixtures": {task["id"]: runner.digest(args.data / "fixtures" / task["id"] / "ready.json") for task in tasks},
             "grader_environment": grader_environment(args.data), "toolchain": toolchain,
             "tasks": {report["task_id"]: {"controls": len(report["runs"]),
                                           "matched": sum(run["matches_recorded"] for run in report["runs"]),
                                           "base_failed_checks": next(run["failed_checks"] for run in report["runs"]
                                                                      if run["fixture"] == "base"),
                                           "report_sha256": runner.digest(args.data / OUTPUT / report["task_id"] / "report.json")}
                       for report in reports},
             "seconds": round(time.monotonic() - started, 3),
             "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")}
    runner.write_json(args.data / PROOF, proof)
    return proof


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalog", required=True, type=Path, help="suite catalog.json")
    parser.add_argument("--data", required=True, type=Path, help="collection data root with prepared fixtures")
    parser.add_argument("--source-root", type=Path, default=runner.SOURCE_ROOT,
                        help="directory holding the operator repositories (read only)")
    args = parser.parse_args(argv)
    if args.data.is_symlink() or args.data.resolve().parent != Path("/private/tmp") or not args.data.name.startswith("erp26-"):
        parser.error("--data must be a non-symlink /private/tmp/erp26-* collection root")
    args.data, args.source_root = args.data.resolve(), args.source_root.resolve()
    catalog_path = args.catalog.resolve()
    catalog = json.loads(catalog_path.read_text())
    args.suite_root, args.suite_id = catalog_path.parent, catalog["suite_id"]
    proof = run_preflight(args, catalog["tasks"])
    print(json.dumps({"status": proof["status"], "proof": str(args.data / PROOF),
                      "tasks": {task: f'{summary["matched"]}/{summary["controls"]}' for task, summary in proof["tasks"].items()},
                      "seconds": proof["seconds"]}))
    return 0 if proof["status"] == "pass" else 1


if __name__ == "__main__":
    sys.exit(main())
