## 17. Evaluation and Proof

### 17.1 Why this chapter decides everything

The playbook's governing rule: build the evaluation rig before the agent, and beat the minimal loop measurably, per component, without regressing leanness **[src: playbook §12.1]**. The harness is a first-class variable worth roughly a model generation (2–9 point swings on Terminal-Bench 2.1 between harnesses on the same model, and far larger in some sweeps) **[src: playbook thesis 1]**. Overseer built the rig early; it has never been pointed at live models because of the eval hold. Every "better" claim in this roadmap waits on that.

### 17.2 Where we stand

The rig (Chapter 3.11): task spec v2 with mandatory oracles, deterministic graders, a k-seed scheduler, an immutable store, paired bootstrap and McNemar statistics, report cards, a frozen bash-only control scaffold; 40 public and 5 canary held-out tasks; adapters for SWE-bench Verified, Terminal-Bench 2.x, SWE-rebench, τ² and LiveCodeBench. Zero spend so far.

### 17.3 Workstreams

#### E1 — Lift the hold: the first measured baseline

**What.** With the owner's go-ahead and a spend cap, run Overseer and the control scaffold on the local corpus and a small SWE-bench Verified slice, on one priced model and the free opencode model, k seeds each. This produces the W0 baseline every later wave is measured against.

::: decision
The eval hold stays until the owner lifts it. Everything in this chapter that spends money is gated on that decision (Chapter 26).
:::

#### E2 — The long-horizon suite

**What.** Tasks that take hours to days: multi-module migrations on pinned open-source repositories, multi-step research with verifiable citations, and the five journeys of Chapter 1 as scripted scenarios with oracles. Measured: completion, unattended completion, cost, wall time, owner interventions, effects executed exactly once.

**Solvability is decided before scoring, never after.** Each task declares, before any scored run, the evidence that it is solvable: an oracle that passes its grader (the rig's existing rule for every task), or a reference run that completed within twice the scored budget. A task without that evidence is excluded from the denominator up front. A scored failure cannot reclassify a task as unsolvable; disputes are settled by a second seeded reference run, and the outcome is recorded with the task's version.

**Cost control.** Long tasks are expensive. The suite runs on the cheapest capable model by default (the free opencode model where possible), with a small priced subset; runs are local, one at a time.

#### E3 — The chaos suite

**What.** The long-horizon suite again, under R15's fault injection: provider errors at several rates, crashes at random points, disk-full, clock jumps, simulated reboots, MCP deaths. Pass criterion: same outcomes as the fault-free run, no duplicate effects, bounded extra cost.

#### E4 — Cost per solved task, published

**What.** Every report card carries cost per solved task with the cache breakdown, against the control and against competitors on the same model.

#### E5 — Regression gates per wave

**What.** Each wave closes only when the rig shows no regression on pass rate (non-inferiority margin 3 points), cost per solved task, resident tokens, binary size and startup, against the previous wave's baseline, with paired statistics.

#### E6 — Memory evaluation

**What.** M9's suites: LongMemEval-S adapter, repeat-question rate, correction persistence, poisoning resistance, and the warm-versus-cold uplift test for outcome memory (M5).

#### E7 — Orchestration evaluation

**What.** Tasks labelled parallelizable and not; measure pass rate and cost per solved task with and without the task graph, merge queue and best-of-N, so the lead's prompt orchestrates only where it pays.

#### E8 — Competitors on the same model

**What.** Run competitor harnesses (with their recommended settings, recorded) on the same tasks and model through the rig's adapters (Harbor already hosts several agents), so "crush" is a measured, reproducible claim rather than a slogan.

**Equivalence policy, published before any run.** Same model and version wherever the competitor allows model selection. Where it does not (subscription-only products, fixed routing), the closest equivalent is used and the mapping is published next to the result. A harness that cannot be run fairly is listed as "not comparable" with the reason, never dropped silently. Settings, versions and dates are part of the report card.

#### E9 — Contamination hygiene

**What.** The held-out suite is repo-visible and therefore a canary check, not a true holdout (the rig's README says so). Add a private holdout kept outside the repository, rotate tasks, and keep the canary audit on every stored run. The holdout is authored early (W2), long before W9 consumes it, and each task records its authoring date: a holdout written after its material was public would be suspect.

### 17.4 What could break

::: risk
**Evaluation costs more than the owner can spend.** Mitigation: free models where possible, small priced subsets, local runs one at a time, and the spend gate on every run.
:::

::: risk
**Overfitting to our own suite.** Mitigation: external benchmarks alongside the local corpus, the private holdout, and competitors on the same tasks.
:::
