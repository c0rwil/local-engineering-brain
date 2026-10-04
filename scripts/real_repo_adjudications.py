"""Evidence-bound, post-collection exclusions; raw grades and usage stay intact.

This module never upgrades a result to pass or writes trial evidence. Call it on
unredacted records before publishing. Invalid evidence raises AdjudicationError;
callers must surface the error instead of silently dropping a trial.
"""
from __future__ import annotations

from copy import deepcopy
import hashlib
import json
from pathlib import Path
import re
from typing import Any

SCHEMA_VERSION = 1
EXPLANATIONS = {
    "ambiguous_grader_contract": (
        "A reviewed grader requirement was not established by the delivered task and repository contract. "
        "The original behavioral result is preserved, but this attempt is "
        "unscorable for effectiveness; its observed resource use remains included."
    ),
}
_ID = re.compile(r"[a-z0-9][a-z0-9_-]{0,127}\Z")
_SHA = re.compile(r"[0-9a-f]{64}\Z")


class AdjudicationError(ValueError):
    """Invalid or stale adjudication evidence. Messages contain no private data."""


def _safe_id(value: Any) -> bool:
    return isinstance(value, str) and _ID.fullmatch(value) is not None


def _unique_object(pairs: list[tuple[str, Any]]) -> dict:
    result = {}
    for key, value in pairs:
        if key in result:
            raise AdjudicationError("Duplicate JSON fields are not valid adjudication evidence.")
        result[key] = value
    return result


def _decode(contents: bytes) -> Any:
    try:
        return json.loads(contents, object_pairs_hook=_unique_object)
    except (UnicodeError, ValueError) as error:
        raise AdjudicationError("Adjudication evidence must be valid, unambiguous JSON.") from error


def _validate_entries(adjudications: Any) -> None:
    if not isinstance(adjudications, dict):
        raise AdjudicationError("Adjudications must be keyed by trial identity.")
    for identity, entry in adjudications.items():
        if not _safe_id(identity):
            raise AdjudicationError("Adjudication trial identity is unsafe.")
        if not isinstance(entry, dict) or set(entry) != {"grade_sha256", "failed_check", "reason"}:
            raise AdjudicationError("Adjudication entry has an unsupported schema.")
        digest = entry["grade_sha256"]
        if not isinstance(digest, str) or _SHA.fullmatch(digest) is None:
            raise AdjudicationError("Adjudication requires an exact SHA-256 grade digest.")
        reason = entry["reason"]
        if not isinstance(reason, str) or reason not in EXPLANATIONS:
            raise AdjudicationError("Adjudication reason is unsupported.")
        name = entry["failed_check"]
        if not isinstance(name, str) or not name.strip() or len(name) > 4096 or any(ord(c) < 32 for c in name):
            raise AdjudicationError("Adjudication requires a valid failed-check name.")


def load_adjudications(data_root: str | Path) -> dict[str, dict]:
    """Read versioned adjudications.json; absence means no reviewed exclusions."""
    path = Path(data_root) / "adjudications.json"
    try:
        contents = path.read_bytes()
    except FileNotFoundError:
        return {}
    except OSError as error:
        raise AdjudicationError("Could not read adjudication evidence.") from error
    document = _decode(contents)
    if not isinstance(document, dict) or set(document) != {"schema_version", "adjudications"}:
        raise AdjudicationError("Adjudication document has an unsupported schema.")
    if type(document["schema_version"]) is not int or document["schema_version"] != SCHEMA_VERSION:
        raise AdjudicationError("Adjudication schema version is unsupported.")
    _validate_entries(document["adjudications"])
    return document["adjudications"]


def _semantic_grade(grade: Any) -> tuple[str, dict[str, bool]]:
    """Compare outcomes, not log paths, timings, or arbitrary failure messages."""
    if not isinstance(grade, dict) or grade.get("status") not in ("pass", "fail"):
        raise AdjudicationError("Original behavioral grade is malformed or unscorable.")
    checks = grade.get("checks")
    if not isinstance(checks, list) or not checks:
        raise AdjudicationError("Original behavioral grade has no valid check list.")
    outcomes = {}
    for check in checks:
        if not isinstance(check, dict):
            raise AdjudicationError("Behavioral check is malformed.")
        name = check.get("name")
        if not isinstance(name, str) or not name.strip() or name in outcomes:
            raise AdjudicationError("Behavioral check identities are missing or ambiguous.")
        status = check.get("status")
        passed = check.get("passed")
        if "status" in check and status not in ("pass", "fail"):
            raise AdjudicationError("Behavioral check outcome is unknown.")
        if "passed" in check and not isinstance(passed, bool):
            raise AdjudicationError("Behavioral check outcome is unknown.")
        if status is None and not isinstance(passed, bool):
            raise AdjudicationError("Behavioral check outcome is missing.")
        if status is not None and isinstance(passed, bool) and passed != (status == "pass"):
            raise AdjudicationError("Behavioral check outcomes contradict each other.")
        outcomes[name] = passed if isinstance(passed, bool) else status == "pass"
    total, passed_count = len(outcomes), sum(outcomes.values())
    for field, expected in (("total_checks", total), ("passed_checks", passed_count)):
        if field in grade and (type(grade[field]) is not int or grade[field] != expected):
            raise AdjudicationError("Behavioral check counts contradict their outcomes.")
    if grade["status"] == "pass" and passed_count != total:
        raise AdjudicationError("Behavioral grade contradicts its failed checks.")
    return grade["status"], outcomes


def apply_adjudication(record: dict, data_root: str | Path, adjudications: dict[str, dict]) -> dict:
    """Return an evidence-verified exclusion, or an unchanged deep copy.

    Existing behavioral_grade is authoritative for already-derived records.
    Only the reviewed sole failure can be excluded; other failures, unsafe IDs,
    stale hashes, and disagreement with the saved grade are hard errors.
    """
    if not isinstance(record, dict):
        raise AdjudicationError("Trial record must be an object.")
    _validate_entries(adjudications)
    identity = record.get("id")
    if "id" in record and not _safe_id(identity):
        raise AdjudicationError("Trial record identity is unsafe.")
    result = deepcopy(record)
    if identity is None or identity not in adjudications:
        return result
    task, repetition, arm = record.get("task_id"), record.get("repetition"), record.get("arm")
    if (not _safe_id(task) or type(repetition) is not int or repetition < 1
            or arm not in ("off", "brain") or identity != f"{task}-r{repetition}-{arm}"):
        raise AdjudicationError("Trial identity disagrees with its task, repetition, or arm.")
    issues = record.get("validity_issues", [])
    if not isinstance(issues, list) or any(not isinstance(issue, str) for issue in issues):
        raise AdjudicationError("Trial validity issues are malformed.")
    current_grade = record.get("grade")
    if not isinstance(current_grade, dict) or current_grade.get("status") not in ("fail", "infra", "infra_error", "infrastructure_error"):
        raise AdjudicationError("Only a failed or already-unscorable trial can be adjudicated.")
    entry = adjudications[identity]
    try:
        path = Path(data_root).resolve() / "trials" / identity / "grade.json"
        if path.resolve(strict=True) != path:
            raise AdjudicationError("Saved grade must be the trial's own regular file.")
        contents = path.read_bytes()
    except OSError as error:
        raise AdjudicationError("Could not read the saved trial grade.") from error
    if hashlib.sha256(contents).hexdigest() != entry["grade_sha256"]:
        raise AdjudicationError("Saved grade digest does not match the reviewed evidence.")
    saved = _semantic_grade(_decode(contents))
    original = record.get("behavioral_grade", record.get("grade"))
    behavioral = _semantic_grade(original)
    if saved != behavioral:
        raise AdjudicationError("Saved grade disagrees with the original behavioral grade.")
    failures = [name for name, passed in behavioral[1].items() if not passed]
    if behavioral[0] != "fail" or failures != [entry["failed_check"]]:
        raise AdjudicationError("The reviewed check must be the sole behavioral failure.")
    result["behavioral_grade"] = deepcopy(original)
    result["validity_issues"] = list(issues)
    reason = entry["reason"]
    if reason not in result["validity_issues"]:
        result["validity_issues"].append(reason)
    result["grade"] = {"status": "infrastructure_error", "issues": list(result["validity_issues"])}
    result["adjudication"] = {
        "schema_version": SCHEMA_VERSION,
        "trial_id": identity,
        "reason": reason,
        "grade_sha256": entry["grade_sha256"],
        "failed_check_sha256": hashlib.sha256(entry["failed_check"].encode()).hexdigest(),
    }
    return result
