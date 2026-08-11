"""Configuration system: config.yaml + OVERSEER_* environment overrides + validation.

Plan: config supports config.yaml plus environment overrides; schema validates
required fields; no real secrets in config.yaml (env vars for sensitive values).
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

import yaml
from pydantic import BaseModel, Field, ValidationError, field_validator

from overseer.errors import ConfigError

ENV_PREFIX = "OVERSEER_"
DEFAULT_CONFIG_NAME = "config.yaml"
DEFAULT_VAULT_NAME = "vault"
DEFAULT_LOG_DIR = "logs"


class ProviderConfig(BaseModel):
    """Model provider settings. Secrets come from env vars, never config.yaml."""

    name: str = "ollama-cloud"
    base_url: str | None = None
    model: str = "deepseek-v4-flash"
    api_key_env: str | None = Field(
        default=None,
        description="Name of the env var holding the API key (e.g. OVERSEER_API_KEY).",
    )


class Config(BaseModel):
    """Top-level overseer configuration."""

    vault_path: str = Field(
        default=DEFAULT_VAULT_NAME,
        description="Path to the canonical Obsidian-compatible vault.",
    )
    log_dir: str = Field(default=DEFAULT_LOG_DIR, description="Directory for logs.")
    provider: ProviderConfig = ProviderConfig()
    power_mode: str = Field(
        default="balanced",
        description="eco | balanced | performance (plan Part 43: power modes).",
    )
    live_learning: bool = Field(
        default=True,
        description="Enable the live learning engine (plan Part 44).",
    )
    max_tokens_per_turn: int = Field(
        default=8000, ge=256, description="Context budget per model call (plan Part 43)."
    )

    @field_validator("power_mode")
    @classmethod
    def _validate_power_mode(cls, v: str) -> str:
        if v not in ("eco", "balanced", "performance"):
            raise ValueError(f"power_mode must be eco|balanced|performance, got {v!r}")
        return v

    @field_validator("vault_path")
    @classmethod
    def _expand_vault_path(cls, v: str) -> str:
        return os.path.expanduser(v)


def _env_override(key: str, current: Any) -> Any:
    """Apply OVERSEEER_* env override for a dotted config key."""
    env_name = ENV_PREFIX + key.upper().replace(".", "_")
    value = os.environ.get(env_name)
    if value is None:
        return current
    if isinstance(current, bool):
        return value.strip().lower() in ("1", "true", "yes", "on")
    if isinstance(current, int):
        return int(value)
    return value


def _apply_env_overrides(data: dict[str, Any]) -> dict[str, Any]:
    """Walk the config dict and apply OVERSEER_* overrides for known keys."""
    for key in list(data.keys()):
        if isinstance(data[key], dict):
            data[key] = _apply_env_overrides(data[key])
        else:
            data[key] = _env_override(key, data[key])
    return data


def load_config(path: str | Path | None = None) -> Config:
    """Load config from a YAML file (or defaults), apply env overrides, validate.

    Raises ConfigError with a clear message when the file is missing/invalid.
    """
    data: dict[str, Any] = {}
    if path is not None:
        p = Path(path)
        if not p.is_file():
            raise ConfigError(f"config file not found: {p}")
        try:
            raw = yaml.safe_load(p.read_text(encoding="utf-8"))
        except yaml.YAMLError as exc:
            raise ConfigError(f"config file is not valid YAML: {p}: {exc}") from exc
        if raw is None:
            raw = {}
        if not isinstance(raw, dict):
            raise ConfigError(f"config file must contain a mapping, got {type(raw).__name__}: {p}")
        data = raw

    data = _apply_env_overrides(data)

    try:
        return Config.model_validate(data)
    except ValidationError as exc:
        raise ConfigError(f"config validation failed: {exc}") from exc


def write_sample_config(path: str | Path, vault_path: str = "~/overseer-vault") -> None:
    """Write a sample config with placeholders only (no real secrets)."""
    sample = {
        "vault_path": vault_path,
        "log_dir": "logs",
        "power_mode": "balanced",
        "live_learning": True,
        "max_tokens_per_turn": 8000,
        "provider": {
            "name": "ollama-cloud",
            "base_url": None,
            "model": "deepseek-v4-flash",
            "api_key_env": "OVERSEER_API_KEY",
        },
    }
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(
        "# Overseer sample config — placeholders only. Never put real secrets here.\n"
        "# Sensitive values come from environment variables (OVERSEER_*).\n"
        + yaml.safe_dump(sample, sort_keys=False),
        encoding="utf-8",
    )
