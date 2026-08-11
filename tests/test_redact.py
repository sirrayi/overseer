"""Redaction tests: secrets must never reach logs, exports, or notes."""

from __future__ import annotations

from overseer.redact import redact


def test_openai_key_redacted():
    out = redact("key=sk-1234567890abcdef1234567890abcdef")
    assert "sk-1234567890abcdef1234567890abcdef" not in out
    assert "sk-***REDACTED***" in out


def test_anthropic_key_redacted_before_generic():
    """sk-ant- must match the specific pattern, not the generic sk- (MAJOR-10)."""
    out = redact("key=sk-ant-abcdefghijklmnopqrstuvwxyz123456")
    assert "sk-ant-abcdefghijklmnopqrstuvwxyz123456" not in out
    assert "sk-ant-***REDACTED***" in out


def test_github_token_redacted():
    out = redact("token=ghp_1234567890abcdefghijklmnopqrstuvwxyz")
    assert "ghp_1234567890abcdefghijklmnopqrstuvwxyz" not in out
    assert "gh***_REDACTED***" in out


def test_aws_key_redacted():
    out = redact("aws=AKIA1234567890ABCDEF")
    assert "AKIA1234567890ABCDEF" not in out
    assert "AKIA***REDACTED***" in out


def test_bearer_token_redacted():
    out = redact("Authorization: Bearer abcdefghijklmnopqrstuvwxyz1234567890")
    assert "abcdefghijklmnopqrstuvwxyz1234567890" not in out
    assert "Bearer ***REDACTED***" in out


def test_private_key_redacted():
    out = redact("-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----")
    assert "MIIEowIBAAKCAQEA" not in out
    assert "PRIVATE KEY REDACTED" in out


def test_assignment_style_redacted():
    out = redact("API_KEY=abcdefghijklmnopqrstuvwxyz123456")
    assert "abcdefghijklmnopqrstuvwxyz123456" not in out


def test_plain_text_untouched():
    text = "hello world, nothing secret here"
    assert redact(text) == text


def test_empty_input():
    assert redact(None) == ""
    assert redact("") == ""
