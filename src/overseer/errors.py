"""Overseer error taxonomy (plan Part 42.6: explicit error taxonomy)."""

from __future__ import annotations


class OverseerError(Exception):
    """Base class for all overseer errors."""


class ConfigError(OverseerError):
    """Configuration is missing, invalid, or unreadable."""


class VaultError(OverseerError):
    """Vault is missing, invalid, or unwritable."""


class ProviderError(OverseerError):
    """Model provider configuration is missing or invalid."""


class RedactionError(OverseerError):
    """Redaction failed (should never happen; defensive)."""
