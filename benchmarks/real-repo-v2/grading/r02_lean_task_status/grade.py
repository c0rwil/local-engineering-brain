#!/usr/bin/env python3
"""Version-2 phone-status grader: semantic compatibility and explicit cursor rules."""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import hashlib
import importlib.util
import io
import json
import logging
import os
from pathlib import Path
import sys
import traceback
import types

sys.dont_write_bytecode = True


def contract_shape(value):
    """Record JSON meaning without publishing private fixture scalar values."""
    kind = {type(None): "null", bool: "boolean", int: "integer", float: "number",
            str: "string", list: "array", dict: "object"}[type(value)]
    if isinstance(value, dict):
        return {"type": kind, "fields": {key: contract_shape(item) for key, item in sorted(value.items())}}
    if isinstance(value, list):
        return {"type": kind, "items": [contract_shape(item) for item in value]}
    encoded = json.dumps(value, ensure_ascii=True, separators=(",", ":"), allow_nan=False).encode()
    return {"type": kind, "value_sha256": hashlib.sha256(encoded).hexdigest()}


def assert_contract_preserved(expected, actual, location):
    """Allow additive object fields, preserving every existing type and value."""
    shape = contract_shape(actual)
    assert shape["type"] == expected["type"], f"Existing JSON type changed: {location}"
    if expected["type"] == "object":
        for key, child in expected["fields"].items():
            assert key in actual, f"Existing JSON field removed: {location}.{key}"
            assert_contract_preserved(child, actual[key], f"{location}.{key}")
    elif expected["type"] == "array":
        assert len(actual) == len(expected["items"]), f"Existing JSON array length changed: {location}"
        for index, (child, item) in enumerate(zip(expected["items"], actual)):
            assert_contract_preserved(child, item, f"{location}[{index}]")
    else:
        assert shape["value_sha256"] == expected["value_sha256"], f"Existing JSON value changed: {location}"


def schema_primary_types(schema, document):
    """Read advertised input types, not implement a JSON Schema validator.

    Accept common nullable/coercible unions and local reusable definitions. The
    actual handler checks below decide conversion, defaults, and invalid params.
    """
    result = set()
    pending = [(schema, frozenset())]
    visited = 0
    while pending:
        node, references = pending.pop()
        visited += 1
        assert visited <= 128, "Tool schema type expansion exceeds bounded inspection"
        assert isinstance(node, dict), "Input type must have a schema object"
        declared = node.get("type", [])
        if isinstance(declared, str):
            declared = [declared]
        assert isinstance(declared, list) and all(isinstance(value, str) for value in declared)
        result.update(declared)
        if "$ref" in node:
            reference = node["$ref"]
            assert isinstance(reference, str) and reference.startswith("#/"), "Only local schema references are inspected"
            assert reference not in references, "Cyclic input type reference"
            target = document
            for part in reference[2:].split("/"):
                target = target[part.replace("~1", "/").replace("~0", "~")]
            pending.append((target, references | {reference}))
        for keyword in ("anyOf", "oneOf"):
            if keyword in node:
                alternatives = node[keyword]
                assert isinstance(alternatives, list) and 0 < len(alternatives) <= 128
                pending.extend((alternative, references) for alternative in alternatives)
    return result


def module(name: str, **attrs):
    value = types.ModuleType(name)
    value.__dict__.update(attrs)
    sys.modules[name] = value
    if "." in name:
        parent, child = name.rsplit(".", 1)
        if parent in sys.modules:
            setattr(sys.modules[parent], child, value)
    return value


def load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    sys.modules[name] = value
    if "." in name:
        parent, child = name.rsplit(".", 1)
        setattr(sys.modules[parent], child, value)
    spec.loader.exec_module(value)
    return value


def deny_network(event, args):
    if event in {"socket.connect", "socket.getaddrinfo", "subprocess.Popen"}:
        raise RuntimeError(f"Grader isolation forbids {event}")


def load_subject(repo: Path):
    # Do not execute package initializers that create development DB clients.
    for name, directory in (
        ("agency", repo / "src/agency"),
        ("agency.api", repo / "src/agency/api"),
        ("agency.api.services", repo / "src/agency/api/services"),
        ("db", repo / "src/db"),
    ):
        module(name, __path__=[str(directory)])
    module("agency.api.logging_config", get_logger=logging.getLogger)
    module("agency.api.services.redis_client", get_redis=lambda: None)
    module("agency.api.services.device_pause_service")
    module("agency.api.services.mcp_billing_service")
    module(
        "agency.api.services.fcm_client",
        FcmDeliveryError=type("FcmDeliveryError", (Exception,), {}),
        FcmPayloadTooLargeError=type("FcmPayloadTooLargeError", (Exception,), {}),
    )
    module("db.users", UserDTO=type("UserDTO", (), {}))
    service = load(
        "agency.api.services.mcp_session_service",
        repo / "src/agency/api/services/mcp_session_service.py",
    )
    handlers = load(
        "agency.api.services.mcp_tool_handlers",
        repo / "src/agency/api/services/mcp_tool_handlers.py",
    )
    return service, handlers


def expected_step(number):
    return {
        "step_number": number,
        "action_type": "tap",
        "description": f"Step {number}",
        "screenshot_thumbnail": "A" * 65536,
        "thinking": f"Reason {number}",
        "created_at": f"2026-01-01T00:00:0{number}Z",
    }


class Redis:
    def __init__(self, service, *, allow_history: bool, count: int = 3):
        self.allow_history = allow_history
        self.calls = []
        self.task = service.MCPTask(
            task_id="t-private", session_id="s-private", user_id="u-private",
            device_id="d-private", prompt="Synthetic grader task",
            enqueued_at="2026-01-01T00:00:00Z", dequeued_at="2026-01-01T00:00:01Z",
            deadline="2099-01-01T00:00:00Z", queue_wait_deadline="2099-01-01T00:00:00Z",
            per_call_timeout_s=300, status="running", terminal=None,
            result_json=None, error_json=None,
        )
        self.steps = [expected_step(n) for n in range(1, count + 1)]
        self.task_key = service.task_key("u-private", "t-private")
        self.steps_key = service.task_steps_key("u-private", "t-private")

    async def hgetall(self, key):
        self.calls.append(("hgetall", key))
        return self.task.to_redis_hash() if key == self.task_key else {}

    async def llen(self, key):
        assert key == self.steps_key, "Count must be scoped to this user and task"
        self.calls.append(("llen", key))
        return len(self.steps)

    async def lrange(self, key, start, end):
        assert key == self.steps_key, "History must be scoped to this user and task"
        self.calls.append(("lrange", key))
        assert self.allow_history, "Default polling loaded full Redis step payloads"
        values = self.steps[start:] if end == -1 else self.steps[start:end + 1]
        return [json.dumps(step) for step in values]


def unwrap(response):
    assert response["jsonrpc"] == "2.0"
    assert response["id"] == "grader-request"
    assert "error" not in response, response.get("error")
    content = response["result"]["content"]
    assert content[0]["type"] == "text"
    return json.loads(content[0]["text"])


def execute(repo: Path):
    checks = []
    metrics = {}

    def check(name, fn):
        try:
            result = fn()
            if asyncio.iscoroutine(result):
                asyncio.run(result)
            checks.append({"name": name, "status": "pass"})
        except Exception as error:
            checks.append({"name": name, "status": "fail", "error": f"{type(error).__name__}: {error}"})

    try:
        service, handlers = load_subject(repo)
    except Exception as error:
        return {
            "status": "fail", "checks": [{"name": "load_actual_modules", "status": "fail", "error": f"{type(error).__name__}: {error}"}],
            "traceback": traceback.format_exc(),
        }

    async def poll(arguments, *, allow_history=False, count=3, state="running", terminal=None, user="u-private"):
        redis = Redis(service, allow_history=allow_history, count=count)
        redis.task.status = state
        redis.task.terminal = terminal
        if terminal == "ok":
            redis.task.result_json = json.dumps({"summary": "Finished"})
        if terminal == "needs_user_control":
            redis.task.error_json = json.dumps({"code": "needs_user_control", "message": "Confirm?"})
        service.get_redis = lambda: redis
        response = await handlers.handle_phone_task_status(
            user_id=user, arguments=arguments, request_id="grader-request",
        )
        return response, redis

    async def lean(value, count=3):
        arguments = {"task_id": "t-private"}
        if value is not None:
            arguments["include_steps"] = value
        response, redis = await poll(arguments, count=count)
        body = unwrap(response)
        assert set(body) == {"status", "terminal", "step_count", "result", "error"}, set(body)
        assert body == {"status": "running", "terminal": None, "step_count": count, "result": None, "error": None}
        assert type(body["step_count"]) is int
        assert sum(name == "llen" for name, _ in redis.calls) == 1, redis.calls
        assert all(name != "lrange" for name, _ in redis.calls)
        metrics["default_response_json_bytes"] = len(json.dumps(body).encode())

    for name, value in (("default_uses_count_without_loading_history", None), ("boolean_false_stays_lean", False), ("string_false_stays_lean", "false"), ("spaced_string_false_stays_lean", " FALSE ")):
        check(name, lambda value=value: lean(value))
    check("empty_history_has_integer_zero_count", lambda: lean(None, count=0))

    async def null_cursor():
        response, redis = await poll({"task_id": "t-private", "since_step": None})
        assert "steps" not in unwrap(response)
        assert all(name != "lrange" for name, _ in redis.calls)

    check("null_cursor_is_absent_and_stays_lean", null_cursor)

    def serializer():
        populated = expected_step(1)
        optional_nulls = {**expected_step(2), "description": None, "screenshot_thumbnail": None, "thinking": None}
        for expected in (populated, optional_nulls):
            actual = json.loads(service.Step(**expected).to_json())
            assert actual == expected, "Runtime Step serialization lost wire fields or their values"

    check("runtime_step_serializer_preserves_all_wire_fields", serializer)

    async def explicit(arguments, expected_numbers):
        response, _ = await poll({"task_id": "t-private", **arguments}, allow_history=True)
        body = unwrap(response)
        assert body["step_count"] == 3
        assert body["steps"] == [expected_step(n) for n in expected_numbers], "Explicit history lost step fields or their values"
        if len(expected_numbers) == 3:
            metrics["explicit_history_response_json_bytes"] = len(json.dumps(body).encode())

    for name, arguments, numbers in (
        ("explicit_true_returns_full_history", {"include_steps": True}, [1, 2, 3]),
        ("string_true_returns_full_history", {"include_steps": "true"}, [1, 2, 3]),
        ("cursor_returns_strictly_newer_steps_total_count", {"since_step": 1}, [2, 3]),
        ("string_cursor_implies_steps_even_with_false", {"since_step": "2", "include_steps": False}, [3]),
        ("cursor_zero_includes_all", {"since_step": 0}, [1, 2, 3]),
        ("cursor_past_end_keeps_total", {"since_step": 100}, []),
        ("false_cursor_converts_to_zero_and_implies_history", {"since_step": False, "include_steps": "false"}, [1, 2, 3]),
        ("true_cursor_converts_to_one", {"since_step": True}, [2, 3]),
        ("fractional_cursor_truncates_toward_zero", {"since_step": 1.8}, [2, 3]),
        ("negative_fractional_cursor_truncates_toward_zero", {"since_step": -0.8}, [1, 2, 3]),
        ("signed_spaced_string_cursor_is_converted", {"since_step": " +2 "}, [3]),
        ("negative_integer_cursor_includes_newer_steps", {"since_step": -1}, [1, 2, 3]),
    ):
        check(name, lambda arguments=arguments, numbers=numbers: explicit(arguments, numbers))

    async def invalid(value):
        response, redis = await poll({"task_id": "t-private", "since_step": value})
        assert response["error"]["code"] == -32602, response
        assert all(name != "lrange" for name, _ in redis.calls)

    for name, value in (("invalid_text_cursor", "abc"), ("invalid_array_cursor", []), ("invalid_object_cursor", {}),
                        ("false_string_cursor_is_invalid_not_disabled", "false"),
                        ("fractional_string_cursor_is_invalid", "1.8"),
                        ("empty_string_cursor_is_invalid", "")):
        check(name, lambda value=value: invalid(value))

    async def errors():
        response, _ = await poll({})
        assert response["error"]["code"] == -32602
        response, _ = await poll({"task_id": "missing"})
        assert response["error"]["code"] == -32004
        response, _ = await poll({"task_id": "t-private"}, user="another-user")
        assert response["error"]["code"] == -32004

    check("missing_task_and_cross_user_access_preserved", errors)

    async def terminal_summary(terminal):
        response, _ = await poll({"task_id": "t-private"}, state="completed", terminal=terminal)
        body = unwrap(response)
        assert body["status"] == "completed" and body["terminal"] == terminal
        assert body["step_count"] == 3 and "steps" not in body
        if terminal == "ok":
            assert body["result"] == {"summary": "Finished"} and body["error"] is None
        else:
            assert body["error"] == {"code": "needs_user_control", "message": "Confirm?"}
            assert body["result"] is None

    check("completed_result_preserved", lambda: terminal_summary("ok"))
    check("user_control_error_preserved", lambda: terminal_summary("needs_user_control"))

    def schema():
        tool = next(t for t in handlers.SESSION_TOOL_DEFS if t["name"] == "phone_task_status")
        document = tool["inputSchema"]
        properties = document["properties"]
        assert "boolean" in schema_primary_types(properties["include_steps"], document)
        # JSON Schema's number type also admits integer inputs.
        assert {"integer", "number"} & schema_primary_types(properties["since_step"], document)
        assert "task_id" in document["required"]

    check("tool_schema_exposes_opt_in_and_cursor", schema)

    contracts = json.loads(Path(__file__).with_name("v1_semantic_manifest.json").read_text())["contracts"]

    def v1_preserved():
        for name, expected in contracts.items():
            actual = json.loads((repo / "contract_test_fixtures/v1" / name).read_text())
            assert_contract_preserved(expected, actual, f"v1/{name}")

    check("v1_fixture_existing_json_meaning_preserved", v1_preserved)

    def v2_devices_preserved():
        if not (repo / "contract_test_fixtures/v2").is_dir():
            return
        for name, expected in contracts.items():
            if name.startswith("status_"):
                continue
            fixture = repo / "contract_test_fixtures/v2" / name
            if not fixture.exists():
                continue
            assert_contract_preserved(expected, json.loads(fixture.read_text()), f"v2/{name}")

    check("v2_device_wire_json_meaning_preserved_if_added", v2_devices_preserved)

    def v2_status():
        root = repo / "contract_test_fixtures/v2"
        if not root.is_dir():
            return
        for fixture in root.glob("status_*.json"):
            body = json.loads(fixture.read_text())
            assert {"status", "terminal", "step_count", "result", "error"} <= body.keys(), fixture.name
            assert type(body["step_count"]) is int and body["step_count"] >= 0, fixture.name
            # Filenames do not identify the request: either an explicit-history
            # or a lean response may illustrate a running/completed task.
            if "steps" in body:
                assert isinstance(body["steps"], list), fixture.name
                assert body["step_count"] >= len(body["steps"]), fixture.name
                for step in body["steps"]:
                    assert type(step["step_number"]) is int, fixture.name
                    assert isinstance(step["action_type"], str), fixture.name
                    assert isinstance(step["created_at"], str), fixture.name
                    for field in ("description", "screenshot_thumbnail", "thinking"):
                        assert step[field] is None or isinstance(step[field], str), (fixture.name, field)

    check("versioned_status_contract_matches_behavior_if_added", v2_status)

    def consumer():
        module("httpx", Client=type("Client", (), {}))
        subject = load("grader_user_control_consumer", repo / "scripts/mcp_user_control_integration_test.py")
        calls = []

        def call(client, *, tool, arguments=None, req_id=1):
            assert tool == "phone_task_status"
            calls.append(dict(arguments))
            # Exercise the real handler, not a hand-written imitation of its response.
            response, _ = asyncio.run(poll(arguments, allow_history=True, state="completed", terminal="needs_user_control"))
            return unwrap(response)

        subject.mcp_call = call
        with contextlib.redirect_stdout(io.StringIO()):
            result = subject.poll_task(object(), "t-private", label="private-grader")
        assert result["terminal"] == "needs_user_control"
        assert len(result["steps"]) == 3, "User-control consumer lost the phone's prompt steps"
        def opted_in(arguments):
            flag = arguments.get("include_steps")
            return (flag is True or isinstance(flag, str) and flag.strip().lower() == "true"
                    or arguments.get("since_step") is not None)

        assert any(opted_in(arguments) for arguments in calls), calls

    check("user_control_consumer_explicitly_fetches_needed_steps", consumer)
    try:
        response, redis = asyncio.run(poll({"task_id": "t-private"}, allow_history=True))
        metrics["default_response_json_bytes"] = len(json.dumps(unwrap(response)).encode())
        metrics["default_history_payload_reads"] = sum(name == "lrange" for name, _ in redis.calls)
        metrics["synthetic_steps"] = len(redis.steps)
        metrics["synthetic_screenshot_characters_per_step"] = 65536
        metrics["measurement_scope"] = "Synthetic product response bytes, not coding-agent token usage"
    except Exception as error:
        metrics["measurement_error"] = f"{type(error).__name__}: {error}"
    return {
        "status": "pass" if all(c["status"] == "pass" for c in checks) else "fail",
        "checks": checks,
        "metrics": metrics,
        "isolation": {"dependencies": "Python standard library only", "real_handler_and_service_executed": True, "development_conftest_loaded": False, "network_and_subprocesses_denied": True, "environment_cleared": True},
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    repo = args.repo.resolve()
    output = args.output.resolve()
    if sys.version_info < (3, 11) or not repo.is_dir():
        result = {"status": "infra_error", "checks": [], "error": "Python >=3.11 and an existing repository directory are required"}
    else:
        os.environ.clear()
        sys.addaudithook(deny_network)
        try:
            result = execute(repo)
        except Exception:
            result = {"status": "infra_error", "checks": [], "traceback": traceback.format_exc()}
    result["task_id"] = "r02_lean_task_status"
    result["suite_version"] = "real-repo-v2"
    result["repo"] = str(repo)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"status": result["status"], "checks": len(result["checks"]), "output": str(output)}))
    return {"pass": 0, "fail": 1, "infra_error": 2}[result["status"]]


if __name__ == "__main__":
    raise SystemExit(main())
