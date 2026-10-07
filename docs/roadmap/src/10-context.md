## 8. Context Management

Context management decides **what is in the window**. Token efficiency (Chapter 10) decides **what it costs**. They overlap, but the questions differ: a cheap window full of the wrong things still fails the task.

### 8.1 The bar

Advertised context is not usable context: effective context runs at a half to a quarter of the sticker on long-context benchmarks, and every tested model degrades as input grows **[src: RULER, NoLiMa, Context Rot]**. Plain observation masking halves cost against a raw agent and matches LLM summarization on SWE-bench Verified **[src: Lindenbauer et al. 2025]**. Critical state belongs at the edges of the prompt. Summaries must never be re-summarized.

Overseer already does the hard part (compaction as a deterministic view, masking of stale results). The gaps are in precision: eviction counts results instead of measuring them, repeated content re-enters verbatim, notices replay forever, and nothing keeps the run's intent at the edge of the window.

### 8.2 Where we stand

See Chapter 3.4: deterministic compaction at 70–83% of context with a two-turn tail; keep-last-5 tool results with pinned `tools` search results; last 2 images; read dedup; spill to file over 30,000 characters.

### 8.3 Target: a context planner

Today, each mechanism acts on its own rule. The target is one deterministic **context planner** that builds each request's view from the log under an explicit token budget, with every inclusion and exclusion explainable. The planner is a pure function of (events, profile, budget) so it stays deterministic and testable, and it keeps the prefix stable so caching survives (Chapter 10).

Priority order inside the budget, highest first:

1. System prefix and tool specs (stable, cached).
2. The resume briefing and pinned journal state (R5), at the recency edge.
3. The recency tail (complete turns, on model-response boundaries).
4. Pinned items: loaded tool schemas, the active plan, files the agent is currently editing.
5. Recent tool results, newest first, until the result budget is spent.
6. Older material, represented by the compaction summary.

### 8.4 Workstreams

#### C1 — Token-aware eviction

**What.** Replace "keep the last 5 tool results" with "keep the newest results that fit a token budget", because five results can be 150 KB or 5 KB.

**Design.** A result budget per profile (default 25% of usable context), filled newest first using the token estimator (K4), with pinned items charged first. Evicted results become the existing stale stub. Results the agent referenced in its latest reasoning or that belong to a file under active edit are pinned for the next turn.

**Cache interaction.** Eviction changes message content, which moves the cache point. To keep it rare, eviction runs in batches (when the budget is exceeded by a margin, evict down to a lower watermark), not one result per turn, so the prefix changes in occasional steps rather than every request.

::: gate
On the rig, token-aware eviction matches or beats keep-last-5 on pass rate with lower input tokens per solved task, paired, on the same model. Cache hit rate does not drop by more than 2 points.
:::

#### C2 — Cross-turn result deduplication

**What.** An identical `grep`, `glob` or `bash` output re-entering the window verbatim is waste. `read` already dedups; generalize it.

**Design.** Each tool result gets a content hash. When a new result's hash matches a result still in view, the new one is replaced by a stub naming the earlier call ("identical to the result of call c41"). When the earlier one has been evicted, the new one is shown in full. Deduplication is per-view, never altering events.

#### C3 — Notice and digest aging

**What.** `MemoryNotice` recalls, `SubagentDone` digests and reminders replay verbatim forever. After a few turns most of them are dead weight.

**Design.** Notices age like tool results: after K turns (default 6) a notice becomes a one-line stub with its id, unless pinned by the planner (for example, a digest whose artifact the current plan item consumes). Graph summaries supersede per-node digests.

#### C4 — Shedding old reasoning

**What.** Invariant 6 says reasoning blocks are opaque and round-trip verbatim. It does not say they must stay in the window forever.

**Design.** Per provider, following each provider's rules: where the API permits dropping reasoning from turns before the latest tool-use exchange (for example, providers that only require the most recent reasoning for continuity), old reasoning blocks are dropped whole from the view. Never edited, never summarized. Where a provider requires full round-trip (signed reasoning tied to tool pairing), the planner keeps them. Each adapter declares its rule with a fixture-tested reference to the provider documentation.

::: risk
Getting a provider's reasoning rule wrong breaks requests or degrades quality silently. Mitigation: per-adapter declarations with documentation references, fixture tests, and an eval-rig A/B before enabling by default on any provider.
:::

#### C5 — Edge placement of critical state

**What.** Put what matters most where models attend best: the start and the end.

**Design.** The stable start is the cached system prefix (unchanged). The end gets the resume briefing (R5), pending approvals (R8), in-doubt effects (R7) and the current plan item, in one compact block, rebuilt each turn from events. The block is small (budgeted at 600 tokens outside resume briefings) so it costs little uncached.

#### C6 — Retrieval-scoped reading

**What.** The model often reads whole files to find one function. Give it precise slices.

**Design.** Extend `read` with symbol and range targets backed by the repo map and tree-sitter where available (`read path=… symbol=AuthAdapter::refresh`), returning the symbol with a few lines of context and a line-range header. `grep` results include enclosing symbol names. `symbol` and `repo_map` stay deferred tools; the slice capability lives in `read` because it is the hot path.

::: gate
On the rig, symbol reads reduce input tokens per solved task without lowering pass rate.
:::

#### C7 — The context planner itself

**What.** The unifying piece described in §8.3, replacing the separate eviction, capping and aging rules with one budgeted planner.

**Design.** A `plan_view(events, profile, budget) -> View` function with an explanation trace (`ViewTrace`: what was included, what was stubbed, why) recorded on demand for `overseer why` (P11). The existing rules become planner policies with identical default behaviour, so the switch is behaviour-preserving before any policy changes.

::: gate
Golden tests: for a corpus of logs, the planner with legacy policies produces byte-identical views to today's code. Only then are new policies enabled one at a time, each with its own rig A/B.
:::

#### C8 — Optional semantic compaction

**What.** Mechanical summaries lose nuance on long refactors. An LLM condensation pass might retain more, at a cost.

**Design.** An opt-in compaction policy that adds a small-model condensation of the evicted span to the mechanical summary, never replacing it and never re-summarizing an earlier condensation (invariant 8 holds: each condensation covers raw events only). Ledgered, budgeted, and measured; it stays off by default unless the rig shows gains on long tasks.

#### C9 — Handoff instead of compaction

**What.** Amp's idea: instead of compacting for the tenth time, start a fresh context seeded deliberately.

**Design.** A `handoff` action (by the agent, or by the planner when compaction count passes a threshold): the run writes a `JournalNext` and a handoff brief, then the engine starts a fresh context window inside the same run and session, whose first user turn is the resume briefing plus the brief. The event log continues; the view resets. This is cleaner than repeated compaction because the new window holds a deliberate brief instead of a mechanical summary of a summary's neighbourhood.

::: gate
On the long-horizon suite, handoff-after-N-compactions versus compaction-only: equal or better completion and lower cost. The threshold is tuned from the data.
:::

#### C10 — Context health telemetry

**What.** Make context visible: per request, the token split between prefix, briefing, tail, results, notices and summary; the number of compactions; eviction counts; dedup hits.

**Design.** Recorded on each `ModelRequest` event as a compact breakdown; shown in the cockpit (X1) and `overseer stats`.

### 8.5 What could break

::: risk
**Over-eager eviction hides the evidence the model needs.** Mitigation: pinning rules for active files and referenced results; the stale stub always says how to get the content back (re-run or `read` the spill); rig A/B before any default change.
:::

::: risk
**View changes destroy cache hit rate.** Every eviction or aging edit changes message bytes. Mitigation: batched watermark eviction, aging at fixed turn counts, and the cache metrics in K10 to catch regressions.
:::

### 8.6 Metrics

| Metric | Target |
|---|---|
| Input tokens per solved task | Down ≥ 25% from the W0 baseline on the rig |
| Pass rate | No regression (non-inferiority at 3 points) |
| Share of window used by stale material at compaction time | ≤ 15% |
