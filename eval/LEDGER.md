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

- `uv run pytest -q` — 64 tests green (Python 3.12 via uv).
- `run.py --oracle-check` — 5/5 oracles pass (build-and-test grader
  hardened to v3 after review: test functions must execute, `||`
  import-fallback removed).
- `run.py --agents oracle,fail --seeds 2 --noninferiority-pp 3` — 20
  cells: oracle 10/10, fail 0/10; paired-delta section renders
  (−100pp, McNemar p=0.002, non-inferiority NO).
- `run.py --report` — renders from store without running; arms keyed by
  run_set_id (no cross-matrix pooling); model=None arms don't crash.
- Docker: `docker info` succeeds (OrbStack 2.2.3).
- τ² results schema verified against cloned repo
  (`data_model/simulation.py`: SimulationRun.reward_info.reward,
  termination_reason, trial fields) — parse covered by fixture test.
  Termination→infra mapping aligned with upstream
  NON_EVALUABLE_TERMINATION_REASONS = {infrastructure_error,
  context_window_exceeded}; all other terminations are scored failures.
- LCB `_eval_all.json` schema verified against cloned repo
  (`runner/main.py`, `utils/path_utils.py`) — parse covered by fixture
  test. Missing token/cost fields stay `None` (never fabricated zeros).

## Independent review — round 1 findings, all addressed

1. `--model` was bound at import time → silently ignored. Fixed: agents
   resolve env per call (`_cfg()`).
2. Report pooled arms across run sets and crashed on `model=None` rows.
   Fixed: `run_set_id` is part of arm identity; None-safe sort.
3. `build-and-test` grader `||` chain passed on import alone. Fixed:
   grader v3 executes every `test_*` function (≥2 required).
4. τ² termination mapping diverged from upstream in both directions.
   Fixed: aligned to NON_EVALUABLE = {infrastructure_error,
   context_window_exceeded}.
5. `paired_diff`/`--noninferiority-pp` parsed but never wired. Fixed:
   report emits a Paired deltas section per run set.
6. `manifest.model` as string would crash build_record. Fixed: shape
   guard; outcome.model fallback.
7. `tokens_out` dropped reasoning tokens. Fixed: output+reasoning.
8. Adapters fabricated `0` for absent cost/token fields. Fixed: `None`.
9. Retry bookkeeping: session_dir could point at the failed attempt;
   successful retry unmarked. Fixed: retry is always the recorded attempt.
10. `extra` keys could clobber matrix-header fields. Fixed: core keys win.
11. `mini` ran model-controlled bash with `LEK_API_KEY` in env. Fixed:
    LEK_* scrubbed from the task shell.
12. Stale `eval/solvers/` + Inspect-AI-era README. Removed/rewritten.

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
