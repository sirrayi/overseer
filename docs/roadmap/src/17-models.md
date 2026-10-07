## 15. The Model Layer

### 15.1 Where we stand

Adapters for Anthropic, OpenAI-compatible endpoints, the Responses API and Gemini, plus opencode routing (Chapter 3). A profile table (`profile.rs`) holds per-model context sizes, prices, effort maps and compaction fractions; rows today include `claude-fable-5`, `claude-opus-4-8`, `claude-sonnet-5`, `claude-sonnet-4-5`, `claude-haiku-4-5`, `deepseek-v4.1-flash` and `muse-spark-1.3-contributor`, with an `unknown` fallback. Tiers derive light and heavy models from the family. A small-model aux tier escalates to main. Errors other than one malformed retry end the run.

### 15.2 Workstreams

#### L1 — Error classification in every adapter

**What.** R1's taxonomy, implemented per adapter with fixtures captured from real error responses (status, headers, body). Shared with R1; listed here because each adapter owns its mapping table.

#### L2 — A living profile registry

**What.** Profiles drift: prices change, models launch and retire, context limits grow.

**Design.** Profile data moves to a data file embedded at build time with a `verified_on` date per row and per field group (prices, limits, cache rules); a CLI command shows stale rows; an owner-approved refresh updates them from provider documentation (manually researched, not scraped at runtime). Unknown models fall back to conservative defaults with a visible warning.

#### L3 — Capability probes

**What.** Know what a model can do before relying on it.

**Design.** A cheap probe per model on first use (cached per binary version): tool calling, parallel tool calls, image input and limits (feeding computer use's image sizing), reasoning output and its round-trip rule (C4), cache reporting fields, maximum output. Results recorded in a local capability cache and in the session's `SessionStart`.

#### L4 — Failover ladders

**What.** When the primary model is unavailable, continue on an equivalent rather than stopping.

**Design.** Per profile, an ordered ladder (`opus → sonnet → another family's flagship`), with rules: failover only on `Overloaded`, sustained `Throttled` or `Degraded` (R1); read-only subtasks fail over freely; the lead's own turns fail over only to models of equal or higher tier unless the owner allows a downgrade; every failover is an event and a journal note; the run returns to the primary after a recovery probe succeeds. Cross-family failover requires reasoning blocks from the other family to be dropped from the view (they cannot round-trip across providers); the planner handles it at a handoff boundary (C9), never mid-exchange. Multi-key support in one session (for example, two Anthropic accounts) is part of this, which also unblocks cross-provider verification (O11).

::: gate
Fault injection marks the primary overloaded for 30 simulated minutes: the run continues on the ladder, returns to the primary afterwards, and the journal records both transitions. No reasoning block ever crosses providers.
:::

#### L5 — Local and open-weight models

**What.** A first-class local path (an OpenAI-compatible local server) for privacy-sensitive work and offline operation, with profiles for common local models, conservative tool-calling assumptions verified by L3, and zero-cost ledger rows.

#### L6 — Native provider features

**What.** Adopt provider features that fit the invariants: Anthropic's tool search (T4), extended cache TTL (K2), server-side tool-result clearing if it matches our eviction (K11), structured outputs for verifiers and the review protocol where supported. Each behind a profile flag with a rig A/B.

#### L7 — Routing by task class

**What.** Deterministic routing hints by task class (K6), recorded per decision so `overseer why` can explain the tier choice. No learned router.

#### L8 — Deprecation watch

**What.** A model scheduled for retirement mid-run is a reliability problem. Profiles carry known retirement dates when providers publish them; the supervisor warns before starting a long run on a model retiring within its expected duration, and the failover ladder handles the switch if it happens.
