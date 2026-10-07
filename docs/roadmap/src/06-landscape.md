## 4. The Competitive Bar

"Crush every other harness" is only meaningful against a concrete bar. This chapter sets one per capability, using the harness teardowns in `docs/agent-harness-playbook.pdf` (Chapter 2 of the playbook) and the 2026-09-29 pillar refresh. Competitor details are tagged **[src]**: they come from those earlier research notes, not from fresh measurement, and products move quickly. Workstream E8 re-measures the bar on the eval rig before any public claim.

### 4.1 The best idea from each harness, and our answer

| Harness | Best idea worth taking [src] | Overseer today | How we go past it |
|---|---|---|---|
| Claude Code | ToolSearch deferral; auto-memory index cap (200 lines / 25 KB); fork subagents that inherit the cache; hooks; skills | Deferral via `tools` **[have]**; INDEX cap **[have]**; skills and hooks **[have]** | Cache-inheriting subagent forks (O16); hooks v2 with typed events (P10); resident footprint about a tenth of theirs |
| Codex | App-server JSON-RPC; per-model tuned prompts (~1.5K) | Wire protocol crate exists, thin | ACP adapter plus app-server parity (X3); per-model prompt variants through the policy compiler (P2) |
| Factory Droid | Missions: a feature graph plus validator droids; light/medium/heavy model slots | Tiers **[have]**; no graph | Missions on the task DAG with executable validators per node (O15) |
| Devin | Fusion lead and sidekick; message-forest revert; cache keepalive; cloud handoff | Single lead; fork and rewind **[have]** | Persistent sidekick agents (O7, O8); keepalive (K3); local-first with optional cloud executor (R18) |
| Amp | Handoff instead of compaction; oracle consult | Consult **[have]**; deterministic compaction **[have]** | Handoff as a measured alternative to compaction (C9) |
| OpenCode | One background server, every UI a client; assets embedded | Gateway daemon **[have]**; embedded web assets **[have]** | The supervisor makes the daemon the owner of run liveness (R4); every frontend is a client of it |
| omp | Hashline edits (vendor claims −61% output tokens); stream rules (TTSR); role fallback chains; CDP relay into the real browser | Plain edits | Hashline edits behind a measured gate (T1); stream rules (P8); failover ladders (L4) |
| Hermes | cua-driver computer use; bounded memory files; skill self-improvement; FTS session search; cron plus messaging | Computer use **[have]**; learning review **[have]** | History search (M1), applied skills (M2), dream pass (M3), durable workflows (A) |
| DeepSeek harness | Everything is a plugin; append-only session log with fork and replay; code mode; peer-runtime subagents over ACP | Log, fork, replay **[have]**; code mode **[have]** | Plugin model with trust tiers (Y1); peer agents over ACP (Y4) |
| pi | Smallest resident prompt, four core tools | ~1.76K tokens resident | Stay within 2K resident while carrying far more capability (Z1, K) |
| mini-SWE-agent | A ~100-line bash-only loop scores above 74% on SWE-bench Verified: the capability floor | Frozen control scaffold in the rig **[have]** | Beat the floor per component, measured and published (E1, E4) |
| Synara | Patched cua-driver; foreground versus background consent; screen-lock revocation; progress guard; hard denylist; 25-step validated batches | Lean driver backend **[fix2]** | Consent modes (U2), progress guard (U3), lock revocation (U7); denylist left to owner-configured permissions by decision |
| OpenHands, Goose, Aider, Gemini CLI | Event-stream architecture (OpenHands); MCP-first extensibility (Goose); architect/editor split and repo map (Aider); large context and free tier (Gemini CLI) | Event log, MCP, consult, repo map **[have]** | Beat them on long-horizon survival and cost per solved task, the two axes none of them targets |

### 4.2 The capability bar, axis by axis

For each axis: the best known level, where we are, and the target that counts as crushing it.

| Axis | Best known [src/infer] | Overseer today | Target |
|---|---|---|---|
| Resident startup tokens | pi ~1.1K; Codex ~8.5K; OpenCode ~6.9K; Claude Code 27–33K | ~1.76K | ≤ 2.0K with every pillar in place |
| In-session cache hit rate | Reported ops target 90–95% | Telemetry exists; live rate not yet measured on priced traffic | ≥ 92% on Anthropic, ≥ 85% on OpenAI-compatible, measured on the long-horizon suite |
| Unattended run length | Hours, with manual resume in most CLIs [infer] | Until the first transient error | ≥ 100 hours with zero human intervention on the long-horizon suite |
| Survival of provider outages | Simple retry in some harnesses [infer] | None | Backoff plus failover; no run ends on a transient |
| Crash and reboot survival | Manual resume [infer] | Manual resume | Automatic resume within 60 s of the supervisor restarting |
| Parallel work | Background agents, fork subagents [src] | 4 background readers, isolated writers | DAG with merge queue, persistent agents, depth and fan-out caps |
| Automation | Cron plus messaging (Hermes), scheduled agents [src] | Single-shot triggers | Durable, versioned workflows with compensation |
| Memory | Index files plus topic files, skill self-improvement [src] | v3 learning loop | Outcome memory and measured uplift from a warm store |
| Verification | Tests when present [infer] | Verify mode and gate | Mandatory executable or fresh-context verification for every "done" |
| Security | Permission prompts, some sandboxes [src] | Rule-of-Two latches plus sandbox | Plus egress proxy, scoped grants, full-auto audit trail, signed builds |
| Explainability | Logs [infer] | Event log | `overseer why` on any decision (P11), traces (B1) |
| Leanness | Rust rewrites around 20 MB RSS best case; Codex ~80 MB binary [src] | ~5.5 MB binary | ≤ 15 MB binary with every feature, budgets enforced in the build |

### 4.3 What "crush" means as a claim

A claim of superiority is made only when all of these hold, and Chapter 25 holds the scoreboard:

1. **Same model, same tasks, paired statistics.** Pass rate with a 95% confidence interval from the seeded cluster bootstrap the rig already implements, against each competitor run on the identical model and task set.
2. **Cost per solved task**, not cost per task: total spend divided by tasks solved, with the cache breakdown.
3. **Long-horizon survival**: the fraction of long-horizon suite runs that complete without human intervention, under injected faults (E3).
4. **No regression in leanness**: resident tokens, binary size, RSS and startup within budget.
5. **Published method**: the report card format from the rig, with harness commit, model, seeds and environment digest.

::: risk
**Benchmarks lie quietly.** Vendor-reported numbers fall sharply in independent reruns (the memory benchmark record in the pillar refresh shows LoCoMo claims of 92–96% dropping to 65–85%). Our own claims are only as good as the rig's contamination hygiene and the fairness of competitor configuration. Mitigation: E8 runs competitors with their recommended settings, records those settings in the report, and invites reproduction; E9 handles contamination.
:::

::: risk
**The bar moves while we build.** Every competitor ships monthly. Mitigation: the bar is re-measured at the close of every wave (E5), and targets in this chapter are revised in the roadmap's next edition rather than silently.
:::
