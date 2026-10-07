## 16. Observability

### 16.1 Where we stand

Every event is in the log; every provider call has a ledger row with cache fields; `RunEnd` carries a cache delta; `overseer stats` summarizes one session; the TUI shows a quiet run line (`19 steps · 1m 12s · $0.042 · 87% cached`) and a stats panel. There is no cross-session view, no trace structure, no timeline, and no alerting beyond the cache alert.

### 16.2 Workstreams

#### B1 — Traces from events

**What.** Turn the flat event stream into a tree of spans: run → step → model call / tool call → sub-calls (`run_code`, `tools op=call`) → subagents, with durations, tokens, cost and outcomes. Derived from events (no new instrumentation in the hot path beyond timestamps already recorded), so it works on old logs.

#### B2 — Global rollups

**What.** One local index over all sessions, workflow instances and ledgers: spend by day, model, project, purpose and tier; cache hit rate by model; tool latency percentiles; failure classes over time; run outcomes. Rebuildable from the logs (a cache, per the principles). Feeds R10's global budgets, K9's dashboard and F5.

#### B3 — The run timeline

**What.** A visual timeline per run in the web UI (and a compact form in the TUI): steps, waits (by kind), retries, failovers, compactions, approvals, milestones, restarts. The single most useful view for understanding a 100-hour run.

#### B4 — Forecasting

**What.** For a running long run: projected total cost and remaining time from plan progress and recent rates, with confidence bands; warnings when the forecast exceeds budgets (R10).

#### B5 — Alerts

**What.** Owner-configured alert rules over B2's metrics (cache hit rate below threshold, failure class spikes, budget forecasts, stalled runs, failed workflows), delivered through the notifier and channels with deduplication, and through the digest.

#### B6 — Export

**What.** Optional export of traces and metrics in OpenTelemetry format to a local collector, for owners who want their own dashboards. Off by default; never includes tool result bodies or secrets.

#### B7 — Replay debugger

**What.** Step through any session's events and see the exact view the model received at each request (from the context planner's deterministic reconstruction, C7), the verdicts, and the outputs. Because views are deterministic functions of the log, this is exact, not approximate.

#### B8 — Long-run health panel

**What.** For each active run: heartbeat age, state, step rate, spend rate against budget, cache hit rate, last milestone age, in-doubt effects, pending approvals, soak metrics (RSS, descriptors). The cockpit's core (X1).
