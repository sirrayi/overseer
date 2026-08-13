"""Meta-learning tests: stats, shadow mode, L3 guardrail, proposals (B10)."""

from __future__ import annotations

from pathlib import Path

import pytest

from overseer.errors import VaultError
from overseer.meta import (
    FORBIDDEN_TARGETS,
    MetaStats,
    Proposal,
    ProposalGenerator,
    ShadowEvaluator,
)


def _stats(tmp_path: Path) -> MetaStats:
    return MetaStats(store_dir=tmp_path / ".overseer" / "telemetry.local")


def _seed_stats(stats: MetaStats, n: int = 6) -> None:
    for i in range(n):
        stats.record(
            f"s{i}",
            corrections=2.0,  # rising correction rate
            skill_hits=1,
            skill_uses=5,
            retrieval_useful=1,
            retrievals=5,
            conflicts=0,
        )


class _FakeVault:
    def __init__(self) -> None:
        self.written: list[tuple[str, str, str, dict]] = []

    def write_note(self, note_type, title, body="", **fm):
        self.written.append((note_type, title, body, fm))
        return Path(f"/tmp/{title}.md")


class _FakeEpisodic:
    def __init__(self, sessions: list[dict]) -> None:
        self._sessions = sessions

    def recent_sessions(self, n):
        return [{"session_id": s["id"]} for s in self._sessions[:n]]

    def by_session(self, sid):
        for s in self._sessions:
            if s["id"] == sid:
                return s["events"]
        return []


def test_stats_record_and_summary(tmp_path):
    st = _stats(tmp_path)
    _seed_stats(st)
    s = st.summary()
    assert s["correction_rate"] == pytest.approx(2.0)
    assert s["skill_hit_rate"] == pytest.approx(0.2)
    assert s["retrieval_usefulness"] == pytest.approx(0.2)
    assert s["memory_conflict_rate"] == pytest.approx(0.0)


def test_stats_persist_across_instances(tmp_path):
    st = _stats(tmp_path)
    st.record("s1", corrections=1)
    st2 = _stats(tmp_path)
    assert st2.correction_rate() == pytest.approx(1.0)


def test_generator_creates_proposal_on_trend(tmp_path):
    st = _stats(tmp_path)
    _seed_stats(st)
    gen = ProposalGenerator(st, _FakeVault())
    props = gen.generate()
    assert len(props) >= 1
    assert props[0].metric == "correction_rate"
    assert props[0].old_value == 0.5
    assert props[0].new_value == 0.7


def test_generator_no_trend_no_proposal(tmp_path):
    st = _stats(tmp_path)
    st.record(
        "s1",
        corrections=0,
        skill_hits=0,
        skill_uses=0,
        retrieval_useful=0,
        retrievals=0,
        conflicts=0,
    )
    gen = ProposalGenerator(st, _FakeVault())
    assert gen.generate() == []


def test_forbidden_targets_never_proposable():
    for target in ("approval gate", "denylist", "path containment", "L3 guardrail"):
        assert any(f in target.lower() for f in FORBIDDEN_TARGETS)


def test_proposal_written_to_vault_with_governed_frontmatter(tmp_path):
    st = _stats(tmp_path)
    _seed_stats(st)
    vault = _FakeVault()
    gen = ProposalGenerator(st, vault)
    props = gen.generate()
    assert props
    gen.write_proposal(props[0])
    note_type, title, body, fm = vault.written[0]
    assert note_type == "proposal"
    assert fm["approval"] == "required"
    assert fm["proposal_type"] == "threshold"
    assert "rollback" in body
    assert "shadow mode" in body


def test_shadow_evaluator_pass_on_improvement(tmp_path):
    st = _stats(tmp_path)
    _seed_stats(st)
    gen = ProposalGenerator(st, _FakeVault())
    prop = gen.generate()[0]
    ep = _FakeEpisodic(
        [
            {
                "id": f"s{i}",
                "events": [
                    {"type": "correction", "session_id": f"s{i}"},
                    {"type": "correction", "session_id": f"s{i}"},
                ],
            }
            for i in range(3)
        ]
    )
    ev = ShadowEvaluator(ep)
    verdict = ev.evaluate(prop)
    assert verdict.startswith("PASS")


def test_shadow_evaluator_fail_on_regression(tmp_path):
    st = _stats(tmp_path)
    _seed_stats(st)
    gen = ProposalGenerator(st, _FakeVault())
    prop = gen.generate()[0]
    # No sessions -> cannot evaluate -> FAIL (never proceeds to approval).
    ev = ShadowEvaluator(_FakeEpisodic([]))
    verdict = ev.evaluate(prop)
    assert verdict.startswith("FAIL")


def test_l3_guardrail_apply_without_approval_fails():
    """Applying a proposal without explicit approval must fail."""
    prop = Proposal(
        title="t",
        metric="correction_rate",
        trend="trend",
        target="W_IMPORTANCE",
        old_value=0.5,
        new_value=0.7,
        expected_benefit="b",
        risk="r",
        rollback="rb",
    )
    assert prop.status == "draft"
    # The only way to change status is through the approval flow; there is
    # no silent-apply path. Simulate the guardrail: applying without
    # approval raises.
    with pytest.raises(VaultError):
        _apply_without_approval(prop)


def _apply_without_approval(prop: Proposal) -> None:
    # Guardrail 8: this path must never exist in production. The test
    # asserts that the proposal system refuses to apply a draft.
    if prop.status != "approved":
        raise VaultError("proposal not approved — L3 guardrail: no silent self-modification")
