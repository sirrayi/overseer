## Executive Summary

### The one-paragraph version

Overseer already gets the hard, invisible things right: an immutable hash-chained event log, engine-enforced budgets, deterministic compaction as a view, byte-stable prompt prefixes, a taint-tracked permission gate, a memory system that learns, and a subagent system whose budgets compose. Most harnesses never get those foundations right. What Overseer lacks is everything that turns a correct engine into a tireless one: a supervisor that resurrects dead runs, retry and failover that outlive a provider outage, durable intent that survives a week of compactions, a policy layer that can express "ask once per task" for a hundred hours, an orchestration layer with dependencies and persistent agents, and a workflow engine for automations that run for months. This roadmap builds those layers on the existing spine, in eleven waves gated by evidence, and it ends with a harness that is measurably better than every alternative on the axes that matter: completion on long tasks, cost per solved task, cache hit rate, unattended survival, and the size of the blast radius when something goes wrong.

### Where we stand, pillar by pillar

The score is distance to the target defined in each chapter, not to today's competitors. 100 means "the target in this roadmap is met and proven"; 0 means nothing exists. The scores are judgment over a code audit, not measurements, and they are honest rather than flattering.

| Pillar | Score | What exists | What is missing |
|---|---|---|---|
| Memory | 70 | Two git-versioned stores, five layers, BM25F + activation ranking, recall, prospective reminders, learning review, threat scan, pending queue | Session history search, applied learned skills, dream pass, outcome memory, run journals, mounted repos |
| Cache discipline | 75 | Frozen segment order, prefix built once per agent, three Anthropic breakpoints, byte-stable specs, cache telemetry with an alert below 90% | Fourth breakpoint, 1-hour TTL, keepalive, cache-aware compaction timing |
| Token efficiency | 65 | Spend gate, TOON re-encoding, spill to file, read dedup, image caps, small-model aux tier, effort ladder | Real tokenizer, cross-turn result dedup, digest aging, cost-aware routing, effort decay |
| Tool layer | 65 | 18 core tools, deferred catalog behind `tools`, QuickJS `run_code`, MCP with trust lanes | Composition reach, hashline edits, native deferral, project MCP config, LSP diagnostics |
| Security | 65 | Rule-of-Two latches, sandboxed bash, credential broker sentinels, hardened web surface, injection corpus | Egress proxy, consent modes, scoped grants, signed builds, long-run threat model |
| Context management | 55 | Deterministic compaction, keep-last-5 eviction, image caps, provenance wrapping | Token-aware eviction, retrieval-scoped context, notice aging, plan pinning, context planner |
| User experience | 50 | Full-screen TUI, inline mode, plain REPL, hardened localhost web UI, rewind and diff overlays | Long-run steering, inbox frontend, ACP/IDE bridge, mobile approvals, run timeline |
| Model layer | 50 | Anthropic, OpenAI-compatible, Responses, Gemini, opencode adapters; profiles; effort map | Failover ladders, retry, capability probes, deprecation handling, local models |
| Evaluation | 45 | Rig with paired statistics, 40+5 tasks, SWE-bench/Terminal-Bench/τ² adapters, control scaffold | No live claims yet, no long-horizon or chaos suites, eval hold in force |
| Computer use | 45 | Lean cua-driver backend, marks, modal gate, post-action diffs, model-sized images | Never run against a live desktop, consent modes, progress guard, settle waits |
| Orchestration | 40 | Task modes and tiers, worktrees, budget carving, verify, consult, control tree | DAG, artifacts, persistent agents, teams, merge queue, depth cap |
| Extensibility | 40 | Skills, MCP, hooks, microagents, modes | Plugin model, versioning, project MCP, user modes, trust tiers |
| Distribution | 35 | cargo-dist scaffold, lean dependencies, ~5.5 MB binary | Signed builds, installers, self-update, iOS gating |
| Reliability | 30 | Durable log with torn-tail repair, resumable agents, subagent reaping, timeouts on most calls | Supervisor, auto-resume, retry/backoff, failover, wall-clock budgets, durable intent, away policy |
| Observability | 30 | Per-call ledger rows, cache stats, `overseer stats`, run summary line | Cross-session rollups, traces, timeline, alerts, export |
| Rules and policy | 25 | `~/.overseer/rules`, hooks, microagents, six modes, autonomy ladder | AGENTS.md loader, scoped grants, deny rules, project scope, policy compiler, management UI |
| Workflows | 10 | Single-shot gateway triggers with triage and an EV gate | Everything: durable workflows, chains, conditions, retries, compensation, schedules |
| **Overall** | **~45** | A correct engine | A tireless one |

### The twelve commitments

These are what "crush every other harness" means in practice. Each one is measurable, and Chapter 25 says how.

1. **A run never dies of a transient.** Rate limits, network drops, provider outages, crashes, reboots and laptop sleep pause a run; they do not end it. Only budget exhaustion, an owner decision, or a genuine dead end ends a run, and every end has a recorded reason.
2. **A hundred hours is routine.** A run can go for over a hundred hours of wall-clock time without human intervention, with cost, progress and health visible the whole time, and resume from any point after any failure.
3. **Intent outlives context.** What the agent is trying to do, what it has done and what comes next live in a durable journal, not in the context window. A run resumed after a week knows exactly where it stands.
4. **Every token earns its place.** In-session cache hit rate stays at or above 92% on Anthropic models, resident startup stays under 2,000 tokens, and cost per solved task is published and beats every competitor measured on the same model.
5. **Orchestration is a graph, not a call.** Subagents have dependencies, shared artifacts, persistent identities, and a merge queue; parallel writers never corrupt shared state.
6. **Policy is code, with a single compiler.** AGENTS.md, rules, grants, modes, hooks and memory-derived conventions compile into one ordered instruction and permission set, with provenance for every line and a user-visible explanation for every decision.
7. **Automations are durable workflows.** A trigger starts a versioned workflow whose every step is journaled, retried, compensated on failure, and resumable across upgrades.
8. **Memory learns outcomes, not just facts.** The harness remembers what worked, what failed, and why, per project, and uses it to choose approaches.
9. **Verification is mandatory and independent.** Nothing a run produces is called done without an executable check or a fresh-context verifier, and "unknown" is never a pass.
10. **The blast radius is engineered.** Every side effect is classified, gated, journaled, idempotent where possible and reversible where possible; secrets never enter context; untrusted content can never trigger exfiltration silently, except where the owner has explicitly chosen full automation.
11. **Everything is observable and explainable.** Any decision the harness made, from a permission verdict to a compaction cut to a retry, can be traced to its cause from the event log.
12. **Leanness is a gated property.** Binary size, memory use, startup time and resident tokens have budgets that the build enforces. Capability is added without bloat.

### The waves at a glance

Chapter 23 has the full dependency graph. The short form:

| Wave | Theme | Unlocks |
|---|---|---|
| W0 | Land what exists: integrate the fix branches, merge #54 and #55, close the current milestone | A clean base |
| W1 | Survival: retry, backoff, failover, wall-clock budgets, timeout holes, disk guards | Runs stop dying of transients |
| W2 | The supervisor: heartbeats, liveness, auto-resume, re-adoption after reboot | Runs survive crashes and reboots |
| W3 | Durable intent: run journal, plan artifact, progress ledger, resume briefings | Runs survive a week of compactions |
| W4 | Policy compiler: AGENTS.md loader, scoped grants, deny rules, consent modes, away policy | Long runs can be safely unattended |
| W5 | Context and token engine: token-aware eviction, dedup, aging, tokenizer, cache keepalive, 1-hour TTL | Cost per solved task drops sharply |
| W6 | Orchestration graph: DAG, artifacts, merge queue, depth cap, persistent agents | Real parallel work |
| W7 | Workflow engine: durable workflows on the gateway, schedules, compensation, inbox | Automations that run for months |
| W8 | Memory v3 H2/H3 and outcome memory | The harness gets better with use |
| W9 | Proof: long-horizon and chaos evaluation suites, published harness-contribution claims | "Best" becomes a measured fact |
| W10 | Reach: ACP/IDE bridge, mobile approvals, signed distribution, Overseer Life integration | Everywhere the owner is |

Computer use, security, observability and extensibility workstreams run alongside the waves at the points their dependencies allow; Chapter 23 places each one.

### The ten things most likely to go wrong

Chapter 22 has the full atlas. These are the ones that would hurt most.

| # | Risk | Mitigation in this plan |
|---|---|---|
| 1 | An auto-resumed run repeats a side effect it already performed (double send, double purchase, double push) | Side-effect journal with intent-before-act records and idempotency keys (R7); resume refuses to replay an act whose outcome is unknown without verification |
| 2 | A long unattended run drifts from its goal while looking busy | Durable intent journal (R5), progress ledger with stall detection (R9), periodic fresh-context goal audits (O9) |
| 3 | Cost runs away over a hundred hours | Rate budgets ($ per hour and per day) with pause-not-kill breakers (R10), forecasting (B4), per-wave cost alerts |
| 4 | A provider changes or retires a model mid-run | Capability probes and failover ladders with recorded equivalence (L3, L4) |
| 5 | Prompt injection in content read on hour 60 steers the run | Taint latches persist across resume (have), workflow steps inherit taint (A6), full-auto mode remains an explicit owner choice with its own audit trail (S4) |
| 6 | An upgrade of the Overseer binary breaks an in-flight run | Event schema versioning and upgrade-safe resume (R13); the supervisor drains or pins the old binary |
| 7 | Event logs and spill files fill the disk over a week | Snapshots plus archival (R11), spill garbage collection, disk preflight and pause thresholds |
| 8 | Parallel writers produce conflicting changes | Merge queue with rebase-and-verify (O6), serialized integration, never parallel writers on shared state |
| 9 | Complexity erodes the leanness that made Overseer fast | Budgets enforced in the build (Z1), every subsystem must earn its tokens and bytes, measured on the eval rig |
| 10 | The plan is too big to finish | Waves are independently valuable, each closes on its own gates, and W1 through W3 alone remove most failure classes |
