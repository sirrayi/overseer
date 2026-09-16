"""Agent drivers — the factorial arm (playbook Ch.8 §7.2: the scaffold
layer is the variable under test).

Every solver has one signature:

    solve(instruction, workdir, session_dir, *, limits, seed) -> dict

and returns an outcome dict:

    done, steps, wall_s, tokens_in, tokens_out, cache_hit_rate, cost_usd,
    trajectory (path), model, error, infra_error, provider_error

`infra_error`/`provider_error` mark rows that are not agent trials
(transport failures, missing binaries) — the scheduler retries them once
and excludes them from pass statistics.
"""

from __future__ import annotations

from . import fail, mini, oracle, overseer

REGISTRY = {
    "overseer": overseer,
    "mini": mini,
    "oracle": oracle,  # runs the task's reference solution — solvability check
    "fail": fail,  # always fails — rig self-test
}


def get(name: str):
    if name not in REGISTRY:
        raise KeyError(f"unknown agent {name!r}; available: {sorted(REGISTRY)}")
    return REGISTRY[name]
