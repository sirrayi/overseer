"""Task spec v2 (playbook Ch.8 §7.3.2).

Every task file is JSON:

  {
    "id": "fix-bug",                 # required, unique within a benchmark
    "version": 1,                    # required, bumped on any content change
    "tags": ["smoke", "edit"],       # optional
    "difficulty": "easy",            # optional: easy|med|hard
    "instruction": "...",            # required — what the agent sees
    "setup": "shell script | null",  # prepares the fresh workspace
    "env": {"image_digest": "..."},  # container pin; null/absent = local sh
    "grader": {                      # deterministic preferred
      "type": "bash_assert",         #   script must exit 0 to pass
      "script": "python3 -c ..."
    },
    "oracle": {                      # REQUIRED — a reference solution that
      "script": "..."                # must pass the grader; a task with no
    },                               # oracle (or a failing one) is broken,
    "limits": {                      # not a hard task (0% pass@k ⇒ broken).
      "max_steps": 15,
      "max_cost_usd": 0.5,
      "wall_s": 900
    }
  }
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

GRADER_TYPES = {"bash_assert"}  # judge graders are declared, not implemented
DIFFICULTIES = {"easy", "med", "hard"}


class SpecError(ValueError):
    pass


@dataclass
class TaskSpec:
    id: str
    version: int
    instruction: str
    setup: str | None
    grader: dict
    oracle: dict
    limits: dict
    tags: list[str] = field(default_factory=list)
    difficulty: str | None = None
    env_digest: str = "local-sh"
    source_path: str | None = None

    def grader_script(self) -> str:
        if self.grader.get("type") != "bash_assert":
            raise SpecError(
                f"{self.id}: grader type {self.grader.get('type')!r} has no runner"
            )
        return self.grader["script"]

    def oracle_script(self) -> str:
        return self.oracle["script"]


def _req(spec: dict, key: str, ctx: str):
    if key not in spec or spec[key] is None:
        raise SpecError(f"{ctx}: missing required field {key!r}")
    return spec[key]


def parse(raw: dict, ctx: str = "task") -> TaskSpec:
    tid = _req(raw, "id", ctx)
    version = _req(raw, "version", ctx)
    if not isinstance(version, int) or version < 1:
        raise SpecError(f"{tid}: version must be a positive int")
    instruction = _req(raw, "instruction", ctx)
    grader = _req(raw, "grader", ctx)
    if (
        not isinstance(grader, dict)
        or grader.get("type") not in GRADER_TYPES
        or not grader.get("script")
    ):
        raise SpecError(
            f"{tid}: grader must be {{type: one of {sorted(GRADER_TYPES)}, script: str}}; "
            "judge graders are declared in the spec but have no runner yet"
        )
    oracle = _req(raw, "oracle", ctx)
    if not isinstance(oracle, dict) or not oracle.get("script"):
        raise SpecError(f"{tid}: oracle.script is required (0% pass@k ⇒ broken task)")
    difficulty = raw.get("difficulty")
    if difficulty is not None and difficulty not in DIFFICULTIES:
        raise SpecError(f"{tid}: difficulty must be one of {sorted(DIFFICULTIES)}")
    limits = raw.get("limits") or {}
    if not isinstance(limits, dict):
        raise SpecError(f"{tid}: limits must be an object")
    env = raw.get("env") or {}
    return TaskSpec(
        id=tid,
        version=version,
        instruction=instruction,
        setup=raw.get("setup"),
        grader=grader,
        oracle=oracle,
        limits=limits,
        tags=list(raw.get("tags") or []),
        difficulty=difficulty,
        env_digest=env.get("image_digest") or "local-sh",
    )


def load_dir(path: Path | str) -> list[TaskSpec]:
    """Load every *.json under `path`; ids must be unique. Sorted by id —
    matrix order is then fully deterministic."""
    root = Path(path)
    specs = []
    for f in sorted(root.glob("*.json")):
        spec = parse(json.loads(f.read_text()), ctx=f.name)
        spec.source_path = str(f)
        specs.append(spec)
    ids = [s.id for s in specs]
    if len(ids) != len(set(ids)):
        dupes = sorted({i for i in ids if ids.count(i) > 1})
        raise SpecError(f"duplicate task ids: {dupes}")
    return specs
