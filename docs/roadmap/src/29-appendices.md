# Appendices

## Appendix A — New Event Kinds

Every new kind follows invariant 21: new fields default, unknown kinds are skippable by older readers, and each kind lands with a golden-log replay test.

| Event | Introduced by | Rehydrated into the model view? |
|---|---|---|
| `RunState { state, reason }` | R14 | No (audit and supervisor) |
| `Failure { class, detail }` (or a `class` field on existing failure events) | R1 | No |
| `Resumed { cause, after_sleep, binary_version }` | R4, R12 | Through the resume briefing only |
| `Snapshot { covers_event, covers_hash, state }` | R11 | No (replay cache) |
| `JournalGoal`, `JournalPlan`, `JournalNote`, `JournalMilestone`, `JournalNext` | R5 | Through the resume briefing and edge block (C5) |
| `EffectIntent`, `EffectOutcome` | R7 | In-doubt notices only |
| `ApprovalRequested`, `ApprovalResolved` | R8 | As pending results and resolution notices |
| `Failover { from, to, reason }` | L4 | Journal note |
| `GraphNode { id, state, task_id }` | O2 | Graph summary notice |
| `ArtifactWritten { name, version, hash, taint }` | O3 | As inputs to consumers |
| `BoardEntry { kind, text, taint }` | O4 | In briefs, capped |
| `MailboxMessage { from, to, kind, reply_to, deadline }` | O7, O17 | Yes, at the loop top |
| `Handoff { brief }` | C9 | Starts the new view |
| `PolicyCompiled { fingerprint, sources, diff }` | P2 | No |
| `ShadowVerdict { would_have, rule, class, taint }` | S4 | No |
| `CacheMiss { cause, segment }` | K10 | No |
| `ContextBreakdown` (field on model requests) | C10 | No |
| Workflow instance events: `InstanceStarted`, `StepScheduled`, `StepStarted`, `StepCompleted`, `StepFailed`, `StepCompensated`, `InstanceCompleted` | A2 | Not part of any agent view |

## Appendix B — Configuration Surface

Reliability keys are listed in §5.5. All configuration flows through the policy compiler (P2) from these files:

| File | Scope | Holds |
|---|---|---|
| `~/.overseer/policy.toml` | User | Rules, grants defaults, consent mode, sensitive-surface permissions, budgets, away policy, retry, power |
| `<repo>/.overseer/policy.toml` | Project (trusted by hash) | Project rules within what the owner delegates |
| `~/.overseer/AGENTS.md`, `<repo>/AGENTS.md`, nested `AGENTS.md` | User, project, directory | Instructions; optional frontmatter for verify and heavy commands, protected paths, never-auto-repeat commands |
| `~/.overseer/mcp.json`, `<repo>/.overseer/mcp.json` | User, project (trusted by hash) | MCP servers |
| `~/.overseer/modes/*.toml` | User | User-defined modes |
| `~/.overseer/agents/<name>/agent.toml` | User | Persistent agents |
| `~/.overseer/workflows/*.toml` | User | Workflow definitions |
| `~/.overseer/plugins/<name>/plugin.toml` | User | Plugins |
| `~/.overseer/rules` | User (legacy) | Migrated into `policy.toml`, then read-only |

## Appendix C — Workstream Index

Every workstream in this roadmap, with its chapter and the wave that carries it.

| ID | Workstream | Chapter | Wave |
|---|---|---|---|
| R1 | Failure taxonomy | 5 | W1 |
| R2 | Provider retry, backoff and resumable streams | 5 | W1 |
| R3 | Close every timeout hole | 5 | W1 |
| R4 | The supervisor | 5 | W2 |
| R5 | Durable intent: the run journal | 5 | W3 |
| R6 | Crash-consistency audit of every durable write | 5 | W2 |
| R7 | The side-effect journal and idempotency | 5 | W2 |
| R8 | Away policy: asking when nobody is there | 5 | W4 |
| R9 | Progress ledger and long-window stall detection | 5 | W3 |
| R10 | Pacing: rate budgets and shared provider limits | 5 | W3 |
| R11 | Resource guards: disk, logs, spills, worktrees | 5 | W1, W3 |
| R12 | Time, sleep, power and networks | 5 | W2 |
| R13 | Upgrade-safe runs and schema evolution | 5 | W2 |
| R14 | Run lifecycle and control surface | 5 | W2 |
| R15 | Fault injection built into the engine | 5 | W1 |
| R16 | Environment drift | 5 | W3 |
| R17 | Process hygiene for very long lives | 5 | W2 |
| R18 | Optional remote executor | 5 | W10 |
| O1 | Depth and fan-out limits | 6 | W6 |
| O2 | The task graph | 6 | W6 |
| O3 | Artifacts | 6 | W6 |
| O4 | The blackboard | 6 | W6 |
| O5 | Detach and attach | 6 | W6 |
| O6 | The merge queue | 6 | W6 |
| O7 | Persistent agents | 6 | W6 |
| O8 | Teams and roles: the lead and sidekick protocol | 6 | W6 |
| O9 | Goal audits | 6 | W3 |
| O10 | Best-of-N with executable selection | 6 | W6 |
| O11 | Cross-provider verification | 6 | W6 |
| O12 | Handoff as an alternative to compaction | 6 | W5 (as C9) |
| O13 | Brief quality | 6 | W6 |
| O14 | Seeing the tree | 6 | W6 |
| O15 | Missions | 6 | W6 |
| O16 | Cache-inheriting forks | 6 | W6 |
| O17 | Messaging with deadlines | 6 | W6 |
| O18 | Scheduling and machine resources | 6 | W6 |
| M1 | History search (`memory op=history`) | 7 | W8 |
| M2 | Applied learned skills | 7 | W8 |
| M3 | The dream pass | 7 | W8 |
| M4 | Mounted memory repositories and sync | 7 | W8 |
| M5 | Outcome memory | 7 | W8 |
| M6 | Journals into memory | 7 | W8 |
| M7 | Cross-project promotion | 7 | W8 |
| M8 | Recurring reminders | 7 | W8 |
| M9 | Memory evaluation | 7 | W8 |
| M10 | Poisoning defences, extended | 7 | W8 |
| M11 | Memory you can see and edit | 7 | W8 |
| M12 | Shared and team memory | 7 | W10 |
| C1 | Token-aware eviction | 8 | W5 |
| C2 | Cross-turn result deduplication | 8 | W5 |
| C3 | Notice and digest aging | 8 | W5 |
| C4 | Shedding old reasoning | 8 | W5 |
| C5 | Edge placement of critical state | 8 | W3 |
| C6 | Retrieval-scoped reading | 8 | W5 |
| C7 | The context planner itself | 8 | W5 |
| C8 | Optional semantic compaction | 8 | W5 |
| C9 | Handoff instead of compaction | 8 | W5 |
| C10 | Context health telemetry | 8 | W5 |
| T1 | Hashline edits, behind a measured gate | 9 | W5 |
| T2 | `run_code` reach | 9 | W5 |
| T3 | A persistent QuickJS context | 9 | W5 |
| T4 | Native deferred loading on Anthropic | 9 | W5 |
| T5 | Project-level MCP configuration | 9 | W5 |
| T6 | Diagnostics beyond cargo | 9 | W5 |
| T7 | Availability that follows reality | 9 | W5 |
| T8 | Parallel read-only tool calls | 9 | W5 |
| T9 | Shaped results | 9 | W5 |
| T10 | Web fetch and search | 9 | W5 |
| T11 | Tool latency budgets | 9 | W5 |
| T12 | Tool descriptions as tested artifacts | 9 | W5 |
| K1 | The fourth Anthropic breakpoint | 10 | W5 |
| K2 | Time-to-live policy | 10 | W5 |
| K3 | Cache keepalive | 10 | W5 |
| K4 | Accurate token counts without a heavy tokenizer | 10 | W5 |
| K5 | Adaptive compaction threshold | 10 | W5 |
| K6 | Cost-aware routing inside a run | 10 | W5 |
| K7 | Effort that comes back down | 10 | W5 |
| K8 | Output discipline | 10 | W5 |
| K9 | One view of spend and cache across everything | 10 | W5 |
| K10 | Cache-miss forensics | 10 | W5 |
| K11 | Provider-native context features | 10 | W5 |
| K12 | The first priced live run | 10 | W0 |
| P1 | The AGENTS.md loader | 11 | W4 |
| P2 | The instruction compiler | 11 | W4 |
| P3 | Rules engine v2 | 11 | W4 |
| P4 | Scoped, expiring, revocable grants | 11 | W4 |
| P5 | Consent modes | 11 | W0, W4 |
| P6 | Dedicated permissions for secrets and sensitive surfaces | 11 | W4 |
| P7 | User-defined modes | 11 | W10 |
| P8 | Stream rules | 11 | W10 |
| P9 | Contextual rules (microagents, version 2) | 11 | W4 |
| P10 | Hooks, version 2 | 11 | W10 |
| P11 | Explain any decision | 11 | W4 |
| P12 | Managing permissions | 11 | W4 |
| P13 | Principles enforced by the build | 11 | W4 |
| P14 | The instruction hierarchy as defence in depth | 11 | W4 |
| P15 | Git safety defaults | 11 | W4 |
| A1 | The workflow definition | 12 | W7 |
| A2 | The durable executor | 12 | W7 |
| A3 | Step kinds | 12 | W7 |
| A4 | Retries, timeouts and compensation | 12 | W7 |
| A5 | Schedules and time | 12 | W7 |
| A6 | Taint through workflows | 12 | W7 |
| A7 | Inbox, version 2 | 12 | W7 |
| A8 | The workflow library | 12 | W7 |
| A9 | Versioning and migration | 12 | W7 |
| A10 | Testing workflows | 12 | W7 |
| A11 | Triggers, version 2 | 12 | W7 |
| A12 | Seeing automations | 12 | W7 |
| S1 | The egress proxy | 13 | W4 (S1a macOS), W10 (S1b Linux) |
| S2 | Finish the credential broker | 13 | W4 |
| S3 | A threat model for long runs and automation | 13 | W2 |
| S4 | The full-automation audit trail | 13 | W4 |
| S5 | Signed builds and provenance | 13 | W10 |
| S6 | The injection corpus, extended | 13 | Every wave |
| S7 | Secrets hygiene | 13 | W0 |
| S8 | Sandbox hardening | 13 | W10 |
| S9 | A standing red-team programme | 13 | Every wave close |
| S10 | The kill switch everywhere | 13 | W2 |
| S11 | Data at rest, retention and backup | 13 | W4 |
| U1 | Live validation on the owner's Mac | 14 | W0 |
| U2 | Consent modes for computer use | 14 | W0, W4 |
| U3 | Progress guard | 14 | W4 |
| U4 | Settle waits | 14 | W4 |
| U5 | Clipboard | 14 | W4 |
| U6 | Overseer's own computer helper | 14 | W10 |
| U7 | Revocation on lock, sleep and session switch | 14 | W4 |
| U8 | The browser | 14 | W10 |
| U9 | Computer-use evaluation | 14 | W9 |
| U10 | GUI scripts through `run_code` | 14 | W5 |
| L1 | Error classification in every adapter | 15 | W1 |
| L2 | A living profile registry | 15 | W1 |
| L3 | Capability probes | 15 | W1 |
| L4 | Failover ladders | 15 | W1 |
| L5 | Local and open-weight models | 15 | W10 |
| L6 | Native provider features | 15 | W5 |
| L7 | Routing by task class | 15 | W5 |
| L8 | Deprecation watch | 15 | W1 |
| B1 | Traces from events | 16 | W3 |
| B2 | Global rollups | 16 | W3 |
| B3 | The run timeline | 16 | W3 |
| B4 | Forecasting | 16 | W3 |
| B5 | Alerts | 16 | W3 |
| B6 | Export | 16 | W9 |
| B7 | Replay debugger | 16 | W9 |
| B8 | Long-run health panel | 16 | W2 |
| E1 | Lift the hold: the first measured baseline | 17 | W0 |
| E2 | The long-horizon suite | 17 | W9 |
| E3 | The chaos suite | 17 | W9 |
| E4 | Cost per solved task, published | 17 | W9 |
| E5 | Regression gates per wave | 17 | Every wave |
| E6 | Memory evaluation | 17 | W9 |
| E7 | Orchestration evaluation | 17 | W9 |
| E8 | Competitors on the same model | 17 | W9 |
| E9 | Contamination hygiene | 17 | W2 (authoring), W9 |
| X1 | The long-run cockpit | 18 | W2, W6 |
| X2 | The inbox frontend | 18 | W7 |
| X3 | ACP and the editor bridge | 18 | W10 |
| X4 | Approvals and status from the phone | 18 | W4 |
| X5 | Notifications that respect attention | 18 | W7 |
| X6 | Steering long runs | 18 | W10 |
| X7 | Web UI parity | 18 | W10 |
| X8 | Onboarding | 18 | W10 |
| X9 | Accessibility | 18 | W10 |
| X10 | Documentation as part of the product | 18 | Every wave |
| X11 | Importing from other harnesses | 18 | W10 |
| Y1 | A plugin model | 19 | W10 |
| Y2 | Versioning and trust tiers | 19 | W10 |
| Y3 | Project-level MCP | 19 | W5 (as T5) |
| Y4 | An embedding SDK and peer agents | 19 | W10 |
| Y5 | The hooks protocol | 19 | W10 |
| Y6 | Sharing | 19 | W10 |
| Z1 | Budgets enforced by the build | 20 | Every wave |
| Z2 | Signed and notarized builds | 20 | W10 |
| Z3 | Installers and safe self-update | 20 | W0 decision, W10 |
| Z4 | iOS gating | 20 | W10 |
| Z5 | Linux parity | 20 | W10 |
| Z6 | Windows plan | 20 | W10 |
| Z7 | Soak and leak testing | 20 | W2 onward |
| Z8 | Move the repository out of iCloud | 20 | W0 |
| Z9 | Licences and attribution | 20 | Every wave |
| F1 | Engine APIs for Life | 21 | W10 |
| F2 | Specialist agents on persistent-agent infrastructure | 21 | W10 |
| F3 | Life automations as workflows | 21 | W10 |
| F4 | One permissions model | 21 | W10 |
| F5 | Telemetry into the AI accounts module | 21 | W10 |

## Appendix D — Glossary

| Term | Meaning |
|---|---|
| Artifact | A typed, versioned, taint-carrying output one agent hands another (O3) |
| Away policy | What happens to an approval request when the owner does not answer in time (R8) |
| Blackboard | A run's shared, append-only scratchpad of short entries (O4) |
| Cache boundary | The point in the prompt above which nothing volatile may appear (invariant 2) |
| Compaction | Replacing old events in the model's view with a mechanical summary; never mutates the log (invariant 8) |
| Context planner | The single deterministic function that builds each request's view under a token budget (C7) |
| Detached run | A run owned by the supervisor rather than by the frontend that started it (R4) |
| Effect journal | Intent and outcome records around every side effect (R7) |
| Failover ladder | An ordered list of equivalent models to continue on when the primary is unavailable (L4) |
| Full automation | The owner-chosen consent mode that never asks (P5) |
| Handoff | Starting a fresh context inside the same run, seeded with the journal and a brief (C9) |
| In-doubt effect | A side effect with an intent record and no outcome record after a crash (R7) |
| Journal | The durable record of a run's goal, plan, decisions, milestones and next step (R5) |
| Merge queue | The serialized path by which parallel writers' branches are rebased, verified and landed (O6) |
| Persistent agent | An agent with an identity, memory scope and mailbox that outlives one task (O7) |
| Policy compiler | The single pipeline that turns every instruction and permission source into the compiled policy (P2) |
| Rejuvenation | Restarting a long run's process at a safe boundary to shed slow leaks (R17) |
| Rule of Two | Overseer's taint model: untrusted content plus sensitive data arms the exfiltration gate |
| Shadow verdict | The verdict a call would have received under per-action consent, recorded in full automation (S4) |
| Supervisor | The daemon component that owns run liveness and restarts (R4) |
| Taint | A latch recording that untrusted content entered a run, propagated with data (invariant 19) |
| Wave | A unit of sequencing that closes only on its exit gate (Chapter 23) |

## Appendix E — Sources

In this repository:

- `docs/agent-harness-playbook.pdf` (145 pages, September 2026): the twelve theses, harness teardowns, the Alfred architecture (Ch. 11 §7), the build plan (Ch. 12).
- `docs/LLM_Harness_Arsenal.pdf`.
- `docs/research/2026-09-29-pillars.md`: caching and token economics, resident footprints, the harness distillation table.
- `docs/research/2026-09-30-memory-v2.md`, `2026-10-06-memory-v3.md`: memory design and the H2/H3 specifications referenced by M1–M4.
- `docs/research/2026-09-30-tool-economy.md`: deferral and code-mode evidence and decisions.
- `docs/research/2026-09-30-subagent-tiers.md`: tiers, budgets, verification evidence, refusals.
- `docs/research/2026-10-05-digital-life.md`, `2026-10-05-life-security.md`, `2026-10-05-life-phase0-results.md`: Overseer Life plan, threat model and Phase 0 results.
- `docs/LEAVING-OFF.md`, `AGENTS.md`, `eval/README.md`, `eval/LEDGER.md`.
- The baseline code at the commits listed in Part 0, read for Chapter 3.

External work cited through those notes (tagged **[src]** in the text): Anthropic's multi-agent research system and context engineering posts; Cognition's "Don't build multi-agents"; MAST (Cemri et al. 2025); Huang et al. 2023 on self-correction; Panickssery et al. 2024 on self-preference; CRITIC (Gou et al. 2023); Lightman et al. 2023; Agentless (Xia et al. 2024); CodeMonkeys (Ehrlich et al. 2025); CodeAct (Wang et al. 2024); RAG-MCP (Gan et al. 2025); Lindenbauer et al. 2025 on observation masking; RULER, NoLiMa and Context Rot on effective context; MINJA and AgentPoison on memory poisoning; LongMemEval, LoCoMo and MemoryAgentBench; FrugalGPT and RouteLLM on routing; the agentic-browser security studies listed in the Life plan.

## Appendix F — How This Edition Was Reviewed

The owner asked for this roadmap to be thought through "twice, thrice" before its final edition. This appendix records what each pass changed.

### Pass 1: the author's own review

A full re-read against the contract, the baseline audit and the document's own invariants, looking for gaps, contradictions and designs that would not work. It changed:

| Area | Change |
|---|---|
| Identifiers | Automation workstreams renamed from W to A so they never collide with wave numbers; finding numbers from the fix2 reviews marked as such so they cannot be read as workstreams |
| R7 idempotency keys | The draft derived keys from the attempt number, which would have given every retry a new key and defeated deduplication. Keys now come from the session (or instance) and call (or step) only |
| R10 shared limits | Per-call admission through the daemon would have added a round trip to every request on a single-threaded loop. Replaced by background-renewed capacity leases |
| R10 pacing | Added subscription quota windows, fed by Overseer Life's usage readers |
| R4 | Interactive runs register with the supervisor and are offered, not forced, a resume |
| R16 | Workspace leases so two top-level runs never write the same tree |
| A2 | Workflow tool steps run outside the credential-free daemon; agent steps keep durable sessions instead of `--bare` temp sessions |
| O2 | Graph events go through the run's single log writer |
| P1 | Owner can trust a whole repository (Overseer's own AGENTS.md changes constantly) |
| New workstreams | P15 git safety defaults, S11 data at rest and backup, X10 documentation, Z9 licences and attribution |
| Premortems | Lessons from the three premortems folded back into R7, R9, R10 and A12 |
| Sequencing | Every workstream given a wave (L3, L5–L8, T3 and U10 had none); the coverage matrix completed |
| Failure atlas | Memory lock contention under many concurrent runs; workspace collisions |

### Pass 2: independent adversarial review

A separate reviewer, given only the owner's request, the document and the repository, checked more than 60 factual claims against the code and read every chapter for consistency, soundness and coverage, then inspected all 125 rendered pages. It reported 23 findings: 3 material, 11 minor, 9 nits. Every finding was investigated; all were confirmed and fixed:

| Finding | Fix |
|---|---|
| Eval corpus is 40 public tasks, not 35 (the repository README is stale) | Corrected in Chapters 3 and 17; README correction added to W0 |
| R7 could not resolve effects with no read-back, and parking for them would wedge unattended runs | In-doubt policy declared at intent time (`verify`, `replay_safe`, `model_verify`, `unverifiable`); unverifiable effects block only their dependent plan item while the run continues; invariant 13 reworded |
| The egress proxy's sandbox rule is not expressible with `bwrap` on Linux | S1 split into S1a (macOS, W4) and S1b (Linux transport, W10); Linux keeps deny-all until then |
| Stuck detector has five patterns, not four | Corrected |
| Resident spec "current pin" was the target | Corrected (pin 6,136 plus 5%, target 6,000) |
| Wave ranges swept in workstreams placed elsewhere | Exclusions written into Chapter 23 and its graph |
| "No single failure ends a run" overstated | Scoped to transient and infrastructure failures |
| Long-horizon targets depended on an undefined "solvable" | Solvability declared before scoring, never after (E2, metrics) |
| No true private holdout until the wave that consumes it | Holdout authored in W2 |
| No equivalence policy for competitor comparisons | Published equivalence and "not comparable" policy (E8) |
| Model refusals had no failure class | `Refused` class added to R1 |
| Workflow migration ignored semantic and engine-version changes | A9 refuses semantic changes without confirmation and checks engine ranges |
| Keepalive mechanics unstated | Out-of-band, never logged, suppressed while throttled |
| Invariant 25 contradicted by the pasted-key incident | Scoped to secrets the harness holds; pasted secrets handled by S7 |
| Repository trust grant keyed on values the repository controls | Bound to the repository's git directory identity |
| No relative sizing | Relative size per wave added (no dates, deliberately) |
| No import path from incumbent harnesses | X11 added |
| U2 missing from wave contents; M8 name collision; fix2 wording | Corrected |
| Display headings hyphenated | Template fixed |
| This appendix was a placeholder | Filled |

### Pass 3: final consistency and render check

After the repairs: every workstream reference in the text resolves to a defined workstream (checked by script), Appendix C was regenerated from the chapter headings so it cannot drift from them, and the final build was rendered and every page inspected again.
