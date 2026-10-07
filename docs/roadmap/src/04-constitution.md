## 2. The Constitution: Principles and Invariants

Overseer already has nine invariants in `AGENTS.md`. They are why the engine is trustworthy, and none of them changes. This chapter restates them, adds the invariants that long-horizon operation and the new pillars require, and sets the design principles and refusals that decide close calls.

### 2.1 The existing invariants (unchanged)

| # | Invariant | Why it matters for the roadmap |
|---|---|---|
| 1 | Events are immutable; session state is a view over `events.jsonl`. | Every reliability, observability and replay feature in this plan is built on this. |
| 2 | Stable prompt prefixes: nothing volatile above the cache boundary. | The 92% cache target depends on it; policy compilation (P2) must respect it. |
| 3 | The engine enforces budgets, never the model. | Extends to wall-clock and rate budgets (R3, R10). |
| 4 | Tool results are budgeted: about 30K characters inline, then spill to file. | Extends to token-aware eviction (C1). |
| 5 | Read-before-edit is enforced by the harness, not the prompt. | Extends to hashline edits (T1). |
| 6 | Reasoning blocks are opaque: round-trip verbatim, never inspect or mutate. | Constrains reasoning shedding (C4) to dropping whole blocks where the provider allows it. |
| 7 | The raw provider `stop_reason` is preserved end to end. | Feeds the failure taxonomy (R1). |
| 8 | Compaction is a view over the event log, never a mutation; summaries are mechanical and never re-summarized; the recency tail starts on a model-response boundary. | Durable intent (R5) must not violate it: the journal is a separate artifact, not a summary of summaries. |
| 9 | Checkpoints live per user prompt; write and edit snapshot before first touch; `bash` side effects are not snapshotted. | The side-effect journal (R7) closes the `bash` blind spot as far as it can be closed. |

### 2.2 New invariants

Each new invariant is introduced by the workstream named, and from then on is enforced the same way the existing ones are: by tests, by review, and where possible by construction.

| # | Invariant | Introduced by |
|---|---|---|
| 10 | **A run ends only for a recorded reason.** Every `RunEnd` carries a reason from a closed set; "crashed" is not a reason, it is a state the supervisor resolves. | R1, R4 |
| 11 | **Transient failures pause; they never end a run.** A failure classified transient is retried under a bounded, journaled policy. | R1, R2 |
| 12 | **Intent is journaled.** The run's goal, plan, progress and next step live in a durable journal that is not part of the context window and not derived from compaction. | R5 |
| 13 | **Side effects are recorded before they happen.** Every classified side effect writes an intent record before execution and an outcome record after. Resume never re-executes an intent with an unknown outcome unless its family's declared in-doubt policy permits it (an idempotency key the far side honours, or a declared safe replay) or a verification established the outcome. Effects with no observable read-back are detected and gated, never guessed. | R7 |
| 14 | **Every wait has a deadline and an escalation.** No component waits forever, including on a human. Waiting on a human has an owner-configured policy. | R3, R8 |
| 15 | **Every budget has a rate form.** Long runs are bounded by cost per hour and per day as well as totals, and breaching a rate pauses the run rather than killing it. | R10 |
| 16 | **Resource growth is bounded.** Logs, spills, worktrees, caches and memory stores have caps and collection policies. | R11 |
| 17 | **Policy has one compiler.** Every instruction and permission source compiles through one ordered, provenance-preserving pipeline; no subsystem consults raw rules on its own. | P2 |
| 18 | **Every decision is explainable.** Permission verdicts, compaction cuts, evictions, retries, failovers and routing choices are logged with their cause. | P11, B1 |
| 19 | **Taint flows with data.** Content derived from untrusted input carries its taint through subagents, artifacts, workflow steps and memory. | A6, O3, S3 |
| 20 | **Writers never share a working tree.** Parallel writers isolate; integration is serialized through a merge queue with verification. | O6 |
| 21 | **Old logs always replay.** New event fields default; new event kinds are skipped by old readers; every schema change has a migration test. | R13 |
| 22 | **Leanness is enforced by the build.** Binary size, RSS, startup and resident tokens have budgets that fail the gate when exceeded. | Z1 |
| 23 | **Claims are measured.** A capability is not called better until the eval rig shows it, with paired statistics, on the same model. | E1 |
| 24 | **The owner can always stop everything.** A kill switch reaches every running agent and workflow from every frontend within a bounded latency. | S10 |
| 25 | **Secrets the harness holds (brokered credentials, environment and keychain values) never enter model context, logs or child environments.** Already true in practice; now an invariant every new subsystem must test. Secrets the owner pastes into a conversation do enter context; S7 detects them after the fact and drives redaction and rotation. | S2, S7 |

### 2.3 Design principles

These decide the close calls that invariants do not cover.

::: principle
**Earn every token, byte and millisecond.** A subsystem justifies its resident tokens, binary size and latency with a measured benefit. Minimal scaffolds capture more of each model generation's improvement; complexity that encodes assumptions about model weakness rots.
:::

::: principle
**Correct first, then durable, then fast, then clever.** The order of work inside every pillar.
:::

::: principle
**Mechanism in the engine, policy in configuration.** The engine provides retries, gates, budgets and graphs; the owner's configuration decides how they are used.
:::

::: principle
**Deterministic where possible, model-driven where necessary.** Routing, compaction, triage rules, eviction and retry are deterministic. Model calls are reserved for judgment, and every model call is ledgered and bounded.
:::

::: principle
**Fail safe, fail loud, fail recoverable.** An uncertain state refuses rather than guesses, says so visibly, and leaves the system resumable.
:::

::: principle
**One way to do each thing.** One compiler for policy, one journal for intent, one ledger for spend, one log for events. Parallel mechanisms drift.
:::

::: principle
**The event log is the source of truth; everything else is a cache.** Indexes, journals' derived views, dashboards and memory indexes must be rebuildable from durable sources.
:::

::: principle
**The harness enforces; the prompt informs.** Prose rules shape what the model attempts. Only engine enforcement decides outcomes.
:::

::: principle
**Steal ideas, never dependencies blindly.** Borrow the best idea from every harness with attribution; take a dependency only after it passes the budget and `cargo deny`.
:::

::: principle
**Every deferral is written down.** Anything left out carries a `DEFERRED(owner)` marker with its gate, and appears in the PR and merge commit.
:::

### 2.4 Refusals

The things we will not build, and why. Reversing one needs a decision record.

| Refusal | Reason |
|---|---|
| Parallel writers on a shared tree | Corrupts state; Cognition's and Anthropic's evidence agrees writes should serialize |
| Self-critique as verification | Ungrounded self-correction degrades results (Huang et al. 2023) |
| A learned router | Needs training data we do not have, and is unexplainable |
| Full-transcript handoffs between agents | Briefs and results, not transcripts; transcripts waste tokens and leak taint |
| Vector search in the memory core | Size, poisoning surface, and BM25F + activation already meets the bar (memory v2/v3 records) |
| LLM query rewriting before every recall | Adds a model call to every turn |
| Automatic verification without an executable check | A verifier with nothing to run is self-critique by another name |
| Tokio or an async runtime in core | Leanness; the sync design with threads is sufficient and simpler to reason about |
| Reading or refreshing other apps' credentials | Standing Overseer Life rule |
| Silent downgrade of a pinned sandbox runtime | Already a rule for `--runtime`; generalizes to every security setting |

### 2.5 How decisions get made

1. A workstream that changes architecture starts with a decision record under `docs/research/YYYY-MM-DD-<slug>.md`, in the existing format: problem measured, evidence with tags, decisions, deferred items with owner and gate.
2. The record is reviewed before code starts. Disagreements between the record and this roadmap resolve in favour of the record, and the roadmap gets an erratum in its next edition.
3. Implementation follows the standing review rule: line-by-line review, then independent adversarial review, then the PR.
4. Every PR ends with its Deferred section; every merge commit carries it.
5. Owner decisions are recorded verbatim with their date, as in the Contract section of Part 0.
