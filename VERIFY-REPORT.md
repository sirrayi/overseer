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

> Parts 2 and 3 are still running and will be appended below when they finish. Part 4 was pushed first so it is not lost.

## Part 4: live model

`OPENCODE_API_KEY` set (value not printed). Binary: `target/release/overseer` built from f46ba1b. Per run: fresh `OVERSEER_HOME=$(mktemp -d)/ov`; scratch repo = `Cargo.toml` (name `scratch`, edition 2021, no deps) + `src/lib.rs` containing only `//! A tiny scratch crate.`, git-initialised with one commit. The repo was placed under `~/verify/live/runN/repo`, not /tmp, because bwrap mounts a tmpfs over /tmp. Flags per brief, plus `--json` (stdout captured). Turn 1 `--session <run>/sess`, turns 2–6 `--resume <run>/sess`, then the fresh session `--session <run>/fresh` (no resume). The three runs ran one after another. All 21 exec calls exited 0. `~/.overseer` was never created.

Every `ledger.jsonl` row had `cost_usd: 0`, and every `run_end.total_cost_usd` was 0. crates/overseer-core/src/profile.rs prices `deepseek-v4.1-flash` at 0.0 for input, cache and output, with the comment "subscription-included; the endpoint reports cost \"0\"". So both cost columns read $0 by construction.

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| review calls (ledger rows `purpose=memory_review`, sess+fresh) | 1 (sess) | 1 (sess) | 1 (sess) |
| MemoryReview events (sess / fresh) | 1 / 0 | 1 / 0 | 1 / 0 |
| when review ran | event 85, trigger `tools`, through 84 (end of turn 3) | event 69, `tools`, through 68 (end of turn 3) | event 87, `tools`, through 86 (end of turn 3) |
| review cost | $0 | $0 | $0 |
| main cost (sess / fresh) | $0 (30 calls) / $0 (6 calls) | $0 (24) / $0 (5) | $0 (31) / $0 (9) |
| review applied | 2: `user:procedural/rust-fmt-before-done.md (0.70→0.75)`, `user:procedural/rust-tests-inline-cfg-test.md (0.70→0.75)` | 2: `user:procedural/rust-fmt-before-done.md (0.70→0.75)`, `user:procedural/rust-tests-inline-mod.md (0.70→0.75)` | 3: `user:procedural/rename-rust-symbol-crate-wide.md` (new), `project:procedural/always-run-cargo-fmt-before-declaring-a-rust-tas.md (0.70→0.75)`, `project:procedural/keep-rust-unit-tests-in-an-inline-cfg-test-mod-t.md (0.70→0.75)` |
| staged / quarantined / rejected | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| review skip reasons | none (`skipped: null`) | none | none |
| review ledger row output / reasoning tokens | 39 / 474 (fresh_input 1561) | 37 / 940 (fresh_input 1645) | 214 / 667 (fresh_input 1612) |
| LearnSignal events | 3 (remember id3, correction id31, remember id87) | 3 (ids 3, 27, 71) | 3 (ids 3, 31, 89) |
| main-model `memory` tool `remember` calls in sess | 4 | 6 | 6 |
| fresh: recall notice (`memory_notice` event) | no | no | yes: `kind:recall`, notes `["user:procedural/rename-rust-symbol-crate-wide.md"]` |
| fresh: memory index lines in system_prompt.txt | 4 | 4 | 5 |
| fresh: ran `cargo fmt` | yes (`cargo fmt && cargo test`) | yes (`cargo fmt && cargo test && git diff --stat`) | yes (`cargo fmt && …`, later `cargo fmt --check`) |
| fresh: tests kept inline | yes: `mul` test added to the existing `#[cfg(test)] mod tests` in src/lib.rs; no `tests/` dir | yes, same | yes, same |
| exec wall secs t1..t6 / fresh | 13,8,18,12,8,13 / 16 | 10,16,27,8,37,6 / 11 | 15,9,41,14,9,8 / 18 |

Observed in all 3 runs: the third LearnSignal excerpt is cut to `"remember that this project targets rust 1."`. It stops at the period in "1.80".
Observed in run 3: the session put the fmt and inline-tests notes in the **project** store, while runs 1–2 put them in the **user** store. The fresh run-3 session also changed README.md (to mention `mul`). Turn 4 of run 3 had already added `rust-version = "1.80"` to Cargo.toml.
stderr on every exec: `overseer: credentials — keychain backend 'secret-tool' unavailable — fell back to env` plus the `done: …` line.

### Run 1 artifacts

MemoryReview and LearnSignal events (jq; sess, fresh had none):
```
{"s":"sess","id":3,"type":"learn_signal","kind":"remember","excerpt":"from now on, always run cargo fmt before you say a task is done."}
{"s":"sess","id":31,"type":"learn_signal","kind":"correction","excerpt":"no, don't put tests in a separate file, keep them in a #[cfg(test)] mod at the bottom"}
{"s":"sess","id":85,"type":"memory_review","trigger":"tools","through":84,"applied":["user:procedural/rust-fmt-before-done.md (0.70→0.75)","user:procedural/rust-tests-inline-cfg-test.md (0.70→0.75)"],"staged":[],"quarantined":[],"rejected":0,"skipped":null,"model":"deepseek-v4.1-flash","cost_usd":0,"taint":null}
{"s":"sess","id":87,"type":"learn_signal","kind":"remember","excerpt":"remember that this project targets rust 1."}
```

Every note created under `OVERSEER_HOME` (run 1; `.git/` and `.index/` omitted, INDEX.md shown for completeness):

`memory/INDEX.md`:
```
# Memory Index

One line per topic file: `name.md — what it's about`. Keep this index small; details live in the files.
procedural/rust-fmt-before-done.md — Always run `cargo fmt` before declaring a Rust task done.
procedural/rust-tests-inline-cfg-test.md — For Rust: keep unit tests inline in the same file inside a `#[cfg(test)] mod tes
```

`memory/procedural/rust-fmt-before-done.md`:
```
---
provenance: session:sess
confidence: 0.75
source: overseer:session/sess
added: 2026-10-06
valid_from: 2026-10-06T17:31:52Z
---
Always run `cargo fmt` before declaring a Rust task done.
```

`memory/procedural/rust-tests-inline-cfg-test.md`:
```
---
provenance: session:sess
confidence: 0.75
cues: Rust tests, cfg(test), mod tests, inline tests, separate test file
source: overseer:session/sess
added: 2026-10-06
valid_from: 2026-10-06T17:32:07Z
---
For Rust: keep unit tests inline in the same file inside a `#[cfg(test)] mod tests` block at the bottom — do not create a separate tests/ file or test module file.
```

`projects/repo-150a3486/memory/INDEX.md`:
```
# Memory Index

One line per topic file: `name.md — what it's about`. Keep this index small; details live in the files.
episodic/session-2026-10-06-sess.md — Session 2026-10-06 sess: from now on, always run cargo fmt before you say a task is d
semantic/msrv-rust-1-80-no-let-chains.md — Project "scratch" targets Rust 1.80 (MSRV). Do NOT use let-chains (`if let ... &
episodic/session-2026-10-06-fresh.md — Session 2026-10-06 fresh: add a function mul(a,b) with a test
```

`projects/repo-150a3486/memory/episodic/session-2026-10-06-fresh.md`:
```
---
provenance: engine
confidence: 0.9
valid_from: 2026-10-06T17:33:00Z
---
# Session 2026-10-06 fresh: add a function mul(a,b) with a test
Prompt: add a function mul(a,b) with a test
Later prompts: 0
Files changed: src/lib.rs
Bash calls: 2
Outcome: end_turn after 6 steps, $0.0000, model deepseek-v4.1-flash
```

`projects/repo-150a3486/memory/episodic/session-2026-10-06-sess.md`:
```
---
provenance: engine
confidence: 0.9
valid_from: 2026-10-06T17:31:48Z
---
# Session 2026-10-06 sess: from now on, always run cargo fmt before you say a task is d
Prompt: from now on, always run cargo fmt before you say a task is done. add a function add(a,b) to src/lib.rs with a test
Later prompts: 5
Files changed: src/lib.rs, README.md
Bash calls: 15
Outcome: end_turn after 30 steps, $0.0000, model deepseek-v4.1-flash
```

`projects/repo-150a3486/memory/semantic/msrv-rust-1-80-no-let-chains.md`:
```
---
provenance: session:sess
confidence: 0.7
cues: rust 1.80, MSRV, let-chains, edition 2024, if let chains
source: overseer:session/sess
added: 2026-10-06
valid_from: 2026-10-06T17:32:35Z
---
Project "scratch" targets Rust 1.80 (MSRV). Do NOT use let-chains (`if let ... && ...` / `while let ... && ...`), which are unstable until Rust 2024/1.88. Use nested `if let` / `match` instead. Note: local toolchain is rustc 1.97.1, so let-chains WOULD compile locally — passing `cargo test` does not prove 1.80 compatibility.
```

Both `MEMORY.md` files:

`memory/MEMORY.md`:
```
# Memory: user


## Index
- [[procedural/rust-fmt-before-done]] — Always run `cargo fmt` before declaring a Rust task done.
- [[procedural/rust-tests-inline-cfg-test]] — For Rust: keep unit tests inline in the same file inside a `#[cfg(test)] mod tes
```

`projects/repo-150a3486/memory/MEMORY.md`:
```
# Memory: project repo


## Index
- [[episodic/session-2026-10-06-sess]] — Session 2026-10-06 sess: from now on, always run cargo fmt before you say a task is d
- [[semantic/msrv-rust-1-80-no-let-chains]] — Project "scratch" targets Rust 1.80 (MSRV). Do NOT use let-chains (`if let ... &
- [[episodic/session-2026-10-06-fresh]] — Session 2026-10-06 fresh: add a function mul(a,b) with a test
```

## Part 2: red-team suites (release)

Env: `REDTEAM_ITERS=200000`, `REDTEAM_SEED` ∈ {1, 12648430, 20261006}, run one at a time with `timeout --kill-after=10 540 cargo test --release -q -p overseer-core --test <suite> -- --nocapture` (binaries pre-built with `--no-run`, so the times exclude compiling). Also set `REDTEAM_SECS=530`, the suites' own deadline knob (default 600 s, which is past the 9-min cap). With it, a suite that would hit the cap stops itself at 530 s and prints `iters_done`. A suite counts as "capped" when it stopped on that deadline before 200000 iterations. No suite reached the 540 s kill.

| Suite | Seed 1 | Seed 12648430 | Seed 20261006 |
|---|---|---|---|
| memory_v3_redteam_parser (5 tests) | pass, 372.6 s, A iters_done=200000 | pass, 369.4 s, A iters_done=200000 | pass, 346.6 s, A iters_done=200000 |
| memory_v3_redteam_threat (19 tests) | pass, 20.6 s, B iters_done=200000 (transformed-evasions=24834) | pass, 17.5 s, B iters_done=200000 (transformed-evasions=24901) | pass, 17.5 s, B iters_done=200000 (transformed-evasions=24876) |
| memory_v3_redteam_review (18 tests) | pass, capped at 530.2 s: C4 iters_done=10979 writes=8372 | pass, capped at 530.2 s: C4 iters_done=10888 writes=8496 | pass, capped at 530.2 s: C4 iters_done=10574 writes=8215 |
| memory_v3_redteam_store (17 tests) | pass, capped at 530.1 s: D iters_done=200000 applied=874; F iters_done=85199 (capped) | pass, capped at 530.1 s: D iters_done=200000 applied=866; F iters_done=86807 (capped) | pass, capped at 530.1 s: D iters_done=200000 applied=931; F iters_done=83793 (capped) |
| memory_v3_redteam_lock (10 tests) | pass, 10.1 s (no seed/iters knob; 6 worker processes, 0 acquire timeouts each) | pass, 10.1 s (same) | pass, 10.1 s (same) |
| audit_memory2_notes (16 tests) | pass, 0.1 s (does not read REDTEAM_*) | pass, 0.1 s | pass, 0.2 s |

All 18 suite × seed runs passed: 0 failures, so there is no minimal input to report.

- `transformed-evasions` (threat suite B) is printed for information only. B asserts that every plain payload is caught and that the scan of transformed text never panics; it does not assert that transformed text is caught (memory_v3_redteam_threat.rs, around lines 371–392).
- `REDTEAM_SLOW=1 cargo test --release -p overseer-core --test memory_v3_redteam_lock e_slow_live_holder_31s_keeps_exclusivity`: pass, 31.1 s (`e_slow_live_holder_31s_keeps_exclusivity ... ok`; its re-exec'd `e_worker ... ok`). Without `REDTEAM_SLOW`, the suite runs this test as a skip (`skipped: set REDTEAM_SLOW=1`).
