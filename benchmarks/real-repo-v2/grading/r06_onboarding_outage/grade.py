#!/usr/bin/env python3
"""Behaviorally grade r06 in a disposable platform checkout. No install/network step."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--node-modules", type=Path, default=os.environ.get("ENGRAM_PLATFORM_NODE_MODULES"))
    args = parser.parse_args()
    repo, output = args.repo.resolve(), args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    report = {"task_id": "r06_onboarding_outage", "grading_version": 2, "status": "infrastructure_error", "checks": []}
    started = time.monotonic()
    temporary = None
    linked_modules = False
    exit_code = 2
    log = output.with_suffix(".log")
    raw_report = output.with_suffix(".vitest.json")
    try:
        if repo == Path("/Users/corwil/PycharmProjects/platform"):
            raise ValueError("Pass a disposable source export, never the original platform repository")
        if not (repo / "features/onboarding/actions/get-current-user.ts").is_file():
            raise ValueError("--repo is not a platform source export")
        node = shutil.which("node")
        if not node:
            raise ValueError("Node is unavailable; provision dependencies outside the measured run")
        if not (repo / "node_modules").exists() and args.node_modules:
            modules = Path(args.node_modules).resolve()
            if not (modules / "vitest/vitest.mjs").is_file():
                raise ValueError("--node-modules does not contain prepared Vitest dependencies")
            (repo / "node_modules").symlink_to(modules, target_is_directory=True)
            linked_modules = True
        vitest = repo / "node_modules/vitest/vitest.mjs"
        if not vitest.is_file():
            raise ValueError("Missing prepared node_modules; set ENGRAM_PLATFORM_NODE_MODULES or --node-modules")
        temporary = Path(tempfile.mkdtemp(prefix=".engram-private-r06-", dir=repo))
        shutil.copyfile(Path(__file__).with_name("onboarding.private.test.ts"), temporary / "onboarding.private.test.ts")
        config = {
            "root": str(repo),
            "cacheDir": str(temporary / "vite-cache"),
            "resolve": {"alias": {"@": str(repo), "server": str(repo / "server")}},
            "test": {
                "environment": "node", "include": [str(temporary / "onboarding.private.test.ts")],
                "globals": False, "watch": False, "maxWorkers": 1, "fileParallelism": False,
                "env": {"NEXT_PUBLIC_AGI_API_URL": "http://127.0.0.1:18080"},
                "testTimeout": 7000,
            },
        }
        config_path = temporary / "vitest.config.mjs"
        config_path.write_text("export default " + json.dumps(config) + ";\n")
        env = {key: value for key, value in os.environ.items() if key in {"PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "SYSTEMROOT"}}
        env.update({"CI": "1", "NODE_ENV": "test", "NEXT_TELEMETRY_DISABLED": "1", "NO_COLOR": "1"})
        command = [node, str(vitest), "run", "--config", str(config_path), "--reporter=json", "--outputFile=" + str(raw_report)]
        with log.open("w") as stream:
            process = subprocess.Popen(command, cwd=repo, env=env, stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                process.wait(timeout=45)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                raise
        report["command_exit_code"] = process.returncode
        report["node_version"] = subprocess.check_output([node, "--version"], text=True).strip()
        report["dependency_versions"] = {
            name: json.loads((repo / "node_modules" / name / "package.json").read_text())["version"]
            for name in ("vitest", "msw")
        }
        report["sha256"] = {
            "grader": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            "private_tests": hashlib.sha256(Path(__file__).with_name("onboarding.private.test.ts").read_bytes()).hexdigest(),
            "repository_lockfile": hashlib.sha256((repo / "pnpm-lock.yaml").read_bytes()).hexdigest(),
            "prepared_runtime_lockfile": hashlib.sha256((repo / "node_modules/.pnpm/lock.yaml").read_bytes()).hexdigest(),
        }
        if not raw_report.is_file():
            raise ValueError("Vitest produced no structured test report; see the log")
        raw = json.loads(raw_report.read_text())
        checks = [
            {"name": check["fullName"], "status": "pass" if check["status"] == "passed" else "fail",
             "duration_ms": check.get("duration"), "failures": check.get("failureMessages", [])}
            for suite in raw.get("testResults", []) for check in suite.get("assertionResults", [])
        ]
        report["checks"] = checks
        if not checks:
            # The toolchain was provisioned before execution. A candidate whose
            # source cannot be loaded fails effectiveness rather than vanishing
            # from the result denominator as an infrastructure exclusion.
            report["checks"] = [{
                "name": "candidate source loads in the prepared Node test environment",
                "status": "fail", "failures": [suite.get("message", "source collection failed") for suite in raw.get("testResults", [])],
            }]
            report["status"] = "fail"
            report["passed_checks"] = 0
            report["total_checks"] = 1
            exit_code = 1
            return exit_code
        expected = 31
        if len(checks) != expected:
            raise ValueError(f"Expected {expected} private checks, collected {len(checks)}")
        report["status"] = "pass" if process.returncode == 0 and all(c["status"] == "pass" for c in checks) else "fail"
        report["passed_checks"] = sum(c["status"] == "pass" for c in checks)
        report["total_checks"] = len(checks)
        exit_code = 0 if report["status"] == "pass" else 1
    except subprocess.TimeoutExpired:
        report["status"] = "fail"
        report["checks"] = [{"name": "candidate completes private tests within the 45-second grading limit", "status": "fail"}]
        report["passed_checks"] = 0
        report["total_checks"] = 1
        exit_code = 1
    except (OSError, ValueError, subprocess.SubprocessError, KeyError) as error:
        report["infrastructure_error"] = str(error)
    finally:
        if temporary:
            shutil.rmtree(temporary)
        if linked_modules:
            (repo / "node_modules").unlink()
        report.update({"duration_seconds": round(time.monotonic() - started, 3), "log": str(log), "vitest_report": str(raw_report)})
        output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "output": str(output), "checks": len(report["checks"])}))
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
