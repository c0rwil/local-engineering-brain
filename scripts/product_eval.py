#!/usr/bin/env python3
"""Deterministic product/onboarding contract evaluation against an isolated daemon.

No model, network service, user configuration or live brain is used. This tests
retrieval and lifecycle correctness; it does not measure human onboarding speed.
"""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import time

from capture_eval import api


def evaluate(binary, workspace):
    checks = []
    timings = {}
    socket_path = str(workspace / "daemon.sock")
    def check(name, condition):
        checks.append({"name": name, "passed": bool(condition)})
    def call(route, **payload):
        return api(socket_path, "/api/v1/" + route, payload)
    def query(operation, **payload):
        return call("product/query", operation=operation, **payload)
    def save(record_id, kind, value, version=0):
        return call("product/mutate", operation="save", id=record_id,
                    expected_version=version, item={"type": kind, "value": value})
    def entity(record_id, kind, **value):
        return save(record_id, "entity", {"kind": kind, "name": record_id, **value})
    def lesson(operation, **payload):
        return call("product/lessons", operation=operation, **payload)
    config = workspace / "config.toml"
    config.write_text(f'''[storage]
db_path = {json.dumps(str(workspace / 'brain.sqlite'))}
[daemon]
socket_path = {json.dumps(socket_path)}
[web]
enabled = false
[vault]
enabled = false
root = {json.dumps(str(workspace / 'unused-vault'))}
[watch]
enabled = false
[jobs]
enabled = false
[architect]
auto = "off"
''')
    with (workspace / "daemon.log").open("w") as log:
        daemon = subprocess.Popen([str(binary), "--config", str(config)], stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 30
            while not Path(socket_path).exists():
                if daemon.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("isolated daemon did not start")
                time.sleep(0.05)
            projects = {}
            for name, code in {
                "api": 'pub fn publish() -> &\'static str { "job.created.v1" }\n',
                "worker": 'pub fn consume() -> &\'static str { "job.created.v1" }\n',
            }.items():
                repo = workspace / name
                repo.mkdir()
                (repo / "lib.rs").write_text(code)
                (repo / "other.rs").write_text("pub fn unrelated() {}\n")
                projects[name] = call("projects/index", path=str(repo))["resolved"]["project"]["id"]
            anchor = lambda name, path="lib.rs": {"project_id": projects[name], "path": path}
            entity("product", "product", intent="Complete queued jobs safely")
            entity("jobs", "capability", parent_id="product", intent="Create and execute jobs")
            entity("api", "service", parent_id="jobs", project_id=projects["api"], anchors=[anchor("api")])
            entity("worker", "service", parent_id="jobs", project_id=projects["worker"], anchors=[anchor("worker")])
            entity("queue", "queue", parent_id="jobs")
            entity("unrelated", "service", parent_id="product")
            entity("unanchored-worker", "service", parent_id="jobs")
            save("unanchored-consume", "dependency", {"source_id": "unanchored-worker", "target_id": "queue",
                 "relation": "consumes", "contract": "job.created.v1", "environment": "production"})
            for key, source, relation, environment in [
                ("publish", "api", "publishes", "production"),
                ("consume", "worker", "consumes", "production"),
                ("stage", "unrelated", "consumes", "staging"),
            ]:
                save(key, "dependency", {"source_id": source, "target_id": "queue", "relation": relation,
                     "contract": "job.created.v1", "environment": environment, "anchors": [anchor("api")]})
            entity("flow", "flow", parent_id="jobs", steps=[
                {"entity_id": "api", "title": "Publish job", "anchors": [anchor("api")]},
                {"entity_id": "worker", "title": "Consume job", "anchors": [anchor("worker")]},
            ])
            for key, path in [("lifecycle", "lib.rs"), ("unrelated-doc", "other.rs")]:
                save(key, "section", {"entity_id": "api", "title": key,
                     "curated": "Keep human intent intact", "reviewed": True, "anchors": [anchor("api", path)]})
            start = time.monotonic()
            bundle_args = {"entity_id": "api", "task": "Change the queued job contract",
                           "token_budget": 4000, "environment": "production"}
            bundle = query("bundle", **bundle_args)
            timings["bundle_ms"] = round((time.monotonic()-start)*1000, 3)
            check("cross_repo_consumer_in_context", "worker" in bundle["included_ids"])
            check("unrelated_environment_excluded", "unrelated" not in bundle["included_ids"])
            check("purpose_and_contract_delivered", "Complete queued jobs safely" in bundle["text"] and "job.created.v1" in bundle["text"])
            check("deterministic_bundle", bundle == query("bundle", **bundle_args))
            small = query("bundle", **{**bundle_args, "task": "🧪" * 200, "token_budget": 128})
            check("hard_context_budget", small["tokens"] <= 128 and (len(small["text"].encode()) + 3)//4 <= 128)
            impact = query("impact", project_id=projects["api"], diff="--- a/lib.rs\n+++ b/lib.rs\n", environment="production")
            check("changed_service_direct", "api" in [r["id"] for r in impact["direct"]])
            check("consumer_possible_not_proven", "worker" in [r["id"] for r in impact["possible"]] and bool(impact["gaps"]))
            check("unanchored_consumer_reached_by_traversal", "unanchored-worker" in [r["id"] for r in impact["possible"]])
            check("staging_impact_excluded", "unrelated" not in [r["id"] for r in impact["possible"]])
            source = query("source", anchor=anchor("api"))
            check("inspectable_source", "publish" in json.dumps(source))
            (workspace / "api/lib.rs").write_text('pub fn publish() -> &\'static str { "job.created.v2" }\n')
            call("projects/index", path=str(workspace / "api"))
            records = {r["id"]: r for r in query("snapshot")["records"]}
            check("behavior_change_marks_document_stale", records["lifecycle"]["freshness"] == "needs_review")
            check("unrelated_document_stays_current", records["unrelated-doc"]["freshness"] == "verified")
            refreshed = call("product/mutate", operation="refresh_section", id="lifecycle", expected_version=1)
            check("refresh_preserves_human_intent", refreshed["item"]["value"]["curated"] == "Keep human intent intact")
            check("generated_refresh_requires_review", not refreshed["item"]["value"]["reviewed"])
            check("immutable_section_history", len(query("history", id="lifecycle")) == 2)
            task = call("tasks", project_id=projects["api"], prompt="Observe production traffic", session_id="product-eval")
            completion = {"task_id": task["id"], "outcome": "success", "summary": "Traffic remains pinned until shifted",
                          "verifications": [{"kind": "runtime_observation", "strength": "observational", "command": "fixture describe traffic",
                          "exit_code": 0, "artifact": "fixture: old revision receives 100%; latest receives 0%"}]}
            finished = lesson("complete", **completion)
            repeat = lesson("complete", **completion)
            check("completion_retry_idempotent", finished["evidence_ids"] == repeat["evidence_ids"] and finished["reviews"][0]["id"] == repeat["reviews"][0]["id"])
            pending = query("bundle", **{**bundle_args, "task": "traffic pinned revision"})
            check("pending_lesson_not_injected", "Traffic remains pinned until shifted" not in pending["text"])
            proposal = finished["reviews"][0]
            accepted = lesson("review", id=proposal["id"], expected_updated_at_ms=proposal["updated_at_ms"], decision="accept")
            delivered = query("bundle", **{**bundle_args, "task": "traffic pinned revision"})
            check("reviewed_observation_delivered", accepted["memory_id"] in delivered["included_ids"] and "Traffic remains pinned until shifted" in delivered["text"])
            signals = query("task_signals", task_id=task["id"])
            check("review_is_not_user_acceptance", signals["accepted_by_user"] is None)
            unsupported_task = call("tasks", project_id=projects["worker"], prompt="Model speculation", session_id="product-eval")
            unsupported = lesson("complete", task_id=unsupported_task["id"], outcome="success", summary="Unsupported hypothesis")
            proposal = unsupported["reviews"][0]
            rejected = False
            try:
                lesson("review", id=proposal["id"], expected_updated_at_ms=proposal["updated_at_ms"], decision="accept")
            except RuntimeError:
                rejected = True
            check("unsupported_claim_cannot_validate", rejected)
            check("vault_not_required", not (workspace / "unused-vault").exists())
        finally:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()
    return {"checks": checks, "passed": sum(c["passed"] for c in checks), "total": len(checks), "timings": timings}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", type=Path, default=Path("target/release/engramd"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.daemon.resolve()
    with tempfile.TemporaryDirectory(prefix="engram-product-eval-", dir="/tmp") as temp:
        result = evaluate(binary, Path(temp))
    result.update(generated_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  daemon_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                  evaluator_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                  limitations=["Synthetic two-repository fixture, no model or human participants.",
                                "Correctness checks do not establish task-success or onboarding-time improvement."])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    raise SystemExit(0 if result["passed"] == result["total"] else 1)


if __name__ == "__main__":
    main()
