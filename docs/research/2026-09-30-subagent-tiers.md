# Subagent tiers: routing, budgets, verification (decision record, 2026-09-30)

This record supersedes §6 of `2026-09-29-pillars.md` wherever the two disagree. The tags are the ones defined in `2026-09-30-memory-v2.md`.

## Problems found in the code

- **Budgets don't compose (a bug).** `task` clones the parent's `AgentConfig` into each subagent, including the full `max_cost_usd`, and subagent spend never reaches the parent's ledger. A $5 session can spawn subagents that each spend up to $5. This breaks Invariant 3: the engine enforces budgets.
- **One model for everything.** Every subagent inherits the parent's model and effort, so a grep-and-summarise job runs on the flagship model.
- **No independent check of written work.** The lead reviews its own subagents' output in its own context.
- **No resume.** A subagent's trace persists, but it can't be continued. A naive `Agent::resume` on it would also rebuild the full core registry, handing a read subagent `write` and `task`.
- **Task ids collide across steps (a bug).** The spawn counter lives in a per-batch `ToolCtx` and restarts at 0 each step. The second `task` call in a session reuses `task-1`, so the log creation and the worktree branch both fail.
- **Dead background tasks wedge fan-out (a bug).** In-flight tasks are counted as `bg-*` dirs without `done.txt`, and background threads die with their process. After a crash, the dead dirs hold all 4 slots forever.

## Evidence

**When multi-agent pays**
- Anthropic's lead + subagents system beat a single agent by 90.2% on their research eval. On BrowseComp, token usage alone explained ~80% of performance variance [doc: Anthropic, "How we built our multi-agent research system", 2025].
- Multi-agent runs used ~15× the tokens of chat, and single agents ~4×. So cheap tiers must not starve subagents of tokens, and spend must be capped by the engine [infer].
- Subagents should return condensed 1–2K-token summaries, with detail left on the filesystem [doc: Anthropic, "Effective context engineering", 2025]. That matches `task`'s 8K-char digest plus trace path.
- Cognition says to share context and keep writes single-threaded [doc: Cognition, "Don't build multi-agents"]. Its follow-up endorses multiple agents that contribute intelligence while writes stay serialized [doc]. Overseer already follows this with read subagents, isolated-worktree writers, and serial merges.

**Why failures happen**
- MAST studied 1,600+ traces (κ = 0.88) and found 14 failure modes: specification ~44%, inter-agent misalignment ~32%, verification ~24% [paper: Cemri et al. 2025].
- Its interventions gave modest gains. A verifier catches one tractable slice; it does not fix bad briefs [paper].

**Routing**
- FrugalGPT cascades cut cost by up to 98% at parity on classification-style tasks [paper: Chen et al. 2023].
- RouteLLM cut cost by >85% on MT-Bench at ~95% of GPT-4 quality, but it needs a trained router [paper: Ong et al. 2025].
- No published study routes *agentic coding subtasks* by task type or tool set [unverified gap]. So we use deterministic rules plus the lead's explicit choice, and no learned router.
- Escalate-on-failure has broad support, and Overseer already uses it internally: small-model escalation to main, and an effort bump when stuck [infer].

**Verification**
- Intrinsic self-correction without external feedback degrades reasoning [paper: Huang et al. 2023].
- LLM judges prefer their own outputs [paper: Panickssery et al. 2024].
- Tool-grounded critique works [paper: CRITIC, Gou et al. 2023].
- Verifier-based selection beats outcome-only [paper: Lightman et al. 2023].
- On SWE-bench, executable checks dominate: Agentless uses reproduction and regression tests [paper: Xia et al. 2024], and CodeMonkeys uses test-based selection [paper: Ehrlich et al. 2025].
- So Overseer's verifier:
  - runs with a fresh context;
  - receives the task, the diff and a runnable check;
  - can execute commands;
  - must cite evidence;
  - returns a structured verdict;
  - never counts "unknown" as a pass.

**Advisor split**
- Aider's architect/editor split (R1 architect + Sonnet editor) scored 64.0% on the polyglot benchmark at 14× less cost than the prior o1 state of the art [vendor: aider.chat].
- Claude Code ships an Opus-plan / Sonnet-execute mode and an advisor model setting [doc].
- Evidence for a *cheap executor consulting a strong advisor* is thinner. It is plausible, so `consult` ships as an explicit, capped call the lead chooses to make [infer].

## Decisions

0. **Identity and liveness (fix).**
   - Task ids are session-monotonic.
   - Every subagent has a `task.json` sidecar recording id, mode, tier, model, worktree, cap, a per-process nonce, state and cost.
   - A running task left by a dead process is marked dead and settled, which frees its slot.
1. **Budgets compose (fix).**
   - A subagent's cost cap is the smaller of its requested/tier default and the parent's remaining budget.
   - A background spawn reserves its cap at launch and settles to actual spend on completion.
   - Settlement appends one record with the subagent's total cost to the *parent's* ledger, so resume accounting falls out of the existing ledger replay.
   - The parent's cost check counts settled subagent spend plus outstanding reservations, and `SubagentDone` carries the cost.
   - Steps stay per-agent.
2. **Tiers.** `task` takes `tier: light|standard|heavy`.
   - light = `--small-model`, else the provider's known light model, else the parent model;
   - standard = the parent model;
   - heavy = `--heavy-model`, else the provider's known heavy model, else the parent model.
   - Only models on the same adapter transport as the parent are eligible.
   - Effort: light = Low, heavy = High.
   - The provider's known light and heavy models are the cheapest and priciest rows of the parent's family in the profile table.
   - Defaults: read → light, write → standard, verify → standard, consult → heavy. For verify, the fresh context is the primary mitigation for self-preference; the lead can still ask for heavy. An explicit `tier` wins, except that write never runs light unless the lead sets it explicitly.
3. **Escalation.**
   - A read or consult subagent that ends in step-cap, stuck, empty or error retries **once**, one tier up, if budget remains.
   - The digest says `[escalated light→standard]`.
   - Writers do not auto-retry: the lead decides.
4. **Verify mode.**
   - `mode: verify` takes an optional `target` (a writer task id); without one, it checks the parent's working tree.
   - It gets the read tools plus sandboxed `bash`, a fresh context, and the standard tier by default.
   - It returns a JSON verdict `{verdict: pass|fail|partial, evidence[], issues[], ran[], confidence}`, parsed by the engine. An unparsable verdict counts as `unknown`, never as pass.
   - Tamper evidence: a `git status --porcelain` delta before and after is appended, and any file the verifier changed is flagged.
   - `verify: true` on a write task chains a verifier onto its worktree.
5. **Resume.**
   - `resume: <task-id>` plus a prompt continues that subagent's own log via `Agent::resume`, with the same mode and tier unless overridden.
   - A writer reuses its worktree, or errors if the worktree is gone.
   - A background resume counts against the in-flight cap.
6. **Consult mode.**
   - `mode: consult` makes one heavy-tier call with no tools, capped at 2,000 output tokens and the cost cap.
   - The lead's `prompt` carries the context.
   - No new resident tool, so the token budget holds.
7. **Fan-out.** The cap of 4 in flight stays. It becomes a config field instead of a constant.
8. **Anti-patterns we will not build:**
   - parallel writers on shared state;
   - a learned router;
   - self-critique as verification;
   - full-transcript handoffs;
   - automatic verification without an executable check.

## Deferred
- Cross-family verifier (a different provider from the writer): owner engine; gate is multi-provider credentials in one session.
- An effort-scaling prompt calibrated on the eval rig: owner eval; gate is the owner lifting the eval hold.
- Resume across a rebased parent (a stale writer worktree): owner engine; gate is the rebase-or-fail policy decision.
