#!/usr/bin/env python3
"""Preflight v2 r06 controls in fresh exports; retain raw source and logs privately."""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

BASE = "4a2668b9bb198ae7d9108f2cebefb84fa9244b24"
REFERENCE = "60cfcd4eda9d776f6cdac8af88f853c9276cb30e"
CONTROLS = {
    "base": False,
    "reference_503": True,
    "reference_504": True,
    "alternative_without_abort": True,
    "bad_no_deadline": False,
    "bad_headers_only_deadline": False,
    "bad_welcome_loop": False,
    "bad_timeout_status": False,
}
EXPECTED_CHECK_COUNT = 31
CHECK_PREFIX = "private onboarding behavior "
DEADLINE_CHECKS = {
    phase: {
        "profile": CHECK_PREFIX + f"stalled {phase}: profile action settles with a failure within the total deadline",
        "gate": CHECK_PREFIX + f"stalled {phase}: gate settles closed and preserves the CLI destination within the total deadline",
        "welcome": CHECK_PREFIX + f"stalled {phase}: welcome settles without a redirect loop within the total deadline",
    }
    for phase in ("headers", "body")
}
OUTAGES = ("HTTP 500", "unreachable backend", "non-JSON success")
OUTAGE_WELCOME_CHECKS = {
    CHECK_PREFIX + f"{outage}: welcome remains usable without a redirect" for outage in OUTAGES
}
EXPECTED_FAILURES = {
    "base": (
        {CHECK_PREFIX + f"{outage}: blocks the gate and preserves an invite" for outage in OUTAGES}
        | OUTAGE_WELCOME_CHECKS
        | {CHECK_PREFIX + f"gate discards unsafe return path {json.dumps(path)}" for path in (
            "https://evil.invalid/path", "//evil.invalid/path", "/\t/evil.invalid/path",
            "/welcome?next=/me", "/https://evil.invalid/path",
        )}
        | {name for phase in DEADLINE_CHECKS.values() for name in phase.values()}
    ),
    "reference_503": set(),
    "reference_504": set(),
    "alternative_without_abort": set(),
    "bad_no_deadline": {name for phase in DEADLINE_CHECKS.values() for name in phase.values()},
    "bad_headers_only_deadline": set(DEADLINE_CHECKS["body"].values()),
    "bad_welcome_loop": OUTAGE_WELCOME_CHECKS | {
        CHECK_PREFIX + "unauthenticated checks do not contact the backend or loop welcome",
        DEADLINE_CHECKS["headers"]["welcome"], DEADLINE_CHECKS["body"]["welcome"],
    },
    "bad_timeout_status": {DEADLINE_CHECKS["headers"]["profile"]},
}


def calibration_errors(name: str, result: dict, exit_code: int) -> list[str]:
    """A mutant must execute the full suite and fail its intended behavior."""
    errors = []
    checks = result.get("checks", [])
    expected_status = "pass" if CONTROLS[name] else "fail"
    if result.get("status") != expected_status or exit_code != int(not CONTROLS[name]):
        errors.append("Control status or exit code differs from the expected outcome")
    if len(checks) != EXPECTED_CHECK_COUNT or result.get("total_checks") != EXPECTED_CHECK_COUNT:
        errors.append(f"Control did not collect all {EXPECTED_CHECK_COUNT} behavioral checks")
    if len({check["name"] for check in checks}) != EXPECTED_CHECK_COUNT:
        errors.append("Behavioral check names are missing or duplicated")
    failed = {check["name"] for check in checks if check["status"] != "pass"}
    if failed != EXPECTED_FAILURES[name]:
        errors.append("Named failed checks differ from the exact expected failure set")
    return errors


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def replace_once(text: str, before: str, after: str) -> str:
    if text.count(before) != 1:
        raise ValueError(f"Expected one mutation target, found {text.count(before)}")
    return text.replace(before, after)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-repo", type=Path, required=True)
    parser.add_argument("--node-modules", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--private-parent", type=Path, default=Path("/tmp"))
    args = parser.parse_args()
    workspace = Path(tempfile.mkdtemp(prefix="engram-r06-v2-preflight-", dir=args.private_parent))
    here = Path(__file__).resolve().parent
    for name in CONTROLS:
        repo = workspace / name
        repo.mkdir()
        archive = workspace / (name + ".tar")
        with archive.open("wb") as stream:
            subprocess.run(["git", "-C", str(args.source_repo), "archive", BASE if name == "base" else REFERENCE], stdout=stream, check=True)
        with tarfile.open(archive) as exported:
            exported.extractall(repo, filter="data")
        archive.unlink()
        backend = repo / "features/onboarding/actions/_call-backend.ts"
        source = backend.read_text()
        if name in {"reference_504", "bad_timeout_status"}:
            status = 504 if name == "reference_504" else 599
            source = replace_once(source, "      status: 503,", f'      status: error instanceof Error && ["TimeoutError", "AbortError"].includes(error.name) ? {status} : 503,')
        elif name in {"bad_no_deadline", "alternative_without_abort"}:
            source = replace_once(source, "      signal: AbortSignal.timeout(BACKEND_TIMEOUT_MS),\n", "")
            if name == "alternative_without_abort":
                source = replace_once(source, "export async function callBackend<T>(", "async function callBackendWork<T>(")
                source += '''
// Valid alternative: race the complete operation, without cancelling fetch.
export async function callBackend<T>(actionName: string, req: BackendRequest): Promise<ActionResult<T>> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      callBackendWork<T>(actionName, req),
      new Promise<ActionResult<T>>((resolve) => {
        timer = setTimeout(() => resolve({ ok: false, data: null, error: { status: 504, message: "Backend deadline exceeded" } }), BACKEND_TIMEOUT_MS);
      }),
    ]);
  } finally { if (timer) clearTimeout(timer); }
}
'''
        elif name == "bad_headers_only_deadline":
            source = replace_once(source, "  let response: Response;", "  const controller = new AbortController();\n  const headerTimer = setTimeout(() => controller.abort(), BACKEND_TIMEOUT_MS);\n  let response: Response;")
            source = replace_once(source, "signal: AbortSignal.timeout(BACKEND_TIMEOUT_MS)", "signal: controller.signal")
            source = replace_once(source, "    });\n  } catch (error) {", "    });\n    clearTimeout(headerTimer);\n  } catch (error) {")
        elif name == "bad_welcome_loop":
            gate = repo / "features/onboarding/actions/enforce-onboarding-gate.ts"
            text = gate.read_text()
            at = text.index("export async function redirectIfOnboardingComplete")
            text = text[:at] + replace_once(text[at:], "  const result = await getCurrentUser();", '  const result = await getCurrentUser();\n  if (!result.ok) redirect("/me");')
            gate.write_text(text)
        backend.write_text(source)

    def run(name: str) -> dict:
        output = workspace / (name + ".json")
        process = subprocess.run([sys.executable, str(here / "grade.py"), "--repo", str(workspace / name), "--output", str(output), "--node-modules", str(args.node_modules)], capture_output=True, text=True)
        result = json.loads(output.read_text())
        errors = calibration_errors(name, result, process.returncode)
        print(f"{name}: {result['status']} {result.get('passed_checks')}/{result.get('total_checks')}", flush=True)
        return {
            "name": name, "expected_pass": CONTROLS[name], "status": result["status"],
            "matches_expectation": not errors,
            "expected_failed_checks": sorted(EXPECTED_FAILURES[name]),
            "calibration_errors": errors,
            "passed_checks": result.get("passed_checks"), "total_checks": result.get("total_checks"),
            "exit_code": process.returncode, "duration_seconds": result["duration_seconds"],
            "failed_checks": [check["name"] for check in result["checks"] if check["status"] != "pass"],
            "deadline_checks": [{"name": check["name"], "status": check["status"]} for check in result["checks"] if "stalled " in check["name"]],
            "sha256": result.get("sha256"), "node_version": result.get("node_version"), "dependency_versions": result.get("dependency_versions"),
            "private_report": str(output),
            "control_source_sha256": {str(path.relative_to(workspace / name)): sha(path) for path in (workspace / name / "features/onboarding/actions").glob("*.ts")},
        }

    with ThreadPoolExecutor(max_workers=3) as pool:
        results = list(pool.map(run, CONTROLS))
    report = {
        "grading_version": 2, "task_id": "r06_onboarding_outage", "base_commit": BASE, "reference_commit": REFERENCE,
        "private_workspace": str(workspace), "all_expectations_met": all(item["matches_expectation"] for item in results),
        "sha256": {path.name: sha(path) for path in [here / "grade.py", here / "onboarding.private.test.ts", here / "preflight.py", here.parent.parent / "prompts/r06_onboarding_outage.md"]},
        "controls": results,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"report": str(args.report), "private_workspace": str(workspace), "all_expectations_met": report["all_expectations_met"]}))
    return 0 if report["all_expectations_met"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
