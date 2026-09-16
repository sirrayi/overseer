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
    return (r.get("benchmark"), r.get("harness"), r.get("model"))


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


def render(
    records: list[dict],
    *,
    k: int = 3,
    boot_seed: int = 0,
    n_boot: int = 2000,
    title: str = "overseer eval report",
    contamination_notes: str | None = None,
) -> tuple[str, dict]:
    summaries = summarize_arms(records, k=k, boot_seed=boot_seed, n_boot=n_boot)

    lines = [f"# {title}", ""]
    lines.append(
        "| benchmark | harness | model | tasks | trials | pass@1 ±95%CI "
        f"| pass@{k} | pass^{k} | med $/task | p50 steps | p90 steps |"
    )
    lines.append("|---|---|---|---|---|---|---|---|---|---|---|")
    for (bench, harness, model), s in sorted(summaries.items()):
        p1 = s.get("pass_at_1", {})
        ci = p1.get("ci95") or [None, None]
        cost = f"${s['cost_usd']['median']:.3f}" if s.get("cost_usd") else "—"
        lines.append(
            f"| {bench} | {harness} | {model} | {s.get('tasks', 0)} "
            f"| {s.get('trials', 0)} | {_fmt_pct(p1.get('mean'))} "
            f"[{_fmt_pct(ci[0])}–{_fmt_pct(ci[1])}] "
            f"| {_fmt_pct(s.get('pass_at_k', {}).get('mean'))} "
            f"| {_fmt_pct(s.get('pass_pow_k', {}).get('mean'))} "
            f"| {cost} "
            f"| {s.get('steps', {}).get('p50', '—')} "
            f"| {s.get('steps', {}).get('p90', '—')} |"
        )

    # Provenance block — the §4.5 field set, from the overseer manifest.
    prov_rows = []
    for r in records:
        if r.get("provenance"):
            prov_rows.append(r)
            break
    if prov_rows:
        prov = prov_rows[0]["provenance"]
        mp = prov.get("system_prompt") or {}
        tl = prov.get("tools") or {}
        mdl = prov.get("model") or {}
        lim = prov.get("limits") or {}
        lines += [
            "",
            "## Provenance",
            "",
            f"- harness: overseer {(prov.get('harness') or {}).get('version')} "
            f"commit {(prov.get('harness') or {}).get('commit')}",
            f"- model: {mdl.get('name')} via {mdl.get('provider')} "
            f"(effort={mdl.get('effort')}, temp={mdl.get('temperature')})",
            f"- system prompt sha256: `{mp.get('sha256')}` (scope {mp.get('scope')})",
            f"- tools sha256: `{tl.get('sha256')}` — {tl.get('count')} tools",
            f"- limits: steps≤{lim.get('max_steps')} "
            f"cost≤${lim.get('max_cost_usd')} out≤{lim.get('max_output_tokens')} tok",
            f"- policy: {(prov.get('policy') or {}).get('preset')} "
            f"sandbox={(prov.get('policy') or {}).get('sandbox_bash')}",
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
        "contamination_notes": contamination_notes,
    }
