# overseer fortify: final verification

HEAD: `5b0d523ccbb21e97df8eb694a710863faca3eca9` (`git rev-parse HEAD`). Ubuntu VM, 8 cores, cargo 1.97.1. bubblewrap was installed before the runs (`bwrap --ro-bind / / true` ok), cargo-deny 0.20.2.

## Part 1: gates (Linux)

| gate | result |
|---|---|
| `cargo fmt --all -- --check` | pass (exit 0, no diff) |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass (exit 0, 0 warnings) |
| `cargo test --workspace --no-fail-fast` run 1 | **FAIL** (exit 101): 47 binaries, 1383 passed / 1 failed / 6 ignored, 2m33s |
| `cargo test --workspace --no-fail-fast` run 2 | pass: 47 binaries, 1384 passed / 0 failed / 6 ignored, 2m15s |
| `cargo test --workspace --no-fail-fast` run 3 | pass: 47 binaries, 1384 passed / 0 failed / 6 ignored, 2m28s |
| `cargo deny check` | pass: `advisories ok, bans ok, licenses ok, sources ok` |
| `cargo build --release -p overseer-cli` | pass, 1m50s; `target/release/overseer` = 9,852,984 bytes |

Flake list (failed in any of the 3 runs):
- `overseer-gateway` `tests/audit_gateway.rs` `dash_leading_prompt_reaches_the_child_as_a_prompt`: failed in run 1 only; passed in runs 2 and 3 and 5/5 times run alone afterwards. Output tail (run 1):

```
---- dash_leading_prompt_reaches_the_child_as_a_prompt stdout ----
thread 'dash_leading_prompt_reaches_the_child_as_a_prompt' (24514) panicked at crates/overseer-gateway/tests/audit_gateway.rs:263:6:
called `Result::unwrap()` on an `Err` value: "spawn overseer: Text file busy (os error 26)"
test result: FAILED. 18 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.67s
```
Observed: ETXTBSY from `spawn_run_from(...).unwrap()` at audit_gateway.rs:263, spawning the `overseer-wrapper` shell script that the test writes with `std::fs::write` + `set_permissions` just before (audit_gateway.rs:240-249). Inference (not verified): the classic write-then-exec race, where another thread's concurrent fork holds the script's write fd open during the exec.

Per-binary counts (passed/failed/ignored), identical across runs except audit_gateway:
overseer-cli: unit 44/0/0, audit_surfaces 14/0/0, audit_surfaces_web 14/0/0, code_mode 1/0/0, help 5/0/0, memory 10/0/0, memory_v3_redteam_cli 7/0/0.
overseer-core: unit 714/0/3, audit_computer 32/0/0, audit_data 22/0/0, audit_data_providers 8/0/0, audit_memory2_bench 0/0/2, audit_memory2_index 1/0/0, audit_memory2_notes 16/0/0, audit_memory2_redact 11/0/0, audit_memory2_stores 4/0/1, audit_orchestration 12/0/0, audit_providers_extra 2/0/0, audit_tools 26/0/0, computer_v2 23/0/0, cross_feature 5/0/0, injection_asr 3/0/0, memory_v3 15/0/0, memory_v3_redteam_lock 10/0/0, memory_v3_redteam_parser 5/0/0, memory_v3_redteam_review 18/0/0, memory_v3_redteam_store 17/0/0, memory_v3_redteam_threat 19/0/0, stress_p3 14/0/0.
overseer-gateway: unit 103/0/0, audit_gateway 18/1/0 (run 1), 19/0/0 (runs 2, 3), audit_gateway_daemon 1/0/0, audit_gateway_tz 2/0/0.
overseer-life: unit 16/0/0, audit_life 10/0/0. overseer-proto: unit 3/0/0.
overseer-tui: unit 67/0/0, audit_rules 1/0/0, audit_surfaces 16/0/0, snapshots 28/0/0, stress 9/0/0, connectors 37/0/0.
Doc-tests (core, gateway, life, proto, tui): 0 tests each.

### Ignore list (`rg -n '#\[ignore' crates/`)
Six real `#[ignore]` attributes, which match the 6 ignored tests above:

| location | test | reason string |
|---|---|---|
| crates/overseer-core/src/tools/computer/cua.rs:2777 | `computer_live` | none (bare `#[ignore]`); doc comment: "Live smoke (owner's Mac, cua-driver installed)" |
| crates/overseer-core/src/memory/index.rs:1408 | `memory_bench` | none (bare `#[ignore]`); doc comment: "Release-mode timings" |
| crates/overseer-core/src/memory/notice.rs:567 | `fire_claim_child` | "child process of fire_claims_once_across_processes" |
| crates/overseer-core/tests/audit_memory2_bench.rs:532 | `retrieval_benchmark` | "benchmark: memory v2 retrieval quality" |
| crates/overseer-core/tests/audit_memory2_bench.rs:662 | `scale_benchmark` | "benchmark: memory v2 index scale" |
| crates/overseer-core/tests/audit_memory2_stores.rs:161 | `a_moved_checkout_keeps_its_project_store` | "deferred: H3 store identity" |

The other 10 rg hits are `//!` doc-comment lines that mention `#[ignore = "audit: ..."]` (audit_gateway.rs:4, cli audit_surfaces.rs:4, audit_surfaces_web.rs:3, tui audit_surfaces.rs:4, audit_data.rs:2, audit_data_providers.rs:2, audit_memory2_bench.rs:1, audit_orchestration.rs:2, audit_tools.rs:5, audit_life.rs:4). No `audit:`-reason ignores remain.
