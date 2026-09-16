"""Benchmark adapters (playbook Ch.8 §7.3.1): the eval kernel boundary.

An adapter knows how to enumerate a benchmark's tasks and how to run them
through either (a) the local rig matrix — the paired A/B design where the
scaffold is the variable, or (b) an external harness (Inspect AI, Harbor,
τ²) that owns its execution loop and whose results we normalize into
RunRecords for the shared store + stats + report.

    requires() -> list[str]        e.g. ["docker"], ["cli:tau2"]
    available() -> (bool, reason)
    iter_tasks(cfg) -> Iterable[TaskSpec]   local-matrix adapters only
    run(cfg, store) -> list[record]         external-harness adapters

The docker-gated trio (SWE-bench Verified via inspect_evals, Terminal-Bench
2.x and SWE-rebench via Harbor) declare `requires() == ["docker"]` and fail
loudly with install guidance when absent.
"""

from __future__ import annotations

import shutil
import subprocess


def has_docker() -> bool:
    try:
        return (
            subprocess.run(
                ["docker", "info"], capture_output=True, timeout=10
            ).returncode
            == 0
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return False


def has_cli(name: str) -> bool:
    return shutil.which(name) is not None


class BenchmarkUnavailable(RuntimeError):
    pass


from dataclasses import dataclass, field


@dataclass
class ExternalTask:
    """Task identity for external-harness benchmarks — satisfies the shape
    `manifest.build_record` reads (id, version, env_digest, tags,
    difficulty) without a local task file."""

    id: str
    version: int
    env_digest: str
    tags: list = field(default_factory=list)
    difficulty: str | None = None
