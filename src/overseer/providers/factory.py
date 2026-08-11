"""Provider factory: build a provider instance from config (plan B2).

No guessed endpoints: the factory only instantiates registered provider
classes with the config's base_url/model/api_key_env. Unknown provider
names raise ProviderError with the registered list.
"""

from __future__ import annotations

from typing import Any

from overseer.config import ProviderConfig
from overseer.errors import ProviderError
from overseer.providers.registry import get_provider_class


def build_provider(cfg: ProviderConfig) -> Any:
    """Instantiate a provider from config. Raises ProviderError on unknown."""
    cls = get_provider_class(cfg.name)
    if cls.name == "openai-compat":
        if not cfg.base_url:
            raise ProviderError(
                f"provider {cfg.name!r} requires base_url in config (e.g. "
                "https://api.example.com/v1)"
            )
        return cls(
            base_url=cfg.base_url,
            model=cfg.model,
            api_key_env=cfg.api_key_env,
        )
    # Other registered providers construct from the same fields.
    return cls(
        base_url=cfg.base_url or "",
        model=cfg.model,
        api_key_env=cfg.api_key_env,
    )
