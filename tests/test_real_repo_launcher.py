"""Supervised collection: Claude Code login checks and protocol-safe resume after an inspected stop.

Hermetic: a fake `claude` reports logged in or out, and a fake collector
speaks the frozen collector's CLI (--check-only, --limit 1, --progress) and
writes one record per attempt. No model, network or real collection is used.
"""
import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest

TESTS = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS.parent / "scripts"))
sys.path.insert(0, str(TESTS))
sys.dont_write_bytecode = True
import launch_real_repo_collection as launcher  # noqa: E402
import report_real_repo_pilot as reporter  # noqa: E402
import run_real_repo_followup as followup  # noqa: E402
import test_real_repo_heldout_report as heldout  # noqa: E402

SECRET = "PRIVATE-SENTINEL"

FAKE_CLAUDE = r'''#!{python}
"""Synthetic Claude Code: `auth status --json` from a state file; prints account details when logged in."""
import json, os, sys
from pathlib import Path
root = Path({root!r})
with (root / "claude-calls.jsonl").open("a") as stream:
    stream.write(json.dumps({{"argv": sys.argv[1:], "env": {{key: os.environ.get(key) for key in
                             ("HOME", "PATH", "USER", "TMPDIR", "CLAUDE_CODE_TMPDIR")}}}}) + "\n")
if sys.argv[1:3] == ["auth", "status"]:
    state = (root / "auth-state").read_text().strip() if (root / "auth-state").exists() else "out"
    if state == "garbage":
        print("not json")
        raise SystemExit(0)
    payload = {{"loggedIn": state == "in", "authMethod": "claude.ai" if state == "in" else "none", "apiProvider": "firstParty"}}
    if state == "in":
        payload.update(email="{secret}@example.invalid", orgId="{secret}", orgName="{secret}", subscriptionType="max")
    print(json.dumps(payload, indent=2))
    raise SystemExit(0 if state == "in" else 1)
raise SystemExit(2)
'''

FAKE_COLLECTOR = r'''#!{python}
"""Stands in for run_real_repo_followup.py: its CLI, one record and trial per attempt."""
import fcntl, json, sys
from pathlib import Path
argv = sys.argv[1:]
data = Path(argv[argv.index("--data") + 1])
plan = json.loads((data / "fake-plan.json").read_text())
records = data / "records.jsonl"
done = [json.loads(line)["id"] for line in records.read_text().splitlines() if line.strip()] if records.exists() else []
with (data / "collector-invocations.jsonl").open("a") as stream:
    stream.write(json.dumps(argv) + "\n")
if "--check-only" in argv:
    if (data / "refuse-check").exists():
        print("synthetic check failure")
        raise SystemExit(1)
    print(json.dumps({{"status": "ready", "planned": len(plan), "completed": len(done)}}))
    raise SystemExit(0)
progress = Path(argv[argv.index("--progress") + 1])
lock = (data / "collector.lock").open("a")
fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
remaining = [identity for identity in plan if identity not in done]
if not remaining:
    progress.write_text(json.dumps({{"state": "completed", "completed": len(done)}}))
    raise SystemExit(0)
attempt = remaining[0]
task, rest = attempt.rsplit("-r", 1)
repetition, arm = rest.split("-")
fail = (data / "fail-next").exists()
trial = data / "trials" / attempt
trial.mkdir(parents=True)
(trial / "trace.jsonl").write_text(json.dumps({{"type": "result", "is_error": fail}}) + "\n")
worked = (data / "worked-before-failing").exists()
metrics = {{"tool_calls": 3 if worked or not fail else 0, "output_tokens": 20 if worked or not fail else 0,
           "total_tokens": 900 if worked or not fail else 0, "is_error": fail, "tool_calls_complete": True}}
record = {{"id": attempt, "task_id": task, "repetition": int(repetition), "arm": arm,
          "grade": {{"status": "fail" if fail else "pass", "checks": []}}, "agent_exit_code": 1 if fail else 0,
          "agent_wall_seconds": 0.3 if fail else 300.0, "metrics": metrics}}
with records.open("a") as stream:
    stream.write(json.dumps(record) + "\n")
if fail:
    (data / "fail-next").unlink()
    progress.write_text(json.dumps({{"state": "stopped", "error": "RuntimeError: Unscorable attempt preserved."}}))
    raise SystemExit(1)
logout = data / "logout-after"
if logout.exists() and int(logout.read_text()) == len(done) + 1:
    Path((data / "auth-root").read_text()).joinpath("auth-state").write_text("out")
progress.write_text(json.dumps({{"state": "limit_reached", "completed": len(done) + 1}}))
'''


class LauncherTestCase(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="launcher-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.data = self.root / "data"
        self.data.mkdir()
        (self.root / "home").mkdir()
        suite = self.root / "suite"
        suite.mkdir()
        self.catalog = suite / "catalog.json"
        self.catalog.write_text(json.dumps({"suite_id": "synthetic-launch", "execution_status": "draft_requires_operator_review",
                                            "tasks": [{"id": "t1", "repository": "synthetic", "base_commit": "a" * 40,
                                                       "prompt_file": "prompts/t1.md"}]}))
        self.plan = [f'{task["id"]}-r{repetition}-{arm}' for repetition, task, arm in
                     followup.schedule(json.loads(self.catalog.read_text())["tasks"], 3)]
        (self.data / "fake-plan.json").write_text(json.dumps(self.plan))
        (self.data / "auth-root").write_text(str(self.root))
        self.claude = self.root / "claude"
        self.claude.write_text(FAKE_CLAUDE.format(python=sys.executable, root=str(self.root), secret=SECRET))
        self.claude.chmod(0o755)
        self.collector = self.root / "collector.py"
        self.collector.write_text(FAKE_COLLECTOR.format(python=sys.executable))
        self.environment = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(self.root / "home"), "USER": "synthetic-user",
                            "LOGNAME": "synthetic-user", "SHELL": "/bin/bash", "TMPDIR": "/private/tmp/erp26work-synthetic/scratch",
                            "CLAUDE_CODE_TMPDIR": f"/private/tmp/e26t-unit{os.getpid()}"}
        (self.data / "runtime-preflight.json").write_text(json.dumps({"identity": {
            "coding_environment": self.environment, "claude_sha256": launcher.runner.digest(self.claude)}}))
        self.login("in")
        self.addCleanup(self.wait_for_supervisor)

    def login(self, state):
        (self.root / "auth-state").write_text(state)

    def options(self, *extra):
        return ["--catalog", str(self.catalog), "--data", str(self.data), "--active", str(self.root / "active"),
                "--engram-release", str(self.root / "release"), "--allow-draft-suite", "--claude", str(self.claude),
                "--collector", str(self.collector), *extra]

    def run_launcher(self, command, *extra):
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            code = launcher.main([command, *self.options(*extra)])
        return code, stdout.getvalue()

    def records(self, name="records.jsonl"):
        path = self.data / name
        return [json.loads(line) for line in path.read_text().splitlines() if line.strip()] if path.exists() else []

    def ledger(self):
        return launcher.read_jsonl(self.data / launcher.AUTH_LEDGER)

    def invocations(self):
        return launcher.read_jsonl(self.data / "collector-invocations.jsonl")

    def wait_for_supervisor(self, timeout=30):
        deadline = time.monotonic() + timeout
        while launcher.supervisor_running(self.data) and time.monotonic() < deadline:
            time.sleep(0.05)
        launch = self.data / "launch.json"
        if launch.is_file():
            with contextlib.suppress(ChildProcessError):
                os.waitpid(json.loads(launch.read_text())["pid"], 0)

    def stop_on_auth_failure(self):
        """Run in-process until the collector stops on an attempt whose agent never started."""
        (self.data / "fail-next").write_text("auth")
        code, _ = self.run_launcher("supervise")
        self.assertEqual(code, 1)
        return self.records()[-1]["id"]


class AuthCheckTests(LauncherTestCase):
    def test_logged_out_is_refused_and_recorded_without_account_details(self):
        self.login("out")
        entry = launcher.auth_check(self.data, "check", "t1-r1-off", self.claude)
        self.assertEqual((entry["logged_in"], entry["auth_method"], entry["exit_code"]), (False, "none", 1))
        with self.assertRaisesRegex(SystemExit, "claude auth login"):
            launcher.require_login(entry)
        self.login("in")
        entry = launcher.auth_check(self.data, "check", "t1-r1-off", self.claude)
        self.assertEqual((entry["logged_in"], entry["auth_method"], entry["api_provider"]), (True, "claude.ai", "firstParty"))
        ledger = (self.data / launcher.AUTH_LEDGER).read_text()
        self.assertEqual(len(self.ledger()), 2)
        self.assertNotIn(SECRET, ledger)
        self.assertNotIn("max", ledger)

    def test_the_check_uses_the_trial_environment_with_private_temporary_directories(self):
        launcher.auth_check(self.data, "check", None, self.claude)
        call = launcher.read_jsonl(self.root / "claude-calls.jsonl")[-1]
        self.assertEqual(call["argv"], ["auth", "status", "--json"])
        for key in ("HOME", "PATH", "USER"):
            self.assertEqual(call["env"][key], self.environment[key])
        self.assertNotEqual(call["env"]["CLAUDE_CODE_TMPDIR"], self.environment["CLAUDE_CODE_TMPDIR"])
        self.assertEqual(call["env"]["TMPDIR"], call["env"]["CLAUDE_CODE_TMPDIR"])
        self.assertTrue(call["env"]["TMPDIR"].startswith("/private/tmp/e26a-"))
        self.assertFalse(Path(call["env"]["TMPDIR"]).exists())
        self.assertFalse(Path(self.environment["CLAUDE_CODE_TMPDIR"]).exists())

    def test_another_binary_or_unreadable_status_is_never_a_login(self):
        self.login("garbage")
        self.assertFalse(launcher.auth_check(self.data, "check", None, self.claude)["logged_in"])
        other = self.root / "other-claude"
        other.write_text(self.claude.read_text() + "\n# changed\n")
        other.chmod(0o755)
        with self.assertRaisesRegex(SystemExit, "not the Claude Code binary of the runtime proof"):
            launcher.auth_check(self.data, "check", None, other)
        (self.data / "runtime-preflight.json").unlink()
        with self.assertRaisesRegex(SystemExit, "No runtime proof"):
            launcher.auth_check(self.data, "check", None, self.claude)


class StartTests(LauncherTestCase):
    def test_check_reports_the_login_and_the_collection(self):
        self.login("out")
        code, output = self.run_launcher("check")
        self.assertEqual(code, 1)
        self.assertIs(json.loads(output)["login"]["logged_in"], False)
        self.login("in")
        code, output = self.run_launcher("check")
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(json.loads(output)["collection"])["status"], "ready")

    def test_start_refuses_a_logged_out_trial_environment_before_anything_runs(self):
        self.login("out")
        with self.assertRaisesRegex(SystemExit, "not logged in"):
            self.run_launcher("start")
        self.assertEqual(self.invocations(), [])
        self.assertFalse((self.data / "launch.json").exists())
        self.assertEqual([entry["context"] for entry in self.ledger()], ["start"])

    def test_detached_supervisor_checks_the_login_before_every_attempt(self):
        code, output = self.run_launcher("start", "--", "--agent-seconds", "1200")
        launched = json.loads(output)
        self.assertEqual(launched["command"][:3], [launcher.NOHUP, launcher.CAFFEINATE, "-dims"])
        self.assertIn("supervise", launched["command"])
        self.wait_for_supervisor()
        self.assertEqual([record["id"] for record in self.records()], self.plan)
        contexts = [entry["context"] for entry in self.ledger()]
        self.assertEqual(contexts, ["start"] + ["before_attempt"] * len(self.plan))
        self.assertEqual([entry["next_attempt"] for entry in self.ledger()[1:]], self.plan)
        runs = [argv for argv in self.invocations() if "--check-only" not in argv]
        self.assertEqual(len(runs), len(self.plan) + 1)
        for argv in runs:
            self.assertEqual(argv[argv.index("--limit") + 1], "1")
            self.assertIn("--progress", argv)
            self.assertEqual(argv[argv.index("--agent-seconds") + 1], "1200")
        self.assertEqual(json.loads((self.data / "progress.json").read_text())["state"], "completed")
        self.assertFalse(launcher.supervisor_running(self.data))

    def test_start_on_a_finished_collection_returns_cleanly(self):
        with (self.data / "records.jsonl").open("w") as stream:
            for identity in self.plan:
                task, rest = identity.rsplit("-r", 1)
                stream.write(json.dumps({"id": identity, "task_id": task, "repetition": int(rest.split("-")[0]),
                                         "arm": rest.split("-")[1], "grade": {"status": "pass"}, "agent_exit_code": 0}) + "\n")
        self.run_launcher("start")
        self.wait_for_supervisor()
        self.assertEqual(json.loads((self.data / "progress.json").read_text())["state"], "completed")
        self.assertEqual([entry["context"] for entry in self.ledger()], ["start"])

    def test_supervisor_stops_before_the_next_attempt_when_the_login_expires(self):
        (self.data / "logout-after").write_text("2")
        code, _ = self.run_launcher("supervise")
        self.assertEqual(code, 2)
        self.assertEqual(len(self.records()), 2)
        progress = json.loads((self.data / "progress.json").read_text())
        self.assertEqual(progress["state"], "stopped")
        self.assertIn("not logged in", progress["error"])
        self.assertIs(self.ledger()[-1]["logged_in"], False)
        self.assertEqual(self.ledger()[-1]["next_attempt"], self.plan[2])


class ResumeTests(LauncherTestCase):
    def test_start_refuses_an_uninspected_stop(self):
        identity = self.stop_on_auth_failure()
        with self.assertRaisesRegex(SystemExit, f"stopped on unscorable attempt {identity}"):
            self.run_launcher("start")
        status = io.StringIO()
        with contextlib.redirect_stdout(status):
            launcher.main(["status", "--data", str(self.data)])
        self.assertEqual(json.loads(status.getvalue())["awaiting_inspection"], identity)

    def test_acknowledged_attempt_moves_out_of_the_matrix_and_its_slot_runs_again(self):
        identity = self.stop_on_auth_failure()
        line = (self.data / "records.jsonl").read_text().splitlines()[-1]
        trace = (self.data / "trials" / identity / "trace.jsonl").read_bytes()
        self.run_launcher("resume", "--acknowledge", identity, "--reason", "OAuth session expired before the agent started",
                          "--no-launch")
        self.assertEqual((self.data / "infrastructure_attempts.jsonl").read_text(), line + "\n")
        self.assertNotIn(identity, [record["id"] for record in self.records()])
        kept = self.data / "infrastructure_trials" / f"{identity}.a1"
        self.assertEqual((kept / "trace.jsonl").read_bytes(), trace)
        self.assertFalse((self.data / "trials" / identity).exists())
        entry = launcher.read_jsonl(self.data / launcher.INSPECTIONS)[-1]
        self.assertEqual((entry["decision"], entry["attempt_id"]), ("separate_infrastructure_attempt", identity))
        self.assertEqual(entry["record_sha256"], launcher.line_digest(line))
        self.assertEqual(entry["evidence"]["trial"], f"infrastructure_trials/{identity}.a1")
        self.assertEqual(entry["eligibility"]["tool_calls"], 0)
        self.assertFalse((self.data / launcher.PENDING).exists())
        # The slot is the next planned attempt again.
        self.assertEqual(launcher.next_attempt(SimpleNamespace(data=self.data, catalog=self.catalog)), identity)
        code, _ = self.run_launcher("supervise")
        self.assertEqual(code, 0)
        self.assertEqual([record["id"] for record in self.records()], self.plan)
        self.assertEqual(len(self.records("infrastructure_attempts.jsonl")), 1)

    def test_resume_with_launch_requires_the_login_before_recording_anything(self):
        identity = self.stop_on_auth_failure()
        self.login("out")
        with self.assertRaisesRegex(SystemExit, "not logged in"):
            self.run_launcher("resume", "--acknowledge", identity, "--reason", "expired login")
        self.assertFalse((self.data / launcher.INSPECTIONS).exists())
        self.assertIn(identity, [record["id"] for record in self.records()])

    def test_resume_launches_the_supervisor_after_recording_the_decision(self):
        identity = self.stop_on_auth_failure()
        code, output = self.run_launcher("resume", "--acknowledge", identity, "--reason", "expired login")
        self.wait_for_supervisor()
        self.assertEqual([record["id"] for record in self.records()], self.plan)
        self.assertIn('"decision": "separate_infrastructure_attempt"', output)
        # The stopped run's check, the resume check, then one check before each attempt of the slot and the rest.
        self.assertEqual([entry["context"] for entry in self.ledger()],
                         ["before_attempt", "resume", *["before_attempt"] * len(self.plan)])

    def test_an_attempt_whose_agent_worked_stays_in_the_matrix(self):
        (self.data / "worked-before-failing").write_text("yes")
        identity = self.stop_on_auth_failure()
        with self.assertRaisesRegex(SystemExit, "agent did work"):
            self.run_launcher("resume", "--acknowledge", identity, "--reason", "provider error mid-run", "--no-launch")
        self.assertFalse((self.data / "infrastructure_attempts.jsonl").exists())
        self.assertTrue((self.data / "trials" / identity).is_dir())
        self.run_launcher("resume", "--continue", "--reason", "provider error mid-run; kept as unscorable", "--no-launch")
        self.assertEqual(self.records()[-1]["id"], identity)
        code, _ = self.run_launcher("supervise")
        self.assertEqual(code, 0)
        ids = [record["id"] for record in self.records()]
        self.assertEqual(ids, self.plan)
        self.assertEqual(ids.count(identity), 1)

    def test_only_the_stopping_attempt_once_and_one_decision(self):
        identity = self.stop_on_auth_failure()
        for extra, message in ((["--acknowledge", "t1-r9-off"], "Only the attempt the collector stopped on"),
                               (["--continue", "--acknowledge", identity], "exactly one"),
                               ([], "exactly one")):
            with self.subTest(extra=extra), self.assertRaisesRegex(SystemExit, message):
                self.run_launcher("resume", *extra, "--reason", "x", "--no-launch")
        self.run_launcher("resume", "--acknowledge", identity, "--reason", "expired login", "--no-launch")
        with self.assertRaisesRegex(SystemExit, "not stopped on an uninspected"):
            self.run_launcher("resume", "--acknowledge", identity, "--reason", "again", "--no-launch")

    def test_a_rerun_slot_that_fails_again_is_a_new_stop(self):
        identity = self.stop_on_auth_failure()
        self.run_launcher("resume", "--acknowledge", identity, "--reason", "expired login", "--no-launch")
        self.assertEqual(self.stop_on_auth_failure(), identity)
        with self.assertRaisesRegex(SystemExit, "stopped on unscorable attempt"):
            self.run_launcher("start")
        self.run_launcher("resume", "--acknowledge", identity, "--reason", "expired again", "--no-launch")
        self.assertTrue((self.data / "infrastructure_trials" / f"{identity}.a2").is_dir())
        self.assertEqual(len(self.records("infrastructure_attempts.jsonl")), 2)

    def test_an_interrupted_inspection_blocks_every_launch(self):
        self.stop_on_auth_failure()
        (self.data / launcher.PENDING).write_text("{}")
        for command, extra in (("start", ()), ("resume", ("--continue", "--reason", "x", "--no-launch"))):
            with self.subTest(command=command), self.assertRaisesRegex(SystemExit, "inspection was interrupted"):
                self.run_launcher(command, *extra)
        self.assertEqual(self.run_launcher("supervise")[0], 3)


class ReportedInfrastructureAttemptTests(unittest.TestCase):
    def test_the_report_lists_the_acknowledged_attempt_outside_the_paired_matrix(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            data = heldout.build_heldout(root)
            identity = f"{heldout.TASKS[0]}-r1-off"
            matrix = (data / "records.jsonl").read_text()
            normal = root / "normal-trial"
            (data / "trials" / identity).rename(normal)
            failed = heldout.record(heldout.TASKS[0], 1, "off", 0)
            failed.update(agent_exit_code=1, agent_wall_seconds=0.327)
            failed["metrics"].update(tool_calls=0, output_tokens=0, total_tokens=0, input_tokens=0, cache_read_input_tokens=0,
                                     cache_creation_input_tokens=0, processed_input_tokens=0, cost_usd_estimate=0, is_error=True)
            (data / "trials" / identity).mkdir()
            (data / "trials" / identity / "trace.jsonl").write_text('{"type": "result", "is_error": true}\n')
            (data / "records.jsonl").write_text(json.dumps(failed) + "\n")
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                launcher.main(["resume", "--acknowledge", identity, "--reason", "OAuth session expired before the agent started",
                               "--no-launch", "--catalog", str(data / "protocol/suite/catalog.json"), "--data", str(data),
                               "--active", str(root / "active"), "--engram-release", str(root / "release")])
            # The collection then completes its matrix, including a fresh attempt for the slot.
            normal.rename(data / "trials" / identity)
            (data / "records.jsonl").write_text(matrix)
            output = root / "published"
            progress = reporter.generate_report(data, output)
            summary = json.loads((output / "summary.json").read_text())
            results = json.loads((output / "results.json").read_text())
            readme = (output / "README.md").read_text()
        self.assertEqual((progress["status"], progress["completed_records"], progress["planned_runs"]), ("complete", 12, 12))
        self.assertEqual(summary["infrastructure_attempts"]["attempts"], 1)
        self.assertEqual(summary["infrastructure_attempts"]["by_arm"]["off"]["grade_counts"], {"infra": 1})
        self.assertEqual(summary["pair_counts"]["complete_pairs"], 6)
        self.assertEqual(results["inspections"][0]["decision"], "separate_infrastructure_attempt")
        self.assertIn("1 recorded attempts are outside the planned matrix", readme)
        self.assertIn(f"- `{identity}`", readme)
        self.assertIn("kept here as a separate infrastructure attempt, outside the paired matrix; its slot ran again", readme)
        self.assertIn("Reason: OAuth session expired before the agent started", readme)


if __name__ == "__main__":
    unittest.main()
