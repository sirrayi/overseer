"""aider polyglot adapter — Exercism exercises for the overseer arm.

Source: github.com/Aider-AI/polyglot-benchmark (Exercism tracks,
~225 exercises across cpp/go/java/javascript/python/rust). Each exercise
is a real coding task: fill in the solution file(s) so the track's test
suite passes — the same set the aider leaderboard reports against,
which makes it our cheapest published-comparison surface.

Rollout: sparse-clone the repo, copy each exercise to a clean workdir,
overseer solves from `.docs/instructions.md` (+ the harness's own note
about which file to implement), grade = the track's test command in the
workdir. Grading runs on the worker's host toolchain — no docker
isolation in v1.

DEFERRED(polyglot): exercise grading is not container-isolated — the
  model could edit the test file to force a pass. Mitigation: after
  solving, the grader restores any file under the test-path list from
  the pristine exercise copy before running tests. Full container
  grading (aider's Dockerfile path) is still open.

Env: uv, git; the target language toolchain on PATH (pytest via uv for
python; cargo for rust; go for go). OPENCODE_API_KEY for the arm.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

from .. import agents, keys, manifest, store
from . import BenchmarkUnavailable, ExternalTask, has_cli

NAME = "polyglot"
REPO = "https://github.com/Aider-AI/polyglot-benchmark"

# Test entry per language track. Values: (command, extra marker files
# whose presence means "toolchain ready").
TEST_CMD = {
    "python": ["python", "-m", "pytest", "-x", "-q"],
    "rust": ["cargo", "test", "--quiet"],
    "go": ["go", "test", "./..."],
    "javascript": ["npm", "test", "--", "--silent"],
}
# Files the agent must implement (from .meta/config.json files.solution).
INSTR_NOTE = (
    "\n\nImplement the solution file(s) so the included test suite "
    "passes. Do not modify the test files. When done, stop."
)


def _sh(cmd, *, timeout=600, cwd=None, ok_rcs=(0,), env=None):
    return subprocess.run(cmd, capture_output=True, text=True,
                          timeout=timeout, cwd=cwd, env=env)


class PolyglotAdapter:
    name = NAME

    def __init__(self, lang: str = "python",
                 logs_root: Path | None = None):
        if lang not in TEST_CMD:
            raise BenchmarkUnavailable(
                f"polyglot: unsupported lang {lang!r} "
                f"(have graders for {sorted(TEST_CMD)})")
        self.lang = lang
        self.logs_root = Path(logs_root or "results/polyglot")

    def requirements(self):
        return ["cli:uv", "cli:git"]

    def available(self):
        if not has_cli("uv") or not has_cli("git"):
            return False, "uv/git required"
        return True, "ok"

    def _repo(self) -> Path:
        repo = self.logs_root / "polyglot-benchmark"
        if not (repo / self.lang).exists():
            _sh(["git", "clone", "--depth", "1", "--filter=blob:none",
                 "--sparse", REPO, str(repo)], timeout=900)
            p = _sh(["git", "sparse-checkout", "set", self.lang], cwd=repo)
            if p.returncode != 0:
                raise BenchmarkUnavailable(
                    f"sparse checkout failed: {p.stderr[-300:]}")
        return repo

    def exercises(self, limit=None) -> list[Path]:
        base = self._repo() / self.lang / "exercises" / "practice"
        if not base.exists():
            raise BenchmarkUnavailable(f"{base} missing in polyglot repo")
        xs = sorted(d for d in base.iterdir() if d.is_dir())
        return xs[:limit] if limit else xs

    def _prep(self, src: Path, ws: Path) -> dict:
        shutil.copytree(src, ws, dirs_exist_ok=True)
        cfg = {}
        cfg_path = src / ".meta" / "config.json"
        if cfg_path.exists():
            try:
                cfg = json.loads(cfg_path.read_text())
            except json.JSONDecodeError:
                pass
        return cfg

    def _instruction(self, src: Path, cfg: dict) -> str:
        doc = src / ".docs" / "instructions.md"
        text = doc.read_text() if doc.exists() else (
            f"Complete the {src.name} exercise.")
        sol = (cfg.get("files") or {}).get("solution") or []
        return text + INSTR_NOTE + (
            f"\n\nSolution file(s): {', '.join(sol)}" if sol else "")

    def _restore_tests(self, src: Path, ws: Path, cfg: dict):
        """Anti-gaming: copy every test-declared file back from the
        pristine exercise before grading."""
        for rel in (cfg.get("files") or {}).get("test") or []:
            orig, dst = src / rel, ws / rel
            if orig.exists():
                dst.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(orig, dst)

    def run(self, *, agent_name: str, model_id: str, limit: int | None,
            n_tasks: int | None, ws_root: Path, runs_root: Path,
            progress) -> list[dict]:
        solve = agents.get(agent_name).solve
        cap = limit or n_tasks
        records = []
        for src in self.exercises(limit=cap):
            iid = f"{self.lang}-{src.name}"
            ws = ws_root / iid
            if ws.exists():
                shutil.rmtree(ws)
            cfg = self._prep(src, ws)
            run_id = store.new_run_id(f"{NAME}-{iid}", agent_name, 0)
            sess = Path(runs_root) / run_id
            t0 = time.time()
            try:
                outcome = solve(self._instruction(src, cfg), str(ws),
                                str(sess), limits={"max_steps": 25},
                                seed=0)
            except Exception as e:
                outcome = {"done": False, "infra_error": True,
                           "error": f"rollout: {e}"}
            self._restore_tests(src, ws, cfg)
            test = _sh(TEST_CMD[self.lang], cwd=ws, timeout=600,
                       ok_rcs=(0, 1))
            passed = test.returncode == 0
            outcome.update({
                "pass": passed and outcome.get("done", False),
                "wall_s": round(time.time() - t0, 1),
                "grader_exit": test.returncode,
                "grader_tail": (test.stdout + test.stderr)[-800:],
            })
            progress({
                "task_id": iid, "harness": agent_name, "seed": 0,
                "pass": outcome["pass"],
                "infra_error": outcome.get("infra_error", False),
                "steps": outcome.get("steps") or 0,
                "wall_s": outcome["wall_s"],
                "tokens_in": outcome.get("tokens_in") or 0,
                "cost_usd": outcome.get("cost_usd") or 0,
            })
            records.append((iid, run_id, outcome, str(sess)))
        return records


def run_cli(adapter: PolyglotAdapter, args, st: store.Store,
            harness_commit: str, progress) -> int:
    ok, why = adapter.available()
    if not ok:
        print(f"polyglot unavailable: {why}", file=sys.stderr)
        return 2
    agent = args.agents.split(",")[0].strip()
    model = args.model or os.environ.get(
        "OVERSEER_MODEL", "muse-spark-1.3-contributor")
    if not keys.api_key():
        print(f"{keys.key_env()} required for polyglot rollouts",
              file=sys.stderr)
        return 2
    header = st.matrix_header(
        benchmark=NAME, agents=[agent], k=1,
        tasks=[f"{adapter.lang}:*"],
        scheduler_seed=args.scheduler_seed,
        extra={"harness_commit": harness_commit, "lang": adapter.lang})
    rows = adapter.run(
        agent_name=agent, model_id=model,
        limit=getattr(args, "swebench_limit", None),
        n_tasks=getattr(args, "n_tasks", None),
        ws_root=Path("workspaces/polyglot"),
        runs_root=Path("results/runs"), progress=progress)
    for iid, run_id, outcome, sess in rows:
        task = ExternalTask(
            id=f"{NAME}-{iid}", version=1,
            env_digest=f"polyglot@{adapter.lang}", tags=[NAME, adapter.lang])
        rec = manifest.build_record(
            run_id=run_id, task=task, agent=agent, seed=0,
            outcome=outcome, manifest=None, benchmark=NAME,
            run_set_id=header["run_set_id"], session_dir=sess,
            judge_version="polyglot-exercism",
            harness_commit=harness_commit)
        rec["polyglot"] = {"lang": adapter.lang, "exercise": iid}
        st.append(rec)
    progress({"task_id": "—", "harness": agent, "seed": "-", "pass": None,
              "infra_error": False,
              "steps": f"{len(rows)} polyglot trials ingested",
              "wall_s": "", "tokens_in": "", "cost_usd": ""})
    return 0
