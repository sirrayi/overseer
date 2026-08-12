"""Context compiler: token-budgeted, tiered prompt assembly (plan B6).

Compiles the smallest, highest-signal context under a strict token budget.
Tier 0 (pinned) is always included; Tier 1 corrections/preferences are
never evicted; higher tiers are evicted first when the budget is tight.
Stable content (system prompt, guardrails) sits at the front for
provider-level prompt caching; dynamic content goes at the end.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

# --- tiers (plan B6) ----------------------------------------------------------
TIER_PINNED = 0  # task goal, guardrails, phase, output format — always included
TIER_ADAPTATION = 1  # relevant corrections + preferences (never evicted)
TIER_KNOWLEDGE = 2  # facts, project notes, active skills
TIER_ENVIRONMENT = 3  # repo map, file snippets, failure cards, git diff
TIER_OPTIONAL = 4  # similar past sessions, extra examples

TIER_NAMES = {
    TIER_PINNED: "pinned",
    TIER_ADAPTATION: "adaptation",
    TIER_KNOWLEDGE: "knowledge",
    TIER_ENVIRONMENT: "environment",
    TIER_OPTIONAL: "optional",
}


# Conservative token estimate (plan: len // 4 baseline).
def _estimate_tokens(text: str) -> int:
    return max(1, len(text) // 4)


@dataclass
class ContextItem:
    """One piece of context with a token cost and expected value."""

    tier: int
    content: str
    value: float = 0.5  # expected value score 0..1
    label: str = ""

    @property
    def tokens(self) -> int:
        return _estimate_tokens(self.content)

    def snippet(self, limit: int = 200) -> str:
        """Progressive disclosure: title + short snippet, not the full text."""
        if len(self.content) <= limit:
            return self.content
        return self.content[:limit] + f"… (+{len(self.content) - limit} chars in full note)"


class ContextCompiler:
    """Assembles a budgeted prompt from tiered context items."""

    def __init__(
        self,
        budget: int = 8000,
        reserve_ratio: float = 0.3,
        max_items: int = 40,
    ) -> None:
        self.budget = budget
        # Reserve tokens for the model's response and next action.
        self.reserve = int(budget * reserve_ratio)
        self.max_items = max_items

    def compile(
        self,
        items: list[ContextItem],
        system_prompt: str = "",
    ) -> list[dict[str, str]]:
        """Assemble a budgeted message list.

        Returns [{"role": "system", "content": ...}, ...] with the stable
        prefix first (caching) and dynamic content last. Tier 0 and Tier 1
        are never evicted; higher tiers are evicted first.
        """
        available = self.budget - self.reserve
        if available < 0:
            available = self.budget // 2  # pathological budget: still work

        # Stable prefix: system prompt + pinned items (cache-friendly).
        stable_parts: list[str] = []
        if system_prompt:
            stable_parts.append(system_prompt)

        # Sort: pinned first, then by value desc within tier.
        ordered = sorted(items, key=lambda it: (it.tier, -it.value))

        pinned: list[ContextItem] = []
        adaptation: list[ContextItem] = []
        rest: list[ContextItem] = []
        for it in ordered:
            if it.tier == TIER_PINNED:
                pinned.append(it)
            elif it.tier == TIER_ADAPTATION:
                adaptation.append(it)
            else:
                rest.append(it)

        # Tier 0 + Tier 1 always included (budget permitting for T1).
        included: list[ContextItem] = []
        used = _estimate_tokens("\n".join(stable_parts))
        for it in pinned:
            if used + it.tokens <= available or not included:
                included.append(it)
                used += it.tokens
        for it in adaptation:
            if used + it.tokens <= available:
                included.append(it)
                used += it.tokens
            # Tier 1 is never evicted once included; if budget is truly
            # exhausted, the pinned block still carries the guardrails.

        # Higher tiers: evict lowest-value first when over budget.
        for it in rest:
            if len(included) >= self.max_items:
                break
            # Progressive disclosure: long knowledge/environment items are
            # injected as snippets, not full text (plan B6).
            display = it
            if it.tier >= TIER_KNOWLEDGE and it.tokens > 100:
                display = ContextItem(
                    tier=it.tier,
                    content=it.snippet(),
                    value=it.value,
                    label=it.label,
                )
            if used + display.tokens <= available:
                included.append(display)
                used += display.tokens
            else:
                # Try to evict a lower-value higher-tier item to make room.
                evicted = self._evict(included, display)
                if evicted is not None:
                    used -= evicted.tokens
                    included.append(display)
                    used += display.tokens

        # Assemble: stable prefix (system + pinned) then the rest.
        messages: list[dict[str, str]] = []
        body_parts = [it.content for it in included]
        if body_parts:
            messages.append({"role": "system", "content": "\n\n".join(stable_parts + body_parts)})
        elif stable_parts:
            messages.append({"role": "system", "content": "\n\n".join(stable_parts)})
        return messages

    def _evict(self, included: list[ContextItem], incoming: ContextItem) -> ContextItem | None:
        """Find a lower-value, higher-tier item to evict for `incoming`."""
        candidates = [
            it for it in included if it.tier > TIER_ADAPTATION and it.value < incoming.value
        ]
        if not candidates:
            return None
        # Evict the lowest-value candidate (ties: highest tier).
        return min(candidates, key=lambda it: (it.value, -it.tier))

    def telemetry(self, items: list[ContextItem], messages: list[dict[str, str]]) -> dict[str, Any]:
        """Context cost telemetry (plan B6: context cost is logged)."""
        total = sum(_estimate_tokens(m["content"]) for m in messages)
        return {
            "items_in": len(items),
            "items_out": len(messages),
            "tokens_used": total,
            "budget": self.budget,
            "reserve": self.reserve,
            "utilization": round(total / self.budget, 2) if self.budget else 0,
        }
