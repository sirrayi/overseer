"""Docker-gated benchmark adapters (playbook Ch.8 §7.1 Tier 1).

All three need a container runtime:
- swe_bench       SWE-bench Verified via inspect_evals (per-repo images)
- terminal_bench  Terminal-Bench 2.x via Harbor (official harness)
- swe_rebench     Nebius SWE-rebench monthly split via Harbor Hub dataset

Each adapter checks `docker info` up front and fails with guidance rather
than half-running. Runners wrap the external harness's CLI and harvest
its results into RunRecords.
"""

from __future__ import annotations

from . import BenchmarkUnavailable, has_docker

GUIDANCE = {
    "swe_bench": (
        "SWE-bench Verified via inspect_evals needs Docker + inspect-ai:\n"
        "  docker: orb start (OrbStack installed)\n"
        "  runner: uv tool install inspect-ai && uvx --from inspect-evals[swe_bench] "
        "inspect eval inspect_evals/swe_bench_verified_mini --model openai-api/fleet/<model>"
    ),
    "terminal_bench": (
        "Terminal-Bench 2.x via Harbor needs Docker + harbor:\n"
        "  docker: orb start\n"
        "  runner: uv tool install harbor && harbor run -d terminal-bench@2.0 "
        "-a <installed-agent-or-custom-adapter> --model <provider>/<model>"
    ),
    "swe_rebench": (
        "SWE-rebench monthly split via Harbor Hub needs Docker + harbor:\n"
        "  docker: orb start\n"
        "  runner: harbor run -d swe-rebench/swe-rebench-leaderboard@<YYYY_MM> "
        "-a <agent> --model <provider>/<model>"
    ),
}


class DockerGatedAdapter:
    """Base for adapters whose execution is container-bound. Concrete
    `run()` lands with the harness wrappers; availability + task metadata
    are honest now."""

    name = "docker-gated"
    datasets: dict = None  # populated by concrete adapters

    def requirements(self) -> list[str]:
        return ["docker"]

    def available(self) -> tuple[bool, str]:
        if not has_docker():
            return False, "docker daemon unreachable — install/start OrbStack"
        return True, "ok"

    def check_or_raise(self):
        ok, why = self.available()
        if not ok:
            raise BenchmarkUnavailable(f"{self.name}: {why}\n{GUIDANCE[self.name]}")


class SweBenchAdapter(DockerGatedAdapter):
    name = "swe_bench"


class TerminalBenchAdapter(DockerGatedAdapter):
    name = "terminal_bench"


class SweRebenchAdapter(DockerGatedAdapter):
    name = "swe_rebench"
