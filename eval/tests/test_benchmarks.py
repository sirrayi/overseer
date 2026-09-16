"""Adapter normalization — parse real-schema fixtures offline (zero spend)."""

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rig.benchmarks import harbor, lcb, swebench, tau2

TAU2_RESULTS = {
    "timestamp": "2026-01-01T00:00:00",
    "info": {"domain": "airline"},
    "tasks": [{"id": "task-1"}],
    "simulations": [
        {
            "id": "s1",
            "task_id": "task-1",
            "trial": 0,
            "seed": 0,
            "start_time": "",
            "end_time": "",
            "duration": 12.5,
            "termination_reason": "agent_stop",
            "agent_cost": 0.043,
            "user_cost": 0.01,
            "agent_usage": {"prompt_tokens": 1000, "completion_tokens": 200},
            "reward_info": {"reward": 1.0, "reward_basis": ["DB", "ACTION"]},
            "messages": [
                {"role": "user"},
                {"role": "assistant"},
                {"role": "assistant"},
            ],
        },
        {
            "id": "s2",
            "task_id": "task-1",
            "trial": 1,
            "seed": 0,
            "start_time": "",
            "end_time": "",
            "duration": 9.0,
            "termination_reason": "infrastructure_error",
            "agent_cost": 0.02,
            "user_cost": 0.005,
            "reward_info": {"reward": 0.0},
            "messages": [{"role": "user"}],
        },
        {
            "id": "s3",
            "task_id": "task-1",
            "trial": 2,
            "seed": 0,
            "start_time": "",
            "end_time": "",
            "duration": 40.0,
            "termination_reason": "context_window_exceeded",
            "agent_cost": 0.08,
            "user_cost": 0.02,
            "reward_info": {"reward": 0.0},
            "messages": [{"role": "assistant"}],
        },
        {
            "id": "s4",
            "task_id": "task-2",
            "trial": 0,
            "seed": 0,
            "start_time": "",
            "end_time": "",
            "duration": 30.0,
            "termination_reason": "max_steps",
            "agent_cost": 0.11,
            "user_cost": 0.03,
            "reward_info": {"reward": 0.0},
            "messages": [{"role": "assistant"}],
        },
        {
            "id": "s5",
            "task_id": "task-2",
            "trial": 1,
            "seed": 0,
            "start_time": "",
            "end_time": "",
            "duration": 5.0,
            "termination_reason": "user_error",
            "reward_info": {"reward": 0.0},
            "messages": [],
        },
    ],
}


class TestTau2:
    def test_parse(self, tmp_path):
        p = tmp_path / "results.json"
        p.write_text(json.dumps(TAU2_RESULTS))
        recs = tau2.Tau2Adapter(tmp_path).parse_results(
            p,
            domain="airline",
            agent_name="overseer",
            user_llm="openai/fixed-user",
            model_id="openai/kimi",
            harness_commit="abc",
            run_set_id="rs",
        )
        assert len(recs) == 5
        # deterministic order: (task-1,t0..2), (task-2,t0..1)
        assert [r["task_id"] for r in recs] == (
            ["tau2-airline-task-1"] * 3 + ["tau2-airline-task-2"] * 2
        )
        r0 = recs[0]
        assert r0["pass"] is True and r0["done"] is True
        assert r0["steps"] == 2  # assistant messages counted
        assert r0["cost_usd"] == 0.043
        assert r0["tau2"]["user_llm"] == "openai/fixed-user"
        # upstream non-evaluable terminations → infra (excluded from stats):
        # infrastructure_error AND context_window_exceeded
        assert recs[1]["infra_error"] is True
        assert recs[2]["infra_error"] is True  # context_window_exceeded
        # user_error IS an evaluable failure upstream — scored, not dropped
        assert recs[4]["infra_error"] is False
        assert recs[4]["pass"] is False
        # max_steps is an agent outcome
        assert recs[3]["infra_error"] is False
        assert recs[3]["done"] is False
        # missing fields stay None — never fabricated zeros
        assert recs[4]["cost_usd"] is None
        assert recs[4]["tokens_in"] is None
        # identity keys present
        for r in recs:
            assert r["env_digest"].startswith("tau2@")
            assert r["judge_version"].startswith("tau2-evaluator@")
            assert r["benchmark"] == "tau2"

    def test_missing_trial_falls_back_to_position(self, tmp_path):
        data = dict(TAU2_RESULTS)
        data["simulations"] = [
            dict(s, trial=None)
            for s in TAU2_RESULTS["simulations"][:3]
            if s["task_id"] == "task-1"
        ]
        p = tmp_path / "results.json"
        p.write_text(json.dumps(data))
        recs = tau2.Tau2Adapter(tmp_path).parse_results(
            p,
            domain="airline",
            agent_name="a",
            user_llm="u",
            model_id="m",
            harness_commit="c",
            run_set_id="rs",
        )
        assert {r["seed"] for r in recs} == {0, 1, 2}  # not all 0

    def test_domain_guard(self, tmp_path):
        a = tau2.Tau2Adapter(tmp_path)
        try:
            a.run("telecom", "m", "u", 1, None, "x")
            assert False, "telecom must be rejected"
        except Exception as e:
            assert "telecom" in str(e)

    def test_unavailable_dir(self, tmp_path):
        a = tau2.Tau2Adapter(tmp_path / "missing")
        ok, why = a.available()
        assert not ok and "OVERSEER_TAU2_DIR" in why


LCB_EVAL_ALL = [
    {
        "question_id": "abc1",
        "platform": "leetcode",
        "difficulty": "easy",
        "contest_date": "2025-08-01",
        "output_list": ["code0", "code1"],
        "graded_list": [True, False],
        "metadata": {"input_tokens": 500, "output_tokens": 300, "cost_usd": 0.002},
    },
    {
        "question_id": "xyz9",
        "platform": "atcoder",
        "difficulty": "hard",
        "contest_date": "2025-09-01",
        "output_list": ["code0"],
        "graded_list": [True],
        "metadata": {},
    },
]


class TestLcb:
    def test_parse(self, tmp_path):
        p = tmp_path / "codegeneration_4_0.0_eval_all.json"
        p.write_text(json.dumps(LCB_EVAL_ALL))
        recs = lcb.LcbAdapter(tmp_path).parse_results(
            p,
            agent_name="overseer",
            model_id="kimi",
            harness_commit="abc",
            run_set_id="rs",
            release_version="v6",
        )
        assert len(recs) == 3  # 2 samples + 1 sample
        assert [r["task_id"] for r in recs] == ["lcb-abc1", "lcb-abc1", "lcb-xyz9"]
        assert recs[0]["seed"] == 0 and recs[0]["pass"] is True
        assert recs[1]["seed"] == 1 and recs[1]["pass"] is False
        assert recs[0]["difficulty"] == "easy"
        assert recs[0]["lcb"]["release_version"] == "v6"
        assert recs[0]["tokens_in"] == 500
        # absent metadata stays None
        assert recs[2]["tokens_in"] is None
        assert recs[2]["cost_usd"] is None
        for r in recs:
            assert r["env_digest"].startswith("lcb@")
            assert r["judge_version"].startswith("lcb@")

    def test_unavailable_dir(self, tmp_path):
        a = lcb.LcbAdapter(tmp_path / "missing")
        ok, why = a.available()
        assert not ok and "OVERSEER_LCB_DIR" in why


# Real schema observed from a harbor oracle run
# (jobs/<ts>/<task>__<hash>/result.json):
HARBOR_TRIAL = {
    "id": "t1",
    "task_name": "gpt2-codegolf",
    "trial_name": "gpt2-codegolf__F5Giz78",
    "task_id": {
        "git_url": "https://github.com/laude-institute/terminal-bench-2.git",
        "git_commit_id": "69671fbaac6d67a7ef0dfec016cc38a64ef7a77c",
        "path": "gpt2-codegolf",
    },
    "task_checksum": "c3dfea37" + "0" * 56,
    "agent_info": {"name": "oracle", "version": "1.0.0", "model_info": None},
    "agent_result": {"n_input_tokens": None, "n_output_tokens": None, "cost_usd": None},
    "verifier_result": {"rewards": {"reward": 1.0}},
    "exception_info": None,
    "started_at": "2026-09-16T22:07:14.633575Z",
    "finished_at": "2026-09-16T22:08:20.244405Z",
    "agent_execution": {"started_at": "x", "finished_at": "y"},
    "step_results": None,
}


class TestHarbor:
    def _job(self, tmp_path, trials):
        job = tmp_path / "job1"
        for i, t in enumerate(trials):
            d = job / f"{t['task_name']}__h{i}"
            d.mkdir(parents=True)
            (d / "result.json").write_text(json.dumps(t))
        (job / "result.json").write_text("{}")
        return job

    def test_parse_oracle_trial(self, tmp_path):
        job = self._job(tmp_path, [HARBOR_TRIAL])
        recs = harbor.TerminalBenchAdapter(jobs_root=tmp_path).parse_results(
            job,
            agent_name="oracle",
            model_id=None,
            harness_commit="abc",
            run_set_id="rs",
        )
        assert len(recs) == 1
        r = recs[0]
        assert r["task_id"] == "tb2-gpt2-codegolf"
        assert r["pass"] is True and r["infra_error"] is False
        assert r["wall_s"] == 65.6
        assert r["seed"] == 0
        assert "harbor@" in r["judge_version"]
        assert "task@69671fbaac6d" in r["judge_version"]
        assert r["tb2"]["task_checksum"].startswith("c3dfea37")
        # absent usage stays None
        assert r["tokens_in"] is None and r["cost_usd"] is None

    def test_seed_per_attempt(self, tmp_path):
        job = self._job(tmp_path, [HARBOR_TRIAL, dict(HARBOR_TRIAL)])
        recs = harbor.TerminalBenchAdapter(jobs_root=tmp_path).parse_results(
            job,
            agent_name="a",
            model_id="m",
            harness_commit="c",
            run_set_id="rs",
        )
        assert [r["seed"] for r in recs] == [0, 1]

    def test_env_exception_is_infra(self, tmp_path):
        t = dict(HARBOR_TRIAL)
        t["exception_info"] = {"type": "EnvironmentError"}
        t["agent_execution"] = {"started_at": "x", "finished_at": None}
        t["verifier_result"] = {}
        job = self._job(tmp_path, [t])
        recs = harbor.TerminalBenchAdapter(jobs_root=tmp_path).parse_results(
            job,
            agent_name="a",
            model_id="m",
            harness_commit="c",
            run_set_id="rs",
        )
        assert recs[0]["infra_error"] is True and recs[0]["pass"] is False


# Real schema observed from swebench 5.0.2:
# logs/run_evaluation/<run_id>/<model>/<iid>/report.json
SWEBENCH_REPORT = {
    "sympy__sympy-22914": {
        "resolved": True,
        "patch_exists": True,
        "patch_successfully_applied": True,
        "tests_status": {
            "FAIL_TO_PASS": {"success": ["t1"], "failure": []},
            "PASS_TO_PASS": {"success": ["t2"], "failure": []},
        },
    }
}


class TestSweBench:
    def _report_tree(
        self,
        tmp_path,
        report=SWEBENCH_REPORT,
        model="kimi",
        iid="sympy__sympy-22914",
        run_id="r1",
    ):
        d = tmp_path / "logs" / "run_evaluation" / run_id / model / iid
        d.mkdir(parents=True)
        (d / "report.json").write_text(json.dumps(report))
        return tmp_path / "logs" / "run_evaluation" / run_id

    def test_parse_resolved(self, tmp_path):
        log_dir = self._report_tree(tmp_path)
        recs = swebench.SweBenchAdapter(logs_root=tmp_path).parse_results(
            log_dir,
            agent_name="overseer",
            model_id="kimi",
            seed=0,
            harness_commit="abc",
            run_set_id="rs",
        )
        assert len(recs) == 1
        r = recs[0]
        assert r["task_id"] == "swe_bench-sympy__sympy-22914"
        assert r["pass"] is True and r["infra_error"] is False
        assert r["model"] == "kimi"  # dir name when no rollout sidecar
        assert r["judge_version"].startswith("swebench@")
        assert r["swe_bench"]["tests_status"]["FAIL_TO_PASS"]["success"] == ["t1"]

    def test_unresolved(self, tmp_path):
        rep = {
            "i1": {
                "resolved": False,
                "patch_exists": True,
                "patch_successfully_applied": True,
            }
        }
        log_dir = self._report_tree(tmp_path, report=rep, iid="i1")
        recs = swebench.SweBenchAdapter(logs_root=tmp_path).parse_results(
            log_dir,
            agent_name="a",
            model_id="m",
            seed=0,
            harness_commit="c",
            run_set_id="rs",
        )
        assert recs[0]["pass"] is False and recs[0]["infra_error"] is False

    def test_unapplied_patch_is_infra(self, tmp_path):
        rep = {
            "i1": {
                "resolved": False,
                "patch_exists": False,
                "patch_successfully_applied": False,
            }
        }
        log_dir = self._report_tree(tmp_path, report=rep, iid="i1")
        recs = swebench.SweBenchAdapter(logs_root=tmp_path).parse_results(
            log_dir,
            agent_name="a",
            model_id="m",
            seed=0,
            harness_commit="c",
            run_set_id="rs",
        )
        assert recs[0]["infra_error"] is True

    def test_rollout_sidecar_merges_metrics(self, tmp_path):
        log_dir = self._report_tree(tmp_path)
        sidecar = tmp_path / "rollouts-s0.jsonl"
        sidecar.write_text(
            json.dumps(
                {
                    "instance_id": "sympy__sympy-22914",
                    "run_id": "rid1",
                    "session_dir": "/sessions/rid1",
                    "outcome": {
                        "done": True,
                        "tokens_in": 1234,
                        "cost_usd": 0.01,
                        "model": "fleet-k3",
                    },
                }
            )
        )
        recs = swebench.SweBenchAdapter(logs_root=tmp_path).parse_results(
            log_dir,
            agent_name="overseer",
            model_id="kimi",
            seed=0,
            harness_commit="abc",
            run_set_id="rs",
        )
        r = recs[0]
        assert r["run_id"] == "rid1"
        assert r["tokens_in"] == 1234
        assert r["cost_usd"] == 0.01
        assert r["session_dir"] == "/sessions/rid1"
        assert r["model"] == "fleet-k3"  # rollout model wins over dir name

    def test_image_arch_rewrite(self):
        import platform

        arch = swebench.SweBenchAdapter._image_arch()
        expected = "arm64" if platform.machine() in ("arm64", "aarch64") else "x86_64"
        assert arch == expected
