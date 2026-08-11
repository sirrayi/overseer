"""Approval gate tests: denylist, allowlist, risky, path policy."""

from __future__ import annotations

from pathlib import Path

import pytest

from overseer.approval import ApprovalPolicy
from overseer.errors import ApprovalDenied


def _policy(tmp_path: Path, approver=None) -> ApprovalPolicy:
    return ApprovalPolicy(allowed_roots=[tmp_path], approver=approver)


def test_denylist_blocks_rm_rf_root():
    p = _policy(Path("/tmp"))
    with pytest.raises(ApprovalDenied, match="denylist"):
        p.approve("terminal", {"command": "rm -rf /"})


def test_denylist_blocks_fork_bomb():
    p = _policy(Path("/tmp"))
    with pytest.raises(ApprovalDenied):
        p.approve("terminal", {"command": ":(){ :|:& };:"})


def test_denylist_blocks_git_push_force():
    p = _policy(Path("/tmp"))
    with pytest.raises(ApprovalDenied):
        p.approve("terminal", {"command": "git push --force origin main"})


def test_allowlist_auto_approved():
    p = _policy(Path("/tmp"))
    assert p.approve("terminal", {"command": "ls -la"}) is True
    assert p.approve("terminal", {"command": "git status"}) is True
    assert p.approve("terminal", {"command": "uv run pytest"}) is True


def test_risky_requires_approver():
    p = _policy(Path("/tmp"), approver=None)
    with pytest.raises(ApprovalDenied, match="requires approval"):
        p.approve("terminal", {"command": "rm old.txt"})


def test_risky_approved_by_user():
    calls = []

    def approver(tool, args):
        calls.append(args["command"])
        return True

    p = _policy(Path("/tmp"), approver=approver)
    assert p.approve("terminal", {"command": "rm old.txt"}) is True
    assert calls == ["rm old.txt"]


def test_risky_denied_by_user():
    p = _policy(Path("/tmp"), approver=lambda t, a: False)
    assert p.approve("terminal", {"command": "rm old.txt"}) is False


def test_write_inside_root_auto_approved(tmp_path):
    p = _policy(tmp_path)
    assert p.approve("file_write", {"path": str(tmp_path / "x.txt")}) is True


def test_write_outside_root_requires_approval(tmp_path):
    p = _policy(tmp_path, approver=None)
    with pytest.raises(ApprovalDenied, match="outside allowed roots"):
        p.approve("file_write", {"path": "/etc/x.txt"})


def test_write_outside_root_approved_by_user(tmp_path):
    p = _policy(tmp_path, approver=lambda t, a: True)
    assert p.approve("file_write", {"path": "/etc/x.txt"}) is True


def test_unknown_command_defaults_allow():
    """Unknown commands default to allow (documented policy)."""
    p = _policy(Path("/tmp"))
    assert p.approve("terminal", {"command": "some-unknown-tool --flag"}) is True


def test_non_gated_tools_pass():
    p = _policy(Path("/tmp"))
    assert p.approve("file_read", {"path": "/etc/passwd"}) is True
    assert p.approve("grep", {"pattern": "x"}) is True
