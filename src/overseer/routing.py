"""Task routing: complexity, privacy, and tier selection (plan B8).

The Router classifies tasks and model calls into tiers and picks the
provider chain accordingly. Trivial tasks use cheap models; hard tasks
escalate; privacy-sensitive content is forced to local/user-approved
models only.
"""

from __future__ import annotations

import re

# --- routing tiers (plan B8) -------------------------------------------------
TIER_LOCAL = 0  # simple classification, summarization, formatting
TIER_MID = 1  # routine coding, repo navigation, standard edits
TIER_FRONTIER = 2  # complex planning, hard debugging, architecture, security
TIER_VISION = 3  # screenshots, UI inspection (only when explicitly needed)

TIER_NAMES = {
    TIER_LOCAL: "local/cheap",
    TIER_MID: "mid",
    TIER_FRONTIER: "frontier",
    TIER_VISION: "vision",
}

# --- complexity heuristics ----------------------------------------------------
_TRIVIAL_RE = re.compile(
    r"\b(hello|hi|thanks|ok|yes|no|format|summarize|rename|typo|simple|quick|short)\b",
    re.IGNORECASE,
)
_HARD_RE = re.compile(
    r"\b(debug|bug|root cause|architecture|design|refactor|optimize|security|"
    r"vulnerability|complex|race condition|latency|performance|plan)\b",
    re.IGNORECASE,
)
_VISION_RE = re.compile(
    r"\b(screenshot|image|ui|visual|look at|inspect.*(screen|image)|design review)\b",
    re.IGNORECASE,
)

# --- privacy heuristics -------------------------------------------------------
_PRIVACY_RE = re.compile(
    r"\b(password|api[ _-]?key|secret|token|credential|private key|pem|"
    r"passport|national insurance|nhs|bank|card number|ssn|medical|health record|"
    r"vpn|auth)\b",
    re.IGNORECASE,
)

# Provider chain names for each tier. The CLI wires real provider names
# from config; these defaults are the fallback shape.
DEFAULT_CHAINS: dict[int, list[str]] = {
    TIER_LOCAL: ["local"],
    TIER_MID: ["mid"],
    TIER_FRONTIER: ["frontier"],
    TIER_VISION: ["vision"],
}

# Power modes adjust the escalation ceiling (plan B8).
MODE_CEILING: dict[str, int] = {
    "eco": TIER_MID,  # eco never escalates to frontier
    "balanced": TIER_FRONTIER,
    "performance": TIER_VISION,
}


class Router:
    """Classifies tasks and picks provider chains."""

    def __init__(
        self,
        chains: dict[int, list[str]] | None = None,
        power_mode: str = "balanced",
        privacy_chains: dict[int, list[str]] | None = None,
    ) -> None:
        self.chains = chains or dict(DEFAULT_CHAINS)
        self.power_mode = power_mode
        # Privacy-forced chains: sensitive content uses ONLY these.
        self.privacy_chains = privacy_chains or {
            TIER_LOCAL: ["local"],
            TIER_MID: ["local"],
        }

    # -- classification ------------------------------------------------------

    def complexity(self, text: str) -> int:
        """Estimate task complexity (heuristic, cheap)."""
        if not text:
            return TIER_LOCAL
        if _VISION_RE.search(text) and "screenshot" in text.lower():
            return TIER_VISION
        if _HARD_RE.search(text):
            return TIER_FRONTIER
        if _TRIVIAL_RE.search(text):
            return TIER_LOCAL
        return TIER_MID

    def is_sensitive(self, text: str) -> bool:
        """Privacy classification: does the content look sensitive?"""
        return bool(_PRIVACY_RE.search(text))

    # -- routing -------------------------------------------------------------

    def route(self, text: str) -> tuple[int, list[str], bool]:
        """Return (tier, chain, is_sensitive)."""
        tier = self.complexity(text)
        sensitive = self.is_sensitive(text)
        ceiling = MODE_CEILING.get(self.power_mode, TIER_FRONTIER)
        if tier > ceiling:
            tier = ceiling  # eco never escalates to frontier
        if sensitive:
            # Privacy routing: force local/user-approved chains only.
            chain = self.privacy_chains.get(tier, self.privacy_chains[TIER_LOCAL])
            return tier, chain, True
        chain = self.chains.get(tier, self.chains[TIER_MID])
        return tier, chain, False

    def chain_for(self, text: str) -> list[str]:
        """Convenience: just the chain."""
        _, chain, _ = self.route(text)
        return chain

    def describe(self, text: str) -> str:
        """One-line routing decision for telemetry."""
        tier, chain, sensitive = self.route(text)
        return (
            f"tier={TIER_NAMES.get(tier, tier)} chain={','.join(chain)} "
            f"privacy={'sensitive' if sensitive else 'normal'}"
        )
