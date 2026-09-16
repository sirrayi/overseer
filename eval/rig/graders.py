"""Deterministic graders + oracle verification (playbook Ch.8 §7.3.2).

The grader sees only the workspace — it never sees the trajectory, so a
solver can't argue its way into a pass. `oracle_check` applies the task's
reference solution to a fresh workspace and requires the grader to pass:
a task whose oracle fails is broken, not hard.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

from .taskspec import TaskSpec


def _env() -> dict:
    env = dict(os.environ)
    # Prefer Homebrew python3 over the Xcode CLT shim (exits 69 when the
    # Xcode licence is unaccepted).
    if os.path.isdir("/opt/homebrew/bin"):
        env["PATH"] = "/opt/homebrew/bin:" + env["PATH"]
    return env


def run_script(
    script: str, workdir: Path | str, timeout: int = 300
) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["sh", "-c", script],
        cwd=workdir,
        capture_output=True,
        text=True,
        env=_env(),
        timeout=timeout,
    )


def setup_workspace(task: TaskSpec, workdir: Path | str) -> None:
    ws = Path(workdir)
    shutil.rmtree(ws, ignore_errors=True)
    ws.mkdir(parents=True)
    if task.setup:
        p = run_script(task.setup, ws)
        if p.returncode != 0:
            raise RuntimeError(f"setup failed for {task.id}: {p.stderr[-500:]}")


def grade(task: TaskSpec, workdir: Path | str) -> tuple[bool, int, str]:
    """Returns (passed, exit_code, tail-of-output)."""
    p = run_script(task.grader_script(), workdir)
    tail = (p.stdout + p.stderr)[-2000:]
    return p.returncode == 0, p.returncode, tail


def oracle_check(task: TaskSpec, workdir: Path | str) -> tuple[bool, str]:
    """Fresh workspace → setup → oracle → grader must pass."""
    setup_workspace(task, workdir)
    p = run_script(task.oracle_script(), workdir)
    if p.returncode != 0:
        return False, f"oracle script failed: {(p.stderr or p.stdout)[-500:]}"
    ok, code, tail = grade(task, workdir)
    if not ok:
        return False, f"oracle passed but grader failed (exit {code}): {tail[-500:]}"
    return True, "ok"
