#!/usr/bin/env python3
"""Compare semantic retrieval arms on one frozen snapshot using isolated daemons.

The arms share the snapshot, the authored cases and the daemon binary, and
differ only in their [context] and [semantic] settings:

  off     semantic stage disabled: lexical + structural retrieval (baseline)
  hashed  the built-in hashed n-gram embedder
  local   an OpenAI-compatible embeddings endpoint on a loopback address

Every arm runs on its own disposable copy of the snapshot with background
jobs, watchers, web, vault and judge disabled, so each arm rebuilds its own
vectors explicitly. The report holds case IDs, authored queries, memory IDs,
ranks, scores, counts and latencies; never memory text or compiled context.
Only the local arm calls a model, and only on a loopback address.
Run under caffeinate -dims on macOS.
"""

import argparse
import datetime
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request
import uuid

# Do not create repository-local Python cache files when importing helpers.
sys.dont_write_bytecode = True
from capture_eval import UnixHTTP  # noqa: E402
from eval_context_quality import (  # noqa: E402
    config_text, freeze_snapshot, isolate_checkout_aliases, read_only, sha256, wait_ready,
)

ARMS = ("off", "hashed", "local")
HASHED_ID = "engram-hashed-ngram-v1-d256"
LOOPBACK_HOSTS = {"localhost", "127.0.0.1", "::1"}
LIMITATIONS = [
    "Retrieval diagnostics over authored cases, not an end-to-end coding or savings benchmark.",
    "Relevance comes from the cases' regex patterns over memory claims; a memory can be useful without matching them.",
    "Ranks order the injected memory items of the case's own project by retrieval score; memories that were not injected count as misses.",
    "Arms run sequentially on fresh copies of the same snapshot; latency includes local model inference for the local arm only.",
    "A local arm is valid only if every injected memory names that model as its semantic source; fallbacks are reported, not hidden.",
    "Results describe one model, one snapshot and one case set. They do not establish a general retrieval benefit.",
]


def call(socket_path, route, payload, timeout=120):
    client = UnixHTTP(str(socket_path))
    client.timeout = timeout
    try:
        client.request("POST", route, json.dumps(payload), {"Content-Type": "application/json"})
        response = client.getresponse()
        body = response.read()
    finally:
        client.close()
    if response.status != 200:
        detail = body[:300].decode(errors="replace")
        raise RuntimeError(f"{route} returned HTTP {response.status}: {detail}")
    return json.loads(body)


def loopback(url):
    host = urllib.parse.urlsplit(url).hostname or ""
    return host.lower() in LOOPBACK_HOSTS or host.startswith("127.")


def model_identity(args):
    """Best-effort Ollama digest of the evaluated model, for reproducibility."""
    parts = urllib.parse.urlsplit(args.endpoint)
    if not loopback(args.endpoint):
        return None
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(f"{parts.scheme}://{parts.netloc}/api/tags", timeout=5) as response:
            models = json.loads(response.read(1_048_576)).get("models", [])
    except (OSError, ValueError):
        return None
    names = {args.model, f"{args.model}:latest"}
    for model in models:
        if model.get("name") in names or model.get("model") in names:
            details = model.get("details") or {}
            return {"name": model.get("name"), "digest": model.get("digest"), "bytes": model.get("size"),
                    "family": details.get("family"), "parameter_size": details.get("parameter_size"),
                    "quantization": details.get("quantization_level")}
    return {"name": args.model, "digest": None, "note": "not listed by /api/tags"}


def start_daemon(daemon, root, database, extra_config):
    socket_path = root / "d.sock"
    config = root / "config.toml"
    text = config_text(database, socket_path, root / "vault") + extra_config
    config.write_text(text)
    config.chmod(0o600)
    environment = {key: value for key, value in os.environ.items() if not key.startswith("ENGRAM_")}
    log = (root / "daemon.log").open("wb")
    process = subprocess.Popen([str(daemon), "--config", str(config)], cwd=root, env=environment,
                               stdin=subprocess.DEVNULL, stdout=log, stderr=log)
    log.close()
    return process, socket_path, text


def stop_daemon(process, socket_path):
    if process.poll() is not None:
        return
    try:
        call(socket_path, "/api/v1/admin/shutdown", {}, timeout=10)
        process.wait(timeout=10)
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired):
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def build_synthetic_snapshot(daemon, fixture, snapshot):
    """Create validated fixture memories through the daemon API, then freeze."""
    with tempfile.TemporaryDirectory(prefix="esr-syn-", dir="/tmp") as temp:
        root = Path(temp)
        database = root / "brain.sqlite"
        project_dir = root / fixture["project"]
        project_dir.mkdir()
        process, socket_path, _ = start_daemon(daemon, root, database, "")
        try:
            wait_ready(process, socket_path)
            project_id = call(socket_path, "/api/v1/projects/resolve", {"path": str(project_dir)})["project"]["id"]
            for memory in fixture["memories"]:
                draft = {
                    "scope_type": "project", "scope_id": project_id, "kind": memory.get("kind", "lesson"),
                    "claim": memory["claim"], "rationale": memory.get("rationale", "Synthetic fixture lesson"),
                    "confidence": 0.5, "importance": 0.5, "tags": memory.get("tags", []),
                    "valid_from_commit": None, "valid_to_commit": None, "trust_source": "human_instruction",
                }
                created = call(socket_path, "/api/v1/memories", {"project_id": project_id, "memory": draft})
                call(socket_path, f"/api/v1/memories/{created['id']}/promote",
                     {"target": "validated", "human_override": True})
        finally:
            stop_daemon(process, socket_path)
        freeze_snapshot(database, snapshot)


def load_cases(path, default_project):
    document = json.loads(path.read_text())
    cases = document if isinstance(document, list) else document["cases"]
    budget = None if isinstance(document, list) else document.get("default_token_budget")
    if len({case["id"] for case in cases}) != len(cases):
        raise ValueError("case IDs must be unique")
    for case in cases:
        case.setdefault("project", default_project)
        if not case.get("project") or not case.get("query"):
            raise ValueError(f"case {case['id']} needs a project and a query")
        if not isinstance(case.get("expected_patterns"), list):
            raise ValueError(f"case {case['id']} needs an expected_patterns list")
        if not case["expected_patterns"] and not case.get("negative"):
            raise ValueError(f"case {case['id']} has no patterns; mark negatives with \"negative\": true")
    return cases, budget


def resolve_fixtures(database, cases, aliases):
    projects, relevant = {}, {}
    for case in cases:
        name = aliases.get(case["project"], case["project"])
        if name not in projects:
            rows = database.execute(
                "SELECT id FROM projects WHERE name=? AND tombstoned_at_ms IS NULL", (name,)).fetchall()
            if len(rows) != 1:
                raise ValueError(f"expected exactly one active project named {name!r}")
            projects[name] = rows[0][0]
        project_id = projects[name]
        case["project_id"] = project_id
        claims = database.execute(
            "SELECT id, claim FROM memories WHERE project_id=? AND status='active' "
            "AND epistemic_state='validated'", (project_id,)).fetchall()
        matches = sorted(memory_id for memory_id, claim in claims if case["expected_patterns"]
                         and all(re.search(pattern, claim, re.I) for pattern in case["expected_patterns"]))
        if not matches and not case.get("negative"):
            raise ValueError(f"no validated memory in the snapshot matches case {case['id']}")
        relevant[case["id"]] = matches
    return projects, relevant


def arm_config(arm, args):
    if arm == "off":
        return "[context]\nsemantic_retrieval = false\n"
    lines = ["[context]", "semantic_retrieval = true", "[semantic]"]
    if arm == "hashed":
        lines.append('provider = "hashed"')
    else:
        # JSON strings are valid TOML basic strings.
        lines += [
            'provider = "local"', f"endpoint = {json.dumps(args.endpoint)}", f"model = {json.dumps(args.model)}",
            f"dimension = {args.dimension}", f"batch_size = {args.batch_size}",
            f"timeout_ms = {args.timeout_ms}", f"query_timeout_ms = {args.query_timeout_ms}",
            f"min_similarity = {float(args.min_similarity)!r}",
            f"query_prefix = {json.dumps(args.query_prefix)}", f"document_prefix = {json.dumps(args.document_prefix)}",
        ]
    return "\n".join(lines) + "\n"


def provenance(note):
    marker = note.find("; semantic")
    if marker < 0:
        return "none"
    detail = note[marker + 2:]
    if detail.startswith("semantic skipped"):
        return "skipped"
    served = detail[len("semantic: "):]
    identity = served.split(" ", 1)[0]
    if " fallback (" in served:
        return f"fallback:{identity}"
    if "memories have current vectors" in served:
        return f"partial:{identity}"
    return f"served:{identity}"


def trial(socket_path, case, relevant, budget, repeat, arm):
    request = {
        "request_id": f"semantic_eval_{arm}_{case['id']}_{repeat}_{uuid.uuid4()}", "project_id": case["project_id"],
        "task_text": case["query"], "current_paths": [], "token_budget": budget, "mode": "brain",
        "repository_root": None, "head": None, "task_id": None,
    }
    result = {"case": case["id"], "repeat": repeat, "negative": bool(case.get("negative"))}
    started = time.perf_counter()
    try:
        response = call(socket_path, "/api/v1/context/compile", request)
    except Exception as error:  # Failed trials stay in the denominators.
        result.update({"ok": False, "error_type": type(error).__name__, "error": str(error)[:300],
                       "api_latency_ms": round((time.perf_counter() - started) * 1000, 3)})
        return result
    latency = round((time.perf_counter() - started) * 1000, 3)
    memories = [item for item in response["items"] if item["source_type"] == "memory"]
    ranked = sorted(memories, key=lambda item: -item["score"])
    wanted = set(relevant)
    ranks = [rank for rank, item in enumerate(ranked, 1) if item["source_id"] in wanted]
    result.update({
        "ok": True, "api_latency_ms": latency, "compile_ms": response["metrics"]["compile_ms"],
        "selected_tokens": response["metrics"]["selected_tokens"],
        "budget_pass": response["metrics"]["selected_tokens"] <= budget,
        "injected_memories": len(memories), "relevant_injected": len(ranks),
        "irrelevant_injected": len(memories) - len(ranks), "first_relevant_rank": min(ranks, default=None),
        "provenance": sorted({provenance(item["why"]["note"]) for item in memories}),
        "memory_items": [{"memory_id": item["source_id"], "relevant": item["source_id"] in wanted,
                          "score": round(item["score"], 6), "semantic": round(item["why"]["semantic"], 6),
                          "lexical": round(item["why"]["lexical"], 6)} for item in ranked],
    })
    return result


def off_control(socket_path, case, budget):
    response = call(socket_path, "/api/v1/context/compile", {
        "request_id": f"semantic_eval_off_{case['id']}_{uuid.uuid4()}", "project_id": case["project_id"],
        "task_text": case["query"], "current_paths": [], "token_budget": budget, "mode": "off",
        "repository_root": None, "head": None, "task_id": None,
    })
    return not (response["stable_prefix"] + response["dynamic_context"]) and not response["items"] \
        and response["metrics"]["selected_tokens"] == 0


def summarize(trials, expected_source):
    ok = [item for item in trials if item["ok"]]
    positive = [item for item in ok if not item["negative"]]
    negative = [item for item in ok if item["negative"]]
    ranks = [item["first_relevant_rank"] for item in positive]
    served = [item for item in ok if item["provenance"]]
    clean = [item for item in served if expected_source is None or item["provenance"] == [expected_source]]
    latencies = sorted(item["api_latency_ms"] for item in ok)
    relevant_scores = [entry["semantic"] for item in positive for entry in item["memory_items"] if entry["relevant"]]
    other_scores = [entry["semantic"] for item in positive for entry in item["memory_items"] if not entry["relevant"]]
    return {
        "trials": len(trials), "failed_trials": len(trials) - len(ok), "positive_trials": len(positive),
        "hits_at_1": sum(rank is not None and rank <= 1 for rank in ranks),
        "hits_at_3": sum(rank is not None and rank <= 3 for rank in ranks),
        "hits_at_5": sum(rank is not None and rank <= 5 for rank in ranks),
        "relevant_injected_trials": sum(rank is not None for rank in ranks),
        "mrr": round(sum(1 / rank for rank in ranks if rank) / len(ranks), 4) if ranks else None,
        "mean_injected_memories": round(statistics.mean(item["injected_memories"] for item in ok), 3) if ok else None,
        "mean_irrelevant_injected": round(statistics.mean(item["irrelevant_injected"] for item in positive), 3) if positive else None,
        "negative_trials": len(negative),
        "negative_injected_memories": sum(item["injected_memories"] for item in negative),
        "budget_passes": sum(item["budget_pass"] for item in ok),
        "trials_with_memory_items": len(served),
        "trials_served_by_expected_source": len(clean) if expected_source else None,
        "contaminated_trials": len(served) - len(clean) if expected_source else 0,
        "provenance_counts": {label: sum(label in item["provenance"] for item in served)
                              for label in sorted({label for item in served for label in item["provenance"]})},
        "median_semantic_relevant": round(statistics.median(relevant_scores), 4) if relevant_scores else None,
        "median_semantic_other": round(statistics.median(other_scores), 4) if other_scores else None,
        "api_latency_ms_median": round(statistics.median(latencies), 3) if latencies else None,
        "api_latency_ms_p95": latencies[min(len(latencies) - 1, int(0.95 * len(latencies)))] if latencies else None,
        "compile_ms_median": statistics.median(item["compile_ms"] for item in ok) if ok else None,
    }


def compare(baseline, candidate):
    """Per-case first-relevant-rank changes on the first repeat."""
    first = lambda trials: {item["case"]: item.get("first_relevant_rank") for item in trials
                            if item["repeat"] == 1 and item["ok"] and not item["negative"]}
    before, after = first(baseline), first(candidate)
    better = worse = same = 0
    for case, old in before.items():
        new = after.get(case)
        if new is not None and (old is None or new < old):
            better += 1
        elif old is not None and (new is None or new > old):
            worse += 1
        else:
            same += 1
    return {"cases": len(before), "rank_improved": better, "rank_worsened": worse, "unchanged": same}


def run_arm(args, arm, snapshot, cases, relevant, projects, budget):
    expected = None if arm == "off" else (f"served:{HASHED_ID}" if arm == "hashed" else None)
    record = {"config": arm_config(arm, args), "rebuild": [], "trials": []}
    with tempfile.TemporaryDirectory(prefix=f"esr-{arm}-", dir="/tmp") as temp:
        root = Path(temp)
        copied = root / "brain.sqlite"
        shutil.copyfile(snapshot, copied)
        copied.chmod(0o600)
        record["checkout_aliases_removed"] = isolate_checkout_aliases(copied)
        process, socket_path, _ = start_daemon(args.daemon, root, copied, record["config"])
        try:
            ready = wait_ready(process, socket_path)
            if Path(ready["database"]).resolve() != copied.resolve():
                raise RuntimeError("daemon health did not match the isolated database")
            record["daemon_build_id"] = ready.get("build_id")
            if arm != "off":
                for project_id in sorted(set(projects.values())):
                    started = time.perf_counter()
                    try:
                        rebuilt = call(socket_path, "/api/v1/semantic/rebuild", {"project_id": project_id}, timeout=900)
                    except (OSError, RuntimeError, ValueError) as error:
                        record["rebuild_error"] = str(error)[:500]
                        break
                    rebuilt["seconds"] = round(time.perf_counter() - started, 3)
                    record["rebuild"].append(rebuilt)
                    model = rebuilt.get("model")
                    if arm == "local":
                        if not model or not model["complete"] \
                                or model["embedded"] + model["unchanged"] != model["active_memories"]:
                            record["rebuild_error"] = "model vectors do not cover every active memory"
                            break
                        expected = f"served:{model['embedder']}"
                        record["dimension"] = model["dimension"]
            record["expected_source"] = expected
            record["off_control_passes"] = None
            if "rebuild_error" not in record:
                for repeat in range(1, args.repeats + 1):
                    for case in cases:
                        record["trials"].append(trial(socket_path, case, relevant[case["id"]],
                                                      case.get("token_budget", budget), repeat, arm))
                record["off_control_passes"] = sum(
                    off_control(socket_path, case, case.get("token_budget", budget)) for case in cases)
        finally:
            stop_daemon(process, socket_path)
    record["summary"] = summarize(record["trials"], expected)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--daemon", type=Path, required=True, help="engramd binary to evaluate")
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--snapshot", type=Path, help="frozen SQLite snapshot (see --freeze-from)")
    source.add_argument("--synthetic", type=Path, help="build a disposable snapshot from a fixture of memories and cases")
    parser.add_argument("--freeze-from", type=Path, help="create --snapshot from this database with SQLite backup; refuses to replace a file")
    parser.add_argument("--cases", type=Path, help="cases JSON (list, or {cases, default_token_budget}); defaults to the synthetic fixture's")
    parser.add_argument("--project", action="append", default=[], help="case project alias=registered project name")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--arms", default=None, help="comma-separated subset of off,hashed,local (default: off,hashed plus local with --model)")
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--budget", type=int, help="token budget per compile (default: the cases' default or 1600)")
    parser.add_argument("--model", default="", help="embedding model served by --endpoint; enables the local arm")
    parser.add_argument("--endpoint", default="http://127.0.0.1:11434/v1/embeddings")
    parser.add_argument("--dimension", type=int, default=0)
    parser.add_argument("--batch-size", type=int, default=16)
    parser.add_argument("--timeout-ms", type=int, default=30_000)
    parser.add_argument("--query-timeout-ms", type=int, default=5_000)
    parser.add_argument("--min-similarity", type=float, default=0.0)
    parser.add_argument("--query-prefix", default="")
    parser.add_argument("--document-prefix", default="")
    args = parser.parse_args()
    arms = tuple(args.arms.split(",")) if args.arms else (("off", "hashed", "local") if args.model else ("off", "hashed"))
    if any(arm not in ARMS for arm in arms) or len(set(arms)) != len(arms):
        parser.error("--arms must be distinct values from off,hashed,local")
    if "local" in arms and not args.model:
        parser.error("the local arm needs --model (an embedding model the endpoint serves)")
    if "local" in arms and not loopback(args.endpoint):
        parser.error("--endpoint must be a loopback address")
    if not 1 <= args.repeats <= 10:
        parser.error("--repeats must be between 1 and 10")
    if args.output.exists():
        parser.error("use a new --output path")
    args.daemon = args.daemon.resolve(strict=True)

    fixture = json.loads(args.synthetic.read_text()) if args.synthetic else None
    cases_path = args.cases or args.synthetic
    if cases_path is None:
        parser.error("--cases is required with --snapshot")
    cases, cases_budget = load_cases(cases_path, fixture["project"] if fixture else None)
    budget = args.budget or cases_budget or 1600
    aliases = dict(item.split("=", 1) for item in args.project)

    with tempfile.TemporaryDirectory(prefix="esr-", dir="/tmp") as temp:
        if fixture:
            snapshot = Path(temp) / "synthetic.sqlite"
            build_synthetic_snapshot(args.daemon, fixture, snapshot)
        else:
            snapshot = args.snapshot
            if args.freeze_from:
                freeze_snapshot(args.freeze_from, snapshot)
        snapshot_hash = sha256(snapshot)
        with read_only(snapshot) as frozen:
            projects, relevant = resolve_fixtures(frozen, cases, aliases)
            migration = frozen.execute("SELECT max(version) FROM schema_migrations").fetchone()[0]
        report = {
            "schema_version": 1, "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "harness_sha256": sha256(Path(__file__)), "daemon_sha256": sha256(args.daemon),
            "cases_sha256": sha256(cases_path), "synthetic": bool(fixture),
            "snapshot": {"sha256": snapshot_hash, "bytes": snapshot.stat().st_size, "schema_migration": migration},
            "settings": {"arms": list(arms), "repeats": args.repeats, "token_budget": budget},
            "cases": [{"id": case["id"], "project": case["project"], "query": case["query"],
                       "negative": bool(case.get("negative")), "relevant_memory_ids": relevant[case["id"]]}
                      for case in cases],
            "limitations": LIMITATIONS, "arms": {},
        }
        if "local" in arms:
            report["settings"]["local"] = {
                "endpoint": args.endpoint, "model": args.model, "dimension": args.dimension,
                "batch_size": args.batch_size, "timeout_ms": args.timeout_ms,
                "query_timeout_ms": args.query_timeout_ms, "min_similarity": args.min_similarity,
                "query_prefix": args.query_prefix, "document_prefix": args.document_prefix,
                "model_identity": model_identity(args),
            }
        for arm in arms:
            report["arms"][arm] = run_arm(args, arm, snapshot, cases, relevant, projects, budget)
            print(json.dumps({"arm": arm, **report["arms"][arm]["summary"]}), flush=True)
        report["snapshot_unchanged"] = sha256(snapshot) == snapshot_hash
    report["comparison"] = {
        f"{arm}_vs_{base}": compare(report["arms"][base]["trials"], report["arms"][arm]["trials"])
        for base, arm in (("off", "hashed"), ("off", "local"), ("hashed", "local"))
        if base in report["arms"] and arm in report["arms"]
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x") as output:
        output.write(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report["comparison"], indent=2))
    problems = []
    for arm, record in report["arms"].items():
        summary = record["summary"]
        if "rebuild_error" in record:
            hint = f" (is the model pulled? ollama pull {args.model})" if arm == "local" else ""
            problems.append(f"{arm}: rebuild failed: {record['rebuild_error']}{hint}")
            continue
        if summary["failed_trials"]:
            problems.append(f"{arm}: {summary['failed_trials']} failed trials")
        if summary["budget_passes"] != summary["trials"] - summary["failed_trials"]:
            problems.append(f"{arm}: budget exceeded")
        if record["off_control_passes"] != len(cases):
            problems.append(f"{arm}: brain-off control injected content")
        if summary["contaminated_trials"]:
            problems.append(f"{arm}: {summary['contaminated_trials']} trials not served by {record['expected_source']}")
    if not report["snapshot_unchanged"]:
        problems.append("snapshot changed during evaluation")
    if problems:
        print("INVALID: " + "; ".join(problems), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
