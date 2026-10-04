"""Standard-library harness for the held-out Python graders.

The graders import the candidate's own modules from a disposable grading copy
with package initializers and external services replaced by small stubs. No
dependency installation, database, network or subprocess is used. Exit codes:
0 pass, 1 behavioral failure, 2 infrastructure failure.
"""

from __future__ import annotations

import argparse
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import signal
import sys
import time
import traceback
import types

PROTECTED_ROOTS = (Path.home() / "PycharmProjects",)
_DENIED_EVENTS = {"socket.connect", "socket.getaddrinfo", "subprocess.Popen", "os.system", "os.exec", "os.posix_spawn"}


class InfrastructureError(Exception):
    """The grader could not evaluate the candidate."""


def refuse_protected(repo: Path) -> None:
    for root in PROTECTED_ROOTS:
        root = root.resolve()
        if repo == root or root in repo.parents:
            raise InfrastructureError(f"Refusing to grade inside {root}; supply a disposable grading copy")


def isolate_process(environment: dict[str, str]) -> None:
    """Clear inherited configuration and deny outbound network or subprocesses."""
    os.environ.clear()
    os.environ.update(environment)

    def deny(event, args):
        if event in _DENIED_EVENTS:
            raise RuntimeError(f"Grader isolation forbids {event}")

    sys.addaudithook(deny)


def module(name: str, **attrs) -> types.ModuleType:
    value = types.ModuleType(name)
    value.__dict__.update(attrs)
    sys.modules[name] = value
    if "." in name:
        parent, child = name.rsplit(".", 1)
        if parent in sys.modules:
            setattr(sys.modules[parent], child, value)
    return value


def package(name: str, directory: Path) -> types.ModuleType:
    """A namespace for a real package directory whose __init__ is not executed."""
    value = module(name)
    value.__path__ = [str(directory)]
    value.__package__ = name
    return value


def load(name: str, path: Path) -> types.ModuleType:
    if not path.is_file():
        raise FileNotFoundError(f"Candidate module missing: {path}")
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    sys.modules[name] = value
    if "." in name:
        parent, child = name.rsplit(".", 1)
        if parent in sys.modules:
            setattr(sys.modules[parent], child, value)
    spec.loader.exec_module(value)
    return value


class _CheckTimeout(Exception):
    pass


def _alarm(signum, frame):
    raise _CheckTimeout("check exceeded its time limit")


def run_check(name: str, function, timeout_seconds: int = 30) -> dict:
    started = time.monotonic()
    previous = signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(timeout_seconds)
    try:
        outcome = function()
        if asyncio.iscoroutine(outcome):
            asyncio.run(outcome)
        check = {"name": name, "passed": True}
    except _CheckTimeout as error:
        check = {"name": name, "passed": False, "error": str(error), "timed_out": True}
    except Exception as error:  # noqa: BLE001 - every candidate failure is reported
        check = {"name": name, "passed": False, "error": f"{type(error).__name__}: {error}",
                 "traceback": traceback.format_exc(limit=8)[-4000:]}
    finally:
        signal.alarm(0)
        signal.signal(signal.SIGALRM, previous)
    check["seconds"] = round(time.monotonic() - started, 3)
    return check


def main(task_id: str, load_candidate, checks, environment: dict[str, str], argv: list[str] | None = None) -> int:
    """Load the candidate, run each named check and write the result JSON.

    load_candidate(repo) returns a context object passed to every check, or
    raises: an import/syntax failure in candidate code is a behavioral failure,
    InfrastructureError is not.
    """
    parser = argparse.ArgumentParser(description=f"Private behavioral grader for {task_id}")
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args(argv)
    repo, output = args.repo.resolve(), args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    result: dict = {"task": task_id, "status": "infra", "checks": [], "python": sys.version.split()[0]}
    started = time.monotonic()
    try:
        refuse_protected(repo)
        if not (repo / "src").is_dir():
            raise InfrastructureError("Grading copy has no src directory")
        isolate_process(environment)
        try:
            context = load_candidate(repo)
            result["checks"].append({"name": "candidate_modules_load", "passed": True})
        except InfrastructureError:
            raise
        except Exception as error:  # noqa: BLE001
            result["checks"].append({"name": "candidate_modules_load", "passed": False,
                                     "error": f"{type(error).__name__}: {error}",
                                     "traceback": traceback.format_exc(limit=8)[-4000:]})
            context = None
        if context is not None:
            for name, function in checks:
                result["checks"].append(run_check(name, lambda function=function: function(context)))
        result["status"] = "pass" if all(check["passed"] for check in result["checks"]) else "fail"
    except InfrastructureError as error:
        result["error"] = str(error)
    result["elapsed_seconds"] = round(time.monotonic() - started, 3)
    output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"status": result["status"], "output": str(output)}))
    return {"pass": 0, "fail": 1, "infra": 2}[result["status"]]
