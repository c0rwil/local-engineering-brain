"""Observe the v2 follow-up collection path with every external effect replaced.

tests/fixtures/real_repo_v2_followup.json holds this observation as produced by
the v2-only runner (commit 738d702). The suite-generic runner must reproduce it:
the same frozen manifest, protocol copy, printed output, trial schedule and agent
command. Runner script digests are replaced by their names, so editing a script
does not change the observation; the v2 suite files keep their real digests.

Usage (regenerates the golden; review the diff before committing):
    python3 -B tests/real_repo_v2_observation.py --write
"""
import argparse
import contextlib
import io
import json
from pathlib import Path
import shutil
import sys
import tempfile
from unittest import mock

TESTS = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS.parent / "scripts"))
sys.dont_write_bytecode = True
GOLDEN = TESTS / "fixtures/real_repo_v2_followup.json"
SCRIPT_NAMES = {"run_real_repo_followup.py", "run_real_repo_pilot.py", "real_repo_metrics.py", "eval_context_quality.py",
                "capture_eval.py", "preflight_real_repo_runtime.py", "real_repo_codex.py",
                "preflight_real_repo_codex.py", "preflight_real_repo_graders.py"}
# Options added by the suite-generic runner; they do not exist in the v2-only one.
NEW_OPTIONS = {"catalog", "allow_draft_suite", "source_root", "build_jobs", "progress", "check_only", "suite",
               "refresh_snapshots"}


def observe(followup):
    runner = followup.runner
    data = Path(tempfile.mkdtemp(prefix="erp26-v2-observation-", dir="/private/tmp"))
    active = Path(tempfile.mkdtemp(prefix="erp26work-v2-observation-", dir="/private/tmp"))
    other = Path(tempfile.mkdtemp(prefix="v2-observation-")).resolve()
    saved = {name: getattr(runner, name) for name in ("SUITE", "ENGRAM", "DAEMON", "CLAUDE")}
    try:
        (active / ".pilot-owned").write_text("real-repo-v1\n")
        release = other / "release"
        (release / "bin").mkdir(parents=True)
        for name in ("engram", "engramd"):
            (release / "bin" / name).write_text(f"synthetic {name} binary\n")
        claude = other / "claude"
        claude.write_text("synthetic claude binary\n")
        seeds = other / "seeds"
        seeds.mkdir()
        (data / "runtime-preflight.json").write_text('{"synthetic": "runtime proof"}\n')
        real_digest = runner.digest

        def digest(path):
            path = Path(path)
            return "script:" + path.name if path.name in SCRIPT_NAMES else real_digest(path)

        def command(args, **_):
            program = Path(str(args[0])).name
            return {"claude": "2.1.212 (Claude Code)\n", "rustc": "rustc 1.0.0 (synthetic)\n",
                    "node": "v0.0.0\n", "pnpm": "0.0.0\n"}[program]

        def prepare(task, args):
            ready = args.data / "fixtures" / task["id"] / "ready.json"
            ready.parent.mkdir(parents=True, exist_ok=True)
            ready.write_text(json.dumps({"task_id": task["id"], "synthetic": True}) + "\n")
            return json.loads(ready.read_text())

        trials = []

        def run_trial(task, repetition, arm, args):
            attributes = {key: value for key, value in sorted(vars(args).items())
                          if key not in NEW_OPTIONS and not callable(value)}
            trials.append({"id": f'{task["id"]}-r{repetition}-{arm}', "args": attributes})
            print(f"END {task['id']}-r{repetition}-{arm} grade=pass", flush=True)
            return {"id": f'{task["id"]}-r{repetition}-{arm}', "grade": {"status": "pass"}}

        argv = ["run_real_repo_followup.py", "--engram-release", str(release), "--data", str(data),
                "--active", str(active), "--seed-fixtures", str(seeds)]
        stdout = io.StringIO()
        with mock.patch.object(sys, "argv", argv), \
             mock.patch.object(runner, "CLAUDE", claude), \
             mock.patch.object(runner, "digest", side_effect=digest), \
             mock.patch.object(runner, "command", side_effect=command), \
             mock.patch.object(runner, "run_trial", side_effect=run_trial), \
             mock.patch.object(followup, "prepare_fixture", side_effect=prepare), \
             mock.patch.object(followup, "validate_fixture"), \
             mock.patch.object(followup, "validate_runtime"), \
             contextlib.redirect_stdout(stdout):
            followup.main()
            agent_command = runner.agent_command(active / "state/settings.json", 8)
        raw = (data / "manifest.json").read_text()
        manifest = json.loads(raw)
        # The written file, byte for byte, apart from the clock and interpreter.
        for key in ("frozen_at", "python"):
            raw = raw.replace(json.dumps(manifest[key]), json.dumps(f"<{key}>"))
            manifest[key] = f"<{key}>"
        protocol = sorted(str(path.relative_to(data / "protocol")) for path in (data / "protocol").rglob("*")
                          if path.is_file())
        value = {"stdout": stdout.getvalue().splitlines(), "trials": trials, "manifest": manifest,
                 "manifest_text": raw.splitlines(), "protocol_files": protocol, "agent_command": agent_command}
        text = json.dumps(value, indent=1, sort_keys=True, default=str)
        for path, label in ((data, "<DATA>"), (active, "<ACTIVE>"), (release, "<RELEASE>"), (seeds, "<SEEDS>"),
                            (claude, "<CLAUDE>")):
            text = text.replace(str(path), label)
        return json.loads(text)
    finally:
        for name, value in saved.items():
            setattr(runner, name, value)
        for path in (data, active, other):
            shutil.rmtree(path, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--write", action="store_true", help="write the observation to the golden file")
    args = parser.parse_args()
    import run_real_repo_followup as followup
    value = observe(followup)
    if args.write:
        GOLDEN.parent.mkdir(parents=True, exist_ok=True)
        GOLDEN.write_text(json.dumps(value, indent=1, sort_keys=True) + "\n")
    print(json.dumps({"stdout_lines": len(value["stdout"]), "trials": len(value["trials"])}))


if __name__ == "__main__":
    main()
