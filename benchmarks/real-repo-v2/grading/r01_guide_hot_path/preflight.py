#!/usr/bin/env python3
"""Calibrate the v2 Rust graders using disposable historical source exports."""

import argparse
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


TASKS = {
    "r01_guide_hot_path": (
        "bbce775ef3252e4ad882ac43e6564263302f8076",
        "ff515c8a0b8fedc4757209ce3291ee61d0f5aa7d",
    ),
    "r05_test_env_isolation": (
        "cae95f3086d7320551ab02ba4ba0fc105dc13342",
        "30cc0acfcb7766e8ff0de47119d072b6f46543a8",
    ),
}


def replace_once(path, before, after):
    source = path.read_text()
    if source.count(before) != 1:
        raise RuntimeError(f"Mutation anchor must occur exactly once: {path}: {before!r}")
    path.write_text(source.replace(before, after))


def prepare_controls(root, task):
    if task == "r05_test_env_isolation":
        controls = {"base": ["test_mode_does_not_restore_scrubbed_dotenv_values"],
                    "reference": [], "remove-dotenv-globally": ["development_still_loads_dotenv"]}
        shutil.copytree(root / "reference", root / "remove-dotenv-globally")
        replace_once(root / "remove-dotenv-globally/src/config.rs",
                     "            let _ = dotenvy::dotenv();",
                     "            // Deliberately broken: skip dotenv in every environment.")
        return controls
    controls = {
        "base": ["task_opt_out_skips_search"],
        "reference": [],
        "request-mutator-only": [],
        "resolver-only": [],
        "discard-text-after-retrieval": ["task_opt_out_skips_search", "disabled_task_skips_later_cache_pickup"],
        "ignore-task-policy": ["policy_restricts_task_opt_in"],
    }
    for tag in list(controls)[2:]:
        shutil.copytree(root / "reference", root / tag)
    resolver_term = "            && task_guides_allowed(partner_context)\n"
    policy_merge = """    request.use_task_guides =
        Some(request.use_task_guides.unwrap_or(true) && policy.task_guides_enabled);
"""
    # Both implementations honor the same production context. Neither should be
    # rejected for choosing a different layer to enforce task-family policy.
    replace_once(root / "request-mutator-only/src/services/guides.rs", resolver_term, "")
    replace_once(root / "resolver-only/src/services/partner_runtime.rs", policy_merge, "")
    replace_once(root / "ignore-task-policy/src/services/partner_runtime.rs", policy_merge, "")
    replace_once(root / "ignore-task-policy/src/services/guides.rs", resolver_term, "")
    broken = root / "discard-text-after-retrieval/src/services/guides.rs"
    replace_once(broken,
                 "let task_guides_allowed = request.use_task_guides.unwrap_or(true)\n            && task_guides_allowed(partner_context)",
                 "let task_guides_allowed = task_guides_allowed(partner_context)")
    replace_once(broken, "        ResolvedGuides {\n            task_guide,",
                 "        ResolvedGuides {\n            task_guide: if request.use_task_guides == Some(false) { None } else { task_guide },")
    return controls


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-repo", required=True, type=Path)
    parser.add_argument("--task", choices=TASKS, default="r01_guide_hot_path")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    base, reference = TASKS[args.task]
    root = Path(tempfile.mkdtemp(prefix="engram-v2-rust-preflight-", dir="/private/tmp"))
    grader = Path(__file__).resolve().parent.parent / args.task / "grade.py"
    for tag, revision in (("base", base), ("reference", reference)):
        destination = root / tag
        destination.mkdir()
        archive = subprocess.check_output(["git", "-C", str(args.source_repo), "archive", revision])
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            tar.extractall(destination, filter="data")
    controls = prepare_controls(root, args.task)
    # Save mutations before grade.py appends its private tests to each source.
    mutations = {}
    paths = ("src/services/guides.rs", "src/services/partner_runtime.rs", "src/config.rs")
    for tag in list(controls)[2:]:
        mutations[tag] = "".join(
            "".join(difflib.unified_diff((root / "reference" / p).read_text().splitlines(True),
                                         (root / tag / p).read_text().splitlines(True),
                                         fromfile=f"reference/{p}", tofile=f"{tag}/{p}"))
            for p in paths
        )
    report = {
        "suite_id": "real-repo-v2", "task_id": args.task,
        "base_commit": base, "reference_fix_commit": reference,
        "python": sys.version, "fixture_root": str(root),
        "grader_sha256": hashlib.sha256(grader.read_bytes()).hexdigest(),
        "private_tests_sha256": hashlib.sha256(grader.with_name("hidden_tests.rs").read_bytes()).hexdigest(),
        "preflight_driver_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "mutations": mutations, "runs": [],
    }
    for tag, required_failures in controls.items():
        result_path = root / f"{tag}-result.json"
        expected = 1 if required_failures else 0
        print(f"Calibrating {args.task}: {tag}", flush=True)
        run = subprocess.run([sys.executable, "-B", str(grader), "--repo", str(root / tag),
                              "--output", str(result_path)],
                             text=True, capture_output=True, timeout=2700,
                             env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
        (root / f"{tag}.log").write_text(run.stdout + run.stderr)
        result = json.loads(result_path.read_text())
        failures = {c["name"] for c in result["checks"] if not c["passed"]}
        expected_check_count = 15 if args.task == "r01_guide_hot_path" else 6
        calibrated = (run.returncode == expected
                      and result["status"] == ("fail" if expected else "pass")
                      and len(result["checks"]) == expected_check_count
                      and result["checks"][0] == {"name": "compiles_with_behavioral_tests", "passed": True}
                      and set(required_failures).issubset(failures))
        report["runs"].append({"fixture": tag, "expected_exit_code": expected,
                               "expected_failure_checks": required_failures,
                               "expected_check_count": expected_check_count,
                               "exit_code": run.returncode, "calibrated": calibrated,
                               "result": result})
        report["status"] = "pass" if all(r["calibrated"] for r in report["runs"]) and len(report["runs"]) == len(controls) else "incomplete"
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
        if not calibrated:
            break
    report["status"] = "pass" if all(r["calibrated"] for r in report["runs"]) and len(report["runs"]) == len(controls) else "fail"
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "output": str(args.output), "fixture_root": str(root)}))
    return 0 if report["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
