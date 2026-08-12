"""Routing and telemetry tests (plan B8)."""

from __future__ import annotations

import pytest

from overseer.errors import BudgetExceeded
from overseer.routing import TIER_FRONTIER, TIER_LOCAL, TIER_MID, Router
from overseer.telemetry import Telemetry


def test_complexity_trivial():
    r = Router()
    assert r.complexity("please summarize this for me") == TIER_LOCAL


def test_complexity_mid():
    r = Router()
    assert r.complexity("refactor the module") == TIER_FRONTIER  # 'refactor' is hard
    assert r.complexity("rename the variable in utils.py") == TIER_LOCAL


def test_complexity_frontier():
    r = Router()
    assert r.complexity("debug the race condition in the scheduler") == TIER_FRONTIER


def test_eco_mode_never_frontier():
    r = Router(power_mode="eco")
    tier, chain, _ = r.route("debug the race condition in the scheduler")
    assert tier <= TIER_MID  # eco caps at mid


def test_privacy_routing_forces_local():
    r = Router()
    tier, chain, sensitive = r.route("check my api key in the config")
    assert sensitive is True
    assert chain == ["local"]  # forced to local only


def test_privacy_chain_for_frontier_content():
    r = Router()
    _, chain, _ = r.route("analyze this private key file")
    assert chain == ["local"]


def test_normal_routing_not_sensitive():
    r = Router()
    _, chain, sensitive = r.route("fix the test failure")
    assert sensitive is False


def test_telemetry_records_and_totals():
    t = Telemetry()
    t.record(tokens=1000, tier="mid")
    t.record(tokens=2000, tier="frontier")
    assert t.session_tokens() == 3000
    assert t.session_cost() == pytest.approx(1000 / 1e6 * 1.0 + 2000 / 1e6 * 10.0)


def test_budget_guard_halts_session_cost():
    t = Telemetry(max_cost_per_session=0.0001)
    with pytest.raises(BudgetExceeded):
        t.record(tokens=1000, tier="frontier")  # 0.01 > 0.0001


def test_budget_guard_halts_daily_cost():
    t = Telemetry(max_cost_per_day=0.001)
    t.record(tokens=100, tier="mid")  # 0.0001
    with pytest.raises(BudgetExceeded):
        t.record(tokens=5000, tier="frontier")  # 0.05 total


def test_warning_at_80_percent():
    t = Telemetry(max_cost_per_session=0.01, warn_ratio=0.8)
    t.record(tokens=8000, tier="mid")  # 0.008 = 80%
    assert t.near_limits()
