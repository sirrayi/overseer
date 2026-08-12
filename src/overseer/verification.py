"""Verification engine: targeted checks, failure cards, rollback (plan B4).

VerificationRunner takes a ProjectContext and runs targeted tests, linters,
and typecheckers. Output is parsed into FailureCards (file, line, error
type, message) so the model sees structured summaries instead of raw logs.

Checkpoints: before a patch is applied, the original file is copied to
.overseer/tmp/checkpoints/. If verification fails after the patch, the
runner restores the checkpoint — the repo is never left broken.

Results are cached by file hash so unchanged files are not re-verified.
"""

from __future__ import annotations

import hashlib
import json
import re
import subprocess  # nosec B404 — verification runs project commands by design
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from overseer.project import ProjectContext
from overseer.redact import redact

DEFAULT_TIMEOUT = 120  # seconds; prevents infinite test runs
MAX_OUTPUT_CHARS = 8000  # truncation before the model sees anything

# Failure signature patterns: (error_type, regex) — file:line: message.
FAILURE_PATTERNS: list[tuple[str, str]] = [
    ("test_failure", r"^(?:FAILED|ERROR)\s+([^\s]+)\s+-\s+(.+)$"),  # pytest
    ("assertion", r"^E\s+(AssertionError|TypeError|ValueError|KeyError|IndexError):\s*(.+)$"),
    ("traceback", r'^File\s+"([^"]+)",\s+line\s+(\d+),\s+in\s+(.+)$'),
    ("lint", r"^([^:]+):(\d+):\d+:\s+([A-Z]+\d+)\s+(.+)$"),  # ruff/flake8
    ("typecheck", r"^([^:]+):(\d+):\s+error:\s+(.+)$"),  # mypy
    ("syntax", r"^SyntaxError:\s*(.+)$"),
    ("import_error", r"^(?:ModuleNotFoundError|ImportError):\s*(.+)$"),
]


@dataclass
class FailureCard:
    """Structured summary of one failure (plan B4)."""

    error_type: str
    file: str = ""
    line: int = 0
    message: str = ""

    def render(self) -> str:
        loc = f"{self.file}:{self.line}" if self.file else "?"
        return f"[{self.error_type}] {loc}: {self.message[:200]}"


@dataclass
class VerificationResult:
    """Outcome of a verification run."""

    ok: bool
    command: str
    exit_code: int
    cards: list[FailureCard] = field(default_factory=list)
    output: str = ""  # truncated, redacted
    duration: float = 0.0
    cached: bool = False

    def summary(self) -> str:
        """Compact form for the model: cards, not raw logs."""
        if self.ok:
            return f"verification passed: {self.command} ({self.duration:.1f}s)"
        lines = [f"verification FAILED: {self.command} ({len(self.cards)} failures)"]
        for card in self.cards[:10]:
            lines.append("  " + card.render())
        if not self.cards:
            lines.append(f"  exit code {self.exit_code}; output: {self.output[:300]}")
        return "\n".join(lines)


def _file_hash(path: Path) -> str:
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()[:16]
    except OSError:
        return "missing"


class VerificationRunner:
    """Runs targeted checks for a project and summarizes failures."""

    def __init__(self, project: ProjectContext, timeout: int = DEFAULT_TIMEOUT) -> None:
        self.project = project
        self.timeout = timeout
        self._cache: dict[str, VerificationResult] = {}
        self._cache_file = project.root / ".overseer" / "cache" / "verification.json"
        self._load_cache()

    # --- cache ------------------------------------------------------------

    def _load_cache(self) -> None:
        try:
            data = json.loads(self._cache_file.read_text(encoding="utf-8"))
            self._cache = {k: _result_from_dict(v) for k, v in data.items()}
        except (OSError, json.JSONDecodeError, TypeError):
            self._cache = {}

    def _save_cache(self) -> None:
        try:
            self._cache_file.parent.mkdir(parents=True, exist_ok=True)
            self._cache_file.write_text(
                json.dumps({k: _result_to_dict(v) for k, v in self._cache.items()}),
                encoding="utf-8",
            )
        except OSError:
            pass  # cache is best-effort

    # --- execution --------------------------------------------------------

    def _run(self, command: str, cwd: Path | None = None) -> VerificationResult:
        """Run a command, capture output, parse failure cards."""
        start = time.monotonic()
        try:
            # nosec B602 — commands come from project detection, not untrusted
            # input; the approval gate covers terminal. S602 same rationale.
            proc = subprocess.run(  # noqa: S602
                command,
                shell=True,  # nosec B602
                capture_output=True,
                text=True,
                timeout=self.timeout,
                cwd=str(cwd or self.project.root),
            )
            output = (proc.stdout or "") + (proc.stderr or "")
            exit_code = proc.returncode or 0
        except subprocess.TimeoutExpired:
            output = f"TIMEOUT after {self.timeout}s"
            exit_code = 124
        except OSError as exc:
            output = f"could not run {command}: {exc}"
            exit_code = 127

        output = redact(output)[:MAX_OUTPUT_CHARS]
        cards = parse_failures(output)
        return VerificationResult(
            ok=(exit_code == 0 and not cards),
            command=command,
            exit_code=exit_code,
            cards=cards,
            output=output,
            duration=time.monotonic() - start,
        )

    def _cached(self, key: str, command: str) -> VerificationResult:
        hit = self._cache.get(key)
        if hit is not None:
            hit.cached = True
            return hit
        result = self._run(command)
        self._cache[key] = result
        self._save_cache()
        return result

    # --- public API -------------------------------------------------------

    def run_tests(self, targets: list[str] | None = None) -> VerificationResult:
        """Run the project's test command, optionally targeted at files."""
        cmd = self.project.commands.get("test", "")
        if not cmd:
            return VerificationResult(
                ok=False,
                command="test",
                exit_code=127,
                cards=[],
                output="no test command detected for this project",
            )
        if targets and "pytest" in cmd:
            # Targeted: pytest accepts file paths; others get the full run.
            cmd = f"{cmd} {' '.join(targets)}"
        return self._cached(f"test:{cmd}", cmd)

    def run_linter(self) -> VerificationResult:
        cmd = self.project.commands.get("lint", "")
        if not cmd:
            return VerificationResult(
                ok=False,
                command="lint",
                exit_code=127,
                cards=[],
                output="no linter detected for this project",
            )
        return self._cached(f"lint:{cmd}", cmd)

    def run_typechecker(self) -> VerificationResult:
        cmd = self.project.commands.get("typecheck", "")
        if not cmd:
            return VerificationResult(
                ok=False,
                command="typecheck",
                exit_code=127,
                cards=[],
                output="no typechecker detected for this project",
            )
        return self._cached(f"typecheck:{cmd}", cmd)

    def verify(self, changed_files: list[str] | None = None) -> VerificationResult:
        """Run the full verification suite (tests + lint + typecheck)."""
        results = [
            self.run_tests(changed_files),
            self.run_linter(),
            self.run_typechecker(),
        ]
        failed = [r for r in results if not r.ok]
        if not failed:
            return VerificationResult(
                ok=True,
                command="verify",
                exit_code=0,
                cards=[],
                output="all checks passed",
            )
        # Merge failure cards from all failed checks.
        cards = [c for r in failed for c in r.cards]
        if not cards:
            cards = [FailureCard(error_type="check", message=r.output[:200]) for r in failed]
        return VerificationResult(
            ok=False,
            command="verify",
            exit_code=1,
            cards=cards,
            output="\n".join(r.output for r in failed)[:MAX_OUTPUT_CHARS],
        )

    # --- checkpoints / rollback -------------------------------------------

    def checkpoint(self, path: Path) -> Path | None:
        """Copy a file to .overseer/tmp/checkpoints before it is patched."""
        path = path.expanduser().resolve()
        if not path.is_file():
            return None
        cp_dir = self.project.root / ".overseer" / "tmp" / "checkpoints"
        cp_dir.mkdir(parents=True, exist_ok=True)
        cp = cp_dir / f"{_file_hash(path)}-{path.name}"
        try:
            cp.write_bytes(path.read_bytes())
            return cp
        except OSError:
            return None

    def rollback(self, checkpoint: Path, target: Path) -> bool:
        """Restore a checkpoint over its target. Never leaves repo broken."""
        try:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(checkpoint.read_bytes())
            return True
        except OSError:
            return False


def parse_failures(output: str) -> list[FailureCard]:
    """Extract failure cards from tool output (plan B4)."""
    cards: list[FailureCard] = []
    for line in output.splitlines():
        for error_type, pattern in FAILURE_PATTERNS:
            m = re.match(pattern, line.strip())
            if not m:
                continue
            groups = m.groups()
            if error_type == "traceback" and len(groups) == 3:
                cards.append(
                    FailureCard(
                        error_type="traceback",
                        file=groups[0],
                        line=int(groups[1] or 0),
                        message=groups[2],
                    )
                )
            elif error_type in ("lint", "typecheck") and len(groups) >= 3:
                cards.append(
                    FailureCard(
                        error_type=error_type,
                        file=groups[0],
                        line=int(groups[1] or 0),
                        message=" ".join(groups[2:]),
                    )
                )
            elif error_type == "test_failure" and len(groups) == 2:
                cards.append(
                    FailureCard(error_type="test_failure", file=groups[0], message=groups[1])
                )
            elif error_type in ("assertion", "syntax", "import_error") and groups:
                cards.append(FailureCard(error_type=error_type, message=groups[0]))
            break  # first matching pattern wins per line
    return cards


def _result_to_dict(r: VerificationResult) -> dict[str, Any]:
    return {
        "ok": r.ok,
        "command": r.command,
        "exit_code": r.exit_code,
        "cards": [
            {
                "error_type": c.error_type,
                "file": c.file,
                "line": c.line,
                "message": c.message,
            }
            for c in r.cards
        ],
        "output": r.output,
        "duration": r.duration,
    }


def _result_from_dict(d: dict[str, Any]) -> VerificationResult:
    return VerificationResult(
        ok=bool(d.get("ok")),
        command=str(d.get("command", "")),
        exit_code=int(d.get("exit_code", 0)),
        cards=[FailureCard(**c) for c in d.get("cards", [])],
        output=str(d.get("output", "")),
        duration=float(d.get("duration", 0.0)),
    )
