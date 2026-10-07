## 25. The Scoreboard

"Crush every other harness" needs numbers. These are the metrics that decide it, their targets, and how each is measured. Baselines are taken in W0 (E1); targets are checked at each wave close and published at W9.

### 25.1 North-star metrics

| Metric | Definition | Target | Measured by |
|---|---|---|---|
| Unattended long-horizon completion | Long-horizon suite runs that complete without owner intervention, over tasks pre-declared solvable (E2's rule) | ≥ 95% | E2 |
| Chaos parity | Long-horizon outcomes under injected faults equal fault-free outcomes | 100% of tasks pre-declared solvable | E3 |
| Cost per solved task | Total spend ÷ tasks solved, same model, same tasks | Lowest among measured harnesses; ≥ 30% below Overseer's W0 baseline | E4, E8 |
| Pass rate | Paired, same model, 95% CI | Higher than every measured competitor, or non-inferior at lower cost | E1, E8 |
| In-session cache hit rate | Cache-read input tokens ÷ total input tokens, per session | ≥ 92% Anthropic; ≥ 85% OpenAI-compatible | B2 |
| Duplicate external effects | Across all chaos runs | 0 | E3, R7 tests |

### 25.2 Supporting metrics

| Area | Metric | Target |
|---|---|---|
| Reliability | Runs ended by transient-class failures | 0 |
| Reliability | Time to automatic resume after a crash | ≤ 60 s |
| Reliability | Resume from snapshot for a 100-hour log | ≤ 2 s |
| Reliability | Owner interventions per 100 run-hours, excluding genuine approvals | ≤ 1 |
| Context | Input tokens per solved task | ≥ 25% below W0 |
| Tokens | Resident startup tokens | ≤ 2,000 |
| Tokens | Token estimate error after calibration | < 3% |
| Tools | Wall time per multi-read turn | ≥ 30% below W0 |
| Orchestration | Merge-queue landings that later fail verify | 0 |
| Orchestration | Fork subagent cache-read share of prefix | ≥ 90% |
| Memory | Repeat-question rate on seeded facts | ≤ 5% |
| Memory | Correction persistence over 5 sessions | ≥ 95% |
| Policy | Approval prompts per 100 calls in per-action mode | ≤ 2 |
| Policy | Decisions with a complete explanation | 100% |
| Security | Injection attack success rate per corpus category | No regression; trending down |
| Security | Kill-switch latency to every agent | ≤ 2 s |
| Automation | Silent workflow failures | 0 |
| Automation | Scheduled completions within window over 30 days | ≥ 99% |
| Leanness | Release binary | ≤ 15 MB |
| Leanness | Cold start to first render | ≤ 50 ms |
| Leanness | Idle TUI RSS | ≤ 30 MB |

### 25.3 How a number becomes a claim

A metric is reported publicly only with its method: harness commit, model and version, seeds, task set and version, environment digest, and the confidence interval, in the rig's report card format. Competitor numbers are reported only from runs on the rig with their recommended settings recorded, never from vendor claims.
