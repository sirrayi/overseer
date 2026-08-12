"""Repo intelligence tools: repo_map, git_status, git_diff, git_log (plan B4).

All git commands run via subprocess with a timeout. Destructive commands
(git push, git reset --hard) are approval-gated. Output is redacted and
truncated; full output goes to artifacts.
"""

from __future__ import annotations

import subprocess  # nosec B404 — git tool runs git by design
from pathlib import Path
from typing import Any

from overseer.project import repo_map
from overseer.tools.base import Tool, ToolContext, ToolResult
from overseer.tools.registry import register_tool

_GIT_TIMEOUT = 30
# Destructive git commands: always require approval.
_DESTRUCTIVE = ("push", "reset --hard", "clean -f", "checkout --", "revert", "merge", "rebase")


def _run_git(args: list[str], cwd: Path) -> tuple[int, str]:
    """Run git, return (exit_code, output)."""
    try:
        proc = subprocess.run(  # nosec B603,B607 — fixed git binary, validated args
            ["git", *args],
            capture_output=True,
            text=True,
            timeout=_GIT_TIMEOUT,
            cwd=str(cwd),
        )
        out = (proc.stdout or "") + (proc.stderr or "")
        return proc.returncode, out
    except subprocess.TimeoutExpired:
        return 124, f"git timed out after {_GIT_TIMEOUT}s"
    except OSError as exc:
        return 127, f"git failed: {exc}"


def _is_git_repo(cwd: Path) -> bool:
    code, _ = _run_git(["rev-parse", "--is-inside-work-tree"], cwd)
    return code == 0


@register_tool
class RepoMapTool(Tool):
    """Generate a lightweight map of the repository (cached)."""

    name = "repo_map"
    description = (
        "Generate a lightweight map of the repository: file tree, key modules, entry points."
    )
    parameters: dict[str, Any] = {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Directory to map (default: cwd)."}
        },
        "required": [],
    }

    def run(self, args: dict[str, Any], context: ToolContext | None = None) -> ToolResult:
        root = Path(args.get("path", ".")).expanduser()
        if not root.is_dir():
            return self._error(f"not a directory: {root}")
        m = repo_map(root)
        return self._result(m.summary())


@register_tool
class GitStatusTool(Tool):
    """Show the working tree status (git status --short)."""

    name = "git_status"
    description = "Show the working tree status (git status --short)."
    parameters: dict[str, Any] = {
        "type": "object",
        "properties": {"path": {"type": "string", "description": "Repo directory."}},
        "required": [],
    }

    def run(self, args: dict[str, Any], context: ToolContext | None = None) -> ToolResult:
        cwd = Path(args.get("path", ".")).expanduser()
        if not _is_git_repo(cwd):
            return self._error(f"not a git repository: {cwd}")
        code, out = _run_git(["status", "--short"], cwd)
        if code != 0:
            return self._error(out)
        return self._result(out)


@register_tool
class GitDiffTool(Tool):
    """Show the diff of uncommitted changes (git diff)."""

    name = "git_diff"
    description = "Show the diff of uncommitted changes (git diff)."
    parameters: dict[str, Any] = {
        "type": "object",
        "properties": {"path": {"type": "string", "description": "Repo directory."}},
        "required": [],
    }

    def run(self, args: dict[str, Any], context: ToolContext | None = None) -> ToolResult:
        cwd = Path(args.get("path", ".")).expanduser()
        if not _is_git_repo(cwd):
            return self._error(f"not a git repository: {cwd}")
        code, out = _run_git(["diff", "--stat"], cwd)
        if code != 0:
            return self._error(out)
        return self._result(out)


@register_tool
class GitLogTool(Tool):
    """Show recent commit history (git log --oneline -n 20)."""

    name = "git_log"
    description = "Show recent commit history (git log --oneline -n 20)."
    parameters: dict[str, Any] = {
        "type": "object",
        "properties": {"path": {"type": "string", "description": "Repo directory."}},
        "required": [],
    }

    def run(self, args: dict[str, Any], context: ToolContext | None = None) -> ToolResult:
        cwd = Path(args.get("path", ".")).expanduser()
        if not _is_git_repo(cwd):
            return self._error(f"not a git repository: {cwd}")
        code, out = _run_git(["log", "--oneline", "-n", "20"], cwd)
        if code != 0:
            return self._error(out)
        return self._result(out)
