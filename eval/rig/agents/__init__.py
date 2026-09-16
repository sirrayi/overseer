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

# P4.3 ablation arms: `overseer@<preset>` runs the overseer arm with
# component-removal flags — e.g. `--agents overseer,overseer@no-compact`
# A/Bs the context engine. The arm name lands in records verbatim, so
# paired deltas read `overseer@no-compact − overseer`.
ABLATIONS = {
    "no-compact": ["--no-compact"],
    "no-keep": ["--keep-results", "0"],
    "no-subagents": ["--no-tools", "task"],
    "no-skills": ["--no-tools", "skill"],
    "no-repomap": ["--no-tools", "repo_map,symbol"],
    "no-plan": ["--no-tools", "plan"],
    "minimal": [
        "--no-compact",
        "--keep-results",
        "0",
        "--no-tools",
        "task,skill,repo_map,symbol,plan",
    ],
}


class _Ablation:
    def __init__(self, flags):
        self._flags = flags

    def solve(self, instruction, workdir, session_dir, **kw):
        return overseer.solve(
            instruction, workdir, session_dir, extra_flags=self._flags, **kw
        )


def get(name: str):
    if name.startswith("overseer@"):
        preset = name.split("@", 1)[1]
        if preset not in ABLATIONS:
            raise KeyError(
                f"unknown ablation {preset!r}; available: {sorted(ABLATIONS)}"
            )
        return _Ablation(ABLATIONS[preset])
    if name not in REGISTRY:
        raise KeyError(
            f"unknown agent {name!r}; available: {sorted(REGISTRY)} "
            f"or overseer@<ablation> {sorted(ABLATIONS)}"
        )
    return REGISTRY[name]
