"""Control scaffold — the null hypothesis (playbook Ch.8 §4, §7.2).

A mini-SWE-agent-shaped loop over the same OpenAI-compatible endpoint:
bash-only actions, fenced-block extraction, no tool registry, no budgets
beyond a step cap, no event log, no ledger, no gate. Every Overseer
feature must beat THIS on identical tasks to earn its tokens.

The loop semantics are frozen — it must stay minimal to remain a valid
null scaffold. Changes are limited to protocol plumbing (usage capture,
limits/seed params, trajectory path).

Env: OVERSEER_API_KEY (required), OVERSEER_BASE_URL, OVERSEER_MODEL,
OVERSEER_PROVIDER.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
import urllib.request
from pathlib import Path

OUT_CAP = 4000


def _cfg() -> tuple[str, str, str]:
    """Resolved per call — module-level env binding would freeze --model."""
    provider = os.environ.get("OVERSEER_PROVIDER", "opencode")
    default_base = (
        "https://opencode.ai/zen/go/v1"
        if provider == "opencode"
        else "https://api.openai.com/v1"
    )
    return (
        provider,
        os.environ.get("OVERSEER_BASE_URL", default_base),
        os.environ.get("OVERSEER_MODEL", "deepseek-v4.1-flash"),
    )


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
    provider, base_url, model = _cfg()
    body = json.dumps(
        {
            "model": model,
            "max_tokens": 2048,
            "messages": [{"role": "system", "content": SYSTEM}] + messages,
        }
    ).encode()
    headers = {
        "Authorization": f"Bearer {os.environ['OVERSEER_API_KEY']}",
        "Content-Type": "application/json",
    }
    # The opencode Go gateway requires its routing header on every call.
    if provider == "opencode":
        headers["x-opencode-session"] = "overseer-eval-mini"
    req = urllib.request.Request(
        f"{base_url}/chat/completions",
        data=body,
        headers=headers,
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
    # Model-controlled commands run without credentials — the key is for the
    # API call only, it must never be readable from inside a task shell.
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith("OVERSEER_")
        and not k.endswith(("_KEY", "_TOKEN"))
    }
    try:
        p = subprocess.run(
            ["sh", "-c", cmd],
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=60,
            env=env,
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
    _, _, model = _cfg()
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
            "model": model,
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
        "model": model,
        "ts": int(time.time()),
    }
