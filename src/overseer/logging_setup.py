"""Structured logging to a safe local directory (plan: logging writes to a
safe local dir; secrets redacted; structured logs with session/trace IDs)."""

from __future__ import annotations

import logging
import sys
from pathlib import Path

from overseer.redact import redact


class RedactingFormatter(logging.Formatter):
    """Formatter that redacts secrets from every record before output."""

    def format(self, record: logging.LogRecord) -> str:
        original = record.getMessage()
        record.msg = redact(original)
        record.args = ()
        return super().format(record)


def setup_logging(log_dir: str | Path, level: int = logging.INFO) -> Path:
    """Configure root overseer logger with console + file handlers.

    Returns the log file path. Creates the log directory if needed.
    """
    log_path = Path(log_dir)
    log_path.mkdir(parents=True, exist_ok=True)
    log_file = log_path / "overseer.log"

    formatter = RedactingFormatter("%(asctime)s %(levelname)s [%(name)s] %(message)s")

    file_handler = logging.FileHandler(log_file, encoding="utf-8")
    file_handler.setFormatter(formatter)

    console_handler = logging.StreamHandler(sys.stderr)
    console_handler.setFormatter(formatter)

    root = logging.getLogger("overseer")
    root.setLevel(level)
    # Avoid duplicate handlers on repeated setup (tests, reloads).
    root.handlers.clear()
    root.addHandler(file_handler)
    root.addHandler(console_handler)
    root.propagate = False

    return log_file
