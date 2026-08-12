"""Repo tool tests: repo_map, git_status, git_diff, git_log (plan B4)."""

from __future__ import annotations

import subprocess  # nosec B404 — tests run git
from pathlib import Path

from overseer.tools import ToolContext, ToolRegistry, get_tool_class, registered_tools


def _git(cwd: Path, *args: str) -> None:
    subprocess.run(  # nosec B603,B607 — fixed git binary in tests
        ["git", *args], cwd=str(cwd), capture_output=True, text=True, check=True
    )


def _repo(tmp_path: Path) -> Path:
    _git(tmp_path, "init", "-q", "-b", "main")
    _git(tmp_path, "config", "user.email", "t@t")
    _git(tmp_path, "config", "user.name", "t")
    (tmp_path / "a.py").write_text("x = 1\n", encoding="utf-8")
    _git(tmp_path, "add", ".")
    _git(tmp_path, "commit", "-q", "-m", "init")
    return tmp_path


def _reg() -> ToolRegistry:
    reg = ToolRegistry()
    for name in registered_tools():
        reg.add(get_tool_class(name)())
    return reg


def test_repo_tools_registered():
    names = registered_tools()
    assert "repo_map" in names
    assert "git_status" in names
    assert "git_diff" in names
    assert "git_log" in names


def test_repo_map_tool(tmp_path):
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "main.py").write_text("print(1)\n", encoding="utf-8")
    reg = _reg()
    r = reg.dispatch("repo_map", {"path": str(tmp_path)}, ToolContext())
    assert r.status == "ok"
    assert "main.py" in r.summary


def test_git_status_tool(tmp_path):
    repo = _repo(tmp_path)
    (repo / "a.py").write_text("x = 2\n", encoding="utf-8")
    reg = _reg()
    r = reg.dispatch("git_status", {"path": str(repo)}, ToolContext())
    assert r.status == "ok"
    assert "a.py" in r.summary


def test_git_diff_tool(tmp_path):
    repo = _repo(tmp_path)
    (repo / "a.py").write_text("x = 2\n", encoding="utf-8")
    reg = _reg()
    r = reg.dispatch("git_diff", {"path": str(repo)}, ToolContext())
    assert r.status == "ok"
    assert "a.py" in r.summary


def test_git_log_tool(tmp_path):
    repo = _repo(tmp_path)
    reg = _reg()
    r = reg.dispatch("git_log", {"path": str(repo)}, ToolContext())
    assert r.status == "ok"
    assert "init" in r.summary


def test_git_tools_not_a_repo(tmp_path):
    reg = _reg()
    r = reg.dispatch("git_status", {"path": str(tmp_path)}, ToolContext())
    assert r.status == "error"
    assert "not a git repository" in r.summary
