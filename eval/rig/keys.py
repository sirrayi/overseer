"""Provider API keys — the one place the rig maps a provider to its key.

The harness has no key of its own: every provider reads its own
provider-specific variable, chosen by OVERSEER_PROVIDER (default
"opencode"). The same name is what `overseer exec` resolves, so the
binary inherits it unchanged.
"""

from __future__ import annotations

import os

DEFAULT_PROVIDER = "opencode"

KEY_ENV = {
    "opencode": "OPENCODE_API_KEY",
    "anthropic": "ANTHROPIC_API_KEY",
    "openai": "OPENAI_API_KEY",
    "gemini": "GOOGLE_API_KEY",
}


def provider() -> str:
    return os.environ.get("OVERSEER_PROVIDER", DEFAULT_PROVIDER)


def key_env(name: str | None = None) -> str:
    """The env var holding the key for `name` (default: OVERSEER_PROVIDER)."""
    p = name or provider()
    try:
        return KEY_ENV[p]
    except KeyError:
        raise ValueError(
            f"unknown OVERSEER_PROVIDER {p!r} (want {', '.join(KEY_ENV)})"
        ) from None


def api_key(name: str | None = None) -> str | None:
    """The provider's key, or None when unset or empty."""
    return os.environ.get(key_env(name)) or None


def require(name: str | None = None) -> str:
    key = api_key(name)
    if key is None:
        raise RuntimeError(f"{key_env(name)} is not set")
    return key
