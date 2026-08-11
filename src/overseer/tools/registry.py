"""Self-registering tool registry (plan B1: self-registering tool schemas).

Tools register via the @register_tool decorator. The registry exposes
specs (for the model) and dispatch (for the agent loop).
"""

from __future__ import annotations

from typing import Any

from overseer.errors import ToolError
from overseer.tools.base import Tool, ToolResult

_REGISTRY: dict[str, type[Tool]] = {}


def register_tool(cls: type[Tool]) -> type[Tool]:
    """Class decorator: self-register a tool class by its `name`."""
    _REGISTRY[cls.name] = cls
    return cls


def get_tool_class(name: str) -> type[Tool]:
    try:
        return _REGISTRY[name]
    except KeyError:
        raise ToolError(f"unknown tool: {name!r} (registered: {sorted(_REGISTRY)})") from None


def registered_tools() -> list[str]:
    return sorted(_REGISTRY)


class ToolRegistry:
    """Holds tool instances and dispatches calls."""

    def __init__(self) -> None:
        self._instances: dict[str, Tool] = {}

    def add(self, tool: Tool) -> None:
        self._instances[tool.name] = tool

    def get(self, name: str) -> Tool:
        try:
            return self._instances[name]
        except KeyError:
            raise ToolError(f"tool not registered: {name!r}") from None

    def specs(self) -> list[dict[str, Any]]:
        return [t.spec() for t in self._instances.values()]

    def dispatch(self, name: str, args: dict[str, Any], context: Any | None = None) -> ToolResult:
        """Run a tool by name. Raises ToolError for unknown tools."""
        return self.get(name).run(args, context=context)
