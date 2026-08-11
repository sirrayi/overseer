"""Approval gate: policy engine for risky actions (plan B1, guardrail 5).

Order of checks (hard block wins):
1. Denylist — always blocked, no approval possible.
2. Allowlist — auto-approved.
3. Risky patterns — require explicit user approval.
4. Path policy — writes outside allowed roots require approval.

The gate is pure policy; the actual user prompt happens in the caller
(agent loop / CLI) via the `approver` callback.
"""

from __future__ import annotations

import re
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from overseer.errors import ApprovalDenied

# Commands that are always blocked, regardless of approval.
DENYLIST_PATTERNS: list[str] = [
    r"^\s*rm\s+-rf\s+/\s*$",  # rm -rf /
    r"^\s*rm\s+-rf\s+~?\s*$",  # rm -rf ~
    r"^\s*rm\s+-rf\s+\.\s*$",  # rm -rf .
    r"^\s*shutdown\b",
    r"^\s*reboot\b",
    r"^\s*halt\b",
    r"^\s*poweroff\b",
    r"^\s*:\(\)\s*\{",  # fork bomb
    r"^\s*dd\s+if=.*of=/dev/",  # dd to raw device
    r"^\s*mkfs\b",
    r"^\s*format\b",
    r"^\s*diskutil\s+erase",
    r"^\s*sudo\s+rm\b",
    r"^\s*git\s+push\s+--force",
    r"^\s*chmod\s+-R\s+777\s+/",
]

# Commands auto-approved without prompting.
ALLOWLIST_PATTERNS: list[str] = [
    r"^\s*(ls|pwd|echo|cat|head|tail|wc|date|whoami|uname|hostname)\b",
    r"^\s*(git\s+(status|diff|log|show|branch))\b",
    r"^\s*(python3?\s+-c\s+['\"][^'\"]*['\"])\b",
    r"^\s*(uv\s+run\s+pytest)\b",
    r"^\s*(uv\s+run\s+ruff\s+check)\b",
    r"^\s*(uv\s+run\s+mypy)\b",
    r"^\s*(uv\s+run\s+bandit)\b",
    r"^\s*(uv\s+run\s+pip-audit)\b",
    r"^\s*(gitleaks\s+detect)\b",
    r"^\s*(true|false|exit)\b",
]

# Patterns that require explicit approval.
RISKY_PATTERNS: list[str] = [
    r"\brm\b",
    r"\bmv\b",
    r"\bgit\s+push\b",
    r"\bgit\s+commit\b",
    r"\bgit\s+reset\b",
    r"\bgit\s+checkout\b",
    r"\bsudo\b",
    r"\bcurl\b",
    r"\bwget\b",
    r"\bbrew\s+install\b",
    r"\bbrew\s+uninstall\b",
    r"\bkill\b",
    r"\bpkill\b",
    r"\bkillall\b",
    r"\bchmod\b",
    r"\bchown\b",
    r"\bmkdir\b",
    r"\btouch\b",
    r"\btee\b",
    r"\b>\s*/",  # write to absolute path
    r"\b>\s*~",  # write to home
    r"\bpython3?\s+-m\s+pip\s+install\b",
    r"\buv\s+add\b",
    r"\buv\s+remove\b",
    r"\buv\s+sync\b",
    r"\buv\s+lock\b",
    r"\bexport\s+[A-Z_]+=",  # env mutation
    r"\bunset\b",
    r"\bsource\b",
    r"\b\.\s+[A-Za-z_./]",  # sourcing a script
    r"\bopen\b",
    r"\bdefaults\s+write\b",
    r"\bplutil\b",
    r"\bscp\b",
    r"\brsync\b",
    r"\bssh\b",
    r"\bpython3?\s+[^\s]+\s+[^\s]+",  # running a script with args
]


@dataclass
class ApprovalPolicy:
    """Policy engine. `approver` is called only for risky actions."""

    allowlist: list[str] = field(default_factory=lambda: ALLOWLIST_PATTERNS)
    denylist: list[str] = field(default_factory=lambda: DENYLIST_PATTERNS)
    risky: list[str] = field(default_factory=lambda: RISKY_PATTERNS)
    allowed_roots: list[Path] = field(default_factory=list)
    approver: Callable[[str, dict[str, Any]], bool] | None = None

    def _matches(self, patterns: list[str], text: str) -> bool:
        return any(re.search(p, text) for p in patterns)

    def check_command(self, command: str) -> str:
        """Classify a command: 'allow' | 'deny' | 'risky'."""
        if self._matches(self.denylist, command):
            return "deny"
        if self._matches(self.allowlist, command):
            return "allow"
        if self._matches(self.risky, command):
            return "risky"
        return "allow"  # unknown commands default to allow (documented)

    def check_path(self, path: Path) -> bool:
        """True if a write to `path` is inside an allowed root.

        Relative paths resolve against the first allowed root (matching how
        the file tools resolve them), not against the process CWD.
        """
        if not self.allowed_roots:
            return False
        if not path.is_absolute():
            path = self.allowed_roots[0] / path
        resolved = path.resolve()
        return any(resolved.is_relative_to(r.resolve()) for r in self.allowed_roots)

    def approve(self, tool_name: str, args: dict[str, Any]) -> bool:
        """Gate a tool call. Raises ApprovalDenied when blocked."""
        if tool_name == "terminal":
            command = args.get("command", "")
            verdict = self.check_command(command)
            if verdict == "deny":
                raise ApprovalDenied(f"command blocked by denylist: {command[:200]}")
            if verdict == "allow":
                return True
            # risky: ask the user
            if self.approver is not None:
                return self.approver(tool_name, args)
            raise ApprovalDenied(f"command requires approval: {command[:200]}")

        if tool_name in ("file_write", "file_patch"):
            path = Path(args.get("path", "")).expanduser()
            if self.check_path(path):
                return True
            if self.approver is not None:
                return self.approver(tool_name, args)
            raise ApprovalDenied(f"write outside allowed roots requires approval: {path}")

        # Other tools: no gate.
        return True
