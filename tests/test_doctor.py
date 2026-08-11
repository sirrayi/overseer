"""Doctor tests: clear failures, no false OKs."""

from __future__ import annotations

import yaml

from overseer.config import load_config
from overseer.doctor import run_doctor
from overseer.vault import Vault


def _write_config(tmp_path, **overrides):
    p = tmp_path / "config.yaml"
    data = {
        "vault_path": str(tmp_path / "vault"),
        "log_dir": str(tmp_path / "logs"),
        "provider": {"api_key_env": "OVERSEER_API_KEY"},
    }
    data.update(overrides)
    p.write_text(yaml.safe_dump(data), encoding="utf-8")
    return p


def test_doctor_ok_after_init(tmp_path, monkeypatch):
    Vault(tmp_path / "vault").init()
    monkeypatch.setenv("OVERSEER_API_KEY", "test-key")
    cfg = load_config(_write_config(tmp_path))
    report = run_doctor(cfg)
    assert report.ok, report.render()


def test_doctor_fails_when_vault_missing(tmp_path):
    cfg = load_config(_write_config(tmp_path))
    report = run_doctor(cfg)
    assert not report.ok
    assert any("does not exist" in e for c in report.checks for e in c.errors)


def test_doctor_fails_when_vault_not_initialized(tmp_path):
    (tmp_path / "vault").mkdir()
    cfg = load_config(_write_config(tmp_path))
    report = run_doctor(cfg)
    assert not report.ok
    assert any("overseer init" in e for c in report.checks for e in c.errors)


def test_doctor_fails_on_missing_provider_key(tmp_path, monkeypatch):
    Vault(tmp_path / "vault").init()
    monkeypatch.delenv("OVERSEER_API_KEY", raising=False)
    cfg = load_config(_write_config(tmp_path))
    report = run_doctor(cfg)
    assert not report.ok
    assert any("OVERSEER_API_KEY" in e for c in report.checks for e in c.errors)


def test_doctor_passes_with_provider_key(tmp_path, monkeypatch):
    Vault(tmp_path / "vault").init()
    monkeypatch.setenv("OVERSEER_API_KEY", "test-key")
    cfg = load_config(_write_config(tmp_path))
    report = run_doctor(cfg)
    assert report.ok, report.render()


def test_doctor_render_mentions_result(tmp_path, monkeypatch):
    Vault(tmp_path / "vault").init()
    monkeypatch.setenv("OVERSEER_API_KEY", "test-key")
    cfg = load_config(_write_config(tmp_path))
    report = run_doctor(cfg)
    assert "all checks passed" in report.render()
