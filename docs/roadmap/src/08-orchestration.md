## 6. Orchestration and Subagents

### 6.1 The bar

Multi-agent work pays when the work is parallel or exceeds one context, and it costs a lot of tokens: Anthropic's research system beat a single agent by about 90% on its internal evaluation while using roughly 15 times the tokens of a chat **[src]**. The failure science is just as clear. MAST's study of more than 1,600 traces attributes about 44% of multi-agent failures to specification, 32% to inter-agent misalignment and 24% to verification **[src]**. So the best orchestration layer is not the one with the most agents. It is the one that writes good briefs, shares exactly the right context, verifies independently, serializes writes, and spends tokens only where parallelism or context size pays for them.

### 6.2 Where we stand

The primitives are strong (Chapter 3.6): four modes, three tiers, worktree isolation, composing budgets, a verifier with tamper evidence, escalation, cancellation through a control tree, background digests, sidecar-based liveness. What is missing is structure above the single call: dependencies, shared artifacts, persistent agents, a merge path for parallel writers, limits on depth, and any way to coordinate more than "spawn and wait for a digest".

### 6.3 Target architecture

```
                        ┌───────────────────────────┐
                        │ LEAD (the run's agent)     │
                        │ plan · briefs · decisions  │
                        └──────┬──────────┬──────────┘
                     task graph│          │mailbox
          ┌────────────────────▼──┐   ┌───▼──────────────────────┐
          │ GRAPH EXECUTOR         │   │ PERSISTENT AGENTS         │
          │ nodes · deps · retries │   │ sidekick · reviewer ·     │
          │ fan-out · fan-in       │   │ specialists (identity,    │
          └───┬──────────┬─────────┘   │ memory scope, mailbox)    │
              │          │             └──────────────────────────┘
     readers ─┘          └─ writers (worktrees)
              │                    │
        ┌─────▼──────┐     ┌───────▼────────┐
        │ ARTIFACTS   │◀───│ MERGE QUEUE     │── rebase · build · verify · land
        │ typed, taint│     └────────────────┘
        └────────────┘
```

### 6.4 Workstreams

#### O1 — Depth and fan-out limits

**What.** Bound the shape of the agent tree, not just its cost.

**Design.** `max_depth` (default 2: lead → subagent → one level of grandchildren for writers only) and `max_total_agents` per run (default 16 live, 64 lifetime per hour), enforced in `task` spawn. Readers, verifiers and consults never get `task`. The supervisor (R4) holds a global cap across runs so a workflow fan-out cannot exhaust the machine.

::: gate
Spawning beyond depth or total limits returns a policy refusal naming the limit; a property test over random spawn trees never exceeds either.
:::

#### O2 — The task graph

**What.** Dependencies between subagent tasks, so the lead can express "B after A" and "C after all of A1…A8" in one plan instead of babysitting each step.

**Design.** `task` gains a graph form:

```json
{ "op": "graph",
  "nodes": [
    {"id": "inv",  "mode": "read",  "prompt": "Inventory every use of the old API", "outputs": ["inventory"]},
    {"id": "mig-a","mode": "write", "after": ["inv"], "inputs": ["inventory"], "prompt": "…module A…", "verify": true},
    {"id": "mig-b","mode": "write", "after": ["inv"], "inputs": ["inventory"], "prompt": "…module B…", "verify": true},
    {"id": "integ","mode": "verify","after": ["mig-a","mig-b"], "prompt": "Full suite on the merged tree"}
  ],
  "on_failure": "continue_independent" }
```

- **Semantics.** A node starts when all its `after` nodes have succeeded and its inputs exist. `on_failure` is `fail_fast` (cancel the rest), `continue_independent` (default: nodes not downstream of the failure continue), or `ask_lead` (deliver a notice and wait for the lead's decision).
- **Per-node retries.** Readers escalate as today; writers retry once with the failure's evidence in the brief only if the node says `retry: 1`.
- **Execution.** The graph executor lives in the parent run process and writes node states as events (`GraphNode{id, state, task_id}`) through the run's single log writer, so the event log stays totally ordered with one writer (no second thread appends to it directly). It is resumable: after a crash, completed nodes stay completed (their sidecars and artifacts prove it) and running nodes go through normal subagent reaping.
- **Budget.** The graph gets one cap carved from the parent; nodes carve from the graph. Unspent reservations return as nodes finish.
- **Validation.** Cycles, unknown ids, missing inputs and over-limit fan-out are rejected before any node starts (the same "validate the whole batch first" rule computer use now follows).

::: gate
Graph tests: diamond, fan-out of 8, fan-in, failure under each policy, crash mid-graph with resume, cycle rejection. The lead's context receives one notice per completed node plus one graph summary, never per-step chatter.
:::

#### O3 — Artifacts

**What.** A typed, durable way for agents to hand each other results larger and more structured than an 8,000-character digest.

**Design.** `<session>/artifacts/<name>@<version>` with a manifest: name, version, media type (markdown, JSON, patch, file list, table), producer task, content hash, size, taint (invariant 19), and a short description. Agents write artifacts through `task`'s output contract (`outputs: [...]`) or a small `artifact` op; consumers receive them as inputs, either inline when small (under 4,000 characters) or as a reference with a summary and a `read` path when large.

**Taint.** An artifact produced by a tainted agent is tainted; consuming it arms the consumer's untrusted latch, exactly like reading a web page.

**Caps.** Per artifact 1 MB, per run 256 MB, collected with the run (R11).

::: gate
An artifact round-trips between a reader and a writer; a tainted producer taints its consumer; size caps refuse cleanly; artifacts survive a crash and resume.
:::

#### O4 — The blackboard

**What.** A cheap shared scratchpad for a run's agents: discovered facts, open questions, claimed work, so parallel readers stop rediscovering the same things.

**Design.** An append-only, per-run list of short entries (≤ 400 characters) with author, kind (`fact`, `question`, `claim`, `warning`) and provenance, exposed as `task op=board` read and post. Entries are visible to agents started after them, injected as a compact section in their brief (capped at 1,500 characters, newest and highest-ranked first). Claims let a node announce "I am handling X" to avoid duplicate work.

::: risk
A blackboard is a prompt-injection relay: a tainted agent can post text that steers clean ones. Mitigation: entries carry taint; tainted entries are shown provenance-wrapped and arm the reader's untrusted latch; claims from tainted agents are advisory only.
:::

#### O5 — Detach and attach

**What.** A foreground `task` blocks the parent for its whole run. The lead should be able to send a running foreground task to the background when it turns out to be long, and to wait on a background task when it becomes urgent.

**Design.** `task action=detach id=…` from a steer or the frontend converts a foreground task to background at its next step boundary; `task action=wait id=… timeout=…` blocks on a background task with a deadline. Both are events.

#### O6 — The merge queue

**What.** Invariant 20. Parallel writers each produce a branch in their worktree; integration must be serialized and verified.

**Design.**

1. A writer finishing successfully enqueues its branch.
2. The queue takes one entry at a time: rebase onto the current integration base (the parent's branch), run the project verify command (and the writer's own checks), and fast-forward the base only if both pass.
3. On a rebase conflict, a resolution task is spawned (write mode, with both sides as context, verify required), or the entry parks for the lead under `ask_lead`.
4. On verify failure after a clean rebase, the entry goes back to its writer (if `retry` allows) with the failure, or parks.
5. Every step is an event; the queue is resumable after a crash.

**Edge cases.** The base moves because the owner commits (R16 detects it); the queue rebases subsequent entries onto the new base. Two writers touch the same file without conflict but break each other semantically; the verify step after each landing catches it, which is why every landing verifies.

::: gate
Eight parallel writers on overlapping files produce a linear history where every intermediate commit passes verify; an induced conflict produces a resolution task; a crash mid-queue resumes without losing or double-applying any entry.
:::

#### O7 — Persistent agents

**What.** Agents that outlive one task: a sidekick that keeps its context between handoffs, a standing reviewer, and the specialist agents Overseer Life needs (accountant, security officer, social manager).

**Design.**

- **Identity.** `~/.overseer/agents/<name>/agent.toml`: role prompt, tier, tool set, permission profile, memory scope, budgets, and schedule if any.
- **State.** Each persistent agent owns a session log like any run, resumed on each handoff; compaction and the journal keep it bounded.
- **Memory scope.** A persistent agent can have its own memory store (`agent:<name>`), mounted alongside user and project stores with its own write bars.
- **Mailbox.** Messages from the lead or workflows arrive as `MailboxMessage` events, processed at the loop top. Replies go back as artifacts or digests.
- **Lifecycle.** Persistent agents run under the supervisor (R4) and inherit every reliability property.

::: decision
Persistent agents never write to shared state directly. A persistent writer still works in a worktree and lands through the merge queue (O6). This keeps the refusal on parallel writers intact.
:::

#### O8 — Teams and roles: the lead and sidekick protocol

**What.** Make the lead and sidekick pattern (briefs and reports, never transcripts) a first-class protocol, because it is the cheapest form of multi-agent work that consistently pays **[src: Fusion, vendor claim of 35–60% lower cost]**.

**Design.** A `brief` schema (goal, context, constraints, done criteria, verification commands, runtime state notes) and a `report` schema (what was done, evidence paths, deviations, open questions). The lead's prompt contract and the sidekick's role prompt are tested on the rig. Roles are configuration: `lead`, `sidekick`, `reviewer`, `researcher`, `specialist:<name>`.

#### O9 — Goal audits

**What.** A periodic fresh-context check that the run is still doing what the owner asked.

**Design.** Triggered by the progress ledger (R9), by plan milestones, or every N hours on long runs. A verify-mode agent with read access gets the goal, the journal, the last diffs and recent verify outputs, and returns `{on_track | drifting | blocked, evidence[], recommendation}`. "Drifting" with evidence produces a notice to the lead and an entry in the journal; repeated drifting parks the run.

#### O10 — Best-of-N with executable selection

**What.** When a deterministic check exists, sampling several attempts and selecting by the check beats smarter control flow (Agentless and CodeMonkeys evidence) **[src]**.

**Design.** `task op=best_of n=3 mode=write verify=…`: N writers in parallel worktrees, each verified; the first that passes (or the best by a scored check) enters the merge queue; the others are discarded. Budget is N times one writer, so the policy compiler can restrict it to tasks marked as high value.

#### O11 — Cross-provider verification

**What.** LLM judges prefer their own outputs **[src]**. A verifier on a different model family is a stronger independent check.

**Design.** When credentials for a second family are present, verify mode prefers a model from another family at the same tier. Deferred today on multi-provider credentials in one session; unblocked by the model layer's multi-key support (L4).

#### O12 — Handoff as an alternative to compaction

See C9. For orchestration, handoff means a run can end its own context deliberately and continue as a fresh agent seeded with the journal and a brief, which is often cleaner than a tenth compaction.

#### O13 — Brief quality

**What.** Specification failures are the largest MAST class. Most bad subagent results start with a bad brief.

**Design.** A deterministic brief linter at spawn time: refuses briefs under a minimum length for write mode; warns when no done criteria or verification are given; warns when the brief references "the above" or "as discussed" (context the subagent does not have). The lead sees warnings as tool results. The linter's thresholds are tuned on the rig.

#### O14 — Seeing the tree

**What.** The owner must be able to see what every agent is doing.

**Design.** A graph view in the TUI and web UI (X1): nodes with state, tier, spend, step, last action; drill into any node's transcript; artifacts listed with producers. The data is already in events and sidecars.

#### O15 — Missions

**What.** Factory's missions idea: a feature graph where each node has a validator. On Overseer it is a graph (O2) whose nodes all require executable verification, plus a mission-level acceptance suite, plus a journal (R5) that tracks the graph.

**Design.** A `mission` is a saved graph template with acceptance criteria, startable as a long run. It is mostly composition of O2, O6, O9 and R5; the new part is the template format and its validation.

#### O16 — Cache-inheriting forks

**What.** A subagent started from a fork of the parent's conversation can reuse the parent's cached prefix, which makes "look at this from another angle with everything I know" nearly free in input cost **[src: Claude Code fork subagents]**.

**Design.** `task mode=fork`: the subagent's first request is the parent's exact prefix (system, tools, messages up to the fork point) plus the brief as a new user turn, so the provider's cache hits. Fork subagents are read-only by default and inherit the parent's taint. The tool set must be byte-identical for the cache to hit, so forks use the parent's registry with a policy filter applied at the gate rather than a different spec array.

::: gate
On Anthropic, a fork subagent's first call shows cache read tokens covering at least 90% of the parent's prefix.
:::

#### O17 — Messaging with deadlines

**What.** Request and response between agents that are both alive (lead and persistent agents), with deadlines, so coordination does not degrade into polling.

**Design.** `MailboxMessage { from, to, kind, body, reply_to, deadline }`; replies correlate by id; a missed deadline produces a timeout notice (invariant 14). Messages are bounded in size and count per hour.

#### O18 — Scheduling and machine resources

**What.** Agents compete for the machine as well as for budget: eight writers all running `cargo build` will thrash a laptop.

**Design.** A resource-aware scheduler in the graph executor and the supervisor: a cap on concurrent heavy commands (builds and test suites, recognized by pattern or declared in AGENTS.md), queued fairly; node priorities; and a CPU load check that delays heavy starts when the machine is saturated.

### 6.5 What could break

::: risk
**Token blow-up.** Multi-agent runs can cost an order of magnitude more. Mitigation: graph budgets carved from the parent, tier defaults that keep readers light, fork subagents that reuse cache, and E7 measuring cost per solved task with and without orchestration so the lead's prompt is tuned to orchestrate only when it pays.
:::

::: risk
**Merge-queue livelock.** Writers repeatedly conflicting with each other. Mitigation: the queue serializes and verifies after every landing; repeated conflicts on the same files trigger a re-plan notice to the lead instead of endless resolution tasks.
:::

::: risk
**Persistent agents accumulate stale context and stale memory.** Mitigation: compaction, journal, and the dream pass apply to agent memory scopes; an agent's context is reset to its journal on a configurable cadence.
:::

::: risk
**Orchestration hides failures.** A node fails quietly and the summary says "done". Mitigation: every node's verdict is in the graph summary, and the run cannot complete while any node is failed or unknown unless the lead explicitly drops it with a reason recorded in the journal.
:::

### 6.6 Metrics

| Metric | Target |
|---|---|
| Cost per solved task with orchestration ÷ without, on tasks marked parallel | ≤ 1.5 with a pass-rate gain, or orchestration is not used for that class |
| Merge-queue landings that later fail verify | 0 |
| Fork subagent cache read share of prefix | ≥ 90% |
| Graph runs resumed after crash without lost or duplicated nodes | 100% |
