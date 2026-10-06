# VERIFY-REPORT-3: feat/fortify @ f6d41a3b14aefe3fb7f87823255d57899792287a

Verification only; no code changed. VM: Ubuntu, 8 vCPU, rustc 1.97.1, cargo 1.97.1, bubblewrap 0.6.1 (installed for this run), cargo-deny 0.20.2.

## Part 1: gates

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS (exit 0, no output) |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS (exit 0, 0 warnings, 0 errors) |
| `cargo test --workspace --no-fail-fast` run 1 | PASS: 1388 passed, 0 failed, 6 ignored (47 test binaries / doc-test sections) |
| `cargo test --workspace --no-fail-fast` run 2 | PASS: 1388 passed, 0 failed, 6 ignored |
| `cargo test --workspace --no-fail-fast` run 3 | PASS: 1388 passed, 0 failed, 6 ignored |
| `cargo deny check` | PASS: advisories ok, bans ok, licenses ok, sources ok (0 warnings) |
| `cargo build --release -p overseer-cli` | PASS (1m 49s); `target/release/overseer` 9,859,928 bytes; `--version` prints `overseer 0.1.0` |

Counting method: the last `test result:` line in each `Running`/`Doc-tests` section, so the re-exec'd child-process results aren't double-counted.

Flake list (3 runs): none. No test failed in any run.

Ignored (6, the same in every run): `memory::index::tests::memory_bench`, `memory::notice::tests::fire_claim_child` (child process of fire_claims_once_across_processes), `tools::computer::cua::tests::computer_live`, `retrieval_benchmark` (benchmark), `scale_benchmark` (benchmark), `a_moved_checkout_keeps_its_project_store` (deferred: H3 store identity).
