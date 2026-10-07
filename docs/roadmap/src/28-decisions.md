## 26. Decisions for the Owner, and the Risk Register

### 26.1 Decisions needed, in the order they block work

| # | Decision | Options | Recommendation | Blocks |
|---|---|---|---|---|
| D1 | Lift the eval hold for a capped baseline | Keep the hold; lift with a spend cap; lift for free models only | Lift with a small spend cap plus the free model; without it every "better" in this roadmap is a guess | E1 and every measured gate (W0 onward) |
| D2 | First priced live run | Approve a capped run on Anthropic; defer | Approve; the spend gate has never seen real prices | K12 (W0) |
| D3 | Live computer-use session with owner present | Schedule; defer | Schedule; nothing in computer use is proven live | U1 (W0) |
| D4 | Rotate the provider key from 2026-09-20 | Rotate now | Rotate now | S7 (W0) |
| D5 | Move the repository out of iCloud-synced Desktop | Move; mark `target/` unsynced; leave | Move | Z8; risk before W2's long runs |
| D6 | Stop the failing PR release job | `pr-run-mode = "skip"`; keep `[skip actions]` on every PR | `pr-run-mode = "skip"` | Z3 (W0) |
| D7 | Default rate budgets for long runs | Owner sets per run (required); global defaults | Required per run the first time, then remembered per project | R10 (W3) |
| D8 | Keep the machine awake during long runs on mains power | Never; opt-in; default on | Opt-in | R12 (W2) |
| D9 | Away-policy default on timeout | `deny_once`; `park`; `wait` | `deny_once` | R8 (W4) |
| D10 | Trust model for third-party AGENTS.md | Ask once per content hash; always untrusted; always trusted | Ask once per hash | P1 (W4) |
| D11 | Full-automation sensitive-surface defaults at onboarding | Owner chooses each category; offer a preset | Owner chooses, with a suggested preset shown | P6 (W4) |
| D12 | Order of W5 and W6 | Context and tokens first; orchestration first | Context and tokens first | W5/W6 |
| D13 | Workflow definition format | TOML; JSON (like gateway config today) | TOML for human-authored files | A1 (W7) |
| D14 | Apple Developer enrollment | Enroll now; later | Enroll when convenient; it gates signing, the computer helper and Life's browser entitlements | Z2, U6, S5 (W10); Life phases |
| D15 | Remote executor | Build R18; skip | Build after W9, one session at a time | R18 (W10) |
| D16 | Close PR #50 | Close; keep | Close; fully contained in #51 | Hygiene |

Overseer Life's own pending owner items stay as recorded in its plan: the X live spike (developer app, a few dollars of credit, owner present), bank and email providers for the money spike, and the Apple Developer account.

### 26.2 Risk register

The highest-impact risks from Chapter 22 with owners and triggers for review.

| ID | Risk | Likelihood | Impact | Owner | Mitigation | Review trigger |
|---|---|---|---|---|---|---|
| K-1 | Auto-resume repeats a side effect | M | H | engine | R7 before R4 auto-resume | Any in-doubt resolution bug |
| K-2 | Long run drifts while looking busy | M | H | engine | R5, R9, O9 | Goal audit drift verdicts above 5% of audits |
| K-3 | Cost runaway on long runs | M | H | engine | R10 rate budgets, B4 forecasts | Any run exceeding forecast by 50% |
| K-4 | Provider or model change mid-run | M | H | engine | L3, L4, L8 | Provider announcements; E5 regressions |
| K-5 | Prompt injection late in a run | M | H | security | Latches, invariant 19, S6 | Corpus regression |
| K-6 | Upgrade breaks in-flight runs | M | H | engine | R13, Z3 | Any golden replay failure |
| K-7 | Disk exhaustion | M | H | engine | R11 | Free space alerts |
| K-8 | Parallel writers conflict | M | M | engine | O6 | Repeated conflict re-plans |
| K-9 | Leanness erodes | H | M | engine | Z1 budgets in every gate | Any budget exceedance |
| K-10 | The roadmap stalls | H | M | owner | One wave at a time; W1–W3 first | A wave open longer than its predecessor took twice over |
| K-11 | Evaluation unaffordable | M | M | owner | Free models, small priced subsets | Spend cap reached |
| K-12 | Full automation causes harm | L | H | owner | P6, S4, S10 | Any highlighted effect in the audit digest |
| K-13 | iCloud sync corrupts the repository | M | H | owner | Z8 | Any conflict file under `.git` |
| K-14 | Scheduled automation fails silently | M | H | automation | A12 dead-man's switches and independent channels | Any missed window |
