"""SWE-bench Verified adapter (playbook Ch.8 §7.1 Tier 1).

Two halves with different cost profiles:

1. rollouts (PAID — model calls): per instance, clone `repo` at
   `base_commit`, run an agent arm on `problem_statement`, harvest the
   working-tree diff as `model_patch` → predictions JSONL.
2. evaluation (FREE — docker only): `swebench.harness.run_evaluation`
   spins the per-instance eval image, applies the patch, runs FAIL_TO_PASS
   + PASS_TO_PASS tests, writes
   `logs/run_evaluation/<run_id>/<model>/<iid>/report.json`.

`--predictions-path` (or the literal `gold`) skips rollouts entirely —
evaluating a provided/gold patch exercises the whole docker path with no
model spend. Verified is the 500-row human-verified subset; swebench ≥5.x
requires the `SWE-bench/` org mirror (carries the per-instance `image`
column the harness needs).

k-seed support: SWE-bench keys predictions on instance_id, so seeds map to
separate predictions files (`preds-s<seed>.jsonl`) and separate eval
run_ids (`<run_set>-s<seed>`); each (instance, seed) pair is one RunRecord.
"""

from __future__ import annotations

import json
import os
import platform
import subprocess
import sys
import time
from pathlib import Path

from .. import agents, keys, manifest, store
from . import BenchmarkUnavailable, ExternalTask, has_cli, has_docker

NAME = "swe_bench"
DATASET = "SWE-bench/SWE-bench_Verified"
SPLIT = "test"
SWEBENCH_PKG = "swebench"

INSTRUCTION_TMPL = """\
Resolve this issue in the repository checkout at your cwd (already at the
base commit). Make the minimal correct change to the non-test source.
Do NOT commit; leave your edits in the working tree.

## Issue

{problem}
"""


def _uv_python(pkgs: list[str], code: str, timeout: int = 300, cwd=None):
    """Run `python -c code` in an ephemeral uv env — keeps the eval project
    itself stdlib-only while letting us shell out to swebench/datasets."""
    return subprocess.run(
        [
            "uv",
            "run",
            "--no-project",
            *(x for p in pkgs for x in ("--with", p)),
            "python",
            "-c",
            code,
        ],
        capture_output=True,
        text=True,
        timeout=timeout,
        cwd=cwd,
    )


class SweBenchAdapter:
    name = NAME

    def __init__(
        self,
        dataset: str = DATASET,
        split: str = SPLIT,
        logs_root: Path | None = None,
    ):
        self.dataset = dataset
        self.split = split
        # run_evaluation writes logs/run_evaluation relative to its cwd.
        self.logs_root = Path(logs_root or "results/swebench")

    def requirements(self) -> list[str]:
        return ["docker", "cli:uv"]

    def available(self) -> tuple[bool, str]:
        if not has_docker():
            return False, "docker daemon unreachable — install/start OrbStack"
        if not has_cli("uv"):
            return False, "uv not installed"
        return True, "ok"

    def check_or_raise(self):
        ok, why = self.available()
        if not ok:
            raise BenchmarkUnavailable(f"{self.name}: {why}")

    # ---- dataset ----

    @staticmethod
    def _image_arch() -> str:
        """swebench image names encode arch (sweb.eval.x86_64.* /
        sweb.eval.arm64.*) — rewrite to the host arch or pulls 404."""
        return "arm64" if platform.machine() in ("arm64", "aarch64") else "x86_64"

    def materialize_dataset(self) -> Path:
        """Dump the HF dataset to a local JSONL with `image` rewritten to
        the host arch. run_evaluation accepts .jsonl for --dataset_name;
        the file also pins the exact task set the eval ran against."""
        out = self.logs_root / f"dataset-{self.split}.jsonl"
        if out.exists():
            return out
        arch = self._image_arch()
        code = (
            "import json,sys\n"
            "from datasets import load_dataset\n"
            f"ds=load_dataset({json.dumps(self.dataset)},split={json.dumps(self.split)})\n"
            f"arch={json.dumps(arch)}\n"
            "w=open(sys.argv[1],'w')\n"
            "for r in ds:\n"
            "    d=dict(r)\n"
            "    img=d.get('image') or ''\n"
            "    if 'sweb.eval.' in img and '.x86_64.' in img and arch=='arm64':\n"
            "        d['image']=img.replace('.x86_64.','.arm64.')\n"
            "    elif 'sweb.eval.' in img and '.arm64.' in img and arch=='x86_64':\n"
            "        d['image']=img.replace('.arm64.','.x86_64.')\n"
            "    w.write(json.dumps(d)+'\\n')\n"
        )
        self.logs_root.mkdir(parents=True, exist_ok=True)
        proc = subprocess.run(
            [
                "uv",
                "run",
                "--no-project",
                "--with",
                "datasets",
                "python",
                "-c",
                code,
                str(out),
            ],
            capture_output=True,
            text=True,
            timeout=600,
        )
        if proc.returncode != 0 or not out.exists():
            raise BenchmarkUnavailable(
                f"dataset materialize failed: {proc.stderr[-400:]}"
            )
        return out

    def load_instances(
        self, limit: int | None = None, instance_ids: list[str] | None = None
    ) -> list[dict]:
        ds_path = self.materialize_dataset()
        rows = [json.loads(ln) for ln in ds_path.read_text().splitlines() if ln.strip()]
        if instance_ids:
            have = {r["instance_id"] for r in rows}
            missing = set(instance_ids) - have
            if missing:
                raise BenchmarkUnavailable(f"unknown instance_ids: {sorted(missing)}")
            wanted = set(instance_ids)
            rows = [r for r in rows if r["instance_id"] in wanted]
            rows.sort(key=lambda r: instance_ids.index(r["instance_id"]))
        return rows[:limit] if limit else rows

    # ---- rollouts (PAID) ----

    def prepare_workdir(self, inst: dict, ws_root: Path) -> Path:
        """Blobless clone of repo at base_commit. A reused checkout is
        reset to a pristine base_commit first — the prior rollout's staged
        diff (`_diff` runs `git add -A`) must never leak into the next
        seed/arm's model_patch."""
        wd = Path(ws_root) / inst["instance_id"]
        if (wd / ".git").exists():
            for cmd in (
                ["git", "reset", "--hard", inst["base_commit"]],
                ["git", "clean", "-fdx"],
            ):
                subprocess.run(
                    cmd, cwd=wd, check=True, capture_output=True, timeout=600
                )
            return wd
        wd.parent.mkdir(parents=True, exist_ok=True)
        url = f"https://github.com/{inst['repo']}.git"
        subprocess.run(
            ["git", "clone", "--filter=blob:none", url, str(wd)],
            check=True,
            capture_output=True,
            timeout=1800,
        )
        subprocess.run(
            ["git", "checkout", inst["base_commit"]],
            cwd=wd,
            check=True,
            capture_output=True,
            timeout=600,
        )
        return wd

    def _diff(self, wd: Path) -> str:
        """Working-tree patch vs base commit (new files included)."""
        subprocess.run(["git", "add", "-A"], cwd=wd, check=True, capture_output=True)
        out = subprocess.run(
            ["git", "diff", "--cached", "--binary"],
            cwd=wd,
            check=True,
            capture_output=True,
        )
        # surrogateescape keeps non-UTF8 hunks byte-stable — text=True
        # would mangle them.
        return out.stdout.decode("utf-8", errors="surrogateescape")

    def generate_predictions(
        self,
        instances: list[dict],
        *,
        agent_name: str,
        model_id: str,
        seeds: list[int],
        limits: dict,
        ws_root: Path,
        runs_root: Path,
        progress,
    ) -> dict[int, Path]:
        """Returns {seed: predictions.jsonl}. Rollout metrics land in a
        rollouts.jsonl sidecar next to each predictions file."""
        solve = agents.get(agent_name).solve
        out: dict[int, Path] = {}
        for seed in seeds:
            preds = self.logs_root / f"preds-s{seed}.jsonl"
            sidecar = self.logs_root / f"rollouts-s{seed}.jsonl"
            preds.parent.mkdir(parents=True, exist_ok=True)
            with preds.open("w") as pf, sidecar.open("w") as sf:
                for inst in instances:
                    iid = inst["instance_id"]
                    run_id = store.new_run_id(f"{NAME}-{iid}", agent_name, seed)
                    sess = Path(runs_root) / run_id
                    try:
                        wd = self.prepare_workdir(inst, ws_root)
                        outcome = solve(
                            INSTRUCTION_TMPL.format(problem=inst["problem_statement"]),
                            str(wd),
                            str(sess),
                            limits=limits,
                            seed=seed,
                        )
                        patch = self._diff(wd)
                    except Exception as e:
                        outcome = {
                            "done": False,
                            "error": f"rollout: {e}",
                            "infra_error": True,
                            "ts": int(time.time()),
                        }
                        patch = ""
                    pf.write(
                        json.dumps(
                            {
                                "instance_id": iid,
                                "model_name_or_path": model_id,
                                "model_patch": patch,
                            }
                        )
                        + "\n"
                    )
                    sf.write(
                        json.dumps(
                            {
                                "instance_id": iid,
                                "run_id": run_id,
                                "session_dir": str(sess),
                                "outcome": outcome,
                            }
                        )
                        + "\n"
                    )
                    progress(
                        {
                            "task_id": iid,
                            "harness": agent_name,
                            "seed": seed,
                            "pass": None,
                            "infra_error": outcome.get("infra_error", False),
                            "steps": outcome.get("steps") or 0,
                            "wall_s": outcome.get("wall_s") or 0,
                            "tokens_in": outcome.get("tokens_in") or 0,
                            "cost_usd": outcome.get("cost_usd") or 0,
                        }
                    )
            out[seed] = preds
        return out

    # ---- evaluation (FREE — docker only) ----

    def evaluate(
        self,
        predictions_path: Path | str,
        run_id: str,
        *,
        instance_ids: list[str] | None = None,
        max_workers: int = 4,
        timeout: int = 1800,
    ) -> Path:
        """Run the official harness. Returns the log dir root."""
        ds_path = self.materialize_dataset()
        cmd = [
            "uv",
            "run",
            "--no-project",
            "--with",
            SWEBENCH_PKG,
            "python",
            "-m",
            "swebench.harness.run_evaluation",
            "--dataset_name",
            str(ds_path.resolve()),
            "--split",
            self.split,
            "--predictions_path",
            # resolve before cwd=logs_root — a relative path would bind
            # under the wrong directory
            str(predictions_path)
            if str(predictions_path) == "gold"
            else str(Path(predictions_path).resolve()),
            "--run_id",
            run_id,
            "--max_workers",
            str(max_workers),
            "--timeout",
            str(timeout),
            "--report_dir",
            str(self.logs_root),
        ]
        if instance_ids:
            cmd += ["--instance_ids", *instance_ids]
        self.logs_root.mkdir(parents=True, exist_ok=True)
        try:
            proc = subprocess.run(
                cmd,
                cwd=self.logs_root,
                capture_output=True,
                text=True,
                timeout=max(timeout * 2, 3600),
            )
        except subprocess.TimeoutExpired as e:
            raise BenchmarkUnavailable(
                f"run_evaluation timed out after {e.timeout}s"
            ) from e
        (self.logs_root / f"{run_id}.stdout.log").write_text(
            (proc.stdout or "") + "\n=== STDERR ===\n" + (proc.stderr or "")
        )
        if proc.returncode != 0:
            raise BenchmarkUnavailable(
                f"run_evaluation rc={proc.returncode} — see "
                f"{self.logs_root / (run_id + '.stdout.log')}"
            )
        return self.logs_root / "logs" / "run_evaluation" / run_id

    def _load_rollouts(self, seed: int) -> dict:
        sidecar = self.logs_root / f"rollouts-s{seed}.jsonl"
        if not sidecar.exists():
            return {}
        out = {}
        for ln in sidecar.read_text().splitlines():
            if ln.strip():
                row = json.loads(ln)
                out[row["instance_id"]] = row
        return out

    def parse_results(
        self,
        run_log_dir: Path,
        *,
        agent_name: str,
        model_id: str,
        seed: int,
        harness_commit: str,
        run_set_id: str,
        benchmark: str = NAME,
    ) -> list[dict]:
        """Per-instance report.json → RunRecord; rollout sidecar supplies
        token/cost/session provenance when rollouts ran here."""
        run_log_dir = Path(run_log_dir)
        rollouts = self._load_rollouts(seed)
        env_digest = f"{self.dataset}@{self.split}:{self._image_arch()}"
        records = []
        for report in sorted(run_log_dir.glob("*/*/report.json")):
            iid = report.parent.name
            # reports land at <run_id>/<model_name_or_path>/<iid>/ —
            # the dir name is the honest evaluated-model label (e.g. gold)
            evaluated_model = report.parent.parent.name
            body = json.loads(report.read_text())
            verdict = body.get(iid) or next(iter(body.values()), {})
            roll = rollouts.get(iid, {})
            outcome = dict(roll.get("outcome") or {})
            outcome.update(
                {
                    "model": outcome.get("model") or evaluated_model,
                    "done": outcome.get("done", True),
                    "pass": bool(verdict.get("resolved")),
                    # SWE-bench convention: a missing/unapplying patch is
                    # a scored failure (resolved=False), NOT infra — only
                    # rollout-side transport failures are.
                    "infra_error": bool(outcome.get("infra_error")),
                    "error": outcome.get("error"),
                }
            )
            task = ExternalTask(
                id=f"{NAME}-{iid}",
                version=1,
                env_digest=env_digest,
                tags=[NAME, iid.split("__")[0]],
            )
            rec = manifest.build_record(
                # deterministic id for eval-only ingests — re-parsing the
                # same eval dir reproduces the same run_id instead of
                # minting duplicates in the append-only store
                run_id=roll.get("run_id") or f"{run_set_id}-s{seed}-{iid}",
                task=task,
                agent=agent_name,
                seed=seed,
                outcome=outcome,
                manifest=None,
                benchmark=benchmark,
                run_set_id=run_set_id,
                session_dir=roll.get("session_dir") or str(report),
                judge_version=f"swebench@{self._swebench_version()}",
                harness_commit=harness_commit,
            )
            rec["swe_bench"] = {
                "dataset": self.dataset,
                "split": self.split,
                "patch_exists": verdict.get("patch_exists"),
                "patch_applied": verdict.get("patch_successfully_applied"),
                "tests_status": verdict.get("tests_status"),
                "eval_only": not roll,
                "report": str(report),
            }
            records.append(rec)
        return records

    def _swebench_version(self) -> str:
        try:
            proc = _uv_python(
                [SWEBENCH_PKG],
                "import swebench;print(getattr(swebench,'__version__','?'))",
                timeout=120,
            )
            return proc.stdout.strip() if proc.returncode == 0 else "unknown"
        except Exception:
            return "unknown"


def run_cli(
    adapter: SweBenchAdapter, args, st: store.Store, harness_commit: str, progress
) -> int:
    ok, why = adapter.available()
    if not ok:
        print(f"swe_bench unavailable: {why}", file=sys.stderr)
        return 2

    instance_ids = (
        args.swebench_ids.split(",") if getattr(args, "swebench_ids", None) else None
    )
    limit = getattr(args, "swebench_limit", None)
    preds_arg = getattr(args, "predictions_path", None)
    # eval-only: identical predictions per seed would rerun the same eval
    # k times — collapse to a single seed. --swebench-limit also applies
    # here by restricting --instance_ids.
    if preds_arg is not None:
        seeds = [0]
        if limit and not instance_ids:
            instance_ids = [
                r["instance_id"] for r in adapter.load_instances(limit=limit)
            ]
    else:
        seeds = list(range(args.seeds))
    agent = args.agents.split(",")[0].strip()
    model_id = args.model or os.environ.get("OVERSEER_MODEL", "deepseek-v4.1-flash")
    if preds_arg is None and not keys.api_key():
        print(
            f"{keys.key_env()} required for swe_bench rollouts "
            "(or pass --predictions-path for eval-only)",
            file=sys.stderr,
        )
        return 2
    run_set = st.matrix_header(
        benchmark=NAME,
        agents=[agent],
        k=len(seeds),
        tasks=instance_ids or [f"{NAME}-{adapter.split}[:{limit or 'all'}]"],
        scheduler_seed=args.scheduler_seed,
        extra={
            "harness_commit": harness_commit,
            "dataset": adapter.dataset,
            "split": adapter.split,
            "predictions": preds_arg or "generated",
        },
    )
    rsid = run_set["run_set_id"]

    instances: list[dict] = []
    if preds_arg is None:  # rollout path needs instance rows
        instances = adapter.load_instances(limit=limit, instance_ids=instance_ids)
        preds_by_seed = adapter.generate_predictions(
            instances,
            agent_name=agent,
            model_id=model_id,
            seeds=seeds,
            limits={},
            ws_root=Path("workspaces/swebench"),
            runs_root=Path("results/runs"),
            progress=progress,
        )
    else:  # eval-only: gold or a provided predictions file
        preds_by_seed = {s: preds_arg for s in seeds}

    total = 0
    for seed, preds in preds_by_seed.items():
        run_id = f"{rsid}-s{seed}"
        try:
            log_dir = adapter.evaluate(
                preds,
                run_id,
                instance_ids=instance_ids,
                max_workers=getattr(args, "swebench_max_workers", 4),
            )
        except BenchmarkUnavailable as e:
            print(e, file=sys.stderr)
            return 2
        for rec in adapter.parse_results(
            log_dir,
            agent_name=agent,
            model_id=model_id,
            seed=seed,
            harness_commit=harness_commit,
            run_set_id=rsid,
        ):
            st.append(rec)
            progress(rec)
            total += 1
    progress(
        {
            "task_id": "—",
            "harness": agent,
            "seed": "-",
            "pass": None,
            "infra_error": False,
            "steps": f"{total} swe_bench reports ingested",
            "wall_s": "",
            "tokens_in": "",
            "cost_usd": "",
        }
    )
    return 0
