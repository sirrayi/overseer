"""Adapter registry, validation gate, and training hook (plan B11).

Weight-level adaptation (Tier 2) is strictly opt-in and power-aware:

- Training NEVER runs in eco mode or on battery.
- Training requires adapter_training_enabled=True in config.
- An adapter is only activated after passing the validation gate
  (golden tasks / held-out corrections; regressions -> reject + rollback).
- Activation requires explicit human approval (no silent hot-swap).
- Adapters live in .overseer/adapters/<version>/.
"""

from __future__ import annotations

import json
import subprocess  # nosec B404 — training hook spawns a local training script
import uuid
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from overseer.errors import ToolError


@dataclass
class Adapter:
    """A trained adapter version."""

    version: str
    path: Path
    created: str = field(default_factory=lambda: datetime.now(UTC).isoformat())
    validated: bool = False
    active: bool = False
    validation_report: str = ""

    def to_meta(self) -> dict[str, Any]:
        return {
            "version": self.version,
            "path": str(self.path),
            "created": self.created,
            "validated": self.validated,
            "active": self.active,
            "validation_report": self.validation_report,
        }


class AdapterRegistry:
    """Manage adapter versions under .overseer/adapters/."""

    def __init__(self, vault_root: Path) -> None:
        self.adapters_dir = vault_root / ".overseer" / "adapters"
        self.adapters_dir.mkdir(parents=True, exist_ok=True)
        self._meta_path = self.adapters_dir / "registry.json"
        self.adapters: dict[str, Adapter] = {}
        self._load()

    def _load(self) -> None:
        if not self._meta_path.exists():
            return
        try:
            data = json.loads(self._meta_path.read_text(encoding="utf-8"))
            for version, meta in data.items():
                self.adapters[version] = Adapter(
                    version=version,
                    path=Path(meta["path"]),
                    created=meta.get("created", ""),
                    validated=meta.get("validated", False),
                    active=meta.get("active", False),
                    validation_report=meta.get("validation_report", ""),
                )
        except (json.JSONDecodeError, KeyError):
            return  # corrupt registry: start fresh

    def _save(self) -> None:
        data = {v: a.to_meta() for v, a in self.adapters.items()}
        self._meta_path.write_text(json.dumps(data, indent=2), encoding="utf-8")

    def register(self, version: str, path: Path) -> Adapter:
        """Register a trained adapter (not yet validated or active)."""
        adapter = Adapter(version=version, path=path)
        self.adapters[version] = adapter
        self._save()
        return adapter

    def get(self, version: str) -> Adapter | None:
        return self.adapters.get(version)

    def active_adapter(self) -> Adapter | None:
        for a in self.adapters.values():
            if a.active:
                return a
        return None

    def list(self) -> list[Adapter]:
        return sorted(self.adapters.values(), key=lambda a: a.created, reverse=True)

    def validate(self, version: str, golden_results: dict[str, bool]) -> Adapter:
        """Validation gate: run the adapter against golden tasks.

        golden_results maps task name -> passed (bool). If ANY golden task
        regresses, the adapter is rejected (validated=False) and rolled
        back. Returns the adapter.
        """
        adapter = self.get(version)
        if adapter is None:
            raise ToolError(f"adapter {version} not registered")
        passed = sum(1 for ok in golden_results.values() if ok)
        total = len(golden_results)
        ok = total > 0 and passed == total  # zero regressions allowed
        adapter.validated = ok
        adapter.validation_report = f"golden tasks: {passed}/{total} passed" + (
            "" if ok else " — REGRESSION: rejected and rolled back"
        )
        if not ok:
            adapter.active = False
        self._save()
        return adapter

    def activate(self, version: str) -> Adapter:
        """Hot-swap: make an adapter active. Only validated adapters."""
        adapter = self.get(version)
        if adapter is None:
            raise ToolError(f"adapter {version} not registered")
        if not adapter.validated:
            raise ToolError(f"adapter {version} not validated — cannot activate (validation gate)")
        for a in self.adapters.values():
            a.active = False
        adapter.active = True
        self._save()
        return adapter

    def rollback(self) -> Adapter | None:
        """Deactivate the active adapter (rollback to base model)."""
        active = self.active_adapter()
        if active is not None:
            active.active = False
            self._save()
        return active


class TrainingHook:
    """Power-aware, opt-in local training hook (MLX LoRA/DPO or stub)."""

    def __init__(
        self,
        vault_root: Path,
        enabled: bool = False,
        power_mode: str = "balanced",
        on_battery: bool = False,
        command: list[str] | None = None,
    ) -> None:
        self.vault_root = vault_root
        self.enabled = enabled
        self.power_mode = power_mode
        self.on_battery = on_battery
        self.command = command  # e.g. ["mlx_lm.lora", "--train", ...]

    def can_train(self) -> tuple[bool, str]:
        """Power-aware gate. Returns (allowed, reason)."""
        if not self.enabled:
            return False, "adapter_training_enabled is False (opt-in required)"
        if self.power_mode == "eco":
            return False, "power_mode is eco — training deferred"
        if self.on_battery:
            return False, "on battery — training deferred"
        return True, "ok"

    def train(self, dataset_path: Path, version: str | None = None) -> Adapter:
        """Run the training hook. Returns the registered adapter.

        With no command configured, this is a stub that registers a
        placeholder adapter (the real MLX pipeline is a B12 concern).
        """
        allowed, reason = self.can_train()
        if not allowed:
            raise ToolError(f"training blocked: {reason}")

        version = version or f"v{uuid.uuid4().hex[:8]}"
        adapter_dir = self.vault_root / ".overseer" / "adapters" / version
        adapter_dir.mkdir(parents=True, exist_ok=True)

        if self.command:
            try:
                proc = subprocess.run(  # noqa: S603  # nosec B603 — user-configured
                    [*self.command, "--dataset", str(dataset_path), "--out", str(adapter_dir)],
                    capture_output=True,
                    text=True,
                    timeout=3600,
                )
                if proc.returncode != 0:
                    raise ToolError(f"training failed: {proc.stderr[-500:]}")
            except subprocess.TimeoutExpired as exc:
                raise ToolError("training timed out") from exc
        else:
            # Stub: write a marker so the registry has something to hold.
            (adapter_dir / "adapter_config.json").write_text(
                json.dumps({"stub": True, "dataset": str(dataset_path)}),
                encoding="utf-8",
            )

        registry = AdapterRegistry(self.vault_root)
        return registry.register(version, adapter_dir)
