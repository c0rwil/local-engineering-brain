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

from grade import contract_shape

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
    "additive-status-fields": [],
    "reformatted-fixtures": [],
    "additive-device-fields": [],
    "removed-existing-field": ["v1_fixture_existing_json_meaning_preserved"],
    "changed-existing-value": ["v1_fixture_existing_json_meaning_preserved"],
    "changed-existing-scalar-type": ["v1_fixture_existing_json_meaning_preserved"],
    "changed-existing-array": ["v1_fixture_existing_json_meaning_preserved"],
    "changed-existing-array-order": ["v1_fixture_existing_json_meaning_preserved"],
    "cursor-false-disabled": ["false_cursor_converts_to_zero_and_implies_history", "false_string_cursor_is_invalid_not_disabled"],
    "cursor-strict-types": ["false_cursor_converts_to_zero_and_implies_history", "true_cursor_converts_to_one", "fractional_cursor_truncates_toward_zero"],
    "cursor-rounded": ["fractional_cursor_truncates_toward_zero"],
    "consumer-missing-history-opt-in": ["user_control_consumer_explicitly_fetches_needed_steps"],
    "consumer-string-true-opt-in": [],
    "schema-type-arrays": [],
    "schema-anyof-oneof": [],
    "schema-local-references": [],
    "schema-missing-flag": ["tool_schema_exposes_opt_in_and_cursor"],
    "schema-missing-cursor": ["tool_schema_exposes_opt_in_and_cursor"],
    "schema-wrong-flag-type": ["tool_schema_exposes_opt_in_and_cursor"],
    "schema-wrong-cursor-type": ["tool_schema_exposes_opt_in_and_cursor"],
    "schema-missing-required-task": ["tool_schema_exposes_opt_in_and_cursor"],
    "schema-cyclic-reference": ["tool_schema_exposes_opt_in_and_cursor"],
    "v2-explicit-history-examples": [],
    "v2-filtered-history-example": [],
    "v2-fixtures-omitted": [],
    "v2-history-total-too-small": ["versioned_status_contract_matches_behavior_if_added"],
    "v2-history-missing-wire-field": ["versioned_status_contract_matches_behavior_if_added"],
    "v2-missing-summary-field": ["versioned_status_contract_matches_behavior_if_added"],
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
    semantic_manifest = grader.with_name("v1_semantic_manifest.json")
    manifest = json.loads(semantic_manifest.read_text())
    assert manifest["base_commit"] == BASE
    for name, shape in manifest["contracts"].items():
        raw = (root / "base/contract_test_fixtures/v1" / name).read_bytes()
        assert hashlib.sha256(raw).hexdigest() == manifest["baseline_file_sha256"][name]
        assert contract_shape(json.loads(raw)) == shape, f"Semantic oracle differs from pinned base: {name}"
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

    for tag in EXPECTED_FAILURES:
        if (root / tag).exists():
            continue
        shutil.copytree(root / "reference", root / tag)
    for tag in ("additive-status-fields", "reformatted-fixtures", "additive-device-fields"):
        fixture_root = root / tag / "contract_test_fixtures"
        for fixture in fixture_root.glob("*/*.json"):
            value = json.loads(fixture.read_text())
            if tag == "additive-status-fields":
                if fixture.parent.name != "v1" or not fixture.name.startswith("status_"):
                    continue
                value["step_count"] = len(value.get("steps", []))
            elif tag == "additive-device-fields":
                if fixture.name.startswith("status_"):
                    continue
                value["optional_addition"] = True
            fixture.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n")

    for tag in ("removed-existing-field", "changed-existing-value", "changed-existing-scalar-type"):
        fixture = root / tag / "contract_test_fixtures/v1/step_basic.json"
        value = json.loads(fixture.read_text())
        if tag == "removed-existing-field":
            del value["thinking"]
        elif tag == "changed-existing-value":
            value["description"] = "Different baseline meaning"
        else:
            assert type(value["step_number"]) is int and value["step_number"] == 1
            value["step_number"] = True  # Python equality alone would miss this change.
        fixture.write_text(json.dumps(value) + "\n")
    fixture = root / "changed-existing-array/contract_test_fixtures/v1/status_running.json"
    value = json.loads(fixture.read_text())
    value["steps"].append(dict(value["steps"][0]))
    fixture.write_text(json.dumps(value) + "\n")
    fixture = root / "changed-existing-array-order/contract_test_fixtures/v1/status_completed.json"
    value = json.loads(fixture.read_text())
    assert len(value["steps"]) > 1 and value["steps"][0] != value["steps"][-1]
    value["steps"].reverse()
    fixture.write_text(json.dumps(value) + "\n")

    for tag in ("cursor-false-disabled", "cursor-strict-types", "cursor-rounded"):
        handler = root / tag / PATHS[0]
        source = handler.read_text()
        if tag == "cursor-rounded":
            original = "            since_step = int(since_step)"
            replacement = "            since_step = round(since_step) if isinstance(since_step, float) else int(since_step)"
        else:
            original = '    since_step = arguments.get("since_step")'
            if tag == "cursor-false-disabled":
                replacement = original + '\n    if since_step is False or since_step == "false":\n        since_step = None'
            else:
                replacement = original + '\n    if isinstance(since_step, (bool, float)):\n        return _jsonrpc_error(request_id, -32602, "strict cursor")'
        assert source.count(original) == 1, f"Cursor mutation anchor changed: {tag}"
        handler.write_text(source.replace(original, replacement))
    shutil.copy2(root / "base" / PATHS[2], root / "consumer-missing-history-opt-in" / PATHS[2])
    consumer = root / "consumer-string-true-opt-in" / PATHS[2]
    consumer_source = consumer.read_text()
    assert consumer_source.count('"include_steps": True') == 1, "Consumer mutation anchor changed"
    consumer.write_text(consumer_source.replace('"include_steps": True', '"include_steps": "true"'))

    # Change only schema representation, preserving the actual reference handler.
    schema_changes = {
        "schema-type-arrays": {
            "include_steps": {"type": ["boolean", "string"], "default": False},
            "since_step": {"type": ["number", "string", "boolean", "null"]},
        },
        "schema-anyof-oneof": {
            "include_steps": {"anyOf": [{"type": "boolean"}, {"type": "string"}], "default": "false"},
            "since_step": {"oneOf": [{"type": "integer"}, {"type": "string"}, {"type": "null"}]},
        },
        "schema-local-references": {
            "include_steps": {"$ref": "#/$defs/history~1flag"},
            "since_step": {"$ref": "#/definitions/cursor"},
        },
        "schema-wrong-flag-type": {"include_steps": {"anyOf": [{"type": "string"}, {"type": "null"}]}},
        "schema-wrong-cursor-type": {"since_step": {"type": ["string", "null"]}},
        "schema-cyclic-reference": {"since_step": {"$ref": "#/properties/since_step"}},
    }
    for tag in (name for name in EXPECTED_FAILURES if name.startswith("schema-")):
        handler = root / tag / PATHS[0]
        addition = '\n_preflight_schema = next(tool["inputSchema"] for tool in SESSION_TOOL_DEFS if tool["name"] == "phone_task_status")\n'
        addition += f'_preflight_schema["properties"].update({schema_changes.get(tag, {})!r})\n'
        if tag == "schema-local-references":
            addition += '_preflight_schema["$defs"] = {"history/flag": {"type": ["boolean", "string"]}}\n'
            addition += '_preflight_schema["definitions"] = {"cursor": {"anyOf": [{"type": "integer"}, {"type": "null"}]}}\n'
        elif tag == "schema-missing-flag":
            addition += 'del _preflight_schema["properties"]["include_steps"]\n'
        elif tag == "schema-missing-cursor":
            addition += 'del _preflight_schema["properties"]["since_step"]\n'
        elif tag == "schema-missing-required-task":
            addition += '_preflight_schema["required"].remove("task_id")\n'
        handler.write_text(handler.read_text() + addition + 'del _preflight_schema\n')

    # A status filename identifies task state, not whether history was requested.
    for name in ("status_running.json", "status_completed.json"):
        value = json.loads((root / "reference/contract_test_fixtures/v1" / name).read_text())
        value["step_count"] = len(value["steps"])
        value["example_request"] = {"include_steps": True}
        fixture = root / "v2-explicit-history-examples/contract_test_fixtures/v2" / name
        fixture.write_text(json.dumps(value) + "\n")
    value = json.loads((root / "reference/contract_test_fixtures/v1/status_completed.json").read_text())
    value["step_count"] = len(value["steps"])
    assert value["step_count"] > 1
    cursor = value["steps"][0]["step_number"]
    value["steps"] = [step for step in value["steps"] if step["step_number"] > cursor]
    assert len(value["steps"]) < value["step_count"]
    value["example_request"] = {"since_step": cursor}
    (root / "v2-filtered-history-example/contract_test_fixtures/v2/status_completed.json").write_text(json.dumps(value) + "\n")
    shutil.rmtree(root / "v2-fixtures-omitted/contract_test_fixtures/v2")
    for tag in ("v2-history-total-too-small", "v2-history-missing-wire-field", "v2-missing-summary-field"):
        fixture = root / tag / "contract_test_fixtures/v2/status_with_steps.json"
        value = json.loads(fixture.read_text())
        if tag == "v2-history-total-too-small":
            value["step_count"] = len(value["steps"]) - 1
        elif tag == "v2-history-missing-wire-field":
            del value["steps"][0]["thinking"]
        else:
            del value["terminal"]
        fixture.write_text(json.dumps(value) + "\n")

    report = {
        "suite_version": "real-repo-v2",
        "task_id": "r02_lean_task_status",
        "base_commit": BASE,
        "reference_fix_commit": REFERENCE,
        "python": sys.version,
        "grader_sha256": hashlib.sha256(grader.read_bytes()).hexdigest(),
        "fixture_manifest_sha256": hashlib.sha256(grader.with_name("v1_sha256.json").read_bytes()).hexdigest(),
        "semantic_manifest_sha256": hashlib.sha256(semantic_manifest.read_bytes()).hexdigest(),
        "preflight_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "exported_paths": PATHS,
        "private_artifacts": "Exported source, raw results and logs retained outside the repository in the private preflight directory.",
        "runs": [],
    }
    for tag, expected_failures in EXPECTED_FAILURES.items():
        expected = int(bool(expected_failures))
        result_path = root / f"{tag}-result.json"
        run = subprocess.run(
            [sys.executable, "-B", str(grader), "--repo", str(root / tag), "--output", str(result_path)],
            cwd=root / tag,
            env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "PYTHONDONTWRITEBYTECODE": "1"},
            capture_output=True, text=True, timeout=60,
        )
        (root / f"{tag}.log").write_text(run.stdout + run.stderr)
        result = json.loads(result_path.read_text())
        report["runs"].append({
            "fixture": tag,
            "expected_exit_code": expected,
            "expected_failure_checks": EXPECTED_FAILURES[tag],
            "exit_code": run.returncode,
            "status": result["status"],
            "check_count": len(result["checks"]),
            "passed_checks": sum(c["status"] == "pass" for c in result["checks"]),
            "failed_checks": [c["name"] for c in result["checks"] if c["status"] == "fail"],
        })
    report["status"] = "pass" if all(
        r["exit_code"] == r["expected_exit_code"]
        and set(r["expected_failure_checks"]).issubset(set(r["failed_checks"]))
        for r in report["runs"]
    ) else "fail"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "output": str(args.output), "fixture_root": str(root)}))
    return 0 if report["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
