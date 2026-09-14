"""Control scaffold — the null hypothesis (playbook Ch.8 §4, Ch.12 §0.7).

A ~minimal ReAct loop on the same provider+model, shaped after
mini-SWE-agent (~100 lines): bash-only actions, regex action extraction,
no tools registry, no budgets beyond a step cap, no compaction.
Every Overseer feature must beat THIS on identical tasks before it earns
its complexity.

Planned: lands with the Phase 0 exit run alongside tasks/ graders.
"""
