#!/usr/bin/env python3
"""Model-free runtime preflight for the Codex arm of the real-repository runner.

For each arm, the runner's own adapter builds the exact per-trial Codex home.
The pinned Codex binary then runs against a scripted loopback Responses
endpoint: no model is called, no credentials are read, and the only provider is
127.0.0.1. The preflight verifies:

- effective feature flags and MCP servers: hooks and the engram server exist
  only in the Brain arm, and every disabled feature is off;
- hook trust: with persisted trust, the Brain config trusts exactly the
  installed handlers and no bypass flag is passed, so hook delivery proves
  Codex accepted the trust entries;
- the model-visible prompt input lists no user skills from the real HOME;
- through Codex's real exec tool: the checkout and scratch directory are
  writable; evidence, the Engram checkout, source repositories, agent state and
  other or later trial roots are unreadable; other writes are denied; loopback
  TCP and Unix sockets are blocked; secrets are absent from the tool environment;
- Brain hooks deliver Engram context and the daemon acknowledges it, Off sends
  none, and the transcript parser recovers the scripted usage exactly.

A passing run writes codex-runtime-preflight.json, which run_real_repo_followup.py
requires before a Codex collection.
"""

from __future__ import annotations

import argparse
import contextlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
from types import SimpleNamespace

sys.dont_write_bytecode = True
import run_real_repo_pilot as runner  # noqa: E402
import real_repo_codex as codex_arm  # noqa: E402
from real_repo_metrics import parse_codex_trace  # noqa: E402

REVISION = "codex-loopback-v1"
PROOF = "codex-runtime-preflight.json"
OUTPUT = "codex-preflight"
ARMS = ("off", "brain")
# A fake value under the production variable name: the probe proves the tool
# environment drops it. The operator's real credential is never used here.
STUB_KEY = "loopback-preflight-not-a-credential"
PROMPT = "Inspect runtime_smoke in smoke.py, then run the prescribed checks."
PROBE_MARKER = "ERP26_PROBE_RESULT "
ENGRAM_MARKERS = ("Task source map", "Stable project architecture")
# Per-response usage the stub reports, and what the transcript parser must recover.
SCRIPTED_USAGE = (
    {"input_tokens": 1200, "input_tokens_details": {"cached_tokens": 400},
     "output_tokens": 60, "output_tokens_details": {"reasoning_tokens": 25}, "total_tokens": 1260},
    {"input_tokens": 1500, "input_tokens_details": {"cached_tokens": 1100},
     "output_tokens": 40, "output_tokens_details": {"reasoning_tokens": 10}, "total_tokens": 1540},
)
EXPECTED_USAGE = {"input_tokens": 1200, "cache_read_input_tokens": 1500, "cache_creation_input_tokens": 0,
                  "output_tokens": 100, "reasoning_output_tokens": 35, "processed_input_tokens": 2700,
                  "total_tokens": 2800}
ARTIFACTS = {f"{OUTPUT}/{arm}.{suffix}" for arm in ARMS for suffix in
             ("checks.json", "trace.jsonl", "stderr.log", "config.toml", "requests.json", "session.jsonl")}
CHECK_NAMES = {
    "cli_exit", "features", "mcp_servers", "hook_trust", "prompt_input_without_user_skills", "pinned_model_and_effort",
    "engram_tools", "engram_context", "probe_executed", "private_reads_denied", "allowed_paths_writable",
    "other_writes_denied", "network_denied", "tool_environment", "transcript_usage", "transcript_tools",
    "delivery_receipts",
}


def identity(data: Path, active: Path, adapter) -> dict:
    scripts = runner.ROOT / "scripts"
    return {
        "revision": REVISION, "platform": sys.platform,
        "codex_sha256": runner.digest(adapter.executable), "codex_version": codex_arm.version(adapter.executable),
        "model": adapter.model, "effort": adapter.effort, "hook_trust": adapter.hook_trust,
        "engram_sha256": runner.digest(runner.ENGRAM), "daemon_sha256": runner.digest(runner.DAEMON),
        "runner_sha256": runner.digest(Path(runner.__file__)),
        "adapter_sha256": runner.digest(scripts / "real_repo_codex.py"),
        "metrics_sha256": runner.digest(scripts / "real_repo_metrics.py"),
        "preflight_sha256": runner.digest(Path(__file__)),
        "config_template_sha256": codex_arm.config_template_sha256(adapter.model, adapter.effort),
        "data": str(data), "active": str(active),
    }


def validate_proof(data: Path, active: Path, adapter) -> dict:
    """Refuse collection unless a passing proof matches the current controls."""
    proof = json.loads((data / PROOF).read_text())
    if (proof.get("schema_version") != 1 or proof.get("status") != "pass"
            or proof.get("identity") != identity(data, active, adapter) or proof.get("arms") != list(ARMS)):
        raise ValueError("Codex runtime preflight is absent, failed, or differs from current collection controls")
    if set(proof.get("artifacts", {})) != ARTIFACTS:
        raise ValueError("Codex runtime preflight proof lacks the exact artifact set")
    for relative, expected in proof["artifacts"].items():
        path = Path(relative)
        if path.is_absolute() or ".." in path.parts or runner.digest(data / path) != expected:
            raise ValueError("Codex runtime preflight artifact differs from its proof")
    for arm in ARMS:
        result = json.loads((data / OUTPUT / f"{arm}.checks.json").read_text())
        checks = result.get("checks", {})
        if result.get("passed") is not True or set(checks) != CHECK_NAMES or any(value is not True for value in checks.values()):
            raise ValueError("Codex runtime preflight did not pass every check")
    return proof


def scripted_provider(probe_command: str):
    """Serve scripted Responses streams: one exec_command call, then a final message."""
    requests = []

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"{}")
            if not self.path.split("?")[0].endswith("/responses"):
                self.send_error(404)
                return
            requests.append(body)
            index = len(requests)
            if index == 1:
                item = {"type": "function_call", "id": "fc_preflight_1", "call_id": "call_preflight_1",
                        "name": "exec_command",
                        "arguments": json.dumps({"cmd": probe_command, "yield_time_ms": 30000,
                                                 "max_output_tokens": 8000})}
            else:
                item = {"type": "message", "id": f"msg_preflight_{index}", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": "Preflight complete.", "annotations": []}]}
            usage = SCRIPTED_USAGE[min(index, len(SCRIPTED_USAGE)) - 1]
            response_id = f"resp_preflight_{index}"
            events = [
                {"type": "response.created", "response": {"id": response_id, "status": "in_progress"}},
                {"type": "response.output_item.added", "output_index": 0, "item": item},
                {"type": "response.output_item.done", "output_index": 0, "item": item},
                {"type": "response.completed",
                 "response": {"id": response_id, "status": "completed", "usage": usage, "output": [item]}},
            ]
            payload = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, requests


def probe_script(spec: dict) -> str:
    """A self-contained check run by Codex's exec tool inside the arm's sandbox."""
    return f'''import errno, json, os, socket
from pathlib import Path
SPEC = json.loads({json.dumps(json.dumps(spec))})

def outcome(action):
    try:
        action()
        return "allowed"
    except FileNotFoundError:
        return "absent"
    except OSError as error:
        return "denied" if error.errno in (errno.EPERM, errno.EACCES) else "error"

def read(path):
    target = Path(path)
    return target.read_bytes() if target.is_file() or not target.exists() else os.listdir(target)

def unix(path):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(3)
        client.connect(path)

result = {{
    "allowed_read": Path(SPEC["allowed"]).read_text().strip() == "RUNTIME_ALLOWED",
    "reads": {{name: outcome(lambda path=path: read(path)) for name, path in SPEC["reads"].items()}},
    "writes": {{name: outcome(lambda path=path: Path(path).write_text("probe")) for name, path in SPEC["writes"].items()}},
    "tcp": outcome(lambda: socket.create_connection(("127.0.0.1", SPEC["tcp_port"]), timeout=3).close()),
    "unix": {{name: outcome(lambda path=path: unix(path)) for name, path in SPEC["sockets"].items()}},
    "home": os.environ.get("HOME"), "tmpdir": os.environ.get("TMPDIR"),
    "secrets_visible": sorted(name for name in SPEC["secrets"] if name in os.environ),
}}
print({json.dumps(PROBE_MARKER)} + json.dumps(result))
'''


def features(codex: Path, env: dict) -> dict:
    output = runner.command([codex, "features", "list"], env=env)
    states = {}
    for line in output.splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[-1] in ("true", "false"):
            states[parts[0]] = parts[-1] == "true"
    return states


def text_of(value) -> str:
    return json.dumps(value) if not isinstance(value, str) else value


def probe_result(requests: list) -> dict | None:
    """The probe's JSON line from a function_call_output sent back to the stub."""
    for body in requests:
        for item in body.get("input") or []:
            if not isinstance(item, dict) or item.get("type") != "function_call_output":
                continue
            output = item.get("output")
            if isinstance(output, list):
                output = "\n".join(part.get("text", "") for part in output if isinstance(part, dict))
            for line in output.splitlines() if isinstance(output, str) else []:
                if PROBE_MARKER in line:
                    with contextlib.suppress(ValueError):
                        return json.JSONDecoder().raw_decode(line[line.index(PROBE_MARKER) + len(PROBE_MARKER):])[0]
    return None


def run_arm(arm: str, data: Path, active: Path, adapter, output: Path) -> dict:
    if active.exists():
        shutil.rmtree(active)
    active.mkdir(mode=0o700)
    (active / ".pilot-owned").write_text("codex-runtime-preflight\n")
    work, state, scratch = active / "work", active / "state", active / "scratch"
    for path in (work, state, scratch):
        path.mkdir()
    (work / "allowed.txt").write_text("RUNTIME_ALLOWED\n")
    (work / "smoke.py").write_text("def runtime_smoke():\n    return True\n")
    runner.command(["git", "init", "-q", work])
    runner.command(["git", "-C", work, "add", "-A"])
    runner.command(["git", "-C", work, "-c", "user.name=Benchmark", "-c", "user.email=benchmark@invalid",
                    "commit", "-qm", "Preflight source"])
    (work / ".engram").mkdir()
    config = state / "config.toml"
    config.write_text(runner.config_text(state / "brain.sqlite", state / "d.sock"))
    (work / ".engram/config.toml").write_text(config.read_text())
    (state / "private-canary.txt").write_text("PRIVATE_RUNTIME_CANARY\n")
    temporary = []

    def private_dir(prefix):
        path = Path(tempfile.mkdtemp(prefix=prefix, dir="/private/tmp"))
        (path / "private.txt").write_text("PRIVATE_RUNTIME_CANARY\n")
        temporary.append(path)
        return path

    # Existing at policy time: enumerated. Created later: covered by globs.
    sibling = private_dir("erp26work-codexpf-sibling-")
    trial = output / f"{arm}-trial"
    trial.mkdir()
    prepared = adapter.prepare(trial=trial, work=work, state=state, arm=arm,
                               args=SimpleNamespace(data=data, active=active))
    (prepared["user_home"] / "private-canary.txt").write_text("PRIVATE_RUNTIME_CANARY\n")
    future, claude_scratch = private_dir("erp26-codexpf-future-"), private_dir("claude-codexpf-")
    unrelated = Path(tempfile.mkdtemp(prefix="codexpf-unrelated-", dir="/private/tmp"))
    temporary.append(unrelated)
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(str(active / "probe.sock"))
    listener.listen()
    server, requests = scripted_provider("python3 .erp26-codex-probe.py")
    checks = {}
    try:
        env = adapter.environment(prepared, compact=True)
        env[codex_arm.AUTH_ENV] = STUB_KEY
        tool_env = {key: env[key] for key in ("PATH", "HOME", "CODEX_HOME", "LANG") if key in env}
        states = features(adapter.executable, tool_env)
        checks["features"] = (states.get("hooks") is (arm == "brain")
                              and all(states.get(name) is False for name in codex_arm.DISABLED_FEATURES))
        servers = json.loads(runner.command([adapter.executable, "mcp", "list", "--json"], env=tool_env) or "[]")
        expected_mcp = str(runner.ENGRAM.parent / "engram-mcp")
        checks["mcp_servers"] = ((len(servers) == 1 and expected_mcp in json.dumps(servers)
                                  and "--no-autostart" in json.dumps(servers)) if arm == "brain" else servers == [])
        rendered = subprocess.run([str(adapter.executable), "debug", "prompt-input", "preflight"], cwd=work,
                                  env=tool_env, capture_output=True, text=True, timeout=120)
        # The permission profile's denied paths (for example ~/.engram) are
        # shown to the model in both arms; Engram *context* must not be.
        prompt_flags = {"user_skills": ".agents/skills" in rendered.stdout,
                        "engram_context": any(marker in rendered.stdout for marker in ENGRAM_MARKERS),
                        "engram_mentions": rendered.stdout.lower().count("engram")}
        checks["prompt_input_without_user_skills"] = (rendered.returncode == 0 and not prompt_flags["user_skills"]
                                                      and not prompt_flags["engram_context"])
        spec = {
            "allowed": str(work / "allowed.txt"),
            "reads": {
                "evidence": str(output / "private-canary.txt"), "engram_checkout": str(runner.ROOT / "Cargo.toml"),
                "source_repositories": str(runner.SOURCE_ROOT), "engram_home": str(Path.home() / ".engram"),
                "codex_user_state": str(Path.home() / ".codex"), "claude_user_state": str(Path.home() / ".claude"),
                "engram_state": str(state / "private-canary.txt"), "codex_home": str(prepared["codex_home"] / "config.toml"),
                "codex_process_home": str(prepared["user_home"] / "private-canary.txt"),
                "sibling_trial": str(sibling / "private.txt"), "future_trial": str(future / "private.txt"),
                "claude_scratch": str(claude_scratch / "private.txt"),
            },
            "writes": {
                "checkout": str(work / "probe-write.txt"), "scratch": str(scratch / "probe-write.txt"),
                "checkout_engram": str(work / ".engram/probe-write.txt"), "active_root": str(active / "probe-write.txt"),
                "unrelated_tmp": str(unrelated / "probe-write.txt"),
                "codex_home": str(prepared["codex_home"] / "probe-write.txt"),
            },
            "tcp_port": server.server_address[1],
            "sockets": {"probe": str(active / "probe.sock"), **({"daemon": str(state / "d.sock")} if arm == "brain" else {})},
            "secrets": list(codex_arm.EXCLUDED_ENV),
        }
        (work / ".erp26-codex-probe.py").write_text(probe_script(spec))
        argv = adapter.command(prepared)
        stored = tomllib.loads((prepared["codex_home"] / "config.toml").read_text()).get("hooks", {}).get("state", {})
        stored = {key: value.get("trusted_hash") for key, value in stored.items()}
        bypass = "--dangerously-bypass-hook-trust" in argv
        if arm == "brain" and adapter.hook_trust == "persisted":
            checks["hook_trust"] = not bypass and stored == codex_arm.codex_hook_trust_entries(prepared["codex_home"])
        else:
            checks["hook_trust"] = bypass == (arm == "brain") and not stored
        argv[-1:-1] = ["-c", 'model_provider="erp26_preflight"',
                       "-c", "model_providers.erp26_preflight={name=\"loopback preflight\", "
                             f"base_url=\"http://127.0.0.1:{server.server_address[1]}/v1\", wire_api=\"responses\", "
                             f"env_key=\"{codex_arm.AUTH_ENV}\", request_max_retries=0, stream_max_retries=0}}"]
        manager = runner.daemon(config, state) if arm == "brain" else contextlib.nullcontext()
        with manager:
            if arm == "brain":
                runner.cli(state, "index", work, "--json")
            with (output / f"{arm}.trace.jsonl").open("w") as trace, (output / f"{arm}.stderr.log").open("w") as err:
                run = subprocess.run(argv, cwd=work, env=env, input=PROMPT, text=True, stdout=trace, stderr=err, timeout=300)
            receipts = runner.receipt_summary(state) if arm == "brain" else {"emission_receipts": 0}
        checks["cli_exit"] = run.returncode == 0
        sessions = codex_arm.session_files(prepared["codex_home"])
        with (output / f"{arm}.session.jsonl").open("wb") as combined:
            for session in sessions:
                combined.write(session.read_bytes())
        metrics = parse_codex_trace(output / f"{arm}.trace.jsonl", sessions)
        inputs = [text_of(body.get("input")) for body in requests]
        tools = [text_of(body.get("tools")) for body in requests]
        runner.write_json(output / f"{arm}.requests.json", [
            {"model": body.get("model"), "reasoning": body.get("reasoning"),
             "tool_count": len(body.get("tools") or []), "engram_tools": "engram" in text_of(body.get("tools")),
             "engram_context": any(marker in text_of(body.get("input")) for marker in ENGRAM_MARKERS)}
            for body in requests])
        shutil.copy2(prepared["codex_home"] / "config.toml", output / f"{arm}.config.toml")
        checks["pinned_model_and_effort"] = bool(requests) and all(
            body.get("model") == adapter.model and (body.get("reasoning") or {}).get("effort") == adapter.effort
            for body in requests)
        has_tools = any("engram" in text for text in tools)
        has_context = any(marker in text for text in inputs for marker in ENGRAM_MARKERS)
        checks["engram_tools"] = has_tools if arm == "brain" else not has_tools
        checks["engram_context"] = (has_context and any("smoke.py" in text for text in inputs)) if arm == "brain" else not has_context
        probe = probe_result(requests)
        checks["probe_executed"] = isinstance(probe, dict) and probe.get("allowed_read") is True
        probe = probe if isinstance(probe, dict) else {}
        reads = probe.get("reads", {})
        optional = {"source_repositories", "engram_home", "codex_user_state", "claude_user_state"}
        checks["private_reads_denied"] = bool(reads) and all(
            reads.get(name) == "denied" or (name in optional and reads.get(name) == "absent") for name in spec["reads"])
        writes = probe.get("writes", {})
        checks["allowed_paths_writable"] = writes.get("checkout") == writes.get("scratch") == "allowed"
        checks["other_writes_denied"] = all(writes.get(name) == "denied" for name in
                                            ("checkout_engram", "active_root", "unrelated_tmp", "codex_home"))
        checks["network_denied"] = (probe.get("tcp") == "denied" and bool(probe.get("unix"))
                                    and all(value == "denied" for value in probe["unix"].values()))
        checks["tool_environment"] = (probe.get("home") == str(Path.home()) and probe.get("tmpdir") == str(scratch)
                                      and probe.get("secrets_visible") == [])
        checks["transcript_usage"] = (all(metrics.get(name) == value for name, value in EXPECTED_USAGE.items())
                                      and metrics["usage_complete"] and metrics["diagnostics"]["usage_sources_agree"] is True
                                      and metrics["models"] == [adapter.model])
        checks["transcript_tools"] = (metrics["tool_calls"] == 1 and metrics["counts_by_tool"] == {"exec_command": 1}
                                      and metrics["trace_complete"] and metrics["is_error"] is False)
        checks["delivery_receipts"] = (receipts["emission_receipts"] >= 1 if arm == "brain"
                                       else not (state / "brain.sqlite").exists())
    finally:
        server.shutdown()
        server.server_close()
        listener.close()
        for path in temporary:
            shutil.rmtree(path, ignore_errors=True)
    result = {"passed": set(checks) == CHECK_NAMES and all(checks.values()), "checks": checks,
              "probe": probe, "requests": len(requests), "prompt_input": prompt_flags}
    runner.write_json(output / f"{arm}.checks.json", result)
    return result


def run_preflight(data: Path, active: Path, adapter) -> dict:
    runner.validate_paths(data, active, compact=True)
    if (data / "collection_stop.json").exists() or (data / "trials").exists():
        raise ValueError("Preflight requires a fresh, unstarted collection")
    data.mkdir(mode=0o700, exist_ok=True)
    output = data / OUTPUT
    if output.exists() or (data / PROOF).exists():
        raise ValueError("Preserve prior preflight evidence; use a fresh collection root")
    if active.exists():
        raise ValueError("Preflight requires an absent active root; do not overwrite existing evidence")
    output.mkdir(mode=0o700)
    (output / "private-canary.txt").write_text("PRIVATE_RUNTIME_CANARY\n")
    observed = identity(data, active, adapter)
    results = {}
    for arm in ARMS:
        results[arm] = run_arm(arm, data, active, adapter, output)
        if not results[arm]["passed"]:
            raise RuntimeError(f"Codex {arm} runtime preflight failed; inspect {output / (arm + '.checks.json')}")
    if identity(data, active, adapter) != observed:
        raise ValueError("Controls changed during the Codex preflight; no passing proof was written")
    proof = {"schema_version": 1, "status": "pass", "identity": observed, "arms": list(ARMS),
             "provider": "scripted loopback Responses endpoint; no model calls and no credentials",
             "artifacts": {relative: runner.digest(data / relative) for relative in sorted(ARTIFACTS)}}
    runner.write_json(data / PROOF, proof)
    return proof


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--data", required=True, type=Path)
    parser.add_argument("--active", required=True, type=Path)
    parser.add_argument("--engram-release", required=True, type=Path)
    parser.add_argument("--codex", type=Path)
    parser.add_argument("--codex-model", required=True)
    parser.add_argument("--codex-effort", default="high")
    parser.add_argument("--codex-hook-trust", choices=("persisted", "bypass"), default="persisted")
    args = parser.parse_args()
    runner.validate_paths(args.data, args.active, compact=True)
    runner.ENGRAM = (args.engram_release / "bin/engram").resolve(strict=True)
    runner.DAEMON = (args.engram_release / "bin/engramd").resolve(strict=True)
    adapter = codex_arm.CodexAgent(codex_arm.find_codex(args.codex), args.codex_model, args.codex_effort,
                                   args.codex_hook_trust)
    started = time.monotonic()
    run_preflight(args.data.resolve(), args.active.resolve(), adapter)
    print(json.dumps({"status": "pass", "proof": str(args.data / PROOF),
                      "seconds": round(time.monotonic() - started, 1)}))


if __name__ == "__main__":
    main()
