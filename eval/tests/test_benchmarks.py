"""Adapter normalization — parse real-schema fixtures offline (zero spend)."""

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rig.benchmarks import lcb, tau2


class FakeArgs:
    agents = "overseer"
    model = None
    seeds = 4
    scheduler_seed = 0
    tau2_domain = "airline"
    tau2_trials = 2
    tau2_user_llm = "openai/fixed-user"
    release_version = "v6"


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
        assert len(recs) == 3
        # deterministic order: (task-1,t0), (task-1,t1), (task-2,t0)
        assert [r["task_id"] for r in recs] == [
            "tau2-airline-task-1",
            "tau2-airline-task-1",
            "tau2-airline-task-2",
        ]
        r0 = recs[0]
        assert r0["pass"] is True and r0["done"] is True
        assert r0["steps"] == 2  # assistant messages counted
        assert r0["cost_usd"] == 0.043
        assert r0["tau2"]["user_llm"] == "openai/fixed-user"
        # infra sim excluded from trials
        assert recs[1]["infra_error"] is True
        assert recs[1]["pass"] is False
        # max_steps is an agent outcome
        assert recs[2]["infra_error"] is False
        assert recs[2]["done"] is False
        assert recs[2]["pass"] is False
        # identity keys present
        for r in recs:
            assert r["env_digest"].startswith("tau2@")
            assert r["judge_version"].startswith("tau2-evaluator@")
            assert r["benchmark"] == "tau2"

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
        for r in recs:
            assert r["env_digest"].startswith("lcb@")
            assert r["judge_version"].startswith("lcb@")

    def test_unavailable_dir(self, tmp_path):
        a = lcb.LcbAdapter(tmp_path / "missing")
        ok, why = a.available()
        assert not ok and "OVERSEER_LCB_DIR" in why


class TestDockerGated:
    def test_detection_stub(self):
        from rig.benchmarks import docker_gated

        a = docker_gated.SweBenchAdapter()
        ok, why = a.available()
        # OrbStack is up on this machine — either answer is honest
        assert isinstance(ok, bool) and isinstance(why, str)
