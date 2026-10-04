#!/usr/bin/env python3
"""Run synthetic evidence decisions against the same local API/prompt as Engram.

This is a small adversarial smoke evaluation, not statistical calibration. It
does not read user repositories, touch the brain, or apply generated decisions.
The result separates model decisions from schema/citation checks. Production
also checks provenance and live source freshness before and after inference.
"""

import argparse
import datetime
import hashlib
import ipaddress
import json
from pathlib import Path
import re
import statistics
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


ROOT = Path(__file__).resolve().parents[1]
MAX_RESPONSE_BYTES = 65_536
VERDICT_FIELDS = {"decision", "claim", "reason", "citations", "contradicts_memory_ids"}
RUST_WHITESPACE = "".join(map(chr, [*range(9, 14), 0x20, 0x85, 0xA0, 0x1680,
                                   *range(0x2000, 0x200B), 0x2028, 0x2029, 0x202F, 0x205F, 0x3000]))


def system_prompt(path):
    match = re.search(r'const SYSTEM_PROMPT: &str = r#"(.*?)"#;', path.read_text(), re.S)
    if match is None:
        raise ValueError("Cannot locate exact SYSTEM_PROMPT raw literal in src/judge.rs")
    return match.group(1)


def strict_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"Duplicate JSON field: {key}")
        result[key] = value
    return result


def invalid_constant(value):
    raise ValueError(f"Invalid JSON number: {value}")


def synthetic_id(prefix, name):
    return f"{prefix}_{uuid.uuid5(uuid.NAMESPACE_URL, 'engram-local-judge-eval:' + name)}"


def valid_id(value, prefix):
    if not isinstance(value, str) or not value.startswith(prefix + "_"):
        return False
    suffix = value[len(prefix) + 1:]
    # Rust Uuid::parse_str accepts these four exact shapes. Python's UUID
    # constructor also strips misplaced/trailing hyphens, which Rust rejects.
    hex_group = "[0-9a-fA-F]"
    hyphenated = f"{hex_group}{{8}}-{hex_group}{{4}}-{hex_group}{{4}}-{hex_group}{{4}}-{hex_group}{{12}}"
    if re.fullmatch(f"(?:{hex_group}{{32}}|{hyphenated}|\\{{{hyphenated}\\}}|urn:uuid:{hyphenated})", suffix) is None:
        return False
    try:
        uuid.UUID(suffix)
    except ValueError:
        return False
    return True


def packet(case):
    revision = "a" * 40
    project_id = synthetic_id("prj", "synthetic-project")
    task_id = synthetic_id("tsk", case["id"])
    evidence = []
    for index, source in enumerate(case.get("evidence", [])):
        excerpt = source["artifact_excerpt"]
        evidence.append({
            "id": source.get("id", synthetic_id("evd", f"{case['id']}:{index}")),
            "eligible": source.get("eligible", True),
            "kind": source.get("kind", "test_result"),
            "command": source.get("command", "python -m unittest synthetic_fixture"),
            "exit_code": source.get("exit_code", 0),
            "revision": source.get("revision", revision),
            "artifact_digest": hashlib.sha256(excerpt.encode()).hexdigest(),
            "artifact_excerpt": excerpt,
            "excerpt_truncated": source.get("excerpt_truncated", False),
        })
    return {
        "review": {
            "id": f"review_{case['id']}", "task_id": task_id,
            "project_id": project_id, "claim": case["claim"],
            "rationale": case.get("rationale", "Evaluate the proposed rule against actual observations."),
            "evidence_ids": [source["id"] for source in evidence],
            "status": "pending", "memory_id": None, "expires_at_ms": None,
            "updated_at_ms": 1_790_000_000_000,
            "related_memories": case.get("related_memories", []),
        },
        "task": {
            "id": task_id, "project_id": project_id, "session_id": "synthetic-session",
            "status": "success", "prompt": case.get("intent", "Verify synthetic behavior"),
            "accepted_by_user": None, "started_at_ms": 1_789_999_999_000,
            "completed_at_ms": 1_790_000_000_000,
            "final_commit": revision,
        },
        "evidence": evidence,
        "source_anchors": {
            "files": [{"path": "src/fixture.py", "revision": revision,
                       "content_digest": "synthetic-current-digest", "indexed_at_ms": 1_789_999_999_000}],
            "current_scan": [revision, "synthetic-clean"],
            "live": {"head": revision, "files": [{"path": "src/fixture.py",
                     "indexed_digest": "synthetic-current-digest", "live_digest": "synthetic-current-digest"}]},
        },
    }


def verdict_errors(verdict, data):
    """Mirror strict JSON fields, bounds and accept citation gates in judge.rs."""
    if not isinstance(verdict, dict) or set(verdict) != VERDICT_FIELDS:
        return ["Verdict must contain every required field and no extra fields"]
    errors = []
    if verdict["decision"] not in ("accept", "reject", "defer"):
        errors.append("Invalid decision")
    for field in ("claim", "reason"):
        value = verdict[field]
        if not isinstance(value, str) or not value.strip(RUST_WHITESPACE) or len(value.encode()) > 4096:
            errors.append(f"Invalid {field}")
    citations = verdict["citations"]
    conflicts = verdict["contradicts_memory_ids"]
    if not isinstance(citations, list) or len(citations) > 32:
        errors.append("Invalid citations list")
        citations = []
    if not isinstance(conflicts, list) or len(conflicts) > 8 or any(not valid_id(i, "mem") for i in conflicts):
        errors.append("Invalid conflicting memory IDs")
    if verdict["decision"] == "accept" and (not citations or conflicts):
        errors.append("Acceptance needs citations and no conflicting memory IDs")
    for citation in citations:
        if (not isinstance(citation, dict) or set(citation) != {"evidence_id", "quote"}
                or not isinstance(citation["quote"], str) or not valid_id(citation["evidence_id"], "evd")):
            errors.append("Invalid citation schema")
            continue
        if verdict["decision"] != "accept":
            continue
        source = next((e for e in data["evidence"] if e["id"] == citation["evidence_id"] and e["eligible"]), None)
        quote = citation["quote"]
        if source is None:
            errors.append("Citation is not eligible evidence")
        elif len(quote.strip(RUST_WHITESPACE).encode()) < 8 or len(quote.encode()) > 2048 or quote not in source["artifact_excerpt"]:
            errors.append("Citation is not an exact 8–2048 byte eligible excerpt")
    return errors


def parse_verdict(response):
    choices = response.get("choices")
    if not isinstance(choices, list) or not choices or not isinstance(choices[0], dict):
        raise ValueError("Runtime returned no choice")
    choice = choices[0]
    # Match Value::as_str in production: missing/null/non-string finish reasons
    # are ignored there; only a string other than 'stop' rejects completion.
    finish = choice.get("finish_reason")
    if isinstance(finish, str) and finish != "stop":
        raise ValueError(f"Incomplete response: {finish}")
    message = choice.get("message")
    if not isinstance(message, dict):
        raise ValueError("Runtime returned no message object")
    if "tool_calls" in message:
        raise ValueError("Runtime attempted a tool call")
    if not isinstance(message.get("content"), str):
        raise ValueError("Runtime returned no text")
    return json.loads(message["content"], object_pairs_hook=strict_object, parse_constant=invalid_constant)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, new_url):
        raise ValueError(f"Local runtime redirect refused (HTTP {code})")


def local_endpoint(value):
    url = urllib.parse.urlsplit(value)
    if url.scheme not in ("http", "https") or url.username or url.password or url.query or url.fragment:
        raise argparse.ArgumentTypeError("Use an HTTP(S) loopback endpoint without credentials, query or fragment")
    if url.hostname == "localhost":
        # Match the daemon's loopback pinning and avoid DNS/hosts-file surprises.
        return urllib.parse.urlunsplit((url.scheme, f"127.0.0.1:{url.port or (443 if url.scheme == 'https' else 80)}", url.path, "", ""))
    try:
        if not ipaddress.ip_address(url.hostname or "").is_loopback:
            raise ValueError()
    except ValueError as error:
        raise argparse.ArgumentTypeError("Only a numeric loopback address or localhost is allowed") from error
    return value


def run_case(case, prompt, args, opener):
    data = packet(case)
    body = {
        "model": args.model,
        "messages": [{"role": "system", "content": prompt},
                     {"role": "user", "content": json.dumps(data, separators=(",", ":"))}],
        "stream": False, "temperature": 0, "max_tokens": args.max_output_tokens,
        "response_format": {"type": "json_object"},
    }
    result = {"id": case["id"], "category": case["category"],
              "expected_accept": case["expected_accept"], "expected_reason": case["expected_reason"],
              "decision": None, "verdict": None, "usage": None, "runtime_model": None,
              "validation_errors": [], "request_error": None}
    started = time.monotonic()
    try:
        request = urllib.request.Request(args.endpoint, json.dumps(body).encode(), {"Content-Type": "application/json"})
        with opener.open(request, timeout=args.timeout) as response:
            raw = response.read(MAX_RESPONSE_BYTES + 1)
        if len(raw) > MAX_RESPONSE_BYTES:
            raise ValueError("Response exceeds 64 KiB")
        response = json.loads(raw, parse_constant=invalid_constant)
        if not isinstance(response, dict):
            raise ValueError("Runtime returned no response object")
        result["usage"] = response.get("usage")
        result["runtime_model"] = response.get("model")
        verdict = parse_verdict(response)
        result["verdict"] = verdict
        result["decision"] = verdict.get("decision") if isinstance(verdict, dict) else None
        result["validation_errors"] = verdict_errors(verdict, data)
    except (ValueError, KeyError, IndexError, TypeError, OSError, urllib.error.URLError) as error:
        result["request_error"] = str(error)
    result["duration_ms"] = round((time.monotonic() - started) * 1000)
    result["valid_response"] = not result["request_error"] and not result["validation_errors"]
    result["accepted_after_citation_checks"] = result["valid_response"] and result["decision"] == "accept"
    result["decision_correct"] = result["valid_response"] and (result["decision"] == "accept") == result["expected_accept"]
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", type=local_endpoint, default="http://127.0.0.1:8010/v1/chat/completions")
    parser.add_argument("--model", required=True)
    parser.add_argument("--timeout", type=float, default=90)
    parser.add_argument("--max-output-tokens", type=int, default=1200)
    parser.add_argument("--cases", type=Path, default=ROOT / "tests/fixtures/local_judge_cases.json")
    parser.add_argument("--judge-source", type=Path, default=ROOT / "src/judge.rs")
    parser.add_argument("--case", action="append", help="Run selected case ID; repeat to select several")
    parser.add_argument("--output", type=Path, help="Also save the JSON report")
    parser.add_argument("--dry-run", action="store_true", help="Validate fixtures and show IDs without model calls")
    args = parser.parse_args()
    if not 1 <= args.timeout <= 300 or not 128 <= args.max_output_tokens <= 4096:
        parser.error("Use timeout 1–300 seconds and max-output-tokens 128–4096")
    prompt = system_prompt(args.judge_source)
    cases = json.loads(args.cases.read_text())
    if not isinstance(cases, list) or not cases or len(cases) > 32:
        parser.error("Expected between 1 and 32 cases")
    if len({case["id"] for case in cases}) != len(cases):
        parser.error("Case IDs must be unique")
    if args.case:
        unknown = set(args.case) - {case["id"] for case in cases}
        if unknown:
            parser.error(f"Unknown case IDs: {sorted(unknown)}")
        cases = [case for case in cases if case["id"] in args.case]
    for case in cases:
        if not isinstance(case["expected_accept"], bool):
            parser.error("Each expected_accept must be a JSON boolean")
        if len(json.dumps(packet(case)).encode()) + len(prompt.encode()) > 49_152:
            parser.error(f"Case exceeds default input budget: {case['id']}")
    if args.dry_run:
        print(json.dumps({"cases": [case["id"] for case in cases], "prompt_sha256": hashlib.sha256(prompt.encode()).hexdigest()}, indent=2))
        return 0
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    results = []
    for case in cases:
        result = run_case(case, prompt, args, opener)
        results.append(result)
        print(f"{case['id']}: {result['decision'] or 'error'}, correct={result['decision_correct']}, {result['duration_ms']} ms", file=sys.stderr, flush=True)
    report = {
        "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "model": args.model, "endpoint": args.endpoint, "timeout_seconds": args.timeout,
        "max_output_tokens": args.max_output_tokens,
        "prompt_sha256": hashlib.sha256(prompt.encode()).hexdigest(),
        "fixtures_sha256": hashlib.sha256(args.cases.read_bytes()).hexdigest(),
        "summary": {
            "cases": len(results), "correct": sum(r["decision_correct"] for r in results),
            "valid_responses": sum(r["valid_response"] for r in results),
            "expected_accepts": sum(r["expected_accept"] for r in results),
            "raw_false_accepts": sum(r["decision"] == "accept" and not r["expected_accept"] for r in results),
            "false_accepts_after_citation_checks": sum(r["accepted_after_citation_checks"] and not r["expected_accept"] for r in results),
            "false_rejections_or_deferrals": sum(r["valid_response"] and r["decision"] != "accept" and r["expected_accept"] for r in results),
            "median_duration_ms": statistics.median(r["duration_ms"] for r in results),
        },
        "limitations": [
            "Small synthetic smoke evaluation; not statistical calibration or proof of real-repository decision quality.",
            "Expected non-accept includes reject or defer. Malformed answers and runtime errors never count as correct.",
            "Production also verifies task provenance and current source revisions before/after inference; this tests raw model behavior plus verdict shape and exact citation checks.",
            "Usage is copied only when reported by the runtime. Wall time includes model warm-up; no monetary savings are inferred.",
        ],
        "results": results,
    }
    output = json.dumps(report, indent=2)
    if args.output:
        args.output.write_text(output + "\n")
    print(output)
    return 0 if all(r["decision_correct"] for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())
