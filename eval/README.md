# eval/ — Overseer evaluation rig (Phase 0.6)

Harness contribution is measured, not assumed (playbook Ch.8 + Ch.12 §12.2).
This rig runs Overseer and the mini-SWE-agent control scaffold on identical
tasks so every harness change gets a paired benchmark delta.

## Design (per playbook)

- **Runner**: Inspect AI. Overseer is invoked headlessly:
  `overseer exec --json "<task>"` in the task's workdir; the JSONL event
  stream is the trajectory.
- **Task spec** (immutable): `{instruction, env_image_digest, setup_script,
  grader, reference_solution, tags, limits}`.
- **Trajectory store**: keyed by
  `{task_id, task_version, harness_commit, model, date, seed?, env_digest}`.
- **Report**: pass@1 ± CI, pass^k, cost/task, tokens/task, cache-hit rate,
  wall time, and the full scaffold spec — the public-reporting checklist.
- **Control**: mini-SWE-agent (~100 lines) on the same tasks — the null
  hypothesis every harness feature must beat.

## Layout

- `tasks/` — task specs (`*.yaml`), smoke suite first (~50 SWE-bench Verified
  + Terminal-Bench subset)
- `scorers/` — deterministic graders (exit-code / test-diff based first)
- `solvers/` — `overseer_exec.py` Inspect solver + `mini_swe_agent.py` control
- `results/` — trajectories + reports (gitignored large artifacts)

## Status

Scaffold only — tasks and solvers land with the Phase 0 exit run.
