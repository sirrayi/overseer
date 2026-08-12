"""Episodic memory: append-only observation stream, SQLite + FTS5 (plan B3).

Every user message, assistant message, tool call, tool result, approval
decision, and error is recorded as an Event. Events are redacted before
touching disk. The SQLite database (.overseer/episodic.sqlite) is a derived
cache: if deleted, it can be rebuilt from the raw transcript logs.

Concurrency: WAL mode + busy_timeout for safe concurrent append/search.
"""

from __future__ import annotations

import sqlite3
import threading
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from overseer.redact import redact

DB_NAME = "episodic.sqlite"
_BUSY_TIMEOUT_MS = 5000

# Event types (observation stream vocabulary).
EV_USER = "user"
EV_ASSISTANT = "assistant"
EV_TOOL_CALL = "tool_call"
EV_TOOL_RESULT = "tool_result"
EV_APPROVAL = "approval"
EV_ERROR = "error"
EV_SYSTEM = "system"


def _now() -> str:
    return datetime.now(UTC).isoformat(timespec="microseconds")


@dataclass
class Event:
    """One observation-stream event. Redacted before persistence."""

    type: str
    session_id: str
    content: str = ""
    tool_name: str = ""
    trace_id: str = ""
    ts: str = field(default_factory=_now)

    def to_row(self) -> tuple[str, str, str, str, str, str]:
        return (self.type, self.session_id, self.trace_id, self.ts, self.tool_name, self.content)


class EpisodicStore:
    """SQLite (WAL) + FTS5 store for the observation stream."""

    def __init__(self, overseer_dir: str | Path) -> None:
        self.db_path = Path(overseer_dir) / DB_NAME
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        # One connection, guarded by a lock: sqlite3 connections are not
        # thread-safe even with check_same_thread=False. WAL still allows
        # concurrent readers from other processes.
        self._lock = threading.RLock()
        self._conn = sqlite3.connect(str(self.db_path), check_same_thread=False)
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute(f"PRAGMA busy_timeout={_BUSY_TIMEOUT_MS}")
        self._init_schema()

    def _init_schema(self) -> None:
        with self._lock, self._conn:
            self._conn.execute(
                """
                CREATE TABLE IF NOT EXISTS events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    type TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    trace_id TEXT NOT NULL DEFAULT '',
                    ts TEXT NOT NULL,
                    tool_name TEXT NOT NULL DEFAULT '',
                    content TEXT NOT NULL DEFAULT ''
                )
                """
            )
            self._conn.execute(
                """
                CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
                    content, tool_name, session_id, type,
                    content='events', content_rowid='id'
                )
                """
            )
            # Triggers keep the FTS index in sync with the events table.
            self._conn.execute(
                """
                CREATE TRIGGER IF NOT EXISTS events_ai AFTER INSERT ON events BEGIN
                    INSERT INTO events_fts(rowid, content, tool_name, session_id, type)
                    VALUES (new.id, new.content, new.tool_name, new.session_id, new.type);
                END
                """
            )
            self._conn.execute(
                """
                CREATE TRIGGER IF NOT EXISTS events_ad AFTER DELETE ON events BEGIN
                    INSERT INTO events_fts(events_fts, rowid, content, tool_name, session_id, type)
                    VALUES ('delete', old.id, old.content, old.tool_name, old.session_id, old.type);
                END
                """
            )

    def append(self, event: Event) -> None:
        """Append one event. Content is redacted before hitting disk."""
        event.content = redact(event.content)
        with self._lock, self._conn:
            self._conn.execute(
                "INSERT INTO events (type, session_id, trace_id, ts, tool_name, content) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                event.to_row(),
            )

    def append_many(self, events: list[Event]) -> None:
        """Batch append (efficiency: rapid event bursts)."""
        rows = []
        for e in events:
            e.content = redact(e.content)
            rows.append(e.to_row())
        with self._lock, self._conn:
            self._conn.executemany(
                "INSERT INTO events (type, session_id, trace_id, ts, tool_name, content) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                rows,
            )

    def search(self, query: str, limit: int = 20) -> list[dict[str, Any]]:
        """Full-text search over events. Returns id, session, type, snippet."""
        try:
            with self._lock:
                cur = self._conn.execute(
                    """
                    SELECT e.id, e.session_id, e.type, e.ts, e.tool_name,
                           snippet(events_fts, 0, '[', ']', '…', 12) AS snip
                    FROM events_fts
                    JOIN events e ON e.id = events_fts.rowid
                    WHERE events_fts MATCH ?
                    ORDER BY e.id DESC
                    LIMIT ?
                    """,
                    (query, limit),
                )
                rows = cur.fetchall()
        except sqlite3.OperationalError:
            # Malformed FTS query (e.g. stray quotes) -> no results, no crash.
            return []
        return [
            {
                "id": row[0],
                "session_id": row[1],
                "type": row[2],
                "ts": row[3],
                "tool_name": row[4],
                "snippet": redact(row[5] or ""),
            }
            for row in rows
        ]

    def by_session(self, session_id: str, limit: int = 1000) -> list[dict[str, Any]]:
        """All events for a session (not FTS — exact session_id match)."""
        with self._lock:
            cur = self._conn.execute(
                "SELECT id, type, session_id, trace_id, ts, tool_name, content "
                "FROM events WHERE session_id = ? ORDER BY id ASC LIMIT ?",
                (session_id, limit),
            )
            rows = cur.fetchall()
        return [
            {
                "id": r[0],
                "type": r[1],
                "session_id": r[2],
                "trace_id": r[3],
                "ts": r[4],
                "tool_name": r[5],
                "content": redact(r[6] or ""),
            }
            for r in rows
        ]

    def count(self) -> int:
        with self._lock:
            cur = self._conn.execute("SELECT COUNT(*) FROM events")
            return int(cur.fetchone()[0])

    def rebuild(self, transcripts: list[tuple[str, list[dict[str, str]]]]) -> int:
        """Rebuild the store from raw transcript logs (derived-cache rule).

        transcripts: list of (session_id, [{"role":..., "content":...}, ...]).
        Returns the number of events written.
        """
        with self._lock, self._conn:
            self._conn.execute("DELETE FROM events")
        events: list[Event] = []
        for session_id, lines in transcripts:
            for line in lines:
                role = line.get("role", "user")
                etype = {
                    "user": EV_USER,
                    "assistant": EV_ASSISTANT,
                    "tool": EV_TOOL_RESULT,
                    "system": EV_SYSTEM,
                }.get(role, EV_SYSTEM)
                events.append(
                    Event(
                        type=etype,
                        session_id=session_id,
                        content=line.get("content", ""),
                        tool_name=line.get("tool_name", ""),
                    )
                )
        self.append_many(events)
        return len(events)

    def close(self) -> None:
        with self._lock:
            self._conn.close()
