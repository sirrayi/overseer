"""Harbor custom agent wrapping `overseer exec` inside task containers.

Harbor's `-a` accepts `module.path:ClassName`; the adapter puts this dir
on PYTHONPATH and passes `overseer_agent:OverseerAgent`.

Per trial: setup() uploads the host-built release binary into the
container (must be same arch — Devin cloud workers are x86_64 Linux,
matching TB2's x86_64 images); run() executes `overseer exec --json`
in the task's workdir with opencode as the provider, then downloads the
session dir so the trial logs carry the real event trajectory.

Env the worker must export for the run:
  OVERSEER_BIN   host path of the release binary (uploaded per trial)
  OPENCODE_API_KEY  forwarded into the container (org secret on cloud)
  OVERSEER_MODEL  e.g. muse-spark-1.3-contributor (falls back to
                  harbor's -m value)
"""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

CONTAINER_BIN = "/opt/overseer/overseer"

# DEFERRED(harbor-agent): uploads the host binary — same-arch only
# (x86_64 Linux workers match TB2 images). Cross-arch needs a musl
# build or an in-container toolchain install.


class OverseerAgent(BaseAgent):
    @staticmethod
    def name() -> str:
        return "overseer"

    def version(self) -> str:
        return os.environ.get("OVERSEER_VERSION", "dev")

    async def setup(self, environment: BaseEnvironment) -> None:
        host_bin = os.environ.get("OVERSEER_BIN", "")
        if not host_bin or not Path(host_bin).exists():
            raise RuntimeError(
                "overseer harbor agent: OVERSEER_BIN must point at the "
                "host-built release binary"
            )
        await environment.exec("mkdir -p /opt/overseer")
        await environment.upload_file(host_bin, CONTAINER_BIN)
        await environment.exec(f"chmod +x {CONTAINER_BIN}")

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        res = await environment.exec("pwd")
        cwd = (res.stdout or "/").strip() or "/"

        fd, tmp = tempfile.mkstemp(prefix="overseer-prompt-", suffix=".txt")
        with os.fdopen(fd, "w") as f:
            f.write(instruction)
        await environment.upload_file(tmp, "/opt/overseer/prompt.txt")

        model = os.environ.get("OVERSEER_MODEL") or self.model_name or ""
        env = {
            "OPENCODE_API_KEY": os.environ.get("OPENCODE_API_KEY", ""),
            "HOME": "/root",
        }
        await environment.exec(
            f"{CONTAINER_BIN} exec --json --provider opencode "
            f"--model {model} --cwd {cwd} --full-access --no-memory --max-steps 40 "
            f"--session /opt/overseer/session - < /opt/overseer/prompt.txt",
            env=env,
            timeout_sec=int(os.environ.get("OVERSEER_TIMEOUT", "1500")),
        )
        # trajectory: events.jsonl lands in the trial logs
        try:
            await environment.download_dir(
                "/opt/overseer/session", self.logs_dir / "session"
            )
        except Exception:
            pass
