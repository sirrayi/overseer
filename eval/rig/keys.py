"""Provider API keys — the one place the rig maps a provider to its key.

The harness has no key of its own: every provider reads its own
provider-specific variable, chosen by OVERSEER_PROVIDER (default
"opencode"). The same names, in the same order, are what `overseer exec`
resolves (crates/overseer-cli/src/provider.rs `key_names`), so the binary
inherits them unchanged.
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

# Names the binary also accepts, after the KEY_ENV one.
KEY_FALLBACKS = {
    "gemini": ("GEMINI_API_KEY",),
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


def key_names(name: str | None = None) -> tuple[str, ...]:
    """Every env var that holds the key for `name`, in lookup order."""
    p = name or provider()
    return (key_env(p), *KEY_FALLBACKS.get(p, ()))


def key_label(name: str | None = None) -> str:
    """The key names for messages: "A" or "A or B"."""
    return " or ".join(key_names(name))


def api_key(name: str | None = None) -> str | None:
    """The provider's key, or None when every name is unset or empty."""
    return next(
        (v for n in key_names(name) if (v := os.environ.get(n))), None
    )


def require(name: str | None = None) -> str:
    key = api_key(name)
    if key is None:
        raise RuntimeError(f"{key_label(name)} is not set")
    return key
