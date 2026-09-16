# Eval Ledger — Phase 4

Working record of what the rig does, what was verified, and what is
deferred. Every claim below cites a check that actually ran.

## Decisions (user-approved)

- Model access: **LegionEdge gateway only** (`LEK_API_KEY`/`LEK_BASE_URL`).
- Container runtime: **OrbStack** (installed, daemon verified via
  `docker info`).
- Spending posture: **zero spend until approved** — no paid benchmark
  runs have been executed.

## What exists (branch `eval/rig-v2`)

| Piece | File | Verified by |
|---|---|---|
| Task spec v2 (versioned, oracle required) | `rig/taskspec.py` | `tests/test_store_spec.py` |
| Deterministic graders + oracle check | `rig/graders.py` | `run.py --oracle-check` → 5/5 |
| k-seed matrix, infra/agent failure split | `rig/scheduler.py` | `tests/test_scheduler.py` |
| Immutable JSONL store (§7.3.4 keys) | `rig/store.py` | `tests/test_store_spec.py` |
| pass@1±CI, pass@k, pass^k, paired bootstrap, McNemar | `rig/stats.py` | `tests/test_stats.py` (hand-computed) |
| Report card md+JSON (§4.5 field set) | `rig/report.py` | `run.py --report` |
| RunRecord merge w/ overseer manifest.json | `rig/manifest.py` | `tests/test_scheduler.py` |
| Agent arms: overseer, mini (frozen null), oracle, fail | `rig/agents/` | scheduler tests |
| τ² adapter (airline/retail, pinned user-sim) | `rig/benchmarks/tau2.py` | fixture parse test vs real schema |
| LCB adapter (release-windowed) | `rig/benchmarks/lcb.py` | fixture parse test vs real schema |
| Docker-gated detection stubs | `rig/benchmarks/docker_gated.py` | `docker info` live-checked |
| Offline eval CI job | `.github/workflows/ci.yml` | this PR's checks |
| Local corpus: 5 tasks, all with oracles | `eval/tasks/*.json` | `--oracle-check` 5/5 |

## Verification actually run

- `uv run pytest -q` — 55 tests green (Python 3.12 via uv).
- `run.py --oracle-check` — 5/5 oracles pass.
- `run.py --agents oracle,fail --seeds 2` — 20 cells: oracle 10/10 pass,
  fail 0/10 pass, report renders pass@2/pass^2 correctly.
- `run.py --report --k-report 2` — renders from store without running.
- Docker: `docker info` succeeds (OrbStack 2.2.3).
- τ² results schema verified against cloned repo
  (`data_model/simulation.py`: SimulationRun.reward_info.reward,
  termination_reason, trial fields) — parse covered by fixture test.
- LCB `_eval_all.json` schema verified against cloned repo
  (`runner/main.py`, `utils/path_utils.py`) — parse covered by fixture test.

## NOT yet verified (honest gaps)

- **No paid run has executed.** τ²/LCB `run()` paths are written but
  have never run against the real harnesses — expect first-run fixes
  (litellm model naming for LEK, LCB runner deps, output paths).
- overseer/mini arms require `LEK_API_KEY`; not exercised this round.
- Docker-gated trio (SWE-bench Verified, Terminal-Bench via Harbor,
  SWE-rebench) are detection+guidance stubs only — runners not
  implemented.
- Single model family available (LegionEdge) — the playbook's ≥3-family
  comparison is blocked on additional provider access.
- `temperature` is pinned to 0.0 in the LCB adapter call; τ² uses the
  harness defaults. Make explicit when runs are approved.

## Usage

```bash
cd eval
uv run pytest -q                          # rig self-tests (no spend)
uv run python run.py --oracle-check       # task solvability
uv run python run.py --agents oracle,fail --seeds 3   # offline matrix
uv run python run.py --report             # report card from store

# Paid paths (require LEK_API_KEY + approval):
uv run python run.py --agents overseer,mini --seeds 3
uv run python run.py --benchmark tau2 --tau2-domain airline \
    --tau2-user-llm openai/<user-model> --tau2-trials 8
uv run python run.py --benchmark lcb --release-version v6
```

External checkouts: `OVERSEER_TAU2_DIR`, `OVERSEER_LCB_DIR` (defaults
`eval/vendor/tau2-bench`, `eval/vendor/LiveCodeBench`; vendor/ is
gitignored).
