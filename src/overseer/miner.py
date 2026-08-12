"""Pattern miner: learn reusable skills from repeated verified successes (B7).

Consumes episode data (from the episodic store / session store), chunks them
into episodes, extracts features, clusters similar episodes, applies strict
evidence gates, and drafts skill notes for the curator.

Safety rules (B7 spec):
- Never mine from unverified outcomes: an episode is only a *verified success*
  when its session status is ``done`` AND it contains no ``error`` events.
- Minimum evidence: >= ``MIN_EVIDENCE_SESSIONS`` independent successful
  sessions before a pattern is eligible.
- Success threshold: >= 70% for low risk, >= 90% for high risk.
- Correction replay: a draft that contradicts a high-confidence correction is
  rejected or flagged as a conflict, never written.
- Mining is deterministic and testable (no ML / weight training — that is B8).

The miner does NOT write directly to the vault. It produces :class:`SkillDraft`
objects for the curator, which is the only component allowed to persist skill
notes. This keeps the approval gate unbypassable.
"""

from __future__ import annotations

import re
from collections.abc import Iterable
from dataclasses import dataclass, field
from typing import Any

from overseer.curator import (
    MIN_EVIDENCE_SESSIONS,
    SUCCESS_THRESHOLD_HIGH,
    SUCCESS_THRESHOLD_LOW,
    classify_risk,
)
from overseer.episodic import (
    EV_ERROR,
    EV_TOOL_CALL,
    EV_TOOL_RESULT,
    EV_USER,
)
from overseer.live_learning import EV_CORRECTION, LiveEvent

# Event types that indicate a tool was actually invoked (not user text).
_TOOL_EVENT_TYPES = {EV_TOOL_CALL, EV_TOOL_RESULT}

# Keywords used to infer a "task type" from user messages. Kept small and
# deterministic so clustering is stable and testable.
_TASK_KEYWORDS: tuple[tuple[str, str], ...] = (
    ("test", "testing"),
    ("fix", "debugging"),
    ("feature", "feature"),
    ("refactor", "refactoring"),
    ("deploy", "deployment"),
    ("build", "build"),
    ("document", "documentation"),
    ("migrate", "migration"),
    ("review", "code-review"),
    ("setup", "setup"),
)

# Keywords that flag a high-confidence user correction for replay.
_CORRECTION_MARKERS = re.compile(
    r"\b(no,?|don'?t|do not|shouldn'?t|should not|never|stop|instead|"
    r"use .* not|avoid|that'?s wrong|is wrong)\b",
    re.IGNORECASE,
)


@dataclass
class Episode:
    """One session's observation stream distilled for mining."""

    session_id: str
    status: str  # done | error | active
    tool_names: list[str] = field(default_factory=list)
    task_types: list[str] = field(default_factory=list)
    has_error: bool = False
    corrections: list[str] = field(default_factory=list)  # correction text


@dataclass
class SkillDraft:
    """A pattern that passed the evidence gates, ready for curation.

    ``conflict`` is set when correction replay flagged a contradiction; such
    drafts must NOT be written unless the conflict is resolved by a human.
    """

    title: str
    trigger: str
    steps: str
    risk: str
    tool_names: list[str]
    evidence_sessions: list[str]
    success_rate: float
    confidence: float
    conflict: bool = False
    conflict_reason: str = ""


def _task_type_of(content: str) -> str | None:
    """Return the first matching task type for a user message, else None."""
    low = content.lower()
    for keyword, label in _TASK_KEYWORDS:
        if keyword in low:
            return label
    return None


def extract_episode(
    session_id: str,
    status: str,
    events: Iterable[dict[str, Any]],
    live_events: Iterable[LiveEvent] | None = None,
) -> Episode:
    """Distill a session's raw events into an :class:`Episode`.

    ``events`` is a sequence of episodic event dicts (the shape returned by
    ``EpisodicStore.by_session``: keys ``type``, ``tool_name``, ``content``).
    ``live_events`` optionally carries the session's live-learning correction
    events.
    """
    tools: set[str] = set()
    task_types: list[str] = []
    has_error = False
    corrections: list[str] = []
    for ev in events:
        etype = ev.get("type", "")
        if etype == EV_ERROR:
            has_error = True
        if etype in _TOOL_EVENT_TYPES:
            name = ev.get("tool_name", "") or ev.get("name", "")
            if name:
                tools.add(name)
        if etype == EV_USER:
            tt = _task_type_of(ev.get("content", ""))
            if tt and tt not in task_types:
                task_types.append(tt)
    if live_events:
        for le in live_events:
            if le.type == EV_CORRECTION and le.confidence >= 0.7:
                text = getattr(le, "content", "") or ""
                if text:
                    corrections.append(text)
    return Episode(
        session_id=session_id,
        status=status,
        tool_names=sorted(tools),
        task_types=task_types,
        has_error=has_error,
        corrections=corrections,
    )


def _pattern_key(ep: Episode) -> tuple[str, ...]:
    """Deterministic cluster key: shared tool set + task types.

    Episodes that use the same tools for the same task type cluster together.
    An empty tool set clusters to ``("<no-tools>",)`` and is weak evidence.
    """
    parts: list[str] = list(ep.tool_names)
    parts.extend(ep.task_types)
    if not parts:
        return ("<no-tools>",)
    return tuple(sorted(set(parts)))


def _is_verified_success(ep: Episode) -> bool:
    """An episode counts as a verified success only when done and error-free."""
    return ep.status == "done" and not ep.has_error


class PatternMiner:
    """Clusters episodes, applies evidence gates, and drafts skills."""

    def __init__(self, min_evidence: int = MIN_EVIDENCE_SESSIONS) -> None:
        self.min_evidence = min_evidence

    def mine(self, episodes: Iterable[Episode]) -> list[SkillDraft]:
        """Return skill drafts for patterns passing the evidence gates.

        Drafts that contradict a high-confidence correction are returned with
        ``conflict=True`` and must be rejected by the caller.
        """
        episodes = list(episodes)
        clusters = self._cluster(episodes)
        drafts: list[SkillDraft] = []
        for key, group in clusters.items():
            draft = self._evaluate_group(key, group)
            if draft is not None:
                drafts.append(draft)
        return drafts

    def replay_corrections(
        self, drafts: list[SkillDraft], corrections: list[str]
    ) -> list[SkillDraft]:
        """Replay Correction Memory against drafted skills.

        Any draft that contradicts a high-confidence correction is marked as a
        conflict (``conflict=True``) so the caller rejects it. ``corrections``
        is a list of persisted correction-note contents from the vault.
        """
        out: list[SkillDraft] = []
        for draft in drafts:
            conflict, reason = _check_correction_conflict(draft, corrections)
            draft.conflict = conflict
            draft.conflict_reason = reason
            out.append(draft)
        return out

    def _cluster(self, episodes: list[Episode]) -> dict[tuple[str, ...], list[Episode]]:
        clusters: dict[tuple[str, ...], list[Episode]] = {}
        for ep in episodes:
            clusters.setdefault(_pattern_key(ep), []).append(ep)
        return clusters

    def _evaluate_group(self, key: tuple[str, ...], group: list[Episode]) -> SkillDraft | None:
        successes = [ep for ep in group if _is_verified_success(ep)]
        independent = {ep.session_id for ep in group if _is_verified_success(ep)}

        # Evidence floor: at least N independent verified successes.
        if len(independent) < self.min_evidence:
            return None

        # Risk + threshold for this pattern.
        tools = [t for t in key if t not in _TASK_LABELS]
        risk = classify_risk(tools)
        threshold = SUCCESS_THRESHOLD_HIGH if risk == "high" else SUCCESS_THRESHOLD_LOW
        total = len(group)
        rate = len(successes) / total if total else 0.0
        if rate < threshold:
            return None

        # Gather corrections across the group to replay the guard.
        corrections: list[str] = []
        for ep in group:
            corrections.extend(ep.corrections)

        draft = self._draft(key, group, successes, rate, risk)
        if corrections:
            draft.conflict, draft.conflict_reason = _check_correction_conflict(draft, corrections)
        return draft

    def _draft(
        self,
        key: tuple[str, ...],
        group: list[Episode],
        successes: list[Episode],
        rate: float,
        risk: str,
    ) -> SkillDraft:
        tools = [t for t in key if t not in _TASK_LABELS]
        tasks = [t for t in key if t in _TASK_LABELS]
        evidence = sorted({ep.session_id for ep in successes})
        title = _make_title(tasks, tools)
        trigger = _make_trigger(tasks, tools)
        steps = _make_steps(successes)
        confidence = min(rate, 0.95)
        return SkillDraft(
            title=title,
            trigger=trigger,
            steps=steps,
            risk=risk,
            tool_names=sorted(tools),
            evidence_sessions=evidence,
            success_rate=rate,
            confidence=confidence,
        )


_TASK_LABELS = {label for _, label in _TASK_KEYWORDS}


def _make_title(tasks: list[str], tools: list[str]) -> str:
    if tasks:
        base = tasks[0]
        if tools:
            return f"{base} with {' and '.join(tools[:2])}"
        return base
    return "recurring workflow"


def _make_trigger(tasks: list[str], tools: list[str]) -> str:
    triggers: list[str] = []
    if tasks:
        triggers.append(f"task involves {tasks[0]}")
    if tools:
        triggers.append(f"tools: {' '.join(tools)}")
    return "; ".join(triggers) or "general"


def _make_steps(successes: list[Episode]) -> str:
    """Compose canonical guidance from the most common tool sequence.

    Builds a deterministic step list from the sorted tool set of the successes,
    with the evidence sessions attached.
    """
    tools: set[str] = set()
    for ep in successes:
        tools.update(ep.tool_names)
    lines = [
        "This pattern was mined from repeated verified successes across "
        f"{len({ep.session_id for ep in successes})} independent sessions.",
        "",
        "Steps:",
    ]
    for i, tool in enumerate(sorted(tools), 1):
        lines.append(f"{i}. Use {tool}")
    lines.append("")
    lines.append("Evidence: " + ", ".join(sorted({ep.session_id for ep in successes})))
    return "\n".join(lines)


def _check_correction_conflict(draft: SkillDraft, corrections: list[str]) -> tuple[bool, str]:
    """Replay high-confidence corrections against a draft.

    Returns ``(conflict, reason)``. A draft conflicts when a correction
    mentions a tool/topic the draft also promotes, and the correction is
    framed negatively (do NOT use X / avoid X).
    """
    for correction in corrections:
        low = correction.lower()
        if not _CORRECTION_MARKERS.search(low):
            continue
        for tool in draft.tool_names:
            if tool.lower() in low:
                return True, f"contradicts correction: {correction[:120]}"
    return False, ""
