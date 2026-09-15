"""Control scaffold — the null hypothesis (playbook Ch.8 §4, Ch.12 §0.7).

A mini-SWE-agent-shaped loop (~100 lines) over the same OpenAI-compatible
endpoint Overseer uses: bash-only actions, fenced-block extraction, no tool
registry, no budgets beyond a step cap, no event log, no ledger, no gate.
Every Overseer feature must beat THIS on identical tasks to earn its tokens.

Env: OVERSEER_API_KEY (required), OVERSEER_BASE_URL, OVERSEER_MODEL.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import urllib.request

BASE_URL = os.environ.get("OVERSEER_BASE_URL", "https://inference.fleet.ai/v1")
MODEL = os.environ.get("OVERSEER_MODEL", "fleet-turbo")
MAX_STEPS = int(os.environ.get("MAX_STEPS", "30"))
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


def chat(messages: list[dict]) -> str:
    body = json.dumps({
        "model": MODEL,
        "max_tokens": 2048,
        "messages": [{"role": "system", "content": SYSTEM}] + messages,
    }).encode()
    req = urllib.request.Request(
        f"{BASE_URL}/chat/completions", data=body,
        headers={"Authorization": f"Bearer {os.environ['OVERSEER_API_KEY']}",
                 "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=120) as r:
        data = json.loads(r.read())
    msg = data["choices"][0]["message"]
    # reasoning fields differ per model; only content matters for actions
    return msg.get("content") or ""


def extract_action(text: str) -> tuple[str, str]:
    """Return (kind, payload): bash | done | none."""
    m = re.search(r"```bash\n(.*?)```", text, re.S)
    if m:
        return "bash", m.group(1).strip()
    if "overseer_done" in text:
        return "done", ""
    return "none", text[:500]


def run_bash(cmd: str, cwd: str) -> str:
    try:
        p = subprocess.run(["sh", "-c", cmd], cwd=cwd, capture_output=True,
                           text=True, timeout=60)
        out = (p.stdout + p.stderr).strip() or "(no output)"
        return f"exit {p.returncode}\n{out[:OUT_CAP]}"
    except subprocess.TimeoutExpired:
        return "error: command timed out (60s)"


def solve(instruction: str, workdir: str, session_dir: str) -> dict:
    """Run the control loop; return trajectory + stats."""
    messages = [{"role": "user", "content": instruction}]
    traj = []
    for step in range(1, MAX_STEPS + 1):
        reply = chat(messages)
        kind, payload = extract_action(reply)
        traj.append({"step": step, "reply": reply, "kind": kind})
        if kind == "done":
            return {"steps": step, "done": True, "trajectory": traj}
        if kind != "bash":
            obs = "No ```bash block found. Reply with exactly one bash block or overseer_done."
        else:
            obs = run_bash(payload, workdir)
        traj[-1]["observation"] = obs
        messages += [{"role": "assistant", "content": reply},
                     {"role": "user", "content": f"OBSERVATION:\n{obs}"}]
    return {"steps": MAX_STEPS, "done": False, "trajectory": traj}


if __name__ == "__main__":
    instruction = sys.argv[1] if len(sys.argv) > 1 else sys.stdin.read()
    result = solve(instruction, os.getcwd(), "")
    print(json.dumps({k: v for k, v in result.items() if k != "trajectory"}))
    json.dump(result, open("trajectory.json", "w"), indent=2)
