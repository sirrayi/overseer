"""Report card (playbook Ch.8 §7.3.7, Ch.12 §4.5).

Every published number ships with: task set+revision, harness name+commit,
system-prompt hash, tool inventory, model+effort, temperature policy,
seeds, step/$/time limits, cost/task, tokens/task, pass@1±95%CI, pass^k,
contamination notes. This document is itself a differentiator — most
leaderboard entries can't produce it.

`render` emits a markdown table for humans and a JSON block with the
complete field set for the record.
"""

from __future__ import annotations

from collections import defaultdict

from . import stats


def group_key(r: dict) -> tuple:
    # run_set_id is part of the arm identity: two matrices of the same arm
    # are separate measurements, never silently pooled. A report that wants
    # cross-matrix aggregation must say so explicitly.
    return (
        r.get("benchmark"),
        r.get("harness"),
        r.get("model"),
        r.get("run_set_id"),
    )


def summarize_arms(
    records: list[dict], *, k: int, boot_seed: int = 0, n_boot: int = 2000
) -> dict[tuple, dict]:
    arms: dict[tuple, list[dict]] = defaultdict(list)
    for r in records:
        if r.get("kind") == "run":
            arms[group_key(r)].append(r)
    return {
        g: stats.summarize(rs, k=k, boot_seed=boot_seed, n_boot=n_boot)
        for g, rs in arms.items()
    }


def _fmt_pct(v) -> str:
    return "—" if v is None else f"{100 * v:.1f}%"


def _sort_key(g: tuple) -> tuple:
    # None-safe ordering (infra-only arms can carry model=None)
    return tuple("" if x is None else str(x) for x in g)


def paired_sections(
    records: list[dict],
    *,
    noninferiority_pp: float | None = None,
    boot_seed: int = 0,
    n_boot: int = 2000,
) -> list[dict]:
    """Pairwise A/B deltas within each run set. Arms in one matrix share
    (benchmark, run_set_id); every pair is compared — differing in harness
    or model is what the factorial design is for."""
    by_set: dict[tuple, dict[tuple, list[dict]]] = defaultdict(
        lambda: defaultdict(list)
    )
    for r in records:
        if r.get("kind") == "run":
            by_set[(r.get("benchmark"), r.get("run_set_id"))][
                (r.get("harness"), r.get("model"))
            ].append(r)
    out = []
    for (bench, rs), arms in sorted(by_set.items(), key=lambda kv: _sort_key(kv[0])):
        keys = sorted(arms, key=_sort_key)
        for i in range(len(keys)):
            for j in range(i + 1, len(keys)):
                a, b = keys[i], keys[j]
                d = stats.paired_diff(
                    arms[a],
                    arms[b],
                    boot_seed=boot_seed,
                    n_boot=n_boot,
                    noninferiority_pp=noninferiority_pp,
                )
                if d.get("tasks_common"):
                    out.append(
                        {
                            "run_set_id": rs or "—",
                            "a": "/".join(str(x) for x in a),
                            "b": "/".join(str(x) for x in b),
                            **d,
                        }
                    )
    return out


def render(
    records: list[dict],
    *,
    k: int = 3,
    boot_seed: int = 0,
    n_boot: int = 2000,
    title: str = "overseer eval report",
    contamination_notes: str | None = None,
    noninferiority_pp: float | None = None,
) -> tuple[str, dict]:
    summaries = summarize_arms(records, k=k, boot_seed=boot_seed, n_boot=n_boot)

    lines = [f"# {title}", ""]
    lines.append(
        "| benchmark | harness | model | run set | tasks | trials | "
        f"pass@1 ±95%CI | pass@{k} | pass^{k} | med $/task | p50 steps | "
        "p90 steps |"
    )
    lines.append("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for (bench, harness, model, rs), s in sorted(
        summaries.items(), key=lambda kv: _sort_key(kv[0])
    ):
        p1 = s.get("pass_at_1", {})
        ci = p1.get("ci95") or [None, None]
        cost = f"${s['cost_usd']['median']:.3f}" if s.get("cost_usd") else "—"
        lines.append(
            f"| {bench} | {harness} | {model or '—'} | {(rs or '—')[:8]} "
            f"| {s.get('tasks', 0)} "
            f"| {s.get('trials', 0)} | {_fmt_pct(p1.get('mean'))} "
            f"[{_fmt_pct(ci[0])}–{_fmt_pct(ci[1])}] "
            f"| {_fmt_pct(s.get('pass_at_k', {}).get('mean'))} "
            f"| {_fmt_pct(s.get('pass_pow_k', {}).get('mean'))} "
            f"| {cost} "
            f"| {s.get('steps', {}).get('p50', '—')} "
            f"| {s.get('steps', {}).get('p90', '—')} |"
        )

    # Paired per-task deltas — the A/B claim (paired_diff is authoritative;
    # McNemar is reported alongside as a secondary heuristic). The delta is
    # (A − B); the non-inferiority verdict tests whether A's CI lower bound
    # stays above −delta vs B.
    paired = paired_sections(
        records, noninferiority_pp=noninferiority_pp, boot_seed=boot_seed, n_boot=n_boot
    )
    if paired:
        has_ni = noninferiority_pp is not None
        header = "| run set | A | B | tasks | Δ(A−B) pass@1 ±95%CI | McNemar p |"
        sep = "|---|---|---|---|---|---|"
        if has_ni:
            header += f" A non-inf vs B ≥−{noninferiority_pp:g}pp |"
            sep += "---|"
        lines += [
            "",
            "## Paired deltas (cluster bootstrap over tasks)",
            "",
            header,
            sep,
        ]
        for p in paired:
            pval = p["mcnemar"]["p"]
            pstr = "<0.001" if pval < 0.001 else f"{pval:.3f}"
            row = (
                f"| {p['run_set_id'][:8]} | {p['a']} | {p['b']} "
                f"| {p['tasks_common']} "
                f"| {_fmt_pct(p['diff']['mean'])} "
                f"[{_fmt_pct(p['diff']['ci95'][0])}–"
                f"{_fmt_pct(p['diff']['ci95'][1])}] "
                f"| {pstr} |"
            )
            if has_ni:
                ok = p["noninferiority"]["pass"]
                row += f" {'yes' if ok else 'NO'} |"
            lines.append(row)

    # Provenance block — the §4.5 field set, from the overseer manifest.
    def _d(x) -> dict:
        # manifest fields may arrive as strings from external adapters —
        # tolerate, never crash the report.
        return x if isinstance(x, dict) else {}

    prov_rows = []
    for r in records:
        if r.get("provenance"):
            prov_rows.append(r)
            break
    if prov_rows:
        prov = prov_rows[0]["provenance"]
        mp = _d(prov.get("system_prompt"))
        tl = _d(prov.get("tools"))
        mdl = _d(prov.get("model"))
        lim = _d(prov.get("limits"))
        pol = _d(prov.get("policy"))
        hrn = _d(prov.get("harness"))
        lines += [
            "",
            "## Provenance",
            "",
            f"- harness: overseer {hrn.get('version')} commit {hrn.get('commit')}",
            f"- model: {mdl.get('name') or prov.get('model')} "
            f"via {mdl.get('provider')} "
            f"(effort={mdl.get('effort')}, temp={mdl.get('temperature')})",
            f"- system prompt sha256: `{mp.get('sha256')}` (scope {mp.get('scope')})",
            f"- tools sha256: `{tl.get('sha256')}` — {tl.get('count')} tools",
            f"- limits: steps≤{lim.get('max_steps')} "
            f"cost≤${lim.get('max_cost_usd')} out≤{lim.get('max_output_tokens')} tok",
            f"- policy: {pol.get('preset')} sandbox={pol.get('sandbox_bash')}",
            f"- bootstrap: {n_boot} resamples, seed {boot_seed}",
            f"- contamination: {contamination_notes or 'none declared'}",
        ]
    md = "\n".join(lines) + "\n"
    return md, {
        "title": title,
        "k": k,
        "boot_seed": boot_seed,
        "n_boot": n_boot,
        "arms": {str(g): s for g, s in summaries.items()},
        "paired": paired,
        "contamination_notes": contamination_notes,
    }
