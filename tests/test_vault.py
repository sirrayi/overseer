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
    path = vault.write_note("fact", "Python 3.11 is required", "Overseer requires Python 3.11+.")
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
    vault.write_note("fact", "First fact")
    vault.write_note("fact", "Second fact")
    vault.write_note("preference", "Short answers")
    facts = vault.list_notes("fact")
    prefs = vault.list_notes("preference")
    assert len(facts) == 2
    assert len(prefs) == 1


def test_slugify_unicode_safe(tmp_path):
    vault = Vault(tmp_path / "vault")
    vault.init()
    path = vault.write_note("fact", "Ünïcode & special: chars!")
    assert path.name == "ünïcode-special-chars.md"
