"""Dataset builder tests: redaction, extraction, hosted opt-in (plan B11)."""

from __future__ import annotations

import json
from pathlib import Path

from overseer.datasets import DatasetBuilder


def _vault(tmp_path: Path) -> Path:
    root = tmp_path / "vault"
    (root / "80-Corrections").mkdir(parents=True)
    (root / "50-Preferences").mkdir(parents=True)
    return root


def _write_note(root: Path, folder: str, name: str, fm: dict, body: str) -> None:
    lines = ["---"]
    for k, v in fm.items():
        lines.append(f"{k}: {v}")
    lines.append("---")
    lines.append("")
    lines.append(body)
    (root / folder / name).write_text("\n".join(lines), encoding="utf-8")


def test_correction_pairs_extracted(tmp_path):
    root = _vault(tmp_path)
    _write_note(
        root,
        "80-Corrections",
        "c1.md",
        {"id": "OVR-CORR-1", "mistake": "used tabs", "rule": "use spaces"},
        "body",
    )
    db = DatasetBuilder(root)
    pairs = db.correction_pairs()
    assert len(pairs) == 1
    assert pairs[0]["bad"] == "used tabs"
    assert pairs[0]["good"] == "use spaces"


def test_preference_pairs_extracted(tmp_path):
    root = _vault(tmp_path)
    _write_note(
        root,
        "50-Preferences",
        "p1.md",
        {"id": "OVR-PREF-1", "rejected": "verbose", "accepted": "concise"},
        "body",
    )
    db = DatasetBuilder(root)
    pairs = db.preference_pairs()
    assert len(pairs) == 1
    assert pairs[0]["rejected"] == "verbose"
    assert pairs[0]["accepted"] == "concise"


def test_build_redacts_secrets(tmp_path):
    root = _vault(tmp_path)
    _write_note(
        root,
        "80-Corrections",
        "c1.md",
        {
            "id": "OVR-CORR-1",
            "mistake": "leaked api_key=sk-1234567890abcdef",
            "rule": "never log keys",
        },
        "body",
    )
    db = DatasetBuilder(root)
    path = db.build(include_traces=False)
    raw = path.read_text(encoding="utf-8")
    assert "sk-1234567890abcdef" not in raw
    assert "sk-" in raw  # redacted form present
    rec = json.loads(path.read_text(encoding="utf-8").splitlines()[0])
    assert rec["bad"] != "leaked api_key=sk-1234567890abcdef"


def test_build_writes_jsonl(tmp_path):
    root = _vault(tmp_path)
    _write_note(
        root,
        "80-Corrections",
        "c1.md",
        {"id": "OVR-CORR-1", "mistake": "m1", "rule": "r1"},
        "body",
    )
    db = DatasetBuilder(root)
    path = db.build(include_traces=False)
    assert path.exists()
    assert path.suffix == ".jsonl"
    assert path.parent.name == "datasets"
    lines = path.read_text(encoding="utf-8").splitlines()
    assert len(lines) == 1


def test_hosted_upload_blocked_by_default(tmp_path):
    root = _vault(tmp_path)
    db = DatasetBuilder(root)  # hosted_training_enabled defaults False
    path = db.build(include_traces=False)
    assert db.upload_stub(path) is False


def test_hosted_upload_allowed_when_opted_in(tmp_path):
    root = _vault(tmp_path)
    db = DatasetBuilder(root, hosted_training_enabled=True)
    path = db.build(include_traces=False)
    assert db.upload_stub(path) is True


def test_tool_traces_from_episodic(tmp_path):
    root = _vault(tmp_path)

    class _FakeEpisodic:
        def recent_sessions(self, n):
            return [{"session_id": "s1"}]

        def by_session(self, sid):
            return [
                {
                    "type": "tool_call",
                    "tool_name": "file_read",
                    "arguments": {"path": str(tmp_path / "x")},
                    "status": "ok",
                },
                {
                    "type": "tool_call",
                    "tool_name": "file_patch",
                    "arguments": {"path": str(tmp_path / "x")},
                    "status": "ok",
                },
            ]

    db = DatasetBuilder(root, episodic=_FakeEpisodic())
    traces = db.tool_traces()
    assert len(traces) == 1
    assert traces[0]["type"] == "tool_trace"
    assert len(traces[0]["calls"]) == 2
