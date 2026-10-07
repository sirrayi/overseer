## 12. Automation: Durable Workflows, Schedules and Triggers

Workstreams in this chapter use the letter **A** (automation), so they never collide with wave numbers (W0–W10).

### 12.1 The bar

The playbook's Alfred architecture puts a thin, always-on gateway in front of ephemeral, sandboxed, budgeted agent runs, with proactivity producing inbox items rather than interruptions, and asks for "durable execution or checkpoint plus idempotency discipline; budgets and circuit breakers; a dead-man's switch; a kill switch reachable from every frontend" **[src: playbook Ch. 11 §8]**. Harnesses with automation today offer cron plus messaging **[src: Hermes]** or scheduled agents. Nobody surveyed offers durable, versioned, compensating workflows for a personal agent. That is the gap to own.

### 12.2 Where we stand

The gateway is a solid single-shot pipeline (Chapter 3.9): triggers, deduplication, rule-based triage, untrusted downgrade, an EV gate for attention, a capped spawner, durable inbox and outbox. One event produces at most one run. There are no chains, conditions, retries, fan-out, compensation, schedules beyond 5-field cron with a fixed time-zone offset, or versioned definitions.

### 12.3 Target architecture

```
 triggers (cron · calendar · file · webhook · channel · connector · manual)
        │
        ▼
 ┌────────────────────────────── GATEWAY ──────────────────────────────┐
 │ trigger bus → dedup → triage → EV gate                              │
 │                    │                                                │
 │                    ▼                                                │
 │ WORKFLOW EXECUTOR (durable, event-sourced per instance)             │
 │   definition@version → instance → steps: agent · tool · wait ·      │
 │   approval · branch · map · sub-workflow · notify · compensate      │
 │   retries · timeouts · idempotency keys · taint propagation         │
 │                    │                                                │
 │ SUPERVISOR (R4) ◀──┘ runs agent steps as supervised long runs        │
 │ INBOX v2 · OUTBOX · NOTIFIER                                         │
 └─────────────────────────────────────────────────────────────────────┘
```

A workflow instance is to automation what a run is to an agent: an append-only log of what happened, from which its state is a view, resumable after anything.

### 12.4 Workstreams

#### A1 — The workflow definition

**What.** A versioned, human-readable definition of steps and their dependencies.

**Design.** TOML files under `~/.overseer/workflows/<name>.toml` (and project-level ones, trusted by hash like project MCP):

```toml
name    = "morning-digest"
version = 3
trigger = { cron = "0 7 * * 1-5", tz = "Europe/London", missed = "run_once" }
budget  = { cost_per_run = 0.50, wall_minutes = 30 }

[[step]]
id   = "quota"
kind = "tool"
tool = "life.connector"
args = { sources = ["claude", "codex", "cursor", "devin", "opencode"] }

[[step]]
id    = "ci"
kind  = "agent"
brief = "Check CI status on my open PRs in sirrayi/overseer; one line each."
tier  = "light"
mode  = "read"

[[step]]
id    = "digest"
kind  = "agent"
after = ["quota", "ci"]
brief = "Write the morning digest from the inputs. Name any source that failed."
inputs = ["quota", "ci"]

[[step]]
id      = "send"
kind    = "notify"
after   = ["digest"]
channel = "telegram"
body    = "${digest.output}"
```

Interpolation is a tiny, deterministic expression language (field access, defaults, string formatting), never a model call and never code execution.

::: gate
A parser with precise errors (line, step, field), a validator that rejects cycles, unknown references, missing budgets and unknown step kinds before any instance starts, and a schema document generated from the code.
:::

#### A2 — The durable executor

**What.** Execute workflow instances so that any crash, reboot or upgrade resumes them exactly where they were.

**Design.**

- **Event-sourced instances.** `~/.overseer/workflows/instances/<id>/events.jsonl` with the same hash-chained log machinery as sessions: `InstanceStarted{definition, version, trigger_payload}`, `StepScheduled`, `StepStarted{attempt}`, `StepCompleted{output_ref}`, `StepFailed{class}`, `StepCompensated`, `InstanceCompleted{reason}`.
- **Execution semantics.** Steps run at least once; effects inside them are exactly-once through R7's intent records and idempotency keys (derived from instance id and step id, never the attempt, so retries share a key). A step whose outcome is unknown after a crash is reconciled before being retried, with the same per-family rules as R7.
- **Outputs.** Step outputs are artifacts (O3), referenced by id, carrying taint.
- **The daemon holds no credentials.** Tool steps that need credentials (a Life connector, an authenticated MCP server) run in a spawned step process that uses the credential broker, exactly as agent runs do; the executor only schedules and records. This keeps the gateway's thin, credential-free design (playbook Alfred architecture).
- **Agent steps** run as supervised long runs (R4) with the step's brief, tier, budgets and the workflow's policy overlay; their completion is an event delivered to the executor. Unlike today's gateway spawns (`overseer exec --bare`, whose session is throwaway in the temp directory), agent steps keep a durable session under the instance directory so their journal, effects and logs survive for resume and audit; whether they read memory is part of the workflow's policy.
- **Concurrency.** A global cap on concurrent instances and agent steps (extending today's `max_concurrent`), queued fairly, with per-workflow limits ("never two morning digests at once").

::: gate
Crash injection at every step transition across a 10-step workflow with agent, tool, wait and approval steps: every instance completes with each effect exactly once. Daemon restart mid-instance resumes it. 1,000 instances of a trivial workflow complete with bounded memory.
:::

#### A3 — Step kinds

| Kind | Behaviour |
|---|---|
| `agent` | Supervised run with brief, mode, tier, budgets, inputs; output is its final report or declared artifacts |
| `tool` | One deterministic tool or connector call (a sandboxed command, a Life connector fetch, an MCP tool on a read lane) |
| `wait` | Duration, until a time, or until an event (with a deadline) |
| `approval` | An inbox item with a preview and a deadline; outcome `approved`, `rejected` or `expired` |
| `branch` | Choose the next steps by a deterministic condition over earlier outputs |
| `map` | Fan out a step over a list (with a concurrency cap) and fan in |
| `workflow` | Call another workflow as a sub-instance |
| `notify` | Send through a channel via the outbox, idempotent |
| `compensate` | Declared undo for an earlier step, run on failure (A4) |

#### A4 — Retries, timeouts and compensation

**What.** Failure handling per step, and sagas for multi-step changes.

**Design.** Each step declares `retry = { max, backoff, on = ["Transient", "Throttled"] }` (classes from R1) and `timeout`. When a step fails terminally, the instance runs the `compensate` steps of completed steps in reverse order (for example, close the PR that an earlier step opened; delete the draft), then ends `failed` with a report. Compensation steps are themselves retried and journaled. Steps with no possible compensation (a sent message) are marked `irreversible` in the definition, and the validator warns when an irreversible step precedes a fallible one without an approval between them.

#### A5 — Schedules and time

**What.** Schedules that are right in every time zone, across daylight saving changes, after sleep, and after missed windows.

**Design.**

- **Real time zones.** The gateway's attention gate uses a fixed offset (`OVERSEER_TZ_OFFSET_MIN`), which is wrong for half the year in any zone with daylight saving. Schedules carry an IANA zone name, resolved against the system's zoneinfo database. Ambiguous and skipped local times at transitions have defined behaviour (run once at the first occurrence; run at the next valid minute).
- **Missed runs.** Per trigger: `skip`, `run_once` (one catch-up run on wake), or `run_all` (bounded).
- **Calendars.** Business days, quiet hours, and owner-defined blackout dates.
- **Jitter.** Optional, to avoid many workflows firing on the same second against the same provider.

::: gate
Table tests across spring-forward and fall-back transitions in at least three zones; a simulated eight-hour sleep across a scheduled time produces exactly the configured missed-run behaviour.
:::

#### A6 — Taint through workflows

**What.** Invariant 19. A workflow that reads an email in step 1 and sends a message in step 5 is the lethal trifecta spread across steps.

**Design.** Each step output carries the taint of its inputs and of anything the step read. A step consuming tainted input runs with its untrusted latch armed from the start; the policy overlay can require approval for external effects downstream of tainted steps. The gateway's existing untrusted floor (Act downgraded to draft for untrusted origins) generalizes to: no step downstream of untrusted input performs an external effect without approval, unless the owner's consent configuration for that workflow says otherwise, recorded explicitly.

#### A7 — Inbox, version 2

**What.** The inbox is the proactive frontend: triage → item → approve, reject or snooze → receipt **[src: playbook, Agent Inbox]**.

**Design.** Items with typed actions (approve, reject, edit then approve, snooze until, open run), grouping into digests, deadlines with the away policy (R8), receipts after execution, and availability in every frontend (TUI, web, Telegram, later the Life app). Items never expire silently: expiry is an outcome recorded in the instance.

#### A8 — The workflow library

**What.** Built-in templates that make the engine useful on day one, each a tested definition:

| Template | What it does |
|---|---|
| `morning-digest` | Quota, CI, calendar-free status digest (the example above) |
| `memory-dream` | Nightly dream pass (M3) |
| `ci-watch` | Watch open PRs; on failure, diagnose with a read agent and post a summary to the inbox |
| `dependency-review` | Weekly: list outdated dependencies, run `cargo deny`, draft an update PR in a worktree for approval |
| `quota-guard` | Hourly: warn when an AI account crosses a usage threshold; suggest routing changes |
| `cert-expiry` | Daily: check certificate expiry on owner domains |
| `session-gc` | Weekly: `overseer gc` report and approved cleanup (R11) |
| `long-run-babysitter` | For any long run: hourly health summary to the owner, only if something changed |

#### A9 — Versioning and migration

**What.** Changing a workflow definition must not corrupt running instances.

**Design.** Instances pin the definition version they started with. A new version applies to new instances. Running instances finish on their version by default; the owner can migrate an instance at a step boundary when the new version declares a mapping of step ids. Old definitions are retained while any instance uses them.

Two further rules, because workflows run for months and the engine will be upgraded under them:

- **Mapping is not enough when meaning changes.** Migration is refused when a mapped step keeps its id but changes its kind, effect class, irreversibility or compensation, unless the owner confirms that specific change.
- **Engine compatibility is declared and checked.** Each definition declares the engine range it was written for; the instance log records the engine version at every step; a binary outside the range refuses to resume the instance and the supervisor keeps the previous binary for it (the R13 drain policy, applied to workflows).

#### A10 — Testing workflows

**What.** A workflow is code; it needs tests.

**Design.** `overseer workflow test <name>` runs a definition against fakes (recorded tool outputs, stub agents returning fixtures, a fake clock), asserting the path taken and the outputs. `overseer workflow replay <instance> --definition <file>` replays a past instance's trigger and recorded step outputs against a new definition to show what would change. `overseer workflow dry-run` validates and prints the plan without effects.

#### A11 — Triggers, version 2

**What.** Better triggers than polling.

| Trigger | Design |
|---|---|
| File events | Native notifications (FSEvents on macOS, inotify on Linux) behind a feature gate, with the existing mtime polling as fallback |
| Webhooks | HMAC signature verification per source, replay protection (have, improved by **[fix2]**), schema-checked payloads |
| Channels | Telegram (have); more channels as the outbox gains them |
| Connector events | Overseer Life connectors emit change events (follower delta, quota threshold, renewal soon) |
| Run and workflow events | A run completing, a workflow failing, a budget threshold: workflows can trigger on other workflows |
| Manual | `overseer workflow run <name> [--input …]` and the inbox |

#### A12 — Seeing automations

**What.** `overseer workflows` (definitions, schedules, last and next runs, failure counts) and `overseer instances` (state, current step, history), with the same views in the web UI and a daily health line in the digest.

**Failure paths that cannot fail with what they report on** (premortem two, Chapter 22):

- A failure of the digest workflow itself alerts through a channel independent of the digest: the desktop notifier and a cockpit badge.
- Every scheduled workflow has a dead-man's switch ("no successful completion within its expected window") with a primary and a fallback channel.
- Channel credentials (bot tokens, webhook secrets) get their own daily health check, which reports through the other channel when one fails.

### 12.5 What could break

::: risk
**Automations that nobody watches fail silently for weeks.** Mitigation: every instance ends with a recorded reason; failures produce inbox items; the digest reports failed and skipped instances; a dead-man's switch alerts when a scheduled workflow has not completed within its expected window.
:::

::: risk
**A buggy workflow spams the owner or spends money in a loop.** Mitigation: per-workflow budgets and rate limits, per-channel send caps in the outbox, the kill switch, and the validator's warnings.
:::

::: risk
**Daylight saving and sleep cause double or missed runs.** Mitigation: A5's real time zones, missed-run policies and tests at transitions.
:::

::: risk
**A trigger becomes a remote-control channel for an attacker.** A webhook or Telegram message that starts a workflow is untrusted input. Mitigation: signature verification, the untrusted floor, taint propagation (A6), and approval before external effects downstream.
:::

### 12.6 Metrics

| Metric | Target |
|---|---|
| Instances resumed correctly after daemon crash or reboot | 100% |
| Duplicate effects across chaos tests | 0 |
| Scheduled workflows completing within their window over 30 days | ≥ 99% (excluding owner pauses) |
| Silent failures (failure without an inbox item or digest line) | 0 |
