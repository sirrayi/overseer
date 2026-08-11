"""B3 tests: vault session-note bridge, CLI search/rebuild, observation wiring."""

from __future__ import annotations

from pathlib import Path

from typer.testing import CliRunner

from overseer.cli import app
from overseer.providers.base import ChatMessage
from overseer.session import SessionStore

runner = CliRunner()


def _store(tmp_path: Path) -> SessionStore:
    return SessionStore(tmp_path / "vault")


def _session_with_messages(tmp_path: Path) -> tuple[SessionStore, object]:
    store = _store(tmp_path)
    s = store.create(task="explain the codebase")
    store.append(s, ChatMessage(role="user", content="explain the codebase"))
    store.append(s, ChatMessage(role="assistant", content="the codebase is a vault-native harness"))
    s.tokens = 1200
    s.cost = 0.0024
    s.status = "done"
    store.save_meta(s)
    return store, s


def test_session_note_written_to_vault(tmp_path):
    """End of session must write a summary note to 10-Sessions/ (B3)."""
    from overseer.cli import _write_session_note

    store, s = _session_with_messages(tmp_path)

    # _write_session_note needs a runtime-like object with cfg.vault_path.
    class _Cfg:
        vault_path = str(tmp_path / "vault")

    class _Runtime:
        cfg = _Cfg()

    _write_session_note(_Runtime(), s)
    sessions_dir = tmp_path / "vault" / "10-Sessions"
    notes = list(sessions_dir.glob("*.md"))
    assert len(notes) == 1
    text = notes[0].read_text(encoding="utf-8")
    # Frontmatter must be valid and complete.
    assert "id: OVR-SESS-" in text
    assert "type: session" in text
    assert "title:" in text
    assert "created:" in text
    assert "modified:" in text
    assert "status: accepted" in text
    # Summary, not a transcript dump.
    assert "## Summary" in text
    assert "explain the codebase" in text
    assert "vault-native harness" in text
    assert "OVR-SESS-" in text


def test_session_note_redacts_secrets(tmp_path):
    """Vault notes must be redacted (B3 safety)."""
    from overseer.cli import _write_session_note

    store = _store(tmp_path)
    s = store.create(task="task with sk-ant-secret1234567890abcdefghijklmnop")
    store.append(
        s,
        ChatMessage(role="user", content="use key sk-ant-secret1234567890abcdefghijklmnop please"),
    )
    store.append(s, ChatMessage(role="assistant", content="done"))
    s.status = "done"
    store.save_meta(s)

    class _Cfg:
        vault_path = str(tmp_path / "vault")

    class _Runtime:
        cfg = _Cfg()

    _write_session_note(_Runtime(), s)
    notes = list((tmp_path / "vault" / "10-Sessions").glob("*.md"))
    text = notes[0].read_text(encoding="utf-8")
    assert "sk-ant-secret1234567890abcdefghijklmnop" not in text
    assert "«redacted" in text or "REDACTED" in text


def test_observation_stream_records_events(tmp_path):
    """Appending messages must mirror them into the episodic store (B3)."""
    store, s = _session_with_messages(tmp_path)
    assert store.episodic.count() >= 2
    hits = store.episodic.search("codebase")
    assert hits, "FTS5 must find the session content"
    assert hits[0]["session_id"] == s.id


def test_cli_search_command(tmp_path, monkeypatch):
    """overseer search <query> must return matching session events."""

    store, s = _session_with_messages(tmp_path)

    class _Cfg:
        vault_path = str(tmp_path / "vault")

    class _Runtime:
        cfg = _Cfg()
        session_store = store

    monkeypatch.setattr("overseer.cli._build_runtime", lambda config, **kw: _Runtime())
    result = runner.invoke(app, ["search", "codebase", "--config", "x.yaml"])
    assert result.exit_code == 0
    assert "codebase" in result.output


def test_cli_rebuild_command(tmp_path, monkeypatch):
    """overseer rebuild must rebuild the episodic index from transcripts."""

    store, s = _session_with_messages(tmp_path)
    # Corrupt the index: delete all events, then rebuild from transcripts.
    store.episodic.rebuild([])
    assert store.episodic.count() == 0

    class _Cfg:
        vault_path = str(tmp_path / "vault")

    class _Runtime:
        cfg = _Cfg()
        session_store = store

    monkeypatch.setattr("overseer.cli._build_runtime", lambda config, **kw: _Runtime())
    result = runner.invoke(app, ["rebuild", "--config", "x.yaml"])
    assert result.exit_code == 0
    assert store.episodic.count() >= 2
    assert "rebuilt" in result.output


def test_episodic_db_is_derived_cache(tmp_path):
    """Deleting episodic.sqlite must not lose the raw transcript (B3)."""
    store, s = _session_with_messages(tmp_path)
    db = tmp_path / "vault" / ".overseer" / "episodic.sqlite"
    assert db.is_file()
    db.unlink()
    # Raw transcript survives.
    transcript = tmp_path / "vault" / ".overseer" / "sessions" / s.id / "transcript.jsonl"
    assert transcript.is_file()
    # Rebuild restores the index.
    store2 = _store(tmp_path)
    store2.episodic.rebuild([(s.id, [{"role": "user", "content": "explain the codebase"}])])
    assert store2.episodic.search("codebase")
