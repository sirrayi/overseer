"""Seeded run matrix (playbook Ch.8 §7.3.3).

For each (task × agent × seed): fresh workspace → setup → solver →
deterministic grader → RunRecord appended to the store.

Failure discipline (playbook: "infra-failure vs agent-failure separated;
retry infra, never retry-and-count agent failures"):
- Agent failures (grader fail, solver said done=False) are pass=False rows.
- Infra failures (missing binary, transport error, wall-timeout at the
  runner level) are retried ONCE; if still failing the row is recorded
  with infra_error=True and excluded from pass statistics.

Cell order is fully deterministic: sorted(task.id) × sorted(agent) ×
range(k) — the `scheduler_seed` is recorded for any ordering that later
becomes randomized.
"""

from __future__ import annotations

import subprocess
import time
from pathlib import Path

from . import agents, graders, manifest
from . import store as store_mod


class InfraError(Exception):
    """A failure in the rig, not the agent."""


def _is_infra(outcome: dict) -> bool:
    return bool(outcome.get("infra_error") or outcome.get("provider_error"))


def run_cell(
    task, agent_name: str, seed: int, *, ws_root: Path, runs_root: Path, run_id: str
) -> dict:
    """One matrix cell. Returns the raw outcome dict (grader merged in)."""
    solver = agents.get(agent_name)
    ws = ws_root / task.id / agent_name / f"s{seed}"
    sess = runs_root / run_id
    graders.setup_workspace(task, ws)

    outcome = solver.solve(
        task.instruction, str(ws), str(sess), limits=task.limits, seed=seed, task=task
    )
    if _is_infra(outcome):
        return outcome  # graded nowhere — not the agent's fault

    ok, code, tail = grade_after(task, ws, outcome)
    outcome["grader_exit"] = code
    outcome["grader_tail"] = tail
    outcome["pass"] = ok
    return outcome


def grade_after(task, ws: Path, outcome: dict) -> tuple[bool, int, str]:
    ok, code, tail = graders.grade(task, ws)
    # A solver that never signaled done can't pass even if the grader
    # happens to exit 0 (prevents accidental pass on partial work).
    return (ok and outcome.get("done", False)), code, tail


def run_matrix(
    tasks: list,
    agent_names: list[str],
    k: int,
    *,
    results_root: Path,
    ws_root: Path,
    store: store_mod.Store,
    benchmark: str = "local",
    scheduler_seed: int = 0,
    judge_version: str | None = None,
    contamination_notes: str | None = None,
    harness_commit: str = "unknown",
    on_progress=None,
) -> list[dict]:
    """Run the full matrix; returns the run records appended."""
    header = store.matrix_header(
        benchmark=benchmark,
        agents=sorted(agent_names),
        k=k,
        tasks=[t.id for t in tasks],
        scheduler_seed=scheduler_seed,
        extra={
            "harness_commit": harness_commit,
            "contamination_notes": contamination_notes,
        },
    )
    records = []
    for task in sorted(tasks, key=lambda t: t.id):
        for agent_name in sorted(agent_names):
            for seed in range(k):
                run_id = store_mod.new_run_id(task.id, agent_name, seed)
                sess = results_root / "runs" / run_id

                try:
                    outcome = run_cell(
                        task,
                        agent_name,
                        seed,
                        ws_root=ws_root,
                        runs_root=results_root / "runs",
                        run_id=run_id,
                    )
                except (
                    InfraError,
                    FileNotFoundError,
                    subprocess.TimeoutExpired,
                    RuntimeError,
                    OSError,
                    KeyError,
                ) as e:
                    outcome = {
                        "done": False,
                        "infra_error": True,
                        "error": f"{type(e).__name__}: {e}",
                        "wall_s": 0.0,
                        "ts": int(time.time()),
                    }

                # Retry infra once — fresh dirs, never counted as an agent trial.
                if _is_infra(outcome):
                    try:
                        retry = run_cell(
                            task,
                            agent_name,
                            seed,
                            ws_root=ws_root,
                            runs_root=results_root / "runs",
                            run_id=run_id + "r",
                        )
                        # Either way the retry is the recorded attempt: its
                        # session dir and outcome describe the same run.
                        retry["retried"] = True
                        outcome = retry
                        sess = results_root / "runs" / (run_id + "r")
                    except Exception as e:
                        outcome["error"] = f"{outcome.get('error')} | retry: {e}"

                rec = manifest.build_record(
                    run_id=run_id,
                    task=task,
                    agent=agent_name,
                    seed=seed,
                    outcome=outcome,
                    manifest=manifest.load_manifest(sess),
                    benchmark=benchmark,
                    run_set_id=header["run_set_id"],
                    session_dir=str(sess),
                    judge_version=judge_version,
                    harness_commit=harness_commit,
                    contamination_notes=contamination_notes,
                )
                store.append(rec)
                records.append(rec)
                if on_progress:
                    on_progress(rec)
    return records
