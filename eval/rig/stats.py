"""Statistics per playbook Ch.8 §4 + §7.2 — the methods the reporting
standard requires:

- pass@1 mean ± 95% CI via **cluster bootstrap over tasks** (resample
  tasks with replacement, carrying all of a task's seeds — within-task
  correlation survives the resample). Single-run pass@1 can't resolve
  <4–5pp; the CI is the honest number.
- pass@k — Chen et al. unbiased estimator: 1 - C(n-c,k)/C(n,k)
- pass^k — all-k-succeed reliability: C(c,k)/C(n,k), 0 when c<k
  (τ-bench's metric; GPT-4o went 61% pass^1 → <25% pass^8 on retail)
- paired comparisons — per-task rate diff, cluster-bootstrapped;
  McNemar exact on (task, seed)-paired discordant runs as a secondary
  heuristic (ignores within-task correlation — reported, not trusted)
- percentiles p50/p90 for steps/tokens, median cost/task

Bootstrap RNGs are seeded and the seed is recorded in the output — a
reported CI is reproducible bit-for-bit.
"""

from __future__ import annotations

import random
from math import comb
from statistics import median

# ---------- unbiased estimators ----------


def pass_at_k(n: int, c: int, k: int) -> float:
    """P(at least one of k draws succeeds) estimated from n trials with
    c successes — Chen et al.: 1 - C(n-c,k)/C(n,k)."""
    if n <= 0 or c < 0 or c > n:
        raise ValueError(f"bad trials: n={n} c={c}")
    if k < 1 or k > n:
        raise ValueError(f"k={k} out of range for n={n}")
    if n - c < k:  # can't draw k failures → guaranteed success
        return 1.0
    return 1.0 - comb(n - c, k) / comb(n, k)


def pass_pow_k(n: int, c: int, k: int) -> float:
    """P(all k draws succeed) = C(c,k)/C(n,k); 0 when c<k."""
    if n <= 0 or c < 0 or c > n:
        raise ValueError(f"bad trials: n={n} c={c}")
    if k < 1 or k > n:
        raise ValueError(f"k={k} out of range for n={n}")
    if c < k:
        return 0.0
    return comb(c, k) / comb(n, k)


# ---------- helpers ----------


def percentile(xs: list[float], q: float) -> float:
    """Linear-interpolation percentile (numpy 'linear' equivalent)."""
    if not xs:
        raise ValueError("percentile of empty data")
    ys = sorted(xs)
    if len(ys) == 1:
        return ys[0]
    pos = (len(ys) - 1) * q / 100.0
    lo = int(pos)
    frac = pos - lo
    if lo + 1 >= len(ys):
        return ys[-1]
    return ys[lo] + frac * (ys[lo + 1] - ys[lo])


def bootstrap_ci(
    values: list[float], *, seed: int = 0, n_boot: int = 2000, level: float = 0.95
) -> tuple[float, float, float]:
    """Percentile bootstrap CI over cluster units. Returns (mean, lo, hi).
    Each unit contributes one float; resampling is by unit — for eval data
    the unit is the task, so within-task seed correlation is preserved."""
    if not values:
        raise ValueError("bootstrap of empty data")
    rng = random.Random(seed)
    n = len(values)
    means = []
    for _ in range(n_boot):
        means.append(sum(rng.choice(values) for _ in range(n)) / n)
    tail = (1.0 - level) / 2 * 100
    return sum(values) / n, percentile(means, tail), percentile(means, 100 - tail)


def mcnemar_exact(b01: int, b10: int) -> float:
    """Two-sided exact McNemar p on discordant pair counts.
    b01 = pairs where A passed, B failed; b10 = A failed, B passed."""
    n = b01 + b10
    if n == 0:
        return 1.0
    k = min(b01, b10)
    # P(X <= k) for X ~ Bin(n, 0.5), doubled
    p = 2 * sum(comb(n, i) for i in range(k + 1)) / (2**n)
    return min(1.0, p)


# ---------- record-level summaries ----------


def _is_trial(r: dict) -> bool:
    """Infra failures are not agent trials — excluded from n/c but counted."""
    return not r.get("infra_error")


def task_cells(records: list[dict]) -> dict[str, dict]:
    """task_id → {n, c, costs, steps, tokens, infra}. Only valid trials
    (non-infra) count toward n/c."""
    cells: dict[str, dict] = {}
    for r in records:
        cell = cells.setdefault(
            r["task_id"],
            {
                "n": 0,
                "c": 0,
                "infra": 0,
                "costs": [],
                "steps": [],
                "tokens": [],
                "walls": [],
            },
        )
        if not _is_trial(r):
            cell["infra"] += 1
            continue
        cell["n"] += 1
        cell["c"] += 1 if r.get("pass") else 0
        for key, field in (
            ("cost_usd", "costs"),
            ("steps", "steps"),
            ("tokens_in", "tokens"),
            ("wall_s", "walls"),
        ):
            v = r.get(key)
            if v is not None:
                cell[field].append(v)
    return cells


def summarize(
    records: list[dict], *, k: int = 1, boot_seed: int = 0, n_boot: int = 2000
) -> dict:
    """Summarize one arm's records. `k` is the reliability order for
    pass@k / pass^k (tasks with fewer than k valid trials are excluded
    from those means — the exclusion count is reported)."""
    cells = task_cells(records)
    task_ids = sorted(cells)
    if not task_ids:
        return {"tasks": 0, "trials": 0, "infra_failures": 0}

    rates = [cells[t]["c"] / cells[t]["n"] for t in task_ids if cells[t]["n"] > 0]
    mean, lo, hi = (
        bootstrap_ci(rates, seed=boot_seed, n_boot=n_boot) if rates else (0.0, 0.0, 0.0)
    )

    eligible = [t for t in task_ids if cells[t]["n"] >= k]
    trials = [r for r in records if _is_trial(r)]
    costs = [r["cost_usd"] for r in trials if r.get("cost_usd") is not None]
    steps = [r["steps"] for r in trials if r.get("steps") is not None]
    tokens = [
        r["tokens_in"] + (r.get("tokens_out") or 0)
        for r in trials
        if r.get("tokens_in") is not None
    ]
    walls = [r["wall_s"] for r in trials if r.get("wall_s") is not None]

    out = {
        "tasks": len(task_ids),
        "trials": len(trials),
        "infra_failures": sum(c["infra"] for c in cells.values()),
        "pass_at_1": {
            "mean": mean,
            "ci95": [lo, hi],
            "boot_seed": boot_seed,
            "n_boot": n_boot,
        },
        "k": k,
        "pass_at_k": {
            "mean": (
                sum(pass_at_k(cells[t]["n"], cells[t]["c"], k) for t in eligible)
                / len(eligible)
            )
            if eligible
            else None,
            "tasks_eligible": len(eligible),
        },
        "pass_pow_k": {
            "mean": (
                sum(pass_pow_k(cells[t]["n"], cells[t]["c"], k) for t in eligible)
                / len(eligible)
            )
            if eligible
            else None,
            "tasks_eligible": len(eligible),
        },
    }
    if costs:
        out["cost_usd"] = {"median": median(costs), "total": sum(costs)}
    if steps:
        out["steps"] = {"p50": percentile(steps, 50), "p90": percentile(steps, 90)}
    if tokens:
        out["tokens"] = {"p50": percentile(tokens, 50), "p90": percentile(tokens, 90)}
    if walls:
        out["wall_s"] = {"p50": percentile(walls, 50), "p90": percentile(walls, 90)}
    return out


def paired_diff(
    records_a: list[dict],
    records_b: list[dict],
    *,
    boot_seed: int = 0,
    n_boot: int = 2000,
    noninferiority_pp: float | None = None,
) -> dict:
    """Paired per-task comparison A vs B over common tasks.

    diff = mean(per-task pass-rate of A) - mean(per-task pass-rate of B),
    cluster-bootstrapped over tasks. `noninferiority_pp` (e.g. 3.0) tests
    the regression gate: B is non-inferior if CI lower bound of
    (B - A) > -delta — call paired_diff(b, a) for that direction.

    McNemar runs on (task_id, seed) pairs — a useful heuristic but it
    treats repeated seeds as independent, so the bootstrap is authoritative.
    """
    cells_a = task_cells(records_a)
    cells_b = task_cells(records_b)
    common = sorted(set(cells_a) & set(cells_b))
    common = [t for t in common if cells_a[t]["n"] > 0 and cells_b[t]["n"] > 0]
    if not common:
        return {"tasks_common": 0}

    diffs = [
        cells_a[t]["c"] / cells_a[t]["n"] - cells_b[t]["c"] / cells_b[t]["n"]
        for t in common
    ]
    mean, lo, hi = bootstrap_ci(diffs, seed=boot_seed, n_boot=n_boot)

    # McNemar over (task, seed) pairs
    def pair_key(r):
        return (r["task_id"], r.get("seed"))

    pa = {pair_key(r): bool(r.get("pass")) for r in records_a if _is_trial(r)}
    pb = {pair_key(r): bool(r.get("pass")) for r in records_b if _is_trial(r)}
    both = set(pa) & set(pb)
    b01 = sum(1 for k in both if pa[k] and not pb[k])
    b10 = sum(1 for k in both if not pa[k] and pb[k])

    out = {
        "tasks_common": len(common),
        "diff": {"mean": mean, "ci95": [lo, hi], "boot_seed": boot_seed},
        "mcnemar": {"a_only": b01, "b_only": b10, "p": mcnemar_exact(b01, b10)},
    }
    if noninferiority_pp is not None:
        out["noninferiority"] = {
            "delta_pp": noninferiority_pp,
            # A non-inferior to B: lower bound of (A - B) CI > -delta
            "pass": lo > -noninferiority_pp / 100.0,
        }
    return out
