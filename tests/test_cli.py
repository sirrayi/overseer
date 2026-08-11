"""CLI smoke tests: init, doctor, version (plan B2: CLI smoke tests)."""

from __future__ import annotations

from typer.testing import CliRunner

from overseer.cli import app

runner = CliRunner()


def test_version():
    result = runner.invoke(app, ["version"])
    assert result.exit_code == 0
    assert "overseer" in result.output


def test_init_creates_vault(tmp_path):
    vault = str(tmp_path / "vault")
    result = runner.invoke(
        app, ["init", "--vault", vault, "--config", str(tmp_path / "config.yaml")]
    )
    assert result.exit_code == 0, result.output
    assert "vault ready" in result.output
    assert (tmp_path / "vault" / "05-System" / "Guardrails.md").exists()


def test_doctor_fails_clearly_without_vault(tmp_path):
    result = runner.invoke(app, ["doctor", "--config", str(tmp_path / "config.yaml")])
    assert result.exit_code == 1
    assert "config failed to load" in result.output


def test_doctor_ok_after_init(tmp_path, monkeypatch):
    vault = str(tmp_path / "vault")
    runner.invoke(app, ["init", "--vault", vault, "--config", str(tmp_path / "config.yaml")])
    monkeypatch.setenv("OVERSEER_API_KEY", "test-key")
    result = runner.invoke(app, ["doctor", "--config", str(tmp_path / "config.yaml")])
    assert result.exit_code == 0, result.output
    assert "all checks passed" in result.output
