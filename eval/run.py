#!/usr/bin/env python3
"""eval/run.py — paired harness benchmark runner (playbook Ch.12 §4).

Matrix: tasks × agents × k seeds → store → report. Overseer arm harvests
the session manifest (P4.5 provenance). The mini arm is the frozen null
scaffold; `oracle` verifies task solvability with zero spend; `fail`
self-tests the rig.

  uv run python run.py                          # overseer vs mini, 1 seed
  uv run python run.py --seeds 3 --agents overseer,mini
  uv run python run.py --agents oracle          # solvability check, no API
  uv run python run.py --report                 # report card from store

Env: LEK_API_KEY (required for overseer/mini), LEK_BASE_URL, LEK_MODEL,
OVERSEER_BIN.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
sys.path.insert(0, str(ROOT))

from rig import graders, report, scheduler, store, taskspec

RESULTS = ROOT / "results"
WORKSPACES = ROOT / "workspaces"


def harness_commit() -> str:
    try:
        p = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            cwd=REPO,
            capture_output=True,
            text=True,
        )
        return p.stdout.strip() if p.returncode == 0 else "unknown"
    except Exception:
        return "unknown"


def progress(r: dict) -> None:
    mark = "INFRA" if r.get("infra_error") else ("PASS" if r.get("pass") else "FAIL")
    print(
        f"  {mark} {r['task_id']} × {r['harness']} s{r['seed']} "
        f"steps={r.get('steps', '?')} wall={r.get('wall_s', '?')}s "
        f"in={r.get('tokens_in', '?')} cost={r.get('cost_usd', '?')}",
        flush=True,
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--agents", "--solver", default="overseer,mini")
    ap.add_argument("--tasks", default="all")
    ap.add_argument("--seeds", "-k", type=int, default=1)
    ap.add_argument("--model", default=None)
    ap.add_argument(
        "--benchmark",
        default="local",
        choices=["local", "tau2", "lcb", "swe_bench", "terminal_bench", "swe_rebench"],
    )
    ap.add_argument("--tau2-domain", default="airline", choices=["airline", "retail"])
    ap.add_argument("--tau2-trials", type=int, default=8)
    ap.add_argument(
        "--tau2-user-llm",
        default=None,
        help="pinned user-simulator model (required for real tau2 runs)",
    )
    ap.add_argument(
        "--release-version", default="v6", help="LiveCodeBench release window (e.g. v6)"
    )
    ap.add_argument("--scheduler-seed", type=int, default=0)
    ap.add_argument(
        "--oracle-check",
        action="store_true",
        help="verify every task's oracle passes; no agents run",
    )
    ap.add_argument(
        "--report",
        action="store_true",
        help="render the report card from the store (no runs)",
    )
    ap.add_argument(
        "--k-report",
        type=int,
        default=3,
        help="reliability order k for pass@k/pass^k in the report",
    )
    ap.add_argument(
        "--noninferiority-pp",
        type=float,
        default=None,
        help="paired-diff non-inferiority bound in points (e.g. 3)",
    )
    args = ap.parse_args()

    if args.model:
        os.environ["LEK_MODEL"] = args.model

    st = store.Store(RESULTS / "store.jsonl")

    if args.report:
        records = st.runs()
        md, js = report.render(
            records,
            k=args.k_report,
            noninferiority_pp=args.noninferiority_pp,
            contamination_notes=None,
        )
        print(md)
        out = RESULTS / f"report-{int(__import__('time').time())}"
        out.with_suffix(".md").write_text(md)
        out.with_suffix(".json").write_text(json.dumps(js, indent=2))
        print(f"wrote {out}.md / .json")
        return 0

    if args.benchmark != "local":
        from rig.benchmarks import docker_gated, lcb, tau2

        adapter = {
            "tau2": tau2.Tau2Adapter,
            "lcb": lcb.LcbAdapter,
            "swe_bench": docker_gated.SweBenchAdapter,
            "terminal_bench": docker_gated.TerminalBenchAdapter,
            "swe_rebench": docker_gated.SweRebenchAdapter,
        }[args.benchmark]()
        if args.benchmark == "tau2":
            return tau2.run_cli(adapter, args, st, harness_commit(), progress)
        if args.benchmark == "lcb":
            return lcb.run_cli(adapter, args, st, harness_commit(), progress)
        try:
            adapter.check_or_raise()
        except Exception as e:
            print(e)
            return 2
        print(
            f"{args.benchmark}: docker present — adapter run not yet "
            "implemented (tracked in LEDGER)."
        )
        return 2

    tasks = taskspec.load_dir(ROOT / "tasks")
    if args.tasks != "all":
        keep = set(args.tasks.split(","))
        tasks = [t for t in tasks if t.id in keep]

    if args.oracle_check:
        bad = 0
        for t in tasks:
            ok, why = graders.oracle_check(t, WORKSPACES / "_oracle" / t.id)
            print(f"  {'OK ' if ok else 'BAD'} {t.id}: {why}")
            bad += 0 if ok else 1
        print(f"{len(tasks) - bad}/{len(tasks)} oracles pass")
        return 1 if bad else 0

    agent_names = [a.strip() for a in args.agents.split(",")]
    needs_key = [a for a in agent_names if a in ("overseer", "mini")]
    if needs_key and not os.environ.get("LEK_API_KEY"):
        sys.exit("LEK_API_KEY required for " + ",".join(needs_key))

    print(f"matrix: {len(tasks)} tasks × {agent_names} × {args.seeds} seeds")
    records = scheduler.run_matrix(
        tasks,
        agent_names,
        args.seeds,
        results_root=RESULTS,
        ws_root=WORKSPACES,
        store=st,
        benchmark="local",
        scheduler_seed=args.scheduler_seed,
        harness_commit=harness_commit(),
        on_progress=progress,
    )

    # Post-run summary against just this matrix's records.
    md, _ = report.render(
        records, k=args.k_report, noninferiority_pp=args.noninferiority_pp
    )
    print("\n" + md)
    return 0


if __name__ == "__main__":
    sys.exit(main())
