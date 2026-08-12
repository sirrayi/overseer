"""Episodic store tests: append, FTS5 search, redaction, WAL, concurrency, rebuild."""

from __future__ import annotations

import sqlite3
import threading
from pathlib import Path

from overseer.episodic import EpisodicStore, Event


def _store(tmp_path: Path) -> EpisodicStore:
    return EpisodicStore(tmp_path / ".overseer")


def test_append_and_count(tmp_path):
    store = _store(tmp_path)
    store.append(Event(type="user", session_id="s1", content="hello world"))
    store.append(Event(type="assistant", session_id="s1", content="hi there"))
    assert store.count() == 2


def test_fts_search_finds_content(tmp_path):
    store = _store(tmp_path)
    store.append(Event(type="user", session_id="s1", content="fix the flaky test"))
    store.append(Event(type="assistant", session_id="s1", content="done"))
    results = store.search("flaky")
    assert len(results) == 1
    assert results[0]["session_id"] == "s1"
    assert "flaky" in results[0]["snippet"]


def test_fts_search_no_results(tmp_path):
    store = _store(tmp_path)
    store.append(Event(type="user", session_id="s1", content="hello"))
    assert store.search("nonexistentterm") == []


def test_fts_search_malformed_query_no_crash(tmp_path):
    store = _store(tmp_path)
    store.append(Event(type="user", session_id="s1", content="hello"))
    # A stray quote makes an invalid FTS5 query — must return [], not crash.
    assert store.search('"unclosed') == []


def test_events_redacted_before_disk(tmp_path):
    store = _store(tmp_path)
    store.append(
        Event(type="user", session_id="s1", content="my key is sk-1234567890abcdef1234567890abcdef")
    )
    # Raw DB must not contain the secret.
    conn = sqlite3.connect(store.db_path)
    row = conn.execute("SELECT content FROM events").fetchone()
    conn.close()
    assert "sk-1234567890abcdef1234567890abcdef" not in row[0]
    assert "sk-***REDACTED***" in row[0]


def test_search_output_redacted(tmp_path):
    store = _store(tmp_path)
    store.append(
        Event(
            type="user", session_id="s1", content="token sk-1234567890abcdef1234567890abcdef here"
        )
    )
    results = store.search("token")
    assert "sk-1234567890abcdef1234567890abcdef" not in results[0]["snippet"]


def test_wal_mode_enabled(tmp_path):
    store = _store(tmp_path)
    conn = sqlite3.connect(store.db_path)
    mode = conn.execute("PRAGMA journal_mode").fetchone()[0]
    conn.close()
    assert mode == "wal"


def test_concurrent_append(tmp_path):
    """Concurrent appends from multiple threads must not lose events."""
    store = _store(tmp_path)
    errors: list[Exception] = []

    def worker(n: int) -> None:
        try:
            for i in range(20):
                store.append(Event(type="user", session_id=f"s{n}", content=f"msg {i}"))
        except Exception as exc:  # pragma: no cover
            errors.append(exc)

    threads = [threading.Thread(target=worker, args=(n,)) for n in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert not errors
    assert store.count() == 80


def test_batch_append(tmp_path):
    store = _store(tmp_path)
    store.append_many([Event(type="user", session_id="s1", content=f"msg {i}") for i in range(50)])
    assert store.count() == 50


def test_rebuild_from_transcripts(tmp_path):
    store = _store(tmp_path)
    store.append(Event(type="user", session_id="old", content="stale"))
    transcripts = [
        ("s1", [{"role": "user", "content": "hello"}, {"role": "assistant", "content": "hi"}]),
        ("s2", [{"role": "tool", "content": "result", "tool_name": "file_read"}]),
    ]
    n = store.rebuild(transcripts)
    assert n == 3
    assert store.count() == 3
    # Old events are gone (derived cache rebuilt from scratch).
    assert store.search("stale") == []
    assert len(store.search("hello")) == 1


def test_rebuild_redacts(tmp_path):
    store = _store(tmp_path)
    store.rebuild(
        [("s1", [{"role": "user", "content": "key sk-1234567890abcdef1234567890abcdef"}])]
    )
    conn = sqlite3.connect(store.db_path)
    row = conn.execute("SELECT content FROM events").fetchone()
    conn.close()
    assert "sk-1234567890abcdef1234567890abcdef" not in row[0]


def test_close_then_reopen(tmp_path):
    store = _store(tmp_path)
    store.append(Event(type="user", session_id="s1", content="persist me"))
    store.close()
    store2 = _store(tmp_path)
    assert store2.count() == 1
    store2.close()


def test_by_session_filters(tmp_path):
    """by_session must return only that session's events (exact match)."""
    store = _store(tmp_path)
    store.append(Event(type="user", session_id="s1", content="hello"))
    store.append(Event(type="user", session_id="s2", content="other"))
    store.append(Event(type="assistant", session_id="s1", content="hi"))
    rows = store.by_session("s1")
    assert len(rows) == 2
    assert all(r["session_id"] == "s1" for r in rows)
