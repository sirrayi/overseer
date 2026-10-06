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

## Part 2: live check of the review fix (OPENCODE_API_KEY set; value not printed)

Command per turn: `target/release/overseer exec --provider opencode --model deepseek-v4.1-flash --small-model deepseek-v4.1-flash --max-cost 0.50 --cwd <repo>`, with `--session <base>/s1` for turn 1 and `--resume <base>/s1` for turns 2-6, using the 6 turn prompts from the brief. Each run had a fresh `OVERSEER_HOME=<base>/ov` and a fresh repo from `cargo new --lib --name tinycrate` plus one commit. Every invocation exited 0. Each turn is a separate `overseer exec` process. These runs ran at the same time as Part 1 test runs 2 and 3.

| | run 1 | run 2 |
|---|---|---|
| turn wall times (s), turns 1-6 | 64, 7, 47, 14, 52, 20 | 18, 13, 38, 11, 37, 39 |
| ledger rows total / `purpose: memory_review` | 35 / 2 | 36 / 5 |
| MemoryReview events | 2 | 3 |
| review cost | $0 (model priced 0.0) | $0 |

### Review ledger rows
`max_tokens` is **not logged**: ledger.jsonl rows have only `cache_hit_rate, cache_read, cache_write, cost_usd, fresh_input, latency_ms, model, output, reasoning, request_bytes, tool_calls, ts_ms` (plus `purpose`), and the events have no max_tokens field either.

| run | row | when (by ts) | output | reasoning | fresh_input | cache_read | latency ms |
|---|---|---|---|---|---|---|---|
| 1 | 1 | end of turn 1 | 4 | 750 | 1137 | 0 | 5825 |
| 1 | 2 | end of turn 3 | 90 | 998 | 1431 | 0 | 8031 |
| 2 | 1 | end of turn 2 | 91 | 723 | 1379 | 0 | 6309 |
| 2 | 2 | end of turn 5 | 0 | 1200 | 1648 | 0 | 8181 |
| 2 | 3 | end of turn 5 (retry) | 0 | 3600 | 112 | 1536 | 23465 |
| 2 | 4 | end of turn 6 | 0 | 1200 | 1798 | 0 | 8200 |
| 2 | 5 | end of turn 6 (retry) | 0 | 3600 | 6 | 1792 | 21566 |

Inference (max_tokens isn't logged, so this comes from the code): the rows that came back with no text consumed exactly 1200 and then 3600 reasoning tokens. Those numbers match a first attempt at `review_max_tokens` = 1,200 and a retry at `min(12_000, max(2×1_200, 1_200 + 2_400))` = 3,600 (agent.rs:2170-2180). In run 2, the turn-2 review billed `reasoning` 723 > 0, but the reviews in turns 5 and 6 still started at 1,200. Observed in the code: `review_reasoners` is initialised empty in both `Agent::start` (agent.rs:459) and the resume path (agent.rs:601), and is documented as "Session-scoped — never persisted". Each `--resume` turn is a new process, so the set doesn't carry over between turns. The 3,600 retry was used up with no text both times.

### MemoryReview events (`prev_hash`/`hash`/`parent_id` removed)
Run 1:
```
{"id": 61, "type": "memory_review", "trigger": "tools", "through": 60, "applied": [], "staged": [], "quarantined": [], "rejected": 0, "skipped": null, "model": "deepseek-v4.1-flash", "cost_usd": 0.0, "taint": null}
{"id": 107, "type": "memory_review", "trigger": "signal", "through": 106, "applied": ["user:procedural/rust-inline-unit-tests.md", "user:procedural/fmt-before-done.md (0.70→0.75)"], "staged": [], "quarantined": [], "rejected": 0, "skipped": null, "model": "deepseek-v4.1-flash", "cost_usd": 0.0, "taint": null}
```
Run 2:
```
{"id": 57, "type": "memory_review", "trigger": "signal", "through": 56, "applied": ["project:semantic/rust-tests-in-cfg-test-mod.md", "user:procedural/user-requires-running-cargo-fmt-before-declaring.md (0.70→0.75)"], "staged": [], "quarantined": [], "rejected": 0, "skipped": null, "model": "deepseek-v4.1-flash", "cost_usd": 0.0, "taint": null}
{"id": 122, "type": "memory_review", "trigger": "tools", "through": 56, "applied": [], "staged": [], "quarantined": [], "rejected": 0, "skipped": "review call produced no text", "model": "", "cost_usd": 0.0, "taint": null}
{"id": 138, "type": "memory_review", "trigger": "tools", "through": 56, "applied": [], "staged": [], "quarantined": [], "rejected": 0, "skipped": "review call produced no text", "model": "", "cost_usd": 0.0, "taint": null}
```
The two skipped reviews in run 2 kept `through` at 56, so the cursor did not advance.

### Notes
Every note under `OVERSEER_HOME` except INDEX.md/MEMORY.md, with `provenance` and `confidence`:

| run | note | provenance | confidence |
|---|---|---|---|
| 1 | `memory/procedural/fmt-before-done.md` | `session:s1` (main loop) | 0.75 (review bump 0.70→0.75) |
| 1 | `memory/procedural/rust-inline-unit-tests.md` | `review:session:s1` (review) | 0.6 |
| 1 | `projects/repo-4e5af0c5/memory/procedural/rust-1-80-no-let-chains.md` | `session:s1` (main loop) | 0.7 |
| 1 | `projects/repo-4e5af0c5/memory/episodic/session-2026-10-06-s1.md` | `engine` | 0.9 |
| 2 | `memory/procedural/user-requires-running-cargo-fmt-before-declaring.md` | `session:s1` (main loop) | 0.75 (review bump 0.70→0.75) |
| 2 | `projects/repo-a9c920ab/memory/semantic/rust-tests-in-cfg-test-mod.md` | `review:session:s1` (review) | 0.6 |
| 2 | `projects/repo-a9c920ab/memory/semantic/rust-1-80-no-let-chains.md` | `session:s1` (main loop) | 0.7 |
| 2 | `projects/repo-a9c920ab/memory/episodic/session-2026-10-06-s1.md` | `engine` | 0.9 |

| | run 1 | run 2 |
|---|---|---|
| review `applied` entries | 2: 1 new note (provenance `review:`) + 1 confidence bump on a main-loop note | 2: 1 new note (provenance `review:`) + 1 confidence bump on a main-loop note |
| notes written by main loop (`session:s1`) | 2 | 2 |
| staged | 0 (all `staged: []`; no `pending/` dir in either store) | 0 (same) |
| quarantined | 0 | 0 |
| rejected | 0 | 0 |
