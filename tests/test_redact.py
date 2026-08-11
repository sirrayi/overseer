"""Redaction tests: secrets must never reach logs, exports, or notes."""

from __future__ import annotations

from overseer.redact import redact


def test_openai_key_redacted():
    out = redact("key=sk-1234567890abcdef1234567890abcdef")
    assert "sk-1234567890abcdef1234567890abcdef" not in out
    assert "sk-***REDACTED***" in out


def test_github_token_redacted():
    out = redact("token ghp_1234567890abcdefghijklmnopqrstuvwxyz123")
    assert "ghp_1234567890" not in out
    assert "gh***_REDACTED***" in out


def test_bearer_redacted():
    out = redact("Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789")
    assert "abcdefghijklmnopqrstuvwxyz0123456789" not in out
    assert "Bearer ***REDACTED***" in out


def test_private_key_redacted():
    key = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----"
    out = redact(key)
    assert "MIIEowIBAAKCAQEA" not in out
    assert "PRIVATE KEY REDACTED" in out


def test_plain_text_untouched():
    text = "hello world, nothing secret here"
    assert redact(text) == text


def test_empty_input():
    assert redact("") == ""
    assert redact(None) == ""
