"""Agent loop: the deterministic orchestrator state machine (plan B1).

Loop: build messages -> call model (fallback chain) -> parse tool calls ->
dispatch tools (approval-gated) -> append results -> repeat.

Stop conditions:
- Model returns final text (no tool calls).
- Max iterations reached.
- Token budget exceeded.
- Approval denied (fed back to the model as an error result; repeated
  denials are capped by max_iterations).

All tool output is redacted and truncated before reaching the model.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from overseer.approval import ApprovalPolicy
from overseer.errors import ApprovalDenied, BudgetExceeded, ProviderError, ToolError
from overseer.providers.base import ChatMessage, ToolCall
from overseer.providers.registry import ProviderRegistry
from overseer.tools.base import ToolContext, ToolResult
from overseer.tools.registry import ToolRegistry

DEFAULT_MAX_ITERATIONS = 20
DEFAULT_MAX_TOKENS = 200_000


@dataclass
class AgentResult:
    """Outcome of an agent run."""

    content: str
    iterations: int
    total_tokens: int
    tool_calls_made: int = 0
    approvals_denied: int = 0
    stopped_reason: str = "final_answer"  # final_answer | max_iterations | budget | error


class AgentLoop:
    """Runs the SENSE->THINK->ACT->REPEAT loop against a provider chain."""

    def __init__(
        self,
        providers: ProviderRegistry,
        tools: ToolRegistry,
        policy: ApprovalPolicy,
        system_prompt: str = "You are Overseer, a careful coding agent.",
        max_iterations: int = DEFAULT_MAX_ITERATIONS,
        max_tokens: int = DEFAULT_MAX_TOKENS,
        context: ToolContext | None = None,
        stream_callback: Callable[[str], None] | None = None,
    ) -> None:
        self.providers = providers
        self.tools = tools
        self.policy = policy
        self.system_prompt = system_prompt
        self.max_iterations = max_iterations
        self.max_tokens = max_tokens
        self.context = context or ToolContext()
        # Wire the approval gate into the tool context (terminal checks it).
        self.context.approver = self._approver
        self.stream_callback = stream_callback

    def _approver(self, tool_name: str, args: dict[str, Any]) -> bool:
        """Approval gate callback wired into the tool context."""
        try:
            return self.policy.approve(tool_name, args)
        except ApprovalDenied:
            return False

    def run(
        self,
        messages: list[ChatMessage],
        chain: list[str],
        tools: list[dict[str, Any]] | None = None,
    ) -> AgentResult:
        """Run the loop until a stop condition. Returns the final result."""
        history: list[ChatMessage] = [ChatMessage(role="system", content=self.system_prompt)]
        history.extend(messages)

        total_tokens = 0
        tool_calls_made = 0
        approvals_denied = 0
        tool_specs = tools if tools is not None else self.tools.specs()

        for iteration in range(self.max_iterations):
            # 1. Call the model (with fallback chain).
            try:
                result, _used = self.providers.complete_with_fallback(
                    chain, history, tools=tool_specs
                )
            except ProviderError as exc:
                return AgentResult(
                    content=f"provider error: {exc}",
                    iterations=iteration + 1,
                    total_tokens=total_tokens,
                    stopped_reason="error",
                )

            total_tokens += result.usage.get("total_tokens", 0)
            if total_tokens > self.max_tokens:
                raise BudgetExceeded(f"token budget exceeded: {total_tokens} > {self.max_tokens}")

            if self.stream_callback and result.content:
                self.stream_callback(result.content)

            # 2. No tool calls -> final answer.
            if not result.has_tool_calls:
                return AgentResult(
                    content=result.content,
                    iterations=iteration + 1,
                    total_tokens=total_tokens,
                    tool_calls_made=tool_calls_made,
                    approvals_denied=approvals_denied,
                    stopped_reason="final_answer",
                )

            # 3. Echo the assistant tool-call turn, then dispatch each call.
            history.append(
                ChatMessage(role="assistant", content=result.content, tool_calls=result.tool_calls)
            )
            for call in result.tool_calls:
                tool_calls_made += 1
                tool_result = self._dispatch(call)
                if tool_result.error and tool_result.error.startswith("APPROVAL_DENIED"):
                    approvals_denied += 1
                history.append(
                    ChatMessage(
                        role="tool",
                        content=tool_result.to_message(),
                        tool_call_id=call.id,
                    )
                )

        return AgentResult(
            content="",
            iterations=self.max_iterations,
            total_tokens=total_tokens,
            tool_calls_made=tool_calls_made,
            approvals_denied=approvals_denied,
            stopped_reason="max_iterations",
        )

    def _dispatch(self, call: ToolCall) -> ToolResult:
        """Dispatch one tool call, routing through the approval gate.

        Tools marked requires_approval go through policy.approve first
        (denylist/allowlist/risky/path policy). Denials become error results
        fed back to the model.
        """
        try:
            tool = self.tools.get(call.name)
        except ToolError as exc:
            return ToolResult(status="error", summary=str(exc), error=str(exc), token_cost=1)
        if tool.requires_approval:
            try:
                self.policy.approve(call.name, call.arguments)
            except ApprovalDenied as exc:
                # Explicit marker so the loop can count denials reliably.
                return ToolResult(
                    status="error",
                    summary=f"APPROVAL_DENIED: {exc}",
                    error=f"APPROVAL_DENIED: {exc}",
                    token_cost=1,
                )
        try:
            return self.tools.dispatch(call.name, call.arguments, context=self.context)
        except ToolError as exc:
            return ToolResult(status="error", summary=str(exc), error=str(exc), token_cost=1)
