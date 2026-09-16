# eval/ — Overseer evaluation rig (P4)

Harness contribution is measured, not assumed (playbook Ch.8 + Ch.12 §4).
This rig runs Overseer and a frozen mini-SWE-agent control scaffold on
identical tasks so every harness change gets a paired benchmark delta.

## Design (per playbook)

- **Runner**: this repo's own rig (`rig/`), invoked via `run.py`. Overseer
  runs headlessly: `overseer exec --json` in the task workdir; the JSONL
  event stream plus the session `manifest.json` (P4.5) is the trajectory.
- **Task spec v2** (`tasks/*.json`): `{id, version, instruction, setup,
  grader, oracle, limits, tags}` — oracle is required; a task whose oracle
  fails its grader is broken, not hard.
- **Store**: append-only `results/store.jsonl` keyed by `{task_id,
  task_version, harness, harness_commit, model, seed, env_digest,
  judge_version}` + a matrix header per run set.
- **Statistics** (`rig/stats.py`): pass@1 ± 95% CI via seeded cluster
  bootstrap over tasks; Chen pass@k; τ-style pass^k; paired per-task
  diff (McNemar secondary); p50/p90 steps+tokens; median cost.
- **Report** (`rig/report.py`): markdown + machine JSON with the §4.5
  provenance field set.
- **Control**: `rig/agents/mini.py` — frozen bash-only null scaffold.

## Layout

- `tasks/` — local corpus (5 smoke tasks, all oracle-verified)
- `rig/` — the rig: taskspec, graders, scheduler, store, stats, report,
  manifest, agents/, benchmarks/
- `tests/` — pytest suite (offline, no keys)
- `run.py` — CLI: matrix runner, --oracle-check, --report, benchmark mode
- `results/` — store + report cards (gitignored)
- `workspaces/` — per-cell task workspaces (gitignored)
- `vendor/` — external benchmark checkouts (gitignored)
- `LEDGER.md` — evidence, decisions, known gaps

## Usage

```bash
uv run pytest -q                        # offline rig self-tests
uv run python run.py --oracle-check     # task solvability
uv run python run.py --agents oracle,fail --seeds 3
uv run python run.py --report           # report card
```

Paid paths (`overseer`, `mini`, τ², LCB, docker benchmarks) need
`LEK_API_KEY` and spend approval — see LEDGER.md.
