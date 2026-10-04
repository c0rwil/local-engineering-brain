"""Hermetic safety and provenance checks for the historical replay runner."""
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
import sqlite3
import sys
import tempfile
import tomllib
import unittest
from unittest import mock


SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location("real_repo_runner_under_test", SCRIPTS / "run_real_repo_pilot.py")
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


def isolate_test_scratch_inventory(test):
    """Do not let unrelated running Claude tasks affect a unit-test inventory."""
    original_glob = Path.glob

    def private_glob(path, pattern):
        entries = original_glob(path, pattern)
        if path == Path("/private/tmp"):
            return (entry for entry in entries if entry.name.startswith(("claude-runner-unit-", "erp26-unit-")))
        return entries

    patch = mock.patch.object(Path, "glob", new=private_glob)
    patch.start()
    test.addCleanup(patch.stop)


class RunnerIsolationTests(unittest.TestCase):
    def setUp(self):
        isolate_test_scratch_inventory(self)
        self.temp = tempfile.TemporaryDirectory(prefix="real-repo-runner-unit-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.data = self.root / "private evidence"
        self.active = self.root / "active"
        self.work = self.active / "work"
        self.state = self.active / "state"
        self.work.mkdir(parents=True)
        self.state.mkdir()

    def settings(self, arm):
        return runner.settings(self.work, self.data, self.active, self.state, arm)

    def test_compact_policy_does_not_enumerate_growing_private_temp_prefixes(self):
        data = Path("/private/tmp/erp26-compact-unit-data")
        active = Path("/private/tmp/erp26work-compact-unit-active")
        sibling = Path("/private/tmp/erp26work-compact-unit-sibling")
        def inventory(path, pattern):
            self.assertNotIn(pattern, ("engram-*", "erp26-*"))
            return iter([sibling]) if pattern == "erp26work-*" else iter([])
        with mock.patch.object(Path, "glob", new=inventory), mock.patch.object(runner.sys, "platform", "darwin"):
            value = runner.settings(active / "work", data, active, active / "state", "off", compact=True)
        fs = value["sandbox"]["filesystem"]
        self.assertIn("/private/tmp/engram-**", fs["denyRead"])
        self.assertIn("/private/tmp/erp26-**", fs["denyRead"])
        self.assertIn(str(sibling), fs["denyRead"])
        self.assertNotIn(str(active), fs["denyRead"])
        for tool in ("Read", "Edit", "Write", "Glob", "Grep"):
            self.assertIn(f"{tool}(//private/tmp/engram-*)", value["permissions"]["deny"])
            self.assertIn(f"{tool}(//private/tmp/engram-*/**)", value["permissions"]["deny"])
            self.assertIn(f"{tool}(/{sibling}/**)", value["permissions"]["deny"])
        self.assertLess(len(json.dumps(value)), 12000)
        self.assertIs(value["sandbox"]["allowUnsandboxedCommands"], False)

    def test_compact_mode_requires_disjoint_active_prefix_and_macos(self):
        data = Path("/private/tmp/erp26-compact-unit-data")
        active = Path("/private/tmp/erp26-compact-unit-active")
        with self.assertRaises(ValueError):
            runner.validate_paths(data, active, compact=True)
        active = Path("/private/tmp/erp26work-compact-unit-active")
        runner.validate_paths(data, active, compact=True)
        with self.assertRaises(ValueError):
            runner.validate_paths(data, active)
        with mock.patch.object(runner.sys, "platform", "linux"), self.assertRaisesRegex(ValueError, "macOS"):
            runner.settings(active / "work", data, active, active / "state", "off", compact=True)

    def test_compact_shell_is_pinned_without_changing_legacy_environment(self):
        self.assertNotIn("CLAUDE_CODE_SHELL", runner.coding_env(self.work))
        env = runner.coding_env(self.work, compact=True)
        self.assertEqual(env["CLAUDE_CODE_SHELL"], "/bin/bash")
        self.assertEqual(env["SHELL"], "/bin/bash")
        self.assertEqual(env["TMPDIR"], str(self.active / "scratch"))

    def test_off_arm_has_no_hooks_and_both_arms_disable_agent_memory(self):
        off = self.settings("off")
        brain = self.settings("brain")
        self.assertIs(off["disableAllHooks"], True)
        self.assertFalse(off.get("hooks"))
        self.assertIs(brain["disableAllHooks"], False)
        for value in (off, brain):
            self.assertIs(value["autoMemoryEnabled"], False)
            self.assertIs(value["sandbox"]["enabled"], True)
            self.assertIs(value["sandbox"]["failIfUnavailable"], True)
            self.assertIs(value["sandbox"]["allowUnsandboxedCommands"], False)
            self.assertEqual(value["sandbox"]["network"]["allowedDomains"], [])

    def test_brain_hooks_use_only_the_explicit_private_database_and_socket(self):
        value = self.settings("brain")
        self.assertIn("UserPromptSubmit", value["hooks"])
        self.assertIn("PreToolUse", value["hooks"])
        self.assertIn("Stop", value["hooks"])
        for event, entries in value["hooks"].items():
            for entry in entries:
                for hook in entry["hooks"]:
                    with self.subTest(event=event):
                        self.assertEqual(hook["type"], "command")
                        argv = shlex.split(hook["command"])
                        self.assertEqual(Path(argv[0]), runner.ENGRAM)
                        self.assertEqual(argv[argv.index("--db") + 1], str(self.state / "brain.sqlite"))
                        self.assertEqual(argv[argv.index("--socket") + 1], str(self.state / "d.sock"))
                        self.assertEqual(argv[argv.index("--workspace") + 1], "default")
                        self.assertIn("hook", argv)
                        self.assertGreater(hook["timeout"], 0)

    def test_source_repositories_private_state_and_graders_are_denied(self):
        for arm in ("off", "brain"):
            value = self.settings(arm)
            filesystem = value["sandbox"]["filesystem"]
            private = {self.data.resolve(), self.state.resolve(), runner.ROOT.resolve(), runner.SOURCE_ROOT.resolve(),
                       Path.home() / ".engram", Path.home() / ".codex", Path.home() / ".claude"}
            self.assertTrue(private.issubset({Path(path) for path in filesystem["denyRead"]}))
            self.assertIn(str(self.work), filesystem["allowWrite"])
            self.assertIn(str(self.work / ".engram"), filesystem["denyWrite"])
            self.assertNotIn(str(self.work), filesystem["denyRead"])
            denies = value["permissions"]["deny"]
            for path in private:
                for tool in ("Read", "Edit", "Write", "Glob", "Grep"):
                    self.assertIn(f"{tool}(/{path}/**)", denies)

    def test_agent_process_cannot_load_global_settings_or_mcp_servers(self):
        config = self.state / "settings.json"
        argv = runner.agent_command(config, 1.0)
        self.assertEqual(argv[argv.index("--setting-sources") + 1], "")
        self.assertEqual(argv[argv.index("--settings") + 1], str(config))
        self.assertIn("--strict-mcp-config", argv)
        self.assertEqual(json.loads(argv[argv.index("--mcp-config") + 1]), {"mcpServers": {}})
        self.assertIn("--disable-slash-commands", argv)
        self.assertIn("--no-session-persistence", argv)
        self.assertIn("--no-chrome", argv)

    def test_agent_scratch_and_build_cache_are_scoped_to_the_active_trial(self):
        process = mock.Mock(returncode=0)
        with mock.patch.object(runner.subprocess, "Popen", return_value=process) as launch:
            result = runner.execute_agent(self.work, self.state / "settings.json", "synthetic task",
                                          self.root / "trace.jsonl", self.root / "stderr.log", 10, 1.0)
        env = launch.call_args.kwargs["env"]
        self.assertEqual(env["CLAUDE_CODE_TMPDIR"], str(self.active / "scratch"))
        self.assertEqual(env["TMPDIR"], str(self.active / "scratch"))
        self.assertEqual(env["CARGO_HOME"], str(self.active / "scratch/cargo-home"))
        self.assertEqual(env["CARGO_TARGET_DIR"], str(self.work / "target"))
        self.assertEqual(env["GIT_CONFIG_GLOBAL"], "/dev/null")
        self.assertEqual(env["GIT_CONFIG_SYSTEM"], "/dev/null")
        self.assertEqual(env["GIT_CONFIG_NOSYSTEM"], "1")
        self.assertEqual(launch.call_args.kwargs["cwd"], self.work)
        process.communicate.assert_called_once_with("synthetic task", timeout=10)
        self.assertEqual(result["agent_exit_code"], 0)
        self.assertIs(result["agent_timed_out"], False)
        settings = self.settings("off")
        self.assertEqual(set(settings["sandbox"]["filesystem"]["allowWrite"]),
                         {str(self.work), str(self.active / "scratch")})

    def test_existing_claude_scratch_is_protected_with_only_new_cwd_glob_exception(self):
        scratch = tempfile.TemporaryDirectory(prefix="claude-runner-unit-", dir="/private/tmp")
        self.addCleanup(scratch.cleanup)
        scratch_root = Path(scratch.name).resolve()
        old_directory = scratch_root / "old-task-output"
        old_directory.mkdir()
        (old_directory / "answer.txt").write_text("synthetic prior-task answer")
        old_children = {old_directory}
        for name in ("old-command.log", ".hidden-artifact"):
            path = scratch_root / name
            path.write_text("synthetic prior-task artifact")
            old_children.add(path)
        for arm in ("off", "brain"):
            with self.subTest(arm=arm):
                value = self.settings(arm)
                filesystem = value["sandbox"]["filesystem"]
                self.assertIn(str(scratch_root), filesystem["denyRead"])
                self.assertTrue(old_children.issubset({Path(path) for path in filesystem["denyWrite"]}))
                self.assertIn(str(scratch_root / "cwd-*"), filesystem["allowRead"])
                self.assertIn(str(scratch_root / "cwd-*"), filesystem["allowWrite"])
                self.assertNotIn(str(scratch_root), filesystem["allowRead"])
                self.assertNotIn(str(scratch_root), filesystem["allowWrite"])
                for tool in ("Read", "Glob", "Grep"):
                    self.assertIn(f"{tool}(/{scratch_root}/**)", value["permissions"]["deny"])
                for tool in ("Edit", "Write"):
                    self.assertNotIn(f"{tool}(/{scratch_root}/**)", value["permissions"]["deny"])
                    for child in old_children:
                        self.assertIn(f"{tool}(/{child})", value["permissions"]["deny"])
                        self.assertIn(f"{tool}(/{child}/**)", value["permissions"]["deny"])

    def test_existing_cwd_scratch_artifact_fails_closed(self):
        scratch = tempfile.TemporaryDirectory(prefix="claude-runner-unit-", dir="/private/tmp")
        self.addCleanup(scratch.cleanup)
        old_cwd = Path(scratch.name) / "cwd-prior-task"
        old_cwd.write_text("synthetic prior-task artifact")
        for arm in ("off", "brain"):
            with self.subTest(arm=arm):
                with self.assertRaises((ValueError, RuntimeError)):
                    self.settings(arm)
        self.assertEqual(old_cwd.read_text(), "synthetic prior-task artifact")

    def test_clean_environment_does_not_inherit_service_credentials_or_cargo_target(self):
        with mock.patch.dict(os.environ, {
            "OPENAI_API_KEY": "synthetic-secret", "DATABASE_URL": "synthetic-database",
            "AGENT_API_KEY": "synthetic-backend-key", "CARGO_TARGET_DIR": "/private/other-candidate-target",
            "ENGRAM_DB": "/private/real-brain.sqlite", "AWS_SECRET_ACCESS_KEY": "synthetic-aws-key",
            "CARGO_HOME": "/private/real-cargo-home", "GIT_CONFIG_GLOBAL": "/private/user-git-config",
            "GIT_CONFIG_SYSTEM": "/private/system-git-config", "GIT_CONFIG_NOSYSTEM": "0",
        }):
            env = runner.clean_env()
        for key in ("OPENAI_API_KEY", "DATABASE_URL", "AGENT_API_KEY", "CARGO_HOME", "CARGO_TARGET_DIR", "ENGRAM_DB", "AWS_SECRET_ACCESS_KEY"):
            self.assertNotIn(key, env)
        self.assertEqual(env["CARGO_NET_OFFLINE"], "true")
        self.assertEqual(env["ENVIRONMENT"], "test")
        self.assertEqual(env["AGENT_DATA_SOURCE"], "api")
        self.assertEqual(env["USE_REDIS_SESSIONS"], "false")
        self.assertEqual(env["AWS_EC2_METADATA_DISABLED"], "true")
        self.assertEqual(env["GIT_CONFIG_GLOBAL"], "/dev/null")
        self.assertEqual(env["GIT_CONFIG_SYSTEM"], "/dev/null")
        self.assertEqual(env["GIT_CONFIG_NOSYSTEM"], "1")

    def test_daemon_config_disables_background_learning_and_external_sources(self):
        db = self.state / 'brain "quoted".sqlite'
        sock = self.state / "d.sock"
        config = tomllib.loads(runner.config_text(db, sock))
        self.assertEqual(config["storage"]["db_path"], str(db))
        self.assertEqual(config["daemon"]["socket_path"], str(sock))
        self.assertEqual(config["storage"]["vector_backend"], "disabled")
        for area in ("web", "watch", "jobs", "vault", "judge"):
            self.assertIs(config[area]["enabled"], False)
        self.assertEqual(config["architect"]["auto"], "off")
        self.assertIs(config["synthesis"]["auto"], False)
        self.assertIs(config["synthesis"]["cross_project_hypotheses"], False)
        self.assertIs(config["brain"]["auto_learn"], False)
        self.assertIs(config["links"]["auto_scan"], False)
        self.assertIs(config["context"]["include_linked_projects"], False)
        self.assertIs(config["context"]["semantic_retrieval"], False)
        self.assertEqual(config["embeddings"]["provider"], "disabled")
        self.assertIs(config["embeddings"]["allow_remote"], False)
        self.assertEqual(config["sync"]["central_url"], "")
        self.assertIs(config["sync"]["push"], False)
        self.assertIs(config["network"]["local_only"], True)

    def test_clone_refuses_to_overwrite_existing_evidence(self):
        destination = self.root / "existing-evidence"
        destination.mkdir()
        evidence = destination / "trace.jsonl"
        evidence.write_text("preserved evidence\n")
        with mock.patch.object(runner, "command") as command:
            with self.assertRaises(RuntimeError):
                runner.clone(self.work, destination)
            command.assert_not_called()
        self.assertEqual(evidence.read_text(), "preserved evidence\n")

    def test_clone_refuses_an_occupied_dangling_symlink(self):
        destination = self.root / "occupied-path"
        destination.symlink_to(self.root / "missing-target", target_is_directory=True)
        with mock.patch.object(runner, "command") as command:
            with self.assertRaises(RuntimeError):
                runner.clone(self.work, destination)
            command.assert_not_called()
        self.assertTrue(destination.is_symlink())

    def test_receipts_count_only_acknowledged_nonempty_brain_context(self):
        with contextlib.closing(sqlite3.connect(self.state / "brain.sqlite")) as con:
            con.executescript("""
                CREATE TABLE context_runs(id TEXT, selected_tokens INTEGER, mode TEXT, stable_prefix TEXT, dynamic_context TEXT);
                CREATE TABLE tasks(id TEXT, session_id TEXT);
                CREATE TABLE usage_events(source_type TEXT, source_id TEXT, usage_kind TEXT, context_run_id TEXT);
                CREATE TABLE context_items(source_type TEXT);
                INSERT INTO tasks VALUES ('task-1','session-1');
                INSERT INTO context_runs VALUES ('delivered',40,'brain','prefix','context');
                INSERT INTO context_runs VALUES ('compiled-only',90,'brain','prefix','context');
                INSERT INTO context_runs VALUES ('empty',0,'brain','','');
                INSERT INTO context_runs VALUES ('off',0,'off','unexpected','content');
                INSERT INTO usage_events VALUES ('task','task-1','context_served','delivered');
                INSERT INTO usage_events VALUES ('task','task-1','context_served','delivered');
                INSERT INTO usage_events VALUES ('task','task-1','context_served','empty');
                INSERT INTO usage_events VALUES ('task','task-1','context_served','off');
                INSERT INTO usage_events VALUES ('task','task-1','context_compiled','compiled-only');
                INSERT INTO usage_events VALUES ('memory','task-1','context_served','compiled-only');
                INSERT INTO usage_events VALUES ('task','nonexistent-task','context_served','compiled-only');
                INSERT INTO context_items VALUES ('code'),('code_dependency'),('verification'),('memory');
            """)
        # A sidecar claims delivery for a compilation never acknowledged in SQL.
        (self.state / "forged.context-delivery").write_text(json.dumps({"run_id": "compiled-only", "text_digest": "synthetic"}))
        result = runner.receipt_summary(self.state)
        self.assertEqual(result["emission_receipts"], 1)
        self.assertEqual(result["delivered_run_ids"], ["delivered"])
        self.assertEqual(result["delivered_sessions"], ["session-1"])
        self.assertEqual(result["delivered_compiled_estimated_tokens"], 40)
        self.assertEqual(result["compiled_runs"], 4)
        self.assertEqual(result["compiled_estimated_tokens"], 130)
        self.assertEqual(result["source_items"], 3)


class RunnerPathProtectionTests(unittest.TestCase):
    def setUp(self):
        isolate_test_scratch_inventory(self)
        data = tempfile.TemporaryDirectory(prefix="erp26-unit-data-", dir="/private/tmp")
        active = tempfile.TemporaryDirectory(prefix="erp26-unit-active-", dir="/private/tmp")
        self.addCleanup(data.cleanup)
        self.addCleanup(active.cleanup)
        self.data = Path(data.name)
        self.active = Path(active.name)
        self.marker = self.active / ".pilot-owned"
        self.marker.write_text("real-repo-v1\n")

    def test_marked_sibling_temp_roots_are_accepted(self):
        runner.validate_paths(self.data, self.active)
        self.assertEqual(self.marker.read_text(), "real-repo-v1\n")

    def test_absent_private_active_root_is_accepted_without_creating_it(self):
        self.marker.unlink()
        self.active.rmdir()
        runner.validate_paths(self.data, self.active)
        self.assertFalse(self.active.exists())

    def test_existing_active_directory_requires_ownership_marker(self):
        self.marker.unlink()
        evidence = self.active / "user-file"
        evidence.write_text("retain me")
        with self.assertRaises(ValueError):
            runner.validate_paths(self.data, self.active)
        self.assertEqual(evidence.read_text(), "retain me")

    def test_real_repositories_and_broad_temp_paths_are_rejected(self):
        for protected in (Path("/"), Path("/tmp"), Path("/private/tmp"), Path.home(),
                          runner.ROOT, runner.SOURCE_ROOT, runner.SOURCE_ROOT / "agi-backend-rust"):
            with self.subTest(path=str(protected), position="data"):
                with self.assertRaises(ValueError):
                    runner.validate_paths(protected, self.active)
            with self.subTest(path=str(protected), position="active"):
                with self.assertRaises(ValueError):
                    runner.validate_paths(self.data, protected)

    def test_equal_nested_and_wrong_prefix_roots_are_rejected(self):
        for data, active in ((self.data, self.data), (self.data, self.data / "erp26-active"),
                             (self.active / "erp26-data", self.active),
                             (self.data, Path("/private/tmp/ordinary-user-checkout"))):
            with self.subTest(data=str(data), active=str(active)):
                with self.assertRaises(ValueError):
                    runner.validate_paths(data, active)

    def test_leaf_symlink_is_rejected_even_when_target_is_a_valid_root(self):
        link = self.active.with_name(self.active.name + "-link")
        link.symlink_to(self.active, target_is_directory=True)
        self.addCleanup(link.unlink)
        with self.assertRaises(ValueError):
            runner.validate_paths(self.data, link)
        with self.assertRaises(ValueError):
            runner.validate_paths(link, self.active)
        self.assertEqual(self.marker.read_text(), "real-repo-v1\n")

    def test_other_trial_temp_roots_are_denied_without_blocking_current_scratch(self):
        other = tempfile.TemporaryDirectory(prefix="erp26-unit-other-", dir="/private/tmp")
        self.addCleanup(other.cleanup)
        work, state = self.active / "work", self.active / "state"
        value = runner.settings(work, self.data, self.active, state, "brain")
        filesystem = value["sandbox"]["filesystem"]
        self.assertIn(str(Path(other.name).resolve()), filesystem["denyRead"])
        self.assertIn(str(self.data.resolve()), filesystem["denyRead"])
        self.assertNotIn(str(self.active.resolve()), filesystem["denyRead"])
        self.assertIn(str(self.active / "scratch"), filesystem["allowWrite"])

    def dependency_fixture(self):
        task = {"id": "unit-rust", "repository": "agi-backend-rust"}
        fixture = self.data / "fixtures" / task["id"]
        target = fixture / "work/target"
        target.mkdir(parents=True)
        (target / "baseline-artifact").write_text("baseline")
        (fixture / "ready.json").write_text(json.dumps({"base_commit": "synthetic-base"}))
        fake_home = self.data / "fake-user-home"
        registry = fake_home / ".cargo/registry"
        registry.mkdir(parents=True)
        (registry / "public-crate").write_text("synthetic dependency")
        (fake_home / ".cargo/credentials.toml").write_text("synthetic credential")
        (fake_home / ".cargo/config.toml").write_text("synthetic user configuration")
        (fake_home / ".cargo/git").mkdir()
        (fake_home / ".cargo/git/private-checkout").write_text("synthetic checkout")
        return task, fixture, fake_home

    def test_private_dependency_preparation_copies_only_registry_and_warms_private_paths(self):
        task, fixture, fake_home = self.dependency_fixture()
        copies = []

        def copy(source, destination):
            copies.append((source, destination))
            shutil.copytree(source, destination)

        def warmup(argv, **kwargs):
            self.assertEqual(argv, ["cargo", "test", "--offline", "--locked", "--no-run"])
            self.assertEqual(kwargs["cwd"], self.active / "work")
            self.assertEqual(kwargs["env"]["CARGO_HOME"], str(self.active / "scratch/cargo-home"))
            self.assertEqual(kwargs["env"]["CARGO_TARGET_DIR"], str(self.active / "work/target"))
            self.assertEqual(kwargs["env"]["GIT_CONFIG_GLOBAL"], "/dev/null")
            (self.active / "work/target/private-warmup").write_text("warm baseline only")
            return ""

        with mock.patch.object(Path, "home", return_value=fake_home), \
             mock.patch.object(runner, "clone", side_effect=copy), \
             mock.patch.object(runner, "command", side_effect=warmup) as build:
            runner.prepare_private_dependencies(task, self.data, self.active)
        self.assertEqual(copies[0], (fake_home / ".cargo/registry", fixture / "cargo-home/registry"))
        self.assertEqual({path.name for path in (fixture / "cargo-home").iterdir()}, {"registry"})
        self.assertFalse((self.active / "scratch/cargo-home/credentials.toml").exists())
        self.assertFalse((self.active / "scratch/cargo-home/config.toml").exists())
        self.assertFalse((self.active / "scratch/cargo-home/git").exists())
        self.assertEqual((fixture / "work/target/private-warmup").read_text(), "warm baseline only")
        ready = json.loads((fixture / "ready.json").read_text())
        self.assertEqual(ready["base_commit"], "synthetic-base")
        self.assertTrue(ready["cargo_home_scope"])
        build.assert_called_once()
        self.assertEqual((fake_home / ".cargo/credentials.toml").read_text(), "synthetic credential")

    def test_failed_private_dependency_warmup_preserves_baseline_and_does_not_claim_ready(self):
        task, fixture, fake_home = self.dependency_fixture()
        with mock.patch.object(Path, "home", return_value=fake_home), \
             mock.patch.object(runner, "clone", side_effect=shutil.copytree), \
             mock.patch.object(runner, "command", side_effect=RuntimeError("synthetic build failure")):
            with self.assertRaisesRegex(RuntimeError, "synthetic build failure"):
                runner.prepare_private_dependencies(task, self.data, self.active)
        self.assertEqual((fixture / "work/target/baseline-artifact").read_text(), "baseline")
        self.assertNotIn("cargo_home_scope", json.loads((fixture / "ready.json").read_text()))


if __name__ == "__main__":
    unittest.main()
