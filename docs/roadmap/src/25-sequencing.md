## 23. Sequencing: Waves, Dependencies and Gates

### 23.1 Rules of sequencing

1. **One wave in flight at a time.** A wave closes when its exit gate passes. Parallel waves are how roadmaps die (premortem three).
2. **Inside a wave, workstreams land in reviewable slices.** A refactor that preserves behaviour (proven by golden tests) lands before any behaviour change, as in C7 and P2.
3. **Every wave's exit gate includes the standing gates** (and every wave carries X10 documentation and Z9 attribution for what it changes): formatting, clippy with warnings denied, the full test suites, the build budgets (Z1), the golden log corpus (R13), and an independent adversarial review of the wave's delta (S9). Where the eval hold is lifted, it also includes E5's regression check; where it is not, the free model on the local corpus stands in.
4. **Owner decisions that block a wave are asked before the wave starts** (Chapter 26).
5. **Local first.** Everything runs on the owner's Mac. One cloud session at a time, only deliberately.

### 23.2 Dependency graph

```
W0  land what exists ─────────────────────── E1 baseline ───────────┐
 │                                                                   │
W1  survival      R1 R2 R3 R15 L1 L2 L3 L4 L8                        │
 │                                                                   │
W2  supervisor    R4 R6 R7 R12 R13 R14 R17 S3 S10 X1-basic B8 E9*    │
 │                                                                   │
W3  intent        R5 R9 R10 R11 R16 C5 O9 B1-B5 Z7                   │
 │                                                                   │
W4  policy        P1-P6 P9 P11-P15 R8 S1a S2 S4 S11 X4 U3-U5 U7      │
 ├───────────────────────────┐                                       │
W5  context+tokens            W6  orchestration                      │
    C1-C10 (not C5) K1-K11        O1-O18 (not O9; O12 is C9)         │
    T1-T12 L6 L7 U10              X1-full (needs W4 taint, grants)   │
 ├───────────────────────────┘                                       │
W7  automation    A1-A12 X2 X5   (needs W2, W4, W6 artifacts)        │
 │                                                                   │
W8  memory        M1-M11                                             │
 │                                                                   │
W9  proof         E2-E9 U9 B6 B7  ◀──────────────────────────────────┘
 │
W10 reach         X3 X6-X11 Y1-Y6 (Y3 is T5) Z2-Z6 U6 U8 L5 R18 F1-F5
                  P7 P8 P10 M12 S1b S5 S8          * E9 holdout authored
```

W5 and W6 both depend on W4 and not on each other. By rule 1 they still run one after the other; the order is the owner's choice (Chapter 26), with W5 first recommended because its savings compound across everything after it.

### 23.3 The waves in detail

#### W0 — Land what exists

**Contents.** Integrate `fix2-memory` into `feat/memory-v3` with the owner's option (a) for fix2-memory finding M8; integrate all fix branches plus the `--bare` skills fix into `feat/fortify`; full local gates; adversarial review of the fix delta; push both PR branches and update their Deferred sections; merge #54, retarget and merge #55. Then the consent-modes branch: P5 and its computer-use application U2, as a first cut on today's `perm.rs`, migrated into the compiler in W4. Owner actions in parallel: rotate the 2026-09-20 key (S7), decide on the eval hold (E1), live computer-use validation with owner presence (U1), the first priced live run (K12), move the repository out of iCloud (Z8), the `pr-run-mode` decision (Z3). Hygiene: correct `eval/README.md`, which still says 35 public tasks after the corpus grew to 40.

**Exit gate.** #54 and #55 merged into `review` with green local gates and a clean adversarial review; the consent-modes branch merged with tests for both modes; owner decisions recorded.

#### W1 — Survival

**Contents.** R1 failure taxonomy; L1 adapter classification tables; R2 retry and backoff with discarded partial streams; R3 every timeout hole; R15 fault injection; L2 living profiles; L3 capability probes; L4 failover ladders with multi-key support; L8 deprecation watch; the disk preflight part of R11.

**Exit gate.** Zero run ends from transient classes under injected faults at 1%, 10% and 50% rates; the R3 blocking-call lint passes; failover round-trip test passes; no reasoning block crosses providers.

#### W2 — The supervisor

**Contents.** R7 effect journal (lands before auto-resume is enabled); R4 supervisor with registry, heartbeats, verdicts, restart budget, reboot re-adoption and detached runs; R14 lifecycle and commands; R6 crash-consistency audit; R12 time, sleep and power; R13 versioning and the golden log corpus; R17 process hygiene and rejuvenation; S10 kill switch everywhere; S3 threat model update; X1 basic cockpit (runs list, attach); B8 health panel; authoring of the private evaluation holdout (E9) so it seasons before W9 uses it.

**Exit gate.** The R4 chaos gates (200 random kills, daemon kill, simulated reboot, crash loop); zero duplicate effects across 1,000 injected crashes; kill switch latency targets from every frontend; a 24-hour soak (Z7).

#### W3 — Durable intent

**Contents.** R5 journal, plan tool promotion and resume briefings; R9 progress ledger and stall detection; R10 rate budgets, shared key buckets and forecasting; the rest of R11 (snapshots, log segments, spill and worktree collection); R16 environment drift; C5 edge placement; O9 goal audits; B1 traces, B2 rollups, B3 timeline, B4 forecasting, B5 alerts.

**Exit gate.** The 40-compaction briefing test; snapshot resume ≤ 2 s on a synthetic 100-hour log; rate budget pause and resume; the first 100-hour soak run on the owner's Mac completes unattended.

#### W4 — Policy

**Contents.** P1 AGENTS.md loader; P2 compiler with behaviour-preserving migration; P3 rules v2; P4 scoped grants; P5 consent modes migrated into the compiler; P6 sensitive-surface permissions; P9 contextual rules; P11 explanations; P12 permissions management; P13 build-enforced principles; P14 instruction hierarchy; P15 git safety defaults; R8 away policy with non-blocking asks; S1a egress proxy on macOS; S2 credential broker completion; S4 full-automation audit trail; S11 data at rest, retention and backup; X4 phone approvals; U3 progress guard, U4 settle waits, U5 clipboard, U7 revocation on lock.

**Exit gate.** Golden tests show no behaviour change from the compiler migration; no module reads raw policy sources; untrusted AGENTS.md never lands above the cache boundary; away-policy and egress gates pass.

#### W5 — Context and tokens

**Contents.** C1–C10 except C5, which landed in W3 (C7 planner first, behaviour-preserving); K1–K11; T1 hashline edits (measured); T2 `run_code` reach and U10 GUI scripts (after U1); T3 persistent QuickJS context (if the rig shows need); T4 native deferral (measured); T8 parallel reads; T9 shaped results; T10 web tool (after S1a; on Linux after S1b); T11 latency budgets; T12 description tests; T5 (which is also Y3), T6, T7; L6 native provider features and L7 routing by task class.

**Exit gate.** Cost per solved task down at least 30% from the W0 baseline with pass-rate non-inferiority; cache hit rate at or above target; resident tokens at or below 2,000.

#### W6 — Orchestration

**Contents.** O1–O18 except O9 (landed in W3) and O12 (the same work as C9, landed in W5), with O6 merge queue and O2 graph first; X1 full cockpit with the agent graph (O14).

**Exit gate.** The O2 and O6 gates; fork subagent cache share at least 90%; E7 shows orchestration pays where it is used.

#### W7 — Automation

**Contents.** A1–A12; X2 inbox frontend; X5 notifications.

**Exit gate.** The A2 crash gates; A5 time-zone tests; the workflow library templates pass their own tests; a 30-day run of the morning digest and memory dream workflows on the owner's machine with zero silent failures.

#### W8 — Memory

**Contents.** M1–M11 (M12 waits for W10).

**Exit gate.** The memory v3 record's verification lists for H2 and H3; M9's suites; M5 shows measured uplift or stays off.

#### W9 — Proof

**Contents.** E2 long-horizon suite; E3 chaos suite; E4 published cost per solved task; E6, E7, E8 competitors on the same model; E9 private holdout; U9 computer-use evaluation; B6 export; B7 replay debugger.

**Exit gate.** A published report card with paired statistics against competitors on the same model, the long-horizon and chaos results, and every commitment in the executive summary either met or explicitly reported as not yet met.

#### W10 — Reach

**Contents.** X3 ACP adapter; X6–X11; Y1–Y6 except Y3 (landed as T5 in W5); Z2 signing and Z3 installers (Apple account); Z4 iOS gating; Z5 Linux parity; Z6 Windows plan; L5 local models; U6 Overseer's computer helper (Apple account); U8 browser protocol; R18 remote executor; F1–F5 Life integration; P7 user modes, P8 stream rules, P10 hooks v2; M12 shared memory; S1b Linux egress transport; S5 signed provenance; S8 sandbox hardening.

**Exit gate.** Per workstream; W10 is a portfolio, and its items can close independently once W9 is done.

### 23.4 Coverage matrix: every gap has an owner

Every gap found in the baseline audit (Chapter 3) and every open item in `docs/LEAVING-OFF.md` maps to a workstream.

| Gap (Chapter 3 reference) | Workstream | Wave |
|---|---|---|
| Provider errors end the run; `retry_after` unused (3.1) | R1, R2, L1 | W1 |
| No automatic resume or supervisor (3.2) | R4 | W2 |
| No wall-clock budget (3.3) | R3 | W1 |
| Ask dialog has no deadline (3.3) | R8 | W4 |
| Notifier and Telegram block the daemon tick (3.3) | R3 | W1 |
| Gateway runs killed at 600 s (3.3) | R3, R4 | W1–W2 |
| Provider HTTP single 600 s timeout (3.3) | R3 | W1 |
| Count-based eviction (3.4) | C1, C7 | W5 |
| No real tokenizer (3.4) | K4 | W5 |
| Only three breakpoints, 5-minute TTL only (3.4) | K1, K2, K3 | W5 |
| No web tool (3.5) | T10, S1a | W4–W5 |
| `run_code` cannot reach computer or memory (3.5) | T2 | W5 |
| No project MCP, cargo-only diagnostics, one-time availability (3.5) | T5, T6, T7 | W5 |
| No task DAG, artifacts, depth cap, persistent agents, detach (3.6) | O1–O7 | W6 |
| Memory v3 H2 and H3 (3.7) | M1–M4 | W8 |
| Rules bash-only, exact-match, append-only, no deny (3.8) | P3, P4, P12 | W4 |
| AGENTS.md not read (3.8) | P1 | W4 |
| No instruction compiler (3.8) | P2 | W4 |
| Single-shot gateway, no workflows (3.9) | A1–A12 | W7 |
| Fixed time-zone offset in the gateway (3.9) | A5 | W7 |
| No egress proxy (3.10) | S1a (macOS), S1b (Linux) | W4, W10 |
| Broker rate and window not enforced (3.10) | S2 | W4 |
| No signed builds (3.10) | S5, Z2 | W10 |
| No live evaluation results (3.11) | E1, K12 | W0 |
| No long-horizon, chaos or memory suites (3.11) | E2, E3, E6 | W9 |
| Live computer-use validation (3.12) | U1 | W0 |
| Live priced run (3.12) | K12 | W0 |
| Orchestration wave B (3.12) | O2, O3, O4, O6 | W6 |
| Eval hold (3.12) | E1 (owner decision) | W0 |
| Ignored `fix2-agent` reviewer tests (3.12) | W0 integration review | W0 |
| `release.yml` plan job failing (3.12) | Z3 (owner decision) | W0 |
| Provider key rotation (3.12) | S7 (owner action) | W0 |
| Repository in iCloud (3.12) | Z8 (owner action) | W0 |
| Bash background processes killed after the command (3.12) | Documented behaviour; long-running services belong to workflows or supervised runs, not `cmd &` | W0 note |
| Subagent worktrees accumulate (3.12) | R11 worktree collection | W3 |
| No cross-session observability (3.4, 16.1) | B1–B5 | W3 |
| No user-defined modes (3.1) | P7 | W10 |
| Microagent substring triggers (3.1) | P9 | W4 |
| Hooks are data-only (3.8) | P10 | W10 |
| Consent modes not implemented (3.8) | P5, U2 | W0, W4 |

### 23.5 Relative size of each wave

Sizes are relative engineering effort, not calendar time. How long a wave takes depends on review capacity, owner decisions and what the evaluation shows, so this roadmap deliberately gives no dates.

| Wave | Relative size | Why |
|---|---|---|
| W0 | S | The fixes exist and are reviewed; the work is integration, gates and review |
| W1 | M | Contained in the provider layer and the agent loop; fault injection is the larger half |
| W2 | L | New process model (supervisor, detached runs) plus the effect journal; heavy crash testing |
| W3 | L | Journal, snapshots and log segments touch the event layer that everything depends on |
| W4 | XL | The compiler migrates every policy source behaviour-preserving, then adds new semantics; egress proxy |
| W5 | L | Many independent, measured changes; the planner refactor is the critical piece |
| W6 | XL | Graph executor, artifacts, merge queue and persistent agents are each substantial |
| W7 | XL | A durable workflow engine is a product in its own right |
| W8 | M | The memory v3 record already specifies H2 and H3 in detail |
| W9 | L | Mostly building suites and running them; cost is money and machine time more than code |
| W10 | XL | A portfolio of independent items that close one by one |

### 23.6 How progress is reported

At each wave close: a short report in `docs/research/` (what landed, gate evidence, deferred items with owners and gates, metric deltas), an update to `docs/LEAVING-OFF.md`, an updated scorecard (the executive summary table) in the roadmap's next edition, and the PR and merge commit Deferred sections the standing rule requires.
