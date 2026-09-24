"""τ²-bench adapter (airline + retail).

Runs the real τ² harness from a checkout (env `OVERSEER_TAU2_DIR`) and
normalizes its `data/simulations/<run>/results.json` into RunRecords.
Per playbook:

- domains: airline, retail (telecom skipped — saturated)
- pinned user-simulator model (`--tau2-user-llm`) recorded in the record —
  τ² scores are only comparable within a fixed user-sim
- `num-trials` is τ²'s native pass^k unit; recommend 8 for reliability claims
- TerminationReason mapping: infra-side terminations land in
  `infra_error` (excluded from pass stats); agent-side ones are scored
  as real outcomes
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

from .. import manifest, store
from . import BenchmarkUnavailable, ExternalTask, has_cli

NAME = "tau2"
DOMAINS = ("airline", "retail")

# Terminations that are NOT the agent's outcome — excluded from pass stats.
# Aligned with upstream NON_EVALUABLE_TERMINATION_REASONS
# (sierra-research/tau2-bench): {infrastructure_error,
# context_window_exceeded}. Diverging from this set breaks comparability
# with published τ² numbers.
INFRA_TERMS = {"infrastructure_error", "context_window_exceeded"}
# Terminations scored as agent outcomes with done=True (natural stops).
NATURAL_TERMS = {"user_stop", "agent_stop"}
# {agent_error, max_steps, timeout, user_error, unexpected_error,
# too_many_errors} are scored failures upstream — we score them too.


class Tau2Adapter:
    name = NAME

    def __init__(self, tau2_dir: Path | None = None):
        self.dir = Path(
            tau2_dir or os.environ.get("OVERSEER_TAU2_DIR", "vendor/tau2-bench")
        )

    def requirements(self) -> list[str]:
        return ["cli:uv", f"dir:{self.dir}"]

    def available(self) -> tuple[bool, str]:
        if not self.dir.is_dir():
            return False, (
                f"tau2 checkout not found at {self.dir} (set OVERSEER_TAU2_DIR)"
            )
        if not has_cli("uv"):
            return False, "uv not installed"
        return True, "ok"

    def tau2_commit(self) -> str:
        try:
            out = subprocess.run(
                ["git", "rev-parse", "--short=12", "HEAD"],
                cwd=self.dir,
                capture_output=True,
                text=True,
                timeout=10,
            )
            return out.stdout.strip() if out.returncode == 0 else "unknown"
        except Exception:
            return "unknown"

    # ---- execution ----

    def run(
        self,
        domain: str,
        agent_llm: str,
        user_llm: str,
        trials: int,
        num_tasks: int | None,
        save_to: str,
    ) -> Path:
        if domain not in DOMAINS:
            raise BenchmarkUnavailable(f"tau2 domain {domain!r} not in {DOMAINS}")
        ok, why = self.available()
        if not ok:
            raise BenchmarkUnavailable(why)
        cmd = [
            "uv",
            "run",
            "tau2",
            "run",
            "--domain",
            domain,
            "--agent-llm",
            agent_llm,
            "--user-llm",
            user_llm,
            "--num-trials",
            str(trials),
            "--save-to",
            save_to,
        ]
        if num_tasks:
            cmd += ["--num-tasks", str(num_tasks)]
        env = dict(os.environ)
        # Fleet OpenAI-compatible gateway: litellm resolves
        # openai/<model> against OPENAI_BASE_URL.
        if agent_llm.startswith("openai/") or user_llm.startswith("openai/"):
            env.setdefault("OPENAI_API_KEY", os.environ.get("OVERSEER_API_KEY", ""))
            env.setdefault(
                "OPENAI_BASE_URL",
                os.environ.get("OVERSEER_BASE_URL", "https://opencode.ai/zen/go/v1"),
            )
        proc = subprocess.run(cmd, cwd=self.dir, env=env)
        if proc.returncode != 0:
            raise BenchmarkUnavailable(f"tau2 run failed (rc={proc.returncode})")
        results = self.dir / "data" / "simulations" / save_to / "results.json"
        if not results.exists():
            raise BenchmarkUnavailable(f"tau2 results not found at {results}")
        return results

    # ---- normalization ----

    def parse_results(
        self,
        results_path: Path,
        *,
        domain: str,
        agent_name: str,
        user_llm: str,
        model_id: str,
        harness_commit: str,
        run_set_id: str,
        benchmark: str = "tau2",
    ) -> list[dict]:
        """results.json → RunRecords. Deterministic ordering by
        (task_id, trial)."""
        data = json.loads(Path(results_path).read_text())
        sha = self.tau2_commit()
        env_digest = f"tau2@{sha}:{domain}"
        sims = sorted(
            data.get("simulations", []),
            key=lambda s: (str(s.get("task_id")), s.get("trial") or 0),
        )
        records = []
        used_trials: dict = {}
        for sim in sims:
            reward_info = sim.get("reward_info") or {}
            reward = reward_info.get("reward")
            term = sim.get("termination_reason", "")
            messages = sim.get("messages") or []
            steps = sum(1 for m in messages if m.get("role") == "assistant")
            usage = sim.get("agent_usage") or {}
            # trial is the seed axis; when absent, take the smallest unused
            # index for the task so missing trials can't collapse to seed 0
            # or collide with an explicit sibling trial.
            tid = sim.get("task_id")
            used = used_trials.setdefault(tid, set())
            trial = sim.get("trial")
            if trial is None or trial in used:
                trial = next(i for i in range(len(used) + 1) if i not in used)
            used.add(trial)
            task = ExternalTask(
                id=f"tau2-{domain}-{sim.get('task_id')}",
                version=1,
                env_digest=env_digest,
                tags=["tau2", domain],
            )
            outcome = {
                "done": term in NATURAL_TERMS,
                "pass": bool(reward is not None and reward >= 1.0),
                "score": reward,
                "infra_error": term in INFRA_TERMS,
                "error": None if term in NATURAL_TERMS else term,
                "steps": steps,
                "wall_s": (
                    round(float(sim["duration"]), 1)
                    if sim.get("duration") is not None
                    else None
                ),
                # None (absent), not 0 — fabricated zeros would poison
                # cost/token statistics.
                "tokens_in": usage.get("prompt_tokens"),
                "tokens_out": usage.get("completion_tokens"),
                "cost_usd": sim.get("agent_cost"),
                "model": model_id,
                "ts": int(time.time()),
            }
            rec = manifest.build_record(
                run_id=store.new_run_id(task.id, agent_name, trial),
                task=task,
                agent=agent_name,
                seed=trial,
                outcome=outcome,
                manifest=None,
                benchmark=benchmark,
                run_set_id=run_set_id,
                session_dir=str(results_path),
                judge_version=f"tau2-evaluator@{sha}",
                harness_commit=harness_commit,
            )
            rec["tau2"] = {
                "domain": domain,
                "user_llm": user_llm,  # pinned user-sim — comparability field
                "termination_reason": term,
                "reward_basis": reward_info.get("reward_basis"),
                "tau2_commit": sha,
            }
            records.append(rec)
        return records


def run_cli(
    adapter: Tau2Adapter, args, st: store.Store, harness_commit: str, progress
) -> int:
    ok, why = adapter.available()
    if not ok:
        print(f"tau2 unavailable: {why}", file=sys.stderr)
        return 2
    user_llm = args.tau2_user_llm
    if not user_llm:
        print(
            "tau2 requires --tau2-user-llm (pinned user simulator — "
            "scores aren't comparable across user models)",
            file=sys.stderr,
        )
        return 2
    agent = args.agents.split(",")[0].strip()  # τ² runs one agent arm
    agent_llm = (
        f"openai/{args.model or os.environ.get('OVERSEER_MODEL', 'deepseek-v4.1-flash')}"
        if agent == "overseer"
        else agent
    )
    header = st.matrix_header(
        benchmark="tau2",
        agents=[agent],
        k=args.tau2_trials,
        tasks=[f"tau2-{args.tau2_domain}"],
        scheduler_seed=args.scheduler_seed,
        extra={
            "harness_commit": harness_commit,
            "tau2_domain": args.tau2_domain,
            "user_llm": user_llm,
            "agent_llm": agent_llm,
        },
    )
    assert agent  # non-empty after split
    try:
        results = adapter.run(
            domain=args.tau2_domain,
            agent_llm=agent_llm,
            user_llm=user_llm,
            trials=args.tau2_trials,
            num_tasks=None,
            save_to=f"overseer-{args.tau2_domain}-{int(time.time())}",
        )
    except BenchmarkUnavailable as e:
        print(e, file=sys.stderr)
        return 2
    records = adapter.parse_results(
        results,
        domain=args.tau2_domain,
        agent_name=agent,
        user_llm=user_llm,
        model_id=agent_llm,
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
            "steps": f"{len(records)} τ² sims ingested",
            "wall_s": "",
            "tokens_in": "",
            "cost_usd": "",
        }
    )
    return 0
