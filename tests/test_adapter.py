"""Adapter tests: power gating, validation gate, hot-swap, rollback (B11)."""

from __future__ import annotations

from pathlib import Path

import pytest

from overseer.adapter import AdapterRegistry, TrainingHook
from overseer.errors import ToolError


def _registry(tmp_path: Path) -> AdapterRegistry:
    return AdapterRegistry(tmp_path / "vault")


def test_training_blocked_without_opt_in(tmp_path):
    hook = TrainingHook(tmp_path / "vault", enabled=False)
    allowed, reason = hook.can_train()
    assert not allowed
    assert "opt-in" in reason


def test_training_blocked_in_eco(tmp_path):
    hook = TrainingHook(tmp_path / "vault", enabled=True, power_mode="eco")
    allowed, reason = hook.can_train()
    assert not allowed
    assert "eco" in reason


def test_training_blocked_on_battery(tmp_path):
    hook = TrainingHook(tmp_path / "vault", enabled=True, on_battery=True)
    allowed, reason = hook.can_train()
    assert not allowed
    assert "battery" in reason


def test_training_allowed_when_ok(tmp_path):
    hook = TrainingHook(tmp_path / "vault", enabled=True, power_mode="balanced", on_battery=False)
    allowed, reason = hook.can_train()
    assert allowed
    assert reason == "ok"


def test_train_stub_registers_adapter(tmp_path):
    hook = TrainingHook(tmp_path / "vault", enabled=True)
    ds = tmp_path / "dataset.jsonl"
    ds.write_text("{}\n", encoding="utf-8")
    adapter = hook.train(ds)
    assert adapter.version.startswith("v")
    assert adapter.path.exists()
    assert not adapter.validated
    assert not adapter.active


def test_validation_gate_rejects_regression(tmp_path):
    reg = _registry(tmp_path)
    adapter = reg.register("v1", tmp_path / "vault" / ".overseer" / "adapters" / "v1")
    adapter.path.mkdir(parents=True, exist_ok=True)
    reg.validate("v1", {"golden1": True, "golden2": False})  # regression
    a = reg.get("v1")
    assert a is not None
    assert not a.validated
    assert "REGRESSION" in a.validation_report


def test_validation_gate_passes_clean(tmp_path):
    reg = _registry(tmp_path)
    adapter = reg.register("v1", tmp_path / "vault" / ".overseer" / "adapters" / "v1")
    adapter.path.mkdir(parents=True, exist_ok=True)
    reg.validate("v1", {"golden1": True, "golden2": True})
    a = reg.get("v1")
    assert a is not None
    assert a.validated


def test_activate_requires_validation(tmp_path):
    reg = _registry(tmp_path)
    adapter = reg.register("v1", tmp_path / "vault" / ".overseer" / "adapters" / "v1")
    adapter.path.mkdir(parents=True, exist_ok=True)
    with pytest.raises(ToolError, match="not validated"):
        reg.activate("v1")


def test_activate_and_rollback(tmp_path):
    reg = _registry(tmp_path)
    adapter = reg.register("v1", tmp_path / "vault" / ".overseer" / "adapters" / "v1")
    adapter.path.mkdir(parents=True, exist_ok=True)
    reg.validate("v1", {"g1": True})
    reg.activate("v1")
    assert reg.active_adapter() is not None
    assert reg.active_adapter().version == "v1"  # type: ignore[union-attr]
    reg.rollback()
    assert reg.active_adapter() is None


def test_registry_persists(tmp_path):
    reg = _registry(tmp_path)
    adapter = reg.register("v1", tmp_path / "vault" / ".overseer" / "adapters" / "v1")
    adapter.path.mkdir(parents=True, exist_ok=True)
    reg.validate("v1", {"g1": True})
    reg2 = _registry(tmp_path)
    a = reg2.get("v1")
    assert a is not None
    assert a.validated
