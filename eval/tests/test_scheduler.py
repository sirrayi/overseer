"""scheduler — the full offline matrix: oracle arm must pass every task,
fail arm must record honest failures, infra errors must be excluded."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rig import graders, manifest, scheduler, stats, store, taskspec

TASKS_DIR = Path(__file__).resolve().parents[1] / "tasks"


def _task():
    return taskspec.parse(
        {
            "id": "mini",
            "version": 1,
            "instruction": "x",
            "setup": "echo hi > f.txt",
            "grader": {"type": "bash_assert", "script": "grep -q done done.txt"},
            "oracle": {"script": "echo done > done.txt"},
            "limits": {"max_steps": 5},
        }
    )


def _run(tasks, agents, k, tmp_path):
    st = store.Store(tmp_path / "store.jsonl")
    recs = scheduler.run_matrix(
        tasks,
        agents,
        k,
        results_root=tmp_path / "res",
        ws_root=tmp_path / "ws",
        store=st,
        harness_commit="test",
    )
    return st, recs


class TestOracle:
    def test_all_real_tasks_pass(self, tmp_path):
        """The shipped corpus is solvable — oracle must pass all 5."""
        tasks = taskspec.load_dir(TASKS_DIR)
        for t in tasks:
            ok, why = graders.oracle_check(t, tmp_path / "oc" / t.id)
            assert ok, f"{t.id}: {why}"

    def test_oracle_matrix(self, tmp_path):
        st, recs = _run([_task()], ["oracle"], 2, tmp_path)
        assert len(recs) == 2
        assert all(r["pass"] for r in recs)
        assert all(not r["infra_error"] for r in recs)
        assert all(r["done"] for r in recs)
        assert {r["seed"] for r in recs} == {0, 1}
        # header + 2 run rows on disk
        assert len(st.load()) == 3

    def test_fail_arm(self, tmp_path):
        st, recs = _run([_task()], ["fail"], 2, tmp_path)
        assert len(recs) == 2
        assert all(r["pass"] is False for r in recs)
        assert all(not r["infra_error"] for r in recs)
        s = stats.summarize(st.runs(harness="fail"), k=1)
        assert s["pass_at_1"]["mean"] == 0.0

    def test_infra_excluded_and_retried(self, tmp_path):
        """A solver raising FileNotFoundError → infra_error row, retried once."""
        from rig import agents

        calls = {"n": 0}

        def bad_solve(*a, **kw):
            calls["n"] += 1
            raise FileNotFoundError("binary gone")

        agents.REGISTRY["bad"] = type("M", (), {"solve": staticmethod(bad_solve)})
        try:
            st, recs = _run([_task()], ["bad"], 1, tmp_path)
            assert len(recs) == 1
            assert recs[0]["infra_error"] is True
            assert calls["n"] == 2  # retried exactly once
            s = stats.summarize(st.runs(harness="bad"), k=1)
            assert s["trials"] == 0  # excluded from pass stats
        finally:
            del agents.REGISTRY["bad"]

    def test_infra_retry_success(self, tmp_path):
        """First attempt infra, retry succeeds → recorded as the retry's run."""
        from rig import agents

        calls = {"n": 0}

        def flaky(instruction, workdir, session_dir, *, limits=None, seed=0, task=None):
            calls["n"] += 1
            if calls["n"] == 1:
                raise FileNotFoundError("transient")
            graders.run_script("echo done > done.txt", workdir)
            return {"done": True, "model": "flaky", "ts": 0}

        agents.REGISTRY["flaky"] = type("M", (), {"solve": staticmethod(flaky)})
        try:
            _, recs = _run([_task()], ["flaky"], 1, tmp_path)
            assert len(recs) == 1
            assert recs[0]["pass"] is True
            assert recs[0]["retried"] is True
            assert calls["n"] == 2
            # session_dir points at the retry's run dir, not the first's
            assert recs[0]["session_dir"].endswith("r")
        finally:
            del agents.REGISTRY["flaky"]

    def test_done_gate(self, tmp_path):
        """Solver that does the work but never says done → fail."""
        from rig import agents

        def sneaky(
            instruction, workdir, session_dir, *, limits=None, seed=0, task=None
        ):
            graders.run_script("echo done > done.txt", workdir)
            return {"done": False, "model": "sneaky", "ts": 0}

        agents.REGISTRY["sneaky"] = type("M", (), {"solve": staticmethod(sneaky)})
        try:
            _, recs = _run([_task()], ["sneaky"], 1, tmp_path)
            assert recs[0]["pass"] is False  # grader passed but done=False
        finally:
            del agents.REGISTRY["sneaky"]


class TestRecordShape:
    def test_identity_keys(self, tmp_path):
        _, recs = _run([_task()], ["oracle"], 1, tmp_path)
        r = recs[0]
        for key in (
            "task_id",
            "task_version",
            "harness",
            "harness_commit",
            "model",
            "seed",
            "env_digest",
            "judge_version",
        ):
            assert key in r, f"missing identity key {key}"
        assert r["harness_commit"] == "test"
        assert r["benchmark"] == "local"
        assert r["schema"] == "overseer.run-record/1"


class TestManifestMerge:
    def test_no_manifest(self, tmp_path):
        rec = manifest.build_record(
            run_id="r",
            task=_task(),
            agent="x",
            seed=0,
            outcome={"pass": True, "done": True},
            manifest=None,
            benchmark="local",
            run_set_id="s",
            session_dir="d",
            judge_version=None,
            harness_commit="c",
        )
        assert rec["provenance"] is None

    def test_with_manifest(self, tmp_path):
        sess = tmp_path / "sess"
        sess.mkdir()
        (sess / "manifest.json").write_text(
            '{"schema":"m","harness":{"commit":"abc"},'
            '"model":{"name":"kimi"},"system_prompt":{"sha256":"x"},'
            '"tools":{"count":3}}'
        )
        m = manifest.load_manifest(sess)
        assert m["harness"]["commit"] == "abc"
        rec = manifest.build_record(
            run_id="r",
            task=_task(),
            agent="x",
            seed=0,
            outcome={"pass": True, "done": True},
            manifest=m,
            benchmark="local",
            run_set_id="s",
            session_dir=str(sess),
            judge_version=None,
            harness_commit="c",
        )
        assert rec["provenance"]["tools"]["count"] == 3
        assert rec["model"] == "kimi"  # manifest model wins
