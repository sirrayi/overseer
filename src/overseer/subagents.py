"""Subagent delegation with strict isolation (plan B9).

The main loop can spawn a subagent for an isolated task. Isolation rules:
- Own session ID, own episodic context, own token budget.
- Inherits the approval gate and path containment (cannot escalate).
- No access to the main agent's working memory or live-learning state
  unless explicitly passed via `context`.
- Halts cleanly when its budget is exceeded.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass, field
from typing import Any

from overseer.agent import AgentLoop
from overseer.errors import BudgetExceeded
from overseer.providers.base import ChatMessage


@dataclass
class SubagentSpec:
    """What the main loop passes to a subagent."""

    task: str
    max_tokens: int = 20_000
    max_iterations: int = 10
    session_id: str = field(default_factory=lambda: f"sub-{uuid.uuid4().hex[:8]}")
    context: dict[str, Any] = field(default_factory=dict)  # explicit pass-through only


@dataclass
class SubagentResult:
    """Outcome of a subagent run."""

    content: str
    session_id: str
    tokens_used: int
    iterations: int
    stopped_reason: str
    halted_by_budget: bool = False


def spawn_subagent(
    loop_factory: Any,
    spec: SubagentSpec,
) -> SubagentResult:
    """Run a subagent in an isolated loop.

    loop_factory: callable that builds an AgentLoop given (session_id,
    max_tokens, max_iterations). The factory is responsible for wiring
    the subagent's own episodic context and approval gate.
    """
    loop: AgentLoop = loop_factory(
        session_id=spec.session_id,
        max_tokens=spec.max_tokens,
        max_iterations=spec.max_iterations,
    )
    try:
        result = loop.run(
            [ChatMessage(role="user", content=spec.task)],
            chain=["default"],
        )
        return SubagentResult(
            content=result.content,
            session_id=spec.session_id,
            tokens_used=result.total_tokens,
            iterations=result.iterations,
            stopped_reason=result.stopped_reason,
            halted_by_budget=result.stopped_reason == "budget",
        )
    except BudgetExceeded as exc:
        return SubagentResult(
            content=f"subagent halted: budget exceeded ({exc})",
            session_id=spec.session_id,
            tokens_used=0,
            iterations=0,
            stopped_reason="budget",
            halted_by_budget=True,
        )
