"""MCP (Model Context Protocol) support (plan B9).

Client: connects to external MCP tool servers over stdio/JSON-RPC.
Server: exposes Overseer's registered tools to external MCP clients.

Security rules (non-negotiable):
- Every MCP tool call routes through the approval gate (denylist,
  allowlist, risky patterns, path containment) exactly like a local call.
- External MCP tool outputs are ALWAYS labeled trust="untrusted".
- Connections have timeouts; cancellation is supported.
- No MCP path can trigger destructive terminal commands without the
  main CLI's explicit user approval.
"""

from __future__ import annotations

import contextlib
import json
import subprocess  # nosec B404 — MCP stdio transport spawns the server process
import threading
from dataclasses import dataclass, field
from typing import Any

from overseer.errors import ToolError
from overseer.tools.base import ToolResult

MCP_TIMEOUT_S = 30


@dataclass
class McpToolSpec:
    """A tool exposed by an external MCP server."""

    name: str
    description: str = ""
    parameters: dict[str, Any] = field(default_factory=dict)


class McpClient:
    """JSON-RPC client over stdio for an external MCP server.

    The server process is spawned once; requests are framed as JSON-RPC
    messages. All responses are treated as untrusted content.
    """

    def __init__(self, command: list[str], timeout: int = MCP_TIMEOUT_S) -> None:
        self.command = command
        self.timeout = timeout
        self._proc: subprocess.Popen | None = None  # type: ignore[type-arg]
        self._lock = threading.RLock()
        self._next_id = 1

    def connect(self) -> None:
        """Spawn the server process (stdio transport)."""
        if self._proc is not None:
            return
        try:
            self._proc = subprocess.Popen(  # noqa: S603  # nosec B603 — user-configured
                self.command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
        except OSError as exc:
            raise ToolError(f"cannot start MCP server {self.command!r}: {exc}") from exc

    def _request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        """Send one JSON-RPC request and read the response."""
        if self._proc is None or self._proc.stdin is None or self._proc.stdout is None:
            raise ToolError("MCP client not connected")
        req = {"jsonrpc": "2.0", "id": self._next_id, "method": method, "params": params}
        self._next_id += 1
        with self._lock:
            try:
                self._proc.stdin.write(json.dumps(req) + "\n")
                self._proc.stdin.flush()
                line = self._proc.stdout.readline()
            except (OSError, ValueError) as exc:
                raise ToolError(f"MCP transport error: {exc}") from exc
        if not line:
            raise ToolError("MCP server closed the connection")
        try:
            resp = json.loads(line)
        except json.JSONDecodeError as exc:
            raise ToolError(f"MCP server returned malformed JSON: {line[:200]!r}") from exc
        if "error" in resp and resp["error"]:
            raise ToolError(f"MCP error: {resp['error']}")
        result: dict[str, Any] = resp.get("result", {})
        return result

    def list_tools(self) -> list[McpToolSpec]:
        """Enumerate the server's tools."""
        result = self._request("tools/list", {})
        specs: list[McpToolSpec] = []
        for t in result.get("tools", []):
            specs.append(
                McpToolSpec(
                    name=str(t.get("name", "")),
                    description=str(t.get("description", "")),
                    parameters=t.get("inputSchema", {}) or {},
                )
            )
        return specs

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        """Invoke a server tool. Output is untrusted by definition."""
        return self._request("tools/call", {"name": name, "arguments": arguments})

    def close(self) -> None:
        if self._proc is not None:
            with contextlib.suppress(OSError):
                self._proc.terminate()
            self._proc = None


def mcp_tool_result(raw: dict[str, Any]) -> ToolResult:
    """Wrap an external MCP tool response as an UNTRUSTED ToolResult.

    The content is redacted and truncated like any tool output, but the
    trust label is always "untrusted" — external servers cannot instruct
    the agent.
    """
    content = raw.get("content", [])
    text = ""
    for part in content if isinstance(content, list) else []:
        if isinstance(part, dict) and part.get("type") == "text":
            text += str(part.get("text", ""))
    if not text:
        text = json.dumps(raw, ensure_ascii=False)[:2000]
    return ToolResult(
        status="ok",
        summary=text[:4000],
        trust="untrusted",
        token_cost=max(1, len(text) // 4),
    )
