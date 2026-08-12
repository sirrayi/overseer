"""Verification engine tests: failure cards, targeted tests, rollback (B4)."""

from __future__ import annotations

from pathlib import Path

from overseer.project import ProjectContext
from overseer.verification import (
    FailureCard,
    VerificationResult,
    VerificationRunner,
    parse_failures,
)


def _project(tmp_path: Path, commands: dict[str, str] | None = None) -> ProjectContext:
    return ProjectContext(
        root=tmp_path,
        name="demo",
        language="python",
        test_runner="pytest",
        linter="ruff",
        typechecker="mypy",
        commands=(
            commands
            if commands is not None
            else {"test": "pytest -q", "lint": "ruff check .", "typecheck": "mypy src"}
        ),
    )


def test_parse_pytest_failures():
    out = (
        "FAILED tests/test_agent.py::test_observer - AssertionError: boom\n"
        "E   AssertionError: expected 1, got 2\n"
        '  File "/repo/src/overseer/agent.py", line 42, in run\n'
        "    return self._dispatch(call)\n"
    )
    cards = parse_failures(out)
    types = [c.error_type for c in cards]
    assert "test_failure" in types
    assert "assertion" in types
    assert "traceback" in types
    tb = next(c for c in cards if c.error_type == "traceback")
    assert tb.file == "/repo/src/overseer/agent.py"
    assert tb.line == 42


def test_parse_ruff_and_mypy():
    out = (
        "src/overseer/agent.py:12:5: F841 Local variable `x` is assigned to but never used\n"
        "src/overseer/agent.py:30: error: Argument 1 has incompatible type\n"
    )
    cards = parse_failures(out)
    assert any(
        c.error_type == "lint" and c.file == "src/overseer/agent.py" and c.line == 12 for c in cards
    )
    assert any(c.error_type == "typecheck" and c.line == 30 for c in cards)


def test_parse_empty_output():
    assert parse_failures("") == []
    assert parse_failures("all tests passed, 42 passed in 0.1s") == []


def test_runner_missing_command(tmp_path):
    r = VerificationRunner(_project(tmp_path, commands={}))
    res = r.run_tests()
    assert not res.ok
    assert "no test command" in res.output


def test_runner_targeted_tests(tmp_path, monkeypatch):
    """Targeted test selection must pass file paths to pytest."""
    calls: list[str] = []

    def fake_run(self, command, cwd=None):
        calls.append(command)
        return VerificationResult(ok=True, command=command, exit_code=0, output="ok")

    monkeypatch.setattr(VerificationRunner, "_run", fake_run)
    r = VerificationRunner(_project(tmp_path))
    r.run_tests(targets=["tests/test_agent.py"])
    assert calls and "tests/test_agent.py" in calls[0]


def test_runner_parses_failures(tmp_path, monkeypatch):
    def fake_run(self, command, cwd=None):
        out = "FAILED tests/test_x.py::test_y - AssertionError: nope\n"
        return VerificationResult(
            ok=False,
            command=command,
            exit_code=1,
            output=out,
            cards=parse_failures(out),  # mimic the real _run
        )

    monkeypatch.setattr(VerificationRunner, "_run", fake_run)
    r = VerificationRunner(_project(tmp_path))
    res = r.run_tests()
    assert not res.ok
    assert res.cards and res.cards[0].error_type == "test_failure"
    assert "FAILED" in res.summary()


def test_runner_cache(tmp_path, monkeypatch):
    """Verification results must be cached (no re-runs on unchanged files)."""
    runs: list[str] = []

    def fake_run(self, command, cwd=None):
        runs.append(command)
        return VerificationResult(ok=True, command=command, exit_code=0, output="ok")

    monkeypatch.setattr(VerificationRunner, "_run", fake_run)
    r = VerificationRunner(_project(tmp_path))
    r.run_tests()
    r.run_tests()
    assert len(runs) == 1  # second call served from cache


def test_checkpoint_and_rollback(tmp_path):
    target = tmp_path / "src" / "agent.py"
    target.parent.mkdir(parents=True)
    target.write_text("original\n", encoding="utf-8")
    r = VerificationRunner(_project(tmp_path))
    cp = r.checkpoint(target)
    assert cp is not None and cp.is_file()
    # Patch the file, then roll back.
    target.write_text("broken\n", encoding="utf-8")
    assert r.rollback(cp, target)
    assert target.read_text(encoding="utf-8") == "original\n"


def test_checkpoint_missing_file(tmp_path):
    r = VerificationRunner(_project(tmp_path))
    assert r.checkpoint(tmp_path / "nope.py") is None


def test_verify_merges_cards(tmp_path, monkeypatch):
    def fake_run(self, command, cwd=None):
        if "pytest" in command:
            out = "FAILED tests/test_x.py - AssertionError: boom\n"
            return VerificationResult(
                ok=False,
                command=command,
                exit_code=1,
                output=out,
                cards=parse_failures(out),
            )
        return VerificationResult(ok=True, command=command, exit_code=0, output="ok")

    monkeypatch.setattr(VerificationRunner, "_run", fake_run)
    r = VerificationRunner(_project(tmp_path))
    res = r.verify()
    assert not res.ok
    assert any(c.error_type == "test_failure" for c in res.cards)


def test_failure_card_render():
    c = FailureCard(error_type="lint", file="a.py", line=3, message="F841 unused")
    assert "a.py:3" in c.render()
    assert "F841" in c.render()
