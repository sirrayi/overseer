## 10. Token and Cache Efficiency

### 10.1 The bar

Agentic coding is an input-token business: sessions run about 25:1 input to output typically and up to about 150:1, and 60–66% of session cost is cache reads of re-sent context **[src: playbook thesis 3]**. Cache hit rate is the single most important infrastructure metric, with 90–95% in-session as the target. The economics are dominated by stable-prefix discipline, append-only history, deterministic serialization and tool-result budgets, not by cleverer prompts.

Provider mechanics that this chapter depends on, from the 2026-09-29 refresh **[doc]**:

- **Anthropic:** up to 4 breakpoints and a 20-block lookback; a minimum cacheable prefix of 1,024 to 4,096 tokens depending on the model, below which caching silently does not happen; a 5-minute TTL refreshed free on a hit; writes at 1.25× and reads at 0.1× of the input price; a 1-hour TTL at 2× write; a top-level automatic caching option.
- **OpenAI:** automatic caching above 1,024 tokens, hits rounded to 128; `prompt_cache_key` matters for routing on models before GPT-5.6 and only separates accounting from 5.6; explicit breakpoints from 5.6.
- **DeepSeek:** on-disk caching by default, best effort.
- **opencode Zen:** best effort, pool-routed, `x-opencode-session` as the sticky key.

These details change. Every workstream below that depends on one re-verifies it against current documentation in its decision record.

### 10.2 Where we stand

See Chapter 3.4. The cache discipline is the strongest part of the engine: frozen segment order, prefix built once per agent, three Anthropic breakpoints, byte-stable specs, telemetry with an alert below 90%. The spend gate is careful. What has never happened is a live run at real prices: the gate and the review-share cap have only seen a $0 model.

### 10.3 Workstreams

#### K1 — The fourth Anthropic breakpoint

**What.** Use the fourth breakpoint to protect the stable middle of the conversation.

**Design.** After a compaction, the compaction summary and the turns before the recency tail are stable until the next compaction. A breakpoint at the end of that stable region means that view edits in the recent part (eviction stubs, notice aging) only invalidate the part after it. Placement: tools tail, system tail, end of the stable region (new), rolling last block (existing). When there is no stable region yet (before the first compaction), the fourth breakpoint moves to the last block before the newest tool exchange, giving a two-step rolling pattern that tolerates one evicted result without a full miss.

::: gate
On recorded long sessions replayed against the provider's usage accounting (live, budgeted, with owner approval for the spend), the fourth breakpoint raises cache read share or holds it equal while view edits are active. Never lowers it.
:::

#### K2 — Time-to-live policy

**What.** Long runs wait: backoff, approvals, long builds. A 5-minute cache dies during a 10-minute build.

**Design.** A deterministic TTL choice per breakpoint. The expected gap before the next request is estimated from what the run is about to do (a long-running tool, a backoff, a pending approval); when the expected gap exceeds 5 minutes and the prefix is large, the stable breakpoints use the 1-hour TTL. The cost model is explicit: 1-hour write costs 2× versus 1.25×, so it pays when it avoids at least one rewrite. The provider's ordering constraints on mixed TTLs are honoured and fixture-tested.

#### K3 — Cache keepalive

**What.** Refresh a large cached prefix during a wait instead of paying to rebuild it **[src: Devin cache keepalive]**.

**Design.** While a run is in `waiting` and its cached prefix is larger than a threshold, issue a minimal request (smallest allowed output) just before the TTL would expire. A keepalive costs about 0.1× of the prefix; a rebuild costs 1.25× (or 2× for 1-hour), so keepalives pay for about a dozen 5-minute refreshes before letting it expire is cheaper. The engine computes the break-even per wait and stops refreshing beyond it. Keepalives are ledgered with their own purpose. They are out-of-band: the response is discarded and never enters the event log or the model view. No keepalive is sent while the run is in `waiting{throttled}` or `waiting{rate_budget}`, because it would spend the very capacity the run is waiting for.

::: gate
Simulated waits of 3, 10, 30 and 120 minutes choose keep, keep, keep-or-1h, expire respectively under the cost model; live spot check on one priced session with owner approval.
:::

#### K4 — Accurate token counts without a heavy tokenizer

**What.** `tokens.rs` estimates with characters per token per family. Compaction timing, eviction budgets and the spend gate all depend on it.

**Design.** Self-calibration instead of shipping tokenizers: after every call, the provider reports actual input tokens; the engine compares with its estimate for the same request and updates a per-model correction factor (exponentially weighted, clamped). Within a few calls the estimate tracks the real tokenizer closely without adding a dependency. For OpenAI models where a tokenizer crate is small and exact, it can be added behind a feature if Z1's budget allows.

::: gate
After 10 calls, estimate error below 3% on recorded sessions for every supported family.
:::

#### K5 — Adaptive compaction threshold

**What.** `compact_at` is a fixed fraction per profile. The best moment depends on cache state and cost.

**Design.** Compaction resets the cache, so it is cheapest right after a cache expiry (a wait just ended) and most expensive right after a cache write. The planner compacts early when a cache miss is happening anyway and context is above a lower threshold, and otherwise waits until the hard threshold. Deterministic, logged with its reason.

#### K6 — Cost-aware routing inside a run

**What.** Use cheaper models where they do just as well, deterministically.

**Design.** Rules, not a learned router (refusal in Chapter 2): aux calls on the small model (have); subagent tiers by mode (have); plus per-task-class tier hints recorded in outcome memory (M5) when the rig or the owner's history shows a lighter tier succeeds for that class. The lead can always override.

#### K7 — Effort that comes back down

**What.** Each stuck trip raises effort one notch and it never decays.

**Design.** Decay one notch after N successful steps without a stuck signal (default 10), never below the profile default. Logged.

#### K8 — Output discipline

**What.** Output tokens are the most expensive per token. Agents narrate.

**Design.** A concise-output contract in the static prompt (measured on the rig), `max_tokens` sized per call type (tool-heavy turns need less than final reports), and the existing reflection caps.

#### K9 — One view of spend and cache across everything

**What.** `overseer stats` reads one session. The owner needs the whole picture.

**Design.** A global rollup (B2) over every session's ledger: spend by day, model, project, purpose (main, review, retry, keepalive, subagent tier); cache hit rate by model and profile; cost per run and per milestone. Feeds the supervisor's global budgets (R10) and Overseer Life's AI accounts module (F5).

#### K10 — Cache-miss forensics

**What.** When cache hit rate drops, find out why automatically.

**Design.** Each request records a prefix fingerprint per segment (system segments, tools, stable region, breakpoint positions). When cache read tokens fall unexpectedly between consecutive requests, the engine diffs fingerprints and records which segment changed, as a `CacheMiss{cause}` diagnostic. The existing hygiene lint catches volatility in tests; this catches it in production.

#### K11 — Provider-native context features

**What.** Providers now offer server-side context editing and compaction features. They might beat ours, or break our invariants.

**Design.** Evaluate each against invariants 1, 6 and 8 and on the rig. Adopt only those that keep the event log authoritative and the view deterministic (for example, a server-side tool-result clearing that mirrors our own eviction and saves bytes on the wire). The compaction seam removed as dead code stays removed until one passes.

#### K12 — The first priced live run

**What.** The spend gate, review-share cap and cost accounting have never run against real prices.

**Design.** One deliberate, owner-approved, budget-capped run on a priced model (Anthropic, since its caching is explicit), on a fixed task set, verifying ledger rows against the provider's own usage dashboard and the gate's clamping behaviour at the cap. Single session, local, with the owner's spend approval (standing rule).

::: gate
Ledger totals match the provider's reported usage to within rounding; the gate refuses at the cap with the expected reason; the review stays under its share.
:::

### 10.4 What could break

::: risk
**Price and policy changes at providers.** A TTL price change or a breakpoint rule change silently makes our policies wrong. Mitigation: provider mechanics live in profile data with a verified-on date, and K10's forensics plus the cache alert catch regressions quickly.
:::

::: risk
**Self-calibration drifts on unusual content.** Code-heavy and prose-heavy requests tokenize differently. Mitigation: separate correction factors per content mix bucket, clamped, with the raw heuristic as the floor.
:::

::: risk
**Keepalive spends money for nothing.** Mitigation: the explicit break-even model, a per-run keepalive budget, and ledger visibility.
:::

### 10.5 Metrics

| Metric | Target |
|---|---|
| In-session cache hit rate, Anthropic | ≥ 92% |
| In-session cache hit rate, OpenAI-compatible | ≥ 85% |
| Resident startup tokens | ≤ 2,000 |
| Cost per solved task | Down ≥ 30% from the W0 baseline at equal pass rate |
| Token estimate error after calibration | < 3% |
