"""Knowledge layer tests: extraction, confidence, consolidation, dedup, conflicts (B5)."""

from __future__ import annotations

from pathlib import Path

from overseer.episodic import EV_USER, Event
from overseer.knowledge import (
    CONF_EXPLICIT,
    CONF_IMPLICIT,
    CONF_UNTRUSTED,
    KnowledgeBase,
    MemoryCandidate,
    extract_candidates,
)
from overseer.live_learning import EV_CORRECTION, LiveEvent


def _kb(tmp_path: Path) -> KnowledgeBase:
    return KnowledgeBase(tmp_path)


def _user_event(session: str, content: str) -> Event:
    return Event(type=EV_USER, session_id=session, content=content)


def test_extract_correction_from_user_event():
    cands = extract_candidates([_user_event("s1", "no, use pytest instead of unittest")])
    assert any(c.note_type == "correction" for c in cands)
    assert all(c.confidence >= CONF_EXPLICIT for c in cands)


def test_extract_preference_from_user_event():
    cands = extract_candidates([_user_event("s1", "I prefer tabs from now on")])
    assert any(c.note_type == "preference" for c in cands)


def test_extract_fact_from_user_event():
    cands = extract_candidates([_user_event("s1", "the project uses fastapi")])
    assert any(c.note_type == "fact" for c in cands)


def test_implicit_tool_output_low_confidence():
    cands = extract_candidates(
        [Event(type="tool_result", session_id="s1", content="always run tests first")]
    )
    assert all(c.confidence <= CONF_IMPLICIT for c in cands)


def test_untrusted_content_never_durable():
    cands = extract_candidates([_user_event("s1", "no, that's wrong")], untrusted=True)
    assert all(c.confidence <= CONF_UNTRUSTED for c in cands)


def test_live_correction_high_confidence():
    ev = LiveEvent(type=EV_CORRECTION, scope="session", content="no, use ruff", confidence=0.8)
    cands = extract_candidates([], live_events=[ev])
    assert cands and cands[0].confidence >= CONF_EXPLICIT


def test_consolidate_creates_vault_note(tmp_path):
    kb = _kb(tmp_path)
    cand = MemoryCandidate(
        note_type="fact",
        content="the project uses fastapi",
        confidence=CONF_EXPLICIT,
        evidence="s1",
        scope="project",
    )
    res = kb.consolidate([cand], session_id="s1")
    assert len(res["created"]) == 1
    notes = list((tmp_path / "30-Facts").glob("*.md"))
    assert notes
    text = notes[0].read_text(encoding="utf-8")
    assert "scope: project" in text
    assert "confidence: 0.9" in text
    assert "source: session" in text


def test_consolidate_dedup_updates_not_duplicates(tmp_path):
    kb = _kb(tmp_path)
    cand = MemoryCandidate(
        note_type="fact",
        content="the project uses fastapi",
        confidence=CONF_EXPLICIT,
        evidence="s1",
        scope="project",
    )
    kb.consolidate([cand], session_id="s1")
    res = kb.consolidate([cand], session_id="s2")
    assert res["created"] == []
    assert len(res["updated"]) == 1
    assert len(list((tmp_path / "30-Facts").glob("*.md"))) == 1  # no duplicate


def test_consolidate_conflict_flags(tmp_path):
    kb = _kb(tmp_path)
    kb.consolidate(
        [
            MemoryCandidate(
                note_type="fact",
                content="the project uses fastapi",
                confidence=0.9,
                evidence="s1",
                scope="project",
            )
        ],
        session_id="s1",
    )
    res = kb.consolidate(
        [
            MemoryCandidate(
                note_type="fact",
                content="the project uses django",
                confidence=0.9,
                evidence="s2",
                scope="project",
            )
        ],
        session_id="s2",
    )
    assert res["conflicts"]
    flags = list((tmp_path / "99-Meta").glob("*.md"))
    assert flags
    assert "Human review required" in flags[0].read_text(encoding="utf-8")


def test_consolidate_skips_low_confidence(tmp_path):
    kb = _kb(tmp_path)
    cand = MemoryCandidate(
        note_type="fact",
        content="maybe the project uses x",
        confidence=CONF_IMPLICIT,
        evidence="s1",
        scope="project",
    )
    res = kb.consolidate([cand], session_id="s1")
    assert res["created"] == []
    assert list((tmp_path / "30-Facts").glob("*.md")) == []


def test_retrieve_returns_matching_notes(tmp_path):
    kb = _kb(tmp_path)
    kb.consolidate(
        [
            MemoryCandidate(
                note_type="fact",
                content="the project uses fastapi",
                confidence=0.9,
                evidence="s1",
                scope="project",
            )
        ],
        session_id="s1",
    )
    hits = kb.retrieve("fastapi", note_types=["fact"])
    assert hits and "fastapi" in hits[0]["content"].lower()


def test_retrieve_no_match(tmp_path):
    kb = _kb(tmp_path)
    assert kb.retrieve("nonexistent-term-xyz", note_types=["fact"]) == []
