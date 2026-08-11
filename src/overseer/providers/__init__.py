"""Provider package: base adapter, registry, OpenAI-compatible impl."""

# Import the concrete provider so its @register_provider decorator runs.
from overseer.providers import openai_compat  # noqa: E402,F401
from overseer.providers.base import ChatMessage, ChatResult, Provider, StreamEvent, ToolCall
from overseer.providers.registry import (
    ProviderRegistry,
    get_provider_class,
    register_provider,
    registered_providers,
)

__all__ = [
    "ChatMessage",
    "ChatResult",
    "Provider",
    "ProviderRegistry",
    "StreamEvent",
    "ToolCall",
    "get_provider_class",
    "register_provider",
    "registered_providers",
]
