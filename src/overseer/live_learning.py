"""Live learning engine: per-turn micro-reflection (plan B4.5).

Speed 0/1: heuristic signal detection after each turn — no model call, no
latency bloat. Corrections and preferences apply to the session immediately.
Speed 2: implicit signals become provisional candidates (low confidence,
vault inbox). Speed 3: explicit "remember this" creates a durable candidate
immediately.

Safety: untrusted content can never create durable memories; live learning
cannot override explicit corrections, bypass approvals, or self-modify
without the L3 guardrail.
"""

from __future__ import annotations

import json
import re
import uuid
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from overseer.redact import redact

# --- event types (plan Part 44) ---------------------------------------------
EV_CORRECTION = "correction"
EV_PREFERENCE = "preference"
EV_FACT = "fact"
EV_CONSTRAINT = "constraint"
EV_TOOL_OUTCOME = "tool_outcome"
EV_RISK = "risk_signal"
EV_UNCERTAINTY = "uncertainty_signal"
EV_REPEATED = "repeated_pattern"
EV_EXPLICIT_MEMORY = "explicit_memory"

EVENT_TYPES = {
    EV_CORRECTION,
    EV_PREFERENCE,
    EV_FACT,
    EV_CONSTRAINT,
    EV_TOOL_OUTCOME,
    EV_RISK,
    EV_UNCERTAINTY,
    EV_REPEATED,
    EV_EXPLICIT_MEMORY,
}

# --- scopes (plan Part 44) ---------------------------------------------------
SCOPE_TURN = "turn"
SCOPE_SESSION = "session"
SCOPE_PROVISIONAL = "provisional"
SCOPE_PROJECT = "project"
SCOPE_GLOBAL = "global"

SCOPES = {SCOPE_TURN, SCOPE_SESSION, SCOPE_PROVISIONAL, SCOPE_PROJECT, SCOPE_GLOBAL}

# --- signal patterns (heuristic, cheap) ---------------------------------------
# Explicit correction: "no, do it this way", "that's wrong", "don't X, Y"
_CORRECTION_RE = re.compile(
    r"\b(no,?|that'?s (not|wrong)|don'?t|stop|instead|actually|rather|never do)\b",
    re.IGNORECASE,
)
# Explicit preference: "I prefer", "I like", "always use", "please use"
_PREFERENCE_RE = re.compile(
    r"\b(i prefer|i like|always use|please use|i want you to|from now on|prefer)\b",
    re.IGNORECASE,
)
# Explicit memory command: "remember this", "remember that", "note this"
_MEMORY_RE = re.compile(r"\b(remember (this|that)|note this|write this down)\b", re.IGNORECASE)
# Constraint: "never", "must not", "always", "only use"
_CONSTRAINT_RE = re.compile(
    r"\b(never|must not|always|only use|do not|under no circumstances)\b", re.IGNORECASE
)
# Risk signal: "careful", "dangerous", "risky", "don't break"
_RISK_RE = re.compile(r"\b(careful|dangerous|risky|don'?t break|be safe)\b", re.IGNORECASE)
# Uncertainty: "i think", "maybe", "not sure", "perhaps"
_UNCERTAINTY_RE = re.compile(r"\b(i think|maybe|not sure|perhaps|i guess)\b", re.IGNORECASE)
# Tool failure: "failed", "error", "traceback", "exit code"
_TOOL_FAILURE_RE = re.compile(r"\b(failed|error|traceback|exit code|exception)\b", re.IGNORECASE)


@dataclass
class LiveEvent:
    """A live learning event (plan Part 44 schema)."""

    type: str
    scope: str
    content: str
    confidence: float = 0.5  # 0..1; implicit signals stay low
    source: str = "detector"  # detector | user | tool
    session_id: str = ""
    ts: str = field(default_factory=lambda: datetime.now(UTC).isoformat())
    metadata: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "type": self.type,
            "scope": self.scope,
            "content": self.content,
            "confidence": self.confidence,
            "source": self.source,
            "session_id": self.session_id,
            "ts": self.ts,
            "metadata": self.metadata,
        }


def detect_signals(text: str) -> list[tuple[str, float]]:
    """Heuristic signal detection. Returns [(event_type, confidence), ...].

    No model call — cheap enough to run after every turn (Speed 0/1).
    """
    signals: list[tuple[str, float]] = []
    if not text:
        return signals
    if _MEMORY_RE.search(text):
        signals.append((EV_EXPLICIT_MEMORY, 0.95))
    if _CORRECTION_RE.search(text):
        signals.append((EV_CORRECTION, 0.8))
    if _PREFERENCE_RE.search(text):
        signals.append((EV_PREFERENCE, 0.7))
    if _CONSTRAINT_RE.search(text):
        signals.append((EV_CONSTRAINT, 0.6))
    if _RISK_RE.search(text):
        signals.append((EV_RISK, 0.5))
    if _UNCERTAINTY_RE.search(text):
        signals.append((EV_UNCERTAINTY, 0.4))
    if _TOOL_FAILURE_RE.search(text):
        signals.append((EV_TOOL_OUTCOME, 0.5))
    return signals


@dataclass
class SessionMemory:
    """Session-scoped working memory: constraints, preferences, rules.

    Corrections apply immediately (Speed 0/1). Undo restores the prior
    state (reversibility requirement).
    """

    constraints: list[str] = field(default_factory=list)
    preferences: list[str] = field(default_factory=list)
    rules: list[str] = field(default_factory=list)
    events: list[LiveEvent] = field(default_factory=list)
    _undo_stack: list[dict[str, Any]] = field(default_factory=list)

    def apply(self, event: LiveEvent) -> None:
        """Apply an event to session memory. Corrections beat preferences."""
        content = redact(event.content)
        if event.type == EV_CORRECTION:
            self._push_undo()
            self.constraints.append(content)
        elif event.type == EV_PREFERENCE:
            self._push_undo()
            self.preferences.append(content)
        elif event.type == EV_CONSTRAINT:
            self._push_undo()
            self.constraints.append(content)
        elif event.type == EV_EXPLICIT_MEMORY:
            self._push_undo()
            self.rules.append(content)
        self.events.append(event)

    def _push_undo(self) -> None:
        self._undo_stack.append(
            {
                "constraints": list(self.constraints),
                "preferences": list(self.preferences),
                "rules": list(self.rules),
            }
        )

    def undo(self) -> bool:
        """Revert the last applied event. Returns True if anything reverted."""
        if not self._undo_stack:
            return False
        state = self._undo_stack.pop()
        self.constraints = state["constraints"]
        self.preferences = state["preferences"]
        self.rules = state["rules"]
        return True

    def context_block(self) -> str:
        """Active constraints/preferences injected into the next context build."""
        parts: list[str] = []
        if self.constraints:
            parts.append("Active constraints: " + "; ".join(self.constraints))
        if self.preferences:
            parts.append("Session preferences: " + "; ".join(self.preferences))
        if self.rules:
            parts.append("Session rules: " + "; ".join(self.rules))
        return "\n".join(parts)

    def summary(self) -> str:
        """One-line telemetry for `overseer live-learn`."""
        return (
            f"{len(self.constraints)} constraints, {len(self.preferences)} preferences, "
            f"{len(self.rules)} rules, {len(self.events)} events"
        )


class ProvisionalStore:
    """Speed 2/3 candidates: durable memory candidates in the vault inbox.

    Explicit "remember this" -> confidence 0.95 (Speed 3).
    Implicit signals -> confidence 0.3-0.5 (Speed 2), never promoted without
    repetition or verification.
    """

    def __init__(self, vault_root: str | Path) -> None:
        self.inbox = Path(vault_root).expanduser().resolve() / "00-Inbox"
        self.inbox.mkdir(parents=True, exist_ok=True)

    def create(self, event: LiveEvent) -> Path:
        """Write a provisional candidate note. Returns the note path."""
        candidate_id = f"OVR-CAND-{uuid.uuid4().hex[:8]}"
        stamp = datetime.now(UTC).isoformat(timespec="seconds")
        body = (
            f"---\n"
            f"id: {candidate_id}\n"
            f"type: candidate\n"
            f"title: {redact(event.content)[:80]}\n"
            f"created: {stamp}\n"
            f"modified: {stamp}\n"
            f"status: provisional\n"
            f"confidence: {event.confidence}\n"
            f"event_type: {event.type}\n"
            f"scope: {event.scope}\n"
            f"---\n\n"
            f"{redact(event.content)}\n"
        )
        path = self.inbox / f"{candidate_id}.md"
        path.write_text(body, encoding="utf-8")
        return path


class LiveLearningEngine:
    """Per-turn micro-reflection orchestrator (plan B4.5).

    detect_and_apply(text, session_id, untrusted=False) runs after each
    turn: detects signals, applies session-scoped updates immediately,
    creates provisional candidates for durable-worthy events, and enforces
    the token budget.
    """

    def __init__(
        self,
        vault_root: str | Path,
        enabled: bool = True,
        max_events_per_turn: int = 3,
        max_events_per_session: int = 50,
    ) -> None:
        self.enabled = enabled
        self.max_events_per_turn = max_events_per_turn
        self.max_events_per_session = max_events_per_session
        self.memory = SessionMemory()
        self.provisional = ProvisionalStore(vault_root)
        self.session_event_count = 0
        self.turn_event_count = 0

    def detect_and_apply(
        self,
        text: str,
        session_id: str = "",
        untrusted: bool = False,
    ) -> list[LiveEvent]:
        """Run the micro-reflection pass. Returns the events created."""
        if not self.enabled:
            return []
        self.turn_event_count = 0
        signals = detect_signals(text)
        events: list[LiveEvent] = []
        for event_type, confidence in signals:
            if self.turn_event_count >= self.max_events_per_turn:
                break
            if self.session_event_count >= self.max_events_per_session:
                break
            scope = SCOPE_SESSION
            if event_type == EV_EXPLICIT_MEMORY:
                scope = SCOPE_PROVISIONAL
                confidence = 0.95  # Speed 3: explicit command
            elif confidence < 0.6:
                scope = SCOPE_PROVISIONAL  # Speed 2: implicit, low confidence
            event = LiveEvent(
                type=event_type,
                scope=scope,
                content=text,
                confidence=confidence,
                source="user" if not untrusted else "untrusted",
                session_id=session_id,
            )
            if untrusted and scope == SCOPE_PROVISIONAL:
                # Untrusted content can never create durable memories.
                continue
            if scope == SCOPE_SESSION:
                self.memory.apply(event)
            else:
                self.provisional.create(event)
            self.session_event_count += 1
            self.turn_event_count += 1
            events.append(event)
        return events

    def undo(self) -> bool:
        """Revert the last session-memory update (reversibility)."""
        return self.memory.undo()

    def context_block(self) -> str:
        """Active session constraints/preferences for the next context build."""
        return self.memory.context_block()

    def summary(self) -> str:
        return self.memory.summary()

    def to_json(self) -> str:
        return json.dumps(
            {
                "enabled": self.enabled,
                "session_events": self.session_event_count,
                "memory": self.memory.summary(),
                "events": [e.to_dict() for e in self.memory.events],
            },
            indent=2,
        )
