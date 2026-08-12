"""Skill curator: storage, risk classification, and promotion gates (plan B7).

Skills are stored as governed vault notes in 40-Skills via ``Vault.write_note``.
The curator owns the lifecycle:

    draft -> proposed -> active

Promotion rules (ROADMAP "Procedural Memory and Skills"):
- A skill needs >= 3 independent successful episodes before it can become active.
- Low-risk skills may AUTO-promote after repeated manual adoption
  (success_count >= 2).
- High-risk skills ALWAYS require explicit human approval.
- Correction replay happens in the miner (``miner.py``), which rejects or flags
  any draft that contradicts a high-confidence correction before it is written.

This module intentionally contains NO network/agent logic. It is pure, testable
state transitions over skill notes. Mining and drafting live in ``miner.py``.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import yaml

from overseer.errors import CuratorError
from overseer.vault import Vault, _atomic_write

# --- risk classification ---------------------------------------------------
# Tools that can mutate the host, hit the network, or write durable state are
# HIGH risk. Read-only / formatting / inspection tools are LOW risk.
_HIGH_RISK_TOOLS: frozenset[str] = frozenset(
    {
        "terminal",  # arbitrary shell — worst case
        "filesystem",  # can write/delete
        "git",  # commit/push/force
        "repo",  # repo mutation (B4 tools)
        "deploy",  # deployment tools
        "package",  # installs
        "network",  # outbound requests
        "db",  # persistent writes
    }
)

# A pattern observed in only this many distinct sessions is too weak to
# generalize. Required evidence floor (ROADMAP: >= 3 independent successful
# episodes).
MIN_EVIDENCE_SESSIONS = 3

# Success thresholds by risk (B7 spec: 70% low-risk, 90% high-risk).
SUCCESS_THRESHOLD_LOW = 0.70
SUCCESS_THRESHOLD_HIGH = 0.90

# Low-risk skills may auto-promote after this many verified successful uses.
AUTO_PROMOTE_MIN_SUCCESS = 2

# Valid lifecycle statuses.
SKILL_STATUSES = ("draft", "proposed", "active")

# Skill note frontmatter required by the B0 vault ontology.
SKILL_FRONTMATTER_FIELDS = (
    "trigger",
    "risk",
    "use_count",
    "success_count",
    "failure_count",
    "confidence",
)


def classify_risk(tool_names: list[str]) -> str:
    """Return ``low`` or ``high`` based on the tools a pattern relies on.

    A pattern is high-risk if it touches ANY high-risk tool. An empty tool set
    is treated as low risk (no observable side effects).
    """
    for name in tool_names:
        if name.lower() in _HIGH_RISK_TOOLS:
            return "high"
    return "low"


def _now() -> str:
    return datetime.now(UTC).isoformat(timespec="seconds")


@dataclass
class Skill:
    """One procedural skill note with its lifecycle counters."""

    id: str
    title: str
    trigger: str
    steps: str  # the body / guidance
    risk: str = "low"  # low | high
    status: str = "draft"  # draft | proposed | active
    confidence: float = 0.0
    use_count: int = 0
    success_count: int = 0
    failure_count: int = 0
    evidence: list[str] = field(default_factory=list)  # session ids
    created: str = ""
    metadata: dict[str, Any] = field(default_factory=dict)

    @property
    def success_rate(self) -> float:
        """Fraction of uses that succeeded. 0.0 when no uses recorded."""
        total = self.use_count
        if total <= 0:
            return 0.0
        return self.success_count / total

    def to_frontmatter(self) -> dict[str, Any]:
        """Frontmatter for the governed ``Vault.write_note`` skill schema."""
        return {
            "trigger": self.trigger,
            "risk": self.risk,
            "use_count": self.use_count,
            "success_count": self.success_count,
            "failure_count": self.failure_count,
            "confidence": self.confidence,
            "status": self.status,
            "tags": ["skill", self.risk],
            "evidence": " ".join(self.evidence),
        }


class SkillRegistry:
    """Read/write skill notes in the governed 40-Skills folder.

    All writes go through ``Vault.write_note`` so stable IDs, timestamps, and
    frontmatter validation are enforced (B0/B5 governed API).
    """

    def __init__(self, vault_root: str | Any) -> None:
        self.vault = Vault(vault_root)
        self.vault.init()  # idempotent
        self._skills_dir = self.vault.root / "40-Skills"

    # -- reads --------------------------------------------------------------

    def load_all(self) -> list[Skill]:
        """Return every skill note, newest file first."""
        if not self._skills_dir.is_dir():
            return []
        out: list[Skill] = []
        for note in sorted(self._skills_dir.glob("*.md"), reverse=True):
            try:
                text = note.read_text(encoding="utf-8")
            except OSError:
                continue
            meta = self._parse_note(note.name, text)
            if meta is not None:
                out.append(meta)
        return out

    def get(self, skill_id: str) -> Skill | None:
        for skill in self.load_all():
            if skill.id == skill_id:
                return skill
        return None

    def by_status(self, status: str) -> list[Skill]:
        return [s for s in self.load_all() if s.status == status]

    # -- writes -------------------------------------------------------------

    def create_draft(self, skill: Skill) -> str:
        """Write a new skill note with status ``draft``.

        Refuses to overwrite an existing note with the same ID. Returns the
        note ID.
        """
        if skill.id and self.get(skill.id) is not None:
            raise CuratorError(f"skill already exists: {skill.id}")
        skill.status = "draft"
        skill.created = skill.created or _now()
        path = self.vault.write_note(
            "skill",
            title=skill.title,
            body=skill.steps,
            **skill.to_frontmatter(),
        )
        # Vault assigns the real ID in the filename (OVR-SKILL-<hex>).
        skill.id = _id_from_path(path)
        return skill.id

    def update(self, skill: Skill) -> None:
        """Rewrite an existing skill note in place, preserving its ID.

        Unlike ``create_draft`` (which lets the vault mint a new ID), updates
        must target the exact file the note already lives in so counters are
        bumped on the same OVR-SKILL id rather than spawning a duplicate.
        """
        existing = self.get(skill.id)
        if existing is None:
            raise CuratorError(f"skill not found: {skill.id}")
        path = self._note_path(skill.id)
        if path is None:
            raise CuratorError(f"skill note not found on disk: {skill.id}")
        meta = skill.to_frontmatter()
        meta["id"] = skill.id
        meta["type"] = "skill"
        meta["title"] = skill.title
        meta["created"] = existing.created or _now()
        meta["modified"] = _now()
        header = yaml.safe_dump(meta, sort_keys=False, allow_unicode=True).strip()
        content = f"---\n{header}\n---\n\n# {skill.title}\n\n{skill.steps}".rstrip() + "\n"
        _atomic_write(path, content)

    def _note_path(self, skill_id: str) -> Path | None:
        """Find the on-disk file for a skill id (OVR-SKILL-<hex>)."""
        if not self._skills_dir.is_dir():
            return None
        for note in self._skills_dir.glob("*.md"):
            if note.name.startswith(skill_id + "-"):
                return note
        return None

    # -- lifecycle transitions ----------------------------------------------

    def record_use(self, skill_id: str, success: bool) -> Skill:
        """Bump a skill's counters and apply auto-promotion for low-risk.

        Low-risk skills with ``success_count >= AUTO_PROMOTE_MIN_SUCCESS`` move
        draft/proposed -> active automatically. High-risk skills NEVER
        auto-promote; they stay proposed until ``promote(approved=True)``.
        Returns the updated skill.
        """
        skill = self.get(skill_id)
        if skill is None:
            raise CuratorError(f"skill not found: {skill_id}")
        skill.use_count += 1
        if success:
            skill.success_count += 1
        else:
            skill.failure_count += 1
        skill = self._apply_auto_promotion(skill)
        self.update(skill)
        return skill

    def promote(self, skill_id: str, approved: bool) -> Skill:
        """Explicit human-gated promotion.

        - ``approved=True`` and low risk -> active.
        - ``approved=True`` and high risk -> active only if it also passes the
          evidence floor (success rate above the high-risk threshold).
        - ``approved=False`` -> status ``rejected``.
        Returns the updated skill.
        """
        skill = self.get(skill_id)
        if skill is None:
            raise CuratorError(f"skill not found: {skill_id}")
        if not approved:
            skill.status = "rejected"
            self.update(skill)
            return skill
        threshold = SUCCESS_THRESHOLD_HIGH if skill.risk == "high" else SUCCESS_THRESHOLD_LOW
        if skill.use_count >= MIN_EVIDENCE_SESSIONS and skill.success_rate >= threshold:
            skill.status = "active"
        else:
            # Not enough evidence yet -> keep it proposed, not active.
            skill.status = "proposed"
        self.update(skill)
        return skill

    # -- helpers ------------------------------------------------------------

    def _apply_auto_promotion(self, skill: Skill) -> Skill:
        if skill.risk != "low":
            return skill  # high risk never auto-promotes
        if skill.success_count >= AUTO_PROMOTE_MIN_SUCCESS:
            skill.status = "active"
        return skill

    def _parse_note(self, filename: str, text: str) -> Skill | None:
        """Parse a skill note's YAML frontmatter into a Skill."""
        try:
            if not text.startswith("---"):
                return None
            end = text.find("\n---", 3)
            if end == -1:
                return None
            meta = yaml.safe_load(text[3:end]) or {}
        except Exception:
            return None
        if not isinstance(meta, dict):
            return None
        title = meta.get("title") or filename
        body = text[end + 4 :].strip()
        # Strip leading "# <title>" heading if present.
        lines = body.splitlines()
        if lines and lines[0].startswith("# "):
            body = "\n".join(lines[1:]).strip()
        try:
            return Skill(
                id=meta.get("id", ""),
                title=str(title),
                trigger=str(meta.get("trigger", "")),
                steps=body,
                risk=str(meta.get("risk", "low")),
                status=str(meta.get("status", "draft")),
                confidence=float(meta.get("confidence", 0.0)),
                use_count=int(meta.get("use_count", 0)),
                success_count=int(meta.get("success_count", 0)),
                failure_count=int(meta.get("failure_count", 0)),
                evidence=(str(meta.get("evidence", "")).split() if meta.get("evidence") else []),
                created=str(meta.get("created", "")),
            )
        except (TypeError, ValueError):
            return None


def _id_from_path(path: Any) -> str:
    """Extract the OVR-* id from a written note filename."""
    name: str = str(path.name)
    # Filenames look like: OVR-SKILL-abc12345-slug.md
    parts = name.split("-", 3)
    if len(parts) >= 3 and parts[0] == "OVR" and parts[1] == "SKILL":
        return f"{parts[0]}-{parts[1]}-{parts[2]}"
    return name
