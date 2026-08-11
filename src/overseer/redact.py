"""Secret redaction (plan: secret redaction started in B0, continuous after).

Redacts common secret patterns from logs, exports, and session notes.
"""

from __future__ import annotations

import re

# Ordered: longer/more specific patterns first so they win over generic ones.
_PATTERNS: list[tuple[str, str]] = [
    # Anthropic-style keys (more specific — must precede generic sk-)
    (r"sk-ant-[A-Za-z0-9_-]{16,}", "sk-ant-***REDACTED***"),
    # OpenAI-style keys
    (r"sk-[A-Za-z0-9_-]{16,}", "sk-***REDACTED***"),
    # GitHub tokens
    (r"gh[pousr]_[A-Za-z0-9]{20,}", "gh***_REDACTED***"),
    # AWS access keys
    (r"AKIA[0-9A-Z]{16}", "AKIA***REDACTED***"),
    # Generic bearer tokens
    (r"Bearer\s+[A-Za-z0-9._~+/=-]{20,}", "Bearer ***REDACTED***"),
    # Generic API keys in assignment form
    (
        r"(?i)(api[_-]?key|token|secret|password)\s*[=:]\s*['\"]?[A-Za-z0-9._~+/=-]{12,}",
        r"\1=***REDACTED***",
    ),
    # Private keys
    (
        r"-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----"
        r".*?-----END (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----",
        "***PRIVATE KEY REDACTED***",
    ),
]

_COMPILED: list[tuple[re.Pattern[str], str]] = [
    (re.compile(pattern, re.DOTALL), repl) for pattern, repl in _PATTERNS
]


def redact(text: str | None) -> str:
    """Replace known secret patterns with redaction markers."""
    if not text:
        return ""
    for pattern, repl in _COMPILED:
        text = pattern.sub(repl, text)
    return text
