"""Tool tests: dispatch, traversal, truncation, redaction, artifacts."""

from __future__ import annotations

from pathlib import Path

import pytest

from overseer.errors import ToolError
from overseer.tools import ToolContext, ToolRegistry, get_tool_class, registered_tools


def _registry(tmp_path: Path) -> ToolRegistry:
    reg = ToolRegistry()
    for name in registered_tools():
        reg.add(get_tool_class(name)())
    return reg


def _ctx(tmp_path: Path, approver=None) -> ToolContext:
    return ToolContext(
        allowed_roots=[tmp_path],
        artifacts_dir=tmp_path / ".overseer" / "artifacts",
        approver=approver,
    )


def test_all_core_tools_registered():
    for name in ("terminal", "file_read", "file_write", "file_patch", "list_dir", "grep"):
        assert name in registered_tools(), f"{name} not registered"


def test_specs_are_openai_format():
    reg = _registry(Path("/tmp"))
    for spec in reg.specs():
        assert spec["type"] == "function"
        assert "name" in spec["function"]
        assert "parameters" in spec["function"]


def test_dispatch_unknown_tool():
    reg = _registry(Path("/tmp"))
    with pytest.raises(ToolError, match="not registered"):
        reg.dispatch("nope", {})


def test_file_write_and_read_roundtrip(tmp_path):
    reg = _registry(tmp_path)
    ctx = _ctx(tmp_path)
    result = reg.dispatch("file_write", {"path": "hello.txt", "content": "hi there"}, ctx)
    assert result.status == "ok"
    read = reg.dispatch("file_read", {"path": "hello.txt"}, ctx)
    assert read.status == "ok"
    assert "hi there" in read.summary


def test_file_write_traversal_blocked(tmp_path):
    reg = _registry(tmp_path)
    ctx = _ctx(tmp_path)
    result = reg.dispatch("file_write", {"path": "../escape.txt", "content": "x"}, ctx)
    assert result.status == "error"
    assert "escapes" in (result.error or "")
    assert not (tmp_path.parent / "escape.txt").exists()


def test_file_write_absolute_outside_blocked(tmp_path):
    reg = _registry(tmp_path)
    ctx = _ctx(tmp_path)
    result = reg.dispatch("file_write", {"path": "/etc/evil.txt", "content": "x"}, ctx)
    assert result.status == "error"


def test_file_read_missing(tmp_path):
    reg = _registry(tmp_path)
    result = reg.dispatch("file_read", {"path": "nope.txt"}, _ctx(tmp_path))
    assert result.status == "error"
    assert "not found" in (result.error or "")


def test_file_patch_roundtrip(tmp_path):
    reg = _registry(tmp_path)
    ctx = _ctx(tmp_path)
    reg.dispatch("file_write", {"path": "a.txt", "content": "hello world"}, ctx)
    result = reg.dispatch("file_patch", {"path": "a.txt", "old": "world", "new": "overseer"}, ctx)
    assert result.status == "ok"
    text = (tmp_path / "a.txt").read_text(encoding="utf-8")
    assert text == "hello overseer"


def test_file_patch_old_not_found(tmp_path):
    reg = _registry(tmp_path)
    ctx = _ctx(tmp_path)
    reg.dispatch("file_write", {"path": "a.txt", "content": "hello"}, ctx)
    result = reg.dispatch("file_patch", {"path": "a.txt", "old": "zzz", "new": "x"}, ctx)
    assert result.status == "error"
    assert "not found" in (result.error or "")


def test_list_dir(tmp_path):
    (tmp_path / "one.txt").write_text("1", encoding="utf-8")
    (tmp_path / "two.txt").write_text("2", encoding="utf-8")
    reg = _registry(tmp_path)
    result = reg.dispatch("list_dir", {"path": str(tmp_path)}, _ctx(tmp_path))
    assert result.status == "ok"
    assert "one.txt" in result.summary
    assert "two.txt" in result.summary


def test_grep_finds_matches(tmp_path):
    (tmp_path / "code.py").write_text("def hello():\n    pass\n", encoding="utf-8")
    reg = _registry(tmp_path)
    result = reg.dispatch("grep", {"pattern": "def", "path": str(tmp_path)}, _ctx(tmp_path))
    assert result.status == "ok"
    assert "code.py:1" in result.summary


def test_grep_invalid_regex(tmp_path):
    reg = _registry(tmp_path)
    result = reg.dispatch("grep", {"pattern": "[", "path": str(tmp_path)}, _ctx(tmp_path))
    assert result.status == "error"
    assert "invalid regex" in (result.error or "")


def test_output_redacted_in_summary(tmp_path):
    """Secrets in tool output must never reach the model summary."""
    reg = _registry(tmp_path)
    ctx = _ctx(tmp_path)
    reg.dispatch(
        "file_write",
        {"path": "secret.txt", "content": "key=sk-1234567890abcdef1234567890abcdef"},
        ctx,
    )
    read = reg.dispatch("file_read", {"path": "secret.txt"}, ctx)
    assert "sk-1234567890abcdef1234567890abcdef" not in read.summary
    assert "sk-***REDACTED***" in read.summary


def test_artifact_stored_for_large_output(tmp_path):
    """Full output goes to artifacts; summary is truncated."""
    reg = _registry(tmp_path)
    ctx = _ctx(tmp_path)
    big = "line of text\n" * 5000  # ~65KB
    reg.dispatch("file_write", {"path": "big.txt", "content": big}, ctx)
    read = reg.dispatch("file_read", {"path": "big.txt"}, ctx)
    assert read.artifacts, "expected an artifact path"
    artifact = Path(read.artifacts[0])
    assert artifact.exists()
    assert len(artifact.read_text(encoding="utf-8")) > 10000
    assert len(read.summary) <= 4000


def test_terminal_requires_approval(tmp_path):
    """Without an approver, terminal must be denied (fail closed)."""
    reg = _registry(tmp_path)
    ctx = _ctx(tmp_path, approver=None)
    result = reg.dispatch("terminal", {"command": "echo hi"}, ctx)
    assert result.status == "error"
    assert "denied" in (result.error or "")


def test_terminal_approved_runs(tmp_path):
    reg = _registry(tmp_path)
    ctx = _ctx(tmp_path, approver=lambda tool, args: True)
    result = reg.dispatch("terminal", {"command": "echo hello-overseer"}, ctx)
    assert result.status == "ok"
    assert "hello-overseer" in result.summary


def test_terminal_denied_by_approver(tmp_path):
    reg = _registry(tmp_path)
    ctx = _ctx(tmp_path, approver=lambda tool, args: False)
    result = reg.dispatch("terminal", {"command": "echo hi"}, ctx)
    assert result.status == "error"
    assert "not approved" in (result.error or "")


def test_terminal_timeout_capped(tmp_path):
    reg = _registry(tmp_path)
    ctx = _ctx(tmp_path, approver=lambda tool, args: True)
    result = reg.dispatch("terminal", {"command": "sleep 1", "timeout": 999}, ctx)
    assert result.status == "error"
    assert "capped" in (result.error or "")
