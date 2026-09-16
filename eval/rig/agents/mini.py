"""Control scaffold — the null hypothesis (playbook Ch.8 §4, §7.2).

A mini-SWE-agent-shaped loop over the same OpenAI-compatible endpoint:
bash-only actions, fenced-block extraction, no tool registry, no budgets
beyond a step cap, no event log, no ledger, no gate. Every Overseer
feature must beat THIS on identical tasks to earn its tokens.

The loop semantics are frozen — it must stay minimal to remain a valid
null scaffold. Changes are limited to protocol plumbing (usage capture,
limits/seed params, trajectory path).

Env: LEK_API_KEY (required), LEK_BASE_URL, LEK_MODEL.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
import urllib.request
from pathlib import Path

BASE_URL = os.environ.get("LEK_BASE_URL", "https://inference.legionedge.ai/v1")
MODEL = os.environ.get("LEK_MODEL", "kimi-k3-turbo")
OUT_CAP = 4000

SYSTEM = """\
You are a coding agent. Reply with exactly ONE action per turn.

To run a shell command, output a fenced block and nothing else:
```bash
<command>
```

When the task is fully complete, output:
```overseer_done
```

Rules: one block per reply; no prose outside the block; commands run in the
task directory; prefer simple, verifiable steps."""


def chat(messages: list[dict]) -> tuple[str, dict]:
    body = json.dumps(
        {
            "model": MODEL,
            "max_tokens": 2048,
            "messages": [{"role": "system", "content": SYSTEM}] + messages,
        }
    ).encode()
    req = urllib.request.Request(
        f"{BASE_URL}/chat/completions",
        data=body,
        headers={
            "Authorization": f"Bearer {os.environ['LEK_API_KEY']}",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(req, timeout=120) as r:
        data = json.loads(r.read())
    msg = data["choices"][0]["message"]
    usage = data.get("usage") or {}
    # reasoning fields differ per model; only content matters for actions
    return msg.get("content") or "", usage


def extract_action(text: str) -> tuple[str, str]:
    """Return (kind, payload): bash | done | none."""
    # Accept closed OR unclosed fences — models regularly drop the tail fence.
    m = re.search(r"```bash\n(.*?)(?:```|$)", text, re.DOTALL)
    if m:
        return "bash", m.group(1).strip()
    if "overseer_done" in text:
        return "done", ""
    return "none", text[:500]


def run_bash(cmd: str, cwd: str) -> str:
    try:
        p = subprocess.run(
            ["sh", "-c", cmd], cwd=cwd, capture_output=True, text=True, timeout=60
        )
        out = (p.stdout + p.stderr).strip() or "(no output)"
        return f"exit {p.returncode}\n{out[:OUT_CAP]}"
    except subprocess.TimeoutExpired:
        return "error: command timed out (60s)"


def solve(
    instruction: str,
    workdir: str,
    session_dir: str,
    *,
    limits: dict | None = None,
    seed: int = 0,
    task=None,
) -> dict:
    limits = limits or {}
    max_steps = int(limits.get("max_steps", 30))
    Path(session_dir).mkdir(parents=True, exist_ok=True)
    t0 = time.time()

    messages = [{"role": "user", "content": instruction}]
    traj = []
    tokens_in = tokens_out = 0
    done = False
    try:
        for step in range(1, max_steps + 1):
            reply, usage = chat(messages)
            tokens_in += usage.get("prompt_tokens", 0)
            tokens_out += usage.get("completion_tokens", 0)
            kind, payload = extract_action(reply)
            traj.append({"step": step, "reply": reply, "kind": kind})
            if kind == "done":
                done = True
                break
            if kind != "bash":
                obs = "No ```bash block found. Reply with exactly one bash block or overseer_done."
            else:
                obs = run_bash(payload, workdir)
            traj[-1]["observation"] = obs
            messages += [
                {"role": "assistant", "content": reply},
                {"role": "user", "content": f"OBSERVATION:\n{obs}"},
            ]
    except urllib.error.URLError as e:
        return {
            "done": False,
            "error": f"transport: {e}",
            "infra_error": True,
            "wall_s": round(time.time() - t0, 1),
            "model": MODEL,
            "steps": len(traj),
            "ts": int(time.time()),
        }

    traj_path = Path(session_dir) / "trajectory.json"
    traj_path.write_text(json.dumps(traj, indent=2))
    return {
        "done": done,
        "steps": len(traj),
        "wall_s": round(time.time() - t0, 1),
        "tokens_in": tokens_in or None,
        "tokens_out": tokens_out or None,
        "cost_usd": None,  # unknown pricing for the control arm — honest null
        "trajectory": str(traj_path),
        "model": MODEL,
        "ts": int(time.time()),
    }
