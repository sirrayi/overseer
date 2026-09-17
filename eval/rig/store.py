"""Immutable run store (playbook Ch.8 §7.3.4).

Every score row is one JSONL append in `eval/results/store.jsonl`, keyed
by {task_id, task_version, harness, harness_commit, model, seed,
env_digest, judge_version}. Nothing is ever rewritten — a re-run appends
new rows. Per-run artifacts (events.jsonl, trajectory, manifest) live in
`eval/results/runs/<run_id>/` and are referenced by path.

A `matrix` header row (kind=matrix) precedes each run-set with the full
factorial parameters — the audit anchor for a report.
"""

from __future__ import annotations

import json
import time
import uuid
from pathlib import Path

RECORD_KEY_FIELDS = (
    "task_id",
    "task_version",
    "harness",
    "harness_commit",
    "model",
    "seed",
    "env_digest",
    "judge_version",
)


class Store:
    def __init__(self, path: Path | str):
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)

    def append(self, record: dict) -> None:
        with self.path.open("a") as f:
            f.write(json.dumps(record) + "\n")

    def load(self) -> list[dict]:
        """All parseable rows. The store is append-only, so a crashed
        writer can leave a truncated tail line — skip bad lines and count
        them (`skipped_lines`) rather than aborting every reader."""
        self.skipped_lines = 0
        if not self.path.exists():
            return []
        rows = []
        for line in self.path.read_text().splitlines():
            if not line.strip():
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                self.skipped_lines += 1
        return rows

    def runs(self, **eq) -> list[dict]:
        """Run rows matching equality filters, e.g. runs(harness='overseer',
        model='fleet-turbo', benchmark='local')."""
        return [
            r
            for r in self.load()
            if r.get("kind") == "run" and all(r.get(k) == v for k, v in eq.items())
        ]

    def matrix_header(
        self,
        *,
        benchmark: str,
        agents: list[str],
        k: int,
        tasks: list[str],
        scheduler_seed: int,
        extra: dict | None = None,
    ) -> dict:
        header = {
            **(extra or {}),  # extras first — reserved keys always win
            "kind": "matrix",
            "run_set_id": uuid.uuid4().hex[:12],
            "benchmark": benchmark,
            "agents": agents,
            "seeds": k,
            "tasks": tasks,
            "scheduler_seed": scheduler_seed,
            "ts": int(time.time()),
        }
        self.append(header)
        return header


def new_run_id(task_id: str, agent: str, seed: int) -> str:
    return f"{task_id}__{agent}__s{seed}__{uuid.uuid4().hex[:8]}"
