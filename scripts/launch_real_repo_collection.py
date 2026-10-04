#!/usr/bin/env python3
"""Launch, supervise and resume a real-repository collection; report its progress.

`check` verifies the Claude Code login in the trials' own environment (the
runtime proof's coding environment) with `claude auth status`, then runs
run_real_repo_followup.py --check-only: fixtures, the runtime and grader proofs,
the draft-suite flag and any frozen manifest. It starts nothing.

`start` runs the same checks, then starts a supervisor in a new session under
nohup and `caffeinate -dims`, so neither a closed terminal nor idle sleep stops
it. The supervisor checks the login before every attempt and runs the
collector for one attempt at a time (--limit 1, --progress). The collector holds
<data>/collector.lock while it runs; the supervisor holds <data>/supervisor.lock
for the whole collection. Output goes to <data>/collector.log. Every login check
is recorded in <data>/auth_checks.jsonl: logged in or not, auth method and API
provider, never account details. `status` prints progress, both locks, the
latest login check and the recorded inspections.

The collector, runner library, helpers and suite files are frozen in the
collection's manifest. This tool changes none of them, so a stopped collection
stays resumable under its frozen controls.

After an unscorable attempt the collector stops for inspection, and `start`
refuses until the inspection is recorded with `resume`:

- `resume --continue --reason TEXT` keeps the attempt in the planned matrix as
  an unscorable outcome and continues with the next planned attempt. This is the
  rule for any attempt in which the agent ran.
- `resume --acknowledge ATTEMPT --reason TEXT` applies only to the attempt the
  collector stopped on, and only if its agent never started: no tool call and
  no output tokens, as when Claude Code failed to authenticate. The attempt is
  kept, unchanged, as a separate infrastructure attempt outside the paired
  matrix: its record line moves to infrastructure_attempts.jsonl and its trial
  directory to infrastructure_trials/. The collector then runs a fresh attempt
  for that slot. The report lists the kept attempt with the other separate
  infrastructure attempts.

Each decision is appended to <data>/inspections.jsonl with the record and trace
digests and the eligibility facts. `resume` requires a working login first,
unless `--no-launch` records the decision without starting.
"""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
sys.dont_write_bytecode = True
import run_real_repo_pilot as runner  # noqa: E402
import run_real_repo_followup as followup  # noqa: E402
from real_repo_metrics import classify_record  # noqa: E402

COLLECTOR = ROOT / "scripts/run_real_repo_followup.py"
PINNED_ENVIRONMENT = ("PATH", "HOME", "USER", "LOGNAME", "LANG")
CAFFEINATE = "/usr/bin/caffeinate"
NOHUP = "/usr/bin/nohup"
AUTH_LEDGER = "auth_checks.jsonl"
INSPECTIONS = "inspections.jsonl"
PENDING = "inspection.pending.json"
SUPERVISOR_LOCK = "supervisor.lock"
INFRASTRUCTURE_TRIALS = "infrastructure_trials"
SAFE_ID = re.compile(r"[a-z0-9][a-z0-9_-]{0,127}")
SAFE_WORD = re.compile(r"[A-Za-z0-9_.:-]{1,40}")


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")


def lock_held(path):
    if not path.exists():
        return False
    with path.open("a") as handle:
        try:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(handle, fcntl.LOCK_UN)
    return False


def collector_running(data):
    """True while a collector holds the data root's lock."""
    return lock_held(data / "collector.lock")


def supervisor_running(data):
    return lock_held(data / SUPERVISOR_LOCK)


def read_jsonl(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()] if path.is_file() else []


def append_jsonl(path, value):
    with path.open("a") as stream:
        stream.write(json.dumps(value, sort_keys=True) + "\n")


def line_digest(line):
    return hashlib.sha256(line.encode()).hexdigest()


def word(value):
    return value if isinstance(value, str) and SAFE_WORD.fullmatch(value) else None


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


def runtime_identity(data):
    proof = data / "runtime-preflight.json"
    if not proof.is_file():
        raise SystemExit(f"No runtime proof in {data}; run preflight_real_repo_runtime.py first")
    identity = json.loads(proof.read_text()).get("identity") or {}
    if not isinstance(identity.get("coding_environment"), dict) or not isinstance(identity.get("claude_sha256"), str):
        raise SystemExit("The runtime proof records no trial environment or Claude Code binary")
    return identity


def auth_check(data, context, attempt=None, claude=None):
    """`claude auth status` in the trials' environment; records the outcome, never account details.

    The environment is the runtime proof's coding environment, used verbatim
    except for a private temporary TMPDIR and CLAUDE_CODE_TMPDIR, so the check
    cannot create the trial's own Claude temp alias. No model is called.
    """
    identity = runtime_identity(data)
    binary = Path(claude) if claude else runner.CLAUDE
    if not binary.is_file() or runner.digest(binary) != identity["claude_sha256"]:
        raise SystemExit(f"{binary} is not the Claude Code binary of the runtime proof; rerun the runtime preflight")
    environment = dict(identity["coding_environment"])
    with tempfile.TemporaryDirectory(prefix="e26a-", dir="/private/tmp") as scratch:
        environment.update(TMPDIR=scratch, CLAUDE_CODE_TMPDIR=scratch)
        try:
            completed = subprocess.run([str(binary), "auth", "status", "--json"], env=environment, cwd=scratch,
                                       capture_output=True, text=True, timeout=60)
            exit_code, output = completed.returncode, completed.stdout
        except (OSError, subprocess.TimeoutExpired):
            exit_code, output = None, ""
    try:
        status = json.loads(output)
    except ValueError:
        status = None
    status = status if isinstance(status, dict) else {}
    entry = {"schema_version": 1, "checked_at": now(), "context": context, "next_attempt": attempt,
             "logged_in": exit_code == 0 and status.get("loggedIn") is True,
             "auth_method": word(status.get("authMethod")), "api_provider": word(status.get("apiProvider")),
             "exit_code": exit_code, "claude_sha256": identity["claude_sha256"],
             "environment_sha256": hashlib.sha256(json.dumps(identity["coding_environment"], sort_keys=True).encode()).hexdigest()}
    append_jsonl(data / AUTH_LEDGER, entry)
    return entry


def require_login(entry):
    if not entry["logged_in"]:
        raise SystemExit("Claude Code is not logged in for the trial environment (claude auth status: loggedIn is not "
                         f"true; authMethod {entry['auth_method']}, exit {entry['exit_code']}). The operator must run "
                         "`claude auth login`, then retry. Nothing was started or changed.")
    return entry


def collector_path(args):
    return getattr(args, "collector", None) or COLLECTOR


def collector_arguments(args):
    arguments = ["--catalog", str(args.catalog), "--data", str(args.data), "--active", str(args.active),
                 "--engram-release", str(args.engram_release)]
    if args.allow_draft_suite:
        arguments.append("--allow-draft-suite")
    return [*arguments, *args.collector_args]


def planned_ids(args):
    suite = followup.Suite(args.catalog)
    return [f'{task["id"]}-r{repetition}-{arm}' for repetition, task, arm in
            followup.schedule(suite.catalog["tasks"], followup.repetitions(suite.catalog))]


def next_attempt(args):
    done = {record.get("id") for record in read_jsonl(args.data / "records.jsonl")}
    return next((identity for identity in planned_ids(args) if identity not in done), None)


def stopped_attempt(data):
    """The last matrix record, with its exact line, when it is unscorable and not yet inspected."""
    path = data / "records.jsonl"
    lines = [line for line in path.read_text().splitlines() if line.strip()] if path.is_file() else []
    if not lines:
        return None
    record = json.loads(lines[-1])
    if classify_record(record)["grade"]["status"] in ("pass", "fail"):
        return None
    # Only a decision to keep this very line in the matrix settles it; an acknowledged
    # attempt left the matrix, so a later attempt in its place is a new stop.
    decided = {(entry.get("record_sha256"), entry.get("matrix_position")) for entry in read_jsonl(data / INSPECTIONS)
               if entry.get("decision") == "continue_in_matrix"}
    return None if (line_digest(lines[-1]), len(lines)) in decided else (record, lines[-1])


def refuse_busy(data):
    if (data / PENDING).exists():
        raise SystemExit(f"An inspection was interrupted ({data / PENDING}); reconcile records.jsonl, "
                         "infrastructure_attempts.jsonl and infrastructure_trials/ with it before continuing")
    if supervisor_running(data) or collector_running(data):
        raise SystemExit(f"A collection is already running for {data}; see `status`")


def eligibility(record):
    metrics = record.get("metrics") if isinstance(record.get("metrics"), dict) else {}
    return {"classified_status": classify_record(record)["grade"]["status"], "tool_calls": metrics.get("tool_calls"),
            "output_tokens": metrics.get("output_tokens"), "total_tokens": metrics.get("total_tokens"),
            "is_error": metrics.get("is_error"), "agent_exit_code": record.get("agent_exit_code"),
            "agent_wall_seconds": record.get("agent_wall_seconds")}


def agent_never_started(facts):
    """No tool call and no model output: the attempt produced no agent work to select on."""
    return facts["tool_calls"] == 0 and facts["output_tokens"] == 0 and facts["total_tokens"] in (0, None)


def inspect(args):
    """Record the decision on the attempt the collector stopped on; move it out of the matrix if acknowledged."""
    data = args.data
    refuse_busy(data)
    stopped = stopped_attempt(data)
    if stopped is None:
        raise SystemExit("The collection is not stopped on an uninspected unscorable attempt; use `start`")
    record, line = stopped
    identity = record.get("id")
    reason = " ".join(args.reason.split())[:500]
    if not reason:
        raise SystemExit("--reason must describe the inspection")
    if args.acknowledge is not None and args.acknowledge != identity:
        raise SystemExit(f"Only the attempt the collector stopped on, {identity}, can be acknowledged")
    trace = data / "trials" / str(identity) / "trace.jsonl"
    facts = eligibility(record)
    position = len([value for value in (data / "records.jsonl").read_text().splitlines() if value.strip()])
    entry = {"schema_version": 1, "attempt_id": identity, "inspected_at": now(), "reason": reason,
             "decision": "continue_in_matrix" if args.acknowledge is None else "separate_infrastructure_attempt",
             "record_sha256": line_digest(line), "matrix_position": position,
             "trace_sha256": runner.digest(trace) if trace.is_file() else None, "eligibility": facts}
    if args.acknowledge is None:
        append_jsonl(data / INSPECTIONS, entry)
        return entry
    trial = data / "trials" / str(identity)
    if not isinstance(identity, str) or not SAFE_ID.fullmatch(identity) or not trial.is_dir():
        raise SystemExit(f"{identity} has no preserved trial directory to keep")
    if not agent_never_started(facts):
        raise SystemExit(f"{identity}'s agent did work (tool calls or output tokens), so it stays in the matrix; "
                         "use `resume --continue` after inspecting it")
    lines = [value for value in (data / "records.jsonl").read_text().splitlines() if value.strip()]
    if not lines or lines[-1] != line:
        raise SystemExit("records.jsonl changed during the inspection; nothing was moved")
    archive = data / INFRASTRUCTURE_TRIALS
    archive.mkdir(exist_ok=True)
    destination = archive / f"{identity}.a{1 + sum(1 for _ in archive.glob(f'{identity}.a*'))}"
    entry["evidence"] = {"record": "infrastructure_attempts.jsonl", "trial": str(destination.relative_to(data))}
    # A marker first: an interruption below is visible and blocks every launch until reconciled.
    (data / PENDING).write_text(json.dumps(entry, sort_keys=True) + "\n")
    # The record line moves verbatim; it is never edited or replaced in place.
    with (data / "infrastructure_attempts.jsonl").open("a") as stream:
        stream.write(line + "\n")
    trial.rename(destination)
    temporary = data / "records.jsonl.inspection"
    temporary.write_text("".join(value + "\n" for value in lines[:-1]))
    temporary.replace(data / "records.jsonl")
    append_jsonl(data / INSPECTIONS, entry)
    (data / PENDING).unlink()
    return entry


def supervisor_command(args):
    command = [sys.executable, "-B", str(Path(__file__).resolve()), "supervise", "--catalog", str(args.catalog),
               "--data", str(args.data), "--active", str(args.active), "--engram-release", str(args.engram_release)]
    if args.allow_draft_suite:
        command.append("--allow-draft-suite")
    if args.claude:
        command += ["--claude", str(args.claude)]
    if getattr(args, "collector", None):
        command += ["--collector", str(args.collector)]
    return [*command, "--", *args.collector_args]


def start(args, *, login=None):
    data = args.data
    refuse_busy(data)
    stopped = stopped_attempt(data)
    if stopped is not None:
        identity = stopped[0].get("id")
        raise SystemExit(f"The collection stopped on unscorable attempt {identity}. Inspect its evidence, then record "
                         f"the decision: `resume --continue --reason ...` keeps it in the matrix, and `resume --acknowledge "
                         f"{identity} --reason ...` keeps it outside the matrix and re-runs the slot (only if its agent never started).")
    login = login or require_login(auth_check(data, "start", next_attempt(args), args.claude))
    environment, pinned = collector_environment(data)
    base = [sys.executable, "-B", str(collector_path(args)), *collector_arguments(args)]
    check = subprocess.run([*base, "--check-only"], cwd=ROOT, env=environment, text=True, capture_output=True)
    if check.returncode:
        raise SystemExit("Collection check failed; nothing was started.\n" + check.stdout + check.stderr)
    command = [NOHUP, CAFFEINATE, "-dims", *supervisor_command(args)]
    if args.dry_run:
        print(json.dumps({"check": check.stdout.strip().splitlines()[-1:], "login": {"logged_in": login["logged_in"],
                          "auth_method": login["auth_method"]}, "command": command,
                          "environment_from_runtime_proof": pinned}, indent=2))
        return
    log = data / "collector.log"
    with log.open("a") as stream:
        stream.write(f"\n=== launch {now()} ===\n")
    # A new session (setsid) without a terminal; output appends to the log.
    pid = os.posix_spawn(command[0], command, environment, setsid=True, file_actions=[
        (os.POSIX_SPAWN_OPEN, 0, "/dev/null", os.O_RDONLY, 0),
        (os.POSIX_SPAWN_OPEN, 1, str(log), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600),
        (os.POSIX_SPAWN_DUP2, 1, 2)])
    launch = {"pid": pid, "command": command, "log": str(log), "progress": str(data / "progress.json"),
              "environment_from_runtime_proof": pinned, "started_at": now(),
              "login": {"logged_in": login["logged_in"], "auth_method": login["auth_method"], "checked_at": login["checked_at"]}}
    (data / "launch.json").write_text(json.dumps(launch, indent=2) + "\n")
    deadline = time.monotonic() + args.wait_seconds
    while time.monotonic() < deadline and not supervisor_running(data):
        exited, status = os.waitpid(pid, os.WNOHANG)
        if exited:
            code = os.waitstatus_to_exitcode(status)
            if code:
                raise SystemExit(f"Supervisor exited immediately ({code}); see {log}")
            # Nothing was left to run, or it all finished before the first poll.
            print(json.dumps({**launch, "running": False, "exit_code": 0, "progress_state": progress_state(data)}, indent=2))
            return
        time.sleep(0.2)
    print(json.dumps({**launch, "running": supervisor_running(data)}, indent=2))


def resume(args):
    if args.continue_in_matrix == (args.acknowledge is not None):
        raise SystemExit("Choose exactly one of --continue and --acknowledge ATTEMPT")
    login = None
    if not args.no_launch:
        # A working login comes first, so a decision is never recorded for a collection that cannot start.
        refuse_busy(args.data)
        login = require_login(auth_check(args.data, "resume", next_attempt(args), args.claude))
    entry = inspect(args)
    print(json.dumps({"inspection": {key: entry[key] for key in ("attempt_id", "decision", "reason", "evidence") if key in entry}},
                     indent=2))
    if not args.no_launch:
        start(args, login=login)


def note_progress(data, **fields):
    path = data / "progress.json"
    current = {}
    try:
        current = json.loads(path.read_text())
    except (OSError, ValueError):
        pass
    current.update(fields, updated_at=now())
    temporary = path.with_name(path.name + ".supervisor")
    temporary.write_text(json.dumps(current, indent=2, default=str) + "\n")
    temporary.replace(path)


def progress_state(data):
    try:
        return json.loads((data / "progress.json").read_text()).get("state")
    except (OSError, ValueError):
        return None


def supervise(args):
    """Run the frozen collector one attempt at a time, checking the Claude Code login before each attempt."""
    data = args.data
    with (data / SUPERVISOR_LOCK).open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit("Another supervisor holds this data root") from None
        return supervise_attempts(args)


def supervise_attempts(args):
    data = args.data
    command = [sys.executable, "-B", str(collector_path(args)), *collector_arguments(args), "--limit", "1",
               "--progress", str(data / "progress.json")]
    while True:
        if (data / PENDING).exists():
            note_progress(data, state="stopped", error="An inspection is pending", supervisor_pid=os.getpid())
            return 3
        attempt = next_attempt(args)
        if attempt is not None:
            login = auth_check(data, "before_attempt", attempt, args.claude)
            note_progress(data, supervisor_pid=os.getpid(), next_attempt=attempt,
                          login={"logged_in": login["logged_in"], "checked_at": login["checked_at"]})
            if not login["logged_in"]:
                note_progress(data, state="stopped", current=None,
                              error=f"Claude Code is not logged in before {attempt}; run `claude auth login`, then `start`")
                return 2
        completed = subprocess.run(command, cwd=ROOT)
        state = progress_state(data)
        if completed.returncode:
            return completed.returncode
        if state == "completed" or attempt is None:
            return 0
        if state != "limit_reached":
            note_progress(data, state="stopped", error=f"Unexpected collector state {state!r} after {attempt}")
            return 3


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
    value = {"data": str(data), "running": supervisor_running(data) or collector_running(data),
             "supervisor_running": supervisor_running(data), "collector_running": collector_running(data)}
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
            if line.strip():
                grade = classify_record(json.loads(line))["grade"]["status"]
                grades[grade] = grades.get(grade, 0) + 1
        value["recorded_grades"] = grades
    stopped = stopped_attempt(data)
    if stopped is not None:
        value["awaiting_inspection"] = stopped[0].get("id")
    checks = read_jsonl(data / AUTH_LEDGER)
    if checks:
        value["latest_login_check"] = checks[-1]
    inspections = read_jsonl(data / INSPECTIONS)
    if inspections:
        value["inspections"] = [{key: entry.get(key) for key in ("attempt_id", "decision", "reason", "inspected_at")}
                                for entry in inspections]
    if args.tail and (data / "collector.log").is_file():
        value["log_tail"] = (data / "collector.log").read_text(errors="replace").splitlines()[-args.tail:]
    print(json.dumps(value, indent=2))


def check(args):
    login = auth_check(args.data, "check", next_attempt(args), args.claude)
    environment, _ = collector_environment(args.data)
    completed = subprocess.run([sys.executable, "-B", str(collector_path(args)), *collector_arguments(args), "--check-only"],
                               cwd=ROOT, env=environment, text=True, capture_output=True)
    print(json.dumps({"login": {key: login[key] for key in ("logged_in", "auth_method", "api_provider", "exit_code")},
                      "collection": (completed.stdout.strip().splitlines() or [""])[-1] if completed.returncode == 0
                      else (completed.stdout + completed.stderr).strip()[-2000:],
                      "awaiting_inspection": (stopped_attempt(args.data) or [{}])[0].get("id")}, indent=2))
    return 0 if login["logged_in"] and completed.returncode == 0 else 1


def collection_options(parser):
    parser.add_argument("--catalog", required=True, type=Path)
    parser.add_argument("--data", required=True, type=Path)
    parser.add_argument("--active", required=True, type=Path)
    parser.add_argument("--engram-release", required=True, type=Path)
    parser.add_argument("--allow-draft-suite", action="store_true")
    parser.add_argument("--claude", type=Path,
                        help="Claude Code binary for the login check (default: the runner's); it must be the runtime proof's")
    parser.add_argument("--collector", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("collector_args", nargs=argparse.REMAINDER,
                        help="further run_real_repo_followup.py options after --, e.g. -- --agent-seconds 1200")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    verify = commands.add_parser("check", help="check the login and the collection; start nothing")
    collection_options(verify)
    launch = commands.add_parser("start", help="check, then start the supervised collection detached")
    launch.add_argument("--dry-run", action="store_true", help="check and print the command without starting it")
    launch.add_argument("--wait-seconds", type=float, default=10, help="how long to wait for the supervisor's lock")
    collection_options(launch)
    again = commands.add_parser("resume", help="record the inspection of the stopping attempt, then start")
    again.add_argument("--continue", dest="continue_in_matrix", action="store_true",
                       help="keep the attempt in the matrix as unscorable and continue with the next planned attempt")
    again.add_argument("--acknowledge", metavar="ATTEMPT",
                       help="keep the attempt outside the matrix as a separate infrastructure attempt and re-run its slot; "
                            "only for an attempt whose agent never started")
    again.add_argument("--reason", required=True, help="what the inspection found")
    again.add_argument("--no-launch", action="store_true", help="record the decision without starting")
    again.add_argument("--dry-run", action="store_true", help=argparse.SUPPRESS)
    again.add_argument("--wait-seconds", type=float, default=10, help=argparse.SUPPRESS)
    collection_options(again)
    run = commands.add_parser("supervise", help=argparse.SUPPRESS)
    collection_options(run)
    report = commands.add_parser("status", help="print collection progress")
    report.add_argument("--data", required=True, type=Path)
    report.add_argument("--tail", type=int, default=0, help="also print the last N collector log lines")
    args = parser.parse_args(argv)
    args.data = args.data.resolve()
    if args.command == "status":
        return status(args)
    if args.collector_args[:1] == ["--"]:
        args.collector_args = args.collector_args[1:]
    args.catalog, args.active = args.catalog.resolve(), args.active.resolve()
    args.engram_release = args.engram_release.expanduser().resolve()
    if args.claude:
        args.claude = args.claude.resolve()
    if args.collector:
        args.collector = args.collector.resolve()
    if args.command == "check":
        return check(args)
    if args.command == "start":
        return start(args)
    if args.command == "resume":
        return resume(args)
    return supervise(args)


if __name__ == "__main__":
    sys.exit(main())
