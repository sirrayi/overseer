"""overseer doctor — validate config, vault, provider, permissions (plan B0).

Fails clearly when something is wrong. Never touches secrets.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path

from overseer.config import Config, load_config
from overseer.errors import ConfigError
from overseer.vault import Vault


@dataclass
class CheckResult:
    name: str
    ok: bool
    detail: str = ""
    errors: list[str] = field(default_factory=list)

    @property
    def status(self) -> str:
        return "OK" if self.ok else "FAIL"


@dataclass
class DoctorReport:
    checks: list[CheckResult]

    @property
    def ok(self) -> bool:
        return all(c.ok for c in self.checks)

    def render(self) -> str:
        lines = ["overseer doctor"]
        for c in self.checks:
            marker = "ok" if c.ok else "FAIL"
            lines.append(f"  [{marker}] {c.name}: {c.detail}")
            for err in c.errors:
                lines.append(f"         - {err}")
        lines.append(f"  result: {'all checks passed' if self.ok else 'FAILURES PRESENT'}")
        return "\n".join(lines)


def _check_config(cfg: Config) -> CheckResult:
    errors: list[str] = []
    if not cfg.vault_path:
        errors.append("vault_path is empty")
    if cfg.max_tokens_per_turn < 256:
        errors.append("max_tokens_per_turn too small (<256)")
    if cfg.power_mode not in ("eco", "balanced", "performance"):
        errors.append(f"invalid power_mode: {cfg.power_mode}")
    detail = f"vault={cfg.vault_path} power={cfg.power_mode}"
    return CheckResult("config", not errors, detail, errors)


def _check_vault(cfg: Config) -> CheckResult:
    errors: list[str] = []
    vault = Vault(cfg.vault_path)
    if not vault.root.exists():
        errors.append(f"vault path does not exist: {vault.root} (run `overseer init`)")
    elif not vault.root.is_dir():
        errors.append(f"vault path is not a directory: {vault.root}")
    elif not vault.is_vault():
        errors.append(
            "vault missing system notes (05-System/Guardrails.md, "
            "05-System/Ontology.md) — run `overseer init`"
        )
    detail = str(vault.root)
    return CheckResult("vault", not errors, detail, errors)


def _check_permissions(cfg: Config) -> CheckResult:
    errors: list[str] = []
    vault = Vault(cfg.vault_path)
    if vault.root.exists() and not os.access(vault.root, os.W_OK):
        errors.append(f"vault not writable: {vault.root}")
    log_dir = Path(cfg.log_dir).expanduser()
    if log_dir.exists() and not os.access(log_dir, os.W_OK):
        errors.append(f"log dir not writable: {log_dir}")
    detail = f"vault_writable={not errors}"
    return CheckResult("permissions", not errors, detail, errors)


def _check_provider(cfg: Config) -> CheckResult:
    errors: list[str] = []
    provider = cfg.provider
    if not provider.name:
        errors.append("provider.name is empty")
    if not provider.model:
        errors.append("provider.model is empty")
    if provider.api_key_env and not os.environ.get(provider.api_key_env):
        errors.append(f"env var {provider.api_key_env} is not set (provider key missing)")
    detail = f"{provider.name}/{provider.model}"
    return CheckResult("provider", not errors, detail, errors)


def run_doctor(cfg: Config) -> DoctorReport:
    """Run all doctor checks against a loaded config."""
    return DoctorReport(
        checks=[
            _check_config(cfg),
            _check_vault(cfg),
            _check_permissions(cfg),
            _check_provider(cfg),
        ]
    )


def load_config_or_report(path: str | None) -> tuple[Config | None, DoctorReport | None]:
    """Load config; on failure return a doctor report explaining the failure."""
    try:
        return load_config(path), None
    except ConfigError as exc:
        report = DoctorReport(
            checks=[CheckResult("config", False, "config failed to load", [str(exc)])]
        )
        return None, report
