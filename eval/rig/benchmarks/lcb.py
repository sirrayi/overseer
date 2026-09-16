"""LiveCodeBench adapter.

Runs the real `lcb_runner` from a checkout (env `OVERSEER_LCB_DIR`) and
normalizes `output/<model>/{scenario}_{n}_{temp}_eval_all.json` into
RunRecords. Per playbook: code_generation scenario with a post-cutoff
release window (`--release_version`); `--n` is the per-problem sample
count — each sample index becomes a seed so pass@1 / pass^k math stays
uniform with the rest of the portfolio.
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

NAME = "lcb"
SCENARIO = "codegeneration"


class LcbAdapter:
    name = NAME

    def __init__(self, lcb_dir: Path | None = None):
        self.dir = Path(
            lcb_dir or os.environ.get("OVERSEER_LCB_DIR", "vendor/LiveCodeBench")
        )

    def requirements(self) -> list[str]:
        return ["cli:python", f"dir:{self.dir}"]

    def available(self) -> tuple[bool, str]:
        if not self.dir.is_dir():
            return False, (
                f"LCB checkout not found at {self.dir} (set OVERSEER_LCB_DIR)"
            )
        if not has_cli("python3"):
            return False, "python3 not installed"
        return True, "ok"

    def lcb_commit(self) -> str:
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
        self, model_repr: str, n: int, temperature: float, release_version: str
    ) -> Path:
        ok, why = self.available()
        if not ok:
            raise BenchmarkUnavailable(why)
        cmd = [
            "python3",
            "-m",
            "lcb_runner.runner.main",
            "--model",
            model_repr,
            "--scenario",
            SCENARIO,
            "--evaluate",
            "--n",
            str(n),
            "--temperature",
            str(temperature),
            "--release_version",
            release_version,
        ]
        env = dict(os.environ)
        env.setdefault("OPENAI_API_KEY", os.environ.get("LEK_API_KEY", ""))
        env.setdefault(
            "OPENAI_BASE_URL",
            os.environ.get("LEK_BASE_URL", "https://inference.legionedge.ai/v1"),
        )
        proc = subprocess.run(cmd, cwd=self.dir, env=env)
        if proc.returncode != 0:
            raise BenchmarkUnavailable(f"lcb run failed (rc={proc.returncode})")
        path = (
            self.dir
            / "output"
            / model_repr
            / f"{SCENARIO}_{n}_{temperature}_eval_all.json"
        )
        if not path.exists():
            raise BenchmarkUnavailable(f"LCB results not found at {path}")
        return path

    # ---- normalization ----

    def parse_results(
        self,
        eval_all_path: Path,
        *,
        agent_name: str,
        model_id: str,
        harness_commit: str,
        run_set_id: str,
        release_version: str,
        benchmark: str = "lcb",
    ) -> list[dict]:
        """_eval_all.json → RunRecords: one record per (problem, sample).
        Deterministic ordering by (question_id, sample_idx)."""
        data = json.loads(Path(eval_all_path).read_text())
        sha = self.lcb_commit()
        env_digest = f"lcb@{sha}:{release_version}"
        rows = []
        for inst in data:
            for i, g in enumerate(inst.get("graded_list") or []):
                rows.append((str(inst.get("question_id")), i, g, inst))
        rows.sort(key=lambda r: (r[0], r[1]))
        records = []
        for qid, i, g, inst in rows:
            meta = inst.get("metadata") or {}
            task = ExternalTask(
                id=f"lcb-{qid}",
                version=1,
                env_digest=env_digest,
                tags=["lcb", str(inst.get("platform") or "")],
                difficulty=inst.get("difficulty"),
            )
            outcome = {
                "done": True,
                "pass": bool(g),
                "score": 1.0 if g else 0.0,
                "steps": 1,  # one generation per sample — semantically true
                "wall_s": None,
                # None (absent), not 0 — fabricated zeros would poison
                # cost/token statistics.
                "tokens_in": meta.get("input_tokens"),
                "tokens_out": meta.get("output_tokens"),
                "cost_usd": meta.get("cost_usd"),
                "model": model_id,
                "ts": int(time.time()),
            }
            rec = manifest.build_record(
                run_id=store.new_run_id(task.id, agent_name, i),
                task=task,
                agent=agent_name,
                seed=i,
                outcome=outcome,
                manifest=None,
                benchmark=benchmark,
                run_set_id=run_set_id,
                session_dir=str(eval_all_path),
                judge_version=f"lcb@{sha}",
                harness_commit=harness_commit,
            )
            rec["lcb"] = {
                "release_version": release_version,
                "contest_date": inst.get("contest_date"),
                "sample_idx": i,
                "n_samples": len(inst.get("graded_list") or []),
                "lcb_commit": sha,
            }
            records.append(rec)
        return records


def run_cli(
    adapter: LcbAdapter, args, st: store.Store, harness_commit: str, progress
) -> int:
    ok, why = adapter.available()
    if not ok:
        print(f"lcb unavailable: {why}", file=sys.stderr)
        return 2
    model_repr = args.model or os.environ.get("LEK_MODEL", "kimi-k3-turbo")
    release = getattr(args, "release_version", "v6")
    agent = args.agents.split(",")[0].strip()
    header = st.matrix_header(
        benchmark="lcb",
        agents=[agent],
        k=args.seeds,
        tasks=["lcb-codegen"],
        scheduler_seed=args.scheduler_seed,
        extra={
            "harness_commit": harness_commit,
            "release_version": release,
            "model_repr": model_repr,
        },
    )
    try:
        path = adapter.run(
            model_repr=model_repr,
            n=args.seeds,
            temperature=0.0,
            release_version=release,
        )
    except BenchmarkUnavailable as e:
        print(e, file=sys.stderr)
        return 2
    records = adapter.parse_results(
        path,
        agent_name=agent,
        model_id=model_repr,
        harness_commit=harness_commit,
        run_set_id=header["run_set_id"],
        release_version=release,
    )
    for rec in records:
        st.append(rec)
        progress(rec)
    return 0
