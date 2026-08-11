"""Vault tests: init idempotency, atomicity, frontmatter, containment."""

from __future__ import annotations

import pytest
import yaml

from overseer.errors import VaultError
from overseer.vault import REQUIRED_FRONTMATTER, Vault


def test_init_creates_layout(tmp_path):
    vault = Vault(tmp_path / "vault")
    created = vault.init()
    assert len(created) >= 8  # system + template notes
    for folder in ("00-Inbox", "05-System", "10-Sessions", "99-Meta"):
        assert (vault.root / folder).is_dir()
    assert vault.is_vault()


def test_init_idempotent(tmp_path):
    vault = Vault(tmp_path / "vault")
    first = vault.init()
    second = vault.init()
    assert len(second) == 0  # nothing recreated
    assert len(first) == len(second) + len(first)  # sanity


def test_guardrails_contains_l3_guardrail(tmp_path):
    vault = Vault(tmp_path / "vault")
    vault.init()
    text = (vault.root / "05-System" / "Guardrails.md").read_text(encoding="utf-8")
    assert "proposal-only" in text
    assert "human-approved" in text


def test_write_note_creates_valid_frontmatter(tmp_path):
    vault = Vault(tmp_path / "vault")
    vault.init()
    path = vault.write_note(
        "fact",
        "Python 3.11 is required",
        "Overseer requires Python 3.11+.",
        scope="project",
        confidence="high",
        source="test",
    )
    assert path.exists()
    text = path.read_text(encoding="utf-8")
    assert text.startswith("---")
    fm = yaml.safe_load(text.split("---", 2)[1])
    for field in REQUIRED_FRONTMATTER:
        assert field in fm, f"missing {field}"
    assert fm["id"].startswith("OVR-FACT-")
    assert fm["type"] == "fact"
    assert fm["status"] == "active"


def test_write_note_unknown_type_rejected(tmp_path):
    vault = Vault(tmp_path / "vault")
    vault.init()
    with pytest.raises(VaultError, match="unknown note type"):
        vault.write_note("bogus", "x")


def test_write_note_path_containment(tmp_path):
    """The real traversal defense is _contained(); write_note slugifies titles
    so traversal can't reach it via titles — test the guard directly."""
    from overseer.vault import _contained

    vault = Vault(tmp_path / "vault")
    vault.init()
    with pytest.raises(VaultError, match="escapes vault"):
        _contained(vault.root, vault.root / ".." / "escape.md")
    # A normal path inside the vault is fine.
    ok = _contained(vault.root, vault.root / "30-Facts" / "x.md")
    assert ok == (vault.root / "30-Facts" / "x.md").resolve()


def test_list_notes_by_type(tmp_path):
    vault = Vault(tmp_path / "vault")
    vault.init()
    vault.write_note("fact", "First fact", scope="project", confidence="high", source="test")
    vault.write_note("fact", "Second fact", scope="project", confidence="high", source="test")
    vault.write_note(
        "preference", "Short answers", scope="global", strength="medium", source="test"
    )
    facts = vault.list_notes("fact")
    prefs = vault.list_notes("preference")
    assert len(facts) == 2
    assert len(prefs) == 1


def test_slugify_unicode_safe(tmp_path):
    vault = Vault(tmp_path / "vault")
    vault.init()
    path = vault.write_note(
        "fact", "Ünïcode & special: chars!", scope="project", confidence="high", source="test"
    )
    assert path.name.endswith("ünïcode-special-chars.md")
    assert path.name.startswith("OVR-FACT-")


def test_duplicate_titles_do_not_overwrite(tmp_path):
    """Two notes with the same title must coexist (CRITICAL-05)."""
    vault = Vault(tmp_path / "vault")
    vault.init()
    p1 = vault.write_note("fact", "Same title", scope="project", confidence="high", source="test")
    p2 = vault.write_note("fact", "Same title", scope="project", confidence="high", source="test")
    assert p1 != p2
    assert p1.exists() and p2.exists()
    assert len(vault.list_notes("fact")) == 2


def test_note_ids_are_collision_safe(tmp_path):
    """IDs must be unique and stable (CRITICAL-06)."""
    vault = Vault(tmp_path / "vault")
    vault.init()
    ids = {
        vault.write_note(
            "fact", f"Note {i}", scope="project", confidence="high", source="test"
        ).name.split("-", 3)[2]
        for i in range(50)
    }
    assert len(ids) == 50


def test_type_specific_frontmatter_governance(tmp_path):
    """fact requires scope/confidence/source; correction requires trigger (MAJOR-16)."""
    vault = Vault(tmp_path / "vault")
    vault.init()
    with pytest.raises(VaultError, match="scope"):
        vault.write_note("fact", "Missing governance")
    with pytest.raises(VaultError, match="trigger"):
        vault.write_note("correction", "Missing trigger")
    # Valid fact passes.
    path = vault.write_note("fact", "Valid fact", scope="project", confidence="high", source="test")
    assert path.exists()


def test_invalid_status_rejected(tmp_path):
    vault = Vault(tmp_path / "vault")
    vault.init()
    with pytest.raises(VaultError, match="invalid status"):
        vault.write_note(
            "fact", "Bad status", status="bogus", scope="x", confidence="high", source="y"
        )


def test_overseer_gitignore_created(tmp_path):
    """.overseer must never be tracked (MAJOR-15)."""
    vault = Vault(tmp_path / "vault")
    vault.init()
    gi = vault.overseer_dir / ".gitignore"
    assert gi.exists()
    assert gi.read_text(encoding="utf-8") == "*\n"


def test_all_generated_notes_have_valid_frontmatter(tmp_path):
    """Every system/template note must parse as frontmatter markdown (CRITICAL-07)."""
    vault = Vault(tmp_path / "vault")
    vault.init()
    for path in vault.root.glob("**/*.md"):
        text = path.read_text(encoding="utf-8")
        assert text.startswith("---"), f"{path} missing frontmatter open"
        parts = text.split("---", 2)
        assert len(parts) >= 3, f"{path} missing frontmatter close"
        fm = yaml.safe_load(parts[1])
        assert isinstance(fm, dict), f"{path} frontmatter not a mapping"
        assert "id" in fm and "type" in fm, f"{path} missing id/type"
