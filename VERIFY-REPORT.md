# overseer memory v3 — final verification (Linux)

HEAD: `f46ba1bef3e264bf41df9c5218f1f2e3bba0227c` (feat/memory-v3). Ubuntu VM, 8 vCPU / 31 GiB, cargo 1.97.1, bwrap installed (apt), cargo-deny 0.20.2.

## Part 1: gates

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | pass (exit 0, no diff) |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass (exit 0, 0 warnings) |
| `cargo test --workspace --no-fail-fast` run 1 | pass: 1166 passed, 0 failed, 6 ignored (32 test binaries incl. doctests), 88 s |
| `cargo test --workspace --no-fail-fast` run 2 | pass: 1166 passed, 0 failed, 6 ignored, 74 s |
| `cargo test --workspace --no-fail-fast` run 3 | pass: 1166 passed, 0 failed, 6 ignored, 71 s |
| `cargo deny check` | pass: `advisories ok, bans ok, licenses ok, sources ok` |
| `cargo build --release -p overseer-cli` | pass, `target/release/overseer` = 9,526,472 bytes (`overseer 0.1.0`) |

Flake list (any test failing in any of the 3 runs): none.

Count method: last `test result:` line of each `Running`/`Doc-tests` section. A naive sum of every `test result:` line gives 1179 passed, because 13 extra lines come from re-exec'd worker subprocesses (10 inside `memory_v3_redteam_lock`, 3 inside `overseer_core` unittests).

Per binary (run 1; runs 2 and 3 identical):

| Binary | passed | ignored |
|---|---|---|
| overseer (cli src/main.rs) | 39 | 0 |
| cli tests/code_mode | 1 | 0 |
| cli tests/help | 5 | 0 |
| cli tests/memory | 10 | 0 |
| cli tests/memory_v3_redteam_cli | 7 | 0 |
| overseer_core lib | 708 | 3 |
| core audit_memory2_bench | 0 | 2 |
| core audit_memory2_index | 1 | 0 |
| core audit_memory2_notes | 16 | 0 |
| core audit_memory2_redact | 11 | 0 |
| core audit_memory2_stores | 4 | 1 |
| core cross_feature | 5 | 0 |
| core injection_asr | 3 | 0 |
| core memory_v3 | 15 | 0 |
| core memory_v3_redteam_lock | 10 | 0 |
| core memory_v3_redteam_parser | 5 | 0 |
| core memory_v3_redteam_review | 18 | 0 |
| core memory_v3_redteam_store | 17 | 0 |
| core memory_v3_redteam_threat | 19 | 0 |
| core stress_p3 | 14 | 0 |
| overseer_gateway lib | 101 | 0 |
| overseer_life lib | 16 | 0 |
| tests/connectors | 37 | 0 |
| overseer_proto lib | 3 | 0 |
| overseer_tui lib | 64 | 0 |
| tui tests/snapshots | 28 | 0 |
| tui tests/stress | 9 | 0 |
| (remaining 5 sections, incl. doctests) | 0 | 0 |

### Ignore list (`rg -n '#\[ignore' crates/`)

| Location | Test | Reason string |
|---|---|---|
| crates/overseer-core/src/memory/notice.rs:567 | `fire_claim_child` | `"child process of fire_claims_once_across_processes"` |
| crates/overseer-core/src/memory/index.rs:1408 | `memory_bench` | none (bare `#[ignore]`; doc comment: release-mode timings) |
| crates/overseer-core/tests/audit_memory2_bench.rs:532 | `retrieval_benchmark` | `"benchmark: memory v2 retrieval quality"` |
| crates/overseer-core/tests/audit_memory2_bench.rs:662 | `scale_benchmark` | `"benchmark: memory v2 index scale"` |
| crates/overseer-core/tests/audit_memory2_stores.rs:161 | `a_moved_checkout_keeps_its_project_store` | `"deferred: H3 store identity"` |
| crates/overseer-core/src/tools/computer/cua.rs:1984 | `computer_live` | none (bare `#[ignore]`; doc comment: live macOS smoke, needs cua-driver) |

Also matched: audit_memory2_bench.rs:1 is the module doc comment, not an attribute. 6 attributes, which matches the 6 ignored in the test runs.
