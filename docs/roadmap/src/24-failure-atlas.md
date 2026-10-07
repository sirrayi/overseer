# Part IV — Execution

## 22. What Could Break: The Failure Atlas

The owner asked: "get into this zone of the future, what could potentially break?" This chapter answers systematically. It lists every failure mode found while writing this roadmap, grouped by where it originates, with its likelihood over a 100-hour run (or over a year of use, for slow risks), its impact, and the workstream that handles it. Then it runs three premortems: stories of how a 100-hour run, a standing automation, and this roadmap itself could fail, written as if they already had.

Likelihood: **H** (expected), **M** (plausible), **L** (rare but real). Impact: **H** (data loss, money, security, or a dead run), **M** (wasted time or money, recoverable), **L** (annoyance).

### 22.1 Providers and models

| Failure | L | I | Handled by |
|---|---|---|---|
| Rate limits (429) during a long run | H | H→L | R1, R2, R10 |
| Provider overload (529) for minutes to hours | H | H→L | R2, L4 |
| Network blips, DNS failures, TLS resets | H | H→L | R2, R12 |
| Stream cut mid-response | M | M→L | R2 (discard partial, retry) |
| Provider returns 200 with an error body | M | M | R1 (body classification) |
| Misused status codes (400 meaning overload) | M | M | R1 (per-adapter tables with fixtures) |
| Key revoked, expired or out of credit | M | H | R1 `Auth` → park with owner action |
| Model retired mid-run | L | H | L8, L4 |
| Model behaviour changes silently under the same name | M | M | E5 regression gates; L3 probes per binary version |
| Price change makes budgets wrong | M | M | L2 `verified_on` dates; K12 ledger reconciliation |
| Cache rules change (TTL, minimum prefix, breakpoint semantics) | M | M | K10 forensics; cache alert; L2 |
| Reasoning round-trip rules change | L | H | C4 per-adapter declarations with fixture tests; L3 probes |
| Provider-side context features conflict with invariants | L | M | K11 evaluation before adoption |
| A cheaper failover model silently lowers quality | M | M | L4 tier rules; journal notes; O9 goal audits |
| Token estimate drifts, causing late compaction and context errors | M | M | K4 self-calibration; existing `ContextWindowExceeded` path |

### 22.2 Processes and the machine

| Failure | L | I | Handled by |
|---|---|---|---|
| Run process crashes (panic, OOM, signal) | M | H→L | R4 auto-resume |
| Laptop reboots (OS update, power loss) | M | H→L | R4 re-adoption at login |
| Laptop sleeps, lid closes | H | M→L | R12 |
| Battery runs flat during a run | M | M | R12 battery policy, R4 resume |
| Daemon crashes | M | M | launchd/systemd restart; runs continue independently |
| Two daemons start | L | M | Daemon lock |
| A step hangs (tool or provider never returns) | M | H | R3 deadlines; R4 hung verdict |
| Grandchild processes hold pipes open | M | H | **[fix2]** process-group kill |
| Zombie and orphan processes accumulate | M | M | R17 reaping and soak checks |
| Memory leak over days | M | M | R17 rejuvenation |
| File descriptor leak | M | M | R17 |
| CPU saturation from parallel builds | H | M | O18 scheduler |
| Disk fills with logs, spills and worktrees | M | H | R11 |
| The repo sits in iCloud sync, which corrupts git state | M | H | Z8 (owner action) |
| An Overseer upgrade breaks in-flight runs | M | H | R13, Z3 |
| The machine's clock jumps (NTP correction, manual change) | L | M | R12 monotonic timing |

### 22.3 Storage and state

| Failure | L | I | Handled by |
|---|---|---|---|
| Torn write at the end of the event log | M | L | Have (torn-tail repair) |
| Corruption earlier in the log | L | H | Have (error names the line); R6 audit; segment manifests (R11) |
| A snapshot disagrees with the log | L | H | R11 (snapshot covers a hash; fall back to full replay) |
| Store written without fsync, lost on power loss | M | H | R6 |
| Concurrent writers to a memory store | L | M | Have (flock, drift check) |
| Memory store lock contention once many supervised runs share a store (10 s acquire, then `BUSY`) | M | M | R1 classifies `BUSY` as transient and retries; learning reviews of concurrent runs are serialized by the supervisor |
| Two top-level runs write the same working tree | M | H | R16 workspace lease |
| Sidecar says running for a dead task | M | M | Have (nonce reaping) |
| Worktree deleted under a writer | L | M | R16; merge queue parks |
| Old logs fail to replay after a schema change | M | H | R13 golden corpus |
| Workflow definition changed under running instances | M | M | A9 version pinning |

### 22.4 Side effects

| Failure | L | I | Handled by |
|---|---|---|---|
| Auto-resume repeats a `bash` command with effects | M | H | R7 in-doubt notice and verify-before-repeat |
| Auto-resume resends a message or webhook | M | H | R7 never auto-replays external; outbox ids |
| A retried workflow step double-spends money | L | H | R7, A2 idempotency keys; money always parked |
| Compensation fails after a partial workflow | M | M | A4 retried, journaled compensation; irreversible steps flagged |
| A writer's merge lands and breaks the build | M | M | O6 verify after every landing |
| Rewind restores files but not external effects | H | M | Invariant 9 blind spot documented; R7 records what it cannot undo |

### 22.5 Model behaviour

| Failure | L | I | Handled by |
|---|---|---|---|
| Wrong diagnosis before the first edit | H | M | Verification (O10, verify gate); playbook evidence says this dominates failures |
| Futile loops late in a run | H | M | Have (stuck detector); R9 long-window stall detection |
| Drift from the goal while looking busy | M | H | R5 journal; R9; O9 goal audits |
| Premature "done" | M | H | Mandatory verification; unknown is never pass |
| Claims milestones without evidence | M | M | R5 evidence-checked milestones |
| Ignores AGENTS.md conventions | M | M | P1 loader; contextual rules (P9); memory |
| Narrates verbosely, wasting output tokens | H | L | K8 |
| Assumes a pending approval succeeded | M | M | R8 wording, gate on dependent actions |
| Misreads a resume briefing as new instructions | L | M | Provenance wrapping; briefing format tested on the rig |

### 22.6 Orchestration

| Failure | L | I | Handled by |
|---|---|---|---|
| Bad brief produces useless subagent output | H | M | O13 brief linter; O8 brief schema |
| Subagents duplicate each other's work | M | L | O4 blackboard claims |
| Token blow-up from over-orchestration | M | M | Graph budgets; E7 tuning |
| Merge-queue conflicts loop | M | M | O6 re-plan notice |
| Unbounded agent tree depth | L | M | O1 |
| A failed node hidden in a "done" summary | M | H | O-design: run cannot complete with failed nodes |
| Tainted subagent poisons artifacts or blackboard | M | H | O3, O4 taint |

### 22.7 Memory

| Failure | L | I | Handled by |
|---|---|---|---|
| Poisoned note from untrusted content | M | H | Have (quarantine, threat scan); M10 for new paths |
| Stale note recalled with authority | M | M | Confidence, validity, fading (M3), FEEDBACK |
| Outcome memory learns superstition | M | M | M5 gate on measured uplift |
| Memory growth slows search | L | L | Have (bounded index); M3 |
| Memory content breaks the cache mid-session | L | M | Have (resident memory frozen per agent) |

### 22.8 Policy and security

| Failure | L | I | Handled by |
|---|---|---|---|
| Malicious repository AGENTS.md, MCP file, hooks or skills | M | H | P1, T5, P10 hash-pinned trust |
| Prompt injection read late in a long run | M | H | Latches persist; invariant 19; S6 corpus |
| Exfiltration through an allowed domain | L | H | S1 proxy policies under taint |
| Approval fatigue pushes the owner to full automation everywhere | M | M | P4 scoped grants; per-action prompt rate target |
| Full automation performs a harmful action | L | H | Owner's decision; P6 permissions; S4 audit; S10 kill switch |
| Forwarded Telegram approval message replayed | L | H | X4 signed, expiring action tokens |
| A secret pasted by the owner lands in logs | M | H | S7 scanning and redaction; key rotation |
| Confused deputy through the daemon | L | H | S3: daemon thin, credential-free, trust levels propagate |
| `sandbox-exec` deprecated by Apple | L | H | S8 contingency |

### 22.9 Automation

| Failure | L | I | Handled by |
|---|---|---|---|
| Scheduled workflow silently stops running | M | H | A12 dead-man's switch; digest health line |
| Daylight saving causes double or missed runs | M | M | A5 |
| A workflow spams channels | L | M | Outbox caps; budgets; kill switch |
| Webhook flood | M | M | Have (rate limits, replay table) |
| Long sleep causes a backlog of catch-up runs | M | M | A5 missed-run policy |

### 22.10 People and process

| Failure | L | I | Handled by |
|---|---|---|---|
| The roadmap is too large to finish | H | M | Waves are independently valuable; W1–W3 remove most failure classes |
| Cloud quota exhausted by parallel sessions (it happened) | M | M | Standing rule: one session at a time, local first |
| GitHub Actions minutes spent by accident | M | L | `[skip actions]` rule; Z3 `pr-run-mode` |
| Review rigour drops under schedule pressure | M | H | Standing review rule; S9 wave-close red team |
| Key decisions wait on the owner and block waves | M | M | Chapter 26 lists every decision with its blocking wave |
| Knowledge lives only in chat sessions | M | M | Decision records, LEAVING-OFF notes, this roadmap |

### 22.11 The longer future

Risks that grow over years rather than days.

| Trend | Risk to Overseer | Posture |
|---|---|---|
| Context windows keep growing and get cheaper | Context management becomes less critical; complexity in C rots | Every C workstream is gated on measured benefit; the planner keeps policies swappable |
| Models get much better at long tasks | Scaffolding encodes outdated weaknesses (playbook thesis 2) | Re-measure every component against the minimal loop each wave (E5); remove what no longer pays |
| Providers ship their own agent runtimes and memory | Competition from the model vendors themselves | Local-first, multi-provider, owner-controlled policy and memory are the differentiators |
| Agent protocols consolidate (ACP, MCP successors) | Integrations go stale | Thin adapters (X3, Y4); protocol code isolated |
| OS vendors restrict accessibility and sandbox APIs | Computer use and sandboxing break | Separate signed helper (U6), sandbox contingency (S8) |
| Regulation of autonomous agents | Full automation may need auditability by law | S4's audit trail and the event log already provide it |
| Terms of service tighten on undocumented endpoints | Life usage readers break | Life plan's connector health and per-source opt-in |

### 22.12 Premortem one: the 100-hour run that failed

*Written as if it happened.* The migration ran for 61 hours and then stopped making progress. Looking back:

1. On hour 14, a writer ran a database migration script against a local development database through `bash`. The process crashed during the command. On resume, the agent saw the in-doubt notice but the verification it chose (listing tables) looked fine, so it re-ran the script, which applied a second, partially conflicting migration. **Lesson:** R7's verify-before-repeat is only as good as the verification; the policy compiler should let AGENTS.md declare commands that are never auto-repeatable, and the in-doubt notice should name the risk class.
2. On hour 30, a provider failover to another family happened mid-exchange because the ladder did not wait for a handoff boundary. Reasoning blocks were dropped from the view, and the model lost the thread of a subtle refactor. **Lesson:** L4 requires cross-family failover only at a handoff boundary; this is now in L4's design.
3. From hour 40, the plan in the journal kept saying "doing #10" while diffs oscillated in the same three files. The stall detector's window was sized for the plan's total length, so it did not fire until hour 58. **Lesson:** R9 windows must scale with per-item history, not plan size.
4. At hour 61 the rate budget paused the run overnight, and the owner, seeing "paused", assumed it had finished. **Lesson:** paused states need a distinct notification and a forecast of when they resume.

Each lesson is folded into the named workstream.

### 22.13 Premortem two: the automation that died quietly

*Written as if it happened.* The morning digest ran perfectly for five weeks, then stopped. Nobody noticed for nine days.

1. A Life connector started returning an authorization error after the owner logged out of one tool. The step failed, retried, and the instance ended `failed`.
2. The failure produced an inbox item, but the inbox item was in a digest that the failed workflow itself was supposed to send.
3. The dead-man's switch was configured for "no successful run in 48 hours" but the alert channel was the same Telegram bot whose token had rotated.

**Lessons:** failures of the digest workflow must alert through a channel independent of the digest (the notifier plus the cockpit badge); the dead-man's switch must have a fallback channel; channel credentials need health checks of their own (A12).

### 22.14 Premortem three: the roadmap that failed

*Written as if it happened.* A year later, half the waves were started and none were finished.

1. Waves ran in parallel to "save time", so no wave closed its gates and nothing was ever measured against a stable baseline.
2. The eval hold was never lifted, so every "improvement" was a guess, and some made things worse.
3. Big-bang branches for the policy compiler and the workflow engine grew for months, became impossible to review line by line, and were abandoned.
4. Leanness budgets were relaxed "temporarily" and never restored.

**Lessons:** one wave at a time with explicit gates (Chapter 23); E1 is an early owner decision (Chapter 26); every workstream lands in reviewable slices behind behaviour-preserving refactors first (the C7 and P2 golden-test pattern); Z1 budgets are part of every wave's exit gate.
