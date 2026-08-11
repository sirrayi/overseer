"""Tool base class and structured result model (plan B1).

Tools return structured results: status, summary, artifacts, token cost.
Full output is stored as an artifact; the model sees a truncated summary.
All output passes through redact() before reaching the model or logs.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any

from overseer.redact import redact

# Max chars of tool output shown to the model (plan: truncate, store full).
SUMMARY_LIMIT = 4000


@dataclass
class ToolContext:
    """Execution context passed to tools by the agent loop."""

    allowed_roots: list[Path] = field(default_factory=list)  # resolved write roots
    artifacts_dir: Path = Path(".overseer/artifacts")
    approver: Callable[[str, dict[str, Any]], bool] | None = None  # (tool, args) -> approved


def store_artifact(artifacts_dir: Path, tool_name: str, content: str) -> Path:
    """Persist full tool output to the artifacts dir; return the path."""
    artifacts_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now().strftime("%Y%m%d-%H%M%S-%f")
    path = artifacts_dir / f"{tool_name}-{ts}.txt"
    path.write_text(content, encoding="utf-8")
    return path


@dataclass
class ToolResult:
    """Structured result of a tool execution.

    trust: provenance label for the model context.
      - "user": direct user instruction (never produced by tools).
      - "project": content from the user's own project/vault.
      - "tool_output": output of a tool the agent ran (evidence, not instructions).
      - "untrusted": content from external/unverified sources (web, unknown repos).
    denied: set ONLY by the approval gate (structured, never string-matched).
    """

    status: str  # "ok" | "error"
    summary: str  # truncated, redacted text for the model
    artifacts: list[str] = field(default_factory=list)  # paths to full outputs
    token_cost: int = 0  # approximate tokens consumed by this result
    error: str | None = None  # error message when status == "error"
    trust: str = "tool_output"  # user | project | tool_output | untrusted
    denied: bool = False  # True only when the approval gate blocked this call

    def to_message(self) -> str:
        """Render the result as a tool-role message for the model."""
        if self.denied:
            return f"ERROR: action was denied by the approval gate: {self.error or self.summary}"
        if self.status == "error":
            return f"ERROR: {self.error or self.summary}"
        return self.summary


class Tool:
    """Base class for tools. Subclasses set `name`, `description`, `parameters`."""

    name: str = "base"
    description: str = ""
    # JSON Schema for arguments (OpenAI tools format).
    parameters: dict[str, Any] = {"type": "object", "properties": {}, "required": []}
    # Whether this tool needs approval-gate review before running.
    requires_approval: bool = False

    def run(self, args: dict[str, Any], context: Any | None = None) -> ToolResult:
        """Execute the tool. Subclasses implement this."""
        raise NotImplementedError

    def spec(self) -> dict[str, Any]:
        """OpenAI tools-format schema for this tool."""
        return {
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            },
        }

    def _result(self, text: str, artifacts: list[str] | None = None) -> ToolResult:
        """Build a redacted, truncated result."""
        safe = redact(text)
        summary = safe[:SUMMARY_LIMIT]
        return ToolResult(
            status="ok",
            summary=summary,
            artifacts=artifacts or [],
            token_cost=max(1, len(summary) // 4),
        )

    def _error(self, message: str) -> ToolResult:
        return ToolResult(
            status="error",
            summary=redact(message)[:SUMMARY_LIMIT],
            error=redact(message),
            token_cost=1,
        )
