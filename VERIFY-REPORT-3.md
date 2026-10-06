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

## Part 2: live check of the review fix

`OPENCODE_API_KEY` was set (value not printed). 2 runs, each with a fresh `OVERSEER_HOME` and a fresh `cargo init --lib` git repo. The command was `target/release/overseer exec --provider opencode --model deepseek-v4.1-flash --small-model deepseek-v4.1-flash --max-cost 0.50 --cwd <repo>`, with `--session <dir>` on turn 1 and `--resume <dir>` on turns 2 to 6. All 12 turns exited 0. The two runs ran at the same time, in separate homes, repos and session dirs. Every cost is $0 because profile.rs prices this model at 0.

`max_tokens`: **not logged.** `UsageRecord` (ledger.rs) has no `max_tokens` field, no review ledger row in either run contains one, and the `MemoryReview` event doesn't carry it either.

### Run 1

Review ledger rows (`purpose: memory_review`). Each review has exactly 1 row, so there were no retries.

| # | turn | fresh_input | output | reasoning | output+reasoning | max_tokens |
|---|---|---|---|---|---|---|
| 1 | 2 | 1475 | 94 | 939 | 1033 | not logged |
| 2 | 6 | 1910 | 118 | 3165 | 3283 | not logged |

`MemoryReview` events:

| event id | trigger | skipped | applied | staged | quarantined | rejected |
|---|---|---|---|---|---|---|
| 37 (turn 2) | signal | null | `project:semantic/rust-tests-inline-in-src.md`, `user:procedural/cargo-fmt-before-done.md (0.70→0.75)` | [] | [] | 0 |
| 113 (turn 6) | tools | null | `project:semantic/cargo-edition-msrv-mismatch.md` | [] | [] | 0 |

Notes on disk at the end:
- Review applied 3 ops. Two are new notes with provenance `review:session:sess` (`project semantic/rust-tests-inline-in-src.md`, `project semantic/cargo-edition-msrv-mismatch.md`). The third raised the confidence of a main-written note (`user procedural/cargo-fmt-before-done.md`, provenance `session:sess`; the file now reads `confidence: 0.75`).
- Main agent wrote 2 notes (provenance `session:sess`): `user procedural/cargo-fmt-before-done.md` and `project semantic/rust-target-1-80-no-let-chains.md`.
- Engine: 1 episodic note (`provenance: engine`).
- Staged: 0 (no `pending/` dir). Quarantined: 0.

### Run 2

Review ledger rows. Again 1 row per review, so no retries.

| # | turn | fresh_input | output | reasoning | output+reasoning | max_tokens |
|---|---|---|---|---|---|---|
| 1 | 1 | 1055 | 76 | 667 | 743 | not logged |
| 2 | 3 | 1235 | 91 | 4948 | 5039 | not logged |

`MemoryReview` events:

| event id | trigger | skipped | applied | staged | quarantined | rejected |
|---|---|---|---|---|---|---|
| 25 (turn 1) | signal | null | [] | `u-d0d64971` | [] | 0 |
| 65 (turn 3) | signal | null | `project:semantic/rust-tests-inline.md`, `project:episodic/session-2026-10-06-sess.md (0.90→0.95)` | [] | [] | 0 |

Notes on disk at the end:
- Review applied 2 ops. One is a new note with provenance `review:session:sess` (`project semantic/rust-tests-inline.md`). The other raised the confidence of the engine episodic note. That file reads `confidence: 0.9` at session end; the episode is rewritten at each run end.
- Review staged 1: `user pending/u-d0d64971.json`, an `op: add` with target `profile/run-fmt-before-declaring-done.md` and origin `review:session:sess`.
- Main agent wrote 2 notes (provenance `session:sess`): `user prospective/cargo-fmt-before-done.md` and `project semantic/project-rust-1-80-no-let-chains.md`.
- Quarantined: 0.

### Observations (facts)
- No review was skipped, and no review in either run came back without text (every `skipped` is null, and each review has exactly one ledger row).
- No review row reports `max_tokens`. Two rows billed output+reasoning above 1,200 (run 1 #2: 3283; run 2 #2: 5039), so those calls had a limit above 1,200. That is an inference from the token counts; the limit itself isn't logged.
- In both runs the review with output+reasoning above 1,200 came after an earlier review row that had billed reasoning (> 0) in the same session ledger, and in a later `--resume` process.
