# VERIFY-REPORT-2: feat/fortify @ 5003b78fd027da3d37aad01bf1ffe60477fa23ef

Verification only. No code changed. VM: Ubuntu, rustc/cargo 1.97.1, cargo-deny 0.20.2, bubblewrap installed (`/usr/bin/bwrap`).

## Part 1: gates

| gate | result |
|---|---|
| `cargo fmt --all -- --check` | pass (exit 0) |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass (exit 0) |
| `cargo test --workspace --no-fail-fast` run 1 | pass (exit 0): 1387 passed, 0 failed, 6 ignored |
| `cargo test --workspace --no-fail-fast` run 2 | FAIL (exit 101): 1386 passed, 1 failed, 6 ignored |
| `cargo test --workspace --no-fail-fast` run 3 | pass (exit 0): 1387 passed, 0 failed, 6 ignored |
| `cargo deny check` | pass (exit 0): `advisories ok, bans ok, licenses ok, sources ok`, 0 warnings |
| `cargo build --release -p overseer-cli` | pass (exit 0): `target/release/overseer` is 9,858,520 bytes |

Counts are the last `test result:` line of each of the 47 `Running`/`Doc-tests` sections, so lines printed by re-exec'd worker subprocesses are not counted twice. The 6 ignored tests are the same in every run: 3 in overseer-core lib, 2 in audit_memory2_bench.rs, 1 in audit_memory2_stores.rs.

Runs 2 and 3 ran on the same VM at the same time as the Part 2 live runs (the release build had already finished; run 1 ran alone).

### Flake list
| test | runs failed | alone |
|---|---|---|
| `overseer-core` lib `tools::computer::cua::tests::batch_members_run_through_the_driver` | 1 of 3 (run 2) | 5 of 5 pass (`cargo test -p overseer-core --lib tools::computer::cua::tests::batch_members_run_through_the_driver`) |

Panic (run 2): `crates/overseer-core/src/tools/computer/cua.rs:2722:10`: `called Result::unwrap() on an Err value: "computer: batch failed at actions[0] (observe) — computer: mcp server cua-driver: could not spawn /tmp/overseer-cua-batch-27613-.../fake.sh: Text file busy (os error 26)"`.

Observed in the code: this test calls `super::super::run_with(...)` directly at cua.rs:2713 and then `.unwrap()`s the result. The ETXTBSY retry helper `run_ok` (cua.rs:1947-1962, 60 tries x 50 ms on "Text file busy"/"os error 26") wraps `run(...)` only. Commit 5003b78 routed "every bare run() call" through `run_ok`, and this `run_with` call is not one of those, so it has no retry.
