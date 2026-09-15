"""Overseer solver — drives the real harness headlessly.

Runs `overseer exec --json --provider legionedge --model <M>` in the task
workdir; the JSONL event stream is the trajectory. The solver never grades —
the task's deterministic grader sees only the workdir afterwards.

Env: OVERSEER_BIN (default: target/release/overseer), LEK_API_KEY,
LEK_BASE_URL, LEK_MODEL.
"""
from __future__ import annotations

import json
import os
import subprocess
import time
from pathlib import Path

OVERSEER_BIN = os.environ.get(
    "OVERSEER_BIN",
    str(Path(__file__).resolve().parents[2] / "target" / "release" / "overseer"),
)
BASE_URL = os.environ.get("LEK_BASE_URL", "https://inference.legionedge.ai/v1")
MODEL = os.environ.get("LEK_MODEL", "kimi-k3-turbo")


def solve(instruction: str, workdir: str, session_dir: str,
          max_steps: int = 30) -> dict:
    Path(session_dir).mkdir(parents=True, exist_ok=True)
    t0 = time.time()
    proc = subprocess.run(
        [OVERSEER_BIN, "exec", "--json",
         "--provider", "openai", "--base-url", BASE_URL,
         "--model", MODEL,
         "--session", session_dir, "--cwd", workdir,
         "--max-steps", str(max_steps), "--full-access", "-"],
        input=instruction, capture_output=True, text=True, timeout=900)
    wall_s = time.time() - t0
    events = [json.loads(l) for l in proc.stdout.splitlines() if l.strip()]
    (Path(session_dir) / "trajectory.jsonl").write_text(
        "\n".join(json.dumps(e) for e in events))
    # Metrics the playbook requires per run.
    usage = [e for e in events if e.get("type") == "model_response"]
    total_in = sum(e["usage"]["fresh_input"] + e["usage"]["cache_read"]
                   + e["usage"]["cache_write"] for e in usage)
    cache_read = sum(e["usage"]["cache_read"] for e in usage)
    return {
        "done": proc.returncode == 0,
        "steps": len(usage),
        "wall_s": round(wall_s, 1),
        "tokens_in": total_in,
        "tokens_out": sum(e["usage"]["output"] for e in usage),
        "cache_hit_rate": round(cache_read / total_in, 3) if total_in else 0.0,
        "cost_usd": sum(e.get("cost_usd", 0.0) for e in usage),
        "exit_code": proc.returncode,
        "stderr": proc.stderr[-2000:],
    }
