"""Oracle arm — applies the task's reference solution, then the grader.

Not a model run: this proves the task is solvable end-to-end and
exercises the whole pipeline (workspace, grader, record) with zero API
spend. A task whose oracle fails is broken, not hard (0% pass@k ⇒
broken task — playbook Ch.8 §7.3.2).
"""

from __future__ import annotations

import time

from .. import graders


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
    if task is None:
        return {"done": False, "error": "oracle arm requires task context"}
    p = graders.run_script(task.oracle_script(), workdir)
    ok = p.returncode == 0
    return {
        "done": ok,
        "steps": 0,
        "wall_s": round(time.time() - t0, 1),
        "tokens_in": 0,
        "tokens_out": 0,
        "cost_usd": 0.0,
        "error": None if ok else f"oracle exit {p.returncode}: {p.stderr[-500:]}",
        "model": "oracle",
        "ts": int(time.time()),
    }
