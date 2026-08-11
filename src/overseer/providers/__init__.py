"""Provider package: base adapter, registry, OpenAI-compatible impl."""

from overseer.providers.base import ChatMessage, ChatResult, Provider, ToolCall
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
    "ToolCall",
    "get_provider_class",
    "register_provider",
    "registered_providers",
]
