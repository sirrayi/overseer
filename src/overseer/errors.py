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


class ProviderError(OverseerError):
    """Model provider call failed (network, auth, malformed response)."""


class ToolError(OverseerError):
    """A tool failed to execute (missing file, bad args, command failure)."""


class ApprovalDenied(OverseerError):
    """An action was blocked by the approval gate (denylist or user denial)."""


class Timeout(OverseerError):
    """A provider call or tool execution exceeded its time budget."""


class BudgetExceeded(OverseerError):
    """The agent loop exceeded its token/cost budget."""
