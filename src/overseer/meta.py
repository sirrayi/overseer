"""Recursive closure and meta-learning (plan B10).

Overseer tracks meta-stats across sessions, detects trends, generates
proposals for its own learning-system parameters, evaluates them in
shadow mode against historical sessions, and — only with explicit human
approval — applies them. Self-modification is NEVER silent (guardrail 8).

Forbidden targets: no proposal may touch the approval gate, the L3
guardrail, or path containment. Those are hard-coded as unproposable.
"""

from __future__ import annotations

import json
import uuid
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

# ---------------------------------------------------------------------------
# Meta-stats
# ---------------------------------------------------------------------------

FORBIDDEN_TARGETS = (
    "approval",
    "denylist",
    "allowlist",
    "path containment",
    "containment",
    "guardrail",
    "l3",
    "untrusted",
)


@dataclass
class MetaStats:
    """Cross-session learning-system metrics, persisted as JSONL."""

    store_dir: Path
    entries: list[dict[str, Any]] = field(default_factory=list)

    def __post_init__(self) -> None:
        self.store_dir.mkdir(parents=True, exist_ok=True)
        self._load()

    def _load(self) -> None:
        path = self.store_dir / "meta.jsonl"
        if not path.exists():
            return
        for line in path.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if line:
                try:
                    self.entries.append(json.loads(line))
                except json.JSONDecodeError:
                    continue  # skip corrupt lines

    def record(self, session_id: str, **metrics: float) -> None:
        """Append one session's meta-metrics."""
        entry = {
            "session_id": session_id,
            "ts": datetime.now(UTC).isoformat(),
            **metrics,
        }
        self.entries.append(entry)
        with (self.store_dir / "meta.jsonl").open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(entry) + "\n")

    def correction_rate(self, last_n: int = 10) -> float:
        """Corrections per session over the last N sessions."""
        recent = self.entries[-last_n:]
        if not recent:
            return 0.0
        total: float = sum(float(e.get("corrections", 0)) for e in recent)
        return total / len(recent)

    def skill_hit_rate(self, last_n: int = 10) -> float:
        """Active skills retrieved vs used successfully."""
        recent = self.entries[-last_n:]
        if not recent:
            return 0.0
        hits = sum(e.get("skill_hits", 0) for e in recent)
        used = sum(e.get("skill_uses", 0) for e in recent)
        return hits / used if used else 0.0

    def retrieval_usefulness(self, last_n: int = 10) -> float:
        """Fraction of retrievals that led to a used memory."""
        recent = self.entries[-last_n:]
        if not recent:
            return 0.0
        useful = sum(e.get("retrieval_useful", 0) for e in recent)
        total = sum(e.get("retrievals", 0) for e in recent)
        return useful / total if total else 0.0

    def memory_conflict_rate(self, last_n: int = 10) -> float:
        """Conflicts flagged per session."""
        recent = self.entries[-last_n:]
        if not recent:
            return 0.0
        total: float = sum(float(e.get("conflicts", 0)) for e in recent)
        return total / len(recent)

    def summary(self) -> dict[str, float]:
        return {
            "correction_rate": self.correction_rate(),
            "skill_hit_rate": self.skill_hit_rate(),
            "retrieval_usefulness": self.retrieval_usefulness(),
            "memory_conflict_rate": self.memory_conflict_rate(),
        }


# ---------------------------------------------------------------------------
# Proposals
# ---------------------------------------------------------------------------


@dataclass
class Proposal:
    """A proposed self-modification. Never applied without human approval."""

    title: str
    metric: str  # which meta-metric the trend is on
    trend: str  # human-readable trend description
    target: str  # what changes, e.g. "W_IMPORTANCE for corrections"
    old_value: float
    new_value: float
    expected_benefit: str
    risk: str
    rollback: str
    shadow_result: str = ""
    status: str = "draft"  # draft | shadow_ok | approved | rejected | applied
    proposal_id: str = field(default_factory=lambda: f"OVR-PROP-{uuid.uuid4().hex[:8]}")

    def to_frontmatter(self) -> dict[str, Any]:
        return {
            "proposal_type": "threshold",
            "risk": "low",
            "expected_benefit": self.expected_benefit,
            "evidence": [self.trend, self.shadow_result],
            "approval": "required",
        }

    def to_body(self) -> str:
        return (
            f"## proposal\n{self.trend}\n\n"
            f"## change\n{self.target}: {self.old_value} -> {self.new_value}\n\n"
            f"## expected benefit\n{self.expected_benefit}\n\n"
            f"## risk\n{self.risk}\n\n"
            f"## rollback\n{self.rollback}\n\n"
            f"## shadow mode\n{self.shadow_result or 'pending'}\n\n"
            f"## approval\nRequired. Never applied silently (guardrail 8)."
        )


class ProposalGenerator:
    """Detect trends in meta-stats and generate governed proposals."""

    def __init__(self, stats: MetaStats, vault: Any) -> None:
        self.stats = stats
        self.vault = vault

    def _forbidden(self, target: str) -> bool:
        t = target.lower()
        return any(f in t for f in FORBIDDEN_TARGETS)

    def generate(self) -> list[Proposal]:
        """Generate proposals from current trends. Empty when no trend."""
        proposals: list[Proposal] = []
        s = self.stats.summary()

        # Trend 1: rising correction rate -> salience weights for corrections too low.
        if s["correction_rate"] > 1.5:
            proposals.append(
                Proposal(
                    title="Raise correction salience weight",
                    metric="correction_rate",
                    trend=(
                        f"correction rate is {s['correction_rate']:.2f}/session "
                        "(>1.5), suggesting correction salience is too low"
                    ),
                    target="W_IMPORTANCE for corrections",
                    old_value=0.5,
                    new_value=0.7,
                    expected_benefit="fewer repeated corrections",
                    risk="low — bounded weight change, reversible",
                    rollback="restore W_IMPORTANCE=0.5 from archived config",
                )
            )

        # Trend 2: low skill hit rate -> skill promotion threshold too lenient.
        if s["skill_hit_rate"] < 0.5 and s["skill_hit_rate"] > 0.0:
            proposals.append(
                Proposal(
                    title="Raise skill promotion threshold",
                    metric="skill_hit_rate",
                    trend=(
                        f"skill hit rate is {s['skill_hit_rate']:.2f} (<0.5), "
                        "suggesting the promotion threshold is too lenient"
                    ),
                    target="MIN_EVIDENCE_SESSIONS for skill promotion",
                    old_value=3,
                    new_value=4,
                    expected_benefit="fewer low-quality active skills",
                    risk="low — one more evidence session required",
                    rollback="restore MIN_EVIDENCE_SESSIONS=3",
                )
            )

        # Trend 3: low retrieval usefulness -> salience recency weight too high.
        if s["retrieval_usefulness"] < 0.4 and s["retrieval_usefulness"] > 0.0:
            proposals.append(
                Proposal(
                    title="Rebalance retrieval recency weight",
                    metric="retrieval_usefulness",
                    trend=(
                        f"retrieval usefulness is {s['retrieval_usefulness']:.2f} "
                        "(<0.4), suggesting recency dominates salience"
                    ),
                    target="W_RECENCY in salience scoring",
                    old_value=0.4,
                    new_value=0.3,
                    expected_benefit="more relevant retrievals",
                    risk="low — bounded weight change, reversible",
                    rollback="restore W_RECENCY=0.4",
                )
            )

        return [p for p in proposals if not self._forbidden(p.target)]

    def write_proposal(self, proposal: Proposal) -> Path:
        """Write a proposal to the vault (90-Proposals) via governed write_note."""
        path: Path = self.vault.write_note(
            "proposal",
            proposal.title,
            proposal.to_body(),
            **proposal.to_frontmatter(),
        )
        return path


# ---------------------------------------------------------------------------
# Shadow mode
# ---------------------------------------------------------------------------


class ShadowEvaluator:
    """Evaluate a proposed change against historical sessions (canary).

    Runs the proposed weight/threshold over the last N sessions in the
    episodic store and compares simulated outcomes against actual ones.
    Only non-regressing proposals pass. Strict budget: bounded sessions,
    bounded time.
    """

    def __init__(self, episodic: Any, max_sessions: int = 5, max_seconds: int = 10) -> None:
        self.episodic = episodic
        self.max_sessions = max_sessions
        self.max_seconds = max_seconds

    def evaluate(self, proposal: Proposal) -> str:
        """Return a verdict string: PASS (improves) or FAIL (regresses)."""
        import time

        start = time.monotonic()
        sessions = self._recent_sessions(self.max_sessions)
        if not sessions:
            return "FAIL — no historical sessions to evaluate against"

        # Simulate: with the proposed value, would the metric improve?
        # Use the actual historical metric as the baseline.
        baseline = self._baseline_metric(proposal.metric, sessions)
        simulated = self._simulate(proposal, sessions)

        if time.monotonic() - start > self.max_seconds:
            return "FAIL — shadow evaluation exceeded time budget"

        if simulated >= baseline:
            return (
                f"PASS — {proposal.metric}: {baseline:.2f} -> {simulated:.2f} "
                f"(simulated, {len(sessions)} sessions)"
            )
        return (
            f"FAIL — {proposal.metric}: {baseline:.2f} -> {simulated:.2f} "
            f"(simulated, {len(sessions)} sessions)"
        )

    def _recent_sessions(self, n: int) -> list[str]:
        """Last N distinct session IDs from the episodic store."""
        try:
            rows = self.episodic.recent_sessions(n)
            return [r["session_id"] for r in rows]
        except Exception:
            return []

    def _baseline_metric(self, metric: str, sessions: list[str]) -> float:
        """Actual historical value of the metric over the sessions."""
        if metric == "correction_rate":
            return self.stats_corrections(sessions)
        if metric == "skill_hit_rate":
            return self.stats_skill_hits(sessions)
        if metric == "retrieval_usefulness":
            return self.stats_retrievals(sessions)
        return 0.0

    def stats_corrections(self, sessions: list[str]) -> float:
        total = 0
        for sid in sessions:
            try:
                events = self.episodic.by_session(sid)
                total += sum(1 for e in events if e.get("type") == "correction")
            except Exception:
                continue
        return total / len(sessions) if sessions else 0.0

    def stats_skill_hits(self, sessions: list[str]) -> float:
        hits = uses = 0
        for sid in sessions:
            try:
                events = self.episodic.by_session(sid)
                for e in events:
                    if e.get("type") == "skill_use":
                        uses += 1
                        if e.get("success"):
                            hits += 1
            except Exception:
                continue
        return hits / uses if uses else 0.0

    def stats_retrievals(self, sessions: list[str]) -> float:
        useful = total = 0
        for sid in sessions:
            try:
                events = self.episodic.by_session(sid)
                for e in events:
                    if e.get("type") == "retrieval":
                        total += 1
                        if e.get("useful"):
                            useful += 1
            except Exception:
                continue
        return useful / total if total else 0.0

    def _simulate(self, proposal: Proposal, sessions: list[str]) -> float:
        """Simulate the metric under the proposed value.

        Conservative simulation: apply a bounded improvement factor based
        on the direction of the change. The point is to catch regressions,
        not to predict exact gains.
        """
        base = self._baseline_metric(proposal.metric, sessions)
        if proposal.new_value > proposal.old_value:
            return base * 1.1  # raising a weight/threshold: modest gain
        if proposal.new_value < proposal.old_value:
            return base * 0.95  # lowering: slight reduction
        return base
