# Eval Ledger — Phase 4

Working record of what the rig does, what was verified, and what is
deferred. Every claim below cites a check that actually ran.

## Decisions (user-approved)

- Model access: **hosted gateway only** (`OVERSEER_API_KEY`/`OVERSEER_BASE_URL`).
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
| SWE-bench Verified adapter (official harness) | `rig/benchmarks/swebench.py` | real docker eval: gold patch on `sympy__sympy-22914` → resolved=true |
| Harbor adapter — Terminal-Bench 2.x + SWE-rebench | `rig/benchmarks/harbor.py` | real oracle job (tb2) + real swe-rebench task `BerriAI__litellm-14715` → pass |
| Held-out suite + canary audit | `heldout/tasks/`, `rig/audit.py` | 5/5 oracles; `--audit` clean |
| Ablation arms `overseer@<preset>` + `--no-tools` | `rig/agents/__init__.py`, `tools/mod.rs` | manifest shows specs removed + dispatch refused |
| Offline eval CI job | `.github/workflows/ci.yml` | this PR's checks |
| Local corpus: 15 public + 5 held-out tasks | `eval/tasks/`, `eval/heldout/tasks/` | `--oracle-check` 15/15 + 5/5 |

## Verification actually run

- `uv run pytest -q` — 68 tests green (Python 3.12 via uv).
- `run.py --oracle-check` — 5/5 oracles pass (build-and-test grader
  hardened to v3 after review: test functions must execute, `||`
  import-fallback removed).
- `run.py --agents oracle,fail --seeds 3 --noninferiority-pp 3` — 30
  cells: oracle 15/15, fail 0/15; paired-delta section renders
  (−100pp, McNemar p<0.001, non-inferiority NO).
- `run.py --report --k-report 3` — renders from store without running;
  arms keyed by run_set_id (no cross-matrix pooling — two stored
  matrices render as four rows); model=None arms don't crash.
- Docker: `docker info` succeeds (OrbStack 2.2.3).
- SWE-bench: gold-patch eval on `sympy__sympy-22914` — arm64 image pull,
  FAIL_TO_PASS+PASS_TO_PASS ran, `resolved=true` → RunRecord. Dataset
  materialized locally (`SWE-bench/SWE-bench_Verified`, `image` column)
  with x86_64→arm64 image-name rewrite; eval-only mode collapses seeds
  to one (identical predictions → identical eval) and records
  deterministic run_ids.
- Terminal-Bench: `harbor run -d terminal-bench@2.0 -a oracle -k 1` real
  job → reward 1.0 → RunRecord (wall_s, checksum, task commit recorded).
- SWE-rebench: real oracle task `BerriAI__litellm-14715` passed.
- Held-out: `--task-dir heldout/tasks` matrix runs; `--audit` reports
  5 canaries, 0 findings; canary scan streams arbitrarily large
  trajectories.
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
11. `mini` ran model-controlled bash with `OVERSEER_API_KEY` in env. Fixed:
    OVERSEER_* scrubbed from the task shell.
12. Stale `eval/solvers/` + Inspect-AI-era README. Removed/rewritten.

## Independent review — round 2 findings, all addressed

1. `run_cell()` got `runs_root=results_root` so sessions/manifests
   landed in `results/<run_id>` while `build_record` looked in
   `results/runs/<run_id>` — provenance never harvested. Fixed: both
   initial and retry calls pass `results_root/"runs"`; regression test
   writes a manifest through a solver and asserts harvest + file path.
2. Paired markdown table dropped a column when `--noninferiority-pp`
   was omitted (header had one more cell than rows). Fixed: header/sep
   built dynamically; Δ column labeled `Δ(A−B)` and verdict column
   `A non-inf vs B`; column-count consistency covered by tests.
3. Report provenance block assumed dict-shaped manifest fields; a
   string-shaped `model`/`harness` would crash rendering. Fixed via
   `_d()` coercion; covered by string-fields test.
4. τ² trial fallback used `list.index(sim)` — could collide on
   identical dicts or an explicit sibling trial. Fixed: smallest unused
   index per task via a used-set.
5. `model_response` usage keys were hard-indexed — a malformed event
   aborted the whole matrix. Fixed: `.get`-defensive reads in the
   overseer arm; scheduler records `KeyError` as infra failure.
6. McNemar p printed as `0.000` for p<0.001. Fixed: `<0.001`.

## Independent review — round 3 findings, all addressed

1. `--swebench-ids` sorted before filtering → ValueError on any proper
   subset. Fixed: filter first, then sort.
2. SWE-bench rollout workdir never reset — a reused clone carried the
   previous seed's staged diff into the next `model_patch`. Fixed:
   `git reset --hard <base_commit> && git clean -fdx` on reuse.
3. `patch_successfully_applied=False` was mapped to `infra_error`,
   letting a do-nothing agent escape the pass@1 denominator. Fixed:
   scored failure per SWE-bench convention; regression test renamed.
4. Harbor `agent_info.model_info` is a dict — landing it in
   `record["model"]` would make `group_key` raise unhashable-dict.
   Fixed: `_model_label` extracts `.name`.
5. `agents.REGISTRY[name]` in the swe_bench path crashed on
   `overseer@<preset>` ablation arms. Fixed: `agents.get`.
6. Canary audit under-reported coverage (scanned only runs with
   sightings) and skipped files >5 MB silently. Fixed: streamed scan
   with boundary overlap; `scanned_runs` counts runs actually read.
7. Eval-only swe_bench ignored `--swebench-limit` and reran the same
   eval k times under different seed labels. Fixed: `seeds=[0]` +
   limit applied via `--instance_ids`.
8. `env_digest` couldn't distinguish x86_64 from arm64 evals. Fixed:
   arch is part of the digest.
9. Relative `--predictions-path` resolved under `cwd=logs_root`.
   Fixed: `.resolve()` before spawn.
10. `needs_key` gating missed benchmark rollout paths. Fixed: swe_bench
    rollouts and harbor LLM agents fail fast without `OVERSEER_API_KEY`.
11. `--no-tools` dispatch ran the permission gate first — a TUI user
    could be prompted for a tool that can never run, and the prompt
    still advertised ablated tools. Fixed: disabled check precedes
    `policy.gate`; the `task` delegation line and skills index segment
    are gated on `disabled_tools`.
12. `profile.rs` FALLBACK priced unknown models at mid-tier rates —
    fabricated nonzero `cost_usd`. Fixed: manifest records
    `model.cost_basis` ("profiled"|"estimated") so reports can
    distinguish measured from estimated spend.
13. Deterministic `run_id` for eval-only ingests (`{rsid}-s{seed}-{iid}`)
    — re-parsing a log dir no longer mints duplicate store rows.
14. `evaluate()` TimeoutExpired propagated uncaught → BenchmarkUnavailable.
    `_diff` decodes with surrogateescape (binary-safe patches).
    `retry-backoff` instruction now matches its grader; `fix-fstring-py38`
    rewritten with a genuine syntax error; held-out grader writes to a
    workspace-relative temp. Wall-timeout documented as an agent outcome.

## NOT yet verified (honest gaps)

- **No paid run has executed.** τ²/LCB `run()` paths are written but
  have never run against the real harnesses — expect first-run fixes
  (litellm model naming for the gateway, LCB runner deps, output paths).
- overseer/mini arms require `OVERSEER_API_KEY`; not exercised this round.
- SWE-bench **rollout** generation (agent solves instances → patches)
  is written and key-gated but unexercised — only the eval path ran.
- `--no-tools` ablation still leaves non-tool prompt text unchanged
  except the task/skill mentions that are now gated; other references
  to removed capabilities (e.g. memory index) are not conditioned.
- Single model family available (Fleet) — the playbook's ≥3-family
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

# Paid paths (require OVERSEER_API_KEY + approval):
uv run python run.py --agents overseer,mini --seeds 3
uv run python run.py --benchmark tau2 --tau2-domain airline \
    --tau2-user-llm openai/<user-model> --tau2-trials 8
uv run python run.py --benchmark lcb --release-version v6
```

External checkouts: `OVERSEER_TAU2_DIR`, `OVERSEER_LCB_DIR` (defaults
`eval/vendor/tau2-bench`, `eval/vendor/LiveCodeBench`; vendor/ is
gitignored).
