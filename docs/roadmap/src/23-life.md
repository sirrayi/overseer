## 21. Overseer Life: How the Harness Carries the Dashboard

Overseer Life (plan: `docs/research/2026-10-05-digital-life.md`) is the digital life dashboard on top of the harness. Its later phases depend directly on pillars in this roadmap; building them in the harness first means Life inherits them instead of reinventing them.

### 21.1 Dependency map

| Life phase (from its plan) | Needs from this roadmap |
|---|---|
| Phase 1: foundation, browser v1, connector SDK | Embedding SDK (Y4), iOS gating (Z4), policy compiler for connector permissions (P2), egress proxy patterns (S1) |
| Phase 2: AI accounts hub | Global spend and cache rollups (K9, B2) feeding the module; multi-key model layer (L4) for "best account right now" routing |
| Phase 3: socials hub | Durable scheduled workflows for counts and follower scans with spend caps (A2, A5, R10) |
| Phase 4: browser v2 (agent sidebar) | Origin-scoped grants (P4), taint through browser content (S3), computer-use consent (U2) |
| Phase 5: subscriptions and money | Money-class policy (P4, P6), approval workflows with compensation (A4), effect journal (R7) |
| Phase 6: automations, agents, computer use | Workflow engine (A), persistent specialist agents (O7), supervisor (R4), computer-use live validation and consent (U1, U2) |
| Phase 7: iPhone | Mobile approvals (X4), inbox (A7), sync of the event log (Life's own sync design) |
| Phase 8: identity and security module | Sensitive-surface permissions (P6), audit trail (S4), red-team programme (S9) |

### 21.2 Workstreams

#### F1 — Engine APIs for Life

**What.** The SDK surface (Y4) that Life's UniFFI layer binds to: start and attach runs, stream events, answer approvals, list workflows and instances, query rollups.

#### F2 — Specialist agents on persistent-agent infrastructure

**What.** Life's accountant, security officer, social manager and subscription canceller are persistent agents (O7) with their own memory scopes and permission profiles, defined as plugins (Y1).

#### F3 — Life automations as workflows

**What.** Life's "triggers → actions with approvals" (its `life-auto` crate in the plan) is the workflow engine (A), not a second engine.

#### F4 — One permissions model

**What.** Life's Permissions page (scoped, expiring, revocable grants; money, security and deletion always ask outside full automation) is P4 and P12's model rendered natively.

#### F5 — Telemetry into the AI accounts module

**What.** Overseer's own usage (B2) appears in Life alongside the provider usage readers already built in Phase 0, so the owner sees harness spend and plan quotas in one place.

::: note
Life's own open owner items (X live spike, bank and email providers, Apple Developer enrollment) are listed in Chapter 26; they gate Life phases, not harness waves.
:::
