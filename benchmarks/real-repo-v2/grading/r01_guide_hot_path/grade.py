#!/usr/bin/env python3
"""Private behavioral grader. Run only against an expendable grading copy."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import time

TASK = "r01_guide_hot_path"
SOURCE = "src/services/guides.rs"
TARGET = ["--lib"]
MODULE = "services::guides::engram_benchmark_guide_family"
CASES = [
    "absent_preserves_both", "task_opt_out_skips_search", "app_opt_out_preserves_task",
    "master_overrides_family_opt_in", "both_families_off",
    "session_conversion_preserves_task_opt_out", "session_conversion_preserves_app_opt_out",
    "policy_restricts_task_opt_in", "policy_cannot_override_caller_opt_out",
    "policy_master_disables_both",
    "disabled_task_does_not_schedule_app_open_warm", "enabled_app_open_still_schedules_warm",
    "disabled_task_skips_later_cache_pickup", "enabled_task_preserves_later_cache_pickup",
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    repo, output = args.repo.resolve(), args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    logdir = output.with_suffix(".logs")
    logdir.mkdir(exist_ok=True)
    result = {"task": TASK, "status": "infra", "checks": [], "logs": str(logdir)}
    started = time.monotonic()
    try:
        if repo == Path("/Users/corwil/PycharmProjects/agi-backend-rust").resolve():
            raise RuntimeError("Refusing to modify the real repository; supply a disposable grading copy")
        env = {key: os.environ[key] for key in ("PATH", "HOME", "TMPDIR", "CARGO_HOME", "RUSTUP_HOME") if key in os.environ}
        env.update({"ENVIRONMENT": "test", "AGENT_DATA_SOURCE": "api", "USE_REDIS_SESSIONS": "false",
                    "REQUIRE_REDIS_WHEN_ENABLED": "false", "SENTRY_ENABLED": "false", "BILLING_ENABLED": "false",
                    "AWS_EC2_METADATA_DISABLED": "true", "CARGO_BUILD_JOBS": "2", "CARGO_NET_OFFLINE": "true",
                    "CARGO_TARGET_DIR": os.environ.get("ENGRAM_RUST_GRADER_TARGET", "/tmp/engram-rust-preflight-target")})
        # Cargo identifies path dependencies by package identity and mtimes.
        # Historic git archives retain old timestamps: refresh the vendor source
        # and serialize reuse so another candidate cannot replace the executable
        # while this grader is running its assertions.
        target_dir = Path(env["CARGO_TARGET_DIR"])
        target_dir.mkdir(parents=True, exist_ok=True)
        build_lock = (target_dir / ".engram-grader.lock").open("a")
        fcntl.flock(build_lock, fcntl.LOCK_EX)
        for vendor_source in (repo / "vendor").rglob("*.rs"):
            vendor_source.touch()
        source = repo / SOURCE
        marker = "\n// ENGRAM_PRIVATE_GRADER_START\n"
        original = source.read_text().split(marker)[0]
        fixture = Path(__file__).with_name("hidden_tests.rs").read_text()
        source.write_text(original + marker + fixture)
        (repo / ".env").write_text("AGENT_DATA_SOURCE=api\nUSE_APP_GUIDES=false\nAGENT_API_URL=http://127.0.0.1:9/benchmark-dotenv\n")
        command = ["cargo", "test", "--offline", "--locked", *TARGET, "--no-run", "--message-format=json"]
        build = subprocess.run(command, cwd=repo, env=env, text=True, capture_output=True, timeout=1800)
        (logdir / "build.stdout.jsonl").write_text(build.stdout)
        (logdir / "build.stderr.log").write_text(build.stderr)
        result["compiler"] = subprocess.check_output(["rustc", "--version"], env=env, text=True).strip()
        result["build_exit_code"] = build.returncode
        artifacts = []
        diagnostics = []
        for line in build.stdout.splitlines():
            try:
                item = json.loads(line)
            except json.JSONDecodeError:
                continue
            if item.get("reason") == "compiler-artifact" and item.get("executable") and item.get("profile", {}).get("test"):
                artifacts.append(item["executable"])
            if item.get("reason") == "compiler-message" and item.get("message", {}).get("level") == "error":
                diagnostics.append(item["message"]["message"])
        if build.returncode:
            result["status"] = "fail" if diagnostics else "infra"
            result["checks"].append({"name": "compiles_with_behavioral_tests", "passed": False, "errors": diagnostics})
        elif len(artifacts) != 1:
            raise RuntimeError(f"Expected one test executable, got {len(artifacts)}")
        else:
            result["checks"].append({"name": "compiles_with_behavioral_tests", "passed": True})
            for name in CASES:
                run = subprocess.run([artifacts[0], f"{MODULE}::{name}", "--exact", "--nocapture", "--test-threads=1"],
                                     cwd=repo, env=env, text=True, capture_output=True, timeout=60)
                (logdir / f"{name}.log").write_text(run.stdout + run.stderr)
                passed = run.returncode == 0 and "1 passed" in run.stdout
                result["checks"].append({"name": name, "passed": passed, "exit_code": run.returncode})
            result["status"] = "pass" if all(check["passed"] for check in result["checks"]) else "fail"
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        result["error"] = str(error)
    result["elapsed_seconds"] = round(time.monotonic() - started, 3)
    output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"status": result["status"], "output": str(output)}))
    return {"pass": 0, "fail": 1, "infra": 2}[result["status"]]


if __name__ == "__main__":
    sys.exit(main())
