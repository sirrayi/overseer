"""Dataset builder: the sessions ARE the dataset (plan B11).

Consumes the episodic store and vault knowledge to build training
datasets for weight-level adaptation (Tier 2):

- Correction pairs: (bad response, corrected response) from 80-Corrections.
- Preference pairs: (rejected style, accepted style) from 50-Preferences.
- Tool traces: successful tool-call sequences from verified sessions.

Security invariants:
- redact() runs on EVERY record before it touches disk.
- Datasets live in .overseer/datasets/ (gitignored, private).
- Nothing leaves the machine unless hosted_training_enabled=True (opt-in).
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from overseer.redact import redact


@dataclass
class DatasetBuilder:
    """Build redacted JSONL training datasets from vault + episodic store."""

    vault_root: Path
    episodic: Any | None = None
    hosted_training_enabled: bool = False
    records: list[dict[str, Any]] = field(default_factory=list)

    def __post_init__(self) -> None:
        self.datasets_dir = self.vault_root / ".overseer" / "datasets"
        self.datasets_dir.mkdir(parents=True, exist_ok=True)

    # -- extraction ---------------------------------------------------------

    def _read_notes(self, folder: str) -> list[dict[str, Any]]:
        """Read markdown notes from a vault folder, returning frontmatter + body."""
        notes: list[dict[str, Any]] = []
        folder_path = self.vault_root / folder
        if not folder_path.exists():
            return notes
        for path in sorted(folder_path.glob("*.md")):
            text = path.read_text(encoding="utf-8")
            fm: dict[str, str] = {}
            body = text
            if text.startswith("---"):
                parts = text.split("---", 2)
                if len(parts) >= 3:
                    for line in parts[1].strip().splitlines():
                        if ":" in line:
                            k, v = line.split(":", 1)
                            fm[k.strip()] = v.strip()
                    body = parts[2]
            notes.append({"frontmatter": fm, "body": body, "path": str(path)})
        return notes

    def correction_pairs(self) -> list[dict[str, Any]]:
        """(bad response, corrected response) from 80-Corrections."""
        pairs: list[dict[str, Any]] = []
        for note in self._read_notes("80-Corrections"):
            fm = note["frontmatter"]
            # Correction notes carry mistake + rule frontmatter (B0 ontology).
            mistake = fm.get("mistake", "")
            rule = fm.get("rule", "")
            if mistake and rule:
                pairs.append(
                    {
                        "type": "correction",
                        "bad": mistake,
                        "good": rule,
                        "source": fm.get("id", note["path"]),
                    }
                )
        return pairs

    def preference_pairs(self) -> list[dict[str, Any]]:
        """(rejected style, accepted style) from 50-Preferences."""
        pairs: list[dict[str, Any]] = []
        for note in self._read_notes("50-Preferences"):
            fm = note["frontmatter"]
            rejected = fm.get("rejected", "")
            accepted = fm.get("accepted", "") or fm.get("preference", "")
            if rejected and accepted:
                pairs.append(
                    {
                        "type": "preference",
                        "rejected": rejected,
                        "accepted": accepted,
                        "source": fm.get("id", note["path"]),
                    }
                )
        return pairs

    def tool_traces(self, max_traces: int = 50) -> list[dict[str, Any]]:
        """Successful tool-call sequences from verified sessions."""
        traces: list[dict[str, Any]] = []
        if self.episodic is None:
            return traces
        try:
            sessions = self.episodic.recent_sessions(max_traces)
        except Exception:
            return traces
        for row in sessions:
            sid = row["session_id"]
            try:
                events = self.episodic.by_session(sid)
            except Exception:  # noqa: S112 — corrupt session skipped by design
                continue
            calls = [
                {
                    "tool": e.get("tool_name", ""),
                    "args": e.get("arguments", {}),
                    "ok": e.get("status") == "ok",
                }
                for e in events
                if e.get("type") == "tool_call"
            ]
            if calls and all(c["ok"] for c in calls):
                traces.append(
                    {
                        "type": "tool_trace",
                        "session": sid,
                        "calls": calls,
                    }
                )
        return traces

    # -- build --------------------------------------------------------------

    def build(self, include_traces: bool = True) -> Path:
        """Build the full redacted dataset and write it to .overseer/datasets/.

        Returns the dataset path. Every record passes through redact().
        """
        self.records = []
        for pair in self.correction_pairs():
            self.records.append(self._redact_record(pair))
        for pair in self.preference_pairs():
            self.records.append(self._redact_record(pair))
        if include_traces:
            for trace in self.tool_traces():
                self.records.append(self._redact_record(trace))

        stamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
        path = self.datasets_dir / f"dataset-{stamp}.jsonl"
        with path.open("w", encoding="utf-8") as fh:
            for rec in self.records:
                fh.write(json.dumps(rec, ensure_ascii=False) + "\n")
        return path

    def _redact_record(self, record: dict[str, Any]) -> dict[str, Any]:
        """Deep-redact a record: every string value passes through redact()."""
        out: dict[str, Any] = {}
        for k, v in record.items():
            if isinstance(v, str):
                out[k] = redact(v)
            elif isinstance(v, dict):
                out[k] = self._redact_record(v)
            elif isinstance(v, list):
                out[k] = [
                    self._redact_record(i)
                    if isinstance(i, dict)
                    else redact(i)
                    if isinstance(i, str)
                    else i
                    for i in v
                ]
            else:
                out[k] = v
        return out

    # -- hosted opt-in ------------------------------------------------------

    def upload_stub(self, dataset_path: Path) -> bool:
        """Stub for hosted training upload. BLOCKED unless opted in.

        Returns True only when hosted_training_enabled is set. The real
        upload is a B12 concern; the gate is enforced now.
        """
        if not self.hosted_training_enabled:
            return False
        # Stub: nothing leaves the machine yet.
        return self.hosted_training_enabled
