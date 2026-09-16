"""Canary audit — contamination/leak detection over stored runs."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rig import audit, taskspec


def _spec(tid, canary):
    return taskspec.parse(
        {
            "id": tid,
            "version": 1,
            "instruction": "x",
            "grader": {"type": "bash_assert", "script": "true"},
            "oracle": {"script": "true"},
            "canary": canary,
        }
    )


class TestCanaryAudit:
    def test_echo_not_a_finding(self, tmp_path):
        t = _spec("held-x", "OVR-CANARY-aaaaaaaaaaaaaaaa")
        traj = tmp_path / "traj.jsonl"
        traj.write_text('{"msg": "saw OVR-CANARY-aaaaaaaaaaaaaaaa"}\n')
        recs = [
            {
                "kind": "run",
                "task_id": "held-x",
                "run_id": "r1",
                "trajectory": str(traj),
            }
        ]
        rep = audit.audit_canaries([t], recs)
        assert rep["findings"] == []
        assert rep["per_task"]["held-x"]["echo"] == 1

    def test_foreign_canary_flagged(self, tmp_path):
        a = _spec("held-a", "OVR-CANARY-aaaaaaaaaaaaaaaa")
        b = _spec("held-b", "OVR-CANARY-bbbbbbbbbbbbbbbb")
        traj = tmp_path / "traj.jsonl"
        # task-b's run contains task-a's canary → contamination
        traj.write_text('{"msg": "OVR-CANARY-aaaaaaaaaaaaaaaa"}\n')
        recs = [
            {
                "kind": "run",
                "task_id": "held-b",
                "run_id": "rX",
                "trajectory": str(traj),
            }
        ]
        rep = audit.audit_canaries([a, b], recs)
        types = [f["type"] for f in rep["findings"]]
        assert "foreign_canary" in types
        f = next(f for f in rep["findings"] if f["type"] == "foreign_canary")
        assert f["owner_task"] == "held-a" and f["found_in_task"] == "held-b"

    def test_leak_to_public_task_file(self, tmp_path):
        t = _spec("held-x", "OVR-CANARY-cccccccccccccccc")
        pub = tmp_path / "pub"
        pub.mkdir()
        (pub / "leaky.json").write_text(
            '{"id": "x", "note": "OVR-CANARY-cccccccccccccccc"}'
        )
        rep = audit.audit_canaries([t], [], public_dirs=[pub])
        assert any(f["type"] == "leak_to_public" for f in rep["findings"])

    def test_missing_and_malformed_canary(self):
        good = _spec("held-ok", "OVR-CANARY-dddddddddddddddd")
        missing = _spec("held-none", None)
        bad = _spec("held-bad", "not-a-canary")
        rep = audit.audit_canaries([good, missing, bad], [])
        types = {f["type"] for f in rep["findings"]}
        assert "missing_canary" in types and "malformed_canary" in types

    def test_canary_in_public_run(self, tmp_path):
        t = _spec("held-x", "OVR-CANARY-eeeeeeeeeeeeeeee")
        traj = tmp_path / "t.jsonl"
        traj.write_text("OVR-CANARY-eeeeeeeeeeeeeeee\n")
        recs = [
            {
                "kind": "run",
                "task_id": "public-task",
                "run_id": "r9",
                "trajectory": str(traj),
            }
        ]
        rep = audit.audit_canaries([t], recs)
        types = [f["type"] for f in rep["findings"]]
        assert "canary_in_public_run" in types

    def test_scanned_counts_runs_actually_read(self, tmp_path):
        # a clean run with readable artifacts still counts as scanned —
        # scanned_runs must reflect coverage, not sightings
        t = _spec("held-x", "OVR-CANARY-aaaaaaaaaaaaaaaa")
        traj = tmp_path / "traj.jsonl"
        traj.write_text('{"msg": "nothing suspicious"}\n')
        recs = [
            {
                "kind": "run",
                "task_id": "held-x",
                "run_id": "r1",
                "trajectory": str(traj),
            },
            {
                "kind": "run",
                "task_id": "held-x",
                "run_id": "r2",
                "trajectory": str(tmp_path / "missing.jsonl"),
            },
        ]
        rep = audit.audit_canaries([t], recs)
        assert rep["scanned_runs"] == 1
        assert rep["findings"] == []

    def test_heldout_tasks_all_have_valid_canaries(self):
        heldout = taskspec.load_dir(
            Path(__file__).resolve().parents[1] / "heldout" / "tasks"
        )
        assert len(heldout) >= 5
        rep = audit.audit_canaries(heldout, [])
        assert rep["findings"] == []
        assert rep["n_canaries"] == len(heldout)
