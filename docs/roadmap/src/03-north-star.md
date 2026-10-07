# Part I — Foundations

## 1. The North Star: What "Alfred" Means

Alfred Pennyworth is not a tool Bruce Wayne operates. He anticipates, he remembers, he runs the house for weeks while Bruce is away, he patches wounds at three in the morning without being asked how, he tells the truth when it is unwelcome, and he never once leaks the secret. The harness we are building has to meet that standard, translated into engineering.

### 1.1 The Alfred standard, operationally

| Trait | What it means for the harness | Measured by |
|---|---|---|
| **Tireless** | Works for days without supervision. Pauses on failure, never dies of a transient. | Unattended survival rate on the long-horizon suite (E2); mean time between owner interventions |
| **Remembers** | Knows the owner, the projects, the conventions, what worked last time and what failed. Never asks the same thing twice. | Repeat-question rate; recall precision on the memory eval (E6) |
| **Anticipates** | Notices what needs doing (reminders, failing CI, expiring certificates, quota running out) and prepares it, but only interrupts when the value of interrupting is high. | Inbox precision (items acted on ÷ items raised); interruption rate |
| **Competent** | Solves the task correctly, at the lowest cost that does it well. | Pass rate and cost per solved task on the rig, against competitors on the same model |
| **Honest** | Reports what it actually verified. "Unknown" is never "done". Owns mistakes in the record. | Verified-claim rate; false-done rate in audits |
| **Discreet** | Secrets never enter context or logs; untrusted content cannot silently drive actions; every side effect is accountable. | Secret-leak tests; injection attack success rate (ASR) on the corpus; audit completeness |
| **Obedient to the right person** | Follows the owner's rules and preferences over anything it reads, explains every refusal, and can be stopped instantly from anywhere. | Policy-violation rate in red team; kill-switch latency from each frontend |
| **Unobtrusive** | Lean in tokens, memory, CPU and attention. | Resident tokens, RSS, startup time, binary size budgets (Z1) |
| **Self-improving** | Gets measurably better at the owner's work over time. | Learning-loop uplift: pass-rate delta between a cold and a warm memory store on repeated task families |

### 1.2 Horizons: what the harness must handle at each timescale

A harness that is excellent at five-minute tasks can be useless at five-day tasks, because different failure modes dominate at each scale. The plan addresses every horizon explicitly.

| Horizon | Typical job | Dominant failure modes | Pillars that carry it |
|---|---|---|---|
| Seconds | Answer a question, run a command | Latency, startup cost, wrong tool | Runtime (Z), tools (T) |
| Minutes | Fix a bug, write a feature | Wrong diagnosis, missing verification, loops | Verification (O), stuck detector, context (C) |
| Hours | Refactor a subsystem, research a topic | Context exhaustion, drift, cost, rate limits | Context (C), tokens (K), reliability (R1–R3) |
| Days | Migrate a codebase, run an audit across repos | Crashes, reboots, provider outages, lost intent, approvals while away | Reliability (R4–R10), policy (P4–P5), orchestration (O) |
| Weeks | Ongoing project management, a long research programme | Environment drift, upgrades, disk growth, stale memory | Reliability (R11–R16), memory (M), workflows (A) |
| Months | Standing automations: inbox triage, quota watching, renewals | Workflow versioning, trigger reliability, notification fatigue | Workflows (A), gateway, Life (F) |

### 1.3 Five journeys the finished harness must handle

Each journey is a test the whole system has to pass end to end. Chapter 17 turns them into evaluation scenarios.

#### Journey A: the 100-hour migration

The owner asks for a migration of a large codebase from one framework to another, with every test passing at the end, and leaves for a long weekend.

1. The lead agent reads AGENTS.md, memory and the repository, writes a plan artifact to its run journal, and decomposes the work into a graph: inventory, per-module migrations, integration, verification.
2. Writers run in isolated worktrees under a merge queue. Each merge is rebased, built and verified before it lands. Readers fan out to answer questions in parallel.
3. On hour 9 the provider returns 529s for forty minutes. The run backs off, fails over to the configured second model for read-only subtasks, and resumes on the primary when it recovers. The journal records it all.
4. On hour 31 the laptop reboots for an OS update. The supervisor restarts with the login session, finds the run in `running` with a stale heartbeat, verifies the last side effect in the journal completed, and resumes.
5. On hour 52 a writer wants to run a command outside its grants. Under the owner's away policy the request waits, notifies the owner's phone, and after the deadline is denied once; the agent routes around it and records the gap for the owner.
6. On hour 97 the merge queue is empty, the verifier passes, and the run ends with a report: what changed, what was verified and how, what remains, what it cost, every decision it took on the owner's behalf.

#### Journey B: the overnight research programme

The owner asks for a deep comparison of approaches to a technical problem, with sources. Readers fan out across topics, write findings as artifacts, a synthesis agent merges them, a fresh-context verifier checks every citation resolves and says what it claims, and the result lands in the inbox with confidence marked per claim.

#### Journey C: the standing automation

The owner defines a workflow: every morning at 07:00, check AI quota usage across accounts (Overseer Life connectors), check CI on open PRs, check certificate expiry on the owner's domains, and send one digest. It runs for months. Its definition is upgraded twice without losing state. When a connector breaks, the digest says so instead of silently omitting the section.

#### Journey D: the incident at 3 a.m.

A webhook reports a failing deploy. The gateway's triage classifies it as urgent; the EV gate decides it is worth waking the owner only if the agent cannot handle it. An agent run spawns with read access, diagnoses, prepares a rollback, and because rollback is an external side effect, asks. The owner approves from the phone. The run executes, verifies, and files a report.

#### Journey E: the life admin day

Through Overseer Life, a specialist agent notices a free trial converting to a paid plan tomorrow, drafts the cancellation, and asks; a second agent notices quota on one AI account is nearly gone and routes the day's Overseer runs to another account; a third notices the X follower count dropped sharply and flags it. Each runs under its own scoped permissions on the shared engine.

### 1.4 What Overseer will not become

Saying no is part of the design. These are permanent non-goals unless a decision record reverses one.

- **Not a chatbot with tools bolted on.** The engine is the product; conversation is one frontend.
- **Not bloated.** No tokio in core, no heavyweight frameworks, no Chromium in the harness binary. Every dependency is budgeted.
- **Not a learned router.** Model routing stays deterministic and explainable (subagent tiers decision, 2026-09-30).
- **Not parallel writers on shared state.** Writers isolate and serialize through a merge queue.
- **Not self-critique as verification.** Verification is executable or fresh-context.
- **Not a cloud service.** Local-first. Cloud is an optional executor, never the source of truth.
- **Not a scraper of other apps' secrets.** Overseer never refreshes another app's login, never writes another app's files.
- **Not opaque.** If the harness did it, the log says why.
