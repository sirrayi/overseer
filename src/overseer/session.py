"""Session lifecycle: create, persist, resume, list, export (plan B2).

Sessions live under <vault>/.overseer/sessions/<id>/:
  - transcript.jsonl  (append-only event log; resume-safe)
  - meta.json         (id, created, updated, task, status, tokens, cost)

Resume never duplicates events: the transcript is append-only and the
session id is the resume key. Exports are redacted markdown.
"""

from __future__ import annotations

import json
import os
import uuid
from dataclasses import asdict, dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from overseer.episodic import (
    EV_APPROVAL,
    EV_ASSISTANT,
    EV_ERROR,
    EV_SYSTEM,
    EV_TOOL_CALL,
    EV_TOOL_RESULT,
    EV_USER,
    EpisodicStore,
    Event,
)
from overseer.errors import SessionError
from overseer.providers.base import ChatMessage
from overseer.redact import redact

SESSIONS_DIR = "sessions"
TRANSCRIPT_FILE = "transcript.jsonl"
META_FILE = "meta.json"

# Rough cost per 1M tokens (USD) per provider family; used for display only.
COST_PER_1M: dict[str, float] = {
    "deepseek": 0.5,
    "kimi": 1.0,
    "ollama": 0.0,
    "openai": 5.0,
    "default": 2.0,
}


def _now() -> str:
    # Microsecond precision: sessions created/updated in the same second must
    # still sort deterministically by updated time.
    return datetime.now(UTC).isoformat(timespec="microseconds")


def _cost_for(provider_name: str, tokens: int) -> float:
    key = next((k for k in COST_PER_1M if k in provider_name.lower()), "default")
    return round(tokens / 1_000_000 * COST_PER_1M[key], 4)


@dataclass
class SessionMeta:
    """Lightweight metadata for listing (never loads the transcript)."""

    id: str
    created: str
    updated: str
    task: str = ""
    status: str = "active"  # active | done | error
    tokens: int = 0
    cost: float = 0.0
    provider: str = ""

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> SessionMeta:
        return cls(**{k: v for k, v in d.items() if k in cls.__dataclass_fields__})


@dataclass
class Session:
    """A persisted conversation with the agent loop."""

    id: str
    created: str
    updated: str
    task: str
    status: str = "active"
    tokens: int = 0
    cost: float = 0.0
    provider: str = ""
    messages: list[ChatMessage] = field(default_factory=list)

    def meta(self) -> SessionMeta:
        return SessionMeta(
            id=self.id,
            created=self.created,
            updated=self.updated,
            task=self.task,
            status=self.status,
            tokens=self.tokens,
            cost=self.cost,
            provider=self.provider,
        )


class SessionStore:
    """Persists sessions under <vault>/.overseer/sessions/."""

    def __init__(self, vault_root: str | Path) -> None:
        self.root = Path(vault_root).expanduser().resolve() / ".overseer" / SESSIONS_DIR
        self.episodic = EpisodicStore(Path(vault_root).expanduser().resolve() / ".overseer")

    def _dir(self, session_id: str) -> Path:
        # Session ids are uuid4 hex — no traversal risk, but guard anyway.
        if not session_id or any(c in session_id for c in ("/", "\\", "..")):
            raise SessionError(f"invalid session id: {session_id!r}")
        return self.root / session_id

    def create(self, task: str = "") -> Session:
        """Start a new session with a unique id."""
        self.root.mkdir(parents=True, exist_ok=True)
        stamp = _now()
        session = Session(
            id=uuid.uuid4().hex[:12],
            created=stamp,
            updated=stamp,
            task=task,
        )
        self._save_meta(session)
        return session

    def load(self, session_id: str) -> Session:
        """Load a session (meta + transcript). Raises SessionError if missing."""
        d = self._dir(session_id)
        meta_path = d / META_FILE
        if not meta_path.is_file():
            raise SessionError(f"session not found: {session_id}")
        try:
            meta = json.loads(meta_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError) as exc:
            raise SessionError(f"session meta corrupt: {session_id}: {exc}") from exc
        session = Session(
            id=meta.get("id", session_id),
            created=meta.get("created", ""),
            updated=meta.get("updated", ""),
            task=meta.get("task", ""),
            status=meta.get("status", "active"),
            tokens=meta.get("tokens", 0),
            cost=meta.get("cost", 0.0),
            provider=meta.get("provider", ""),
        )
        transcript_path = d / TRANSCRIPT_FILE
        if transcript_path.is_file():
            for line in transcript_path.read_text(encoding="utf-8").splitlines():
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue  # skip corrupt lines; never crash a resume
                session.messages.append(
                    ChatMessage(
                        role=event.get("role", "user"),
                        content=event.get("content", ""),
                        tool_call_id=event.get("tool_call_id"),
                    )
                )
        return session

    def append(self, session: Session, message: ChatMessage) -> None:
        """Append one message to the transcript (true append mode, NOTE-01).

        B3: switched from read-whole-file + os.replace to open(..., "a") —
        O(1) per append, safe for the heavy observation stream. The
        transcript is a raw log; the episodic store is the derived index.
        """
        d = self._dir(session.id)
        d.mkdir(parents=True, exist_ok=True)
        event = {
            "role": message.role,
            "content": message.content,
            "tool_call_id": message.tool_call_id,
        }
        line = json.dumps(event, ensure_ascii=False) + "\n"
        with open(d / TRANSCRIPT_FILE, "a", encoding="utf-8") as fh:
            fh.write(line)
        session.messages.append(message)
        session.updated = _now()
        self._save_meta(session)  # keep meta.updated in sync for listing
        # Observation stream: mirror the message into the episodic store.
        self.observe_message(session, message)

    # --- observation stream (plan B3) -------------------------------------

    def observe_message(self, session: Session, message: ChatMessage) -> None:
        """Record a chat message as an episodic event."""
        etype = {
            "user": EV_USER,
            "assistant": EV_ASSISTANT,
            "tool": EV_TOOL_RESULT,
        }.get(message.role, EV_SYSTEM)
        self.episodic.append(
            Event(
                type=etype,
                session_id=session.id,
                content=message.content,
                tool_name=message.tool_call_id or "",
            )
        )

    def observe_tool_call(self, session: Session, name: str, args: dict[str, Any]) -> None:
        """Record a tool call (final accumulated arguments, NOTE-03)."""
        self.episodic.append(
            Event(
                type=EV_TOOL_CALL,
                session_id=session.id,
                content=json.dumps(args, ensure_ascii=False, sort_keys=True),
                tool_name=name,
            )
        )

    def observe_approval(self, session: Session, tool_name: str, allowed: bool) -> None:
        """Record an approval decision."""
        self.episodic.append(
            Event(
                type=EV_APPROVAL,
                session_id=session.id,
                content=f"{tool_name}: {'approved' if allowed else 'denied'}",
                tool_name=tool_name,
            )
        )

    def observe_error(self, session: Session, message: str) -> None:
        """Record an error (redacted)."""
        self.episodic.append(Event(type=EV_ERROR, session_id=session.id, content=message))

    def save_meta(self, session: Session) -> None:
        """Persist meta (status, tokens, cost) after a run."""
        self._save_meta(session)

    def _save_meta(self, session: Session) -> None:
        d = self._dir(session.id)
        d.mkdir(parents=True, exist_ok=True)
        tmp = d / f".{META_FILE}.tmp"
        tmp.write_text(
            json.dumps(asdict(session.meta()), ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        os.replace(tmp, d / META_FILE)

    def list(self) -> list[SessionMeta]:
        """List sessions (meta only — never loads transcripts)."""
        if not self.root.is_dir():
            return []
        metas: list[SessionMeta] = []
        for d in sorted(self.root.iterdir()):
            meta_path = d / META_FILE
            if not meta_path.is_file():
                continue
            try:
                metas.append(
                    SessionMeta.from_dict(json.loads(meta_path.read_text(encoding="utf-8")))
                )
            except (json.JSONDecodeError, OSError):
                continue
        return sorted(metas, key=lambda m: m.updated, reverse=True)

    def export_markdown(self, session: Session) -> str:
        """Render a session as redacted markdown."""
        lines = [
            f"# Session {session.id}",
            "",
            f"- created: {session.created}",
            f"- updated: {session.updated}",
            f"- status: {session.status}",
            f"- tokens: {session.tokens}",
            f"- cost: ${session.cost:.4f}",
            "",
            "## Transcript",
            "",
        ]
        for m in session.messages:
            content = redact(m.content)
            if m.role == "system":
                lines.append(f"### system\n\n{content}\n")
            elif m.role == "user":
                lines.append(f"### user\n\n{content}\n")
            elif m.role == "assistant":
                lines.append(f"### assistant\n\n{content}\n")
            elif m.role == "tool":
                lines.append(f"### tool ({m.tool_call_id})\n\n```\n{content}\n```\n")
        return "\n".join(lines)
