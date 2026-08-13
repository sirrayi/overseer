"""MCP package (plan B9): client + server with strict permission scoping."""

from overseer.mcp.client import McpClient, McpToolSpec, mcp_tool_result
from overseer.mcp.server import McpServer

__all__ = ["McpClient", "McpServer", "McpToolSpec", "mcp_tool_result"]
