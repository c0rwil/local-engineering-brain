#!/usr/bin/env python3
"""Launch a real-repository collection fully detached, and report its progress.

`start` first validates the collection in the foreground
(run_real_repo_followup.py --check-only): fixtures, the runtime and grader
proofs, the draft-suite flag and any frozen manifest. It then starts the
collector in a new session under nohup and `caffeinate -dims`, so neither a
closed terminal nor idle sleep stops it. The collector holds
<data>/collector.lock, rewrites <data>/progress.json around every attempt and
appends its output to <data>/collector.log. Starting again resumes: completed
attempts, including failures, are skipped, and every frozen control is checked
again first. An interrupted attempt blocks the resume until it is inspected.
`status` prints the progress file and whether the collector is alive.

The runtime proof's identity includes the coding environment built from PATH,
HOME, USER, LOGNAME and LANG, so the collector receives those values as the
proof recorded them, whichever shell starts it.
"""
import argparse
import datetime
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
COLLECTOR = ROOT / "scripts/run_real_repo_followup.py"
PINNED_ENVIRONMENT = ("PATH", "HOME", "USER", "LOGNAME", "LANG")
CAFFEINATE = "/usr/bin/caffeinate"
NOHUP = "/usr/bin/nohup"


def collector_running(data):
    """True while a collector holds the data root's lock."""
    lock = data / "collector.lock"
    if not lock.exists():
        return False
    with lock.open("a") as handle:
        try:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(handle, fcntl.LOCK_UN)
    return False


def collector_environment(data, base=None):
    """The launching environment with the runtime proof's PATH, HOME, USER, LOGNAME and LANG."""
    environment = dict(os.environ if base is None else base)
    proof = data / "runtime-preflight.json"
    if not proof.is_file():
        return environment, False
    recorded = json.loads(proof.read_text()).get("identity", {}).get("coding_environment", {})
    for key in PINNED_ENVIRONMENT:
        if key in recorded:
            environment[key] = recorded[key]
        else:
            environment.pop(key, None)
    return environment, True


def collector_arguments(args):
    arguments = ["--catalog", str(args.catalog), "--data", str(args.data), "--active", str(args.active),
                 "--engram-release", str(args.engram_release)]
    if args.allow_draft_suite:
        arguments.append("--allow-draft-suite")
    return [*arguments, *args.collector_args]


def start(args):
    data = args.data
    if collector_running(data):
        raise SystemExit(f"A collector already holds {data / 'collector.lock'}; see `status`")
    environment, pinned = collector_environment(data)
    base = [sys.executable, "-B", str(COLLECTOR), *collector_arguments(args)]
    check = subprocess.run([*base, "--check-only"], cwd=ROOT, env=environment, text=True, capture_output=True)
    if check.returncode:
        raise SystemExit("Collection check failed; nothing was started.\n" + check.stdout + check.stderr)
    command = [NOHUP, CAFFEINATE, "-dims", *base, "--progress", str(data / "progress.json")]
    if args.dry_run:
        print(json.dumps({"check": check.stdout.strip().splitlines()[-1:], "command": command,
                          "environment_from_runtime_proof": pinned}, indent=2))
        return
    log = data / "collector.log"
    with log.open("a") as stream:
        stream.write(f"\n=== launch {datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='seconds')} ===\n")
    # A new session (setsid) without a terminal; output appends to the log.
    pid = os.posix_spawn(command[0], command, environment, setsid=True, file_actions=[
        (os.POSIX_SPAWN_OPEN, 0, "/dev/null", os.O_RDONLY, 0),
        (os.POSIX_SPAWN_OPEN, 1, str(log), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600),
        (os.POSIX_SPAWN_DUP2, 1, 2)])
    launch = {"pid": pid, "command": command, "log": str(log), "progress": str(data / "progress.json"),
              "environment_from_runtime_proof": pinned,
              "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")}
    (data / "launch.json").write_text(json.dumps(launch, indent=2) + "\n")
    # The collector takes its lock once it has validated its arguments.
    deadline = time.monotonic() + args.wait_seconds
    while time.monotonic() < deadline and not collector_running(data):
        exited, status = os.waitpid(pid, os.WNOHANG)
        if exited:
            raise SystemExit(f"Collector exited immediately ({os.waitstatus_to_exitcode(status)}); see {log}")
        time.sleep(0.2)
    print(json.dumps({**launch, "running": collector_running(data)}, indent=2))


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def status(args):
    data = args.data
    value = {"data": str(data), "running": collector_running(data)}
    for name in ("progress", "launch"):
        path = data / f"{name}.json"
        if path.is_file():
            value[name] = json.loads(path.read_text())
    if "launch" in value:
        value["launch_pid_alive"] = alive(value["launch"]["pid"])
    records = data / "records.jsonl"
    if records.is_file():
        grades = {}
        for line in records.read_text().splitlines():
            grade = json.loads(line).get("grade", {}).get("status", "unknown")
            grades[grade] = grades.get(grade, 0) + 1
        value["recorded_grades"] = grades
    if args.tail and (data / "collector.log").is_file():
        value["log_tail"] = (data / "collector.log").read_text(errors="replace").splitlines()[-args.tail:]
    print(json.dumps(value, indent=2))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    launch = commands.add_parser("start", help="validate, then start the collector detached")
    launch.add_argument("--catalog", required=True, type=Path)
    launch.add_argument("--data", required=True, type=Path)
    launch.add_argument("--active", required=True, type=Path)
    launch.add_argument("--engram-release", required=True, type=Path)
    launch.add_argument("--allow-draft-suite", action="store_true")
    launch.add_argument("--dry-run", action="store_true", help="validate and print the command without starting it")
    launch.add_argument("--wait-seconds", type=float, default=10, help="how long to wait for the collector's lock")
    launch.add_argument("collector_args", nargs=argparse.REMAINDER,
                        help="further run_real_repo_followup.py options after --, e.g. -- --agent-seconds 1200")
    report = commands.add_parser("status", help="print collection progress")
    report.add_argument("--data", required=True, type=Path)
    report.add_argument("--tail", type=int, default=0, help="also print the last N collector log lines")
    args = parser.parse_args(argv)
    args.data = args.data.resolve()
    if args.command == "start":
        if args.collector_args[:1] == ["--"]:
            args.collector_args = args.collector_args[1:]
        args.catalog, args.active = args.catalog.resolve(), args.active.resolve()
        args.engram_release = args.engram_release.expanduser().resolve()
        start(args)
    else:
        status(args)


if __name__ == "__main__":
    main()
