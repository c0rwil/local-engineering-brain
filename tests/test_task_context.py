"""Coverage accounting must distinguish definitions, files, and Off controls."""

import json
from pathlib import Path
import sqlite3
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import eval_task_context as evaluation  # noqa: E402


class TaskContextTests(unittest.TestCase):
    def setUp(self):
        self.database = sqlite3.connect(":memory:")
        self.addCleanup(self.database.close)
        self.database.execute("CREATE TABLE nodes(id TEXT,project_id TEXT,file_path TEXT,kind TEXT,label TEXT,tombstoned_at_ms INTEGER)")
        self.database.executemany("INSERT INTO nodes VALUES(?,?,?,?,?,NULL)", [
            ("target", "local", "src/target.rs", "Function", "Owner::run"),
            ("samefile", "local", "src/target.rs", "Function", "unrelated"),
            ("wrongproject", "other", "src/target.rs", "Function", "Owner::run"),
            *[(f"filler{i}", "local", f"src/filler{i}.rs", "Function", "filler") for i in range(5)],
        ])
        self.case = {"id": "case", "query": "Owner::run", "components": [
            {"path": "src/target.rs", "labels": ["run", "Owner::run"]},
        ]}

    def run_trial(self, identifiers, case=None, mode="brain", tokens=20):
        response = {"items": [{"source_id": identity, "tokens": 2} for identity in identifiers],
                    "stable_prefix": "" if mode == "off" else "reference", "dynamic_context": "",
                    "metrics": {"selected_tokens": tokens, "compile_ms": 1}}
        with patch.object(evaluation, "api", return_value=response):
            return evaluation.trial(self.database, Path("unused.sock"), "local", case or self.case, 100, 1, mode)

    def test_file_hits_do_not_award_definition_or_other_project_hits(self):
        wrong = self.run_trial(["wrongproject"])
        self.assertEqual(wrong["files_in_pack"], [])
        self.assertIsNone(wrong["components"][0]["rank"])
        samefile = self.run_trial(["samefile"])
        self.assertEqual(samefile["files_at5"], ["src/target.rs"])
        self.assertIsNone(samefile["components"][0]["rank"])

    def test_pack_coverage_is_separate_from_first_five(self):
        result = self.run_trial([*[f"filler{i}" for i in range(5)], "target"])
        summary = evaluation.summarize([{"trials": [result]}])
        self.assertEqual(summary["definitions_at5"], 0)
        self.assertEqual(summary["definitions_in_pack"], 1)
        self.assertEqual(summary["files_at5"], 0)
        self.assertEqual(summary["files_in_pack"], 1)

    def test_controls_require_empty_output_and_absent_sources(self):
        off = self.run_trial([], mode="off", tokens=0)
        self.assertTrue(off["off_pass"])
        self.assertFalse(self.run_trial(["target"], mode="off", tokens=0)["off_pass"])
        negative = {"query": "missing", "components": [], "expected_no_source": True}
        self.assertTrue(self.run_trial([], case=negative)["negative_pass"])
        self.assertFalse(self.run_trial(["wrongproject"], case=negative)["negative_pass"])

    def test_file_briefs_record_where_required_definitions_land(self):
        compile_response = {"id": "ctx_1", "items": [{"source_id": "target", "tokens": 2}],
                            "stable_prefix": "reference", "dynamic_context": "",
                            "metrics": {"selected_tokens": 20, "compile_ms": 1}}
        fillers = [{"kind": "Function", "label": f"filler{i}"} for i in range(25)]
        shapes = {
            "first": [{"kind": "Function", "label": "Owner::run", "lines": {"start": 3, "end": 9}}, *fillers],
            "beyond the brief": [*fillers, {"kind": "Function", "label": "Owner::run"}],
        }
        for name, expected in (("first", (1, True)), ("beyond the brief", (None, False))):
            def api(socket, route, payload, shape=shapes[name]):
                if route == "/api/v1/world/file-shape":
                    self.assertEqual((payload["task_text"], payload["context_run_id"]), ("Owner::run", "ctx_1"))
                    self.assertEqual(payload["repository_root"], "/checkout")
                    return {"symbols": shape}
                return compile_response
            with patch.object(evaluation, "api", side_effect=api):
                result = evaluation.trial(self.database, Path("unused.sock"), "local", self.case,
                                          100, 1, "brain", "/checkout", True)
            component = result["components"][0]
            self.assertEqual((component["brief_rank"], component["brief_span"]), expected, name)
            self.assertTrue(component["named_in_task"])
            summary = evaluation.summarize([{"trials": [result]}])
            self.assertEqual(summary["brief_named_listed"], 1 if expected[0] else 0, name)
            self.assertEqual(summary["brief_named_with_span"], int(expected[1]), name)
        # Without the option there are no brief requests and no brief counts.
        plain = self.run_trial(["target"])
        self.assertNotIn("brief_rank", plain["components"][0])
        self.assertNotIn("brief_listed", evaluation.summarize([{"trials": [plain]}]))

    def test_task_names_match_whole_identifiers(self):
        self.assertTrue(evaluation.named_in("Fix Settings::from_env defaults", ["Settings::from_env"]))
        self.assertTrue(evaluation.named_in("change from_env", ["Settings::from_env"]))
        self.assertFalse(evaluation.named_in("Make step_57 return", ["step_5"]))
        self.assertFalse(evaluation.named_in("rerun the suite", ["run"]))

    def test_expected_components_must_exist_and_case_ids_are_unique(self):
        with self.assertRaises(ValueError):
            evaluation.definitions(self.database, "local", [{"path": "absent.rs", "labels": ["missing"]}])
        document = json.loads((Path(__file__).parent / "fixtures/task_context_cases.json").read_text())
        self.assertEqual(len(document["tasks"]), len({task["task_id"] for task in document["tasks"]}))
        for task in document["tasks"]:
            self.assertEqual(len(task["cases"]), len({case["id"] for case in task["cases"]}))


if __name__ == "__main__":
    unittest.main()
