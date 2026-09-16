"""Fail arm — rig self-test: always fails the grader. Verifies that the
pipeline records honest failures (a rig that can't record failure is
useless)."""

from __future__ import annotations

import time


def solve(
    instruction: str,
    workdir: str,
    session_dir: str,
    *,
    limits: dict | None = None,
    seed: int = 0,
    task=None,
) -> dict:
    t0 = time.time()
    return {
        "done": False,
        "steps": 0,
        "wall_s": round(time.time() - t0, 1),
        "tokens_in": 0,
        "tokens_out": 0,
        "cost_usd": 0.0,
        "model": "fail",
        "ts": int(time.time()),
    }
