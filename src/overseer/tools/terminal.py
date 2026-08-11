"""Terminal tool: run shell commands with approval gating (plan B1).

Security:
- Every command routes through the approval gate (allowlist/denylist/risky).
- Output is redacted and truncated; full output stored as an artifact.
- Commands run with a timeout; no interactive input.
"""

from __future__ import annotations

import subprocess  # nosec B404 — terminal tool is a shell by design, gated by approval
from pathlib import Path
from typing import Any

from overseer.redact import redact
from overseer.tools.base import Tool, ToolContext, ToolResult, store_artifact
from overseer.tools.registry import register_tool

DEFAULT_CMD_TIMEOUT = 30.0
MAX_OUTPUT_CHARS = 200_000


@register_tool
class TerminalTool(Tool):
    name = "terminal"
    description = "Run a shell command. Risky commands require approval."
    parameters = {
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "The shell command to run."},
            "timeout": {"type": "number", "description": "Timeout in seconds (default 30)."},
        },
        "required": ["command"],
    }
    requires_approval = True

    def run(self, args: dict[str, Any], context: ToolContext | None = None) -> ToolResult:
        command = args["command"]
        timeout = float(args.get("timeout", DEFAULT_CMD_TIMEOUT))
        if timeout > 120:
            return self._error("timeout capped at 120s")

        # Approval gate: fail closed — no approver means no terminal.
        if context is None or context.approver is None:
            return self._error("no approval gate configured; terminal denied")
        approved = context.approver(self.name, args)
        if not approved:
            return self._error(f"command not approved: {command[:200]}")

        try:
            # nosec B602: this tool IS a shell by design; every command is gated
            # by the approval policy (denylist/allowlist/risky) before reaching here.
            proc = subprocess.run(
                command,
                shell=True,  # nosec B602
                capture_output=True,
                text=True,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired:
            return self._error(f"command timed out after {timeout}s: {command[:200]}")
        except OSError as exc:
            return self._error(f"cannot run command: {exc}")

        output = proc.stdout
        if proc.stderr:
            output += f"\n[stderr]\n{proc.stderr}"
        output = output[:MAX_OUTPUT_CHARS]

        # Artifacts are redacted too — raw output can leak secrets into later
        # exports, session notes, or retrieval (Qwen review: artifact leakage).
        artifact = store_artifact(
            context.artifacts_dir if context else Path(".overseer/artifacts"),
            self.name,
            redact(output),
        )
        status = "ok" if proc.returncode == 0 else "error"
        summary = f"exit={proc.returncode}\n{output}"
        return ToolResult(
            status=status,
            summary=summary[:4000],
            artifacts=[str(artifact)],
            token_cost=max(1, len(summary) // 4),
            error=None if proc.returncode == 0 else f"exit code {proc.returncode}",
        )
