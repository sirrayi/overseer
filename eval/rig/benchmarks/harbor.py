"""Harbor adapters — Terminal-Bench 2.x and SWE-rebench (Tier 1).

Harbor (laude-institute) runs agents against containerized task datasets:
`harbor run -d <dataset@ver> -a <agent> -m <model> -k <attempts>`.
Each attempt is one seed — harbor's `result.json` per trial carries
`verifier_result.rewards.reward`, `agent_result` token/cost usage,
`task_checksum`, and the task repo's pinned `git_commit_id` — everything
the RunRecord needs for provenance.

Offline-verifiable: `-a oracle` runs the task's reference solution (no
model calls); `-a nop` runs nothing. LLM agents (terminus-2,
mini-swe-agent, …) accept `-m openai/<model>` and resolve against
OPENAI_BASE_URL — mapped from OVERSEER_* like the other adapters.

Overseer-as-agent needs a harbor custom-agent module that executes inside
the task container — deferred; built-in agents cover the baseline arm.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

from .. import manifest, store
from . import BenchmarkUnavailable, ExternalTask, has_cli, has_docker

HARBOR = ["uvx", "harbor"]

def _parse_ts(s) -> datetime | None:
    """Parse a harbor timestamp — py3.9-safe (fromisoformat has no Z).

    Handles Z suffix, +HH:MM/+HHMM offsets, space separator;
    naive stamps are assumed UTC. None on unparseable input.
    """
    if not isinstance(s, str) or not s.strip():
        return None
    t = s.strip()
    if t[-1:] in ("Z", "z"):
        t = t[:-1] + "+00:00"
    tail = t[-5:]
    if len(t) >= 5 and tail[0] in ("+", "-") and tail[1:].isdigit():
        t = t[:-2] + ":" + t[-2:]
    try:
        dt = datetime.fromisoformat(t)
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def parse_wall(t0, t1) -> float | None:
    """Seconds between harbor started_at/finished_at, rounded to 1dp.

    None when either stamp is missing or unparseable.
    """
    try:
        a, b = _parse_ts(t0), _parse_ts(t1)
        if a is None or b is None:
            return None
        return round((b - a).total_seconds(), 1)
    except (ValueError, TypeError, OverflowError):
        return None


class HarborAdapter:
    """Shared Harbor runner. Subclasses pin `dataset` + `benchmark`."""

    name = "harbor"
    dataset: str = ""
    benchmark = "harbor"
    task_tag = "harbor"

    def __init__(self, jobs_root: Path | None = None):
        self.jobs_root = Path(jobs_root or "results/harbor")

    def requirements(self) -> list[str]:
        return ["docker", "cli:uv"]

    def available(self) -> tuple[bool, str]:
        if not has_docker():
            return False, "docker daemon unreachable — install/start OrbStack"
        if not has_cli("uvx"):
            return False, "uvx not installed"
        return True, "ok"

    def harbor_version(self) -> str:
        try:
            out = subprocess.run(
                [*HARBOR, "--version"], capture_output=True, text=True, timeout=60
            )
            return out.stdout.strip() if out.returncode == 0 else "unknown"
        except Exception:
            return "unknown"

    # ---- execution ----

    def run(
        self,
        *,
        agent: str,
        model: str | None,
        attempts: int,
        n_tasks: int | None,
        task_name: str | None,
        jobs_dir: Path,
    ) -> Path:
        ok, why = self.available()
        if not ok:
            raise BenchmarkUnavailable(why)
        cmd = [
            *HARBOR,
            "run",
            "-d",
            self.dataset,
            "-a",
            agent,
            "-k",
            str(attempts),
            "-o",
            str(jobs_dir),
        ]
        if model:
            cmd += ["-m", model]
        if n_tasks:
            cmd += ["-l", str(n_tasks)]
        if task_name:
            cmd += ["-t", task_name]
        env = dict(os.environ)
        if model and model.startswith("openai/"):
            env.setdefault("OPENAI_API_KEY", os.environ.get("OVERSEER_API_KEY", ""))
            env.setdefault(
                "OPENAI_BASE_URL",
                os.environ.get("OVERSEER_BASE_URL", "https://opencode.ai/zen/go/v1"),
            )
        # Custom agents (`-a module.path:Class`) need this dir importable —
        # our overseer_agent module lives beside this file's sibling dir.
        if ":" in agent:
            agents_dir = str((Path(__file__).resolve().parent.parent
                              / "harbor_agents").resolve())
            env["PYTHONPATH"] = (
                agents_dir + os.pathsep + env.get("PYTHONPATH", "")
            ).rstrip(os.pathsep)
        proc = subprocess.run(
            cmd, env=env, capture_output=True, text=True, timeout=86400
        )
        (jobs_dir / "harbor.stdout.log").parent.mkdir(parents=True, exist_ok=True)
        (jobs_dir / "harbor.stdout.log").write_text(
            (proc.stdout or "") + "\n=== STDERR ===\n" + (proc.stderr or "")
        )
        if proc.returncode != 0:
            raise BenchmarkUnavailable(
                f"harbor run rc={proc.returncode} — see "
                f"{jobs_dir / 'harbor.stdout.log'}"
            )
        # job lands in <jobs_dir>/<YYYY-MM-DD__HH-MM-SS>/
        jobs = sorted(
            (
                p
                for p in jobs_dir.iterdir()
                if p.is_dir() and (p / "result.json").exists()
            ),
            key=lambda p: p.name,
        )
        if not jobs:
            raise BenchmarkUnavailable(f"no harbor job dir under {jobs_dir}")
        return jobs[-1]

    # ---- normalization ----

    def parse_results(
        self,
        job_dir: Path,
        *,
        agent_name: str,
        model_id: str | None,
        harness_commit: str,
        run_set_id: str,
    ) -> list[dict]:
        """<job>/<task>__<hash>/result.json → RunRecord per trial."""
        job_dir = Path(job_dir)
        hver = self.harbor_version()
        records = []
        trials = sorted(
            p for p in job_dir.iterdir() if p.is_dir() and (p / "result.json").exists()
        )
        # attempt index per task → seed (harbor runs n-attempts per task)
        per_task: dict[str, int] = {}
        for td in trials:
            tr = json.loads((td / "result.json").read_text())
            task_name = tr.get("task_name", td.name.split("__")[0])
            seed = per_task.get(task_name, 0)
            per_task[task_name] = seed + 1

            task_id = tr.get("task_id") or {}
            agent_res = tr.get("agent_result") or {}
            verifier = tr.get("verifier_result") or {}
            rewards = verifier.get("rewards") or {}
            reward = rewards.get("reward")
            exc = tr.get("exception_info")
            # env/verifier-side exceptions are infra; an exception raised
            # after agent_execution completed is scored as an agent failure.
            agent_done = bool((tr.get("agent_execution") or {}).get("finished_at"))
            infra = bool(exc is not None and not agent_done)
            t0, t1 = tr.get("started_at"), tr.get("finished_at")
            wall = parse_wall(t0, t1)
            steps = tr.get("step_results")
            outcome = {
                "done": exc is None,
                "pass": bool(reward is not None and reward >= 1.0),
                "score": reward,
                "infra_error": infra,
                "error": json.dumps(exc)[:300] if exc else None,
                "steps": len(steps) if isinstance(steps, list) else None,
                "wall_s": wall,
                "tokens_in": agent_res.get("n_input_tokens"),
                "tokens_out": agent_res.get("n_output_tokens"),
                "cost_usd": agent_res.get("cost_usd"),
                "model": _model_label(tr, model_id),
                "ts": int(time.time()),
            }
            task = ExternalTask(
                id=f"{self.task_tag}-{task_name}",
                version=1,
                env_digest=(f"{self.dataset}:{tr.get('task_checksum', '')[:16]}"),
                tags=[self.task_tag, task_name],
            )
            rec = manifest.build_record(
                run_id=store.new_run_id(task.id, agent_name, seed),
                task=task,
                agent=agent_name,
                seed=seed,
                outcome=outcome,
                manifest=None,
                benchmark=self.benchmark,
                run_set_id=run_set_id,
                session_dir=str(td),
                judge_version=(
                    f"harbor@{hver};task@{(task_id.get('git_commit_id') or '')[:12]}"
                ),
                harness_commit=harness_commit,
            )
            rec[self.task_tag] = {
                "dataset": self.dataset,
                "task_checksum": tr.get("task_checksum"),
                "task_commit": task_id.get("git_commit_id"),
                "trial_name": tr.get("trial_name"),
                "reward": reward,
                "exception": bool(exc),
            }
            records.append(rec)
        return records


def _model_label(tr: dict, model_id: str | None):
    """agent_info.model_info is an object ({name: ...}) in harbor's
    schema — records need a string identity key, not a dict."""
    mi = (tr.get("agent_info") or {}).get("model_info")
    if isinstance(mi, dict):
        return mi.get("name") or model_id
    return mi or model_id


class TerminalBenchAdapter(HarborAdapter):
    name = "terminal_bench"
    benchmark = "terminal_bench"
    task_tag = "tb2"
    dataset = "terminal-bench@2.0"


class SweRebenchAdapter(HarborAdapter):
    name = "swe_rebench"
    benchmark = "swe_rebench"
    task_tag = "swer"

    def __init__(self, jobs_root=None, split: str | None = None):
        super().__init__(jobs_root)
        # monthly split, e.g. 2025_10 — required to pin the leaderboard cut
        self.dataset = (
            f"swe-rebench/swe-rebench-leaderboard@{split}"
            if split
            else "swe-rebench/swe-rebench-leaderboard"
        )


def run_cli(
    adapter: HarborAdapter, args, st: store.Store, harness_commit: str, progress
) -> int:
    ok, why = adapter.available()
    if not ok:
        print(f"{adapter.name} unavailable: {why}", file=sys.stderr)
        return 2
    agent = args.agents.split(",")[0].strip()
    model = args.model or os.environ.get("OVERSEER_MODEL", "deepseek-v4.1-flash")
    # harbor's built-in LLM agents take openai/<model> against the
    # configured gateway; oracle/nop take no model at all.
    model_arg = (
        model
        if ":" in agent  # custom import path — no litellm prefix
        else (None if agent in ("oracle", "nop") else f"openai/{model}")
    )
    if model_arg and not os.environ.get("OVERSEER_API_KEY"):
        print(
            f"OVERSEER_API_KEY required for {adapter.name} agent {agent!r}",
            file=sys.stderr,
        )
        return 2
    jobs_dir = adapter.jobs_root / f"{adapter.name}-{int(time.time())}"
    header = st.matrix_header(
        benchmark=adapter.benchmark,
        agents=[agent],
        k=args.seeds,
        tasks=[adapter.dataset],
        scheduler_seed=args.scheduler_seed,
        extra={
            "harness_commit": harness_commit,
            "dataset": adapter.dataset,
            "model": model_arg,
        },
    )
    try:
        job_dir = adapter.run(
            agent=agent,
            model=model_arg,
            attempts=args.seeds,
            n_tasks=getattr(args, "n_tasks", None) or None,
            task_name=None,
            jobs_dir=jobs_dir,
        )
    except BenchmarkUnavailable as e:
        print(e, file=sys.stderr)
        return 2
    records = adapter.parse_results(
        job_dir,
        agent_name=agent,
        model_id=model_arg,
        harness_commit=harness_commit,
        run_set_id=header["run_set_id"],
    )
    for rec in records:
        st.append(rec)
        progress(rec)
    progress(
        {
            "task_id": "—",
            "harness": agent,
            "seed": "-",
            "pass": None,
            "infra_error": False,
            "steps": f"{len(records)} harbor trials ingested",
            "wall_s": "",
            "tokens_in": "",
            "cost_usd": "",
        }
    )
    return 0
