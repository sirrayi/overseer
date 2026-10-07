## 24. The Verification System

Verification is the highest-leverage harness investment for agents **[src: playbook thesis 6]**, and it is just as decisive for building the harness. This chapter defines how every workstream proves itself.

### 24.1 The layers

| Layer | What it catches | Where it runs | When |
|---|---|---|---|
| Unit tests | Logic errors in one module | `cargo test` | Every change |
| Property tests | State-machine and parser edge cases (R14 lifecycle, A1 parser, O2 graph validation, P2 precedence) | `cargo test` with seeded generators | Every change |
| Golden tests | Behaviour drift in refactors (C7 views, P2 verdicts and prompt bytes, R13 log replay) | `cargo test` against committed corpora | Every change |
| Fixture tests | Provider and driver contract drift (R1 error tables, cua-driver schemas, reasoning rules) | `cargo test` against captured fixtures | Every change; fixtures refreshed deliberately |
| Fault-injection tests | Reliability claims (R15) | Test builds with the fault layer | Every change touching R, A, O |
| Crash-consistency tests | Durable stores (R6) | Kill-at-every-step harness | Every change touching a store |
| Injection corpus | Security latches (S6) | `tests/injection_asr.rs` and extensions | Every change |
| Adversarial review | Everything the tests did not think of | An independent reviewer, locally | Every wave close and every PR (standing rule) |
| Rig evaluations | Capability, cost and regressions (E) | The eval rig | Wave close, and per workstream where a gate demands it |
| Soak tests | Leaks and slow failures (R17, Z7) | The owner's Mac | 24 hours per `dev` release, 100 hours per `main` release |
| Live checks | Reality versus fakes (U1, K12) | The owner's Mac, owner present | Before dependent work ships |

### 24.2 Rules

1. **A bug is first a failing test.** Where tests exist, reproduce before fixing (the fix2 cycle did this and found real issues).
2. **Fakes are verified against reality.** Every fake (provider, driver, channel) has a fixture captured from the real thing, and the fixture's capture date is recorded.
3. **No test is weakened to pass.** Changing an assertion requires stating why the old one was wrong, in the commit.
4. **Ignored tests carry a reason and a gate**, like deferred code.
5. **Evidence is reported, not asserted.** A gate is passed when its evidence (test output, report card, soak log) is attached to the wave report.
6. **The ETXTBSY lesson generalizes.** Any test that writes and executes a file, or depends on timing, uses the retry and deadline helpers; flakes are bugs.

### 24.3 The local gate script

Because GitHub Actions minutes are off the table, the gate is a local script that the reviewer runs on the owner's Mac before any push to a PR branch:

| Step | Command |
|---|---|
| Format | `cargo fmt --all -- --check` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Tests | `cargo test --workspace --no-fail-fast` (twice, to catch flakes) |
| Supply chain | `cargo deny check` |
| Budgets | Z1 size, startup, RSS and resident-token checks |
| Golden corpus | R13 replay of every committed log |
| Static lints | P13 checks (deferral markers, blocking calls, event skippability, store chokepoint, policy reads) |
| Red team | The release-mode red-team suite at `REDTEAM_ITERS=20000` |
| Eval (when the hold is lifted) | E5 regression check against the previous wave baseline |
