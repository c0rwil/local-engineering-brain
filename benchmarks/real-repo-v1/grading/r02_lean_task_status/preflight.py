#!/usr/bin/env python3
"""Reproduce grader calibration from immutable historical Git objects."""

import argparse
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

BASE = "1b5c21e44eda8e68dbbe457f7e98e7a607ff3b1a"
REFERENCE = "5b0c08504f191c7f9b96893764c120cb0f85efd0"
PATHS = [
    "src/agency/api/services/mcp_tool_handlers.py",
    "src/agency/api/services/mcp_session_service.py",
    "scripts/mcp_user_control_integration_test.py",
    "contract_test_fixtures",
]
EXPECTED_FAILURES = {
    "base": ["default_uses_count_without_loading_history"],
    "reference": [],
    "lean-payload-only": ["default_uses_count_without_loading_history"],
    "serializer-drops-field": ["runtime_step_serializer_preserves_all_wire_fields"],
    "history-and-serializer-erase-payload": ["runtime_step_serializer_preserves_all_wire_fields", "explicit_true_returns_full_history"],
}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-repo", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="engram-python-preflight-", dir="/tmp"))
    grader = Path(__file__).with_name("grade.py").resolve()
    for tag, revision in (("base", BASE), ("reference", REFERENCE)):
        destination = root / tag
        destination.mkdir()
        archive = subprocess.check_output(["git", "-C", str(args.source_repo), "archive", revision, *PATHS])
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            tar.extractall(destination, filter="data")
    mutation = root / "lean-payload-only"
    shutil.copytree(root / "reference", mutation)
    handler = mutation / PATHS[0]
    source = handler.read_text()
    original = "steps = await mcp_svc.list_steps(user_id, task_id) if want_steps else []"
    assert source.count(original) == 1, "Reference mutation anchor changed"
    handler.write_text(source.replace(original, "steps = await mcp_svc.list_steps(user_id, task_id)"))
    for tag in ("serializer-drops-field", "history-and-serializer-erase-payload"):
        mutation = root / tag
        shutil.copytree(root / "reference", mutation)
        service = mutation / PATHS[1]
        source = service.read_text()
        original = "        return json.dumps(asdict(self))"
        assert source.count(original) == 1, "Serializer mutation anchor changed"
        if tag == "serializer-drops-field":
            replacement = '        payload = asdict(self)\n        payload.pop("thinking")\n        return json.dumps(payload)'
        else:
            replacement = '        payload = asdict(self)\n        payload["thinking"] = None\n        payload["screenshot_thumbnail"] = None\n        return json.dumps(payload)'
            handler = mutation / PATHS[0]
            handler_source = handler.read_text()
            handler_source = handler_source.replace('"screenshot_thumbnail": s.screenshot_thumbnail,', '"screenshot_thumbnail": None,')
            handler_source = handler_source.replace('"thinking": s.thinking,', '"thinking": None,')
            handler.write_text(handler_source)
        service.write_text(source.replace(original, replacement))
    report = {
        "task_id": "r02_lean_task_status",
        "base_commit": BASE,
        "reference_fix_commit": REFERENCE,
        "python": sys.version,
        "grader_sha256": hashlib.sha256(grader.read_bytes()).hexdigest(),
        "fixture_manifest_sha256": hashlib.sha256(grader.with_name("v1_sha256.json").read_bytes()).hexdigest(),
        "exported_paths": PATHS,
        "fixture_root": str(root),
        "runs": [],
    }
    for tag, expected in (("base", 1), ("reference", 0), ("lean-payload-only", 1), ("serializer-drops-field", 1), ("history-and-serializer-erase-payload", 1)):
        result_path = root / f"{tag}-result.json"
        run = subprocess.run(
            [sys.executable, "-B", str(grader), "--repo", str(root / tag), "--output", str(result_path)],
            cwd=root / tag,
            env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "PYTHONDONTWRITEBYTECODE": "1"},
            capture_output=True, text=True, timeout=60,
        )
        (root / f"{tag}.log").write_text(run.stdout + run.stderr)
        report["runs"].append({
            "fixture": tag,
            "expected_exit_code": expected,
            "expected_failure_checks": EXPECTED_FAILURES[tag],
            "exit_code": run.returncode,
            "result": json.loads(result_path.read_text()),
        })
    report["status"] = "pass" if all(
        r["exit_code"] == r["expected_exit_code"]
        and set(r["expected_failure_checks"]).issubset({c["name"] for c in r["result"]["checks"] if c["status"] == "fail"})
        for r in report["runs"]
    ) else "fail"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "output": str(args.output), "fixture_root": str(root)}))
    return 0 if report["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
