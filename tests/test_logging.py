"""Logging tests: redaction incl. exception text, safe dir creation."""

from __future__ import annotations

import logging

from overseer.logging_setup import RedactingFormatter, setup_logging


def test_exception_text_redacted(tmp_path):
    """Secrets in exception text must not leak (MAJOR-09)."""
    log_file = setup_logging(tmp_path / "logs")
    logger = logging.getLogger("overseer.test")
    try:
        raise RuntimeError("failed with key sk-1234567890abcdef1234567890abcdef")
    except RuntimeError:
        logger.exception("boom")
    text = log_file.read_text(encoding="utf-8")
    assert "sk-1234567890abcdef1234567890abcdef" not in text
    assert "sk-***REDACTED***" in text


def test_log_dir_created_and_expanded(tmp_path):
    """Log dir must be created and ~ expanded (MAJOR-08)."""
    log_file = setup_logging(tmp_path / "nested" / "logs")
    assert log_file.parent.is_dir()
    assert log_file.name == "overseer.log"


def test_formatter_redacts_message():
    fmt = RedactingFormatter("%(message)s")
    record = logging.LogRecord(
        "t", logging.INFO, "f", 1, "key=ghp_1234567890abcdefghijklmnopqrstuvwxyz", None, None
    )
    out = fmt.format(record)
    assert "ghp_1234567890abcdefghijklmnopqrstuvwxyz" not in out
