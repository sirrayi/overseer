"""Session tests: create, persist, resume (no dup), list, export redaction."""

from __future__ import annotations

from pathlib import Path

import pytest

from overseer.errors import SessionError
from overseer.providers.base import ChatMessage
from overseer.session import SessionStore


def _store(tmp_path: Path) -> SessionStore:
    return SessionStore(tmp_path / "vault")


def test_create_and_load_roundtrip(tmp_path):
    store = _store(tmp_path)
    s = store.create(task="fix the bug")
    assert s.id
    loaded = store.load(s.id)
    assert loaded.id == s.id
    assert loaded.task == "fix the bug"
    assert loaded.status == "active"


def test_append_and_resume_no_duplicates(tmp_path):
    store = _store(tmp_path)
    s = store.create()
    store.append(s, ChatMessage(role="user", content="hello"))
    store.append(s, ChatMessage(role="assistant", content="hi there"))
    # Resume: load fresh from disk — must see exactly 2 messages, no dups.
    resumed = store.load(s.id)
    assert len(resumed.messages) == 2
    assert resumed.messages[0].content == "hello"
    assert resumed.messages[1].content == "hi there"


def test_resume_after_multiple_appends(tmp_path):
    store = _store(tmp_path)
    s = store.create()
    for i in range(5):
        store.append(s, ChatMessage(role="user", content=f"msg {i}"))
    resumed = store.load(s.id)
    assert len(resumed.messages) == 5
    assert resumed.messages[-1].content == "msg 4"


def test_load_missing_raises(tmp_path):
    store = _store(tmp_path)
    with pytest.raises(SessionError, match="not found"):
        store.load("nonexistent")


def test_invalid_session_id_rejected(tmp_path):
    store = _store(tmp_path)
    with pytest.raises(SessionError, match="invalid session id"):
        store.load("../escape")


def test_list_returns_metas_sorted_by_updated(tmp_path):
    store = _store(tmp_path)
    a = store.create(task="first")
    store.create(task="second")
    store.append(a, ChatMessage(role="user", content="x"))
    metas = store.list()
    assert len(metas) == 2
    assert metas[0].id == a.id  # most recently updated first
    assert all(m.task in ("first", "second") for m in metas)


def test_list_empty(tmp_path):
    store = _store(tmp_path)
    assert store.list() == []


def test_export_markdown_redacted(tmp_path):
    store = _store(tmp_path)
    s = store.create(task="secret test")
    store.append(
        s,
        ChatMessage(
            role="user",
            content="my key is sk-1234567890abcdef1234567890abcdef",
        ),
    )
    md = store.export_markdown(s)
    assert "sk-1234567890abcdef1234567890abcdef" not in md
    assert "sk-***REDACTED***" in md
    assert "# Session" in md
    assert "## Transcript" in md


def test_export_marks_tool_messages(tmp_path):
    store = _store(tmp_path)
    s = store.create()
    store.append(s, ChatMessage(role="tool", content="[ok]", tool_call_id="call_1"))
    md = store.export_markdown(s)
    assert "### tool (call_1)" in md


def test_corrupt_transcript_line_skipped(tmp_path):
    store = _store(tmp_path)
    s = store.create()
    store.append(s, ChatMessage(role="user", content="good"))
    # Corrupt the transcript with a bad line.
    d = store._dir(s.id)
    with open(d / "transcript.jsonl", "a", encoding="utf-8") as fh:
        fh.write("{not json\n")
    resumed = store.load(s.id)
    assert len(resumed.messages) == 1  # corrupt line skipped, no crash


def test_meta_persists_tokens_and_cost(tmp_path):
    store = _store(tmp_path)
    s = store.create()
    s.tokens = 1234
    s.cost = 0.05
    s.status = "done"
    store.save_meta(s)
    loaded = store.load(s.id)
    assert loaded.tokens == 1234
    assert loaded.cost == 0.05
    assert loaded.status == "done"
