"""Provider registry with fallback chains (plan B1: fallback chains).

A chain is an ordered list of provider names. If the first provider fails
(ProviderError), the next is tried, and so on. The registry is populated by
self-registration: providers call `register_provider` at import time.
"""

from __future__ import annotations

from typing import Any

from overseer.errors import ProviderError
from overseer.providers.base import ChatMessage, ChatResult, Provider

_REGISTRY: dict[str, type[Provider]] = {}


def register_provider(cls: type[Provider]) -> type[Provider]:
    """Class decorator: self-register a provider class by its `name`."""
    _REGISTRY[cls.name] = cls
    return cls


def get_provider_class(name: str) -> type[Provider]:
    try:
        return _REGISTRY[name]
    except KeyError:
        raise ProviderError(
            f"unknown provider: {name!r} (registered: {sorted(_REGISTRY)})"
        ) from None


def registered_providers() -> list[str]:
    return sorted(_REGISTRY)


class ProviderRegistry:
    """Holds provider instances and resolves fallback chains."""

    def __init__(self) -> None:
        self._instances: dict[str, Provider] = {}

    def add(self, name: str, provider: Provider) -> None:
        self._instances[name] = provider

    def get(self, name: str) -> Provider:
        try:
            return self._instances[name]
        except KeyError:
            raise ProviderError(f"provider not configured: {name!r}") from None

    def complete_with_fallback(
        self,
        chain: list[str],
        messages: list[ChatMessage],
        **kwargs: Any,
    ) -> tuple[ChatResult, str]:
        """Try each provider in the chain in order; return (result, provider_name).

        Raises ProviderError only if every provider in the chain fails.
        """
        if not chain:
            raise ProviderError("fallback chain is empty")
        errors: list[str] = []
        for name in chain:
            try:
                result = self.get(name).complete(messages, **kwargs)
                return result, name
            except ProviderError as exc:
                errors.append(f"{name}: {exc}")
        raise ProviderError("all providers failed: " + "; ".join(errors))
