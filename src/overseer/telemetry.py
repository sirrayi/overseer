"""Telemetry: cost tracking, budget guard, efficiency logging (plan B8)."""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import UTC, date, datetime
from pathlib import Path
from typing import Any

from overseer.errors import BudgetExceeded

# Cost per 1M tokens (USD), by tier. Conservative defaults; overridable.
COST_PER_MILLION: dict[str, float] = {
    "local": 0.2,
    "mid": 1.0,
    "frontier": 10.0,
    "vision": 10.0,
}


def _estimate_tokens(text: str) -> int:
    return max(1, len(text) // 4)


@dataclass
class CostEntry:
    """One model call's cost."""

    tier: str
    tokens: int
    cost: float
    model: str = ""
    ts: str = field(default_factory=lambda: datetime.now(UTC).isoformat())

    def to_dict(self) -> dict[str, Any]:
        return {
            "tier": self.tier,
            "tokens": self.tokens,
            "cost": self.cost,
            "model": self.model,
            "ts": self.ts,
        }


class Telemetry:
    """Running token/cost totals + budget guard.

    Tracks the current session and the current day. Warnings fire at
    warn_ratio (default 0.8 = 80%); BudgetExceeded halts the loop when a
    hard limit is crossed.
    """

    def __init__(
        self,
        log_dir: str | Path | None = None,
        max_cost_per_session: float = 5.0,
        max_cost_per_day: float = 20.0,
        max_tokens_per_session: int = 500_000,
        warn_ratio: float = 0.8,
    ) -> None:
        self.log_dir = Path(log_dir) if log_dir else None
        self.max_cost_per_session = max_cost_per_session
        self.max_cost_per_day = max_cost_per_day
        self.max_tokens_per_session = max_tokens_per_session
        self.warn_ratio = warn_ratio
        self.session_entries: list[CostEntry] = []
        self.day_entries: list[CostEntry] = []
        self.warnings_emitted: set[str] = set()

    # -- recording -----------------------------------------------------------

    def record(
        self,
        tokens: int,
        tier: str = "mid",
        model: str = "",
        day_key: str | None = None,
    ) -> CostEntry:
        """Record a model call's token/cost. Raises BudgetExceeded if a
        hard limit is crossed."""
        rate = COST_PER_MILLION.get(tier, COST_PER_MILLION["mid"])
        cost = tokens / 1_000_000 * rate
        entry = CostEntry(tier=tier, tokens=tokens, cost=cost, model=model)
        self.session_entries.append(entry)
        self.day_entries.append(entry)
        self._check_limits(day_key or date.today().isoformat())
        return entry

    # -- totals --------------------------------------------------------------

    def session_tokens(self) -> int:
        return sum(e.tokens for e in self.session_entries)

    def session_cost(self) -> float:
        return sum(e.cost for e in self.session_entries)

    def day_cost(self, day_key: str | None = None) -> float:
        key = day_key or date.today().isoformat()
        # day_entries carry ISO timestamps; match by the day prefix.
        return sum(e.cost for e in self.day_entries if e.ts.startswith(key))

    # -- budget guard --------------------------------------------------------

    def _check_limits(self, day_key: str) -> None:
        tokens = self.session_tokens()
        cost = self.session_cost()
        day = self.day_cost(day_key)

        # Warnings at warn_ratio.
        if (
            tokens >= self.max_tokens_per_session * self.warn_ratio
            and "tokens" not in self.warnings_emitted
        ):
            self.warnings_emitted.add("tokens")
        if (
            cost >= self.max_cost_per_session * self.warn_ratio
            and "cost" not in self.warnings_emitted
        ):
            self.warnings_emitted.add("cost")
        if day >= self.max_cost_per_day * self.warn_ratio and "day" not in self.warnings_emitted:
            self.warnings_emitted.add("day")

        # Hard halts.
        if tokens > self.max_tokens_per_session:
            raise BudgetExceeded(f"token budget exceeded: {tokens} > {self.max_tokens_per_session}")
        if cost > self.max_cost_per_session:
            raise BudgetExceeded(
                f"cost budget exceeded: ${cost:.4f} > ${self.max_cost_per_session}"
            )
        if day > self.max_cost_per_day:
            raise BudgetExceeded(
                f"daily cost budget exceeded: ${day:.4f} > ${self.max_cost_per_day}"
            )

    def near_limits(self) -> list[str]:
        """Human-readable warnings for approaching limits."""
        out: list[str] = []
        tokens = self.session_tokens()
        cost = self.session_cost()
        if tokens >= self.max_tokens_per_session * self.warn_ratio:
            out.append(f"approaching token budget: {tokens}/{self.max_tokens_per_session}")
        if cost >= self.max_cost_per_session * self.warn_ratio:
            out.append(f"approaching session cost budget: ${cost:.4f}")
        if self.day_cost() >= self.max_cost_per_day * self.warn_ratio:
            out.append(f"approaching daily cost budget: ${self.day_cost():.4f}")
        return out

    # -- persistence ----------------------------------------------------------

    def save(self, session_id: str) -> Path | None:
        """Append today's entries to the daily log (if log_dir set)."""
        if self.log_dir is None:
            return None
        self.log_dir.mkdir(parents=True, exist_ok=True)
        path = self.log_dir / f"telemetry-{date.today().isoformat()}.jsonl"
        with path.open("a", encoding="utf-8") as fh:
            for e in self.day_entries:
                fh.write(json.dumps(e.to_dict()) + "\n")
        self.day_entries = []
        return path

    def summary(self) -> dict[str, Any]:
        """Efficiency telemetry for `overseer cost`."""
        return {
            "session_tokens": self.session_tokens(),
            "session_cost": round(self.session_cost(), 4),
            "day_cost": round(self.day_cost(), 4),
            "calls": len(self.session_entries),
            "limits": {
                "max_cost_per_session": self.max_cost_per_session,
                "max_cost_per_day": self.max_cost_per_day,
                "max_tokens_per_session": self.max_tokens_per_session,
            },
            "warnings": self.near_limits(),
        }
