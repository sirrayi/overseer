"""Config system tests: validation, env overrides, failure clarity."""

from __future__ import annotations

from pathlib import Path

import pytest

from overseer.config import load_config, write_sample_config
from overseer.errors import ConfigError


def test_defaults_when_no_file(tmp_path):
    cfg = load_config(None)
    assert cfg.power_mode == "balanced"
    assert cfg.live_learning is True
    assert cfg.provider.model == "deepseek-v4-flash"


def test_missing_file_raises_clear_error(tmp_path):
    with pytest.raises(ConfigError, match="not found"):
        load_config(tmp_path / "nope.yaml")


def test_invalid_yaml_raises_clear_error(tmp_path):
    p = tmp_path / "config.yaml"
    p.write_text("vault_path: [unclosed\n", encoding="utf-8")
    with pytest.raises(ConfigError, match="not valid YAML"):
        load_config(p)


def test_invalid_power_mode_rejected(tmp_path):
    p = tmp_path / "config.yaml"
    p.write_text("power_mode: turbo\n", encoding="utf-8")
    with pytest.raises(ConfigError, match="power_mode"):
        load_config(p)


def test_env_override(tmp_path, monkeypatch):
    p = tmp_path / "config.yaml"
    p.write_text("power_mode: eco\n", encoding="utf-8")
    monkeypatch.setenv("OVERSEER_POWER_MODE", "performance")
    cfg = load_config(p)
    assert cfg.power_mode == "performance"


def test_env_override_bool(tmp_path, monkeypatch):
    p = tmp_path / "config.yaml"
    p.write_text("live_learning: true\n", encoding="utf-8")
    monkeypatch.setenv("OVERSEER_LIVE_LEARNING", "false")
    cfg = load_config(p)
    assert cfg.live_learning is False


def test_env_override_int(tmp_path, monkeypatch):
    p = tmp_path / "config.yaml"
    p.write_text("max_tokens_per_turn: 8000\n", encoding="utf-8")
    monkeypatch.setenv("OVERSEER_MAX_TOKENS_PER_TURN", "4000")
    cfg = load_config(p)
    assert cfg.max_tokens_per_turn == 4000


def test_sample_config_roundtrip(tmp_path):
    p = tmp_path / "config.yaml"
    write_sample_config(p)
    cfg = load_config(p)
    # vault_path is expanded by the validator (~ -> home).
    assert cfg.vault_path == str(Path.home() / "overseer-vault")
    assert cfg.provider.api_key_env == "OVERSEER_API_KEY"


def test_sample_config_has_no_secrets(tmp_path):
    p = tmp_path / "config.yaml"
    write_sample_config(p)
    text = p.read_text(encoding="utf-8")
    assert "sk-" not in text
    assert "ghp_" not in text
    assert "api_key:" not in text  # only api_key_env placeholder
