"""MCP server: expose Overseer's tools to external clients (plan B9).

Every tool call from an external client routes through the SAME approval
gate as a local call — denylist, allowlist, risky patterns, and path
containment all apply. External clients cannot bypass any of it.
"""

from __future__ import annotations

import json
import sys
from typing import Any

from overseer.approval import ApprovalPolicy
from overseer.tools import ToolContext, ToolRegistry, get_tool_class, registered_tools


class McpServer:
    """JSON-RPC server over stdio. Reads requests from stdin, writes to stdout.

    The server is approval-gated: each tool call is checked against the
    policy before dispatch. Denied calls return a structured error.
    """

    def __init__(
        self,
        policy: ApprovalPolicy,
        context: ToolContext | None = None,
        stdin: Any = None,
        stdout: Any = None,
    ) -> None:
        self.policy = policy
        self.context = context or ToolContext()
        self.tools = ToolRegistry()
        for name in registered_tools():
            self.tools.add(get_tool_class(name)())
        self.stdin = stdin or sys.stdin
        self.stdout = stdout or sys.stdout

    def _handle(self, req: dict[str, Any]) -> dict[str, Any]:
        method = req.get("method", "")
        if method == "tools/list":
            return {
                "tools": [
                    {
                        "name": t["function"]["name"],
                        "description": t["function"]["description"],
                        "inputSchema": t["function"]["parameters"],
                    }
                    for t in self.tools.specs()
                ]
            }
        if method == "tools/call":
            params = req.get("params", {})
            name = str(params.get("name", ""))
            args = params.get("arguments", {}) or {}
            return self._dispatch(name, args)
        return {"error": {"code": -32601, "message": f"unknown method: {method}"}}

    def _dispatch(self, name: str, args: dict[str, Any]) -> dict[str, Any]:
        """Dispatch with the approval gate. Denied -> structured error."""
        try:
            tool = self.tools.get(name)
        except Exception as exc:  # unknown tool
            return {"error": {"code": -32602, "message": str(exc)}}
        if tool.requires_approval:
            try:
                self.policy.approve(name, args)
            except Exception as exc:
                return {
                    "error": {
                        "code": -32000,
                        "message": f"denied by approval gate: {exc}",
                    }
                }
        result = tool.run(args, self.context)
        return {
            "content": [{"type": "text", "text": result.to_message()}],
            "isError": result.status == "error",
        }

    def serve_forever(self) -> None:
        """Read JSON-RPC requests until EOF."""
        for line in self.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                req = json.loads(line)
            except json.JSONDecodeError:
                self._write({"error": {"code": -32700, "message": "parse error"}})
                continue
            resp = {"jsonrpc": "2.0", "id": req.get("id"), **self._handle(req)}
            self._write(resp)

    def _write(self, obj: dict[str, Any]) -> None:
        self.stdout.write(json.dumps(obj) + "\n")
        self.stdout.flush()
