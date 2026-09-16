"""stats.py — validated against hand-computed values (playbook Ch.8 §4)."""

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rig import stats


def _rec(task, seed, passed, **kw):
    r = {
        "kind": "run",
        "task_id": task,
        "seed": seed,
        "pass": passed,
        "infra_error": False,
    }
    r.update(kw)
    return r


class TestEstimators:
    def test_pass_at_k(self):
        # 1 - C(4,3)/C(8,3) = 1 - 4/56 = 52/56
        assert stats.pass_at_k(8, 4, 3) == pytest.approx(52 / 56)

    def test_pass_at_k_guaranteed(self):
        assert stats.pass_at_k(5, 5, 3) == 1.0  # n-c=0 < k

    def test_pass_pow_k(self):
        # C(4,3)/C(8,3) = 4/56
        assert stats.pass_pow_k(8, 4, 3) == pytest.approx(4 / 56)

    def test_pass_pow_k_insufficient(self):
        assert stats.pass_pow_k(5, 2, 3) == 0.0  # c < k

    def test_estimator_bounds(self):
        with pytest.raises(ValueError):
            stats.pass_at_k(3, 2, 5)  # k > n
        with pytest.raises(ValueError):
            stats.pass_pow_k(3, 5, 1)  # c > n


class TestPercentile:
    def test_p50(self):
        assert stats.percentile([1, 2, 3, 4], 50) == 2.5

    def test_p90(self):
        assert stats.percentile(list(range(1, 11)), 90) == pytest.approx(9.1)

    def test_singleton(self):
        assert stats.percentile([7], 90) == 7

    def test_empty(self):
        with pytest.raises(ValueError):
            stats.percentile([], 50)


class TestBootstrap:
    def test_reproducible(self):
        vals = [0.0, 0.5, 1.0, 0.25, 0.75]
        a = stats.bootstrap_ci(vals, seed=42, n_boot=500)
        b = stats.bootstrap_ci(vals, seed=42, n_boot=500)
        assert a == b  # same seed → identical CI

    def test_mean(self):
        vals = [0.0, 1.0]
        mean, lo, hi = stats.bootstrap_ci(vals, seed=0, n_boot=200)
        assert mean == 0.5
        assert 0.0 <= lo <= mean <= hi <= 1.0

    def test_constant(self):
        mean, lo, hi = stats.bootstrap_ci([0.6, 0.6, 0.6], seed=0, n_boot=50)
        assert (mean, lo, hi) == (0.6, 0.6, 0.6)


class TestMcNemar:
    def test_no_discordance(self):
        assert stats.mcnemar_exact(0, 0) == 1.0

    def test_skewed(self):
        # b01=9, b10=1: 2*(C(10,0)+C(10,1))/2^10 = 22/1024
        assert stats.mcnemar_exact(9, 1) == pytest.approx(22 / 1024)


class TestSummarize:
    def test_excludes_infra_from_trials(self):
        recs = [
            _rec("t1", 0, True),
            _rec("t1", 1, False),
            _rec("t1", 2, True, infra_error=True),
        ]
        s = stats.summarize(recs, k=1)
        assert s["trials"] == 2
        assert s["infra_failures"] == 1
        assert s["pass_at_1"]["mean"] == 0.5

    def test_pass_metrics(self):
        # t1: 3/3, t2: 1/3 → pass@1 = (1 + 1/3)/2 = 2/3
        recs = [_rec("t1", s, True) for s in range(3)] + [
            _rec("t2", s, s == 0) for s in range(3)
        ]
        s = stats.summarize(recs, k=3)
        assert s["pass_at_1"]["mean"] == pytest.approx(2 / 3)
        # pass@3: t1 → 1.0, t2 → 1 - C(2,3)/C(3,3) = 1 - 0 = 1.0 → mean 1.0
        assert s["pass_at_k"]["mean"] == 1.0
        # pass^3: t1 → 1.0, t2 → C(1,3)/C(3,3) = 0 → mean 0.5
        assert s["pass_pow_k"]["mean"] == 0.5
        assert s["pass_pow_k"]["tasks_eligible"] == 2

    def test_k_ineligible_excluded(self):
        recs = [_rec("t1", 0, True)] + [_rec("t2", s, True) for s in range(3)]
        s = stats.summarize(recs, k=3)
        assert s["pass_at_k"]["tasks_eligible"] == 1  # t1 has n=1 < k=3

    def test_cost_step_token_aggregates(self):
        recs = [
            _rec(
                "t1",
                0,
                True,
                cost_usd=0.1,
                steps=4,
                tokens_in=100,
                tokens_out=20,
                wall_s=5.0,
            ),
            _rec(
                "t1",
                1,
                True,
                cost_usd=0.3,
                steps=8,
                tokens_in=200,
                tokens_out=40,
                wall_s=9.0,
            ),
        ]
        s = stats.summarize(recs, k=1)
        assert s["cost_usd"]["median"] == pytest.approx(0.2)
        assert s["steps"]["p50"] == 6.0
        assert s["tokens"]["p50"] == 180.0  # (100+20 + 200+40)/2

    def test_empty(self):
        assert stats.summarize([], k=1)["tasks"] == 0


class TestPairedDiff:
    def _arm(self, prefix, table):
        return [_rec(t, s, p) for (t, s, p) in table]

    def test_identical_arms(self):
        a = self._arm("a", [("t1", 0, True), ("t2", 0, False)])
        b = self._arm("b", [("t1", 0, True), ("t2", 0, False)])
        out = stats.paired_diff(a, b)
        assert out["tasks_common"] == 2
        assert out["diff"]["mean"] == 0.0
        assert out["mcnemar"]["a_only"] == out["mcnemar"]["b_only"] == 0

    def test_delta(self):
        a = [_rec("t1", 0, True), _rec("t2", 0, True)]
        b = [_rec("t1", 0, True), _rec("t2", 0, False)]
        out = stats.paired_diff(a, b)
        assert out["diff"]["mean"] == 0.5  # +50pp for A

    def test_noninferiority(self):
        a = [_rec("t1", 0, True), _rec("t2", 0, False)]
        b = [_rec("t1", 0, True), _rec("t2", 0, True)]
        # A worse by 0.5 → fails a 3pp non-inferiority bound
        out = stats.paired_diff(a, b, noninferiority_pp=3.0)
        assert out["noninferiority"]["pass"] is False

    def test_disjoint_tasks(self):
        out = stats.paired_diff([_rec("t1", 0, True)], [_rec("t9", 0, True)])
        assert out["tasks_common"] == 0

    def test_mcnemar_counts(self):
        a = [_rec("t1", 0, True), _rec("t1", 1, False)]
        b = [_rec("t1", 0, False), _rec("t1", 1, False)]
        out = stats.paired_diff(a, b)
        assert out["mcnemar"]["a_only"] == 1
        assert out["mcnemar"]["b_only"] == 0
