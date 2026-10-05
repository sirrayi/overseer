"""SWE-bench-Live adapter (playbook Ch.12 §4, contamination-resistant arm).

SWE-bench-Live refreshes monthly (+50 verified issues) — tasks the model
cannot have memorized, which is exactly the contamination story the
ledger needs. Protocol differences vs SWE-bench proper (per their
evaluation/README):

- Rollout runs INSIDE the per-instance docker image; the repo lives at
  /testbed. The agent sees only `problem_statement` — hint/FAIL_TO_PASS/
  test_patch are withheld (we never load them into the prompt).
- Patch = `cd /testbed && git --no-pager diff HEAD --text`.
- Predictions are a JSON dict {instance_id: {"model_patch": …}}.
- Grading: `python -m evaluation.evaluation` inside a cloned
  SWE-bench-Live repo (pip install -e .) — their own harness, not the
  swebench package.

Env: docker + uv + git; OPENCODE_API_KEY for rollouts (overseer arm runs
`--provider opencode` inside the container — the harness binary adds the
x-opencode-session header itself, no relay needed).
"""

from __future__ import annotations

import json
import os
import platform
import subprocess
import sys
import time
from pathlib import Path

from .. import agents, manifest, store
from . import BenchmarkUnavailable, ExternalTask, has_cli, has_docker

NAME = "swe_live"
DATASET = "SWE-bench-Live/SWE-bench-Live"   # Python split; MultiLang via --swe-live-dataset
SPLIT = "test"
LIVE_REPO = "https://github.com/microsoft/SWE-bench-Live"

INSTRUCTION_TMPL = """\
Resolve this issue in the repository checkout at your cwd (already at the
base commit). Make the minimal correct change to the non-test source.
Do NOT commit; leave your edits in the working tree.

## Issue

{problem}
"""


def _uv(pkgs: list[str], code_or_mod: list[str], *, timeout=600, cwd=None):
    return subprocess.run(
        ["uv", "run", "--no-project",
         *(x for p in pkgs for x in ("--with", p)), *code_or_mod],
        capture_output=True, text=True, timeout=timeout, cwd=cwd,
    )


def _sh(cmd: list[str], *, timeout=600, cwd=None, ok_rcs=(0,), env=None):
    p = subprocess.run(cmd, capture_output=True, text=True,
                       timeout=timeout, cwd=cwd, env=env)
    if p.returncode not in ok_rcs:
        raise BenchmarkUnavailable(
            f"{cmd[0]} rc={p.returncode}: {(p.stderr or '')[-300:]}")
    return p


class SweLiveAdapter:
    name = NAME

    def __init__(self, dataset: str = DATASET, split: str = SPLIT,
                 logs_root: Path | None = None):
        self.dataset = dataset
        self.split = split
        self.logs_root = Path(logs_root or "results/swe_live")

    def requirements(self):
        return ["docker", "cli:uv", "cli:git"]

    def available(self):
        if not has_docker():
            return False, "docker daemon unreachable"
        if not has_cli("uv"):
            return False, "uv not installed"
        return True, "ok"

    def check_or_raise(self):
        ok, why = self.available()
        if not ok:
            raise BenchmarkUnavailable(f"{self.name}: {why}")

    def materialize_dataset(self) -> Path:
        out = self.logs_root / f"dataset-{self.split}.jsonl"
        if out.exists():
            return out
        code = (
            "import json,sys\n"
            "from datasets import load_dataset\n"
            f"ds=load_dataset({json.dumps(self.dataset)},split={json.dumps(self.split)})\n"
            "w=open(sys.argv[1],'w')\n"
            "for r in ds: w.write(json.dumps(dict(r))+'\\n')\n"
        )
        self.logs_root.mkdir(parents=True, exist_ok=True)
        proc = _uv(["datasets"], ["python", "-c", code, str(out)], timeout=900)
        if proc.returncode != 0 or not out.exists():
            raise BenchmarkUnavailable(
                f"live dataset materialize failed: {proc.stderr[-400:]}")
        return out

    def load_instances(self, limit=None, instance_ids=None):
        rows = [json.loads(ln) for ln in
                self.materialize_dataset().read_text().splitlines() if ln.strip()]
        if instance_ids:
            have = {r["instance_id"] for r in rows}
            missing = set(instance_ids) - have
            if missing:
                raise BenchmarkUnavailable(f"unknown ids: {sorted(missing)}")
            wanted = set(instance_ids)
            rows = [r for r in rows if r["instance_id"] in wanted]
        return rows[:limit] if limit else rows

    # ---- rollout: overseer INSIDE the instance image ----

    # DEFERRED(swe_live): image-field name is a guess across Live's
    # dataset revisions ("image"/"docker_image"/"instance_image") —
    # tighten once a real row names it definitively.
    def _image(self, inst: dict) -> str:
        for k in ("image", "docker_image", "instance_image"):
            if inst.get(k):
                return inst[k]
        raise BenchmarkUnavailable(
            f"{inst['instance_id']}: no docker image field in dataset row")

    def generate_predictions(self, instances, *, agent_name, model_id,
                             limits, runs_root, progress) -> Path:
        """One container per instance; overseer exec inside; diff out.
        Returns the predictions JSON path (Live dict format)."""
        # DEFERRED(swe_live): the host binary is docker-cp'd into the
        # task image — requires same arch+glibc. Devin workers are
        # x86_64 Linux matching Live's images; macOS local runs will
        # fail at exec time. A static-musl build or per-image install
        # is the fix if cross-arch matters.
        solve_bin = os.environ.get(
            "OVERSEER_BIN", str(Path("..") / "target" / "release" / "overseer"))
        preds_path = self.logs_root / "preds.json"
        sidecar = self.logs_root / "rollouts.jsonl"
        preds = {}
        with sidecar.open("w") as sf:
            for inst in instances:
                iid = inst["instance_id"]
                cid = f"swel-{iid.replace('/', '-')}-{os.getpid()}"
                outcome = {"done": False, "infra_error": False}
                try:
                    _sh(["docker", "run", "-d", "--name", cid,
                         self._image(inst), "sleep", "infinity"], timeout=300)
                    _sh(["docker", "cp", solve_bin, f"{cid}:/opt/overseer"],
                        timeout=120)
                    _sh(["docker", "exec", cid, "chmod", "+x", "/opt/overseer"])
                    problem = inst["problem_statement"]
                    _sh(["docker", "exec", cid, "bash", "-lc",
                         "mkdir -p /opt/ov-session"],
                        timeout=60)
                    env = ["OPENCODE_API_KEY=" +
                           os.environ.get("OPENCODE_API_KEY", "")]
                    p = subprocess.run(
                        ["docker", "exec", *[x for kv in env for x in ("-e", kv)],
                         cid, "/opt/overseer", "exec", "--json",
                         "--provider", "opencode", "--model", model_id,
                         "--cwd", "/testbed", "--full-access",
                         "--max-steps", str(limits.get("max_steps", 40)),
                         "--session", "/opt/ov-session", "-"],
                        input=problem, capture_output=True, text=True,
                        timeout=limits.get("wall_s", 1500), env={
                            **os.environ,
                            "PATH": os.environ.get("PATH", ""),
                        })
                    events = [json.loads(l) for l in p.stdout.splitlines()
                              if l.strip().startswith("{")]
                    run_end = next((e for e in reversed(events)
                                    if e.get("type") == "run_end"), {})
                    outcome = {
                        "done": p.returncode == 0,
                        "stop_reason": run_end.get("stop_reason"),
                        "provider_error":
                            run_end.get("stop_reason") == "provider_error",
                        "steps": sum(1 for e in events
                                     if e.get("type") == "model_response"),
                        "wall_s": limits.get("wall_s", 0),
                        "stderr": (p.stderr or "")[-1500:],
                    }
                    diff = _sh(
                        ["docker", "exec", cid, "bash", "-lc",
                         "cd /testbed && git --no-pager diff HEAD --text"],
                        timeout=120)
                    preds[iid] = {"model_patch": diff.stdout}
                    # trajectory for leaderboard-submission compliance
                    traj = _sh(
                        ["docker", "exec", cid, "bash", "-lc",
                         "cat /opt/ov-session/events.jsonl 2>/dev/null || true"],
                        timeout=60, ok_rcs=(0, 1))
                    td = Path(runs_root) / f"swe_live-{iid}"
                    td.mkdir(parents=True, exist_ok=True)
                    (td / "events.jsonl").write_text(traj.stdout)
                    outcome["session_dir"] = str(td)
                except (BenchmarkUnavailable, subprocess.TimeoutExpired,
                        json.JSONDecodeError, OSError) as e:
                    outcome.update(done=False, infra_error=True,
                                   error=f"{type(e).__name__}: {e}")
                    preds[iid] = {"model_patch": ""}
                finally:
                    subprocess.run(["docker", "rm", "-f", cid],
                                   capture_output=True, timeout=60)
                sf.write(json.dumps({"instance_id": iid,
                                     "outcome": outcome}) + "\n")
                progress({"task_id": iid, "harness": agent_name, "seed": 0,
                          "pass": None,
                          "infra_error": outcome.get("infra_error", False),
                          "steps": outcome.get("steps") or 0,
                          "wall_s": outcome.get("wall_s") or 0,
                          "tokens_in": 0, "cost_usd": 0})
        preds_path.write_text(json.dumps(preds, indent=2))
        return preds_path

    # ---- evaluation: their harness in their repo ----

    def _eval_repo(self) -> Path:
        repo = self.logs_root / "SWE-bench-Live"
        if not (repo / "evaluation").exists():
            _sh(["git", "clone", "--depth", "1", "--filter=blob:none",
                 "--sparse", LIVE_REPO, str(repo)], timeout=600)
            _sh(["git", "sparse-checkout", "set", "evaluation"], cwd=repo)
        py = repo / ".venv" / "bin" / "python"
        if not py.exists():
            _sh(["uv", "venv", ".venv"], cwd=repo)
            _sh(["uv", "pip", "install", "-e", ".", "datasets", "docker"],
                cwd=repo, timeout=900)
        return repo

    def evaluate(self, preds_path: Path, run_id: str, *, workers=4) -> Path:
        repo = self._eval_repo()
        out_dir = (self.logs_root / "eval-out" / run_id).resolve()
        # --patch_dir expects a dir of per-instance patch jsons OR the
        # preds file itself (their CLI treats both; verified in smoke).
        patch_dir = (self.logs_root / "patches" / run_id).resolve()
        patch_dir.mkdir(parents=True, exist_ok=True)
        preds = json.loads(Path(preds_path).read_text())
        for iid, body in preds.items():
            (patch_dir / f"{iid}.json").write_text(json.dumps(body))
        proc = subprocess.run(
            [str(repo / ".venv" / "bin" / "python"),
             "-m", "evaluation.evaluation",
             "--dataset", self.dataset, "--split", self.split,
             "--platform", "linux", "--patch_dir", str(patch_dir),
             "--output_dir", str(out_dir), "--workers", str(workers),
             "--overwrite", "1"],
            capture_output=True, text=True, timeout=86400, cwd=repo)
        (self.logs_root / f"{run_id}.eval.log").write_text(
            (proc.stdout or "") + "\n=== STDERR ===\n" + (proc.stderr or ""))
        if proc.returncode != 0:
            raise BenchmarkUnavailable(
                f"live eval rc={proc.returncode} — see "
                f"{self.logs_root / (run_id + '.eval.log')}")
        return out_dir

    def parse_results(self, out_dir: Path, *, agent_name, model_id,
                      harness_commit, run_set_id) -> list[dict]:
        """eval output → RunRecord per instance (resolved flag)."""
        rollouts = {}
        sc = self.logs_root / "rollouts.jsonl"
        if sc.exists():
            for ln in sc.read_text().splitlines():
                if ln.strip():
                    r = json.loads(ln)
                    rollouts[r["instance_id"]] = r.get("outcome") or {}
        records = []
        verdicts = {}
        for f in Path(out_dir).rglob("*.json"):
            try:
                body = json.loads(f.read_text())
            except json.JSONDecodeError:
                continue
            # their writers emit {iid: {"resolved": bool, ...}} shapes
            for iid, v in (body.items() if isinstance(body, dict) else []):
                if isinstance(v, dict) and "resolved" in v:
                    verdicts[iid] = v
        for iid, v in sorted(verdicts.items()):
            out = dict(rollouts.get(iid) or {})
            out.update({"model": model_id, "done": out.get("done", True),
                        "pass": bool(v.get("resolved")),
                        "infra_error": bool(out.get("infra_error"))})
            task = ExternalTask(
                id=f"{NAME}-{iid}", version=1,
                env_digest=f"{self.dataset}@{self.split}", tags=[NAME])
            rec = manifest.build_record(
                run_id=f"{run_set_id}-{iid}", task=task, agent=agent_name,
                seed=0, outcome=out, manifest=None, benchmark=NAME,
                run_set_id=run_set_id,
                session_dir=out.get("session_dir", ""),
                judge_version="swe-bench-live",
                harness_commit=harness_commit)
            rec["swe_live"] = {"dataset": self.dataset,
                               "report": str(out_dir)}
            records.append(rec)
        return records


def run_cli(adapter: SweLiveAdapter, args, st: store.Store,
            harness_commit: str, progress) -> int:
    ok, why = adapter.available()
    if not ok:
        print(f"swe_live unavailable: {why}", file=sys.stderr)
        return 2
    agent = args.agents.split(",")[0].strip()
    model = args.model or os.environ.get(
        "OVERSEER_MODEL", "muse-spark-1.3-contributor")
    if not os.environ.get("OPENCODE_API_KEY"):
        print("OPENCODE_API_KEY required for swe_live rollouts",
              file=sys.stderr)
        return 2
    limit = getattr(args, "n_tasks", None) or getattr(args, "swebench_limit", None)
    ids = (args.swebench_ids.split(",")
           if getattr(args, "swebench_ids", None) else None)
    instances = adapter.load_instances(limit=limit, instance_ids=ids)
    header = st.matrix_header(
        benchmark=NAME, agents=[agent], k=1,
        tasks=[i["instance_id"] for i in instances],
        scheduler_seed=args.scheduler_seed,
        extra={"harness_commit": harness_commit,
               "dataset": adapter.dataset})
    preds = adapter.generate_predictions(
        instances, agent_name=agent, model_id=model, limits={},
        runs_root="results/runs", progress=progress)
    out_dir = adapter.evaluate(
        preds, header["run_set_id"],
        workers=getattr(args, "swebench_max_workers", 4))
    n = 0
    for rec in adapter.parse_results(
            out_dir, agent_name=agent, model_id=model,
            harness_commit=harness_commit,
            run_set_id=header["run_set_id"]):
        st.append(rec)
        progress(rec)
        n += 1
    progress({"task_id": "—", "harness": agent, "seed": "-", "pass": None,
              "infra_error": False,
              "steps": f"{n} swe_live reports ingested",
              "wall_s": "", "tokens_in": "", "cost_usd": ""})
    return 0
