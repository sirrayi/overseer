"""Inspect AI solver: drive Overseer headlessly.

Launches `overseer exec --json <instruction>` in the task workdir, captures
the JSONL event stream as the trajectory, and writes it to the trajectory
store. The solver never interprets events — the scorer grades the workdir.

Requires OVERSEER_BIN (default: target/release/overseer) and a provider key.
"""
from __future__ import annotations

import json
import os
import subprocess
import time
from pathlib import Path

OVERSEER_BIN = os.environ.get("OVERSEER_BIN", "target/release/overseer")


def run_task(instruction: str, workdir: Path, session_dir: Path,
             max_steps: int = 100, max_cost: float = 5.0) -> dict:
    """Execute one task; return trajectory + run metadata."""
    session_dir.mkdir(parents=True, exist_ok=True)
    t0 = time.time()
    proc = subprocess.run(
        [OVERSEER_BIN, "exec", "--json",
         "--session", str(session_dir),
         "--cwd", str(workdir),
         "--max-steps", str(max_steps),
         "--max-cost", str(max_cost),
         "-"],
        input=instruction, capture_output=True, text=True,
    )
    wall_s = time.time() - t0
    events = [json.loads(l) for l in proc.stdout.splitlines() if l.strip()]
    traj_path = session_dir / "trajectory.jsonl"
    traj_path.write_text("\n".join(json.dumps(e) for e in events))
    return {
        "exit_code": proc.returncode,
        "wall_s": wall_s,
        "n_events": len(events),
        "trajectory": str(traj_path),
        "stderr": proc.stderr[-2000:],
    }
