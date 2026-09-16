"""Capture report denominators and delivered-text scoring are explicit."""
import importlib.util
from pathlib import Path
import sqlite3
import sys
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("capture_eval", Path(__file__).parents[1] / "scripts/capture_eval.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class CaptureMetricsTest(unittest.TestCase):
    def test_session_grouping_and_unknown_acceptance(self):
        db = sqlite3.connect(":memory:")
        db.executescript("""
        CREATE TABLE tasks(id,project_id,session_id,status,accepted_by_user,started_at_ms);
        CREATE TABLE memories(id,epistemic_state,status,known_from_ms);
        CREATE TABLE memory_sources(memory_id,task_id);
        CREATE TABLE events(payload_json,created_at_ms,event_type);
        INSERT INTO tasks VALUES('tsk_1','p','session','success',NULL,1000),
          ('tsk_2','p','session','abandoned',NULL,2000),('tsk_3','p',NULL,'success',1,3000);
        INSERT INTO memories VALUES('m1','validated','active',2100),('m2','candidate','active',3500);
        INSERT INTO memory_sources VALUES('m1','tsk_1'),('m1','tsk_2'),('m2','tsk_3');
        INSERT INTO events VALUES('{"data":{"memory_id":"m1","epistemic_state":"validated"}}',5000,'MEMORY_STATE_CHANGED');
        INSERT INTO events VALUES('{"data":{"memory_id":"m1","source_task_ids":["tsk_1","tsk_2"],"reused_existing":false}}',2100,'LEARNING_DISTILLED');
        INSERT INTO events VALUES('{"data":{"memory_id":"m2","source_task_ids":["tsk_3"],"reused_existing":false}}',3500,'LEARNING_DISTILLED');
        """)
        result = module.capture_metrics(db)
        self.assertEqual(result['aggregate']['validated_memories'], 1)
        self.assertEqual(result['aggregate']['candidate_to_validated_conversion'], .5)
        self.assertEqual(result['aggregate']['successes_without_acceptance'], 1)
        self.assertAlmostEqual(result['aggregate']['acceptance_recorded_share_of_closed'], 1/3)
        session = next(s for s in result['sessions'] if s['grouping'] == 'session')
        self.assertEqual(session['memories_created'], 1)  # shared sources must not double count
        self.assertEqual(session['time_to_first_validated_memory_ms'], 4000)
        self.assertEqual(session['abandoned_task_rate'], .5)
        self.assertIsNone(module.ratio(0, 0))
        db.execute("INSERT INTO memory_sources VALUES('m1','tsk_3')")
        reused = module.capture_metrics(db)
        later = next(s for s in reused['sessions'] if s['grouping'] == 'episode')
        self.assertEqual(later['sourced_memories'], 2)
        self.assertEqual(later['memories_created'], 1)
        self.assertEqual(later['validated_memories'], 0)
        self.assertIsNone(later['time_to_first_validated_memory_ms'])
        # An older, still-active session reuses a memory created by a newer one.
        db.execute("INSERT INTO memory_sources VALUES('m2','tsk_1')")
        db.execute("INSERT INTO events VALUES(?,6000,'LEARNING_DISTILLED')", ('{"data":{"memory_id":"m2","source_task_ids":["tsk_1"],"reused_existing":true}}',))
        overlapping = next(s for s in module.capture_metrics(db)['sessions'] if s['grouping'] == 'session')
        self.assertEqual(overlapping['sourced_memories'], 2)
        self.assertEqual(overlapping['memories_created'], 1)
        db.execute("INSERT INTO memories VALUES('m3','candidate','active',4000)")
        db.execute("INSERT INTO memory_sources VALUES('m3','tsk_1')")
        unknown = next(s for s in module.capture_metrics(db)['sessions'] if s['grouping'] == 'session')
        self.assertEqual(unknown['sourced_memories_without_creation_provenance'], 1)
        self.assertEqual(unknown['memories_created'], 1)


if __name__ == '__main__':
    unittest.main()
