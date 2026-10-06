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

## Part 4: live model (OPENCODE_API_KEY set; value not printed)
Command per turn: `target/release/overseer exec --provider opencode --model deepseek-v4.1-flash --small-model deepseek-v4.1-flash --max-cost 0.50 --cwd <repo>`, `--session <base>/s1` for turn 1, `--resume <base>/s1` for turns 2-6, then a fresh `--session <base>/s2` with `add a function mul(a,b) with a test`. Each run: fresh `OVERSEER_HOME=$(mktemp -d)/ov`, and a fresh repo from `cargo new --lib` (crate `tinycrate`) plus one commit. Every invocation exited 0. This Part ran concurrently with Part 2 (CPU-bound tests on the same VM).

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| review calls (ledger rows `purpose: memory_review`) | 3 | 6 | 1 |
| review cost | $0 | $0 | $0 |
| main cost (all non-review rows, s1+s2) | $0 (27+6 calls) | $0 (24+6 calls) | $0 (24+9 calls) |
| MemoryReview events | 2 (after turns 2, 6) | 3 (after turns 4, 5, 6) | 1 (after turn 3) |
| notes applied / staged / quarantined / rejected | 2 / 0 / 0 / 0 | 0 / 0 / 0 / 0 | 2 / 0 / 0 / 0 |
| review skip reasons | none (`skipped: null` x2) | `review call produced no text` x3 | none (`skipped: null`) |
| review rows `output`/`reasoning` (in order) | 55/494; 0/1200; 4/1964 | 0/1200; 0/2400; 0/1200; 0/2400; 0/1200; 0/2400 | 39/856 |
| LearnSignal events | remember, correction, remember | remember, correction, remember | remember, correction, remember |
| fresh session: recall notice (`memory_notice` event) | no (0 memory_notice events) | no | no |
| fresh session: ran `cargo fmt` | yes (1 bash call) | yes (1) | yes (2: `cargo fmt`, `cargo fmt --check`) |
| fresh session: tests inline | yes (`#[cfg(test)] mod tests` in src/lib.rs, no tests/ dir) | yes | yes |
| turn wall times (s), turns 1-6 + fresh | 14, 46, 13, 40, 8, 44, 17 | 34, 13, 10, 32, 31, 29, 17 | 19, 10, 18, 19, 17, 12, 16 |

Other facts:
- All "applied" entries are confidence bumps on notes that the main loop had already written through the `memory` tool (`... (0.70→0.75)`). The main loop made 9 / 6 / 4 `memory` tool calls in s1 of runs 1/2/3. No `memory_updated` events.
- Fresh session (s2): the system prompt's `## Memory index` section listed the s1 episode, the Rust-1.80 note and the cargo-fmt note (`user:procedural/...`). s2 made 0 / 1 (`search`) / 2 (`get` of the MSRV note, then `remember`) `memory` calls in runs 1/2/3.
- Review row limits: every row with `output` 0 has `reasoning` at exactly 1200 or 2400. agent.rs:2246-2253 `review_max_tokens` returns 1,200 + 4,000 only when `profile::lookup(model).reasons()`. `reasons()` (profile.rs:153) checks `accepted_params` for `reasoning_effort`/`thinkingConfig`. deepseek-v4.1-flash uses `GATEWAY_PARAMS` (profile.rs:97-103: temperature, top_p, frequency_penalty, presence_penalty, max_tokens), so its review limit is 1,200, with one retry at 2,400 (agent.rs:2105-2107). profile.rs:243-244 says "Both rows reason". The 5,200 headroom described in AGENTS.md:30 was not applied to this model in these runs.
- No review was skipped for `budget` or `review-share` in any run.

### Run 1 artifacts
MemoryReview and LearnSignal events (s1; `prev_hash`/`hash`/`parent_id` removed). s2 had none.
```
{"id":3,"ts_ms":1791310222584,"type":"learn_signal","kind":"remember","excerpt":"from now on, always run cargo fmt before you say a task is done."}
{"id":27,"ts_ms":1791310236355,"type":"learn_signal","kind":"correction","excerpt":"no, don't put tests in a separate file, keep them in a #[cfg(test)] mod at the bottom"}
{"id":65,"ts_ms":1791310282605,"type":"memory_review","trigger":"tools","through":64,"applied":["user:procedural/always-run-cargo-fmt-before-declaring-a-rust-tas.md (0.70→0.75)","user:procedural/when-adding-tests-to-rust-code-never-use-a-separ.md (0.70→0.75)"],"staged":[],"quarantined":[],"rejected":0,"skipped":null,"model":"deepseek-v4.1-flash","cost_usd":0,"taint":null}
{"id":95,"ts_ms":1791310295303,"type":"learn_signal","kind":"remember","excerpt":"remember that this project targets rust 1."}
{"id":129,"ts_ms":1791310387024,"type":"memory_review","trigger":"tools","through":128,"applied":[],"staged":[],"quarantined":[],"rejected":0,"skipped":null,"model":"deepseek-v4.1-flash","cost_usd":0,"taint":null}
```

Every note file under `OVERSEER_HOME` (excluding INDEX.md/MEMORY.md, which follow; `.git` and `.index/` omitted), full text:

`ov/memory/procedural/always-run-cargo-fmt-before-declaring-a-rust-tas.md`
```
---
provenance: session:s1
confidence: 0.75
source: overseer:session/s1
added: 2026-10-06
valid_from: 2026-10-06T18:10:27Z
---
Always run `cargo fmt` before declaring a Rust task done.
```

`ov/memory/procedural/when-adding-tests-to-rust-code-never-use-a-separ.md`
```
---
provenance: session:s1
confidence: 0.75
cues: tests location, cfg(test) mod, separate test file, tests/ directory, Rust testing
source: overseer:session/s1
added: 2026-10-06
valid_from: 2026-10-06T18:10:54Z
---
When adding tests to Rust code: never use a separate test file (no tests/ directory, no tests.rs / integration tests). Always keep tests in a `#[cfg(test)] mod tests { use super::*; ... }` at the bottom of the same source file.
```

`ov/projects/repo-1d05e017/memory/episodic/session-2026-10-06-s1.md`
```
---
provenance: engine
confidence: 0.9
valid_from: 2026-10-06T18:10:22Z
---
# Session 2026-10-06 s1: from now on, always run cargo fmt before you say a task is d
Prompt: from now on, always run cargo fmt before you say a task is done. add a function add(a,b) to src/lib.rs with a test
Later prompts: 5
Files changed: src/lib.rs, README.md
Bash calls: 12
Outcome: end_turn after 27 steps, $0.0000, model deepseek-v4.1-flash
```

`ov/projects/repo-1d05e017/memory/episodic/session-2026-10-06-s2.md`
```
---
provenance: engine
confidence: 0.9
valid_from: 2026-10-06T18:13:07Z
---
# Session 2026-10-06 s2: add a function mul(a,b) with a test
Prompt: add a function mul(a,b) with a test
Later prompts: 0
Files changed: src/lib.rs
Bash calls: 3
Outcome: end_turn after 6 steps, $0.0000, model deepseek-v4.1-flash
```

`ov/projects/repo-1d05e017/memory/semantic/this-project-targets-rust-1-80-as-its-toolchain.md`
```
---
provenance: session:s1
confidence: 0.7
cues: rust 1.80, MSRV, minimum supported rust version, let-chains, let chains, edition 2024, unstable feature, newer than 1.80
source: overseer:session/s1
added: 2026-10-06
valid_from: 2026-10-06T18:11:59Z
---
This project targets Rust 1.80 as its toolchain/MSRV. Do NOT use let-chains (`if let Some(x) = opt && cond { }` / `while let ... && ...`) — they were only stabilized in Rust 1.88 and are a parse error on 1.80. Avoid any language or std feature newer than 1.80; when unsure, check the stabilization version before using it. Note: the local `rustc` is 1.97.1, so newer features will compile locally but break the target — do not trust a local build as proof of 1.80 compatibility.
```

`ov/memory/MEMORY.md`
```
# Memory: user


## Index
- [[procedural/always-run-cargo-fmt-before-declaring-a-rust-tas]] — Always run `cargo fmt` before declaring a Rust task done.
- [[procedural/when-adding-tests-to-rust-code-never-use-a-separ]] — When adding tests to Rust code: never use a separate test file (no tests/ direct
```

`ov/projects/repo-1d05e017/memory/MEMORY.md`
```
# Memory: project repo


## Index
- [[episodic/session-2026-10-06-s1]] — Session 2026-10-06 s1: from now on, always run cargo fmt before you say a task is d
- [[semantic/this-project-targets-rust-1-80-as-its-toolchain]] — This project targets Rust 1.80 as its toolchain/MSRV. Do NOT use let-chains (`if
- [[episodic/session-2026-10-06-s2]] — Session 2026-10-06 s2: add a function mul(a,b) with a test
```

`ov/memory/INDEX.md`
```
# Memory Index

One line per topic file: `name.md — what it's about`. Keep this index small; details live in the files.
procedural/always-run-cargo-fmt-before-declaring-a-rust-tas.md — Always run `cargo fmt` before declaring a Rust task done.
procedural/when-adding-tests-to-rust-code-never-use-a-separ.md — When adding tests to Rust code: never use a separate test file (no tests/ direct
```

`ov/projects/repo-1d05e017/memory/INDEX.md`
```
# Memory Index

One line per topic file: `name.md — what it's about`. Keep this index small; details live in the files.
episodic/session-2026-10-06-s1.md — Session 2026-10-06 s1: from now on, always run cargo fmt before you say a task is d
semantic/this-project-targets-rust-1-80-as-its-toolchain.md — This project targets Rust 1.80 as its toolchain/MSRV. Do NOT use let-chains (`if
episodic/session-2026-10-06-s2.md — Session 2026-10-06 s2: add a function mul(a,b) with a test
```
