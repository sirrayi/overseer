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


@dataclass
class StreamEvent:
    """One event from a streaming provider response.

    - type="delta": incremental text content.
    - type="tool_call_delta": a tool call (possibly partial; the final event
      for a call carries the complete accumulated arguments).
    - type="done": stream finished; carries usage when available.
    """

    type: str  # "delta" | "tool_call_delta" | "done"
    content: str = ""
    tool_call: ToolCall | None = None
    usage: dict[str, int] = field(default_factory=dict)


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

    def stream(
        self,
        messages: list[ChatMessage],
        tools: list[dict[str, Any]] | None = None,
        **kwargs: Any,
    ) -> Any:
        """Stream a response as an iterator of StreamEvent.

        Must be safe against: partial tool-call deltas, malformed SSE/JSON
        (skip, don't crash), timeouts, and cancellation (closing the iterator
        releases the connection). Errors raise ProviderError with redacted
        messages. Subclasses implement this.
        """
        raise NotImplementedError

    def close(self) -> None:
        """Release any resources (sessions, connections)."""
