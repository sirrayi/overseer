## 9. Tool Efficiency

### 9.1 The bar

Tools are where an agent spends most of its turns and most of its tokens. The evidence on what works:

- Deferred tool loading cut tokens by 85% on Anthropic's example workload and raised MCP accuracy substantially **[vendor]**; top-k tool retrieval halved prompt tokens and tripled selection accuracy in RAG-MCP **[src]**; accuracy falls past roughly 30–40 tools **[src]**.
- Code-as-action beats JSON tool calls by up to 20% success with about 30% fewer turns across 17 models **[src: CodeAct]**, and programmatic tool calling reports large input-token cuts on heavy catalogs **[vendor]**.
- Anchored (hashline) edits claim large output-token cuts **[vendor: omp, −61%]**, unverified independently.

Overseer is already near the top here: deferral through `tools`, capability-free QuickJS `run_code`, byte-stable specs, one gated pipeline for every call. The remaining work is reach, precision, speed and proof.

### 9.2 Where we stand

See Chapter 3.5.

### 9.3 Workstreams

#### T1 — Hashline edits, behind a measured gate

**What.** Address edit targets by line plus a short content hash instead of reproducing old text, rejecting stale anchors before applying.

**Design.** `read` optionally returns lines prefixed with a short hash (`17:a3f|    let x = …`); `edit` accepts `anchor: "17:a3f"` ranges as an alternative to `old_string`. The harness checks the anchor's hash against the file's current line; a mismatch refuses with the current content of that region (read-before-edit, invariant 5, enforced at line granularity). Prototype behind an `EditFormat` switch, as the pillar refresh planned.

::: gate
Adopt as default only if the rig shows lower output tokens per solved task with no pass-rate loss, paired, on at least two model families. Otherwise it stays an option.
:::

#### T2 — `run_code` reach

**What.** `run_code` cannot call `computer` or `memory` today (deferred on live computer-use verification and memory v2 adoption).

**Design.** Allow `computer` sub-calls once U1 (live validation) passes, so GUI batches run as one script with intermediate observations never entering context. Allow `memory` search and get (read operations) immediately; memory writes stay native so the per-turn write cap and write bars apply at the top level.

#### T3 — A persistent QuickJS context

**What.** Let a script keep state across `run_code` calls within one run (parsed data, helper functions), deferred on demonstrated need.

**Design.** Opt-in `session: true` keeps one context alive per run with the same caps, reset on resume and on any error. Gate: the rig shows multi-call pipelines that benefit.

#### T4 — Native deferred loading on Anthropic

**What.** Use Anthropic's native tool-search and deferred loading for Anthropic sessions, so deferred tools get native calls instead of going through `tools op=call`.

**Design.** Behind a profile flag; the advertised spec array stays byte-stable because deferred definitions expand in the conversation, not the cached prefix **[doc]**. Gate: measured accuracy parity or better on the rig (as already recorded in the tool economy deferred list).

#### T5 — Project-level MCP configuration

**What.** `<repo>/.overseer/mcp.json` alongside the user file, so a project can declare its servers.

**Design.** Project servers default to `trust: ask` regardless of what the file says until the owner approves the file once (by content hash), because a cloned repository is untrusted input. Approval is stored per hash; any change re-asks.

#### T6 — Diagnostics beyond cargo

**What.** `diagnostics` is cargo-only. Make it language-aware.

**Design.** A checker table detected per project: `cargo check`, `tsc --noEmit`, `pyright`/`mypy`, `go vet`, `eslint`, plus an optional LSP client mode for languages where a server is installed, all in the sandbox with the bash environment allowlist (the existing rule). Output normalized to `file:line:col severity message` and budgeted like any result.

#### T7 — Availability that follows reality

**What.** Optional tools are detected once per registry. Installing `ast-grep` mid-session does nothing until a new session.

**Design.** Re-probe on explicit request (`tools op=refresh`) and at resume. A re-probe that changes availability is applied at the next resume or handoff only, never mid-window, so the spec array stays byte-stable within a context.

#### T8 — Parallel read-only tool calls

**What.** Models often emit several independent read-only calls in one turn. Run them concurrently.

**Design.** Calls classified `Read` with no ordering dependency (different paths, pure searches) dispatch on a small thread pool (default 4), results returned in call order. Writes, bash and anything above `Read` stay sequential. The control boundary semantics (interrupts and steering at launch boundaries) are preserved by treating the parallel batch as one launch boundary.

::: gate
Wall-time reduction on turns with multiple reads, zero ordering-dependent test failures, identical event content to sequential execution apart from timestamps.
:::

#### T9 — Shaped results

**What.** Tool output designed for the model, not for a terminal.

**Design.** Per tool: compact headers (path, line range, match counts), consistent truncation markers that say how to get more, TOON for uniform arrays (have), and for `bash`, structured exit status and a separated stderr tail. Each change is A/B tested on the rig, because "obviously better" output formats sometimes are not.

#### T10 — Web fetch and search

**What.** Overseer has no web tool; bash network is denied. Research journeys (Chapter 1, journey B) need the web.

**Design.** A deferred `web` tool with `fetch` (URL to readable text, size-capped) and `search` (through a configured provider), routed through the egress proxy (S1) with domain policy, classified ExternalComms for anything but GET, and always latching the untrusted taint. Depends on S1.

#### T11 — Tool latency budgets

**What.** Measure and bound how long tools take, because over a long run tool time dominates wall time.

**Design.** Per-call durations already exist in events; add per-tool percentile telemetry (B2), slow-call warnings, and budget checks in the rig (p95 per tool must not regress between waves).

#### T12 — Tool descriptions as tested artifacts

**What.** Tool names and descriptions have non-trivial effects on accuracy **[doc: Anthropic, "Writing effective tools for agents"]**.

**Design.** Every spec change runs a rig A/B; the startup-token test (have) pins size; a description changelog records why each change was made.

### 9.4 What could break

::: risk
**Parallel reads race with writes from the same turn.** Mitigation: only pure reads run in parallel, and a turn containing any write dispatches everything sequentially.
:::

::: risk
**Project MCP files become a supply-chain attack.** A malicious repository declares a server that runs arbitrary code. Mitigation: hash-pinned approval before any project server spawns, servers run with the environment allowlist, and `trust: read` is never honoured from a project file without explicit owner approval.
:::

### 9.5 Metrics

| Metric | Target |
|---|---|
| Resident tool spec size | Target ≤ 6,000 characters (the test currently pins 6,136 plus 5% headroom), with every new tool deferred unless it is hot |
| Output tokens per solved task | Down ≥ 15% if T1 is adopted |
| Wall time per turn with multiple reads | Down ≥ 30% with T8 |
