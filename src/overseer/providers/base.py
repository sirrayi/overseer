"""Provider abstraction: base adapter, message model, tool-call model.

Plan B1: provider-agnostic adapter supporting OpenAI-compatible APIs
(Ollama Cloud, DeepSeek, local Ollama) with a registry and fallback chains.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Literal

Role = Literal["system", "user", "assistant", "tool"]


@dataclass
class ToolCall:
    """A structured tool invocation requested by the model."""

    id: str
    name: str
    arguments: dict[str, Any]


@dataclass
class ChatMessage:
    """One message in the conversation.

    - role="assistant" with tool_calls set: the model's tool-call turn
      (must be echoed back to the API on the next request).
    - role="tool": a tool result; `tool_call_id` links it to the call.
    """

    role: Role
    content: str = ""
    tool_calls: list[ToolCall] | None = None
    tool_call_id: str | None = None


@dataclass
class ChatResult:
    """A model response: either final text or one or more tool calls."""

    content: str = ""
    tool_calls: list[ToolCall] = field(default_factory=list)
    usage: dict[str, int] = field(default_factory=dict)  # prompt/completion/total tokens

    @property
    def has_tool_calls(self) -> bool:
        return bool(self.tool_calls)


class Provider:
    """Base class for model providers. Subclasses implement `complete`."""

    name: str = "base"

    def complete(
        self,
        messages: list[ChatMessage],
        tools: list[dict[str, Any]] | None = None,
        stream_callback: Any | None = None,
        **kwargs: Any,
    ) -> ChatResult:
        """Send messages, return the model's response.

        Raises ProviderError on network/auth/malformed-response failures.
        """
        raise NotImplementedError

    def close(self) -> None:
        """Release any resources (sessions, connections)."""
