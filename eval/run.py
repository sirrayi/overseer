#!/usr/bin/env python3
"""eval/run.py — paired harness benchmark runner (playbook Ch.12 §0.6/0.7).

For each task × solver: fresh workspace → setup → solver run → deterministic
grader → trajectory + metrics keyed by {task_id, version, solver, model,
harness_commit, env_digest, ts}. Report is pass@1 + cost + tokens + cache-hit
+ wall time — the fields the public-reporting standard requires.

Usage:
  OVERSEER_API_KEY=... python3 eval/run.py [--solver overseer|mini|both]
      [--tasks id1,id2|all] [--model claude-haiku-4-5] [--max-steps N]
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent


def load_solver(name: str):
    path = ROOT / "solvers" / {"overseer": "overseer_exec.py",
                               "mini": "mini_swe_agent.py"}[name]
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def harness_commit() -> str:
    try:
        return subprocess.run(["git", "rev-parse", "--short", "HEAD"],
                              cwd=REPO, capture_output=True, text=True).stdout.strip()
    except Exception:
        return "unknown"


def run_one(task: dict, solver_name: str, args) -> dict:
    ws = REPO / "eval" / "workspaces" / task["id"] / solver_name
    sess = REPO / "eval" / "results" / task["id"] / solver_name
    shutil.rmtree(ws, ignore_errors=True)
    shutil.rmtree(sess, ignore_errors=True)
    ws.mkdir(parents=True)
    sess.mkdir(parents=True, exist_ok=True)

    # Prefer Homebrew's python3 over the Xcode CLT shim — graders run
    # `python3` and the shim exits 69 when the Xcode licence is unaccepted.
    env = dict(os.environ)
    if os.path.isdir("/opt/homebrew/bin"):
        env["PATH"] = "/opt/homebrew/bin:" + env["PATH"]

    setup = task.get("setup")
    if setup:
        subprocess.run(["sh", "-c", setup], cwd=ws, check=True,
                       capture_output=True, text=True, env=env)

    solver = load_solver(solver_name)
    if solver_name == "overseer":
        result = solver.solve(task["instruction"], str(ws), str(sess),
                              max_steps=args.max_steps)
    else:
        os.environ["MAX_STEPS"] = str(args.max_steps)
        result = solver.solve(task["instruction"], str(ws), str(sess))
        (sess / "trajectory.json").write_text(
            json.dumps(result.pop("trajectory"), indent=2))

    grader = task["grader"]["script"]
    g = subprocess.run(["sh", "-c", grader], cwd=ws,
                       capture_output=True, text=True, env=env)
    passed = g.returncode == 0 and result.get("done", False)
    return {
        "task": task["id"], "task_version": task["version"],
        "solver": solver_name, "pass": passed,
        "grader_exit": g.returncode, **result,
        "harness_commit": harness_commit(), "model": os.environ.get("OVERSEER_MODEL", "claude-haiku-4-5"),
        "env_digest": task.get("env", {}).get("image_digest") or "local-sh",
        "ts": int(time.time()),
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--solver", default="both", choices=["overseer", "mini", "both"])
    ap.add_argument("--tasks", default="all")
    ap.add_argument("--model", default=None)
    ap.add_argument("--max-steps", type=int, default=30)
    args = ap.parse_args()

    if args.model:
        os.environ["OVERSEER_MODEL"] = args.model
    if not os.environ.get("OVERSEER_API_KEY"):
        sys.exit("OVERSEER_API_KEY required")

    task_files = sorted((ROOT / "tasks").glob("*.json"))
    tasks = [json.loads(f.read_text()) for f in task_files]
    if args.tasks != "all":
        keep = set(args.tasks.split(","))
        tasks = [t for t in tasks if t["id"] in keep]
    solvers = ["overseer", "mini"] if args.solver == "both" else [args.solver]

    results = []
    for task in tasks:
        for s in solvers:
            print(f"▶ {task['id']} × {s} ...", flush=True)
            try:
                r = run_one(task, s, args)
            except Exception as e:  # a crashed solver is a failed run, logged
                r = {"task": task["id"], "solver": s, "pass": False,
                     "error": str(e), "model": os.environ.get("OVERSEER_MODEL")}
            results.append(r)
            print(f"  {'PASS' if r['pass'] else 'FAIL'} "
                  f"steps={r.get('steps','?')} wall={r.get('wall_s','?')}s "
                  f"in={r.get('tokens_in','?')} hit={r.get('cache_hit_rate','?')}")

    out = REPO / "eval" / "results" / f"report-{int(time.time())}.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(results, indent=2))

    print(f"\n{'task':<22} {'solver':<9} {'pass':<5} {'steps':>5} {'wall_s':>7} {'tok_in':>8} {'cache':>6}")
    for r in results:
        print(f"{r['task']:<22} {r['solver']:<9} {str(r['pass']):<5} "
              f"{r.get('steps','-'):>5} {r.get('wall_s','-'):>7} "
              f"{r.get('tokens_in','-'):>8} {r.get('cache_hit_rate','-'):>6}")
    print(f"\nwrote {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
