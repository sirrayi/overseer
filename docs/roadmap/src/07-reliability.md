# Part II — The Pillars

## 5. Reliability and Long-Horizon Execution

> "tasks can go on for 100+ hours no issue and uninterrupted if needed" (owner, 2026-10-07)

Reliability is the spine of this roadmap. Orchestration, workflows, policy and memory all assume that the unit they build on, a run, survives. Today it does not survive much: the first rate limit, network drop or crash ends it. This chapter turns "resumable" into "self-healing".

### 5.1 Why a hundred hours changes the architecture

A hundred hours is not a short run made longer. It crosses thresholds where failures that are rare per call become certain per run.

Assume a long run makes one provider call per minute on average. Over 100 hours that is about 6,000 calls. If one call in two hundred fails transiently (rate limit, overload, network blip, a provider's bad minute), the expected number of transient failures in the run is about 30, and the probability of seeing none is about 0.995^6000, which is effectively zero **[infer]**. The same arithmetic applies to every other component: a hundred hours on a laptop will include sleep, network changes, probably an OS update, and possibly a reboot. A run that dies on the first of these cannot reach hour 100.

So the requirement is not "fewer failures". It is that **no transient or infrastructure failure ends a run**: runs end only for terminal conditions that are recorded and owner-controlled (a total budget reached, a fatal invariant violation, the owner abandoning the run). That needs five things the engine does not have: a classification of failures, retry policies per class, a process that brings dead runs back, a record of intent that survives everything, and a record of side effects that makes coming back safe.

### 5.2 Where we stand

The substrate is right, and that is most of the battle:

- The event log is immutable, hash-chained and torn-tail tolerant, so a run's history survives a crash **[have]**.
- `Agent::resume` rebuilds a run from its log **[have]**, and compaction is a view, so resuming never depends on a lost in-memory summary.
- Subagent liveness is tracked with per-process nonces and dead tasks are reaped **[have]**.
- Most tool calls have timeouts, and **[fix2]** closes the worst hang (bash grandchildren holding the pipe).

What is missing, from the audit in Chapter 3:

- Every provider error except one malformed-response retry ends the run.
- Nothing restarts a dead run; resume is manual.
- No wall-clock budget exists; cost and step budgets are tuned for minutes.
- The ask dialog waits forever; gateway-spawned runs are killed after 600 s.
- Intent lives in the context window and the compaction summary.
- `bash` side effects are not recorded, so a resumed run cannot know whether its last command ran.
- Logs, spill files and worktrees grow without bound.

### 5.3 The target in one picture

```
                 ┌───────────────────────────────────────────────┐
  login/boot ──▶ │ SUPERVISOR (in overseerd, launchd/systemd unit) │
                 │  run registry · heartbeats · liveness verdicts │
                 │  restart budget · upgrade policy · key buckets │
                 └──────┬──────────────────────────────┬─────────┘
             spawns/adopts                       notifies / asks
                        ▼                                ▼
 ┌──────────────────────────────────────┐   ┌──────────────────────────┐
 │ RUN PROCESS (overseer run --id …)    │   │ frontends: TUI · web ·   │
 │  heartbeat thread · step watchdog    │◀─▶│ Telegram · notifications │
 │  retry/failover · rate budgets       │   └──────────────────────────┘
 │  effect journal · intent journal     │
 │  event log (source of truth)         │
 └──────────────────────────────────────┘
```

The run process does the work and keeps its own durable records. The supervisor watches, restarts, enforces global limits and talks to the owner. Frontends are clients that attach to runs; closing a frontend never ends a run.

### 5.4 Workstreams

#### R1 — Failure taxonomy

**What.** A closed classification of every failure the engine can see, used by every retry, failover, pause and end decision.

**Design.** A `FailureClass` enum in core, assigned at the point of failure, logged on the event that records the failure:

| Class | Examples | Default handling |
|---|---|---|
| `Transient` | Connection reset, DNS failure, TLS handshake failure, 502/503/504, stream cut mid-response, a memory store lock reporting `BUSY` | Retry with backoff (R2) |
| `Throttled` | 429, provider "rate limit" errors, opencode pool saturation | Honour `retry-after`; slow the key's bucket (R10) |
| `Overloaded` | Anthropic 529, "overloaded" messages | Backoff; after N attempts consider failover (L4) |
| `Degraded` | Repeated malformed or truncated responses, empty responses | Retry once; then failover or effort change |
| `ContextLimit` | Context-window exceeded | Compact (existing behaviour) |
| `Budget` | Cost, step, wall-clock or rate budget exhausted | Pause (rate) or end (total), per R10 |
| `Auth` | 401/403, revoked or expired key | Park with a clear owner action; never retry blindly |
| `Policy` | Gate refusal, hard deny | Return to the model as a tool result (existing) |
| `Environment` | Disk full, workspace missing, TCC revoked, MCP server gone | Pause or park with a named cause (R11, R16) |
| `Fatal` | Internal invariant violation, corrupt log line before the tail | End with a recorded reason and a preserved log |
| `Refused` | A successful response whose `stop_reason` or finish reason is a refusal or content filter, or an adapter-detected refusal in the body | Recorded with the raw reason (invariant 7); one retry with the refusal noted; repeated refusals on the same step park the run with the refusal text attached, instead of burning steps until the stuck detector trips |
| `Unknown` | Anything unclassified | Treated as `Degraded` for one retry, then park; every `Unknown` is a bug to classify |

**Interfaces.** `ProviderError` gains a `class()` method; tool errors and environment errors map through one function. The raw provider `stop_reason` and error body stay preserved (invariant 7).

**Edge cases.** Providers misuse status codes (a 400 that means overload, a 500 that means bad request). Per-provider mapping tables live in the adapters, with fixtures captured from real errors. A class can be upgraded by repetition: three `Transient` in a row on one endpoint inside a minute becomes `Overloaded`.

::: gate
Every `ProviderError` variant and every tool error path maps to a class, proven by an exhaustive match with no wildcard arm. Fixture tests per provider adapter for at least 429, 500, 502, 503, 529, 401, connection reset and mid-stream cut. `RunEnd` carries a reason from the closed set (invariant 10).
:::

#### R2 — Provider retry, backoff and resumable streams

**What.** A run never ends on a `Transient`, `Throttled` or `Overloaded` failure while retry budget remains.

**Design.**

- **Backoff.** Decorrelated-jitter exponential backoff: base 2 s, cap 5 minutes, attempt delay `min(cap, random(base, previous × 3))`. When the response carries `retry-after` or a provider-specific reset header, wait at least that long, capped by a configurable ceiling (default 30 minutes).
- **Budgets on retrying itself.** Per class, a maximum consecutive attempt count (default 12 for `Transient`, unlimited-but-paced for `Throttled`), and a global retry budget per hour per run. Exhausting it moves the run to `waiting{backoff}` with a growing interval rather than ending it. Only `Auth` and `Fatal` end or park immediately.
- **Ledger.** Each attempt is a call, so each gets its ledger row (the existing one-row-per-call rule). Retries are visible in cost and in `overseer stats` as their own purpose.
- **Spend gate.** Each attempt is re-priced against the remaining budget, so a retry storm cannot overspend.
- **Partial streams.** A response cut mid-stream is discarded whole; nothing partial is appended to the event log. The attempt is retried from the same request. The log only ever holds complete model responses, which keeps tool pairing and invariant 6 safe.
- **Idempotency.** Provider calls have no side effects beyond billing, so they are always safe to retry. Tool calls are not; they are covered by R7.

**Edge cases.**

- A provider returns 200 with a body that is an error. Adapters classify by body as well as status.
- `retry-after` values in the hours. Above the ceiling the run moves to `waiting{throttled}` and notifies the owner instead of sleeping silently.
- Retries during an owner interrupt. The interrupt wins; backoff sleeps are interruptible (the same condition-variable pattern `wait_for` uses).
- Clock jumps during a sleep. Backoff uses monotonic time (R12).

::: gate
Fault-injection tests (R15) at 1%, 10% and 50% transient rates over 1,000 simulated calls show zero run ends, correct ledger rows per attempt, and backoff timing within bounds. A `retry-after` of 90 s is honoured to within 1 s. An interrupt during backoff stops the run within 200 ms.
:::

#### R3 — Close every timeout hole

**What.** Invariant 14: every wait has a deadline and an escalation. The audit found these holes, each with its fix:

| Hole | Fix |
|---|---|
| No wall-clock run budget | `max_wall_hours` on `AgentConfig`, counted in monotonic time while awake (R12); default off for interactive runs, set by the long-run profile (§5.6) |
| Provider HTTP is one 600 s global timeout | Split into connect (10 s), time-to-first-byte (120 s, longer for high-effort reasoning profiles) and stream idle (90 s between chunks). **Hypothesis to verify:** the pinned `ureq` version exposes separate connect and receive timeouts; if not, wrap the stream reader with an idle watchdog |
| Ask dialog waits forever | Away policy (R8) |
| Desktop notifier has no timeout | Spawn with a 5 s deadline and kill; move off the daemon tick to a notifier thread |
| Telegram calls run inside the daemon tick | Move channel polling to a dedicated thread with a bounded queue into the tick |
| Gateway spawn watchdog kills at 600 s | Replace with supervisor-owned runs (R4) that carry their own wall and rate budgets; the watchdog becomes a heartbeat check |
| Foreground subagents inherit an unbounded clock | Per-task `max_wall_minutes` (tier defaults), counted against the parent's wall budget |
| A single step can run indefinitely (long tool + long provider call) | Step watchdog: a step exceeding `step_max_minutes` (default 30) raises a `hung` verdict for the supervisor |

::: gate
A test per hole proves the deadline fires and the escalation runs. A static check in CI lists every `Command::status()`, `Command::output()`, `recv()` and blocking read in the workspace and fails on any not wrapped in a deadline or explicitly allow-listed with a comment.
:::

#### R4 — The supervisor

**What.** A component that owns run liveness: knows every run, detects death and hangs, restarts runs safely, and survives reboots.

**Where it lives.** In the gateway daemon (`overseerd`), which is already always-on, single-threaded and Unix-only. It is started at login by a launchd agent on macOS and a systemd user unit on Linux, both installed by a new `overseer daemon install` command. The supervisor adds a module to the tick loop; it does not change the daemon's thin, credential-free character (playbook Alfred architecture). Runs stay separate processes.

**The run registry.** `~/.overseer/runs/<run-id>.json`, one sidecar per supervised run, modelled on the existing `task.json`:

| Field | Meaning |
|---|---|
| `id`, `session_dir`, `workspace` | Identity and location |
| `binary`, `binary_version`, `schema` | What started it (R13) |
| `state`, `state_reason`, `since` | Lifecycle (R14) |
| `pid`, `nonce` | The current owning process; the nonce is per process, as in sidecars |
| `restarts`, `restart_window` | Crash-loop accounting |
| `profile` | Interactive, long, or workflow-step (§5.6) |
| `budgets` | Wall, cost and rate limits, plus spend so far |

**Heartbeats.** The run process runs a heartbeat thread that rewrites `<session>/heartbeat` every 10 s with the pid, nonce, a monotonic counter, the current step number and the time of the last step transition. Two signals, not one: the thread proves the process is alive; the step transition proves the loop is moving.

**Liveness verdicts.**

| Verdict | Condition | Action |
|---|---|---|
| `alive` | Heartbeat fresh, steps moving within `step_max_minutes` | None |
| `hung` | Heartbeat fresh, no step transition beyond the step watchdog, and not in a legitimate wait (backoff, human, budget) | Interrupt; after a grace period, kill the process group; then resume |
| `dead` | Heartbeat stale beyond 60 s and the pid is gone, or the nonce does not match the live lock holder | Resume |
| `zombie` | Pid alive but heartbeat stale and the live lock not held | Kill; resume |

The kernel `flock` behind `live.lock` is released when a process dies, so lock state is a reliable death signal that cannot go stale (it is never stolen, per the existing design).

**Restarting.** Resume means: spawn `overseer run --resume <session>` with the run's profile, which reacquires `live.lock`, replays the log, runs the side-effect reconciliation (R7), injects a resume briefing (R5) and continues. A restart budget (default 5 restarts per rolling hour, with exponential spacing) prevents crash loops; exceeding it parks the run with the last crash's evidence attached.

**Reboots.** On daemon start, the supervisor scans the registry. Every run in `running` or `waiting` whose process is gone is resumed (subject to R13 version policy). Runs in `paused` or `parked` stay put.

**Detached runs and frontends.** Today the TUI owns its agent on a worker thread, so closing the TUI ends the run. Under the supervisor, a run can be started detached (`overseer run --detach`, and workflows always detach). The TUI and web UI attach to a detached run over the daemon's control socket, rendering its events and sending input and interrupts. Interactive runs stay in-process by default, and can be detached on demand (`/detach`), which hands the run to the supervisor at a step boundary. Interactive runs still register in the run registry with the `interactive` profile; if their frontend dies, the supervisor does not resume them silently but notifies the owner and offers a one-step resume, because an interactive run may have been mid-conversation.

**Edge cases.**

- **Two daemons.** A pidfile plus `flock` on `~/.overseer/daemon.lock`; the second exits with a clear message.
- **The daemon itself crashes.** launchd/systemd restart it; runs keep running (they are separate processes); the restarted supervisor re-reads the registry and reattaches by pid and nonce.
- **The workspace is deleted or moved.** The run parks with `Environment` class and a named cause.
- **The owner resumes the run manually at the same time.** The live lock decides; the loser reports who holds it.
- **The run is open in a TUI when it dies.** The TUI shows the death and the supervisor's restart, then reattaches.
- **Laptop on battery at 5%.** Supervisor policy can pause long runs below a battery threshold (owner setting, R12).

::: gate
Chaos tests (R15, E3): `kill -9` of the run process at 200 random points across a scripted long run; the run completes with an event log identical in content (modulo restart events) to an uninterrupted control. Daemon `kill -9` with five runs in flight: all five survive. Simulated reboot (daemon and runs killed, daemon restarted): all `running` runs resume within 60 s. Crash loop: a run that dies on startup parks after the restart budget with evidence attached.
:::

#### R5 — Durable intent: the run journal

**What.** Invariant 12. The run's goal, plan, progress and next step live in a journal outside the context window, so a resume after any amount of compaction knows exactly where it stands.

**Why compaction is not enough.** The compaction summary is mechanical and lossy by design, and it must never be re-summarized (invariant 8). After the third or tenth compaction of a long run, the window holds a summary of the last stretch and a two-turn tail. The goal stated on hour 1 and the decisions taken on hour 20 are gone from view unless something durable carries them.

**Design.** The journal is part of the event log, so state remains a view (invariant 1). New event kinds:

| Event | Payload | Who writes it |
|---|---|---|
| `JournalGoal` | The goal statement and acceptance criteria, as the owner gave them | Engine, at run start; only the owner can amend |
| `JournalPlan` | The structured plan: items with id, title, state (`todo`, `doing`, `done`, `blocked`, `dropped`), dependencies, evidence links | Agent through the `plan` tool (promoted from deferred to resident in long runs) |
| `JournalNote` | A decision, finding or assumption, with its kind | Agent, through `plan op=note` |
| `JournalMilestone` | A plan item reached `done`, with evidence (verify result, test counts, commit) | Agent; the engine verifies evidence references exist |
| `JournalNext` | What the agent intends to do next, one paragraph | Agent, at each milestone and before any pause |

Materialized views (`<session>/journal/goal.md`, `plan.json`, `notes.md`) are written for humans and frontends; they are caches, rebuilt from events.

**Resume briefing.** After any resume, compaction or failover, the engine injects a deterministic briefing at the recency edge (below the cache boundary, provenance-wrapped as `journal`), built only from journal events:

```
Goal: <goal>  · Acceptance: <criteria>
Plan: 14 items — 9 done, 1 doing (#10 migrate auth module), 2 todo, 1 blocked (#12: needs owner)
Last milestone (2h ago): #9 billing module migrated; 412/412 tests pass (verify e8812)
Decisions: 6 (latest: keep the legacy adapter until #13)
Next: finish #10, then run the integration suite
In-doubt effects: none
```

**Agent contract.** The long-run prompt segment tells the agent to keep the plan current, and the engine enforces freshness: if no journal event lands in N steps (default 25) while the run is busy, a nudge asks for an update (this is a deterministic check like the stuck detector, not a model call).

**Edge cases.** An agent writes a plan that is wildly wrong; the goal audit (O9) catches drift. A plan item marked done without evidence is flagged in the briefing as `unverified`. Journal size is bounded: notes beyond 200 roll into an archived view, but goal, plan and the last 20 notes are always in the briefing.

::: gate
A scripted 40-compaction run resumes after each compaction and each injected crash with a briefing that names the correct goal, plan state and next step (asserted against the journal events). The briefing never exceeds 1,500 tokens. Journal events replay on old binaries as skipped unknown kinds without error (invariant 21).
:::

#### R6 — Crash-consistency audit of every durable write

**What.** Prove that every durable store survives a crash at any instruction.

**Inventory.** Event log, ledger, run registry, task sidecars, heartbeat, journal views, checkpoints and manifests, spill files, memory stores and their indexes, pending queue, skill ledger, rules file, inbox and outbox, worktrees, the web token.

**Policy per store.** For each: the write pattern (append, temp-file plus rename, in-place), `fsync` placement (file and, for renames, the directory), what a torn write looks like, and how readers recover. The rules:

- Appends that matter for correctness (event log, ledger, effect intents) are `fsync`ed at turn boundaries and before every classified side effect (R7).
- Replacements use temp-file, `fsync`, rename, directory `fsync`.
- Readers tolerate exactly the torn states the writer can produce, and nothing else.

::: gate
A table in the decision record covering every store, and a crash-injection test per store that kills the writer at each write step and asserts recovery. A power-loss simulation on a disk image (Linux CI-equivalent run locally) for the event log and ledger.
:::

#### R7 — The side-effect journal and idempotency

**What.** Invariant 13. Coming back after a crash is only safe if the run knows which side effects already happened.

**Design.** For every tool call whose classification is above `Read`:

1. Before dispatch, append and `fsync` an `EffectIntent { call_id, tool, class, target, idempotency_key, fingerprint }`.
2. After completion, append an `EffectOutcome { call_id, status, evidence }`.
3. On resume, reconciliation finds intents with no outcome: the **in-doubt set**.

**Resolution per tool family:**

| Family | How in-doubt is resolved |
|---|---|
| `write`, `edit` | Compare the file's current hash with the intended result (the pre-image is in the checkpoint, the intended content is in the call). Matches → outcome `applied`; matches the pre-image → `not_applied`; neither → `conflict`, surfaced |
| `bash` | Cannot be known in general. The model receives an explicit in-doubt notice ("this command may or may not have run; verify before repeating") and the command's text. Repeating an in-doubt command requires a verification step first, enforced as a gate rule. Model-chosen verification is best effort, not proof; commands where best effort is not good enough are declared never auto-repeatable (below) and take the unverifiable path |
| git operations through bash | Recognized patterns (commit, push, merge, tag) are resolved by inspecting refs and reflog |
| `memory` writes | Idempotent by content hash; replays are harmless and deduplicated |
| `task` spawns | Sidecar state resolves them (existing reaping) |
| External comms (gateway sends, MCP calls on ask lanes, `navigate`) | Never auto-replayed. Outbox items carry stable ids so a resend deduplicates where the channel supports it; otherwise the run parks the decision for the owner |
| Money and identity | Never auto-replayed; always parked |

**In-doubt policy is declared at intent time, not decided at resume.** Each effect family (and, through the policy compiler, each command pattern) carries one of four policies, recorded in the `EffectIntent` itself so resume never has to improvise:

| Policy | Meaning | Examples |
|---|---|---|
| `verify` | A read-back exists and the engine resolves it | `write`, `edit`, git ref operations, memory writes |
| `replay_safe` | Re-executing is harmless or deduplicated by an idempotency key the far side honours | `cargo test`, outbox sends on channels with id deduplication |
| `model_verify` | No engine read-back; the model must verify before any repeat (best effort) | Ordinary `bash` commands |
| `unverifiable` | No read-back and no safe replay | POSTs without idempotency keys, messages on channels without receipts, never-auto-repeat commands, ambiguous GUI actions, money and identity |

**The unverifiable path does not wedge the run.** An `unverifiable` in-doubt effect blocks only the plan item (or graph node) that depends on it: that item becomes `blocked{in_doubt}`, an approval-style request goes to the owner through the away policy (R8) asking "did this happen?", and the run continues with independent work. If nothing independent remains, the run parks with the question as its reason. The journal's role for these effects is detection and gating, never recovery; the document says so plainly because no engine can reconcile an effect that leaves no trace.

**Idempotency keys.** Generated as a hash of the session id and the call id only, never the attempt number, so every retry and every resumed attempt of the same effect carries the same key; passed to every integration that accepts one (outbox, webhooks, future connectors), so a retried send is recognized by the far side.

**Edge cases.** A command that is idempotent by nature (`cargo test`) is still in doubt, but its verification is free: run it again. The policy compiler (P2) lets the owner, and the project's AGENTS.md, mark command patterns as safe to replay or as **never auto-repeatable** (database migrations, deploy scripts); for the latter an in-doubt instance always parks for the owner. The in-doubt notice names the command's risk class so the model chooses a verification proportionate to it (premortem one, Chapter 22). A crash between intent and dispatch produces a false in-doubt; harmless, because resolution checks reality.

::: gate
For each family, a crash test between intent and dispatch, during execution, and between completion and outcome, asserting correct resolution. Zero double-sends of outbox items across 1,000 injected crashes.
:::

#### R8 — Away policy: asking when nobody is there

**What.** Invariant 14 applied to humans. A long run must keep moving when an approval is needed and the owner is away.

**Design.**

- **Non-blocking asks.** An Ask verdict no longer blocks the agent thread. The gate records an `ApprovalRequested { id, call, class, preview, deadline }`, returns a tool result telling the model the call is pending with an id, and the agent may continue with independent work (other plan items, other DAG nodes once O2 lands). When the decision arrives, an `ApprovalResolved` notice is delivered at the loop top like `SubagentDone`, and the agent can retry the call, which then passes the gate on the recorded grant.
- **Interactive runs keep the dialog.** When a frontend is attached and focused, the existing dialog appears as today. Non-blocking mode applies to detached runs and to interactive runs whose frontend has been unfocused beyond a threshold.
- **Deadlines and escalation.** Per run (from its profile or the policy compiler): an escalation chain (attached frontend → desktop notification → Telegram or another channel) with a delay per step, and an on-timeout action:

| On timeout | Behaviour |
|---|---|
| `deny_once` (default) | The call is denied with a reason; the run continues and records the gap in the journal |
| `park` | The run parks until the owner answers |
| `wait` | Wait indefinitely (explicit owner choice only) |

- **Late answers.** An approval that arrives after timeout is recorded and applies to the next identical request, never retroactively.
- **Full automation.** Under the owner's full-auto consent mode (P5), Asks do not arise for the domains it covers, so away policy is moot there; everything is still journaled (S4).

::: gate
A detached run hits an Ask with no frontend attached: the notification chain fires at the configured delays (tested with a fake channel), the run continues independent work meanwhile, `deny_once` applies at the deadline, and a later approval enables the next identical call. With a focused TUI attached, behaviour is unchanged from today.
:::

#### R9 — Progress ledger and long-window stall detection

**What.** The stuck detector catches local loops over a handful of steps. A long run can fail differently: busy for hours and getting nowhere.

**Design.** A progress ledger derived from events: journal milestones, plan items completed, verify outcomes, test pass counts when a verify command reports them, files changed, commits made. Windowed signals over hours: no milestone within the expected window (sized as below), monotone growth of failed verify attempts, oscillating diffs (the same files edited back and forth), rising cost per milestone.

**Window sizing.** Windows scale with the current plan item's own history (time already spent on it against the median time per completed item), not with the plan's total size, so a long plan cannot hide a stuck item for most of a day (premortem one, Chapter 22).

**Actions, escalating.** A nudge with the evidence → a fresh-context goal audit (O9) that reads the journal and recent diffs and returns a structured verdict → park with a report for the owner. Each step is journaled.

::: gate
Scripted scenarios for each stall signal trigger the right escalation step; a healthy long run with steady milestones never triggers.
:::

#### R10 — Pacing: rate budgets and shared provider limits

**What.** Invariant 15. A hundred-hour run cannot have a $5 total cap, and an uncapped one is reckless. Budgets need a rate form.

**Design.**

- **Rate budgets.** Per run: dollars per hour, dollars per day, and tokens per hour, alongside totals. Breaching a rate moves the run to `waiting{rate_budget}` until the window has room; breaching a total ends the run (as today) unless the profile says park.
- **Global budgets.** The supervisor enforces daily and monthly limits across all runs from the global ledger rollup (B2).
- **Alerts.** Notifications at 50%, 80% and 100% of any total or daily budget.
- **Shared provider limits.** Runs that share a key share its rate limit. The supervisor keeps a token bucket per key and model, adapted by additive-increase, multiplicative-decrease on 429s. Run processes do not ask permission per call (the daemon's tick loop is single-threaded, and a round trip per call would add latency to every request); instead each run holds a short lease of call capacity, renewed in the background, and reports 429s so the bucket shrinks for everyone. When the daemon is unreachable, a run falls back to local backoff alone. This stops five parallel runs from collectively hammering a limit and all failing.
- **Subscription quota windows.** Several providers bill through plans with rolling usage windows (the opencode Go subscription the eval rig uses is one; Claude and ChatGPT plans have the same shape). Overseer Life's usage readers already know each account's usage and reset time (Phase 0). Pacing consumes them: slow down before a window is exhausted, wait for the reset rather than failing on it, or route to another configured account (L4) when the owner allows.
- **Paused is not finished.** A run paused by a rate budget sends its own notification, distinct from completion, stating when it will resume on its own (premortem one, Chapter 22).
- **Forecasting.** The run's cost rate and remaining plan items project an estimated total, shown in the cockpit (X1) and used to warn early (B4).

::: gate
A simulated run under a $2/hour rate pauses and resumes at window boundaries without losing work. Five simulated runs sharing a fake key with a hard limit converge to the limit without any run ending on a 429.
:::

#### R11 — Resource guards: disk, logs, spills, worktrees

**What.** Invariant 16. A week of activity must not fill the disk or slow replay to a crawl.

**Design.**

- **Disk preflight and thresholds.** Before each step, check free space on the session and workspace volumes. Below a warning threshold, notify; below a hard threshold, pause the run with `Environment` class.
- **Replay snapshots.** Replaying a 100-hour log on every resume gets slow. At compaction boundaries the engine writes a snapshot event holding the derived state needed for replay (the compaction view, ledger totals, latches, journal state, read-dedup table), plus the hash of the event it covers. Resume starts from the latest valid snapshot and replays forward. The snapshot is a cache: if it fails verification, replay from the start (invariant 1 and the "everything else is a cache" principle).
- **Log segments.** `events.jsonl` rotates into numbered segments past a size threshold, with the hash chain continuing across segments and a manifest listing them. Old segments compress. Nothing is ever deleted by the engine; archival to cold storage is an owner command.
- **Spill collection.** Spill files no longer referenced by the current view, the journal or an artifact are compressed after a day and moved to an archive directory after a week.
- **Worktree collection.** Merged or discarded writer worktrees are removed (the branch stays until merged or explicitly pruned); the **[fix2]** change that keeps worktrees with gitignored output gets a size check so `target/` directories do not accumulate. `overseer gc` reports and reclaims.
- **Memory stores.** Already capped by INDEX limits and the dream pass (M3).

::: gate
A synthetic 100-hour log (generated at realistic event rates) resumes in under 2 s from its latest snapshot on the owner's M1. Disk-full injection pauses, and freeing space resumes. `overseer gc` on a session with 50 worktrees reclaims all merged ones.
:::

#### R12 — Time, sleep, power and networks

**What.** Laptops sleep, clocks jump, networks change. None of it should look like a failure.

**Design.**

- **Monotonic time for every timeout and budget**, wall time only for schedules and display. Rust's `Instant` on macOS does not advance during sleep **[infer: platform behaviour to confirm per OS in the decision record]**, which is the behaviour wanted for step watchdogs; the wall-clock budget (R3) is defined as awake time, and the decision record states this explicitly.
- **Sleep detection.** A wall-time jump that monotonic time did not see means the machine slept. On wake: refresh HTTP agents and MCP sessions, re-verify the driver for computer-use runs, and record a `Resumed{after_sleep}` event.
- **Keeping awake.** Optional and owner-controlled: while a long run is active and the machine is on mains power, hold a power assertion (macOS `caffeinate -i -w <pid>` as a supervised child, so it dies with the run). Never by default on battery.
- **Battery policy.** Pause long runs below a configurable charge level when unplugged.
- **Network changes.** DNS and connection failures after a network change are `Transient` and retried (R2); the HTTP agent is rebuilt after a detected change to drop dead pooled connections.

::: gate
Simulated sleep (clock-offset injection) does not trigger hung verdicts or budget expiry; real lid-close and reopen during a long run on the owner's Mac resumes without a restart or an error event.
:::

#### R13 — Upgrade-safe runs and schema evolution

**What.** Invariant 21, extended to live runs. The owner will upgrade Overseer while runs are in flight.

**Design.**

- **Versions recorded.** `SessionStart` records the binary version and an event schema version; the run registry records the binary path.
- **Compatibility rule.** A newer binary can resume a run if it can read every event kind in the log (old kinds never removed, new fields always defaulted). An older binary resumes a newer log only if every unknown kind is marked skippable; otherwise it refuses with a clear message.
- **Upgrade policy in the supervisor.** When a new binary is installed: runs at a safe boundary (between steps) are handed to the new binary on their next restart; the owner can choose `drain` (finish current runs on the old binary, start new runs on the new one) by keeping the previous binary installed side by side, which the installer supports (Z3).
- **Golden log corpus.** Every release adds representative logs to a test corpus; every later build must replay them all.

::: gate
The golden corpus replays on every build in the gate. An in-flight run survives a binary swap at a step boundary in a local test.
:::

#### R14 — Run lifecycle and control surface

**What.** One state machine for every run, and commands to see and steer them.

**States.** `created` → `running` ⇄ `waiting{kind}` (backoff, throttled, human, rate_budget, resource, dependency) → `paused` (owner) / `parked` (needs owner, with a reason) / `suspended` (process gone, resumable by the supervisor) → `completed` / `failed{reason}` / `abandoned` (owner). Every transition is a `RunState` event with a reason.

**Commands.**

| Command | Effect |
|---|---|
| `overseer runs` | List runs with state, age, spend, rate, progress and next step |
| `overseer attach <id>` | Open the TUI on a detached run |
| `overseer pause/resume <id>` | Pause at the next step boundary; resume |
| `overseer park <id> --reason` | Park deliberately |
| `overseer abandon <id>` | End with reason `abandoned`; worktrees kept for inspection |
| `overseer report <id>` | Generate the run report (goal, plan, milestones, decisions, effects, spend, verification) |
| `overseer logs <id> [--follow]` | Human-readable event stream |

The same operations exist in the web UI and over Telegram for approvals and status (X4).

::: gate
Property test over random sequences of commands and injected failures: the state machine never reaches an undefined state, and every terminal state has a reason.
:::

#### R15 — Fault injection built into the engine

**What.** Reliability is proven by breaking things on purpose, repeatedly and reproducibly.

**Design.** A fault layer compiled into test and debug builds only (a cargo feature, off in release), configured by a seeded spec: provider errors by class and rate, mid-stream cuts, tool timeouts, disk-full, `kill -9` at event N or at random with a seed, clock jumps, slow I/O, MCP server death, notifier hangs. The eval rig's chaos suite (E3) and every reliability gate in this chapter use it.

::: gate
Every fault kind is exercised by at least one test, and the fault layer is provably absent from release builds (symbol check in the build gate).
:::

#### R16 — Environment drift

**What.** Over days, the world under the run changes.

| Drift | Detection | Response |
|---|---|---|
| The owner edits files or moves HEAD in the run's workspace | Workspace fingerprint (HEAD, index, dirty set excluding the agent's own recorded effects) checked each step | Pause and ask, or adapt if the profile allows; never overwrite owner changes |
| Dependencies change (lockfile edits, toolchain updates) | Fingerprint of lockfiles and toolchain versions | Note in the journal; rerun the verify baseline |
| Provider key revoked or expired | `Auth` class | Park with the exact owner action |
| macOS revokes Accessibility or Screen Recording | Driver refusal codes | Park computer-use steps; continue the rest |
| An MCP server disappears or changes its tool list | Spawn failure or schema change | Mark tools unavailable with a hint; re-probe periodically (T7) |
| The repository is rebased upstream under a writer worktree | Merge-base check in the merge queue | Rebase-and-verify (O6) or park |
| Two top-level runs target the same working tree | A workspace lease (a lock in the repository's git directory, held by the writing run, released on death like `live.lock`) | The second run is offered a worktree of its own or waits; two runs never write the same tree (invariant 20) |

#### R17 — Process hygiene for very long lives

**What.** A process that runs for a hundred hours accumulates leaks that short tests never see.

**Design.**

- **Soak instrumentation.** RSS, thread count, open file descriptors and child processes sampled into the heartbeat; trends checked by the supervisor.
- **Rejuvenation.** Because a run's state is a view over its log, restarting the process is cheap and safe. The supervisor restarts a long run's process at a safe boundary every N hours (default 12) or when soak metrics cross a threshold. This turns slow leaks from a correctness risk into a non-event.
- **Child reaping.** Every spawned child is in a known process group and is reaped; the soak test asserts zero zombies.

::: gate
A 24-hour soak on the owner's Mac (local, no cloud) with a scripted workload shows flat RSS after rejuvenation, zero zombies, and stable descriptor counts. A 100-hour soak runs before the W3 wave closes.
:::

#### R18 — Optional remote executor

**What.** Sometimes the laptop has to close for a long time. A run should be able to continue elsewhere.

**Design.** Export a run bundle (session directory, journal, worktree state as a git bundle, run registry entry), start the same binary on a remote host, stream events back to the local log mirror, and re-import on completion. Credentials are never exported; the remote host uses its own. This is local-first by construction: the local log stays the source of truth once re-imported. One cloud session at a time, and only with the owner's go-ahead (standing rule).

::: note
R18 is deliberately late. Everything before it makes local runs survive; R18 only matters once they do.
:::

### 5.5 Configuration surface

All reliability behaviour is configurable through the policy compiler (P2), with defaults chosen so interactive use is unchanged.

| Key | Interactive default | Long-run default |
|---|---|---|
| `retry.transient.max_consecutive` | 6 | 12 |
| `retry.retry_after_ceiling` | 5 min | 30 min |
| `budget.max_wall_hours` | off | 120 |
| `budget.cost_per_hour` | off | owner-set (required) |
| `budget.cost_per_day` | off | owner-set (required) |
| `budget.on_total` | end | park |
| `step.max_minutes` | 30 | 30 |
| `away.ask_deadline` | n/a (dialog) | 30 min |
| `away.on_timeout` | n/a | `deny_once` |
| `away.chain` | — | frontend → desktop → Telegram |
| `journal.freshness_steps` | off | 25 |
| `supervisor.restart_budget` | — | 5 per hour |
| `supervisor.rejuvenate_hours` | — | 12 |
| `power.keep_awake_on_ac` | false | owner choice |
| `power.pause_below_battery` | — | 20% |

### 5.6 The long-run profile

`overseer run --profile long "…"` (and every workflow step) applies the long-run defaults above, detaches under the supervisor, promotes `plan` to a resident tool, turns on the journal contract and non-blocking asks, and requires the owner to set rate budgets explicitly the first time. That last point is deliberate: a hundred-hour run should never start with an implicit spending policy.

### 5.7 What could break

::: risk
**Auto-resume repeats a side effect.** The most damaging failure in this chapter. Mitigated by R7's intent-before-act records and per-family resolution; external, money and identity effects are never auto-replayed. Residual risk: a `bash` command with external effects (for example `curl -X POST`) that the classifier treats as internal. Mitigation: the sandbox denies network by default, so such a command fails unless the owner has opened egress; once the egress proxy lands (S1), egress-capable commands are classified ExternalComms.
:::

::: risk
**Retry storms make things worse.** Many runs retrying in lockstep against an overloaded provider. Mitigated by decorrelated jitter, per-key buckets with multiplicative decrease, and global retry budgets.
:::

::: risk
**The supervisor becomes a single point of failure.** If it wedges, nothing restarts. Mitigated by launchd/systemd restarting it, by runs being independent processes that keep working without it, and by a supervisor self-heartbeat checked by the run processes, which notify the owner if the supervisor has been silent too long.
:::

::: risk
**Snapshots disagree with the log.** A bug in snapshot derivation would make resumed runs subtly wrong. Mitigated by the snapshot storing the hash of the covered event, by periodic full-replay verification in tests, and by falling back to full replay on any mismatch.
:::

::: risk
**The journal becomes theatre.** The agent keeps a beautiful plan while doing something else. Mitigated by evidence-checked milestones, the progress ledger (R9) comparing claims against diffs and verify results, and fresh-context goal audits (O9).
:::

::: risk
**Non-blocking asks confuse the model.** A pending approval may lead the model to assume success. Mitigated by an explicit pending result wording tested on the rig, and by the gate refusing any dependent action that would assume the pending call ran.
:::

::: risk
**Keeping the laptop awake has costs.** Heat, battery, and a machine that never sleeps. Mitigated by making it opt-in, mains-only, and visible in the cockpit.
:::

### 5.8 Metrics

| Metric | Target |
|---|---|
| Runs ended by a transient-class failure | 0 |
| Long-horizon suite unattended completion (E2) | ≥ 95% of tasks pre-declared solvable (E2) |
| Mean time to automatic resume after a crash | ≤ 60 s |
| Resume-from-snapshot time for a 100-hour log | ≤ 2 s |
| Double-executed external effects across chaos runs | 0 |
| Owner interventions per 100 run-hours (excluding genuine approvals) | ≤ 1 |
