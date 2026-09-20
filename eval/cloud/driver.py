#!/usr/bin/env python3
"""eval/cloud/driver.py — Devin Cloud (v1 API) worker orchestrator.

Spawns Devin sessions that run overseer eval slices in cloud VMs, then
commit results to `eval/<run>` branches. Built for the Oct-1 stress
campaign: memory/orch stress suite + benchmark matrix on muse/deepseek
via the opencode Go subscription ($0 model cost).

Env:
  DEVIN_API_KEY   — PAT (apk_user_…) or service-user key (v1 surface).
  DEVIN_SECRET_IDS — comma-separated secret ids injected into every
                    worker (e.g. the OPENCODE_API_KEY org secret), so
                    keys never appear in prompts.
  OVERSEER_REPO   — repo URL workers clone (default: sirrayi/overseer).
  OVERSEER_REF    — git ref workers checkout (default: review).

Usage:
  driver.py plan                     # print the shard matrix
  driver.py launch [--only <sel>]    # create sessions (dry-run w/o --go)
  driver.py poll [--watch]           # status table of tracked sessions
  driver.py track <session_id>       # register an externally-created session

State: eval/cloud/state.json tracks spawned sessions across calls
(workers outlive this script — the driver is deliberately stateless
beyond that file).
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.request
import urllib.error
from pathlib import Path

ROOT = Path(__file__).resolve().parent
STATE = ROOT / "state.json"
API = "https://api.devin.ai/v1"

REPO = os.environ.get("OVERSEER_REPO", "https://github.com/sirrayi/overseer")
REF = os.environ.get("OVERSEER_REF", "review")


def _req(method: str, path: str, body: dict | None = None) -> dict:
    key = os.environ.get("DEVIN_API_KEY", "")
    if not key:
        raise SystemExit("DEVIN_API_KEY not set")
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        f"{API}{path}",
        data=data,
        method=method,
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        detail = e.read()[:300].decode(errors="replace")
        raise SystemExit(f"{method} {path}: HTTP {e.code} — {detail}")


def create_session(prompt: str, *, tags: list[str], unlisted: bool = True) -> dict:
    body: dict = {"prompt": prompt, "tags": tags, "unlisted": unlisted}
    secret_ids = [s for s in os.environ.get("DEVIN_SECRET_IDS", "").split(",") if s]
    if secret_ids:
        body["secret_ids"] = secret_ids
    return _req("POST", "/sessions", body)


def get_session(sid: str) -> dict:
    return _req("GET", f"/session/{sid}")


def send_message(sid: str, message: str) -> dict:
    return _req("POST", f"/session/{sid}/message", {"message": message})


# ---------- worker runbook ----------

def worker_prompt(spec: dict) -> str:
    """The runbook a worker session executes. The agent clones overseer,
    builds the release binary, runs the slice, and pushes a results
    branch — the last message must be the RUN_RESULT JSON line."""
    cmds = "\n".join(f"   {c}" for c in spec["commands"])
    return f"""You are an eval worker for the overseer harness. Execute this
runbook EXACTLY — do not improvise extra tasks.

1. Clone the repo and check out the eval ref:
   git clone {REPO} ~/overseer && cd ~/overseer && git checkout {REF}

2. Toolchain: ensure `cargo` and `uv` exist
   (`curl https://sh.rustup.rs -sSf | sh -s -- -y` and
   `curl -LsSf https://astral.sh/uv/install.sh | sh` if missing; then
   `export PATH="$HOME/.cargo/bin:$PATH"`).

3. Build: `cd ~/overseer && cargo build --release -p overseer-cli`

4. Prepare eval env: `cd ~/overseer/eval && uv sync`.
   Export (in each shell you use):
   `export OVERSEER_BIN=~/overseer/target/release/overseer`
   `export OVERSEER_PROVIDER=opencode` `export OVERSEER_MODEL={spec['model']}`
   `export OVERSEER_API_KEY=$OPENCODE_API_KEY`
   `export OVERSEER_MODEL={spec['model']}`
   (OPENCODE_API_KEY is already in your environment via org secret —
   never print or commit it).

5. Run the assigned slice (each may take a long time — that is expected;
   keep working until every command finishes):
{cmds}

6. Collect results into the repo and push:
   `cd ~/overseer && git checkout -b eval/{spec['run']} &&`
   `git add -f eval/results eval/stress/out eval/cloud/state.json 2>/dev/null;`
   `git commit -m "eval({spec['run']}): worker results" &&`
   `git push -u origin eval/{spec['run']}`

7. FINAL STEP — reply with exactly one line:
   RUN_RESULT {{"run": "{spec['run']}", "benchmark": "{spec['benchmark']}",
   "model": "{spec['model']}", "pushed": <true|false>}}

If a step fails twice, stop and reply with RUN_RESULT {{"run":
"{spec['run']}", "error": "<what failed>"}} — do not retry forever.
"""


# ---------- shard matrix ----------

def matrix() -> list[dict]:
    """The full campaign matrix. `commands` run inside eval/
    (cwd = ~/overseer/eval). Workers share one opencode key — shard
    sizes are tuned so each worker stays well under the 5h session cap.
    """
    out: list[dict] = []
    models = ["muse-spark-1.3-contributor", "deepseek-v4.1-flash"]
    SWEB_SHARDS = 12  # 500 verified / 12 ≈ 42 tasks per worker

    # --- stress suite: muse only (hardest reasoner we have) ---
    out.append({
        "run": "stress-muse",
        "benchmark": "stress",
        "model": "muse-spark-1.3-contributor",
        "commands": [
            "uv run python stress/suite.py --out stress/out "
            "--provider opencode --model muse-spark-1.3-contributor",
        ],
    })

    # --- SWE-bench Verified 500: exact id shards (disjoint, complete) ---
    for model in models:
        tag = "muse" if model.startswith("muse") else "deepseek"
        for i in range(SWEB_SHARDS):
            out.append({
                "run": f"sweb-{tag}-s{i:02d}",
                "benchmark": "swe_bench",
                "model": model,
                "commands": [
                    f'IDS=$(uv run python ../cloud/shard_ids.py '
                    f'--dataset SWE-bench/SWE-bench_Verified '
                    f'--shard {i} --of {SWEB_SHARDS}) && '
                    f'uv run python run.py --benchmark swe_bench '
                    f'--agents overseer --swebench-ids "$IDS" '
                    f'--model {model}',
                ],
            })

    # --- Terminal-Bench 2.0: overseer inside the task containers ---
    # 3 workers × same 30 tasks = k=3 trials per task (leaderboard wants
    # ≥5; variance data is the point, not coverage).
    for model in models:
        tag = "muse" if model.startswith("muse") else "deepseek"
        for i in range(3):
            out.append({
                "run": f"tb2-{tag}-s{i}",
                "benchmark": "terminal_bench",
                "model": model,
                "commands": [
                    f"uv run python run.py --benchmark terminal_bench "
                    f"--agents overseer_agent:OverseerAgent "
                    f"--model {model} --n-tasks 30 --scheduler-seed {i}",
                ],
            })

    # --- SWE-bench-Live: contamination-free fresh tasks ---
    for model in models:
        tag = "muse" if model.startswith("muse") else "deepseek"
        for i in range(2):
            out.append({
                "run": f"swel-{tag}-s{i}",
                "benchmark": "swe_live",
                "model": model,
                "commands": [
                    f"uv run python run.py --benchmark swe_live "
                    f"--swe-live-dataset SWE-bench-Live/MultiLang "
                    f"--agents overseer --model {model} --n-tasks 25",
                ],
            })

    # --- aider polyglot: the leaderboard-comparable track ---
    for model in models:
        tag = "muse" if model.startswith("muse") else "deepseek"
        for lang in ("python", "rust", "go"):
            out.append({
                "run": f"poly-{tag}-{lang}",
                "benchmark": "polyglot",
                "model": model,
                "commands": [
                    f"uv run python run.py --benchmark polyglot "
                    f"--agents overseer --model {model} "
                    f"--polyglot-lang {lang} --n-tasks 40",
                ],
            })

    return out


# ---------- state ----------

def load_state() -> dict:
    return json.loads(STATE.read_text()) if STATE.exists() else {"sessions": {}}


def save_state(st: dict) -> None:
    STATE.write_text(json.dumps(st, indent=2))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["plan", "launch", "poll", "track", "msg"])
    ap.add_argument("arg", nargs="?", default=None)
    ap.add_argument("--only", default=None, help="run-prefix filter (e.g. sweb-muse)")
    ap.add_argument("--go", action="store_true", help="actually create sessions")
    ap.add_argument("--max", type=int, default=0, help="cap sessions launched")
    ap.add_argument("--watch", action="store_true")
    ap.add_argument("--stagger", type=int, default=90, help="s between launches")
    args = ap.parse_args()

    specs = matrix()
    if args.only:
        specs = [s for s in specs if s["run"].startswith(args.only)]

    if args.cmd == "plan":
        for s in specs:
            print(f"{s['run']:>20}  {s['benchmark']:<16} {s['model']}")
        print(f"\n{len(specs)} workers total")
        return 0

    if args.cmd == "track":
        st = load_state()
        st["sessions"][args.arg] = {"run": "external", "ts": int(time.time())}
        save_state(st)
        print(f"tracking {args.arg}")
        return 0

    if args.cmd == "msg":
        sid, text = args.arg.split(":", 1)
        print(send_message(sid, text))
        return 0

    if args.cmd == "launch":
        st = load_state()
        launched = {v["run"] for v in st["sessions"].values()}
        n = 0
        for s in specs:
            if s["run"] in launched:
                continue
            if args.max and n >= args.max:
                break
            if not args.go:
                print(f"[dry-run] would launch {s['run']}")
                n += 1
                continue
            sess = create_session(worker_prompt(s), tags=["overseer-eval", s["benchmark"]])
            sid = sess.get("session_id") or sess.get("id")
            st["sessions"][sid] = {"run": s["run"], "ts": int(time.time())}
            save_state(st)
            print(f"launched {s['run']} → {sid} ({sess.get('url','')})")
            n += 1
            if n < len(specs):
                time.sleep(args.stagger)
        if not args.go:
            print("\n(dry run — pass --go to create sessions)")
        return 0

    if args.cmd == "poll":
        st = load_state()
        while True:
            for sid, meta in st["sessions"].items():
                try:
                    s = get_session(sid)
                    status = s.get("status_enum") or s.get("status") or "?"
                    print(f"{meta['run']:>20}  {status:<12} {sid}")
                except SystemExit as e:
                    print(f"{meta['run']:>20}  ERR          {e}")
            if not args.watch:
                break
            print("---")
            time.sleep(60)
        return 0

    return 1


if __name__ == "__main__":
    sys.exit(main())
