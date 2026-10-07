# Part 0 — Before You Read

## How to Read This Roadmap

This is the master plan for turning Overseer from a careful, well-instrumented agent harness into the best agent harness that exists: the one an owner can hand a hundred-hour job to, walk away from, and come back to a finished, verified, fully audited result. It is written to be acted on. Every chapter ends in workstreams with identifiers, acceptance gates, and the failure modes that would defeat them.

### Who this is for

- **The owner.** You set direction, make the decisions listed in Chapter 26, and approve what reaches `review`.
- **Any engineer or agent working on Overseer.** Each workstream is specified closely enough to start from, with pointers into the code as it stands.
- **Reviewers.** The gates and the failure atlas (Chapter 22) are the checklist that a change is measured against.

### What this document is, and is not

It is a roadmap: current state, target state, the path between them, and the evidence required to say each step is done. It is not a design record for any single feature. When a workstream starts, it gets its own decision record under `docs/research/` in the existing style (the memory v2, memory v3, tool economy and subagent tier records are the model), and where the decision record disagrees with this roadmap, the decision record wins.

It is also not a calendar. Work is sequenced by dependency and by gate, never by date. A wave closes when its gates pass, not when a week ends.

### Baseline

Everything described as "today" refers to this exact state:

- `feat/fortify` at `cfc56a6`, stacked on `feat/memory-v3` at `f46ba1b` (PRs #54 and #55, open, not yet merged into `review`).
- Plus five reviewed but unmerged fix branches: `fix2-memory` (`68c1d62`), `fix2-agent` (`4b4a2b3`), `fix2-gateway` (`627b63c`), `fix2-tools` (`0bf3719`) and the local `fix2-computer` (`8e53d27`, including the lean pass).

Where a fix branch changes the picture, the text says so, tagged **[fix2]**. A reader working after integration should treat **[fix2]** items as present.

Size of the baseline, measured on `feat/fortify`:

| Crate | Source lines | Test lines | Role |
|---|---|---|---|
| `overseer-core` | 58,695 | 12,903 | Engine: IR, event log, providers, tools, permissions, memory, subagents |
| `overseer-tui` | 8,500 | 1,900 | Terminal UI and the localhost browser UI |
| `overseer-gateway` | 8,175 | 1,002 | Always-on daemon, channels, triggers, inbox and outbox |
| `overseer-life` | 4,772 | 1,615 | Overseer Life connectors (Phase 0) |
| `overseer-cli` | 3,788 | 1,834 | The `overseer` command |
| `overseer-proto` | 207 | 0 | Wire types |

### Conventions

**Status tags** mark how far a capability is from existing:

| Tag | Meaning |
|---|---|
| **[have]** | Present on the baseline and covered by tests |
| **[fix2]** | Present on a reviewed fix branch, lands at integration |
| **[partial]** | Exists but incomplete, with the gap named |
| **[deferred]** | Explicitly parked in code with a `DEFERRED(owner)` marker |
| **[new]** | Does not exist; this roadmap proposes it |

**Evidence tags** follow the research notes:

| Tag | Meaning |
|---|---|
| **[code]** | Read in the Overseer source at the cited path |
| **[doc]** | Primary vendor documentation |
| **[src]** | Secondary source, recorded in an earlier research note |
| **[infer]** | Reasoning, not measurement |

**Workstream identifiers** are a letter plus a number, and they never change once published. The letters are:

| Letter | Pillar |
|---|---|
| R | Reliability and long-horizon execution |
| O | Orchestration and subagents |
| M | Memory |
| C | Context management |
| T | Tool efficiency |
| K | Token and cache efficiency |
| P | Policy: principles, rules, AGENTS.md, instruction architecture |
| A | Automation: workflows, schedules and triggers |
| S | Security and trust |
| U | Computer use and the browser |
| L | Model layer |
| B | Observability |
| E | Evaluation and proof |
| X | Frontends and experience |
| Y | Extensibility and ecosystem |
| Z | Runtime, performance and distribution |
| F | Overseer Life integration |

**Waves** are numbered W0 to W10 (Chapter 23). That is why automation workstreams use the letter A rather than W.

**Callouts** have fixed meanings:

::: principle
A rule that holds across the whole system. Breaking it needs a decision record.
:::

::: decision
A choice the owner has already made, quoted or paraphrased with its date.
:::

::: gate
The evidence that must exist before a workstream or wave is called done.
:::

::: risk
A specific way the plan, or the system it describes, could fail.
:::

::: note
Context that helps but does not bind.
:::

### Standing rules that govern all of this work

These come from the owner and from `AGENTS.md`. They apply to every workstream in this document.

1. **No GitHub Actions spend.** Pushing a branch is fine. A pull request's newest commit carries `[skip actions]`. Scratch branches are transport only and are never opened as PRs.
2. **One cloud session at a time, and only deliberately.** Stress testing happens on the owner's Mac first.
3. **Full line-by-line review, then an independent adversarial review**, before anything lands on a PR branch.
4. **Deferred-item comments everywhere.** Every PR body ends with a Deferred section; every merge commit carries the same list; every code site that defers work carries `// DEFERRED(<owner>): <what> — <gate>`.
5. **Branch ladder.** Feature branches fork from `review` and target it; `review` promotes to `dev` for stress releases; `dev` promotes to `main` only for tagged releases.
6. **No spending, no sign-ups, no writes to other apps' credentials or data** without the owner's explicit say.

## The Contract

The owner's request for this document, verbatim:

> "design a pdf and throw it into the github. a multi step, extremely detailed roadmap to make this harness crush every other harness like dust. dont tell me no, aim for the moon and youll land amongst the stars. we want literally the best. a true alfred to batman. i dont care if the pdf is more than 100 pages long or however many. extreme details. dont miss out on anything. dont compromise on literally anything. absolutely no gap to be left. get into this zone of the future, what could potentially break? think twice, thrice before the final edition of the roadmap."

And the pillar framing that preceded it, also verbatim:

> "listen, we have a few pillars. memory. agent orchestration and everything subagent related. context management. tool efficiency. token efficiency. sub pillars would be things like principles, rules, agents.md, so so so many things more to discuss"

> "reliability has to be a main pillar too. long horizon automations, workflows, everything needs to be bulletproof. tasks can go on for 100+ hours no issue and uninterrupted if needed"

### What this contract requires of the document

| Requirement | Where it is met |
|---|---|
| Multi-step and extremely detailed | Every pillar chapter breaks into numbered workstreams with design, interfaces, edge cases and gates; Chapter 23 sequences them into waves |
| Crush every other harness | Chapter 4 sets the competitive bar per capability; Chapter 25 turns "crush" into measurable claims |
| A true Alfred | Chapter 1 defines the Alfred standard operationally; every pillar is tested against it |
| Nothing missed, no gap left | Seventeen pillars and systems, plus a coverage matrix in Chapter 23 mapping every gap found in the baseline audit to a workstream |
| What could potentially break | Every workstream carries its own risks; Chapter 22 is a full failure atlas plus premortems of a 100-hour run |
| Reliability as a main pillar, 100+ hour runs | Chapter 5 is the longest chapter, and reliability is the spine every wave depends on |
| Memory, orchestration, context, tools, tokens | Chapters 6 through 10 |
| Principles, rules, AGENTS.md | Chapters 2 and 11 |
| Thought through more than once | Appendix F records the review rounds this edition went through and what each changed |

### Prior owner decisions this roadmap honours

These were made in earlier sessions and are binding here.

::: decision
**Computer-use consent (2026-10-07).** In the default per-action mode, screen reads and mutating actions ask **once per task**. Full automation is **truly never ask**, with no Rule-of-Two safety floor. There is no mandatory hard denylist in full automation: "in full auto mode, we'll have dedicated permissions for secrets and things like this up to the user to configure." Consent modes ship on their own branch after the current integration.
:::

::: decision
**fix2-memory finding M8 (2026-10-07).** Option (a): apply the ASCII-alphanumeric neighbour rule for U+200C/U+200D and change the two affected test rows to a letter split.
:::

::: decision
**`--bare` skips user skills (fix2-tools finding S1, 2026-10-07).** Implemented as an `AgentConfig` field threaded through `prompt.rs` and `tools/skill.rs`, not a process-global switch.
:::

::: decision
**Cloud posture (2026-10-07).** One session at a time; stress testing runs locally. Weekly cloud quota is scarce.
:::

::: decision
**Overseer Life (2026-10-05).** Me first, product later. Rust core with a native SwiftUI shell on macOS, iOS soon, Windows and Linux later. Backend before frontend. Repo stays inside Overseer. X is the first social platform.
:::

::: decision
**No project-level API key (2026-09-25).** Providers read their own key names only.
:::
