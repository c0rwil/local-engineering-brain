"""Codex arm: transcript accounting, isolated configuration, hook vetting, and fake-CLI trials.

Hermetic: a fake `codex` executable writes a synthetic rollout transcript; no
model, no network, no real credentials and no pre-existing daemon are used.
"""
import contextlib
import http.client
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import textwrap
import tomllib
from types import SimpleNamespace
import unittest
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))
sys.dont_write_bytecode = True
import real_repo_codex as codex  # noqa: E402
import real_repo_metrics as metrics  # noqa: E402
import preflight_real_repo_codex as preflight  # noqa: E402
import run_real_repo_followup as followup  # noqa: E402

runner = codex.runner


def usage_row(input_tokens, cached, output, reasoning=None, cache_write=0, drop=()):
    usage = {"input_tokens": input_tokens, "cached_input_tokens": cached, "cache_write_input_tokens": cache_write,
             "output_tokens": output, "total_tokens": input_tokens + output}
    if reasoning is not None:
        usage["reasoning_output_tokens"] = reasoning
    for name in drop:
        usage.pop(name, None)
    return {"type": "event_msg", "payload": {"type": "token_count",
                                             "info": {"total_token_usage": usage, "last_token_usage": usage}}}


def session_meta(session="session-1", subagent=False):
    payload = {"id": session, "model_provider": "openai"}
    if subagent:
        payload["source"] = {"subagent": {"thread_spawn": {"depth": 1}}}
    return {"type": "session_meta", "payload": payload}


def turn_context(model="pinned-model"):
    return {"type": "turn_context", "payload": {"model": model, "cwd": "/synthetic"}}


def tool_call(call_id, name="exec_command", kind="function_call"):
    payload = {"type": kind, "name": name, "arguments": "{}"}
    if call_id is not None:
        payload["call_id"] = call_id
    return {"type": "response_item", "payload": payload}


def usage_record(response_id, input_tokens, cached, output):
    return {"type": "token_usage_record", "payload": {"response_id": response_id, "usage": {
        "input_tokens": input_tokens, "cached_input_tokens": cached, "output_tokens": output}}}


def completed(input_tokens=3000, cached=1200, output=100):
    return {"type": "turn.completed", "usage": {"input_tokens": input_tokens, "cached_input_tokens": cached,
                                                "output_tokens": output}}


def write_jsonl(path, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join((row if isinstance(row, str) else json.dumps(row)) + "\n" for row in rows))
    return path


class CodexTranscriptTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="codex-transcript-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def parse(self, sessions, events=({"type": "thread.started", "thread_id": "t"}, completed())):
        paths = [write_jsonl(self.root / f"rollout-{index}.jsonl", rows) for index, rows in enumerate(sessions)]
        return metrics.parse_codex_trace(write_jsonl(self.root / "events.jsonl", events), paths)

    def test_cumulative_rows_become_deltas_with_claude_field_meaning(self):
        parsed = self.parse([[
            session_meta(), turn_context(),
            usage_row(1000, 400, 50, reasoning=20), usage_row(1000, 400, 50, reasoning=20),
            tool_call("call-1"), tool_call("call-2", "apply_patch", "custom_tool_call"), tool_call("call-1"),
            usage_row(3000, 1200, 100, reasoning=40), usage_record("r1", 1000, 400, 50), usage_record("r2", 2000, 800, 50),
            {"type": "event_msg", "payload": {"type": "task_complete"}},
        ]])
        self.assertEqual(parsed["processed_input_tokens"], 3000)
        self.assertEqual(parsed["cache_read_input_tokens"], 1200)
        self.assertEqual(parsed["input_tokens"], 1800)
        self.assertEqual(parsed["cache_creation_input_tokens"], 0)
        self.assertEqual(parsed["output_tokens"], 100)
        self.assertEqual(parsed["reasoning_output_tokens"], 40)
        self.assertEqual(parsed["total_tokens"], 3100)
        # Shared identity with Claude records: processed = input + cache writes + cache reads.
        self.assertEqual(parsed["input_tokens"] + parsed["cache_creation_input_tokens"] + parsed["cache_read_input_tokens"],
                         parsed["processed_input_tokens"])
        self.assertEqual(parsed["tool_calls"], 2)
        self.assertEqual(parsed["counts_by_tool"], {"apply_patch": 1, "exec_command": 1})
        self.assertEqual(parsed["shell_tool_calls"], 1)
        self.assertTrue(parsed["usage_complete"])
        self.assertTrue(parsed["tool_calls_complete"])
        self.assertEqual(parsed["diagnostics"]["usage_increments"], 2)
        self.assertEqual(parsed["diagnostics"]["unchanged_usage_rows"], 1)
        self.assertIs(parsed["diagnostics"]["usage_sources_agree"], True)
        self.assertEqual(parsed["models"], ["pinned-model"])
        self.assertEqual(parsed["result_subtype"], "success")
        self.assertIs(parsed["is_error"], False)
        self.assertIsNone(parsed["cost_usd_estimate"])
        self.assertEqual(codex.CodexAgent(Path("/x"), "m").record_flags(parsed), {})

    def test_reset_starts_a_new_baseline_without_inventing_usage(self):
        # Mirrors src/usage_capture.rs: (1000, 300, 500) -> reset at (10, 2, 5) -> +25 input, +5 output.
        parsed = self.parse([[usage_row(1000, 500, 300), usage_row(10, 5, 2), usage_row(35, 8, 7)]])
        self.assertEqual(parsed["diagnostics"]["counter_resets"], 1)
        self.assertEqual(parsed["processed_input_tokens"], 1025)
        self.assertEqual(parsed["output_tokens"], 305)
        self.assertEqual(parsed["cache_read_input_tokens"], 503)
        self.assertFalse(parsed["usage_complete"])
        self.assertEqual(codex.CodexAgent(Path("/x"), "m").record_flags(parsed), {"usage_may_be_partial": True})

    def test_counter_missing_from_a_middle_row_is_covered_by_the_next_row(self):
        parsed = self.parse([[usage_row(100, 40, 20), usage_row(0, 0, 25, drop=("input_tokens", "cached_input_tokens")),
                              usage_row(150, 50, 30)]])
        self.assertEqual(parsed["processed_input_tokens"], 150)
        self.assertEqual(parsed["output_tokens"], 30)
        self.assertTrue(parsed["usage_complete"])
        unknown = self.parse([[usage_row(100, 40, 20), usage_row(0, 0, 25, drop=("input_tokens", "cached_input_tokens"))]])
        self.assertIsNone(unknown["processed_input_tokens"])
        self.assertIsNone(unknown["cache_read_input_tokens"])
        self.assertIsNone(unknown["total_tokens"])
        self.assertEqual(unknown["output_tokens"], 25)
        self.assertFalse(unknown["usage_complete"])

    def test_reasoning_is_unknown_when_never_reported(self):
        parsed = self.parse([[usage_row(10, 0, 5, drop=("reasoning_output_tokens",))]])
        self.assertIsNone(parsed["reasoning_output_tokens"])
        self.assertTrue(parsed["usage_complete"])

    def test_missing_usage_is_unknown_not_zero(self):
        parsed = self.parse([[session_meta(), turn_context(), tool_call("call-1")]])
        for name in (*metrics.TOKEN_FIELDS, "reasoning_output_tokens", "processed_input_tokens", "total_tokens"):
            self.assertIsNone(parsed[name], name)
        self.assertFalse(parsed["usage_complete"])
        self.assertIsNone(parsed["num_turns"])
        no_sessions = self.parse([])
        self.assertIsNone(no_sessions["total_tokens"])
        self.assertFalse(no_sessions["tool_calls_complete"])

    def test_tool_calls_without_ids_or_with_conflicts_are_incomplete(self):
        missing = self.parse([[usage_row(1, 0, 1), tool_call("call-1"), tool_call(None)]])
        self.assertEqual(missing["tool_calls"], 1)
        self.assertEqual(missing["diagnostics"]["tool_blocks_without_ids"], 1)
        self.assertFalse(missing["tool_calls_complete"])
        conflict = self.parse([[usage_row(1, 0, 1), tool_call("shared"), tool_call("shared", "view_image")]])
        self.assertEqual(conflict["diagnostics"]["conflicting_tool_ids"], ["shared"])
        self.assertFalse(conflict["tool_calls_complete"])
        shells = self.parse([[usage_row(1, 0, 1), tool_call("a", "exec"), {"type": "response_item", "payload": {
            "type": "local_shell_call", "call_id": "b"}}]])
        self.assertEqual(shells["counts_by_tool"], {"exec": 1, "local_shell_call": 1})
        self.assertEqual(shells["shell_tool_calls"], 2)

    def test_subagent_sessions_are_summed_with_separate_baselines(self):
        parsed = self.parse([
            [session_meta("parent"), turn_context(), usage_row(1000, 400, 50), tool_call("p1")],
            [session_meta("child", subagent=True), turn_context(), usage_row(300, 100, 20), tool_call("c1")],
        ])
        self.assertEqual(parsed["processed_input_tokens"], 1300)
        self.assertEqual(parsed["cache_read_input_tokens"], 500)
        self.assertEqual(parsed["output_tokens"], 70)
        self.assertEqual(parsed["tool_calls"], 2)
        self.assertEqual(parsed["diagnostics"]["session_files"], 2)
        self.assertEqual(parsed["diagnostics"]["session_kinds"], {"root": 1, "spawned": 1})

    def test_thread_kinds_mirror_usage_capture(self):
        # The session_meta shapes from src/usage_capture.rs's classification test.
        cases = (
            ({"id": "r", "source": "vscode", "thread_source": "user"}, "root"),
            ({"id": "c", "source": "cli"}, "root"),
            ({"id": "w", "thread_source": "subagent",
              "source": {"subagent": {"thread_spawn": {"parent_thread_id": "r", "depth": 1}}}}, "spawned"),
            ({"id": "g", "thread_source": "guardian_review", "source": {"subagent": {"other": "guardian"}}},
             "internal:guardian"),
            ({"id": "v", "source": {"subagent": "review"}}, "internal:review"),
            ({"id": "m", "source": {"internal": "memory_consolidation"}}, "internal:memory_consolidation"),
            ({"id": "l", "thread_source": "guardian_review"}, "internal:guardian"),
        )
        for payload, kind in cases:
            with self.subTest(payload=payload):
                self.assertEqual(metrics.codex_thread_kind(payload), kind)

    def test_failed_turn_is_a_runtime_error_but_recovered_retries_are_not(self):
        failed = self.parse([[usage_row(10, 0, 1)]], events=[
            {"type": "error", "message": "Connection failed"},
            {"type": "turn.failed", "error": {"message": "Connection failed"}}])
        self.assertIs(failed["is_error"], True)
        self.assertEqual(failed["result_subtype"], "error")
        record = {"task_id": "t", "repetition": 1, "arm": "off", "grade": {"status": "pass"},
                  "agent_exit_code": 1, "metrics": failed}
        self.assertEqual(metrics.classify_record(record)["grade"]["status"], "infrastructure_error")
        recovered = self.parse([[usage_row(10, 0, 1)]], events=[
            {"type": "error", "message": "Reconnecting... 1/5"}, completed(10, 0, 1)])
        self.assertIs(recovered["is_error"], False)
        self.assertEqual(recovered["error_messages"], ["Reconnecting... 1/5"])
        unfinished = self.parse([[usage_row(10, 0, 1)]], events=[{"type": "turn.started"}])
        self.assertFalse(unfinished["trace_complete"])
        self.assertIsNone(unfinished["is_error"])
        self.assertFalse(unfinished["tool_calls_complete"])

    def test_malformed_lines_and_disagreeing_sources_are_visible(self):
        parsed = self.parse([["not json", usage_row(10, 0, 1), usage_record("r1", 99, 0, 1)]])
        self.assertEqual(parsed["diagnostics"]["malformed_session_lines"], 1)
        self.assertFalse(parsed["usage_complete"])
        self.assertFalse(parsed["tool_calls_complete"])
        disagree = self.parse([[usage_row(10, 0, 1), usage_record("r1", 99, 0, 1)]])
        self.assertIs(disagree["diagnostics"]["usage_sources_agree"], False)
        with self.assertRaises(OSError):
            metrics.parse_codex_trace(self.root / "missing-events.jsonl", [])

    def test_preflight_constants_match_the_scripted_stream(self):
        rows, totals = [], {"input": 0, "cached": 0, "output": 0, "reasoning": 0}
        for usage in preflight.SCRIPTED_USAGE:
            totals["input"] += usage["input_tokens"]
            totals["cached"] += usage["input_tokens_details"]["cached_tokens"]
            totals["output"] += usage["output_tokens"]
            totals["reasoning"] += usage["output_tokens_details"]["reasoning_tokens"]
            rows.append(usage_row(totals["input"], totals["cached"], totals["output"], reasoning=totals["reasoning"]))
        parsed = self.parse([rows])
        self.assertEqual({name: parsed[name] for name in preflight.EXPECTED_USAGE}, preflight.EXPECTED_USAGE)

    def test_partial_codex_usage_never_counts_as_savings(self):
        def record(arm, tokens, partial=False):
            parsed = {"total_tokens": tokens, "reasoning_output_tokens": tokens // 10, "tool_calls": 3}
            result = {"task_id": "t", "repetition": 1, "arm": arm, "grade": {"status": "pass"}, "metrics": parsed}
            if partial:
                result["usage_may_be_partial"] = True
            return result
        report = metrics.paired_summary([record("off", 1000), record("brain", 100, partial=True)])
        aggregate = report["both_pass"]["aggregate"]["metrics"]
        self.assertIsNone(aggregate["reasoning_output_tokens"]["comparison"])
        self.assertIsNone(aggregate["total_tokens"]["comparison"])
        self.assertEqual(aggregate["tool_calls"]["pair_count"], 1)
        self.assertEqual(report["all_attempts"]["brain"]["totals_observed"]["reasoning_output_tokens"], 10)
        self.assertEqual(report["all_attempts"]["brain"]["measurement_coverage"]["reasoning_output_tokens"]["partial"], 1)


def installer_document(binary):
    """The hooks document `engram hooks codex --install` writes (see src/agenthooks.rs)."""
    quoted = "'" + str(binary).replace("'", "'\\''") + "'"
    hooks = {}
    for event, (subcommand, timeout) in {
            "SessionStart": ("session-start", 15), "UserPromptSubmit": ("prompt", 30), "PreToolUse": ("pretool", 10),
            "PostToolUse": ("bash", 15), "Stop": ("stop", 15), "SessionEnd": ("session-end", 3)}.items():
        group = {"hooks": [{"type": "command", "command": f"{quoted} hook {subcommand} --codex", "timeout": timeout}]}
        if event == "PreToolUse":
            group = {"matcher": "Read|Edit|Write|Bash", **group}
        hooks[event] = [group]
    return {"hooks": hooks}


ENGRAM_EXAMPLE = "'/opt/example/bin/engram'"
# (event, matcher, handler, sha256 hex): Codex `hooks/list` ground truth, as
# captured in src/codex_trust.rs on roadmap/capture-health (commit a4f63cc).
CODEX_TRUST_VECTORS = (
    ("session_start", None, {"type": "command", "command": f"{ENGRAM_EXAMPLE} hook session-start --codex", "timeout": 15},
     "05f4938e028b498e6195c0fa4197727720dcc3e3c5f934856ba9d2f2d9386a05"),
    ("user_prompt_submit", None, {"type": "command", "command": f"{ENGRAM_EXAMPLE} hook prompt --codex", "timeout": 30},
     "832c3125bf24b8bab303ff6192060c3c8f33bf8573705d3f8c261f9c3af36b78"),
    ("pre_tool_use", "Read|Edit|Write|Bash",
     {"type": "command", "command": f"{ENGRAM_EXAMPLE} hook pretool --codex", "timeout": 10},
     "be099b495f071ca422fe6e594a484d65dc32080741992bac60d9b89dd1178552"),
    ("post_tool_use", None, {"type": "command", "command": f"{ENGRAM_EXAMPLE} hook bash --codex", "timeout": 15},
     "f6d8e3b597a7caaf9275c6e51d6cacd69b8230df6ccd56820b06f2b3b9027435"),
    ("stop", None, {"type": "command", "command": f"{ENGRAM_EXAMPLE} hook stop --codex", "timeout": 15},
     "506e86873046ef403a8a8a9ff8bf6b2995e5c63d73a65e3f94d971dc16949391"),
    ("session_end", None, {"type": "command", "command": f"{ENGRAM_EXAMPLE} hook session-end --codex", "timeout": 3},
     "7589f10cd488ad1784f006a3c41cedcc2d253e2c0f1c3310ee7bbab03a44fadd"),
    ("stop", None, {"type": "command", "command": "/opt/example/engram hook stop --codex"},
     "a027afa1f5a48849560e53f3e8f261fe3ae2313e9d8aa05c5fd631be49d13034"),
    ("stop", None, {"type": "command", "command": "echo second", "timeout": 7, "async": True, "statusMessage": "Second"},
     "28c9f2f0da6c7feeb39d03a32f86f814c0cd52742b0ae18817eb2739dedd3c0e"),
    ("stop", "", {"type": "command", "command": "echo third", "timeout": 9},
     "a0a98263f2c6accff0ea8df10a4d0a5632bb4cf86f9e78bcdf0a86b2608eea71"),
    ("session_start", "startup", {"type": "command", "command": "echo start", "timeout": 4,
                                  "commandWindows": "echo win", "additionalContextLimit": 100},
     "2af7b4dee17e0935f435e15c5765a61d4fe32a725c6cfda57615d95b519dd33c"),
)


FAKE_INSTALLER = """#!{python}
import json, os, sys
from pathlib import Path
home = Path(os.environ["HOME"])
stable = home / ".local/bin/engram"
binary = stable if stable.is_file() else Path(sys.argv[0]).resolve()
assert sys.argv[1:] == ["hooks", "codex", "--install"], sys.argv
quoted = "'" + str(binary).replace("'", "'\\\\''") + "'"
events = {{"SessionStart": ("session-start", 15), "UserPromptSubmit": ("prompt", 30), "PreToolUse": ("pretool", 10),
          "PostToolUse": ("bash", 15), "Stop": ("stop", 15), "SessionEnd": ("session-end", 3)}}
hooks = {{}}
for event, (sub, timeout) in events.items():
    group = {{"hooks": [{{"type": "command", "command": f"{{quoted}} hook {{sub}} --codex", "timeout": timeout}}]}}
    if event == "PreToolUse":
        group = {{"matcher": "Read|Edit|Write|Bash", **group}}
    hooks[event] = [group]
target = Path(os.environ["CODEX_HOME"]) / "hooks.json"
target.write_text(json.dumps({{"hooks": hooks}}))
print("Wrote", target)
"""


def make_executable(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o755)
    return path


class CodexHookTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="codex-hooks-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.engram = self.root / "release/bin/engram"
        self.engram.parent.mkdir(parents=True)

    def test_verify_hooks_accepts_only_the_installed_engram_handlers(self):
        document = installer_document(self.engram)
        summary = codex.verify_hooks(document, self.engram)
        self.assertEqual(set(summary), set(codex.HOOK_EVENTS))
        self.assertEqual(summary["PreToolUse"]["matcher"], "Read|Edit|Write|Bash")
        variants = []
        foreign = json.loads(json.dumps(document))
        foreign["hooks"]["Stop"][0]["hooks"].append({"type": "command", "command": "echo foreign"})
        variants.append(foreign)
        variants.append(installer_document(self.root / "other/engram"))
        missing_flag = json.loads(json.dumps(document))
        missing_flag["hooks"]["Stop"][0]["hooks"][0]["command"] = f"'{self.engram}' hook stop"
        variants.append(missing_flag)
        variants.append({**document, "description": "extra top-level content"})
        extra_event = json.loads(json.dumps(document))
        extra_event["hooks"]["PreCompact"] = document["hooks"]["Stop"]
        variants.append(extra_event)
        for variant in variants:
            with self.subTest(variant=json.dumps(variant)[:80]), self.assertRaises(ValueError):
                codex.verify_hooks(variant, self.engram)

    def test_installer_runs_with_a_private_home_and_records_the_pinned_binary(self):
        make_executable(self.engram, FAKE_INSTALLER.format(python=sys.executable))
        codex_home, hook_home = self.root / "codex-home", self.root / "hook-home"
        codex_home.mkdir()
        # A real ~/.local/bin/engram must not leak into the trial's hooks.
        decoy = Path.home() / ".local/bin/engram"
        with mock.patch.object(runner, "ENGRAM", self.engram):
            summary = codex.install_hooks(codex_home, hook_home)
        self.assertEqual(summary["SessionStart"]["command"], ["hook", "session-start", "--codex"])
        self.assertNotIn(str(decoy), (codex_home / "hooks.json").read_text())
        self.assertTrue(hook_home.is_dir())

    @unittest.skipUnless((SCRIPTS.parent / "target/debug/engram").is_file(), "debug engram binary not built")
    def test_real_installer_output_passes_verification(self):
        binary = (SCRIPTS.parent / "target/debug/engram").resolve()
        codex_home = self.root / "codex-home"
        codex_home.mkdir()
        with mock.patch.object(runner, "ENGRAM", binary):
            summary = codex.install_hooks(codex_home, self.root / "hook-home")
        self.assertEqual(summary["PostToolUse"]["command"], ["hook", "bash", "--codex"])

    def test_trust_hash_matches_codex_hooks_list_vectors(self):
        # Shared with src/codex_trust.rs (roadmap/capture-health): Codex CLI
        # 0.155.0-alpha.2.6 app-server `hooks/list` currentHash values.
        for event, matcher, handler, expected in CODEX_TRUST_VECTORS:
            with self.subTest(event=event, handler=handler):
                self.assertEqual(codex.hook_trust_hash(event, matcher, handler), f"sha256:{expected}")
        self.assertIsNone(codex.hook_trust_hash("stop", None, {"type": "prompt"}))
        self.assertIsNone(codex.hook_trust_hash("stop", None, {"type": "command", "command": "x", "timeout": -1}))
        self.assertEqual([codex.event_snake_case(name) for name in ("SessionStart", "PreToolUse", "UserPromptSubmit", "Stop")],
                         ["session_start", "pre_tool_use", "user_prompt_submit", "stop"])

    def test_trust_entries_cover_every_installed_handler_with_canonical_keys(self):
        active = self.root / "active"
        home = active / "codex-home"
        home.mkdir(parents=True)
        (active / ".pilot-owned").write_text("unit\n")
        (home / "hooks.json").write_text(json.dumps(installer_document("/opt/example/bin/engram")))
        link = self.root / "linked-home"
        link.symlink_to(home, target_is_directory=True)
        entries = codex.codex_hook_trust_entries(link)
        canonical = home.resolve() / "hooks.json"
        expected = {f"{canonical}:{event}:0:0": f"sha256:{digest}"
                    for event, _, _, digest in CODEX_TRUST_VECTORS[:6]}
        self.assertEqual(entries, expected)

    def test_trust_is_never_written_outside_a_runner_owned_home(self):
        unowned = self.root / "unowned/codex-home"
        unowned.mkdir(parents=True)
        (unowned / "hooks.json").write_text(json.dumps(installer_document(self.engram)))
        with self.assertRaisesRegex(ValueError, "never ~/.codex"):
            codex.codex_hook_trust_entries(unowned)
        fake_home = self.root / "user"
        (fake_home / ".codex").mkdir(parents=True)
        (fake_home / ".pilot-owned").write_text("decoy\n")
        (fake_home / ".codex/hooks.json").write_text(json.dumps(installer_document(self.engram)))
        with mock.patch.object(codex.Path, "home", return_value=fake_home), \
             self.assertRaisesRegex(ValueError, "never ~/.codex"):
            codex.codex_hook_trust_entries(fake_home / ".codex")
        owned = self.root / "owned"
        (owned / "codex-home").mkdir(parents=True)
        (owned / ".pilot-owned").write_text("unit\n")
        document = installer_document(self.engram)
        document["hooks"]["Stop"][0]["hooks"][0] = {"type": "prompt", "prompt": "synthetic"}
        (owned / "codex-home/hooks.json").write_text(json.dumps(document))
        with self.assertRaisesRegex(ValueError, "not a command handler"):
            codex.codex_hook_trust_entries(owned / "codex-home")


class CodexConfigTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="codex-config-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.work, self.state = self.root / "active/work", self.root / "active/state"
        self.denied = [self.root / "data", self.root / "private"]
        self.globs = ["/private/tmp/erp26-*", "/private/tmp/erp26-*/**"]

    def config(self, arm, model="pinned-model", effort="high"):
        text = codex.config_text(arm=arm, model=model, effort=effort, work=self.work, denied=self.denied,
                                 globs=self.globs, state=self.state,
                                 engram_mcp=self.root / "release/bin/engram-mcp" if arm == "brain" else None)
        return tomllib.loads(text)

    def test_arms_differ_only_in_the_hooks_feature_and_engram_mcp(self):
        off, brain = self.config("off"), self.config("brain")
        self.assertIs(off["features"]["hooks"], False)
        self.assertIs(brain["features"]["hooks"], True)
        self.assertNotIn("mcp_servers", off)
        self.assertEqual(set(brain["mcp_servers"]), {"engram"})
        for value in (off, brain):
            value["features"].pop("hooks")
            value.pop("mcp_servers", None)
        self.assertEqual(off, brain)

    def test_shared_settings_pin_the_model_and_mirror_claude_isolation(self):
        value = self.config("off")
        self.assertEqual((value["model"], value["model_reasoning_effort"]), ("pinned-model", "high"))
        self.assertEqual(value["approval_policy"], "never")
        self.assertIs(value["allow_login_shell"], False)
        self.assertEqual(value["web_search"], "disabled")
        self.assertEqual(value["history"]["persistence"], "none")
        for name in codex.DISABLED_FEATURES:
            self.assertIs(value["features"][name], False, name)
        profile = value["permissions"][value["default_permissions"]]
        self.assertEqual(profile["extends"], ":workspace")
        filesystem = profile["filesystem"]
        self.assertEqual(filesystem[":slash_tmp"], "read")
        self.assertEqual(filesystem[str(self.work / ".engram")], "read")
        for path in (*map(str, self.denied), *self.globs):
            self.assertEqual(filesystem[path], "deny", path)
        self.assertIs(profile["network"]["enabled"], False)
        policy = value["shell_environment_policy"]
        self.assertEqual(policy["set"], {"HOME": str(Path.home())})
        self.assertTrue({"CODEX_API_KEY", "OPENAI_API_KEY", "ENGRAM_DB_PATH", "ENGRAM_SOCKET_PATH"} <= set(policy["exclude"]))

    def test_trusted_hooks_render_as_hooks_state_tables(self):
        key = f"{self.root}/active/codex-home/hooks.json:session_start:0:0"
        text = codex.config_text(arm="brain", model="m", effort="high", work=self.work, denied=self.denied,
                                 globs=self.globs, state=self.state, engram_mcp=self.root / "engram-mcp",
                                 trusted_hooks={key: "sha256:" + "a" * 64})
        value = tomllib.loads(text)
        self.assertEqual(value["hooks"]["state"], {key: {"trusted_hash": "sha256:" + "a" * 64}})
        self.assertNotIn("hooks", self.config("off"))
        self.assertNotIn("hooks", self.config("brain"))

    def test_brain_mcp_uses_explicit_state_and_never_autostarts(self):
        server = self.config("brain")["mcp_servers"]["engram"]
        self.assertEqual(server["command"], str(self.root / "release/bin/engram-mcp"))
        args = server["args"]
        self.assertEqual(args[args.index("--db") + 1], str(self.state / "brain.sqlite"))
        self.assertEqual(args[args.index("--socket") + 1], str(self.state / "d.sock"))
        self.assertIn("--no-autostart", args)

    def test_private_paths_cover_evidence_sources_agent_state_and_other_trials(self):
        active = self.root / "erp26work-unit-active"
        sibling = Path("/private/tmp/erp26work-unit-sibling")
        original_glob = Path.glob

        def inventory(path, pattern):
            if path == Path("/private/tmp"):
                return iter([sibling, active])
            return original_glob(path, pattern)

        with mock.patch.object(Path, "glob", new=inventory):
            exact, globs = codex.private_paths(self.work, self.root / "data", active, self.state,
                                               active / "codex-home", active / "codex-user-home")
        expected = {self.root / "data", runner.ROOT.resolve(), runner.SOURCE_ROOT.resolve(), Path.home() / ".engram",
                    Path.home() / ".codex", Path.home() / ".claude", self.state, active / "codex-home",
                    active / "codex-user-home", sibling.resolve()}
        self.assertTrue(expected <= set(exact))
        self.assertNotIn(active.resolve(), exact)
        for prefix in ("engram-", "erp26-", "claude-"):
            self.assertIn(f"/private/tmp/{prefix}*", globs)
            self.assertIn(f"/private/tmp/{prefix}*/**", globs)

    def test_config_template_digest_tracks_model_and_effort(self):
        digest = codex.config_template_sha256("m", "high")
        self.assertEqual(digest, codex.config_template_sha256("m", "high"))
        self.assertNotEqual(digest, codex.config_template_sha256("other", "high"))
        self.assertNotEqual(digest, codex.config_template_sha256("m", "medium"))


class CodexDiscoveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="codex-discovery-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.nothing = mock.patch.multiple(codex, APP_CANDIDATES=(), RUNTIME_CACHE=self.root / "no-cache")
        self.which = mock.patch.object(codex.shutil, "which", return_value=None)
        self.mdfind = mock.patch.object(codex.subprocess, "run", return_value=SimpleNamespace(stdout=""))

    def executable(self, path):
        path.parent.mkdir(parents=True, exist_ok=True)
        return make_executable(path, "#!/bin/sh\nexit 0\n")

    def test_explicit_path_must_be_executable(self):
        binary = self.executable(self.root / "explicit/codex")
        self.assertEqual(codex.find_codex(binary), binary)
        plain = self.root / "plain-file"
        plain.write_text("not executable")
        with self.assertRaisesRegex(ValueError, "not executable"):
            codex.find_codex(plain)
        with self.assertRaises(OSError):
            codex.find_codex(self.root / "missing")

    def test_discovery_order_path_runtime_cache_bundle_then_spotlight(self):
        on_path = self.executable(self.root / "path/codex")
        cached = self.executable(self.root / "cache/runtime/bin/codex")
        bundled = self.executable(self.root / "Bundle.app/Contents/Resources/codex")
        spotlight = self.executable(self.root / "Found.app/Contents/Resources/codex")
        with self.nothing, self.mdfind:
            with mock.patch.object(codex.shutil, "which", return_value=str(on_path)):
                self.assertEqual(codex.find_codex(), on_path)
            with self.which, mock.patch.object(codex, "RUNTIME_CACHE", self.root / "cache"):
                self.assertEqual(codex.find_codex(), cached)
            with self.which, mock.patch.object(codex, "APP_CANDIDATES", (bundled,)):
                self.assertEqual(codex.find_codex(), bundled)
        found = SimpleNamespace(stdout=str(self.root / "Found.app") + "\n")
        with self.nothing, self.which, mock.patch.object(codex.subprocess, "run", return_value=found):
            self.assertEqual(codex.find_codex(), spotlight)
        with self.nothing, self.which, self.mdfind, self.assertRaisesRegex(FileNotFoundError, "--codex"):
            codex.find_codex()


class CodexAdapterTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="codex-adapter-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.active = self.root / "active"
        self.prepared = {"arm": "off", "trial": self.root / "trial", "work": self.active / "work",
                         "state": self.active / "state", "codex_home": self.active / "codex-home",
                         "user_home": self.active / "codex-user-home", "scratch": self.active / "scratch"}
        self.agent = codex.CodexAgent(self.root / "codex", "pinned-model")

    def test_bypass_flag_only_for_brain_in_bypass_mode(self):
        self.assertEqual(self.agent.hook_trust, "persisted")
        self.assertNotIn("--dangerously-bypass-hook-trust", self.agent.command({**self.prepared, "arm": "brain"}))
        bypassing = codex.CodexAgent(self.root / "codex", "pinned-model", hook_trust="bypass")
        off = bypassing.command(self.prepared)
        brain = bypassing.command({**self.prepared, "arm": "brain"})
        for argv in (off, brain):
            self.assertEqual(argv[1], "exec")
            self.assertEqual(argv[-1], "-")
            for flag in ("--json", "--strict-config", "--ignore-rules"):
                self.assertIn(flag, argv)
            self.assertEqual(argv[argv.index("--cd") + 1], str(self.prepared["work"]))
        self.assertNotIn("--dangerously-bypass-hook-trust", off)
        self.assertIn("--dangerously-bypass-hook-trust", brain)

    def test_environment_is_private_and_passes_only_the_documented_credential(self):
        with mock.patch.dict(os.environ, {"CODEX_API_KEY": "synthetic-key", "OPENAI_API_KEY": "synthetic-other"}):
            env = self.agent.environment(self.prepared, compact=True)
        self.assertEqual(env["CODEX_API_KEY"], "synthetic-key")
        self.assertNotIn("OPENAI_API_KEY", env)
        self.assertEqual(env["HOME"], str(self.prepared["user_home"]))
        self.assertEqual(env["CODEX_HOME"], str(self.prepared["codex_home"]))
        self.assertEqual(env["TMPDIR"], str(self.active / "scratch"))
        self.assertFalse([key for key in env if key.startswith("CLAUDE_CODE_")])
        self.assertNotIn("ENGRAM_DB_PATH", env)
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("CODEX_API_KEY", None)
            brain = self.agent.environment({**self.prepared, "arm": "brain"}, compact=True)
            with self.assertRaisesRegex(ValueError, "CODEX_API_KEY"):
                self.agent.require_credentials()
        self.assertNotIn("CODEX_API_KEY", brain)
        self.assertEqual(brain["ENGRAM_DB_PATH"], str(self.prepared["state"] / "brain.sqlite"))
        self.assertEqual(brain["ENGRAM_SOCKET_PATH"], str(self.prepared["state"] / "d.sock"))

    def test_project_codex_configuration_is_refused(self):
        work = self.root / "checkout"
        work.mkdir()
        codex.validate_checkout(work)
        (work / ".codex").mkdir()
        with self.assertRaisesRegex(ValueError, ".codex"):
            codex.validate_checkout(work)

    def test_limits_record_no_cost_cap(self):
        self.assertEqual(self.agent.limits(SimpleNamespace(cost_cap=8)),
                         {"max_budget_usd_estimate": None, "model": "pinned-model", "effort": "high", "agent": "codex"})
        with self.assertRaises(ValueError):
            codex.CodexAgent(self.root / "codex", "")
        with self.assertRaises(ValueError):
            codex.CodexAgent(self.root / "codex", "m", hook_trust="unknown")


FAKE_CODEX = """#!{python}
\"\"\"Synthetic Codex CLI: writes a rollout transcript and JSON events; no model.\"\"\"
import json, os, sys, tomllib
from pathlib import Path
REPORTED_MODEL = {reported_model!r}
RESET = {reset!r}
home = Path(os.environ["CODEX_HOME"])
config = tomllib.loads((home / "config.toml").read_text())
model = REPORTED_MODEL or config["model"]
prompt = sys.stdin.read()
work = Path(sys.argv[sys.argv.index("--cd") + 1])
(work / "feature.txt").write_text("synthetic change\\n")
(home / "auth.json").write_text("synthetic credential file that must not be preserved")
(home / "observations.json").write_text(json.dumps({{
    "argv": sys.argv[1:], "prompt_chars": len(prompt), "cwd": os.getcwd(),
    "home": os.environ.get("HOME"), "has_api_key": "CODEX_API_KEY" in os.environ,
    "engram_db": os.environ.get("ENGRAM_DB_PATH"), "claude_vars": sorted(k for k in os.environ if k.startswith("CLAUDE_CODE_")),
    "hooks_feature": config["features"]["hooks"], "mcp": sorted(config.get("mcp_servers", {{}})),
    "trusted_hooks": sorted(config.get("hooks", {{}}).get("state", {{}})),
}}))
def usage(i, c, o, r):
    return {{"type": "event_msg", "payload": {{"type": "token_count", "info": {{"total_token_usage": {{
        "input_tokens": i, "cached_input_tokens": c, "cache_write_input_tokens": 0, "output_tokens": o,
        "reasoning_output_tokens": r, "total_tokens": i + o}}}}}}}}
rows = [
    {{"type": "session_meta", "payload": {{"id": "fake-session", "model_provider": "openai"}}}},
    {{"type": "turn_context", "payload": {{"model": model}}}},
    usage(1000, 400, 50, 20),
    {{"type": "response_item", "payload": {{"type": "function_call", "name": "exec_command", "call_id": "call-1", "arguments": "{{}}"}}}},
    usage(10, 0, 5, 1) if RESET else usage(1000, 400, 50, 20),
    {{"type": "response_item", "payload": {{"type": "custom_tool_call", "name": "apply_patch", "call_id": "call-2", "input": ""}}}},
    usage(30, 10, 9, 3) if RESET else usage(3000, 1200, 100, 40),
]
rollout = home / "sessions/2026/10/04/rollout-2026-10-04T00-00-00-fake.jsonl"
rollout.parent.mkdir(parents=True)
rollout.write_text("".join(json.dumps(row) + "\\n" for row in rows))
for event in ({{"type": "thread.started", "thread_id": "fake-thread"}}, {{"type": "turn.started"}},
              {{"type": "turn.completed", "usage": {{"input_tokens": 3000, "cached_input_tokens": 1200, "output_tokens": 100}}}}):
    print(json.dumps(event), flush=True)
"""

GRADER = """import argparse, json
from pathlib import Path
parser = argparse.ArgumentParser()
parser.add_argument("--repo", type=Path)
parser.add_argument("--output", type=Path)
args = parser.parse_args()
passed = (args.repo / "feature.txt").is_file()
args.output.write_text(json.dumps({"status": "pass" if passed else "fail", "checks": [{"name": "feature", "passed": passed}]}))
"""


class FakeCodexTrialTests(unittest.TestCase):
    """run_trial end to end with a fake Codex CLI and a synthetic task."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="codex-trial-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.data, self.active = self.root / "data", self.root / "active"
        suite = self.root / "suite"
        (suite / "prompts").mkdir(parents=True)
        (suite / "prompts/synthetic.md").write_text("Add feature.txt.\n")
        (suite / "grading/synthetic").mkdir(parents=True)
        (suite / "grading/synthetic/grade.py").write_text(GRADER)
        fixture = self.data / "fixtures/synthetic"
        work = fixture / "work"
        (work / ".engram").mkdir(parents=True)
        (work / ".engram/project.toml").write_text('project_id = "prj_synthetic"\n')
        (work / "README.md").write_text("synthetic\n")
        for command in (["init", "-q"], ["add", "-A"],
                        ["-c", "user.name=t", "-c", "user.email=t@invalid", "commit", "-qm", "base"]):
            subprocess.run(["git", "-C", str(work), *command], check=True)
        (fixture / "snapshot.sqlite").write_bytes(b"synthetic snapshot")
        self.task = {"id": "synthetic", "repository": "synthetic", "prompt_file": "prompts/synthetic.md"}
        suite_patch = mock.patch.object(runner, "SUITE", suite)
        suite_patch.start()
        self.addCleanup(suite_patch.stop)
        # Unrelated live state must not influence the synthetic trials.
        glob_patch = mock.patch.object(codex, "private_paths", side_effect=lambda work, data, active, state, home, user: (
            [data, state, home, user], ["/private/tmp/erp26-*"]))
        glob_patch.start()
        self.addCleanup(glob_patch.stop)

    def agent(self, reported_model=None, reset=False):
        fake = make_executable(self.root / f"codex-{reported_model}-{reset}",
                               FAKE_CODEX.format(python=sys.executable, reported_model=reported_model, reset=reset))
        return codex.CodexAgent(fake, "pinned-model")

    def run_trial(self, arm, agent, repetition=1):
        args = SimpleNamespace(data=self.data, active=self.active, agent_seconds=60, grader_seconds=60, cost_cap=8.0,
                               compact_settings=True, agent_adapter=agent)
        with mock.patch.dict(os.environ, {"CODEX_API_KEY": "synthetic-key"}), contextlib.redirect_stdout(None):
            record = runner.run_trial(self.task, repetition, arm, args)
        trial = self.data / "trials" / record["id"]
        return record, trial, json.loads((trial / "codex-home/observations.json").read_text())

    def test_off_arm_runs_without_hooks_or_mcp_and_preserves_private_evidence(self):
        record, trial, seen = self.run_trial("off", self.agent())
        self.assertEqual(record["grade"]["status"], "pass", record)
        self.assertEqual(record["validity_issues"], [])
        usage = record["metrics"]
        self.assertEqual((usage["processed_input_tokens"], usage["cache_read_input_tokens"], usage["output_tokens"],
                          usage["reasoning_output_tokens"], usage["total_tokens"]), (3000, 1200, 100, 40, 3100))
        self.assertEqual(usage["tool_calls"], 2)
        self.assertNotIn("usage_may_be_partial", record)
        self.assertNotIn("--dangerously-bypass-hook-trust", seen["argv"])
        self.assertIs(seen["hooks_feature"], False)
        self.assertEqual(seen["mcp"], [])
        self.assertTrue(seen["has_api_key"])
        self.assertIsNone(seen["engram_db"])
        self.assertEqual(seen["claude_vars"], [])
        self.assertEqual(seen["home"], str(self.active / "codex-user-home"))
        self.assertFalse((trial / "codex-home/auth.json").exists())
        self.assertTrue(list((trial / "codex-home/sessions").rglob("rollout-*.jsonl")))
        self.assertFalse((trial / "codex-hooks.json").exists())
        self.assertTrue((trial / "candidate/feature.txt").is_file())
        limits = json.loads((trial / "limits.json").read_text())
        self.assertEqual((limits["model"], limits["max_budget_usd_estimate"], limits["agent"]), ("pinned-model", None, "codex"))
        self.assertNotIn("synthetic-key", "".join(path.read_text(errors="replace") for path in trial.rglob("*")
                                                  if path.is_file() and path.suffix in (".json", ".jsonl", ".toml", ".log", ".txt")))

    def brain_trial(self, agent, installer=None):
        engram = make_executable(self.root / "release/bin/engram",
                                 (installer or FAKE_INSTALLER).format(python=sys.executable))
        with mock.patch.object(runner, "ENGRAM", engram), \
             mock.patch.object(runner, "daemon", return_value=contextlib.nullcontext()), \
             mock.patch.object(runner, "cli"), \
             mock.patch.object(runner, "receipt_summary", return_value={"emission_receipts": 1, "compiled_runs": 1}):
            return engram, *self.run_trial("brain", agent)

    def test_brain_arm_trusts_vetted_hooks_and_registers_engram_mcp(self):
        engram, record, trial, seen = self.brain_trial(self.agent())
        self.assertEqual(record["grade"]["status"], "pass", record)
        self.assertNotIn("--dangerously-bypass-hook-trust", seen["argv"])
        self.assertIs(seen["hooks_feature"], True)
        self.assertEqual(seen["mcp"], ["engram"])
        self.assertEqual(seen["engram_db"], str(self.active / "state/brain.sqlite"))
        installed = json.loads((trial / "codex-hooks.json").read_text())
        codex.verify_hooks(installed, engram)
        hooks_file = (self.active / "codex-home").resolve() / "hooks.json"
        self.assertEqual(seen["trusted_hooks"], sorted(
            f"{hooks_file}:{codex.event_snake_case(event)}:0:0" for event in codex.HOOK_EVENTS))
        trust = tomllib.loads((trial / "codex-config.toml").read_text())["hooks"]["state"]
        for event, groups in installed["hooks"].items():
            key = f"{hooks_file}:{codex.event_snake_case(event)}:0:0"
            self.assertEqual(trust[key]["trusted_hash"], codex.hook_trust_hash(
                codex.event_snake_case(event), groups[0].get("matcher"), groups[0]["hooks"][0]))

    def test_bypass_mode_passes_the_flag_and_writes_no_trust(self):
        agent = codex.CodexAgent(self.agent().executable, "pinned-model", hook_trust="bypass")
        _, record, _, seen = self.brain_trial(agent)
        self.assertEqual(record["grade"]["status"], "pass", record)
        self.assertIn("--dangerously-bypass-hook-trust", seen["argv"])
        self.assertEqual(seen["trusted_hooks"], [])

    def test_brain_arm_without_delivery_or_with_model_drift_is_unscorable(self):
        engram = make_executable(self.root / "release/bin/engram", FAKE_INSTALLER.format(python=sys.executable))
        with mock.patch.object(runner, "ENGRAM", engram), \
             mock.patch.object(runner, "daemon", return_value=contextlib.nullcontext()), \
             mock.patch.object(runner, "cli"), \
             mock.patch.object(runner, "receipt_summary", return_value={"emission_receipts": 0, "compiled_runs": 0}):
            record, _, _ = self.run_trial("brain", self.agent(reported_model="other-model"))
        self.assertEqual(record["grade"]["status"], "infrastructure_error")
        self.assertEqual(record["validity_issues"], ["model_drift", "no_delivery_receipt"])
        self.assertEqual(record["behavioral_grade"]["status"], "pass")

    def test_reset_counters_mark_usage_partial(self):
        record, _, _ = self.run_trial("off", self.agent(reset=True))
        self.assertTrue(record["usage_may_be_partial"])
        self.assertEqual(record["metrics"]["diagnostics"]["counter_resets"], 1)

    def test_unexpected_installed_handlers_stop_brain_preparation(self):
        foreign = 'hooks[event] = [group, {{"hooks": [{{"type": "command", "command": "echo foreign"}}]}}]'
        engram = make_executable(self.root / "release/bin/engram", FAKE_INSTALLER.replace(
            "hooks[event] = [group]", foreign).format(python=sys.executable))
        with mock.patch.object(runner, "ENGRAM", engram), contextlib.redirect_stdout(None):
            record = runner.run_trial(self.task, 1, "brain", SimpleNamespace(
                data=self.data, active=self.active, agent_seconds=60, grader_seconds=60, cost_cap=8.0,
                compact_settings=True, agent_adapter=self.agent()))
        self.assertIn("exactly one handler", record["infrastructure_error"])
        self.assertNotIn("metrics", record)
        self.assertEqual(record["grade"]["status"], "infrastructure_error")


class CodexFollowupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="codex-followup-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.catalog = {"pilot_repetitions": 3, "tasks": [{"id": "task"}]}
        self.digests = []

        def digest(path):
            self.digests.append(Path(path))
            return "digest"

        patch = mock.patch.object(followup.runner, "digest", side_effect=digest)
        patch.start()
        self.addCleanup(patch.stop)

    def args(self, **extra):
        return SimpleNamespace(data=self.root, agent_seconds=1200, grader_seconds=1800, cost_cap=8.0, **extra)

    def test_claude_controls_keep_their_original_keys(self):
        with mock.patch.object(followup.runner, "command", return_value="2.1.212 (Claude Code)\n"):
            value = followup.controls(self.args(), self.catalog)
        self.assertEqual(set(value), {
            "suite", "suite_files", "engram_sha256", "daemon_sha256", "claude_sha256", "claude_version", "model",
            "effort", "repetitions", "agent_limit_seconds", "grading_limit_seconds", "cost_cap_usd_estimate",
            "runner_sha256", "runner_library_sha256", "metrics_sha256", "helper_sha256",
            "runtime_preflight_sha256", "fixture_hashes"})
        self.assertEqual(set(value["helper_sha256"]), set(followup.CLAUDE_HELPERS))
        self.assertEqual(value["cost_cap_usd_estimate"], 8.0)
        self.assertIn(self.root / "runtime-preflight.json", self.digests)

    def test_codex_controls_record_agent_identity_and_no_cost_cap(self):
        agent = codex.CodexAgent(self.root / "codex", "pinned-model", "high")
        with mock.patch.object(followup.codex_arm, "version", return_value="codex-cli 0.0.0"):
            value = followup.controls(self.args(agent="codex", agent_adapter=agent), self.catalog)
        self.assertEqual((value["agent"], value["codex_version"], value["model"], value["effort"]),
                         ("codex", "codex-cli 0.0.0", "pinned-model", "high"))
        self.assertEqual(value["codex_hook_trust"], "persisted")
        self.assertEqual(value["codex_config_sha256"], codex.config_template_sha256("pinned-model", "high"))
        self.assertIsNone(value["cost_cap_usd_estimate"])
        self.assertNotIn("claude_sha256", value)
        self.assertEqual(set(value["helper_sha256"]), set(followup.CODEX_HELPERS))
        self.assertIn(self.root / preflight.PROOF, self.digests)

    def test_scope_limits_keep_claude_text_and_state_codex_differences(self):
        claude = followup.scope_limits(self.args())
        self.assertEqual(claude[3], "Single agent, six core tools, source-only hooks. Does not measure learned "
                                    "memories, automatic judging, or MCP tools.")
        self.assertEqual(len(claude), 5)
        text = " ".join(followup.scope_limits(self.args(agent="codex")))
        for phrase in ("no USD estimate", "trusted in each isolated CODEX_HOME", "engram MCP", "do not compare across agents"):
            self.assertIn(phrase, text)
        bypassing = codex.CodexAgent(self.root / "codex", "m", hook_trust="bypass")
        self.assertIn("--dangerously-bypass-hook-trust",
                      " ".join(followup.scope_limits(self.args(agent="codex", agent_adapter=bypassing))))

    def test_codex_records_skip_the_claude_shell_audit(self):
        trace = self.root / "trials/trial/trace.jsonl"
        trace.parent.mkdir(parents=True)
        trace.write_text("{}\n")
        record = {"id": "trial", "grade": {"status": "pass"}}
        args = self.args(agent="codex", agent_adapter=None)
        with mock.patch.object(followup, "controls", return_value={}), \
             mock.patch.object(followup, "frozen_inputs_match", return_value=True), \
             mock.patch.object(followup, "apply_tool_runtime_checks") as audit:
            followup.validate_trial_controls(record, args, {}, {})
        audit.assert_not_called()

    def test_cli_requires_a_codex_model_and_a_known_trust_mode(self):
        base = ["run_real_repo_followup.py", "--engram-release", str(self.root), "--agent", "codex"]
        for argv, message in ((base, "--codex-model is required"),
                              ([*base, "--codex-model", "m", "--codex-hook-trust", "guess"], "invalid choice")):
            stderr = tempfile.TemporaryFile(mode="w+")
            with self.subTest(argv=argv), mock.patch.object(sys, "argv", argv), \
                 contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit):
                followup.main()
            stderr.seek(0)
            self.assertIn(message, stderr.read())
            stderr.close()


class CodexPreflightTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="codex-preflight-")
        self.addCleanup(self.temp.cleanup)
        self.data = Path(self.temp.name).resolve()
        self.identity = {"controls": "fixed"}
        output = self.data / preflight.OUTPUT
        output.mkdir()
        self.proof = {"schema_version": 1, "status": "pass", "identity": self.identity,
                      "arms": list(preflight.ARMS), "artifacts": {}}
        for relative in sorted(preflight.ARTIFACTS):
            path = self.data / relative
            path.write_text(json.dumps({"passed": True, "checks": dict.fromkeys(preflight.CHECK_NAMES, True)})
                            if relative.endswith("checks.json") else "synthetic evidence")
            self.proof["artifacts"][relative] = preflight.runner.digest(path)
        self.save()

    def save(self):
        (self.data / preflight.PROOF).write_text(json.dumps(self.proof))

    def validate(self):
        with mock.patch.object(preflight, "identity", return_value=self.identity):
            return preflight.validate_proof(self.data, self.data / "active", None)

    def test_accepts_only_a_matching_complete_passing_proof(self):
        self.assertEqual(self.validate(), self.proof)
        (self.data / preflight.OUTPUT / "brain.trace.jsonl").write_text("changed")
        with self.assertRaisesRegex(ValueError, "artifact differs"):
            self.validate()

    def test_changed_controls_or_failed_checks_block_collection(self):
        self.proof["identity"] = {"controls": "old"}
        self.save()
        with self.assertRaisesRegex(ValueError, "differs"):
            self.validate()
        self.proof["identity"] = self.identity
        checks = self.data / preflight.OUTPUT / "off.checks.json"
        checks.write_text(json.dumps({"passed": True, "checks": {**dict.fromkeys(preflight.CHECK_NAMES, True),
                                                                 "network_denied": False}}))
        self.proof["artifacts"][str(checks.relative_to(self.data))] = preflight.runner.digest(checks)
        self.save()
        with self.assertRaisesRegex(ValueError, "every check"):
            self.validate()
        (self.data / preflight.PROOF).unlink()
        with self.assertRaises(OSError):
            self.validate()

    def test_probe_result_is_read_from_function_call_output(self):
        line = preflight.PROBE_MARKER + json.dumps({"allowed_read": True})
        for output in (f"Chunk ID: 1\nOutput:\n{line}\n", [{"type": "input_text", "text": f"Output:\n{line}"}]):
            requests = [{"input": [{"type": "message"}]},
                        {"input": [{"type": "function_call_output", "call_id": "c", "output": output}]}]
            self.assertEqual(preflight.probe_result(requests), {"allowed_read": True})
        self.assertIsNone(preflight.probe_result([{"input": [{"type": "function_call_output", "output": "no marker"}]}]))

    def test_scripted_provider_streams_one_tool_call_then_a_final_message(self):
        server, requests = preflight.scripted_provider("python3 probe.py")
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        items = []
        for index in range(2):
            connection = http.client.HTTPConnection("127.0.0.1", server.server_address[1], timeout=10)
            body = json.dumps({"model": "m", "input": [{"type": "message", "index": index}]})
            connection.request("POST", "/v1/responses", body, {"Content-Type": "application/json"})
            response = connection.getresponse()
            events = [json.loads(line[len("data: "):]) for line in response.read().decode().splitlines()
                      if line.startswith("data: ")]
            connection.close()
            self.assertEqual(events[-1]["type"], "response.completed")
            self.assertEqual(events[-1]["response"]["usage"], preflight.SCRIPTED_USAGE[index])
            items.append(events[-1]["response"]["output"][0])
        self.assertEqual(items[0]["type"], "function_call")
        self.assertEqual(items[0]["name"], "exec_command")
        self.assertEqual(json.loads(items[0]["arguments"])["cmd"], "python3 probe.py")
        self.assertEqual(items[1]["type"], "message")
        self.assertEqual(len(requests), 2)


if __name__ == "__main__":
    unittest.main()
