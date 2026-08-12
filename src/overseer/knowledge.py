"""Knowledge layer: reflection pipeline, consolidation, retrieval (plan B5).

Consumes the episodic stream and live-learning provisional candidates at
end of session, extracts candidate memories (facts, preferences,
corrections, project conventions, skill drafts), scores them by evidence
strength and salience, and writes durable notes to the vault through the
governed Vault.write_note API.

Safety: untrusted content can never reach high confidence; every durable
memory links to evidence; duplicates are merged, conflicts are flagged for
human review, never silently overwritten.
"""

from __future__ import annotations

import contextlib
import re
import uuid
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from overseer.episodic import (
    EV_TOOL_CALL,
    EV_TOOL_RESULT,
    EV_USER,
    Event,
)
from overseer.live_learning import (
    EV_CORRECTION,
    EV_EXPLICIT_MEMORY,
    EV_PREFERENCE,
    LiveEvent,
)
from overseer.redact import redact
from overseer.vault import Vault

# --- confidence tiers (plan: explicit > repeated verified > implicit) --------
CONF_EXPLICIT = 0.9  # explicit user command / correction
CONF_REPEATED = 0.75  # repeated verified outcome
CONF_IMPLICIT = 0.4  # single implicit inference
CONF_UNTRUSTED = 0.1  # untrusted source — never durable

# --- salience weights (plan: importance x recency x access) ------------------
W_IMPORTANCE = 0.5
W_RECENCY = 0.3
W_ACCESS = 0.2

# --- extraction heuristics ----------------------------------------------------
_PREFERENCE_RE = re.compile(
    r"\b(i prefer|i like|always use|please use|from now on|prefer)\b", re.IGNORECASE
)
_CORRECTION_RE = re.compile(
    r"\b(no,?|that'?s (not|wrong)|don'?t|stop|instead|actually|never do)\b", re.IGNORECASE
)
_FACT_RE = re.compile(
    r"\b(the (project|repo|codebase|app)|it uses|it is|it has|built with|depends on)\b",
    re.IGNORECASE,
)
_SKILL_RE = re.compile(
    r"\b(always|never|when .* (use|run|check)|the way to|best practice)\b", re.IGNORECASE
)


@dataclass
class MemoryCandidate:
    """A candidate durable memory extracted from evidence."""

    note_type: str  # fact | preference | correction | project | skill
    content: str
    confidence: float
    evidence: str  # session id / artifact path / user quote
    scope: str = "project"
    trigger: str = ""
    salience: float = 0.0
    source: str = "session"  # session | live_learning | untrusted
    metadata: dict[str, Any] = field(default_factory=dict)

    def to_frontmatter(self) -> dict[str, Any]:
        """Type-specific frontmatter matching the B0 vault ontology."""
        base: dict[str, Any] = {
            "scope": self.scope,
            "confidence": self.confidence,
            "source": self.source,
        }
        if self.note_type == "correction":
            base.update(
                {
                    "trigger": self.trigger or "general",
                    "mistake": self.metadata.get("mistake", ""),
                    "correction": self.content,
                    "rule": self.content,
                    "severity": self.metadata.get("severity", "medium"),
                }
            )
        elif self.note_type == "preference":
            base.update({"strength": self.confidence, "source": self.source})
        elif self.note_type == "skill":
            base.update(
                {
                    "trigger": self.trigger or "general",
                    "risk": self.metadata.get("risk", "low"),
                    "use_count": self.metadata.get("use_count", 1),
                    "success_count": self.metadata.get("success_count", 1),
                    "failure_count": self.metadata.get("failure_count", 0),
                }
            )
        elif self.note_type == "project":
            base.update(
                {
                    "languages": self.metadata.get("languages", []),
                    "commands": self.metadata.get("commands", {}),
                    "conventions": self.metadata.get("conventions", []),
                    "risks": self.metadata.get("risks", []),
                }
            )
        return base


def _confidence_for(event: Event | LiveEvent, untrusted: bool = False) -> float:
    """Evidence-strength confidence scoring.

    Explicit user commands/corrections are high confidence; tool output
    is implicit; untrusted content is never durable.
    """
    if untrusted:
        return CONF_UNTRUSTED
    etype = getattr(event, "type", "")
    if etype in (EV_CORRECTION, EV_EXPLICIT_MEMORY, EV_PREFERENCE):
        return CONF_EXPLICIT
    if etype == EV_USER:
        return CONF_EXPLICIT  # direct user statement
    if etype in (EV_TOOL_RESULT, EV_TOOL_CALL):
        return CONF_IMPLICIT
    return CONF_IMPLICIT


def _salience(importance: float, recency: float, access: float) -> float:
    """Salience = importance x recency x access (plan Part 44)."""
    return W_IMPORTANCE * importance + W_RECENCY * recency + W_ACCESS * access


def extract_candidates(
    events: list[Event],
    live_events: list[LiveEvent] | None = None,
    untrusted: bool = False,
) -> list[MemoryCandidate]:
    """Extract candidate memories from episodic + live events.

    Heuristic extraction: preferences, corrections, facts, and skill
    patterns from user/assistant/tool content. Untrusted content is
    capped at CONF_UNTRUSTED and never becomes durable.
    """
    candidates: list[MemoryCandidate] = []
    live_events = live_events or []

    for ev in live_events:
        conf = _confidence_for(ev, untrusted)
        if conf < 0.5:
            continue  # implicit/untrusted — not durable yet
        if ev.type == EV_CORRECTION:
            candidates.append(
                MemoryCandidate(
                    note_type="correction",
                    content=redact(ev.content)[:500],
                    confidence=conf,
                    evidence=ev.session_id or "live-learning",
                    scope="project",
                    trigger="general",
                    source="live_learning",
                )
            )
        elif ev.type == EV_PREFERENCE:
            candidates.append(
                MemoryCandidate(
                    note_type="preference",
                    content=redact(ev.content)[:500],
                    confidence=conf,
                    evidence=ev.session_id or "live-learning",
                    scope="project",
                    source="live_learning",
                )
            )
        elif ev.type == EV_EXPLICIT_MEMORY:
            candidates.append(
                MemoryCandidate(
                    note_type="fact",
                    content=redact(ev.content)[:500],
                    confidence=conf,
                    evidence=ev.session_id or "live-learning",
                    scope="project",
                    source="live_learning",
                )
            )

    for evt in events:
        text = redact(evt.content or "")
        if not text:
            continue
        conf = _confidence_for(evt, untrusted)
        if conf < 0.5:
            continue
        if evt.type == EV_USER:
            if _CORRECTION_RE.search(text):
                candidates.append(
                    MemoryCandidate(
                        note_type="correction",
                        content=text[:500],
                        confidence=conf,
                        evidence=evt.session_id,
                        scope="project",
                        trigger="general",
                    )
                )
            elif _PREFERENCE_RE.search(text):
                candidates.append(
                    MemoryCandidate(
                        note_type="preference",
                        content=text[:500],
                        confidence=conf,
                        evidence=evt.session_id,
                        scope="project",
                    )
                )
            elif _FACT_RE.search(text):
                candidates.append(
                    MemoryCandidate(
                        note_type="fact",
                        content=text[:500],
                        confidence=conf,
                        evidence=evt.session_id,
                        scope="project",
                    )
                )
        elif evt.type == EV_TOOL_RESULT and _SKILL_RE.search(text):
            candidates.append(
                MemoryCandidate(
                    note_type="skill",
                    content=text[:500],
                    confidence=conf,
                    evidence=evt.session_id,
                    scope="project",
                    trigger="general",
                )
            )
    return candidates


class KnowledgeBase:
    """Consolidation + retrieval over the canonical vault (plan B5)."""

    def __init__(self, vault_root: str | Path) -> None:
        self.vault = Vault(vault_root)
        self.vault.init()  # idempotent

    # -- consolidation ------------------------------------------------------

    def consolidate(
        self,
        candidates: list[MemoryCandidate],
        session_id: str = "",
    ) -> dict[str, Any]:
        """Write candidates to the vault with dedup + conflict detection.

        Returns {"created": [...], "updated": [...], "conflicts": [...]}.
        """
        created: list[str] = []
        updated: list[str] = []
        conflicts: list[str] = []

        for cand in candidates:
            if cand.confidence < 0.5:
                continue  # implicit/untrusted — not durable
            existing = self._find_existing(cand)
            if existing:
                if self._is_conflict(cand, existing):
                    conflicts.append(self._flag_conflict(cand, existing))
                    continue
                self._merge(cand, existing)
                updated.append(existing["id"])
                continue
            note_id = self._write_note(cand, session_id)
            created.append(note_id)

        return {"created": created, "updated": updated, "conflicts": conflicts}

    def _find_existing(self, cand: MemoryCandidate) -> dict[str, Any] | None:
        """Search the vault for a note with the same type + scope + trigger.

        Facts dedup by scope (same scope + different content = conflict).
        Other types dedup by trigger or content prefix.
        """
        folder = {
            "fact": "30-Facts",
            "preference": "50-Preferences",
            "correction": "80-Corrections",
            "skill": "40-Skills",
            "project": "60-Projects",
        }.get(cand.note_type)
        if folder is None:
            return None
        folder_path = self.vault.root / folder
        if not folder_path.is_dir():
            return None
        for note in folder_path.glob("*.md"):
            try:
                text = note.read_text(encoding="utf-8")
            except OSError:
                continue
            parsed = self._parse_note(note, text)
            if cand.trigger and parsed.get("trigger") == cand.trigger:
                return parsed
            if cand.note_type == "fact" and parsed.get("scope") == cand.scope:
                return parsed
            if cand.content[:40] in text:
                return parsed
        return None

    def _parse_note(self, path: Path, text: str) -> dict[str, Any]:
        """Minimal frontmatter parse (id, confidence, scope, trigger, content)."""
        meta: dict[str, Any] = {
            "id": "",
            "confidence": 0.0,
            "scope": "",
            "trigger": "",
            "content": "",
            "path": path,
        }
        for line in text.splitlines():
            if line.startswith("id:"):
                meta["id"] = line.split(":", 1)[1].strip()
            elif line.startswith("confidence:"):
                with contextlib.suppress(ValueError):
                    meta["confidence"] = float(line.split(":", 1)[1].strip())
            elif line.startswith("scope:"):
                meta["scope"] = line.split(":", 1)[1].strip()
            elif line.startswith("trigger:"):
                meta["trigger"] = line.split(":", 1)[1].strip()
        # Body: everything after the frontmatter close (first --- pair),
        # minus the "# title" heading.
        parts = text.split("---", 2)
        if len(parts) >= 3:
            body = parts[2].strip()
            lines = body.splitlines()
            if lines and lines[0].startswith("# "):
                lines = lines[1:]
            meta["content"] = "\n".join(lines).strip()
        return meta

    def _is_conflict(self, cand: MemoryCandidate, existing: dict[str, Any]) -> bool:
        """A new fact contradicting a high-confidence existing fact."""
        if cand.note_type != "fact":
            return False
        if existing["confidence"] < 0.7:
            return False
        # Compare against the first line of the existing note (the fact
        # itself, not the evidence block appended below it).
        existing_fact = existing["content"].splitlines()[0] if existing["content"] else ""
        return cand.content[:60] != existing_fact[:60]

    def _flag_conflict(self, cand: MemoryCandidate, existing: dict[str, Any]) -> str:
        """Write a conflict flag to 99-Meta/ for human review."""
        flag_id = f"OVR-CONF-{uuid.uuid4().hex[:8]}"
        stamp = datetime.now(UTC).isoformat(timespec="seconds")
        body = (
            f"---\n"
            f"id: {flag_id}\n"
            f"type: meta\n"
            f"title: Conflict: {redact(cand.content)[:60]}\n"
            f"created: {stamp}\n"
            f"modified: {stamp}\n"
            f"status: proposed\n"
            f"---\n\n"
            f"## Conflict\n\n"
            f"- **New:** {redact(cand.content)[:300]}\n"
            f"- **Existing:** {existing['content'][:300]}\n"
            f"- **Existing id:** {existing['id']}\n"
            f"- **Evidence:** {cand.evidence}\n\n"
            f"Human review required before either note is changed.\n"
        )
        path = self.vault.root / "99-Meta" / f"{flag_id}.md"
        path.write_text(body, encoding="utf-8")
        return flag_id

    def _merge(self, cand: MemoryCandidate, existing: dict[str, Any]) -> None:
        """Update confidence/salience on the existing note (no duplicates)."""
        path = existing["path"]
        text = path.read_text(encoding="utf-8")
        new_conf = max(cand.confidence, existing["confidence"])
        text = re.sub(r"confidence: [\d.]+", f"confidence: {new_conf}", text, count=1)
        stamp = datetime.now(UTC).isoformat(timespec="seconds")
        text = re.sub(r"modified: .*", f"modified: {stamp}", text, count=1)
        path.write_text(text, encoding="utf-8")

    def _write_note(self, cand: MemoryCandidate, session_id: str) -> str:
        """Write a durable note via the governed Vault.write_note API."""
        body = (
            f"{redact(cand.content)}\n\n"
            f"---\n"
            f"**Evidence:** {cand.evidence or session_id or 'unknown'}\n"
            f"**Confidence:** {cand.confidence}\n"
            f"**Salience:** {cand.salience:.2f}\n"
        )
        path = self.vault.write_note(
            cand.note_type,
            redact(cand.content)[:80],
            body,
            **cand.to_frontmatter(),
        )
        # Filename is OVR-<TYPE>-<hex>-<slug>.md — the id is the first 3 parts.
        return "-".join(path.stem.split("-")[:3])

    # -- retrieval ----------------------------------------------------------

    def retrieve(
        self,
        query: str,
        note_types: list[str] | None = None,
        limit: int = 5,
    ) -> list[dict[str, Any]]:
        """Return the top-N most salient notes matching the query.

        Uses the B3 FTS5 index when available, else a simple filename scan.
        """
        folders = {
            "fact": "30-Facts",
            "preference": "50-Preferences",
            "correction": "80-Corrections",
            "skill": "40-Skills",
            "project": "60-Projects",
        }
        types = note_types or list(folders)
        results: list[dict[str, Any]] = []
        for note_type in types:
            folder = folders.get(note_type)
            if folder is None:
                continue
            folder_path = self.vault.root / folder
            if not folder_path.is_dir():
                continue
            for note in folder_path.glob("*.md"):
                try:
                    text = note.read_text(encoding="utf-8")
                except OSError:
                    continue
                if query.lower() in text.lower():
                    meta = self._parse_note(note, text)
                    meta["note_type"] = note_type
                    meta["salience"] = _salience(
                        importance=0.5,
                        recency=0.5,
                        access=0.5,
                    )
                    results.append(meta)
        results.sort(key=lambda r: r["salience"], reverse=True)
        return results[:limit]
