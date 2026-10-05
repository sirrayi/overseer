#!/usr/bin/env python3
"""eval/cloud/shard_ids.py — deterministic instance-id slicer.

Prints a comma-separated `--swebench-ids` list for one shard of a
dataset, so cloud workers run disjoint slices of SWE-bench Verified /
SWE-bench-Live without a shared coordinator. Dataset order is the
materialized JSONL order (stable for a fixed dataset+split).

  IDS=$(uv run python ../cloud/shard_ids.py \
        --dataset SWE-bench/SWE-bench_Verified --shard 3 --of 12)
  uv run python run.py --benchmark swe_bench --swebench-ids "$IDS" …
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys


def load_ids(dataset: str, split: str) -> list[str]:
    code = (
        "import json\n"
        "from datasets import load_dataset\n"
        f"ds=load_dataset({json.dumps(dataset)},split={json.dumps(split)})\n"
        "print('\\n'.join(r['instance_id'] for r in ds))\n"
    )
    proc = subprocess.run(
        ["uv", "run", "--no-project", "--with", "datasets", "python", "-c", code],
        capture_output=True,
        text=True,
        timeout=900,
    )
    if proc.returncode != 0:
        raise SystemExit(f"dataset load failed: {proc.stderr[-400:]}")
    return [ln.strip() for ln in proc.stdout.splitlines() if ln.strip()]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--split", default="test")
    ap.add_argument("--shard", type=int, required=True)
    ap.add_argument("--of", type=int, required=True)
    ap.add_argument("--limit", type=int, default=0, help="cap total ids first")
    args = ap.parse_args()

    ids = load_ids(args.dataset, args.split)
    if args.limit:
        ids = ids[: args.limit]
    n = len(ids)
    size = (n + args.of - 1) // args.of
    part = ids[args.shard * size : (args.shard + 1) * size]
    if not part:
        raise SystemExit(f"shard {args.shard}/{args.of} empty ({n} ids)")
    print(",".join(part))
    sys.stderr.write(f"shard {args.shard}/{args.of}: {len(part)}/{n} ids\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
