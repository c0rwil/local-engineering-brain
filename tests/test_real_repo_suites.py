"""Suite-generic collection: v2 stays byte-identical; local fixtures, grader preflight and detached launch.

Hermetic: synthetic source repositories, a fake `claude` executable and fake
graders. No model, network, real repository or pre-existing daemon is used.
"""
import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

TESTS = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS.parent / "scripts"))
sys.path.insert(0, str(TESTS))
sys.dont_write_bytecode = True
import import_real_repo_pilot as importer  # noqa: E402
import launch_real_repo_collection as launcher  # noqa: E402
import preflight_real_repo_graders as graders  # noqa: E402
import report_real_repo_pilot as reporter  # noqa: E402
import real_repo_v2_observation as v2_observation  # noqa: E402
import run_real_repo_followup as followup  # noqa: E402

runner = followup.runner
HELDOUT = runner.ROOT / "benchmarks/real-repo-heldout-v1/catalog.json"
RELEASE = "82b5df1dbe45dd72b1d113909ec81a549242f0a846fa00ff4202cb9b2e12aa9a"


GIT_ENV = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1", "GIT_OPTIONAL_LOCKS": "0",
           "GIT_AUTHOR_DATE": "2020-01-01T00:00:00+0000", "GIT_COMMITTER_DATE": "2020-01-01T00:00:00+0000"}


def git(repository, *arguments):
    return subprocess.run(["git", "-C", str(repository), *arguments], check=True, capture_output=True, text=True,
                          env={**os.environ, **GIT_ENV}).stdout.strip()


def commit(repository, message):
    git(repository, "add", "-A", "--force")
    git(repository, "-c", "user.name=Synthetic", "-c", "user.email=synthetic@invalid", "commit", "-qm", message)
    return git(repository, "rev-parse", "HEAD")


def preserve_runner_globals(test):
    saved = {name: getattr(runner, name) for name in ("SUITE", "ENGRAM", "DAEMON", "CLAUDE")}
    test.addCleanup(lambda: [setattr(runner, name, value) for name, value in saved.items()])


def source_state(repository):
    """Everything a read-only preparation must leave unchanged."""
    return {"head": git(repository, "rev-parse", "HEAD"), "refs": git(repository, "for-each-ref"),
            "status": git(repository, "status", "--porcelain", "--untracked-files=all"),
            "worktrees": git(repository, "worktree", "list", "--porcelain"),
            "index_mtime": (repository / ".git/index").stat().st_mtime_ns}


class V2ByteIdentityTests(unittest.TestCase):
    def test_v2_manifest_protocol_output_and_schedule_are_unchanged(self):
        golden = json.loads(v2_observation.GOLDEN.read_text())
        observed = v2_observation.observe(followup)
        for key in ("manifest_text", "manifest", "protocol_files", "stdout", "trials", "agent_command"):
            with self.subTest(part=key):
                self.assertEqual(observed[key], golden[key])
        self.assertEqual(observed, golden)

    def test_v2_has_no_draft_or_local_fixture_controls(self):
        args = mock.Mock(spec=["data", "agent_seconds", "grader_seconds", "cost_cap"])
        self.assertFalse(followup.local_fixtures(args))
        self.assertEqual(followup.suite_of(args).id, "real-repo-v2")
        self.assertEqual(followup.dependency_note(args), followup.SEED_DEPENDENCY_NOTES["real-repo-v2"])

    def test_draft_flag_is_refused_for_a_ready_suite(self):
        argv = ["run_real_repo_followup.py", "--engram-release", "/nonexistent", "--allow-draft-suite"]
        stderr = io.StringIO()
        with mock.patch.object(sys, "argv", argv), contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit):
            followup.main()
        self.assertIn("applies only to a suite marked draft_requires_operator_review", stderr.getvalue())


class HeldoutProtocolTests(unittest.TestCase):
    def setUp(self):
        preserve_runner_globals(self)
        self.suite = followup.Suite(HELDOUT)
        self.temp = tempfile.TemporaryDirectory(prefix="heldout-protocol-")
        self.addCleanup(self.temp.cleanup)
        self.data = Path(self.temp.name)

    def args(self, **extra):
        return mock.Mock(spec=[], suite=self.suite, data=self.data, seed_fixtures=None, agent="claude",
                         agent_seconds=1200, grader_seconds=1800, cost_cap=8.0, **extra)

    def test_default_protocol_is_five_tasks_three_repetitions_two_alternating_arms(self):
        self.assertEqual(self.suite.defaults["data"], "/private/tmp/erp26-heldout-data")
        planned = followup.schedule(self.suite.catalog["tasks"], followup.repetitions(self.suite.catalog))
        self.assertEqual(len(planned), 30)
        self.assertEqual({task["id"] for _, task, _ in planned}, {task["id"] for task in self.suite.catalog["tasks"]})
        pairs = [planned[index:index + 2] for index in range(0, 30, 2)]
        for pair in pairs:
            self.assertEqual(pair[0][:2], pair[1][:2])
            self.assertEqual({pair[0][2], pair[1][2]}, {"off", "brain"})
        firsts = [pair[0][2] for pair in pairs]
        self.assertIn("off", firsts)
        self.assertIn("brain", firsts)

    def test_controls_record_the_draft_flag_grader_proof_and_settings(self):
        with mock.patch.object(followup.runner, "digest", return_value="digest"), \
             mock.patch.object(followup.runner, "command", return_value="2.1.212 (Claude Code)\n"):
            plain = followup.controls(self.args(), self.suite.catalog)
            drafted = followup.controls(self.args(allow_draft_suite=True), self.suite.catalog)
        self.assertEqual(plain["suite"], "real-repo-heldout-v1")
        self.assertEqual(plain["repetitions"], 3)
        self.assertEqual(set(plain["fixture_hashes"]), {task["id"] for task in self.suite.catalog["tasks"]})
        self.assertIn("preflight_real_repo_graders.py", plain["helper_sha256"])
        self.assertEqual(plain["grader_environment"], graders.grader_environment(self.data))
        self.assertIn("grader_preflight_sha256", plain)
        self.assertNotIn("allow_draft_suite", plain)
        self.assertIs(drafted["allow_draft_suite"], True)
        self.assertEqual(drafted["suite_status"], "draft_requires_operator_review")
        self.assertTrue(any("--allow-draft-suite" in line for line in followup.scope_limits(self.args(allow_draft_suite=True))))
        self.assertEqual(followup.dependency_note(self.args()), followup.LOCAL_DEPENDENCY_NOTE)

    def test_collection_of_the_draft_requires_the_explicit_flag(self):
        with tempfile.TemporaryDirectory(prefix="erp26-heldout-unit-", dir="/private/tmp") as data:
            argv = ["run_real_repo_followup.py", "--catalog", str(HELDOUT), "--data", data,
                    "--active", "/private/tmp/erp26work-heldout-unit-absent", "--engram-release", str(self.release())]
            with mock.patch.object(sys, "argv", argv), \
                 mock.patch.object(followup, "prepare_fixture") as prepare, \
                 self.assertRaisesRegex(ValueError, "requires --allow-draft-suite"):
                followup.main()
            prepare.assert_not_called()

    def release(self):
        release = self.data / "release/bin"
        release.mkdir(parents=True, exist_ok=True)
        for name in ("engram", "engramd"):
            (release / name).write_text("synthetic\n")
        return release.parent


FAKE_CLAUDE = r'''#!{python}
"""Synthetic Claude Code: emulates the private temp handling and edits the checkout; no model."""
import json, os, sys
from pathlib import Path
if "--version" in sys.argv:
    print("2.1.212 (Claude Code)")
    raise SystemExit(0)
prompt = sys.stdin.read()
temp = Path(os.environ["CLAUDE_CODE_TMPDIR"])
private = temp / f"claude-{{os.getuid()}}"
private.mkdir(mode=0o700, parents=True, exist_ok=True)
(private / "cwd-0001").write_text(os.getcwd())
observations = Path({observations!r})
observations.mkdir(exist_ok=True)
index = len(list(observations.iterdir()))
(observations / f"{{index}}.json").write_text(json.dumps({{
    "claude_tmp": str(temp), "claude_tmp_is_link": temp.is_symlink(), "claude_tmp_real": os.path.realpath(temp),
    "sandbox_tmp_bytes": len(str(private).encode()), "cwd": os.getcwd(), "shell": os.environ.get("CLAUDE_CODE_SHELL"),
    "prompt_has_task": "Enable the feature" in prompt}}))
Path("feature.txt").write_text("enabled\n")
model = "claude-opus-4-8[1m]"
events = [
    {{"type": "system", "subtype": "init", "session_id": "fake-session-{{}}".format(index), "model": model}},
    {{"type": "assistant", "message": {{"model": model, "content": [{{"type": "tool_use", "id": "toolu_fake_1", "name": "Write", "input": {{}}}}]}}}},
    {{"type": "result", "subtype": "success", "is_error": False, "num_turns": 2, "total_cost_usd": 0.01,
      "usage": {{"input_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 5}},
      "modelUsage": {{model: {{"inputTokens": 10, "outputTokens": 5}}}}}},
]
for event in events:
    print(json.dumps(event), flush=True)
'''

GRADER = '''import argparse, json, os
from pathlib import Path
parser = argparse.ArgumentParser()
parser.add_argument("--repo", type=Path)
parser.add_argument("--output", type=Path)
args = parser.parse_args()
feature = args.repo / "feature.txt"
checks = [{"name": "candidate_loads", "passed": (args.repo / "README.md").is_file()},
          {"name": "feature_present", "passed": feature.is_file() and feature.read_text().strip() == "enabled"}]
status = "pass" if all(check["passed"] for check in checks) else "fail"
args.output.write_text(json.dumps({"status": status, "checks": checks,
                                   "grader_target": os.environ.get("ENGRAM_RUST_GRADER_TARGET")}))
raise SystemExit(0 if status == "pass" else 1)
'''

PREFLIGHT = '''from pathlib import Path
import sys
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "_shared"))
sys.dont_write_bytecode = True
from preflight_common import Control, Edit, Task, main  # noqa: E402
CONTROLS = (
    Control("base", "base", "base", "fail", "Base lacks the feature.", required_failures=("feature_present",)),
    Control("reference", "reference", "reference", "pass", "Historical fix."),
    Control("disabled-feature", "mutant", "reference", "fail", "Writes the wrong value.",
            edits=(Edit("feature.txt", "enabled", "disabled"),), required_failures=("feature_present",)),
)
TASK = Task(task_id="t1", base_commit={base!r}, reference_commit={reference!r}, grader=HERE / "grade.py",
            preflight=Path(__file__).resolve(), private_files=(), expected_check_count=2, controls=CONTROLS,
            grader_timeout_seconds=60)
if __name__ == "__main__":
    sys.exit(main(TASK))
'''


def fake_index(state, *arguments):
    """Stand-in for `engram index`: a source-only database for the work tree's pinned project."""
    work = Path(arguments[1])
    project = work.joinpath(".engram/project.toml").read_text().split('"')[1]
    with contextlib.closing(sqlite3.connect(state / "brain.sqlite")) as con:
        con.executescript("""
            CREATE TABLE IF NOT EXISTS project_aliases(alias TEXT);
            CREATE TABLE IF NOT EXISTS nodes(id TEXT, search_aliases TEXT, tombstoned_at_ms INTEGER);
            CREATE TABLE IF NOT EXISTS memories(id TEXT);
            CREATE TABLE IF NOT EXISTS tasks(id TEXT);
            CREATE TABLE IF NOT EXISTS context_runs(id TEXT);
            CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER);
            INSERT INTO project_aliases VALUES ('/private/synthetic/alias');
            INSERT INTO nodes VALUES ('n1', '', NULL);
            INSERT INTO schema_migrations VALUES (16);
        """)
        con.commit()
    return json.dumps({"resolved": {"project": {"id": project}}, "indexed_files": 3})


class LocalSuiteTestCase(unittest.TestCase):
    """A synthetic draft suite whose fixture is built from a synthetic local repository."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="local-suite-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.data = Path(tempfile.mkdtemp(prefix="erp26-suite-unit-", dir="/private/tmp"))
        self.active = Path("/private/tmp") / f"erp26work-suite-unit-{os.getpid()}-{id(self)}"
        self.addCleanup(shutil.rmtree, self.data, True)
        self.addCleanup(shutil.rmtree, self.active, True)
        self.sources = self.root / "sources"
        self.source = self.sources / "synthetic"
        self.source.mkdir(parents=True)
        git(self.source, "init", "-q")
        (self.source / "README.md").write_text("synthetic repository\n")
        (self.source / ".gitignore").write_text("ignored-but-tracked.txt\n/target\n")
        (self.source / "ignored-but-tracked.txt").write_text("tracked despite the ignore rule\n")
        (self.source / ".env").write_text("SYNTHETIC_SECRET=never-copied\n")
        (self.source / "tool.sh").write_text("#!/bin/sh\necho tool\n")
        (self.source / "tool.sh").chmod(0o755)
        self.base = commit(self.source, "base")
        (self.source / "feature.txt").write_text("enabled\n")
        self.reference = commit(self.source, "fix")
        (self.source / "future.txt").write_text("a later commit the fixture must not contain\n")
        commit(self.source, "future")
        self.suite_root = self.root / "suite"
        grading = self.suite_root / "grading"
        (grading / "_shared").mkdir(parents=True)
        shutil.copy2(runner.ROOT / "benchmarks/real-repo-heldout-v1/grading/_shared/preflight_common.py",
                     grading / "_shared/preflight_common.py")
        (grading / "t1").mkdir()
        (grading / "t1/grade.py").write_text(GRADER)
        (grading / "t1/preflight.py").write_text(PREFLIGHT.format(base=self.base, reference=self.reference))
        (self.suite_root / "prompts").mkdir()
        (self.suite_root / "prompts/t1.md").write_text("Enable the feature.\n")
        self.task = {"id": "t1", "repository": "synthetic", "base_commit": self.base,
                     "reference_fix_commit": self.reference, "prompt_file": "prompts/t1.md",
                     "grader": {"entrypoint": "grading/t1/grade.py", "preflight_evidence": "grading/t1/preflight.json"}}
        self.catalog = self.suite_root / "catalog.json"
        self.catalog.write_text(json.dumps({"suite_id": "synthetic-heldout", "status": "draft_requires_operator_review",
                                            "execution_status": "draft_requires_operator_review", "tasks": [self.task]}))
        # Calibrate the synthetic grader exactly as the held-out graders were calibrated.
        calibration = subprocess.run([sys.executable, "-B", str(grading / "t1/preflight.py"), "--source-repo", str(self.source),
                                      "--output", str(grading / "t1/preflight.json"), "--root", str(self.root / "calibration")],
                                     capture_output=True, text=True)
        self.assertEqual(calibration.returncode, 0, calibration.stdout + calibration.stderr)
        self.release = self.root / "release"
        (self.release / "bin").mkdir(parents=True)
        for name in ("engram", "engramd"):
            (self.release / "bin" / name).write_text(f"synthetic {name}\n")
        self.observations = self.root / "observations"
        self.claude = self.root / "claude"
        self.claude.write_text(FAKE_CLAUDE.format(python=sys.executable, observations=str(self.observations)))
        self.claude.chmod(0o755)
        preserve_runner_globals(self)
        real_command = runner.command

        def command(arguments, **kwargs):
            if [str(part) for part in arguments[1:]] == ["--version"] and str(arguments[0]) in ("rustc", "node", "pnpm"):
                return f"{arguments[0]} synthetic\n"
            return real_command(arguments, **kwargs)

        patches = [mock.patch.object(runner, "CLAUDE", self.claude), mock.patch.object(runner, "command", side_effect=command),
                   mock.patch.object(runner, "daemon", side_effect=lambda *_: contextlib.nullcontext(mock.Mock())),
                   mock.patch.object(runner, "wait_ready", return_value={"build_id": "synthetic-build"}),
                   mock.patch.object(runner, "cli", side_effect=self.cli),
                   mock.patch.object(runner, "receipt_summary", return_value={"emission_receipts": 1, "compiled_runs": 1}),
                   mock.patch.object(followup.runtime_preflight, "validate_proof", return_value={})]
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)
        (self.data / "runtime-preflight.json").write_text('{"synthetic": "runtime proof"}\n')

    def cli(self, state, *arguments):
        return fake_index(state, *arguments) if arguments[0] == "index" else "{}"

    def collector(self, *extra, release=None):
        argv = ["--catalog", str(self.catalog), "--data", str(self.data), "--active", str(self.active),
                "--source-root", str(self.sources), "--engram-release", str(release or self.release), *extra]
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            followup.main(argv)
        return stdout.getvalue()

    def grader_preflight(self):
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            code = graders.main(["--catalog", str(self.catalog), "--data", str(self.data), "--source-root", str(self.sources)])
        return code, stdout.getvalue()


class LocalFixtureTests(LocalSuiteTestCase):
    def test_fixture_is_the_base_tree_in_one_commit_and_the_source_is_untouched(self):
        before = source_state(self.source)
        output = self.collector("--prepare-only")
        self.assertEqual(source_state(self.source), before)
        self.assertIn("PREPARE t1", output)
        self.assertIn("PREPARED t1", output)
        fixture = self.data / "fixtures/t1"
        work = fixture / "work"
        ready = json.loads((fixture / "ready.json").read_text())
        self.assertEqual(followup.validate_fixture(self.task, fixture), ready)
        self.assertEqual(git(work, "rev-list", "--count", "HEAD"), "1")
        self.assertEqual(git(work, "remote"), "")
        self.assertFalse((work / "feature.txt").exists())
        self.assertFalse((work / "future.txt").exists())
        self.assertFalse((work / ".env").exists())
        self.assertEqual(ready["removed_paths"], [".env"])
        self.assertTrue((work / "ignored-but-tracked.txt").is_file())
        self.assertIn("ignored-but-tracked.txt", git(work, "ls-files"))
        self.assertTrue(os.access(work / "tool.sh", os.X_OK))
        self.assertEqual(ready["project_id"], (work / ".engram/project.toml").read_text().split('"')[1])
        self.assertTrue(ready["project_id"].startswith("prj_"))
        self.assertFalse((work / ".engram/config.toml").exists())
        commit_time = int(git(self.source, "show", "-s", "--format=%ct", self.base))
        self.assertGreater((work / "README.md").stat().st_mtime, commit_time + 1)
        with contextlib.closing(sqlite3.connect(fixture / "snapshot.sqlite")) as con:
            self.assertEqual(con.execute("SELECT count(*) FROM project_aliases").fetchone()[0], 0)
        self.assertEqual(ready["source_counts"]["memories"], 0)
        self.assertEqual(ready["engram_sha256"], runner.digest(self.release / "bin/engram"))
        # Preparing again validates and reuses the frozen fixture.
        self.collector("--prepare-only")
        self.assertEqual(json.loads((fixture / "ready.json").read_text()), ready)

    def test_tree_verification_rejects_a_fixture_that_differs_from_the_base(self):
        work = self.root / "work"
        followup.export_base(self.source, self.base, work, self.root / "archive.log")
        removed = followup.sanitize_tree(work)
        (work / "README.md").write_text("tampered\n")
        followup.commit_fixture(work, self.task, "synthetic-heldout")
        with self.assertRaisesRegex(ValueError, "differs from"):
            followup.verify_fixture_tree(self.source, self.base, work, removed)

    def test_rust_dependencies_build_at_trial_paths_and_share_a_base_build(self):
        home = self.root / "home"
        (home / ".cargo/registry/cache").mkdir(parents=True)
        (home / ".cargo/registry/cache/crate.crate").write_text("synthetic crate\n")
        (home / ".cargo/credentials.toml").write_text("synthetic credential\n")
        builds = []
        real_command = runner.command

        def command(arguments, **kwargs):
            if str(arguments[0]) == "cargo":
                builds.append(kwargs)
                target = Path(kwargs["env"]["CARGO_TARGET_DIR"])
                target.mkdir(exist_ok=True)
                (target / f"build-{len(builds)}").write_text("artifact\n")
                return ""
            return real_command(arguments, **kwargs)

        args = mock.Mock(spec=[], suite=followup.Suite(self.catalog), data=self.data, active=self.active, build_jobs=4)
        first = self.data / "fixtures/first"
        second = self.data / "fixtures/second"
        args.suite.catalog["tasks"] = [{**self.task, "id": "first"}, {**self.task, "id": "second"}]
        with mock.patch.object(Path, "home", return_value=home), mock.patch.object(runner, "command", side_effect=command):
            first.mkdir(parents=True)
            (first / "work").mkdir()
            one = followup.prepare_rust_dependencies(args.suite.catalog["tasks"][0], args, first)
            (first / "ready.json").write_text("{}")
            second.mkdir()
            (second / "work").mkdir()
            two = followup.prepare_rust_dependencies(args.suite.catalog["tasks"][1], args, second)
        self.assertEqual(builds[0]["env"]["CARGO_HOME"], str(self.active / "scratch/cargo-home"))
        self.assertEqual(builds[0]["env"]["CARGO_TARGET_DIR"], str(self.active / "work/target"))
        self.assertEqual(builds[0]["env"]["CARGO_BUILD_JOBS"], "4")
        self.assertEqual(builds[0]["cwd"], self.active / "work")
        self.assertEqual({path.name for path in (first / "cargo-home").iterdir()}, {"registry"})
        self.assertIsNone(one["dependency_reuse"])
        self.assertEqual(two["dependency_reuse"], "first")
        # The second build started from the first one's cache.
        self.assertTrue((second / "work/target/build-1").is_file())
        self.assertTrue((second / "work/target/build-2").is_file())
        self.assertFalse(self.active.exists())


class GraderPreflightTests(LocalSuiteTestCase):
    def setUp(self):
        super().setUp()
        self.collector("--prepare-only")

    def test_recalibration_against_the_fixture_matches_the_recorded_runs(self):
        before = source_state(self.source)
        code, output = self.grader_preflight()
        self.assertEqual(code, 0, output)
        self.assertEqual(source_state(self.source), before)
        proof = graders.validate_proof(self.data, self.suite_root, "synthetic-heldout", [self.task])
        self.assertEqual(proof["tasks"]["t1"]["matched"], 3)
        self.assertEqual(proof["tasks"]["t1"]["base_failed_checks"], ["feature_present"])
        report = json.loads((self.data / "grader-preflight/t1/report.json").read_text())
        runs = {run["fixture"]: run for run in report["runs"]}
        self.assertEqual((runs["base"]["exit_code"], runs["reference"]["exit_code"]), (1, 0))
        self.assertEqual(report["reference_changed_files"], 1)
        self.assertEqual(runs["base"]["result"]["grader_target"], str(self.data / "grader-target"))
        # The base control is the prepared fixture itself, without the build cache.
        self.assertEqual((self.data / "grader-preflight/t1/base/.engram/project.toml").read_text(),
                         (self.data / "fixtures/t1/work/.engram/project.toml").read_text())

    def test_a_mismatch_with_the_recorded_calibration_writes_no_passing_proof(self):
        evidence = self.suite_root / "grading/t1/preflight.json"
        recorded = json.loads(evidence.read_text())
        recorded["runs"][0]["failed_checks"] = ["candidate_loads"]
        evidence.write_text(json.dumps(recorded))
        code, output = self.grader_preflight()
        self.assertEqual(code, 1, output)
        with self.assertRaisesRegex(ValueError, "absent, failed, or differs"):
            graders.validate_proof(self.data, self.suite_root, "synthetic-heldout", [self.task])

    def test_changed_fixture_or_grader_invalidates_the_proof(self):
        self.assertEqual(self.grader_preflight()[0], 0)
        validate = lambda: graders.validate_proof(self.data, self.suite_root, "synthetic-heldout", [self.task])
        validate()
        ready = self.data / "fixtures/t1/ready.json"
        original = ready.read_text()
        ready.write_text(original.replace('"t1"', '"t1"', 1) + "\n")
        with self.assertRaisesRegex(ValueError, "differs"):
            validate()
        ready.write_text(original)
        validate()
        grader = self.suite_root / "grading/t1/grade.py"
        grader.write_text(grader.read_text() + "\n# changed\n")
        with self.assertRaisesRegex(ValueError, "differs"):
            validate()

    def test_absent_proof_and_reruns_are_refused(self):
        with self.assertRaisesRegex(ValueError, "absent"):
            graders.validate_proof(self.data, self.suite_root, "synthetic-heldout", [self.task])
        self.assertEqual(self.grader_preflight()[0], 0)
        with self.assertRaisesRegex(ValueError, "Preserve prior grader preflight evidence"):
            self.grader_preflight()


class FakeAgentCollectionTests(LocalSuiteTestCase):
    def setUp(self):
        super().setUp()
        self.collector("--prepare-only")
        self.assertEqual(self.grader_preflight()[0], 0)

    def test_draft_collection_runs_with_the_flag_recorded_and_private_claude_temp(self):
        with self.assertRaisesRegex(ValueError, "requires --allow-draft-suite"):
            self.collector("--limit", "1")
        self.assertFalse((self.data / "manifest.json").exists())
        progress = self.data / "progress.json"
        output = self.collector("--allow-draft-suite", "--limit", "2", "--progress", str(progress))
        self.assertIn("START t1-r1-off", output)
        self.assertIn("END t1-r1-brain grade=pass", output)
        manifest = json.loads((self.data / "manifest.json").read_text())
        self.assertIs(manifest["allow_draft_suite"], True)
        self.assertIs(manifest["controls"]["allow_draft_suite"], True)
        self.assertEqual(manifest["suite"], "synthetic-heldout")
        self.assertEqual(manifest["suite_status"], "draft_requires_operator_review")
        self.assertEqual(manifest["grader_environment"], graders.grader_environment(self.data))
        self.assertTrue((self.data / "protocol/scripts/preflight_real_repo_graders.py").is_file())
        records = [json.loads(line) for line in (self.data / "records.jsonl").read_text().splitlines()]
        self.assertEqual([record["id"] for record in records], ["t1-r1-off", "t1-r1-brain"])
        self.assertEqual([record["delivery"]["emission_receipts"] for record in records], [0, 1])
        for record in records:
            self.assertEqual(record["grade"]["status"], "pass", record)
            self.assertEqual(record["grade"]["grader_target"], str(self.data / "grader-target"))
        state = json.loads(progress.read_text())
        self.assertEqual((state["state"], state["completed"], state["last"]["id"]), ("limit_reached", 2, "t1-r1-brain"))
        alias = runner.claude_tmp_alias(self.active)
        for observation in sorted(self.observations.iterdir()):
            seen = json.loads(observation.read_text())
            self.assertEqual(seen["claude_tmp"], str(alias))
            self.assertTrue(seen["claude_tmp_is_link"])
            self.assertEqual(seen["claude_tmp_real"], str(runner.claude_tmp_target(self.active)))
            self.assertLessEqual(seen["sandbox_tmp_bytes"], runner.CLAUDE_SANDBOX_TMP_LIMIT)
            self.assertTrue(seen["prompt_has_task"])
        self.assertFalse(alias.is_symlink() or alias.exists())
        # Resuming without the flag is refused; with it, completed attempts are skipped.
        with self.assertRaisesRegex(ValueError, "requires --allow-draft-suite"):
            self.collector("--limit", "1")
        resumed = self.collector("--allow-draft-suite", "--limit", "1")
        self.assertIn("START t1-r2-brain", resumed)
        self.assertNotIn("t1-r1-", resumed)

    def test_a_complete_collection_reports_and_imports_from_its_frozen_catalog(self):
        self.collector("--allow-draft-suite")
        output = self.root / "published"
        progress = reporter.generate_report(self.data, output)
        self.assertEqual((progress["status"], progress["planned_runs"], progress["completed_records"]), ("complete", 6, 6))
        results = json.loads((output / "results.json").read_text())
        self.assertEqual(results["suite"]["task_ids"], ["t1"])
        self.assertIs(results["suite"]["allow_draft_suite"], True)
        self.assertEqual(set(results["suite"]["grader_files"]["t1"]),
                         {f"grading/t1/{name}" for name in ("grade.py", "preflight.json", "preflight.py")})
        readme = (output / "README.md").read_text()
        self.assertTrue(readme.startswith("# Real-repository task benchmark: synthetic-heldout\n"))
        self.assertIn("## Context delivery and reads", readme)
        payloads = importer.build_payloads(importer.load_report(output / "results.json"), "synthetic", "/public/results.json")
        self.assertEqual(len(payloads), 6)
        self.assertEqual(payloads[0]["metrics"]["suite"], {"suite_id": "synthetic-heldout", "held_out": False,
                                                            "allow_draft_suite": True})
        self.assertTrue(all(payload["success"] for payload in payloads))

    def test_check_only_validates_without_freezing_or_running(self):
        output = self.collector("--allow-draft-suite", "--check-only")
        status = json.loads(output.strip().splitlines()[-1])
        self.assertEqual((status["status"], status["planned"], status["completed"]), ("ready", 6, 0))
        self.assertFalse((self.data / "manifest.json").exists())
        self.assertFalse((self.data / "trials").exists())
        self.assertFalse(self.observations.exists())

    def test_collection_refuses_a_failed_or_stale_grader_proof(self):
        proof = self.data / "grader-preflight.json"
        value = json.loads(proof.read_text())
        proof.write_text(json.dumps({**value, "status": "fail"}))
        with self.assertRaisesRegex(ValueError, "Grader preflight"):
            self.collector("--allow-draft-suite", "--check-only")
        progress = self.data / "progress.json"
        with self.assertRaisesRegex(ValueError, "Grader preflight"):
            self.collector("--allow-draft-suite", "--progress", str(progress))
        stopped = json.loads(progress.read_text())
        self.assertEqual(stopped["state"], "stopped")
        self.assertIn("Grader preflight", stopped["error"])
        self.assertFalse((self.data / "manifest.json").exists())
        proof.write_text(json.dumps(value))
        self.collector("--allow-draft-suite", "--check-only")

    def test_an_interrupted_attempt_blocks_the_resume_until_inspected(self):
        (self.data / "trials/t1-r1-off").mkdir(parents=True)
        with self.assertRaisesRegex(ValueError, "after trials have started"):
            self.collector("--allow-draft-suite", "--check-only")
        shutil.rmtree(self.data / "trials")
        self.collector("--allow-draft-suite", "--limit", "1")
        (self.data / "trials/t1-r1-brain").mkdir()
        with self.assertRaisesRegex(ValueError, r"Interrupted attempts need inspection.*t1-r1-brain"):
            self.collector("--allow-draft-suite", "--check-only")
        with self.assertRaisesRegex(RuntimeError, "refusing a silent retry"):
            self.collector("--allow-draft-suite", "--limit", "1")


class SnapshotRefreshTests(LocalSuiteTestCase):
    """A new Engram release re-indexes prepared fixtures without rebuilding them."""

    def setUp(self):
        super().setUp()
        self.collector("--prepare-only")
        self.assertEqual(self.grader_preflight()[0], 0)
        self.fixture = self.data / "fixtures/t1"
        # Stand-ins for a dependency build and cargo home that a refresh must keep.
        (self.fixture / "work/target").mkdir()
        (self.fixture / "work/target/artifact").write_text("built once\n")
        (self.fixture / "cargo-home/registry").mkdir(parents=True)
        self.newer = self.root / "release-newer"
        (self.newer / "bin").mkdir(parents=True)
        for name in ("engram", "engramd"):
            (self.newer / "bin" / name).write_text(f"synthetic {name} with call-site indexing\n")

    def proof_valid(self):
        return graders.validate_proof(self.data, self.suite_root, "synthetic-heldout", [self.task])

    def test_refresh_reindexes_with_the_new_release_and_rebinds_the_grader_proof(self):
        before = json.loads((self.fixture / "ready.json").read_text())
        before_digest = runner.digest(self.fixture / "ready.json")
        artifact = (self.fixture / "work/target/artifact").stat()
        with mock.patch.object(followup, "prepare_rust_dependencies", side_effect=AssertionError("rebuilt")), \
             mock.patch.object(followup, "export_base", side_effect=AssertionError("re-exported")):
            output = self.collector("--prepare-only", "--refresh-snapshots", release=self.newer)
        self.assertIn("REFRESHED t1", output)
        self.assertIn("GRADER-PROOF rebound for 1 refreshed fixture(s): t1", output)
        ready = json.loads((self.fixture / "ready.json").read_text())
        self.assertEqual(ready["engram_sha256"], runner.digest(self.newer / "bin/engram"))
        self.assertEqual(ready["daemon_sha256"], runner.digest(self.newer / "bin/engramd"))
        for unchanged in ("fixture_head", "project_id", "lockfile_sha256", "base_commit", "setup_seconds"):
            self.assertEqual(ready[unchanged], before[unchanged], unchanged)
        refresh = ready["snapshot_refreshes"][-1]
        self.assertEqual((refresh["previous_ready_sha256"], refresh["previous_engram_sha256"]),
                         (before_digest, before["engram_sha256"]))
        archive = self.fixture / refresh["superseded"]
        self.assertEqual(json.loads((archive / "ready.json").read_text()), before)
        self.assertEqual(runner.digest(archive / "snapshot.sqlite"), before["snapshot_sha256"])
        self.assertTrue((archive / "seed").is_dir())
        after = (self.fixture / "work/target/artifact").stat()
        self.assertEqual((after.st_ino, after.st_mtime_ns), (artifact.st_ino, artifact.st_mtime_ns))
        self.assertTrue((self.fixture / "cargo-home/registry").is_dir())
        proof = self.proof_valid()
        self.assertEqual(proof["fixtures"]["t1"], runner.digest(self.fixture / "ready.json"))
        self.assertEqual(proof["rebinds"][-1]["tasks"]["t1"]["from_ready_sha256"], before_digest)
        self.assertEqual(len(list(self.data.glob("grader-preflight.superseded-*.json"))), 1)
        # The refreshed fixtures pass the collection check under the new release.
        status = json.loads(self.collector("--allow-draft-suite", "--check-only", release=self.newer).strip().splitlines()[-1])
        self.assertEqual(status["status"], "ready")

    def test_fixtures_of_the_current_release_are_left_alone(self):
        before = (self.fixture / "ready.json").read_text()
        output = self.collector("--prepare-only", "--refresh-snapshots")
        self.assertNotIn("REFRESHED", output)
        self.assertIn("GRADER-PROOF unchanged (valid)", output)
        self.assertEqual((self.fixture / "ready.json").read_text(), before)
        self.assertFalse((self.fixture / "superseded").exists())
        self.assertFalse(list(self.data.glob("grader-preflight.superseded-*.json")))

    def test_a_proof_invalid_before_the_refresh_is_not_rebound(self):
        path = self.data / "grader-preflight.json"
        proof = json.loads(path.read_text())
        path.write_text(json.dumps({**proof, "status": "fail"}))
        output = self.collector("--prepare-only", "--refresh-snapshots", release=self.newer)
        self.assertIn("REFRESHED t1", output)
        self.assertIn("GRADER-PROOF not rebound (invalid:", output)
        self.assertNotIn("rebinds", json.loads(path.read_text()))
        with self.assertRaisesRegex(ValueError, "Grader preflight"):
            self.proof_valid()

    def test_a_stale_fixture_names_the_refresh_and_refresh_has_limits(self):
        with self.assertRaisesRegex(ValueError, "another Engram release.*--refresh-snapshots"):
            self.collector("--prepare-only", release=self.newer)
        for argv, message in ((["--refresh-snapshots"], "requires --prepare-only"),):
            stderr = io.StringIO()
            with self.subTest(argv=argv), contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit):
                self.collector(*argv)
            self.assertIn(message, stderr.getvalue())
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit):
            followup.main(["--engram-release", str(self.release), "--prepare-only", "--refresh-snapshots"])
        self.assertIn("applies to fixtures built from local repositories", stderr.getvalue())
        (self.data / "manifest.json").write_text("{}")
        with self.assertRaisesRegex(ValueError, "cannot be refreshed after the collection has started"):
            self.collector("--prepare-only", "--refresh-snapshots", release=self.newer)


FAKE_COLLECTOR = r'''#!{python}
import fcntl, json, os, sys, time
from pathlib import Path
argv = sys.argv[1:]
data = Path(argv[argv.index("--data") + 1])
if "--check-only" in argv:
    if (data / "refuse").exists():
        print("synthetic check failure")
        raise SystemExit(1)
    print(json.dumps({{"status": "ready"}}))
    raise SystemExit(0)
progress = Path(argv[argv.index("--progress") + 1])
lock = (data / "collector.lock").open("a")
fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
(data / "environment.json").write_text(json.dumps({{key: os.environ.get(key) for key in ("PATH", "HOME", "USER", "LOGNAME", "LANG")}}))
progress.write_text(json.dumps({{"state": "collecting", "pid": os.getpid(), "argv": argv}}))
while not (data / "release").exists():
    time.sleep(0.05)
progress.write_text(json.dumps({{"state": "completed", "pid": os.getpid()}}))
'''


class LauncherTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="launcher-")
        self.addCleanup(self.temp.cleanup)
        self.data = Path(self.temp.name).resolve()
        collector = self.data / "fake_collector.py"
        collector.write_text(FAKE_COLLECTOR.format(python=sys.executable))
        patch = mock.patch.object(launcher, "COLLECTOR", collector)
        patch.start()
        self.addCleanup(patch.stop)
        self.addCleanup(self.release_collector)
        coding = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(Path.home()), "USER": "synthetic-user",
                  "LOGNAME": "synthetic-user", "SHELL": "/bin/bash", "TMPDIR": "/private/tmp/scratch"}
        (self.data / "runtime-preflight.json").write_text(json.dumps({"identity": {"coding_environment": coding}}))
        self.coding = coding

    def release_collector(self):
        (self.data / "release").write_text("done")
        deadline = time.monotonic() + 10
        while launcher.collector_running(self.data) and time.monotonic() < deadline:
            time.sleep(0.05)
        # The launcher's own process normally exits at once; here the test is the parent.
        if (self.data / "launch.json").is_file():
            with contextlib.suppress(ChildProcessError):
                os.waitpid(json.loads((self.data / "launch.json").read_text())["pid"], 0)

    def start(self, *extra):
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            launcher.main(["start", "--catalog", str(HELDOUT), "--data", str(self.data), "--active", str(self.data / "active"),
                           "--engram-release", str(self.data), "--allow-draft-suite", *extra])
        return stdout.getvalue()

    def status(self):
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            launcher.main(["status", "--data", str(self.data), "--tail", "5"])
        return json.loads(stdout.getvalue())

    def wait_for(self, predicate):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.05)
        self.fail("timed out waiting for the detached collector")

    def test_start_detaches_a_locked_collector_with_the_proof_environment(self):
        started = json.loads(self.start("--", "--agent-seconds", "1200"))
        self.assertTrue(started["running"])
        self.assertEqual(started["command"][:3], [launcher.NOHUP, launcher.CAFFEINATE, "-dims"])
        self.assertIn("--progress", started["command"])
        self.assertIn("--allow-draft-suite", started["command"])
        self.assertEqual(started["command"][-4:-2], ["--agent-seconds", "1200"])
        self.wait_for(lambda: (self.data / "environment.json").exists())
        environment = json.loads((self.data / "environment.json").read_text())
        self.assertEqual(environment["PATH"], self.coding["PATH"])
        self.assertEqual(environment["USER"], "synthetic-user")
        self.assertIsNone(environment["LANG"])
        self.wait_for(lambda: self.status()["running"])
        with self.assertRaises(SystemExit) as refused:
            self.start()
        self.assertIn("already holds", str(refused.exception))
        status = self.status()
        self.assertEqual(status["progress"]["state"], "collecting")
        self.assertTrue(status["launch_pid_alive"])
        # The collector runs in its own session, detached from this process group.
        self.assertNotEqual(os.getsid(status["launch"]["pid"]), os.getsid(0))
        (self.data / "release").write_text("done")
        self.wait_for(lambda: not self.status()["running"])
        self.assertEqual(self.status()["progress"]["state"], "completed")

    def test_failed_check_starts_nothing(self):
        (self.data / "refuse").write_text("synthetic")
        with self.assertRaises(SystemExit) as failed:
            self.start()
        self.assertIn("synthetic check failure", str(failed.exception))
        self.assertFalse((self.data / "launch.json").exists())
        self.assertFalse((self.data / "collector.log").exists())

    def test_dry_run_prints_the_detached_command(self):
        planned = json.loads(self.start("--dry-run"))
        self.assertEqual(planned["command"][:3], [launcher.NOHUP, launcher.CAFFEINATE, "-dims"])
        self.assertTrue(planned["environment_from_runtime_proof"])
        self.assertFalse((self.data / "launch.json").exists())


class ReadmeCommandTests(unittest.TestCase):
    def test_heldout_readme_names_the_installed_release_launch(self):
        readme = (runner.ROOT / "benchmarks/real-repo-heldout-v1/README.md").read_text()
        for phrase in ("scripts/launch_real_repo_collection.py start", "--allow-draft-suite",
                       "--catalog benchmarks/real-repo-heldout-v1/catalog.json", "/private/tmp/erp26-heldout-data",
                       "/private/tmp/erp26work-heldout-active", RELEASE, "scripts/preflight_real_repo_graders.py",
                       "scripts/preflight_real_repo_runtime.py"):
            self.assertIn(phrase, readme)


if __name__ == "__main__":
    unittest.main()
