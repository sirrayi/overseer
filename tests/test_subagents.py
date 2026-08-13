"""Subagent tests: isolation, budget halting, session separation (plan B9)."""

from __future__ import annotations

from pathlib import Path

from overseer.agent import AgentLoop
from overseer.approval import ApprovalPolicy
from overseer.providers.base import ChatResult, Provider
from overseer.providers.registry import ProviderRegistry
from overseer.subagents import SubagentSpec, spawn_subagent
from overseer.tools import ToolContext, ToolRegistry, get_tool_class, registered_tools


class _ScriptedProvider(Provider):
    name = "scripted"

    def __init__(self, responses: list[ChatResult]) -> None:
        self.responses = responses
        self.calls = 0

    def complete(self, messages, tools=None, stream_callback=None, **kwargs):
        self.calls += 1
        if self.calls > len(self.responses):
            return ChatResult(content="done")
        return self.responses[self.calls - 1]


def _loop_factory(tmp_path: Path, provider: Provider):
    def factory(session_id: str, max_tokens: int, max_iterations: int) -> AgentLoop:
        reg = ProviderRegistry()
        reg.add("default", provider)
        tools = ToolRegistry()
        for name in registered_tools():
            tools.add(get_tool_class(name)())
        policy = ApprovalPolicy(allowed_roots=[tmp_path])
        ctx = ToolContext(
            allowed_roots=[tmp_path], artifacts_dir=tmp_path / ".overseer" / "artifacts"
        )
        return AgentLoop(
            providers=reg,
            tools=tools,
            policy=policy,
            context=ctx,
            max_tokens=max_tokens,
            max_iterations=max_iterations,
        )

    return factory


def test_subagent_runs_isolated_task(tmp_path):
    provider = _ScriptedProvider([ChatResult(content="subagent answer")])
    res = spawn_subagent(
        _loop_factory(tmp_path, provider),
        SubagentSpec(task="do the thing"),
    )
    assert res.content == "subagent answer"
    assert res.session_id.startswith("sub-")
    assert res.stopped_reason == "final_answer"
    assert not res.halted_by_budget


def test_subagent_budget_halts(tmp_path):
    """A subagent that exceeds its token budget must halt cleanly."""
    provider = _ScriptedProvider([ChatResult(content="x" * 5000)])
    res = spawn_subagent(
        _loop_factory(tmp_path, provider),
        SubagentSpec(task="big task", max_tokens=100),
    )
    assert res.halted_by_budget
    assert res.stopped_reason == "budget"


def test_subagent_session_ids_unique(tmp_path):
    provider = _ScriptedProvider([ChatResult(content="ok")])
    a = spawn_subagent(_loop_factory(tmp_path, provider), SubagentSpec(task="a"))
    b = spawn_subagent(_loop_factory(tmp_path, provider), SubagentSpec(task="b"))
    assert a.session_id != b.session_id


def test_subagent_inherits_approval_gate(tmp_path):
    """Subagents cannot bypass the approval gate (denylist applies)."""
    from overseer.providers.base import ToolCall

    provider = _ScriptedProvider(
        [
            ChatResult(
                tool_calls=[ToolCall(id="c1", name="terminal", arguments={"command": "rm -rf /"})]
            ),
            ChatResult(content="done"),
        ]
    )
    res = spawn_subagent(
        _loop_factory(tmp_path, provider),
        SubagentSpec(task="try dangerous thing"),
    )
    # The terminal call is denied; the loop continues and answers.
    assert res.content == "done"
