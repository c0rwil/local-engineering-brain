"""Semantic retrieval diagnostics must rank honestly and refuse contaminated arms."""

import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import tomllib
import unittest
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
sys.dont_write_bytecode = True
import eval_semantic_retrieval as evaluation  # noqa: E402

BASE = "active validated memory; current symbol digests and commit bounds match"


def item(identity, score, note, semantic=0.0):
    return {"source_type": "memory", "source_id": identity, "score": score, "tokens": 5,
            "why": {"note": note, "semantic": semantic, "lexical": 0.1}}


class SemanticEvalTests(unittest.TestCase):
    def run_trial(self, items, relevant, negative=False):
        response = {"items": items + [{"source_type": "code", "source_id": "node", "score": 9.0, "tokens": 5,
                                       "why": {"note": "source", "semantic": 0.0, "lexical": 1.0}}],
                    "stable_prefix": "", "dynamic_context": "text",
                    "metrics": {"selected_tokens": 40, "compile_ms": 2}}
        case = {"id": "case", "project_id": "prj", "query": "q", "negative": negative}
        with patch.object(evaluation, "call", return_value=response):
            return evaluation.trial(Path("unused.sock"), case, relevant, 100, 1, "local")

    def test_provenance_distinguishes_served_fallback_partial_and_skipped(self):
        self.assertEqual(evaluation.provenance(BASE), "none")
        self.assertEqual(evaluation.provenance(f"{BASE}; semantic: local:nomic-embed-text"),
                         "served:local:nomic-embed-text")
        self.assertEqual(
            evaluation.provenance(f"{BASE}; semantic: engram-hashed-ngram-v1-d256 fallback (local:m unavailable: x)"),
            "fallback:engram-hashed-ngram-v1-d256")
        self.assertEqual(
            evaluation.provenance(f"{BASE}; semantic: local:m (3 of 4 memories have current vectors; rebuild requested)"),
            "partial:local:m")
        self.assertEqual(evaluation.provenance(f"{BASE}; semantic skipped: no current local:m vectors for 4 memories"),
                         "skipped")

    def test_ranks_order_memory_items_by_score_and_ignore_sources(self):
        note = f"{BASE}; semantic: local:m"
        result = self.run_trial([item("low", 0.2, note), item("high", 0.9, note), item("target", 0.5, note, 0.7)],
                                ["target"])
        self.assertEqual(result["first_relevant_rank"], 2)
        self.assertEqual((result["injected_memories"], result["irrelevant_injected"]), (3, 2))
        self.assertEqual(result["provenance"], ["served:local:m"])
        self.assertEqual([entry["memory_id"] for entry in result["memory_items"]], ["high", "target", "low"])
        self.assertNotIn("text", json.dumps(result["memory_items"]))

    def test_any_fallback_contaminates_the_local_arm(self):
        clean = self.run_trial([item("target", 0.5, f"{BASE}; semantic: local:m")], ["target"])
        fallback = self.run_trial([item("target", 0.5, f"{BASE}; semantic: engram-hashed-ngram-v1-d256 fallback (x)")],
                                  ["target"])
        summary = evaluation.summarize([clean, fallback], "served:local:m")
        self.assertEqual(summary["contaminated_trials"], 1)
        self.assertEqual(summary["trials_served_by_expected_source"], 1)
        self.assertEqual((summary["hits_at_1"], summary["mrr"]), (2, 1.0))

    def test_misses_count_against_mrr_and_negative_injections_are_reported(self):
        miss = self.run_trial([item("other", 0.5, BASE)], ["target"])
        negative = self.run_trial([item("other", 0.5, BASE)], [], negative=True)
        summary = evaluation.summarize([miss, negative], None)
        self.assertEqual((summary["positive_trials"], summary["mrr"], summary["hits_at_5"]), (1, 0.0, 0))
        self.assertEqual(summary["negative_injected_memories"], 1)

    def test_compare_counts_rank_changes_per_case(self):
        def record(case, rank):
            return {"case": case, "repeat": 1, "ok": True, "negative": False, "first_relevant_rank": rank}
        baseline = [record("a", 1), record("b", None), record("c", 2)]
        candidate = [record("a", 2), record("b", 3), record("c", 2)]
        self.assertEqual(evaluation.compare(baseline, candidate),
                         {"cases": 3, "rank_improved": 1, "rank_worsened": 1, "unchanged": 1})

    def test_arm_configs_are_valid_toml_with_machine_level_provider_settings(self):
        args = SimpleNamespace(endpoint="http://127.0.0.1:11434/v1/embeddings", model='odd "name"', dimension=768,
                               batch_size=16, timeout_ms=30000, query_timeout_ms=5000, min_similarity=0.5,
                               query_prefix="search_query: ", document_prefix="search_document: ")
        self.assertFalse(tomllib.loads(evaluation.arm_config("off", args))["context"]["semantic_retrieval"])
        self.assertEqual(tomllib.loads(evaluation.arm_config("hashed", args))["semantic"]["provider"], "hashed")
        local = tomllib.loads(evaluation.arm_config("local", args))["semantic"]
        self.assertEqual((local["provider"], local["model"], local["min_similarity"]), ("local", 'odd "name"', 0.5))
        self.assertTrue(evaluation.loopback(args.endpoint))
        self.assertFalse(evaluation.loopback("http://10.0.0.1:11434/v1/embeddings"))

    def test_cases_must_match_a_validated_memory_unless_negative(self):
        database = sqlite3.connect(":memory:")
        self.addCleanup(database.close)
        database.executescript(
            "CREATE TABLE projects(id TEXT, name TEXT, tombstoned_at_ms INTEGER);"
            "CREATE TABLE memories(id TEXT, project_id TEXT, claim TEXT, status TEXT, epistemic_state TEXT);"
            "INSERT INTO projects VALUES ('prj', 'repo', NULL);"
            "INSERT INTO memories VALUES ('m1', 'prj', 'Use attempts-1 for retry budgets', 'active', 'validated'),"
            "('m2', 'prj', 'Use attempts-1 when unvalidated', 'active', 'candidate');")
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "cases.json"
            path.write_text(json.dumps([
                {"id": "hit", "project": "alias", "query": "retry", "expected_patterns": ["attempts-1"]},
                {"id": "none", "project": "alias", "query": "printer", "expected_patterns": [], "negative": True},
            ]))
            cases, budget = evaluation.load_cases(path, None)
            self.assertIsNone(budget)
            _, relevant = evaluation.resolve_fixtures(database, cases, {"alias": "repo"})
            self.assertEqual(relevant, {"hit": ["m1"], "none": []})
            path.write_text(json.dumps([{"id": "x", "project": "repo", "query": "q", "expected_patterns": []}]))
            with self.assertRaises(ValueError):
                evaluation.load_cases(path, None)
            path.write_text(json.dumps([{"id": "x", "project": "repo", "query": "q", "expected_patterns": ["absent"]}]))
            cases, _ = evaluation.load_cases(path, None)
            with self.assertRaises(ValueError):
                evaluation.resolve_fixtures(database, cases, {})


if __name__ == "__main__":
    unittest.main()
