"""Overseer arm — drives the real harness headlessly.

`overseer exec --json` in the task workdir; the JSONL event stream is the
trajectory. The solver never grades — the deterministic grader sees only
the workdir afterwards. The session's manifest.json (overseer-core
manifest.rs, P4.5) is harvested into the record's provenance block.

Env: OVERSEER_BIN (default target/release/overseer), OVERSEER_PROVIDER
(default "opencode" — the opencode.ai/zen/go subscription endpoint),
the provider's own key (OPENCODE_API_KEY by default; rig/keys.py), which
the binary inherits, OVERSEER_BASE_URL, OVERSEER_MODEL.
"""

from __future__ import annotations

import json
import os
import subprocess
import time
from pathlib import Path

DEFAULT_BIN = str(
    Path(__file__).resolve().parents[3] / "target" / "release" / "overseer"
)


def _cfg() -> tuple[str, str, str, str]:
    """Resolved per call — module-level env binding would freeze --model."""
    provider = os.environ.get("OVERSEER_PROVIDER", "opencode")
    default_base = (
        "https://opencode.ai/zen/go/v1"
        if provider == "opencode"
        else "https://api.openai.com/v1"
    )
    return (
        os.environ.get("OVERSEER_BIN", DEFAULT_BIN),
        provider,
        os.environ.get("OVERSEER_BASE_URL", default_base),
        os.environ.get("OVERSEER_MODEL", "deepseek-v4.1-flash"),
    )


def solve(
    instruction: str,
    workdir: str,
    session_dir: str,
    *,
    limits: dict | None = None,
    seed: int = 0,
    task=None,
    extra_flags: list | None = None,
) -> dict:
    limits = limits or {}
    max_steps = int(limits.get("max_steps", 30))
    wall_cap = int(limits.get("wall_s", 900))
    binary, provider, base_url, model = _cfg()
    Path(session_dir).mkdir(parents=True, exist_ok=True)

    cmd = [
        binary,
        "exec",
        "--json",
        "--provider",
        provider,
        "--base-url",
        base_url,
        "--model",
        model,
        "--session",
        session_dir,
        "--cwd",
        workdir,
        "--max-steps",
        str(max_steps),
        "--full-access",
        *(extra_flags or []),
        "-",
    ]
    if limits.get("max_cost_usd") is not None:
        cmd += ["--max-cost", str(limits["max_cost_usd"])]

    t0 = time.time()
    try:
        proc = subprocess.run(
            cmd, input=instruction, capture_output=True, text=True, timeout=wall_cap
        )
    except subprocess.TimeoutExpired:
        return {
            "done": False,
            "error": f"wall timeout {wall_cap}s",
            "infra_error": False,
            "wall_s": wall_cap,
            "model": model,
            "ts": int(time.time()),
        }
    except FileNotFoundError:
        return {
            "done": False,
            "error": f"binary missing: {binary}",
            "infra_error": True,
            "wall_s": 0.0,
            "model": model,
            "ts": int(time.time()),
        }
    wall_s = time.time() - t0

    events = []
    for line in proc.stdout.splitlines():
        if line.strip():
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    traj = Path(session_dir) / "trajectory.jsonl"
    traj.write_text("\n".join(json.dumps(e) for e in events))

    usage_events = [e for e in events if e.get("type") == "model_response"]

    def _u(e, key):
        return (e.get("usage") or {}).get(key, 0)

    total_in = sum(
        _u(e, "fresh_input") + _u(e, "cache_read") + _u(e, "cache_write")
        for e in usage_events
    )
    cache_read = sum(_u(e, "cache_read") for e in usage_events)
    run_end = next((e for e in reversed(events) if e.get("type") == "run_end"), {})

    return {
        "done": proc.returncode == 0,
        "stop_reason": run_end.get("stop_reason"),
        "provider_error": run_end.get("stop_reason") == "provider_error",
        "steps": len(usage_events),
        "wall_s": round(wall_s, 1),
        "tokens_in": total_in,
        "tokens_out": sum(
            _u(e, "output") + _u(e, "reasoning") for e in usage_events
        ),  # billed as output+reasoning
        "cache_hit_rate": round(cache_read / total_in, 3) if total_in else 0.0,
        "cost_usd": sum(e.get("cost_usd", 0.0) for e in usage_events),
        "trajectory": str(traj),
        "exit_code": proc.returncode,
        "stderr": proc.stderr[-2000:],
        "model": model,
        "ts": int(time.time()),
    }
