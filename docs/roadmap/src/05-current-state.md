## 3. Where We Stand: The Baseline Audit

This chapter is the honest inventory the rest of the roadmap builds on. It was produced by reading the code on the baseline (Part 0), not by reading `AGENTS.md`. File and line references are to `feat/fortify` at `cfc56a6` unless tagged **[fix2]**. Line numbers drift; the symbol names are the durable pointer.

### 3.1 The engine loop

| Mechanism | State | Where |
|---|---|---|
| ReAct loop with step and cost budgets | **[have]** `max_steps` default 100, `max_cost_usd` default $5 | `agent.rs` (budget check near the loop top) |
| Spend gate | **[have]** Worst-case pre-pricing (input at the higher of fresh and cache-write price plus `max_tokens`); clamps `max_tokens` and thinking budget to what is affordable; refuses below 1,024 output tokens; exactly one ledger row per call, answered or failed | `ledger.rs` `Gate` |
| Stuck detector | **[have]** Five patterns: same call pair 4×, same error 3×, three or more reasoning turns with no tool call or progress (monologue), ping-pong over 6, context errors 3×. First trip nudges and bumps effort; second trip ends the run | `stuck.rs`; handling in `agent.rs` |
| Empty-response guard | **[have]** Three empty responses end the run | `agent.rs` |
| Malformed-response retry | **[have]** Exactly one retry on `Malformed` | `agent.rs` |
| Provider error handling | **[partial]** `RateLimit`, `Http` and `Transport` errors end the run as `provider_error`. `retry_after_ms` is parsed (`provider/mod.rs`) but only surfaced in the message | `agent.rs`, `provider/mod.rs` |
| Verify gate | **[have]** `verify_cmd` runs in the sandbox with a fixed 120 s timeout; output tail returns to context; capped at 8 blocks | `agent.rs` |
| Stop hooks | **[have]** `stop` hooks can veto a stop; they share the verify cap and fail open | `hooks.rs`, `agent.rs` |
| Interrupt and steer | **[have]** `Control` with interrupt and a steer queue, checked at tool-launch boundaries; skipped calls get synthetic results so pairing survives | `control.rs`, `agent.rs` |
| Microagents | **[have]** `<cwd>/.overseer/microagents/**/MICROAGENT.md` with `triggers:` (substring or `always`), injected per user turn as provenance-wrapped nudges, 8 KB per body, 16 KB per turn | `microagent.rs` |
| Modes | **[have]** Six built-in modes (code, architect, ask, debug, docs, orchestrator); user-defined modes deferred | `modes.rs` |

### 3.2 Durability and recovery

| Mechanism | State | Notes |
|---|---|---|
| Append-only event log | **[have]** JSONL, hash chain v2 (sha256 over previous hash plus canonical payload) on new logs, v1 structural hash on old | `event.rs` |
| Torn-tail repair | **[have]** `EventLog::open` repairs a torn final line; a corrupt earlier line is an error naming the line | `event.rs` |
| Crash mid-tool-call | **[fix2]** Reused call-id pairing and placeholder results; torn tail keeps complete events | `fix2-agent` |
| Checkpoints and rewind | **[have]** Per user prompt; write/edit snapshot before first touch; `bash` effects not recorded | `rewind.rs`, `session.rs` |
| Resume | **[have]** `Agent::resume` rebuilds from the log; manual via `--resume`, `--continue`, `--last` | `agent.rs`, CLI |
| Liveness lock | **[have]** `live.lock` held for an agent's life; refuses a second process's `learn`, `resume` and `rewind` | `live.rs` |
| Rewind lock | **[fix2]** Rewind holds the live lock | `fix2-tools` |
| Subagent reaping | **[have]** Sidecar nonce per process; `reap_dead` marks foreign-nonce `running` tasks dead and writes a synthetic marker; delivery set prevents double drain | `tools/task/sidecar.rs` |
| Automatic resume | **[new]** Nothing restarts a dead top-level run | — |
| Supervisor | **[new]** No process owns run liveness | — |

### 3.3 Timeouts and deadlines

| Site | Bound | On expiry |
|---|---|---|
| `bash` | 120 s default, 600 s max | Kill; **[fix2]** now kills the whole process group, so `cmd &` no longer hangs the agent thread |
| `verify_cmd` | 120 s fixed | Kill, tail into context |
| `diagnostics`, `struct_search` | 120 s (+5 s pipe grace for the latter) | Kill, fail open |
| MCP calls (including initialize and `tools/list`) | 30 s | Server killed and reaped; next use respawns |
| `computer` (cua-driver) | 30 s per MCP call; `wait_for` ≤ 10 s, interrupt-aware | Error surfaces |
| `run_code` | 30 s default, 120 s max; 5 s engine grace | Error result |
| Provider HTTP | 600 s global (`ureq` `timeout_global`) | Run ends |
| Memory store lock | 10 s acquire | `BUSY` |
| Live lock | 2 s | `BUSY` |
| Ask dialog | **None** | Blocks the agent thread for as long as the human takes |
| Gateway control socket | 10 s request deadline, 64 KiB lines, 16 connections; **[fix2]** reply shares the request deadline | Error |
| Telegram | 15 s per call, long-poll 0 | Error; runs inside the daemon tick, so a slow call stalls the tick |
| Desktop notifier | **None** (`Command::status()` on `osascript`/`notify-send`) | Can stall the daemon tick |
| Spawned gateway runs | 600 s watchdog | Kill |
| Wall-clock run budget | **None** | — |

### 3.4 Context and tokens

| Mechanism | State |
|---|---|
| Prompt assembly | **[have]** `prompt::assemble` emits named segments in a frozen order: identity, contract, safety, memory, skills, MCP, persona. Assembled once per agent (start, resume, model switch). Fingerprint and hygiene lints in tests |
| Anthropic caching | **[have]** Three of four breakpoints: last cacheable system segment, tools tail, rolling breakpoint on the last eligible message block. 5-minute TTL only |
| OpenAI caching | **[have]** `prompt_cache_key` = session id, official hosts only; cache fields parsed for Responses and DeepSeek |
| Gemini caching | **[have]** Implicit only; `cachedContentTokenCount` parsed |
| Compaction | **[have]** Deterministic, view-only, two-turn tail on model-response boundaries, fixed-schema mechanical summary, triggered at a per-profile fraction (0.70–0.83) of context, by estimator, or on context-window errors |
| Result budgeting | **[have]** 30,000-character inline cap; TOON re-encoding of uniform JSON arrays first; credential redaction; spill to `tool-outputs/output-N.txt` (0600) with a 4K preview; middle truncation fallback |
| Eviction | **[have]** Keep the last 5 tool results; pin the last 3 `tools op=search` results; keep the last 2 images; reflection nudges keep last 1. Count-based, not token-based |
| Read dedup | **[have]** `[unchanged]` stub on a (path, mtime, range) hit |
| Token estimation | **[partial]** Characters-per-token heuristic calibrated per family (`tokens.rs`); no real tokenizer |
| Bounded prompt artifacts | **[have]** Memory resident ≤ 4,000 bytes / 24 lines; INDEX ≤ 25 KB; CORE ≤ 1.5 KB; repo map ≤ 4,000 bytes; subagent digest ≤ 8,000 characters |
| Telemetry | **[have]** `UsageRecord` per call with cache hit rate; `CacheStats` with an alert below 0.90; per-run cache delta on `RunEnd`; `overseer stats`; TUI shows "NN% cached" |

### 3.5 Tools

| Mechanism | State |
|---|---|
| Registry | **[have]** 18 core names; specs name-sorted and byte-stable; optional tools detected once per registry (`computer`, `struct_search`, `diagnostics`, `skill`) |
| Advertised by default | **[have]** `bash`, `read`, `write`, `edit`, `grep`, `glob`, `task`, `memory`, `tools`, plus `run_code` (feature `code-mode`) and `skill` when a skill exists |
| Deferred behind `tools` | **[have]** `computer`, `diagnostics`, `mcp`, `plan`, `repo_map`, `struct_search`, `symbol`, and every MCP tool |
| Call pipeline | **[have]** disabled → unavailable → pre-tool hooks → broker sensitive latch → schema check → gate → dispatch → memory observe → policy notes → post-tool hooks → taint on raw text → credential sanitize → budget |
| `run_code` | **[have]** QuickJS with no `std`/`os`/modules; 64 MB heap, 1 MB stack, 64 sub-calls, 8 MB sub-call bytes, 16K print, ≤ 120 s; cannot reach `computer`, `memory`, `plan`, `run_code`, `skill`, `task`, `tools` |
| MCP | **[have]** `~/.overseer/mcp.json` only, `${VAR}` expansion, per-server `trust: read|ask`, lazy spawn, respawn on failure, env allowlist, shadowing names skipped |
| Computer use | **[fix2]** Lean pass: cua-driver is the only backend; model-visible output stripped of audit fields; model-sized screenshots with one refit retry; lean observation rendering; snapshot-gated post-reads; whole batch validated before dispatch |
| Web access | **[new]** No web fetch or search tool; bash network is denied by default |

### 3.6 Orchestration

| Mechanism | State |
|---|---|
| Task modes | **[have]** read (read/grep/glob/memory), write (full core in a worktree), verify (read plus sandboxed bash), consult (one heavy call, no tools) |
| Tiers | **[have]** light/standard/heavy derived from profiles, same transport only; defaults read→light, write/verify→standard, consult→heavy |
| Budget composition | **[have]** Cap = min(requested or tier default, parent remaining); reservations at launch; settlement into the parent ledger. Tier caps: light $0.25, standard $1, heavy $2, consult $0.50 |
| Worktrees | **[have]** `overseer/<sha8 of session path>/task-N` branches; no-diff writers cleaned up; **[fix2]** gitignored writer output counts as a change |
| Background | **[have]** Max 4 in flight (config), digests ≤ 8,000 characters as `SubagentDone` notices; drained at the loop top |
| Escalation | **[have]** read/consult retry once one tier up on step cap, stuck, empty or error; writers never auto-retry |
| Verify | **[have]** Fresh-context verifier with JSON verdict and git tamper evidence; unparsable verdict is `unknown`, never pass |
| Cancellation | **[have]** `task action=cancel` at the next step boundary; parent interrupts fan out; **[fix2]** cancel holds the reservation until settlement |
| Subagent limits | **[have]** ≤ 20 steps, no auto-compact, no recall, no learning, filtered read-only memory |
| Task dependencies | **[deferred]** No `after:`; `task.rs` carries the DEFERRED marker |
| Shared artifacts | **[new]** Only digests and incidental files |
| Depth cap | **[new]** Writers see `task` and can spawn grandchildren; money is bounded by the account chain, depth is not |
| Persistent agents | **[new]** Every subagent is ephemeral |
| Foreground detach | **[new]** A foreground `task` blocks the parent for its whole run |

### 3.7 Memory

| Mechanism | State |
|---|---|
| Stores | **[have]** User and project, git-versioned, five layers with ADD-only INDEX files |
| Search | **[have]** In-memory BM25F fused by RRF with Petrov activation and confidence; activation and confidence vote only within the lexical top tier |
| Writes | **[have]** ≤ 5 per user turn; per-layer write bars; supersede refused under the untrusted latch; quarantine to `proposals/` |
| Recall and reminders | **[have]** ≤ 3 notes on user input with a trivial-prompt skip; prospective `at:`/`kw:`/`path:` reminders, fired once |
| Learning review | **[have]** Run-end review with signal lexicon and cadence, ≤ 14K digest, line protocol, write policy, pending queue, threat scan; **[fix2]** fixes for review findings M1–M7 and S2–S4 (finding numbers from the fix2-memory review, not workstreams in this roadmap) |
| Store IO safety | **[have]** No link following below the root, temp-file plus rename, `O_NOFOLLOW` reads, flock with 10 s acquire, drift check on unlocked model calls |
| History search, learned-skill application, dream pass, mounts, sync | **[deferred]** Memory v3 H2 and H3 |

### 3.8 Policy and rules

| Mechanism | State |
|---|---|
| Presets and ladder | **[have]** ReadOnly, WorkspaceWrite, Plan; irreversibility classes Read, InternalWrite, ExternalComms, Money, Identity; per-domain autonomy from Observe to ActSilently |
| Hard deny | **[have]** Containment, bash deny globs, escalation tokens, ahead of every allow |
| Persisted rules | **[partial]** `~/.overseer/rules`: one `tool:resource` key per line, optional `@turns=N` TTL; exact match; global scope; append-only; no deny entries; no list or remove surface. **[fix2]** grant keys now exist for non-bash tools, and a persisted grant key can no longer smuggle an `@turns=` TTL (a hand-written trailing `@turns=N` still parses) |
| Rule-of-Two | **[have]** Untrusted × sensitive latches arm the exfil gate; `Tainted{latch}` events; re-armed on resume; **[fix2]** untrusted floor survives approval |
| Hooks | **[have]** `hooks.json` data rules: `pre_tool_use` blocks, `post_tool_use` annotates, `stop` vetoes |
| AGENTS.md | **[new]** Never enters the prompt. Project conventions reach the model only through microagents, memory, or the model reading the file itself |
| Instruction compiler | **[new]** Each source is consulted where it is used; no single ordering, provenance or explanation |

### 3.9 Gateway and automation

| Mechanism | State |
|---|---|
| Daemon | **[have]** Single-threaded tick: kill switch → control drain → trigger poll → process → reap → heartbeat |
| Triggers | **[have]** Interval, file watch (mtime), heartbeat, 5-field cron, webhook spool, Telegram poll |
| Pipeline | **[have]** Dedup → triage (first-match class rules: Ignore, Notify, DraftForReview, Act) → untrusted Act downgraded to DraftForReview → EV gate (benefit − cost > θ, quiet hours, focus) → push, inbox or silent |
| Spawn | **[have]** `overseer exec --bare --max-steps N` child; max 2 concurrent; overflow to inbox; untrusted origins get `--autonomy external=approve` and an env marker; 600 s watchdog |
| Inbox and outbox | **[have]** Durable stores; draft → approve → send |
| Webhook replay | **[fix2]** Replay table never evicts young entries |
| Workflows | **[new]** One event → at most one run. No chains, conditions, retries, fan-out, compensation or versioned definitions |

### 3.10 Security

| Mechanism | State |
|---|---|
| Startup hardening | **[have]** umask 0o077, proxy-env scrub, owner-only roots |
| Sandbox | **[have]** `sandbox-exec` on macOS, `bwrap` on Linux, network denied, writes confined, secret dirs read-denied; pinned runtimes never downgrade |
| Credential broker | **[partial]** HMAC sentinels under a per-process key; consent recorded for audit; rate and window enforcement not wired |
| Web surface | **[have]** Per-install token in the URL fragment, Host allowlist, Origin and Sec-Fetch-Site checks, size and time limits, CSP |
| Injection corpus | **[have]** `tests/injection_asr.rs` gates the latches |
| Supply chain | **[have]** `cargo deny` |
| Egress proxy | **[new]** v1 denies all egress; a domain-allowlist proxy is still open |
| Signed builds | **[new]** Waits on the Apple Developer account |

### 3.11 Evaluation

| Mechanism | State |
|---|---|
| Rig | **[have]** Task spec v2 with mandatory oracle, deterministic graders, k-seed scheduler, immutable store, paired bootstrap and McNemar statistics, report cards, frozen bash-only control scaffold |
| Corpus | **[have]** 40 public tasks plus 5 canary held-out tasks, all oracle-verified (the corpus grew from 35 to 40 in commit `e875a8f`; `eval/README.md` still says 35 and gets corrected in W0) |
| External adapters | **[have]** SWE-bench Verified (gold patch verified), Terminal-Bench 2.x and SWE-rebench via Harbor, τ², LiveCodeBench |
| Live results | **[new]** None. The eval hold is in force and no paid runs have happened |
| Long-horizon, chaos and memory suites | **[new]** — |

### 3.12 Known open items carried into this roadmap

From `docs/LEAVING-OFF.md`, the PR Deferred sections and the fix reports:

- Memory v3 H2 (history search, applied learned skills) and H3 (dream pass, mounts, sync).
- Live computer-use validation on the owner's Mac.
- A live run on a priced model: the spend gate and review-share cap have only been exercised at $0.
- Orchestration wave B: task DAG, fan-out, shared scratchpad, writer retry.
- The eval hold.
- Two `fix2-agent` reviewer tests that stay ignored, with replacement tests through the real paths.
- `release.yml`'s `plan` job fails on every PR; `pr-run-mode = "skip"` would stop it.
- Rotate the provider key pasted in the 2026-09-20 session.
- Move the repository out of iCloud-synced Desktop.
- Bash background processes are now killed when the command finishes; only `setsid` survives (behaviour change from `fix2-tools`).
- Subagent worktrees can pile up now that gitignored output counts as change.
