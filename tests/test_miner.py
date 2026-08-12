"""Pattern miner + curator tests: evidence gates, promotion, correction replay (B7)."""

from __future__ import annotations

from overseer.curator import (
    AUTO_PROMOTE_MIN_SUCCESS,
    MIN_EVIDENCE_SESSIONS,
    Skill,
    SkillRegistry,
    classify_risk,
)
from overseer.episodic import EV_ERROR, EV_TOOL_RESULT, EV_USER
from overseer.live_learning import EV_CORRECTION, LiveEvent
from overseer.miner import PatternMiner, extract_episode

# --- helpers ----------------------------------------------------------------


def _episode(session: str, tools, *, status="done", error=False, tasks=("fix",), corrections=()):
    """Build an Episode directly (avoids repeating event wiring)."""
    from overseer.miner import Episode

    return Episode(
        session_id=session,
        status=status,
        tool_names=list(tools),
        task_types=list(tasks),
        has_error=error,
        corrections=list(corrections),
    )


def _events_for(session: str, tools, *, error=False, task="fix the bug"):
    """Build raw episodic event dicts for extract_episode."""
    events: list[dict[str, str]] = [
        {"type": EV_USER, "content": task, "tool_name": ""},
    ]
    for t in tools:
        events.append({"type": EV_TOOL_RESULT, "content": "ok", "tool_name": t})
    if error:
        events.append({"type": EV_ERROR, "content": "boom", "tool_name": ""})
    return events


# --- evidence gates ----------------------------------------------------------


def test_minimum_evidence_rejects_below_three(tmp_path):
    """A pattern seen in fewer than 3 independent sessions is not mined."""
    miner = PatternMiner()
    eps = [
        _episode("s1", ["terminal", "filesystem"]),
        _episode("s2", ["terminal", "filesystem"]),
    ]
    drafts = miner.mine(eps)
    assert drafts == []


def test_minimum_evidence_requires_independent_sessions(tmp_path):
    """3 successes from the SAME session do not count as independent evidence."""
    miner = PatternMiner()
    # Same session id repeated 3x is still one independent source.
    eps = [_episode("s1", ["terminal", "filesystem"]) for _ in range(3)]
    drafts = miner.mine(eps)
    assert drafts == []


def test_evidence_floor_reached_mines_draft(tmp_path):
    """3 independent verified successes produce a draft."""
    miner = PatternMiner()
    eps = [
        _episode("s1", ["terminal", "filesystem"]),
        _episode("s2", ["terminal", "filesystem"]),
        _episode("s3", ["terminal", "filesystem"]),
    ]
    drafts = miner.mine(eps)
    assert len(drafts) == 1
    draft = drafts[0]
    assert draft.risk == "high"  # terminal is high-risk
    assert len(draft.evidence_sessions) == 3
    assert set(draft.evidence_sessions) == {"s1", "s2", "s3"}


def test_unverified_outcome_never_counts(tmp_path):
    """Failed or unverified sessions never contribute to evidence."""
    miner = PatternMiner()
    eps = [
        _episode("s1", ["terminal"]),  # verified success
        _episode("s2", ["terminal"]),  # verified success
        _episode("s3", ["terminal"]),  # verified success
        _episode("s4", ["terminal"], error=True),  # failed — no evidence
    ]
    drafts = miner.mine(eps)
    # Evidence floor still met (3 successes), but rate = 3/4 = 0.75 < 0.90 high
    assert len(drafts) == 0  # threshold gate rejects


def test_success_threshold_gating(tmp_path):
    """High-risk pattern with rate below 90% is rejected."""
    miner = PatternMiner()
    eps = [
        _episode("s1", ["terminal"]),  # success
        _episode("s2", ["terminal"]),  # success
        _episode("s3", ["terminal"]),  # success
        _episode("s4", ["terminal"], error=True),  # failure
        _episode("s5", ["terminal"], error=True),  # failure
    ]
    # rate = 3/5 = 0.60 < 0.90
    assert miner.mine(eps) == []


def test_low_risk_lower_threshold(tmp_path):
    """Low-risk pattern passes at >= 70% success."""
    miner = PatternMiner()
    # read-only tools -> low risk
    eps = [
        _episode("s1", ["search", "read"]),
        _episode("s2", ["search", "read"]),
        _episode("s3", ["search", "read"]),
        _episode("s4", ["search", "read"], error=True),  # 3/4 = 0.75 >= 0.70
    ]
    drafts = miner.mine(eps)
    assert len(drafts) == 1
    assert drafts[0].risk == "low"
    assert drafts[0].success_rate == 0.75


# --- risk classification ------------------------------------------------------


def test_classify_risk_high_for_terminal():
    assert classify_risk(["terminal", "read"]) == "high"
    assert classify_risk(["search", "read"]) == "low"
    assert classify_risk([]) == "low"


# --- episode extraction -------------------------------------------------------


def test_extract_episode_flags_error_and_tools():
    events = _events_for("s1", ["terminal", "filesystem"], error=True)
    ep = extract_episode("s1", "done", events)
    assert ep.has_error is True
    assert "terminal" in ep.tool_names
    assert "filesystem" in ep.tool_names
    assert "debugging" in ep.task_types  # "fix" maps to the debugging label


def test_extract_episode_rejects_not_done():
    events = _events_for("s2", ["terminal"])
    ep = extract_episode("s2", "error", events)
    assert _verified(ep) is False


def _verified(ep) -> bool:
    from overseer.miner import _is_verified_success

    return _is_verified_success(ep)


# --- curator: promotion -------------------------------------------------------


def _make_skill(risk="low") -> Skill:
    return Skill(
        id="",
        title="run tests",
        trigger="task involves testing",
        steps="1. run pytest\n2. check output",
        risk=risk,
    )


def test_high_risk_requires_human_approval(tmp_path):
    reg = SkillRegistry(tmp_path)
    skill = _make_skill(risk="high")
    sid = reg.create_draft(skill)

    # Simulate 3 verified uses (all success) -> 100% success.
    for _ in range(MIN_EVIDENCE_SESSIONS):
        s = reg.record_use(sid, success=True)

    # High risk must NOT auto-promote even after repeated success.
    got = reg.get(sid)
    assert got is not None and got.status != "active"

    # Without approval it stays proposed.
    s = reg.promote(sid, approved=False)
    assert s.status == "rejected"


def test_high_risk_approved_becomes_active(tmp_path):
    reg = SkillRegistry(tmp_path)
    sid = reg.create_draft(_make_skill(risk="high"))
    for _ in range(MIN_EVIDENCE_SESSIONS):
        reg.record_use(sid, success=True)
    s = reg.promote(sid, approved=True)
    assert s.status == "active"


def test_high_risk_approved_without_evidence_stays_proposed(tmp_path):
    reg = SkillRegistry(tmp_path)
    sid = reg.create_draft(_make_skill(risk="high"))
    reg.record_use(sid, success=True)  # only 1 use — below evidence floor
    s = reg.promote(sid, approved=True)
    assert s.status == "proposed"


def test_low_risk_auto_promotes_after_repeated_success(tmp_path):
    reg = SkillRegistry(tmp_path)
    sid = reg.create_draft(_make_skill(risk="low"))
    for _ in range(AUTO_PROMOTE_MIN_SUCCESS):
        reg.record_use(sid, success=True)
    got = reg.get(sid)
    assert got is not None and got.status == "active"


# --- correction replay ---------------------------------------------------------


def test_correction_replay_blocks_conflicting_draft(tmp_path):
    """A draft promoting a tool a high-confidence correction forbids is flagged."""
    miner = PatternMiner()
    # 3 verified successes using terminal, but a high-confidence correction
    # says "don't use terminal". The correction flows via live events.
    correction = LiveEvent(
        type=EV_CORRECTION,
        scope="session",
        content="don't use terminal for this",
        confidence=0.8,
        session_id="s1",
    )
    events = [
        _events_for("s1", ["terminal"]),
        _events_for("s2", ["terminal"]),
        _events_for("s3", ["terminal"]),
    ]
    episodes = [
        extract_episode(f"s{i + 1}", "done", evs, live_events=[correction] if i == 0 else None)
        for i, evs in enumerate(events)
    ]
    drafts = miner.mine(episodes)
    assert drafts, "expected a draft to be produced"
    assert drafts[0].conflict is True
    assert "terminal" in drafts[0].conflict_reason


def test_non_conflicting_correction_does_not_block(tmp_path):
    """An unrelated correction does not flag the draft."""
    miner = PatternMiner()
    # Correction about something else entirely.
    correction = LiveEvent(
        type=EV_CORRECTION,
        scope="session",
        content="don't use lodash for utilities",
        confidence=0.8,
        session_id="s1",
    )
    events = [
        _events_for("s1", ["search"]),
        _events_for("s2", ["search"]),
        _events_for("s3", ["search"]),
    ]
    episodes = [
        extract_episode(f"s{i + 1}", "done", evs, live_events=[correction] if i == 0 else None)
        for i, evs in enumerate(events)
    ]
    drafts = miner.mine(episodes)
    assert drafts
    assert drafts[0].conflict is False


# --- skill frontmatter governance ---------------------------------------------


def test_skill_note_frontmatter_validation(tmp_path):
    """Written skill notes carry the strict B0 frontmatter schema."""
    reg = SkillRegistry(tmp_path)
    reg.create_draft(_make_skill())
    notes = list((tmp_path / "40-Skills").glob("*.md"))
    assert len(notes) == 1
    text = notes[0].read_text(encoding="utf-8")
    for field in (
        "trigger",
        "risk",
        "use_count",
        "success_count",
        "failure_count",
        "confidence",
        "status",
    ):
        assert f"{field}:" in text
    assert "status: draft" in text
    assert "type: skill" in text
    assert "id:" in text


def test_skill_roundtrip_load(tmp_path):
    """A written skill can be read back with its counters intact."""
    reg = SkillRegistry(tmp_path)
    sid = reg.create_draft(_make_skill(risk="high"))
    reg.record_use(sid, success=True)
    loaded = reg.get(sid)
    assert loaded is not None
    assert loaded.use_count == 1
    assert loaded.success_count == 1
    assert loaded.risk == "high"
    assert loaded.status in ("draft", "proposed")
