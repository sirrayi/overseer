# OVERSEER — Founding Design

> The overseer. Under the hood, the engine. Everything.
> A coding agent harness built as a cognitive architecture: half human, half robot.

**Status:** Design v0.1 — not yet implemented
**Location:** ~/overseer
**Repo:** public on GitHub, MIT license. README leads with the L3 guardrail.
**Date:** 2026-08-11

---

## 0. The Thesis

Robot side is easy to build: an agent loop, a tool registry, a provider
adapter, a sandbox. Every harness on the market has one. Hermes, Codex CLI,
Claude Code, Aider. They differ in polish, not in kind.

What none of them do well is the human side. Hermes gets closest — skills,
memory, a curator — but it treats learning as a bolt-on feature. Overseer
treats it as the spine.

The name is the spec:

- **Overseer** — supervises its own work, its own learning, and eventually
  its own architecture. An agent that watches itself.
- **Engine** — the robot body is real. Deterministic, fast, sandboxed,
  verifiable. Not a toy bolted to a chatbot.
- **Everything** — one process, one home, one memory. Session, skills,
  patterns, facts, user model, tools, schedules. A single brain.

**The one design principle that overrides all others:**

> Perfection of recall where it matters, human-like relevance everywhere else.

Robots remember everything verbatim, forever. That is not a feature — it is
noise. Humans forget, and forgetting is a feature: it keeps the important
stuff salient. Overseer's memory must behave like a person who never
forgets the important stuff: perfect for critical state (git state, pending
work, approvals), relevance-filtered everywhere else (session history,
patterns, trivia).

---

## 1. Dual-Cognition Architecture

```
                        ┌──────────────────────────────┐
                        │        OVERSEER CORE         │
                        │   (the judge / the planner)  │
                        │  intent → plan → delegate    │
                        │         → verify → reflect   │
                        └───────┬────────────┬─────────┘
                                │            │
                 ┌──────────────▼───┐   ┌────▼─────────────┐
                 │   HUMAN SIDE     │   │   ROBOT SIDE     │
                 │   (cognition)    │   │   (execution)    │
                 │                  │   │                  │
                 │  • memory        │   │  • agent loop    │
                 │  • pattern recog │   │  • tool registry │
                 │  • reflection    │   │  • providers     │
                 │  • learning      │   │  • sandbox       │
                 │  • judgment      │   │  • approvals     │
                 └──────────────┬───┘   └────┬─────────────┘
                                │            │
                                └─────┬──────┘
                                      ▼
                        ┌──────────────────────────┐
                        │     OBSERVATION STREAM   │
                        │  everything that happens │
                        │  flows through here and  │
                        │  is written to memory    │
                        └──────────────────────────┘
```

**The rule of the split:** the human side decides *what* and *why*. The
robot side decides *how* and *when*. The human side never executes, the
robot side never decides. The core sits on top and arbitrates.

| Dimension | Human side | Robot side |
|---|---|---|
| Speed | slow, deliberate | fast, parallel |
| Recall | relevance-filtered | verbatim where flagged critical |
| Learning | reflection, pattern mining | tool retry logic, deterministic fixes |
| Failure | judgment, judgment, judgment | error types, exit codes, retries |
| Weakness | drift, hallucination | blindness to context |
| Guard | every learning is a proposal | every action is sandboxed |

---

## 2. The Recursive Learning Loop

Learning is not a feature. It is the loop the whole system runs on.

```
        ┌───────────────────────────────────────────────┐
        │                                               │
        ▼                                               │
   SENSE — observe everything: user msgs, tool calls,   │
           errors, outcomes, timing, latency            │
        │                                               │
        ▼                                               │
   THINK — interpret, plan, decide (with memory +       │
           patterns retrieved by salience)              │
        │                                               │
        ▼                                               │
   ACT — execute via robot side (sandboxed tools)       │
        │                                               │
        ▼                                               │
   REFLECT — what worked, what failed, what was         │
             repeated, what matters to the user         │
        │                                               │
        ▼                                               │
   COMPILE — distill into: facts, patterns, skills,     │
             user-model entries                         │
        │                                               │
        └───────────► back to SENSE at higher level     │
                      (the learner watches the learner) │
```

### The four learning levels

| Level | What it learns | Mechanism | Status |
|---|---|---|---|
| L0 | Tool use, task execution | agent loop + tool registry | built first |
| L1 | Skills from experience | pattern miner → skill compiler → curator | core of this project |
| L2 | How to learn | meta-stats: which skills helped, thresholds self-tune | later |
| L3 | Its own architecture | self-modification **as PRs, human-approved only** | far future, guarded |

**The L3 guardrail, written now, before we build anything:**

Self-modification is always a *proposal*. Overseer never patches itself
silently. It writes the change, explains the reasoning, opens a review
artifact, and waits. The human is the final overseer of the overseer. This
is non-negotiable and goes in the repo README on day one.

---

## 3. Memory Architecture

### 3.1 The three stores (like a person: episodic / semantic / procedural)

```
┌────────────────────────────────────────────────────────┐
│                    MEMORY CORE                        │
│                                                       │
│  EPISODIC (what happened)   — event log               │
│    sessions, tool calls, errors, messages, outcomes   │
│    append-only, cheap, raw truth                      │
│                                                       │
│  SEMANTIC (what I know)     — facts                   │
│    user prefs, env facts, project facts               │
│    each fact has: importance, confidence, sources,    │
│    access count, expiry                                │
│                                                       │
│  PROCEDURAL (how I do it)   — patterns + skills       │
│    mined motifs, compiled skills, curated playbooks   │
│    each with: trigger, body, confidence, use count    │
│                                                       │
│  USER MODEL (who you are)   — distilled prefs         │
│    weighted statements with evidence links            │
└────────────────────────────────────────────────────────┘
```

Backing store: SQLite. One file, FTS5 for search, WAL mode, zero daemons.
The entire brain of overseer is a single portable file — copy it, the agent
moves with you. (Same trick Hermes uses, and it works.)

### 3.2 The salience model (how things get remembered or forgotten)

Every memory item has a score. Retrieval and pruning both use it.

```
score = importance × recency_decay(t) × (1 + ln(access_count))
        × connectivity(links to other items) × outcome_bias
```

- **importance** — set at write time by the reflector, on a 1-10 scale.
  User corrections and stated preferences start at 9. Trivia at 2.
- **recency_decay** — exponential half-life. Different half-lives per kind:
  user prefs decay slowly (months), session trivia fast (days).
- **access_count** — every successful retrieval strengthens the memory.
  This is the human mechanism: memories you revisit survive.
- **connectivity** — items linked to other items (a fact cited by 4
  patterns) outrank islands. This is how *meaning* emerges from data.
- **outcome_bias** — things that led to success get a bump, things that led
  to repeated failure get flagged for review.

Forgetting is a scheduled job, not a crisis: every night, prune items below
a decaying floor. What survives is the person-shaped core.

### 3.3 Critical state is separate and never decays

Git state, pending tasks, open approvals, scheduled jobs, in-flight
sessions. This lives in its own table, verbatim, no salience, no pruning.
The robot side needs perfect state; the human side needs clean memory. This
is the "best of both worlds" made concrete.

---

## 4. Pattern Recognition (building through patterns)

### 4.1 What we mine

The observation stream is full of structure:

- **Tool sequences** — "when grep finds nothing, I search_files then
  fall back to web_search" — repeated 3-tool chains.
- **Error recoveries** — "tool X failed with error E → this 2-step fix
  recovered it" — the highest-value pattern class.
- **Question structures** — how the user asks things, what form the best
  answers take.
- **User rhythms** — when the user works, what they ask about, what they
  abandon. This is the beginning of *anticipation*.

### 4.2 The miner (offline job, runs after idle or on schedule)

```
1. CHUNK   — slice sessions into episodes with outcomes
2. EMBED   — feature vectors: tool names, error classes,
             intent hashes, duration, success/failure
3. CLUSTER — group similar episodes (n-gram motif mining on
             tool sequences + simple centroid clustering;
             no heavy ML dependencies to start)
4. FILTER  — keep clusters with n ≥ 3 and success ≥ threshold
5. GENERALIZE — turn cluster centroid into a skill draft:
             trigger conditions + ordered steps + known pitfalls
6. PROPOSE — human/overseer review → promoted to skill store
             (never auto-promoted at first; auto-promote only
             after a skill has been manually adopted 2+ times)
```

### 4.3 The compile rule

Patterns become skills. Skills become behavior. Behavior produces new
observations. That is the recursive loop closing: **the system literally
builds itself out of its own experience, and the more it's used, the more
it knows how to be used.**

---

## 5. The Robot Side (engine core)

### 5.1 Agent loop

The classic, done right:

```
loop:
  messages = build_messages(system_prompt, history, retrieved_memory)
  response = provider.complete(messages, tools)
  if response has tool_calls:
      for each call (parallel where safe):
          result = dispatch(call)        # registry lookup
          stream result to observation   # always logged
      append results
      continue
  else:
      final_answer = response
      reflect(final_answer)              # human side kicks in
      break
```

With: turn budget, token budget, interruption, compaction, and a max-loop
guard. Boring, correct, tested.

### 5.2 Tool registry

- Tools self-register: `name, description, JSON schema, handler, toolset`.
- Toolsets gate tools by context (CLI gets terminal, chat gets more).
- Every dispatch passes through: schema validation → approval check →
  sandbox check → execution → result size cap → observation log.
- MCP client support planned for phase 7 (external tool universe).

### 5.3 Core tool set (phase 1)

| Tool | Purpose |
|---|---|
| terminal | shell exec, cwd-pinned, timeout, background support |
| read_file / write_file / patch | file ops with syntax checks |
| search_files | ripgrep-backed content/file search |
| web_search / web_extract | research |
| todo | task list management |
| memory ops | explicit reads/writes into the stores |
| delegate_task | subagents with isolated context |

### 5.4 Provider layer

- OpenAI-compatible adapter first (works with Ollama Cloud, Kimi, OpenAI,
  any local server). One interface, N backends, config-selected.
- Fallback chain: primary → fallbacks on error/rate-limit/timeout.
- Streaming, tool calling, token accounting from day one.

### 5.5 Safety

- Shell-approval gating with an allowlist that learns (approved commands
  become allowed, with the user's blessing).
- Path sandbox: writes outside the workdir require approval.
- Prompt-injection scan on context files (pattern library like Hermes's
  threat patterns).
- Credential vault: keys in a git-ignored file, never in logs, never in
  memory stores.

---

## 6. Tech Stack

**Recommendation: Python 3.11+, uv for packaging.**

Why Python wins for overseer specifically:

1. The three pillars (learning, memory, patterns) are the hard parts, and
   the ecosystem for them is Python: embeddings, clustering, SQLite, NLP.
2. You are Python-native (sirkimi, Hermes configs). Speed of iteration
   beats engine purity for a project this ambitious.
3. Hermes sits in ~/.hermes/hermes-agent as 1.9 GB of reference
   implementation. Reading its source is free education; matching its
   language makes that frictionless.
4. Agent harnesses are I/O-bound, not CPU-bound. Python's perf is a
   non-issue for the loop itself. Hot paths (search, embeddings) go to C
   libraries.

Go remains the right call if we ever want a single static binary with no
runtime — noted as a future migration option, not a v1 constraint.

Key dependencies (deliberately lean):

| Need | Choice |
|---|---|
| runtime | Python 3.11 + uv |
| storage | sqlite3 stdlib, FTS5 |
| embeddings | local via fastembed (small model) or Ollama endpoint |
| clustering | custom n-gram + centroid, scikit-learn only if needed |
| HTTP | httpx |
| CLI | typer + rich — first-class: chat, model, tools, config, sessions, memory, skills, cron, doctor |
| testing | pytest, TDD from day one |
| adapter training (phase 8, optional) | mlx-lm local LoRA/DPO on Apple Silicon; hosted alt: OpenPipe / Together / Modal |

No web framework, no ORM, no message broker, no heavy AI framework.
Overseer is one process with one SQLite file.

---

## 7. Build Phases

Each phase ends with something that demonstrably works.

### Phase 0 — Scaffold (one session)
Repo at ~/overseer, uv init, git, pytest wired, README with the L3
guardrail written in. `overseer hello` prints and exits.

### Phase 1 — The Robot Body
Provider adapter (Ollama Cloud first), agent loop with tool calling,
registry with the 8 core tools, CLI entry, approval gating, observation
stream logging to SQLite. **Demo:** `overseer "check git status and tell
me what branch I'm on"` works end to end, logged.

### Phase 2 — Episodic Memory
Event log schema, session persistence, FTS5 search, session resume.
**Demo:** ask overseer what it did yesterday; it answers from its own log.

### Phase 3 — Reflection
Post-task reflection pass: extract candidate facts (user prefs, env facts),
salience scoring, critical-state separation, nightly prune job.
**Demo:** tell it once "I prefer short answers"; the preference survives
across sessions and shows up in retrieval.

### Phase 4 — Pattern Miner
Chunk/embed/cluster/filter/generalize pipeline. Skill draft proposals.
**Demo:** run 3 identical failure-recovery sessions; overseer proposes a
skill for that recovery on its own.

### Phase 5 — Retrieval Integration
Salience-scored memory injection into prompts. Facts, patterns, user model
all feed the system prompt. **Demo:** it stops repeating mistakes you
corrected once.

### Phase 6 — Recursive Closure
Skills feed back into behavior. Curator stats. L2 meta-learning start.
**Demo:** overseer's own learning curve visible: skills created, adopted,
refined, retired. It gets measurably better with use.

### Phase 7 — Flesh
Subagents, MCP, gateway messaging, dashboard, skins. By now the shape is
proven; this phase is surface.

### Phase 8 — The Adapter (fine-tuning pipeline)
Preference/correction recorder wired into reflection, dataset builder,
local MLX LoRA trainer with a validation gate (did the correction rate
drop?), adapter hot-swap, opt-in hosted path. The loop closes end to end:
overseer measurably gets better with use, in weights, not just in context.

---

## 10. The Adaptation Stack (how the model becomes yours)

Question asked during design: can a harness fine-tune models? Yes — in two
very different tiers. And the harness is the only place the data can come
from: the sessions ARE the dataset. Overseer's role is recorder -> dataset
builder -> scheduler -> adapter swapper.

### 10.1 Tier 1 — In-context adaptation (instant, free, every session)

90% of "gets better every session" comes from here, not from training:

- memory injection: salience-scored facts, patterns, prefs into the prompt
- few-shot: the user's own past successful episodes as demonstrations
- correction memory: past corrections injected so mistakes are not repeated
- user-model injection: persona, workflow, voice

Works with any model. Zero training. This is the default path.

### 10.2 Tier 2 — Weight fine-tuning (periodic, heavier, real)

- LoRA/QLoRA: small trainable adapters on a frozen base. On M1 16 GB with
  MLX, a 7-8B LoRA trains overnight; QLoRA on 14B quantized is tight but
  possible. Local, private, free (electricity only).
- Dataset is mined from the observation stream: correction pairs (prompt,
  wrong, right), preference pairs (accepted vs rejected), style samples,
  tool-call traces.
- DPO: with accept/reject pairs captured, train the adapter on the user's
  taste directly (mlx-lm ships DPO support).
- Hosted alternative (opt-in): OpenPipe / Together / Modal / Replicate —
  upload dataset, get an endpoint. Zero local compute, costs money, data
  leaves the machine.
- Pipeline: accumulate -> distill -> build dataset -> schedule job
  (nightly/weekly) -> train adapter -> swap in -> validate (correction
  rate dropped?).

### 10.3 Tier 3 — Model routing (cost scales with task, not everything)

Generalizes Chief's auto-reasoning + auto-vision instincts into the core:

- routine / personal / private -> local fine-tuned model: free, fast, private
- hard / novel / research -> frontier model (Kimi K3 class): smart, paid
- vision / media -> vision-capable model
- reasoning effort tier inside any model, by task complexity

Routing is a policy function: complexity + cost budget + privacy level.

### 10.4 What fine-tuning cannot fix (say it plainly)

- context limits, live facts, tool reliability: retrieval and routing fix
  these, not weights
- local adapters will not outsmart frontier models. The point is not raw
  IQ — it is being you-shaped, cheap, private, and always on.

### 10.5 Privacy default

Local MLX adapter path is the default. Session data never leaves the
machine unless hosted fine-tuning is explicitly enabled. The
preference-pair recorder is on by default — it is cheap and future-proof —
but can be disabled with one config flag.

---

## 11. The Friend Layer (limits we remove)

Everyone says "AI assistant". Most are chatbots with a memory bolt-on.
Here is the list of removable limits — overseer removes each one:

| Limit | Removal mechanism |
|---|---|
| no memory across sessions | episodic store + salience retrieval |
| cold start every session | warmup: relevant yesterday-context injected |
| repeats mistakes you corrected | correction memory (Tier 1) |
| generic voice | persona file + user-model voice injection |
| forgets what you're working on | project/workspace awareness |
| one-shot answers, no follow-through | persistent todo + background jobs |
| doesn't know your tools | MCP + custom tool registration |
| doesn't know your habits | rhythm mining -> anticipation |
| costs money on trivial tasks | model routing (Tier 3) |
| never gets better | adaptation stack (Section 10) |
| leaks your data | local-first processing, vault, opt-in cloud |

Friend qualities are concrete, not vibes:

- it remembers what you told it and why (evidence-linked user model)
- it notices patterns (weekly rhythms, recurring errors) and acts on them:
  reminders, warm-ups, prepared context
- it has a consistent self (persona), so it feels like the same being
  across sessions
- it is honest about what it cannot do (refuses to fake confidence)

*The robot never forgets what matters. The human never tires of learning.
The overseer never stops watching itself.*

---

## 8. Open Questions (decide as we go, not now)

1. Embeddings: local (fastembed) vs Ollama endpoint — privacy vs quality.
2. Skill format: SKILL.md-style frontmatter (proven) vs JSON — probably
   SKILL.md, it works.
3. How much of Hermes's skill/curator design to copy — the license is MIT,
   and it's installed locally. Steal ruthlessly, credit cleanly.
4. CLI UX: single command with subcommands (like hermes) vs TUI-first.
5. Naming internals: "cortex", "reflex", "mine" as module names vs boring
   names. (Boring names; the personality goes in the product, not the API.)

---

## 9. First Build Session Scope

Phase 0 + the skeleton of Phase 1:

1. `uv init overseer`, git init, pytest green, README with guardrail.
2. `overseer/` package: `engine.py` (loop), `tools/registry.py`, `provider.py`.
3. One provider (Ollama Cloud, OpenAI-compatible), one real tool (terminal),
   and a `--demo` mode that runs one loop turn against a local model.
4. Observation stream: every event JSON-logged to `~/.overseer/events.db`.

That is a small enough first bite to finish in one sitting and a big
enough skeleton to grow into everything above.

---

*The robot never forgets what matters. The human never tires of learning.
The overseer never stops watching itself.*
