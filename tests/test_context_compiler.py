"""Context compiler tests: budget, tier eviction, stable prefix, disclosure (B6)."""

from __future__ import annotations

from overseer.context_compiler import (
    TIER_ADAPTATION,
    TIER_ENVIRONMENT,
    TIER_KNOWLEDGE,
    TIER_OPTIONAL,
    TIER_PINNED,
    ContextCompiler,
    ContextItem,
)


def _item(tier: int, text: str, value: float = 0.5) -> ContextItem:
    return ContextItem(tier=tier, content=text, value=value)


def test_pinned_always_included():
    cc = ContextCompiler(budget=1000)
    msgs = cc.compile(
        [
            _item(TIER_PINNED, "TASK: fix the bug", value=1.0),
            _item(TIER_OPTIONAL, "x" * 5000, value=0.1),  # huge, low value
        ],
        system_prompt="You are Overseer.",
    )
    assert msgs
    assert "TASK: fix the bug" in msgs[0]["content"]
    assert "You are Overseer." in msgs[0]["content"]


def test_tier4_evicted_before_tier1():
    cc = ContextCompiler(budget=800)
    msgs = cc.compile(
        [
            _item(TIER_PINNED, "TASK: go", value=1.0),
            _item(TIER_ADAPTATION, "CORRECTION: use pytest", value=0.9),
            _item(TIER_OPTIONAL, "x" * 3000, value=0.1),  # over budget
        ],
        system_prompt="sys",
    )
    content = msgs[0]["content"]
    assert "CORRECTION: use pytest" in content  # Tier 1 never evicted
    assert "x" * 3000 not in content  # Tier 4 evicted


def test_eviction_order_tier4_before_tier3_before_tier2():
    cc = ContextCompiler(budget=600)
    msgs = cc.compile(
        [
            _item(TIER_PINNED, "TASK: go", value=1.0),
            _item(TIER_KNOWLEDGE, "FACT: fastapi", value=0.8),
            _item(TIER_ENVIRONMENT, "REPO: src/", value=0.7),
            _item(TIER_OPTIONAL, "x" * 2000, value=0.2),
        ],
        system_prompt="sys",
    )
    content = msgs[0]["content"]
    assert "FACT: fastapi" in content  # Tier 2 kept
    assert "x" * 2000 not in content  # Tier 4 evicted


def test_stable_prefix_first_for_caching():
    cc = ContextCompiler(budget=5000)
    msgs = cc.compile(
        [
            _item(TIER_PINNED, "GUARDRAIL: never self-modify", value=1.0),
            _item(TIER_ENVIRONMENT, "LATEST ERROR: crash", value=0.9),
        ],
        system_prompt="You are Overseer. The engine.",
    )
    content = msgs[0]["content"]
    # Stable content (system + pinned) must come before dynamic content.
    assert content.index("You are Overseer.") < content.index("LATEST ERROR")


def test_progressive_disclosure_snippet():
    cc = ContextCompiler(budget=5000)
    long_note = "FACT: " + "z" * 1000
    msgs = cc.compile([_item(TIER_KNOWLEDGE, long_note, value=0.8)], system_prompt="sys")
    content = msgs[0]["content"]
    assert "…" in content  # truncated with a disclosure marker
    assert len(content) < len(long_note) + 100


def test_budget_reserve_respected():
    cc = ContextCompiler(budget=1000, reserve_ratio=0.5)
    msgs = cc.compile([_item(TIER_OPTIONAL, "x" * 900, value=0.9)], system_prompt="sys")
    # 500 reserved -> only ~500 available; the 900-char item must be evicted.
    assert "x" * 900 not in msgs[0]["content"]


def test_telemetry_reports_usage():
    cc = ContextCompiler(budget=200)
    items = [_item(TIER_PINNED, "TASK: go", value=1.0)]
    msgs = cc.compile(items, system_prompt="sys")
    t = cc.telemetry(items, msgs)
    assert t["tokens_used"] > 0
    assert t["budget"] == 200
    assert 0 < t["utilization"] <= 1.0
