#!/usr/bin/env python3
"""Exercise the real CLI tools against a deterministic loopback provider, without a model."""
import argparse
import contextlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid

sys.dont_write_bytecode = True
import run_real_repo_pilot as runner

REVISION = "compact-macos-tools-v2"
CHECK_NAMES = {"cli_exit", "exact_tool_count", "active_read", "private_read_denials", "sandboxed_bash", "unsandboxed_bypass_closed", "temp_inventory_stress",
               "private_claude_tmp", "shared_claude_tmp_unused"}
SHARED_CLAUDE_TMP = Path("/private/tmp") / f"claude-{os.getuid()}"


def session_ids(trace):
    """Claude session ids reported in a stream-JSON trace."""
    found = set()
    for line in trace.read_text(errors="replace").splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(event, dict) and isinstance(event.get("session_id"), str):
            found.add(event["session_id"])
    return found


def shared_claude_tmp_traces(work, sessions):
    """Entries in the shared per-uid Claude temp root that belong to this run's sessions or checkout."""
    if not SHARED_CLAUDE_TMP.is_dir():
        return []
    slug = re.sub(r"[^A-Za-z0-9]", "-", str(work))
    return sorted(path.name for path in SHARED_CLAUDE_TMP.iterdir()
                  if path.name.startswith(slug) or any(session in path.name for session in sessions))
ARTIFACTS = {f"runtime-preflight/{arm}.{suffix}" for arm in ("off", "brain") for suffix in
             ("checks.json", "trace.jsonl", "tool-results.json", "stderr.log", "settings.json")} | {"runtime-preflight/private-canary.txt"}


def identity(data, active):
    work, state = active / "work", active / "state"
    return {
        "revision": REVISION, "platform": sys.platform,
        "runner_sha256": runner.digest(Path(runner.__file__)),
        "preflight_sha256": runner.digest(Path(__file__)),
        "claude_sha256": runner.digest(runner.CLAUDE),
        "engram_sha256": runner.digest(runner.ENGRAM), "daemon_sha256": runner.digest(runner.DAEMON),
        "data": str(data), "active": str(active),
        "coding_environment": runner.coding_env(work, compact=True),
        # The policy without the sibling-root inventory: the executed policy adds a
        # by-name denial for every other erp26work root present at each attempt.
        "settings": {arm: runner.settings(work, data, active, state, arm, compact=True, sibling_roots=False)
                     for arm in ("off", "brain")},
    }


def validate_proof(data, active):
    proof_path = data / "runtime-preflight.json"
    proof = json.loads(proof_path.read_text())
    if (proof.get("schema_version") != 1 or proof.get("status") != "pass"
            or proof.get("identity") != identity(data, active)
            or proof.get("arms") != ["off", "brain"]):
        raise ValueError("Runtime preflight is absent, failed, or differs from current collection controls "
                         "(it binds the checkout, scripts, binaries and environment; rerun it with --supersede)")
    if set(proof.get("artifacts", {})) != ARTIFACTS:
        raise ValueError("Runtime preflight proof lacks the exact tool execution artifact set")
    for relative, expected in proof["artifacts"].items():
        path = Path(relative)
        if path.is_absolute() or ".." in path.parts or runner.digest(data / path) != expected:
            raise ValueError("Runtime preflight artifact differs from its proof")
    for arm in ("off", "brain"):
        checks_path = f"runtime-preflight/{arm}.checks.json"
        result = json.loads((data / checks_path).read_text())
        checks = result.get("checks", {})
        if result.get("passed") is not True or set(checks) != CHECK_NAMES or any(value is not True for value in checks.values()):
            raise ValueError("Runtime preflight did not pass every isolation check")
    return proof


def scripted_provider(calls):
    """Serve prescribed Anthropic streaming messages; no upstream exists."""
    received = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path.split("?")[0].endswith("/count_tokens"):
                payload = json.dumps({"input_tokens": 100}).encode()
                self.send_response(200); self.end_headers(); self.wfile.write(payload)
                return
            if self.path.split("?")[0] != "/v1/messages":
                self.send_error(404)
                return
            for message in body.get("messages", []):
                for block in message.get("content", []) if isinstance(message.get("content"), list) else []:
                    if block.get("type") == "tool_result" and block.get("tool_use_id") not in {r["tool_use_id"] for r in received}:
                        received.append(block)
            index = len(received)
            block = ({"type": "tool_use", "id": f"toolu_runtime_{index}", **calls[index]}
                     if index < len(calls) else {"type": "text", "text": "Deterministic runtime preflight complete."})
            start_block = {**block, "input": {}} if block["type"] == "tool_use" else {**block, "text": ""}
            delta = ({"type": "input_json_delta", "partial_json": json.dumps(block["input"])}
                     if block["type"] == "tool_use" else {"type": "text_delta", "text": block["text"]})
            events = [
                ("message_start", {"type": "message_start", "message": {"id": f"msg_runtime_{index}", "type": "message", "role": "assistant", "model": body["model"], "content": [], "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 100, "output_tokens": 0}}}),
                ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": start_block}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": delta}),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use" if block["type"] == "tool_use" else "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 25}}),
                ("message_stop", {"type": "message_stop"}),
            ]
            payload = "".join(f"event: {name}\ndata: {json.dumps(value)}\n\n" for name, value in events).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, received


def run_preflight(data, active):
    runner.validate_paths(data, active, compact=True)
    if (data / "collection_stop.json").exists() or (data / "trials").exists():
        raise ValueError("Preflight requires a fresh, unstarted collection")
    data.mkdir(mode=0o700, exist_ok=True)
    data.chmod(0o700)
    output = data / "runtime-preflight"
    if output.exists() or (data / "runtime-preflight.json").exists():
        raise ValueError("Preserve prior preflight evidence; use a fresh preflight directory")
    output.mkdir(mode=0o700)
    if active.exists():
        raise ValueError("Preflight requires an absent active root; do not overwrite existing evidence")
    active.mkdir(mode=0o700)
    (active / ".pilot-owned").write_text("runtime-preflight\n")
    work, state = active / "work", active / "state"
    work.mkdir(); state.mkdir(); (active / "scratch").mkdir()
    (work / ".engram").mkdir()
    (work / "allowed.txt").write_text("RUNTIME_ALLOWED\n")
    (work / "smoke.py").write_text("def runtime_smoke():\n    return True\n")
    config = state / "config.toml"
    config.write_text(runner.config_text(state / "brain.sqlite", state / "d.sock"))
    (work / ".engram/config.toml").write_text(config.read_text())
    private = [output / "private-canary.txt", state / "private-canary.txt"]
    for path in private:
        path.write_text("PRIVATE_RUNTIME_CANARY\n")
    # Capture production controls before temporary sibling enumeration changes
    # the executed policy. Their cleanup must restore this exact identity.
    observed_identity = identity(data, active)
    sibling = Path(tempfile.mkdtemp(prefix="erp26work-preflight-sibling-", dir="/private/tmp"))
    (sibling / "private.txt").write_text("PRIVATE_RUNTIME_CANARY\n")
    # Another session's Claude temp root: the static claude-* denial must cover it
    # without the policy (or the identity) changing when it appears.
    claude_canary = Path(tempfile.mkdtemp(prefix="claude-runtime-canary-", dir="/private/tmp"))
    (claude_canary / "private.txt").write_text("PRIVATE_RUNTIME_CANARY\n")
    write_probe = "runtime-preflight-write-probe-" + uuid.uuid4().hex
    private += [sibling / "private.txt", claude_canary / "private.txt", runner.ROOT / "Cargo.toml"]
    # Generated after policy construction below: compact globs must cover future entries.
    future = Path("/private/tmp") / ("engram-runtime-canary-" + uuid.uuid4().hex)
    private.append(future)
    (work / "private-link").symlink_to(private[0])
    private += [Path(str(private[0]).replace("/private/tmp/", "/tmp/", 1)), work / "private-link"]
    stress_paths = []
    stress_prefix = "engram-runtime-stress-" + uuid.uuid4().hex + "-"
    try:
        before_stress = runner.settings(work, data, active, state, "off", compact=True)
        for _ in range(320):
            descriptor, name = tempfile.mkstemp(prefix=stress_prefix, dir="/private/tmp")
            os.close(descriptor)
            stress_paths.append(Path(name))
        after_stress = runner.settings(work, data, active, state, "off", compact=True)
        private += [stress_paths[0], stress_paths[-1]]
        for arm in ("off", "brain"):
            settings = runner.settings(work, data, active, state, arm, compact=True)
            settings_path = state / "settings.json"
            runner.write_json(settings_path, settings)
            runner.write_json(output / f"{arm}.settings.json", settings)
            with future.open("x") as stream:
                stream.write("PRIVATE_RUNTIME_CANARY\n")
            calls = [{"name": "Read", "input": {"file_path": str(work / "allowed.txt")}}]
            calls += [{"name": "Read", "input": {"file_path": str(path)}} for path in private]
            script = (
                "import glob, os, pathlib\n"
                f"denied = {list(map(str, private))!r}\n"
                f"stress = glob.glob('/private/tmp/{stress_prefix}*')\n"
                "assert len(stress) == 320\n"
                "denied += stress\n"
                "for name in denied:\n"
                "    try: pathlib.Path(name).read_bytes()\n"
                "    except PermissionError: pass\n"
                "    else: raise AssertionError('private path readable: ' + name)\n"
                "assert pathlib.Path('allowed.txt').read_text().strip() == 'RUNTIME_ALLOWED'\n"
                "pathlib.Path('bash-write.txt').write_text('RUNTIME_BASH_OK')\n"
                "assert os.environ.get('SANDBOX_RUNTIME') == '1'\n"
                "import tempfile\n"
                f"private_tmp = {str(runner.claude_tmp_target(active).resolve())!r}\n"
                "tmp = os.environ.get('TMPDIR', '')\n"
                "assert os.path.realpath(tmp).startswith(private_tmp + '/') or os.path.realpath(tmp) == private_tmp, 'TMPDIR outside the private Claude temp: ' + tmp\n"
                "descriptor, name = tempfile.mkstemp()\n"
                "os.close(descriptor)\n"
                "os.unlink(name)\n"
                f"for root in {[str(claude_canary), str(SHARED_CLAUDE_TMP)]!r}:\n"
                "    if not os.path.isdir(root): continue\n"
                "    try: os.listdir(root)\n"
                "    except PermissionError: pass\n"
                "    else: raise AssertionError('shared Claude temp listable: ' + root)\n"
                "    try: open(os.path.join(root, " + repr(write_probe) + "), 'x').close()\n"
                "    except PermissionError: pass\n"
                "    else: raise AssertionError('shared Claude temp writable: ' + root)\n"
                "print('RUNTIME_PRIVATE_TMP_OK')\n"
                "print('RUNTIME_BASH_OK')\n"
            )
            command = ("set -e\npython3 - <<'RUNTIME_PREFLIGHT'\n" + script + "RUNTIME_PREFLIGHT\n"
                       "git --version && python3 --version && node --version && rustc --version\n")
            calls.append({"name": "Bash", "input": {"command": command, "timeout": 10000}})
            calls.append({"name": "Bash", "input": {"command": "python3 -c 'import os; print(\"OVERRIDE_REMAINS_SANDBOXED\" if os.environ.get(\"SANDBOX_RUNTIME\") == \"1\" else \"OVERRIDE_UNSANDBOXED\")'", "dangerouslyDisableSandbox": True, "timeout": 10000}})
            server, received = scripted_provider(calls)
            env = runner.coding_env(work, compact=True)
            # A fresh config directory avoids credential reads. The fake endpoint
            # is the only provider; all tool subprocesses retain production policy.
            auth = output / (arm + "-fake-provider-config")
            auth.mkdir()
            env.update({"ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
                        "ANTHROPIC_API_KEY": "synthetic-local-preflight-key", "CLAUDE_CONFIG_DIR": str(auth),
                        "DISABLE_AUTOUPDATER": "1", "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1",
                        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"})
            manager = runner.daemon(config, state) if arm == "brain" else contextlib.nullcontext()
            runner.prepare_claude_tmp(active)
            started = time.monotonic()
            try:
                with manager:
                    if arm == "brain":
                        runner.cli(state, "index", work, "--json")
                    argv = runner.agent_command(settings_path, 1.0)
                    with (output / f"{arm}.trace.jsonl").open("w") as trace, (output / f"{arm}.stderr.log").open("w") as stderr:
                        run = subprocess.run(argv, cwd=work, env=env, input="Run the prescribed local runtime checks.", text=True,
                                             stdout=trace, stderr=stderr, timeout=180)
            finally:
                server.shutdown(); server.server_close()
                runner.release_claude_tmp(active)
            runner.write_json(output / f"{arm}.tool-results.json", received)
            private_claude = runner.claude_tmp_target(active) / f"claude-{os.getuid()}"
            claude_tmp = {"private_entries": sorted(path.name for path in private_claude.iterdir()) if private_claude.is_dir() else [],
                          "shared_entries_for_this_run": shared_claude_tmp_traces(work, session_ids(output / f"{arm}.trace.jsonl")),
                          "shared_write_probe_created": any((root / write_probe).exists() for root in (claude_canary, SHARED_CLAUDE_TMP))}
            text = lambda row: json.dumps(row.get("content", ""))
            checks = {
                "cli_exit": run.returncode == 0, "exact_tool_count": len(received) == len(calls),
                "active_read": bool(received) and not received[0].get("is_error", False) and "RUNTIME_ALLOWED" in text(received[0]),
                "private_read_denials": all(row.get("is_error") is True for row in received[1:1 + len(private)]),
                "sandboxed_bash": len(received) >= 2 and received[-2].get("is_error") is not True and "RUNTIME_BASH_OK" in text(received[-2]),
                "unsandboxed_bypass_closed": len(received) >= 1 and (received[-1].get("is_error") is True or "OVERRIDE_REMAINS_SANDBOXED" in text(received[-1])),
                "temp_inventory_stress": before_stress == after_stress and len(stress_paths) == 320,
                "private_claude_tmp": len(received) >= 2 and "RUNTIME_PRIVATE_TMP_OK" in text(received[-2]) and private_claude.is_dir(),
                "shared_claude_tmp_unused": not claude_tmp["shared_entries_for_this_run"] and not claude_tmp["shared_write_probe_created"],
            }
            runner.write_json(output / f"{arm}.checks.json", {"passed": all(checks.values()), "checks": checks, "claude_tmp": claude_tmp,
                                                            "elapsed_seconds": time.monotonic() - started})
            if not all(checks.values()):
                raise RuntimeError(f"{arm} runtime preflight failed; inspect preserved local logs")
            future.unlink()
    finally:
        future.unlink(missing_ok=True)
        for path in stress_paths:
            path.unlink()
        shutil.rmtree(sibling)
        shutil.rmtree(claude_canary)
        (SHARED_CLAUDE_TMP / write_probe).unlink(missing_ok=True)
    if identity(data, active) != observed_identity:
        raise ValueError("Runtime preflight controls changed during tool execution; no passing proof was written")
    proof = {"schema_version": 1, "status": "pass", "identity": observed_identity, "arms": ["off", "brain"],
             "provider": "deterministic loopback stub; no remote model calls or live Max authentication",
             "execution_policy": "Preserved arm settings additionally deny a temporary sibling canary root. "
                                 "The canary is removed before checking the captured production identity again. "
                                 "A temporary claude-* canary root is created after the identity is captured and "
                                 "must leave the policy unchanged.",
             "artifacts": {str(path.relative_to(data)): runner.digest(path) for path in output.glob("*.*") if path.is_file()}}
    runner.write_json(data / "runtime-preflight.json", proof)
    return proof


def supersede(data, active):
    """Keep an earlier preflight's evidence under a dated name and clear the active
    root it left behind, so the preflight can run again (for example from the
    checkout that will launch the collection). Refused once attempts exist."""
    if (data / "manifest.json").exists() or (data / "trials").exists() or (data / "records.jsonl").exists():
        raise ValueError("A runtime preflight cannot be superseded after the collection has started")
    previous = [path for path in (data / "runtime-preflight", data / "runtime-preflight.json") if path.exists()]
    if active.exists() and (active / ".pilot-owned").read_text() != "runtime-preflight\n":
        raise ValueError("The active root was not left by a runtime preflight; inspect it before rerunning")
    if previous:
        archive = data / ("runtime-preflight.superseded-" + time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()))
        archive.mkdir(mode=0o700)
        for path in previous:
            path.rename(archive / path.name)
    if active.exists():
        shutil.rmtree(active)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data", required=True, type=Path)
    parser.add_argument("--active", required=True, type=Path)
    parser.add_argument("--engram-release", required=True, type=Path)
    parser.add_argument("--supersede", action="store_true",
                        help="keep an earlier preflight's evidence as runtime-preflight.superseded-<time> and clear "
                             "the active root it left, then run again; refused once attempts exist")
    args = parser.parse_args()
    runner.validate_paths(args.data, args.active, compact=True)
    runner.ENGRAM = (args.engram_release / "bin/engram").resolve(strict=True)
    runner.DAEMON = (args.engram_release / "bin/engramd").resolve(strict=True)
    if args.supersede:
        supersede(args.data.resolve(), args.active.resolve())
    run_preflight(args.data.resolve(), args.active.resolve())
    print(json.dumps({"status": "pass", "proof": str(args.data / "runtime-preflight.json")}))


if __name__ == "__main__":
    main()
