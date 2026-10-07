## 7. Memory

### 7.1 The bar

Production harnesses converged on memory as a filesystem the agent edits: an index of pointers, topic files, progressive disclosure, git versioning, and a periodic cheap consolidation pass **[src: playbook thesis 5]**. Procedural memory (ACE-style append-only playbooks) shows the highest upside **[src]**. Memory writes are side effects and must be gated, because poisoning attacks reach very high write success in research (MINJA about 70%, AgentPoison at least 80%) **[src]**. Multi-hop conflict resolution remains unsolved for every system measured (MemoryAgentBench at or below 7%) **[src]**.

Overseer's memory already meets most of the converged bar and exceeds it on safety. The roadmap's job is to finish v3, then go where nobody else is: memory of outcomes, run journals feeding long-term learning, and measured uplift.

### 7.2 Where we stand

See Chapter 3.7. Memory v2 and v3 H1 are complete and red-teamed, with **[fix2]** closing every review finding except fix2-memory finding M8, which W0 resolves with the owner's option (a). Open: H2 (history search, applied learned skills) and H3 (dream pass, mounted repos, sync), plus the `every:` reminder form and the SKILL and Merge/Contradict operations that parse but do not apply.

### 7.3 Workstreams

#### M1 — History search (`memory op=history`)

**What.** v3 §4: search raw past sessions with Overseer's own BM25F engine, no model call.

**Design.** As specified in the memory v3 record: one document per user turn, fields weighted (user text 3, first prompt 2, assistant text 1), tool result bodies never indexed, RRF over BM25F, recency and same-project, four shapes (query, around, session, recent), output ≤ 8,000 characters, redacted, provenance-wrapped as `history`, threat-flagged. Built lazily, refreshed by modification time.

**Long-run addition.** Journal events (R5) are indexed too, so "what did I decide about the auth adapter last week" finds the decision note directly.

::: gate
The v3 record's verification list for §4, plus a test that tool result bodies never appear in any history output, plus a latency bound (≤ 50 ms over 400 sessions on the owner's M1, release build).
:::

#### M2 — Applied learned skills

**What.** v3 §5: the review's SKILL and PATCH-SKILL operations actually create and patch skills, with probation, ledger and rollback.

**Design.** As in the record: learned skills under `~/.overseer/skills/<slug>/` with `created_by: overseer-learn`, status lifecycle, content-addressed ledger and blobs, read-before-write patching, usage tracking. User-authored skills are never touched.

::: gate
The v3 record's §5 verification list; a red-team test that a tainted window can never create or patch a skill; rollback restores byte-identical content.
:::

#### M3 — The dream pass

**What.** v3 §6: consolidation, exact-duplicate supersession, near-duplicate and contradiction judging with source checking, fading, skill lifecycle, `MEMORY.md` regeneration.

**Scheduling.** The deterministic subset at session start when stale (as specified), and the full pass as a daily workflow (A) on the gateway rather than a bare cron trigger, so it inherits retries and journaling.

::: gate
The v3 record's §6 verification list; unattended runs stage every model-judged change; a dry run produces an exact diff of what would change.
:::

#### M4 — Mounted memory repositories and sync

**What.** v3 §9.4–9.5: AMR-compatible mounted repos with ownership and trust, and `ff-only` sync that never forces or merges on its own.

#### M5 — Outcome memory

**What.** Remember what worked and what failed, per project and per task class, and use it when choosing an approach. No harness surveyed does this systematically **[infer]**.

**Design.**

- **Capture.** At run end (and at each journal milestone in long runs), the engine records an `Outcome` note candidate deterministically: task class (from the plan or the first prompt, classified by a small fixed taxonomy), approach markers (tools used, files touched, verify commands), result (verified pass, fail, abandoned), cost, duration, and the journal's decisions. No model call is needed for capture.
- **Distillation.** The learning review (and the dream pass) may turn repeated outcomes into procedural notes: "in this repo, migrations need the schema check before the test suite; three runs failed without it".
- **Use.** Recall gains an outcome lane: when a new task's class and project match past outcomes, the top relevant lesson (success pattern or failure warning) is offered in the memory notice, within the existing three-note cap.
- **Safety.** Outcomes from tainted runs are quarantined like any other tainted write.

::: gate
On a repeated task family in the rig, a warm store with outcome memory shows a measured pass-rate or cost improvement over a cold store, with paired statistics (E6). If it does not, outcome recall stays off by default.
:::

#### M6 — Journals into memory

**What.** The run journal (R5) is the richest record of a long run. At run end its decisions and milestones feed the episodic note and the review digest, so a week-long run leaves durable lessons instead of a one-paragraph episode.

**Design.** The episode note for a journaled run is derived from the journal (goal, plan outcome, decisions, verify evidence) rather than the first prompt alone. The review digest includes journal decisions as a section with its own cap.

#### M7 — Cross-project promotion

**What.** A lesson learned in three projects is probably a user-level lesson.

**Design.** The dream pass looks for semantically equivalent procedural notes across project stores (term overlap plus one judge call, staged unattended) and proposes a user-level note that supersedes the copies' recall weight without deleting them.

#### M8 — Recurring reminders

**What.** The deferred `every:` prospective form (`every: weekday 09:00`, `every: 7d`), firing once per period with the existing claim-file mechanism. Long schedules belong to workflows (A5); `every:` covers personal reminders inside sessions.

#### M9 — Memory evaluation

**What.** Measure memory instead of trusting it.

**Design.** A LongMemEval-S adapter in the rig (planned since memory v2), plus Overseer-specific suites: repeat-question rate across sessions, recall precision on seeded facts, correction persistence (a correction given once is honoured in the next N sessions), poisoning resistance (seeded tainted content must not reach a live note), and the warm-versus-cold uplift test from M5.

#### M10 — Poisoning defences, extended

**What.** Every new path into memory (journals, outcomes, artifacts, persistent agents' stores, mounted repos) inherits the threat scan and taint rules.

**Design.** A single write chokepoint audit: every store write goes through one function that applies redaction, the strict threat scan, the taint check and the write bars. A test enumerates every caller.

#### M11 — Memory you can see and edit

**What.** Memory is only trustworthy if the owner can inspect and correct it easily.

**Design.** A memory panel in the TUI and web UI (X7): browse layers, see provenance and confidence, approve or reject pending items, edit or forget notes, see what was recalled in each turn and why (the ranking factors). The CLI already covers `pending`, `approve`, `reject`, `stats`, `log`, `restore`.

#### M12 — Shared and team memory

**What.** Several people or several machines sharing a project's memory.

**Design.** Mounted repos (M4) with a private remote are the mechanism; ownership and trust per mount decide who writes. Conflict handling stays git-native with `ff-only`, never automatic merges. Deliberately late: the owner's posture is "me first".

### 7.4 What could break

::: risk
**Memory makes the agent confidently wrong.** A stale or wrong note recalled with authority. Mitigation: confidence and validity on every note, FEEDBACK lowering confidence on wrong notes, fading, and recall shown provenance-wrapped so the model treats it as evidence, not instruction.
:::

::: risk
**Outcome memory learns superstition.** Correlation from a handful of runs becomes a rule. Mitigation: distillation requires repeated outcomes with consistent evidence, new rules start at low confidence on probation, and M5's gate requires measured uplift before outcome recall is on by default.
:::

::: risk
**The cache breaks.** Memory content above the cache boundary changing mid-session would rewrite the prefix. Mitigation: the existing rule holds (resident memory assembled once per agent); all new recall and outcome lanes inject below the boundary.
:::

### 7.5 Metrics

| Metric | Target |
|---|---|
| Repeat-question rate across sessions on seeded facts | ≤ 5% |
| Correction persistence over the next 5 sessions | ≥ 95% |
| Poisoned content reaching a live note in the red-team suite | 0 |
| Warm-store uplift on repeated task families | Positive with 95% confidence, or the feature stays off |
| History search latency over 400 sessions | ≤ 50 ms |
