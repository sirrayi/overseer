"""Merge the harness's `manifest.json` (overseer-core manifest.rs) with
run metrics into a RunRecord — the row the §4.5 reporting standard keys on.

Manifest fields land under `provenance` so non-overseer arms (the mini
control) can carry a degraded-but-honest equivalent.
"""

from __future__ import annotations

import json
from pathlib import Path

SCHEMA = "overseer.run-record/1"


def load_manifest(session_dir: Path | str) -> dict | None:
    p = Path(session_dir) / "manifest.json"
    if not p.exists():
        return None
    try:
        return json.loads(p.read_text())
    except (json.JSONDecodeError, OSError):
        return None


def build_record(
    *,
    run_id: str,
    task,
    agent: str,
    seed: int,
    outcome: dict,
    manifest: dict | None,
    benchmark: str,
    run_set_id: str,
    session_dir: str,
    judge_version: str | None,
    harness_commit: str,
    contamination_notes: str | None = None,
) -> dict:
    """Assemble the canonical score row. `outcome` carries the agent-side
    metrics (pass, steps, tokens, cost, wall, error, infra_error)."""
    prov = None
    if manifest:
        prov = {
            "schema": manifest.get("schema"),
            "harness": manifest.get("harness"),
            "model": manifest.get("model"),
            "limits": manifest.get("limits"),
            "policy": manifest.get("policy"),
            "context": manifest.get("context"),
            "system_prompt": manifest.get("system_prompt"),
            "tools": manifest.get("tools"),
        }
    # manifest.model may be a dict (harness manifest) or a plain string —
    # tolerate both; the record's model falls back to outcome.model.
    m_model = manifest.get("model") if manifest else None
    model_name = m_model.get("name") if isinstance(m_model, dict) else m_model
    return {
        "kind": "run",
        "schema": SCHEMA,
        "run_id": run_id,
        "run_set_id": run_set_id,
        "benchmark": benchmark,
        # identity keys (playbook 7.3.4)
        "task_id": task.id,
        "task_version": task.version,
        "harness": agent,
        "harness_commit": harness_commit,
        "model": model_name or outcome.get("model"),
        "seed": seed,
        "env_digest": task.env_digest,
        "judge_version": judge_version,
        # outcome + metrics
        "pass": bool(outcome.get("pass")),
        "infra_error": bool(outcome.get("infra_error")),
        "error": outcome.get("error"),
        "grader_exit": outcome.get("grader_exit"),
        "grader_tail": outcome.get("grader_tail"),
        "retried": outcome.get("retried"),
        "steps": outcome.get("steps"),
        "wall_s": outcome.get("wall_s"),
        "tokens_in": outcome.get("tokens_in"),
        "tokens_out": outcome.get("tokens_out"),
        "cache_hit_rate": outcome.get("cache_hit_rate"),
        "cost_usd": outcome.get("cost_usd"),
        "done": bool(outcome.get("done")),
        # provenance + audit
        "provenance": prov,
        "session_dir": session_dir,
        "trajectory": outcome.get("trajectory"),
        "ts": outcome.get("ts"),
        "tags": task.tags,
        "difficulty": task.difficulty,
        "contamination_notes": contamination_notes,
    }
