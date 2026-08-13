# OVERSEER — Implementation Status & Master Checklist

> Living document. Updated after every batch slice.
> Source of truth for what exists, what passes, what's next.

## Master checklist (all batches)

| Batch | Status | Notes |
|---|---|---|
| B0 Foundation | APPROVED | Qwen round 1: 10 critical/25 major/10 minor — all fixed, packet rebuilt verbatim, resubmitted. Round 2: APPROVED (upload corruption identified as the mangling cause, not the code). |
| B1 Robot Body | APPROVED | Qwen round 2: verified live repo — symlink containment, denylist hardening, streaming, trust labels, structured denial all confirmed. 126 tests. |
| B2 CLI + Sessions | APPROVED | Qwen: verified live repo at ea21c91 — session lifecycle, streaming, CLI surface, stubs, safety, efficiency all confirmed. 3 minor notes carried into B3 (all fixed). |
| B3 Episodic Memory | APPROVED | Qwen: verified live repo at 3cd91c5 — O(1) append, WAL + RLock, FTS5 + redaction, observer hooks, derived-cache rebuild all confirmed. 175 tests. |
| B4 Verification + Repo Intel | APPROVED | Qwen: verified live repo at 194eb1f — project detection, repo maps, failure cards, rollback checkpoints, git tools, verifier hook all confirmed. 204 tests. |
| B4.5 Live Learning | APPROVED | Qwen: verified live repo at deba794 — signal detector, session memory, provisional candidates, untrusted blocking, latency budgets all confirmed. 219 tests. |
| B5 Knowledge Layer | APPROVED | Qwen: verified live repo at 0eb9c8f — confidence tiers, extraction, vault consolidation, dedup, conflict flags, evidence linking, retrieve API all confirmed. 233 tests. |
| B6 Context Compiler | APPROVED | Qwen: verified live repo at 7f3aecc — budget+reserve, tiered assembly, eviction, progressive disclosure, stable prefix, loop hook all confirmed. 242 tests. CI green. |
| B7 Recursive Learning | APPROVED | Qwen: verified live repo at fc97362 — PatternMiner evidence gates, correction replay, SkillRegistry governed writes, promotion gates all confirmed. 259 tests. |
| B8 Routing + Economy | APPROVED | Qwen: verified live repo — complexity router, privacy routing, power-mode ceilings, telemetry + budget guard, cost CLI, loop integration all confirmed. 3 notes fixed (tier telemetry, provider_tiers config, TIER_NAMES). 275 tests. |
| B9 Flesh + Integrations | APPROVED | Qwen: verified live repo at 1bb4260 — MCP client/server approval routing, untrusted labeling, subagent isolation, packaging, all B8 notes fixed. 285 tests. |
| B10 Recursive Closure | APPROVED | Qwen: verified live repo at feaed90 — meta-stats, proposal generation with forbidden targets, shadow canary evaluation, L3 guardrail all confirmed. 296 tests. |
| B11 Adapter Pipeline | IN PROGRESS (slices 1-3 done) | dataset builder (redacted pairs + traces), adapter registry (validation gate, hot-swap, rollback), power-aware training hook, opt-in flags, CLI. 315 tests. CI green. |
| B2 CLI | pending | full command surface, sessions, budget display |
| B3 Episodic Memory | pending | observation stream, FTS5, session notes |
| B4 Verification + Repo Intelligence | pending | project detection, repo maps, targeted tests, rollback |
| B4.5 Live Learning Engine | pending | 5 speeds, signal detector, provisional memory |
| B5 Knowledge Layer | pending | facts/prefs/corrections, salience, prune |
| B6 Context Compiler | pending | budget engine, tiers, token accounting |
| B8 Routing, Economy, Power Governor | pending | power modes, budgets, telemetry |
| B9 Adapter Pipeline | pending | dataset builder, MLX LoRA, validation gate |
| B10 Recursive Closure | pending | meta-stats, L2/L3 proposals |
| B11 Flesh | pending | MCP, subagents, Telegram, packaging |
| B12 Hardening + Launch | pending | threat model, injection suite, docs, v0.1.0 |

## B0 — Foundation (current)

### Done-when checklist (plan Part 42.3, B0 guidance)
- [x] MIT license
- [x] README leads with L3 guardrail (proposal-only, human-approved)
- [x] .gitignore excludes secrets, caches, logs, artifacts, .overseer, private docs
- [x] uv scaffold, src layout, pyproject, .python-version
- [x] config.yaml + OVERSEER_* env overrides (dotted prefix) + pydantic validation
- [x] logging to safe dir (expanduser+resolve), redaction incl. exception text
- [x] vault template generator: idempotent, atomic writes, stable IDs, .overseer/.gitignore
- [x] doctor: config, vault, provider, permissions, .overseer subdirs, log-dir creatability
- [x] eval skeleton: 6 golden tasks (tests/golden_tasks.py)
- [x] CI green: ruff, format, mypy, pytest (50), bandit, gitleaks, pip-audit
- [x] review_packet.sh: verbatim git-show content, committed tree, full evidence
- [x] status docs: DECISIONS, RISKS, SECURITY_NOTES, TEST_REPORT, EFFICIENCY_REPORT

### Qwen review round 1 (2026-08-11) — disposition
- CRITICAL-01/02/03/07/08/11, MAJOR-13, MINOR-07: PACKET ARTIFACTS — source verified clean
  (grep: __future__ intact, no trailing spaces, option strings clean). Root cause: old packet
  generator used `cat` + markdown paste mangled underscores. FIXED: verbatim `git show` generator.
- CRITICAL-04: env prefix lost in recursion → FIXED (dotted prefix, OVERSEER_PROVIDER_MODEL works)
- CRITICAL-05/06: note overwrite + ID collision → FIXED (ID in filename, uuid4 hex[:8])
- MAJOR-08: log_dir not expanded → FIXED (expanduser+resolve)
- MAJOR-09: exception text not redacted → FIXED (redact final formatted string)
- MAJOR-10: regex order → FIXED (sk-ant- before sk-)
- MAJOR-12: config overwrite → FIXED (refuse if exists, CLI warns)
- MAJOR-14: int env crash → FIXED (ConfigError)
- MAJOR-15: .overseer/.gitignore → FIXED (created on init)
- MAJOR-16: type-specific frontmatter → FIXED (fact/correction/proposal/skill/preference/decision/project)
- MAJOR-17/18: doctor false-OKs → FIXED (log-dir creatability, .overseer subdirs)
- MAJOR-19: provider base_url validation → FIXED
- MAJOR-20: root --version → FIXED (is_eager + callback; verified via minimal repro)
- MINOR-01/02/03/04: unused import, ConfigError catch, logging wiring, frontmatter validation → FIXED
- CRITICAL-10 (CI not exercised): already done — repo pushed, CI green (runs 31522441760, 31522638036)
- MAJOR-21 (ROADMAP outdated): already done — ROADMAP in repo has B4.5/B6 rename/critical path
- MAJOR-23/24 (gitleaks/pip-audit evidence): now in packet (10-gitleaks.txt, 11-pip-audit.txt)
- NEW: PYSEC-2026-1845 (pytest 8.4.2) → FIXED (pytest 9.1.1, pip-audit clean)

### Tests (50 passing)
- config: defaults, missing file, invalid YAML, power_mode, env override (flat/nested/bool/int),
  invalid int → ConfigError, sample roundtrip, no-secrets, refuse-overwrite
- redact: openai, anthropic (order), github, aws, bearer, private key, assignment, plain text, empty
- vault: layout, idempotent, L3 guardrail, frontmatter valid, unknown type, containment,
  list by type, unicode slug, duplicate titles, ID uniqueness (50), governance, status enum,
  .overseer/.gitignore, all-notes-frontmatter
- doctor: ok after init, fails without vault, missing key, render, bad base_url, .overseer subdirs
- cli: version subcommand, --version flag, init creates vault, doctor fail/ok
- logging: redaction incl. exception text, safe dir creation

### Known problems / risks
- R-01: redaction pattern set not exhaustive (tracked; grows in B1/B3)
- R-02: no fsync on atomic writes (acceptable for B0; revisit for durability-sensitive notes)
- R-03: power_mode/live_learning config placeholders until B8/B4.5 (explicit)
- R-04: gitleaks allowlists tests/test_redact.py (fake keys only — verified no real secrets)

## B1 — Robot Body (in progress)

### Slices done (committed + pushed)
- **Slice 1** (5c56eff): providers/ — base adapter (ChatMessage/ChatResult/ToolCall),
  registry with fallback chains, OpenAI-compatible impl (httpx, tool-call wire format,
  auth via env var). 12 tests.
- **Slices 2-3** (b8ee1ab): tools/ — self-registering registry, base Tool + ToolResult
  (status/summary/artifacts/token_cost), 6 core tools (terminal, file_read, file_write,
  file_patch, list_dir, grep), approval.py policy engine (denylist > allowlist > risky,
  path policy). Fail-closed terminal. 30 tests.
- **Slice 4** (14fbf63): agent.py — AgentLoop state machine (build messages -> call model
  with fallback -> parse tool calls -> approval-gated dispatch -> append results -> repeat),
  stop conditions (final_answer/max_iterations/budget/error), APPROVAL_DENIED marker,
  e2e verified (write+read+artifact+final answer). 11 tests.

### B1 done-when status
- [x] End-to-end loop completes a real task with tools (e2e test + smoke run)
- [x] Risky terminal command requires approval (denylist/risky tests)
- [x] File write outside allowed path requires approval (path policy tests)
- [x] Tool outputs stored as artifacts and summarized (artifact tests)
- [x] Basic security checks pass (redaction, containment, fail-closed, bandit/gitleaks clean)
- [x] Streaming (Provider.stream + SSE parsing, partial tool-call deltas, malformed-safe, redacted errors)
- [x] Untrusted-content labeling (ToolResult.trust + system-prompt rule + hostile-file test)
- [x] Structured denial (ToolResult.denied, never string-matched)
- [x] Denylist bypass hardening (whitespace tricks, $HOME, force-push variants, curl|sh, bash -c, eval, base64|sh, python -c, chmod 777)
- [x] Symlink containment (resolve-based, tested)
- [x] Artifact redaction (terminal artifacts redacted, tested)
- [x] Provider error redaction (HTTP bodies redacted, tested)
- [x] Loop robustness (empty responses, budget estimator, duplicate IDs, non-dict args, transcript for resume)
- [ ] CLI wiring (B2: overseer chat/run)

### B1 round 2 (Qwen review) — disposition
- Streaming wiring: DONE (Provider.stream, StreamEvent, partial tool-call delta accumulation, malformed SSE skipped, timeout/cancellation, redacted errors, 5 streaming tests)
- Untrusted-content labeling: DONE (ToolResult.trust, UNTRUSTED_RULE in system prompt, hostile-file test)
- APPROVAL_DENIED marker spoofing: FIXED (structured ToolResult.denied, no string matching)
- Terminal denylist bypass: FIXED (whitespace normalization + 10 new bypass patterns + 8 bypass tests)
- Filesystem symlink escape: FIXED (resolve-based containment, symlink test)
- Artifact leakage: FIXED (terminal artifacts redacted, test)
- Provider error credential leak: FIXED (HTTP bodies redacted, test)
- Budget without usage: FIXED (conservative estimator, test)
- Empty responses / duplicate IDs / non-dict args: FIXED (tests)
- Fallback with tool calls: documented — provider failure before dispatch is retryable; after dispatch, tool results are already in history (no duplicate work)

### B1 bugs found & fixed (bug-hunt pass)
- file_read resolved relative paths against CWD, not allowed root -> fixed
- _resolve raised ToolError out of run() instead of structured error -> fixed
- terminal failed OPEN without approver -> fail-closed
- approval gate not enforced for file_write/file_patch in agent dispatch -> fixed
- approvals_denied never counted (message mismatch) -> APPROVAL_DENIED marker
- check_path resolved relative paths against CWD -> fixed (allowed_roots[0])
- unknown tool in _dispatch crashed loop -> error result fed back
- duplicate ProviderError class -> deduped
- gitleaks-action config_path input invalid -> removed (auto-detect)

## B2 — CLI & Session Experience (in progress)

### Slices done (committed + pushed)
- **Slice 1** (13d0c93): session.py — SessionStore (create/load/append/list/export),
  atomic append (temp carry-over), meta-only listing, redacted markdown export,
  microsecond timestamps for deterministic sort. 11 tests.
- **Slice 2** (13d0c93): agent streaming — run(stream=True) consumes Provider.stream()
  (text deltas via stream_callback, tool-call deltas accumulated), falls back to
  complete() when a provider has no streaming path, mid-stream failure falls back
  down the chain. 4 tests.
- **Slice 3** (13d0c93): cli.py rewrite — chat (interactive, streaming, resume),
  run (one-shot), model (inspect/set, secrets never shown), tools (table),
  config (view/validate), sessions (meta table), trace (redacted transcript),
  export (redacted markdown), doctor (provider-wired), init (restored),
  stubs (memory→B5, skills→B7, cron→refused). Approval UX: exact command/path +
  risk reason + approve/deny prompt, decisions logged to .overseer/logs/approvals.log.
  14 tests.
- **Slice 4** (13d0c93): providers/factory.py (build_provider, no guessed endpoints),
  providers/__init__.py imports openai_compat so registration runs (real bug found
  by smoke test), sample config uses openai-compat + example base_url. README
  quickstart + command table.

### B2 done-when status
- [x] chat: interactive session with the agent loop (streaming, resume)
- [x] run: non-interactive single task
- [x] model: inspect/switch provider config (no secrets)
- [x] tools: list registered tools and schemas
- [x] config: view/validate safely
- [x] sessions: list/resume/export
- [x] doctor: wired to provider system
- [x] version + root --version: verified
- [x] trace: inspect session transcript by ID
- [x] stubs: memory (B5), skills (B7), cron (refused, B10)
- [x] session lifecycle: unique IDs, persist, resume (no dup), export (redacted)
- [x] budget display: warning at 80%, clear BudgetExceeded errors
- [x] approval UX: exact command/path + risk reason + approve/deny + logged
- [x] streaming UX: tokens as they arrive, mid-stream cancellation (Ctrl+C),
      mid-stream provider failure (fallback chain)
- [x] safety: redact all display/export, config never shows values
- [x] efficiency: lazy imports, version/doctor never import agent/providers,
      sessions list reads meta only

### B2 bugs found & fixed (bug-hunt pass)
- session append wiped prior events (temp file didn't carry transcript) -> fixed
- session list sort unstable (second-precision timestamps) -> microseconds
- provider registry empty (openai_compat never imported) -> __init__ imports it
- sample config named unregistered provider (ollama-cloud) -> openai-compat
- streaming double-emitted content (callback fired for full text after deltas) -> fixed
- test streaming provider was stateless (replayed events) -> stateful batches
- gitleaks flagged new test files (fake keys) -> allowlist extended

## B3 — Episodic Memory & Observation Stream (in progress)

### Slices done (committed + pushed)
- **Slice 1** (349a9a6): episodic.py — Event schema (type/session_id/trace_id/ts/
  tool_name/content), EpisodicStore (SQLite WAL + busy_timeout + FTS5, append/
  append_many/search/rebuild/count/close), redaction before write, thread lock
  (sqlite3 connections are not thread-safe). 12 tests.
- **Slice 2** (349a9a6): session.py — SessionStore owns an EpisodicStore; append
  mirrors messages into the stream; observe_tool_call/observe_approval/
  observe_error; transcript switched to true append mode (NOTE-01); provider
  field on Session/Meta for cost (NOTE-02).
- **Slice 3** (349a9a6): agent.py — observer hook (event_type, payload) fired for
  tool calls (FINAL accumulated args, NOTE-03), approvals (allowed bool), and
  errors. Loop stays store-agnostic; CLI wires it.
- **Slice 4** (349a9a6): cli.py — sessions search <query> (FTS5 table),
  sessions rebuild (derived-cache rule), _write_session_note vault bridge
  (10-Sessions/, frontmatter validated, status mapped to vault vocabulary),
  _session_cost provider-aware (NOTE-02). 6 tests.

### B3 done-when status
- [x] Observation stream: user/assistant/tool-call/tool-result/approval/error events
- [x] Append-only events with timestamps, session IDs, trace IDs
- [x] Redaction before disk (events, SQLite, vault notes)
- [x] Episodic store: SQLite WAL + FTS5, .overseer/episodic.sqlite (derived cache)
- [x] Rebuild from raw transcripts (deleting the DB loses nothing)
- [x] Session note generation: 10-Sessions/, valid frontmatter, summary not dump
- [x] Search: overseer search <query> (FTS5, session IDs + snippets)
- [x] Efficiency: meta.json listing preserved, batch inserts, no full-DB loads
- [x] Concurrency: WAL + busy_timeout + thread lock, concurrent append test
- [x] NOTE-01: transcript true append mode (O(1) per append)
- [x] NOTE-02: provider-aware cost via _cost_for
- [x] NOTE-03: final accumulated tool calls only (no delta flooding)

### B3 bugs found & fixed (bug-hunt pass)
- sqlite3 connection shared across threads -> SystemError -> RLock guard
- EV_SYSTEM missing from session.py import -> added
- vault session status vocabulary (active/accepted/rejected/deprecated) vs
  session status (done/error) -> mapped in _write_session_note
- Runtime dataclass missing _current_session -> added (mypy)
- test fake Cfg missing vault_path -> added
- S112 try-except-continue in rebuild -> log + continue

## B4 — Verification & Repo Intelligence (in progress)

### Slices done (committed + pushed)
- **Slice 1** (fdbe851): project.py — ProjectContext (name/language/framework/
  package_manager/test_runner/linter/typechecker/build_system + commands),
  detect_project reads pyproject.toml/package.json/Cargo.toml/go.mod/Makefile/
  requirements.txt, RepoMap (lightweight tree, skips noise dirs, cached by
  root signature). 10 tests.
- **Slice 2** (fdbe851): verification.py — VerificationRunner (run_tests
  targeted, run_linter, run_typechecker, verify merges cards), parse_failures
  (pytest/lint/typecheck signatures -> FailureCard file/line/error_type/
  message), checkpoint/rollback (JSON payload with backup+original paths),
  cache by command+file hash, timeout. 11 tests.
- **Slice 3** (fdbe851): tools/repo.py — repo_map, git_status, git_diff,
  git_log (read-only, timeout, redacted). 6 tests.
- **Slice 4** (fdbe851): filesystem.py checkpoints before file_write/
  file_patch (ToolResult.checkpoint); agent.py verifier hook — after a
  checkpointed write, run verification; on failure roll back + feed the
  failure card to the model. 2 tests.

### B4 done-when status
- [x] Project detection (language, framework, package manager, test runner,
      linter, typechecker, build system)
- [x] Repo map generated and cached (lightweight, no full reads)
- [x] Targeted test selection (pytest file paths)
- [x] Failure card generation (file/line/error_type/message)
- [x] Patch validation + rollback checkpoints (never leave repo broken)
- [x] Git integration (git_status, git_diff, git_log; destructive gated)
- [x] Verification-driven iteration (fail -> rollback -> card to model)
- [x] Safety: destructive git approval-gated, output redacted + truncated
- [x] Efficiency: repo map cached, verification cached by hash, targeted tests

### B4 bugs found & fixed (bug-hunt pass)
- os.walk yields str not Path -> wrapped
- bare requirements.txt not detected as Python -> added
- ProjectContext missing name field -> added
- proc.exitcode doesn't exist -> proc.returncode
- checkpoint path didn't carry original location -> JSON payload
- bandit B603/B607 on git tools -> pyproject skips + -c pyproject.toml
- ruff S603/S607 on tests -> tests/* glob extended

## B4.5 — Live Learning Engine (in progress)

### Slices done (committed + pushed)
- **Slice 1** (c153e6c): live_learning.py — LiveEvent schema (9 types, 5 scopes),
  detect_signals (heuristic, no model call), SessionMemory (constraints/
  preferences/rules, undo stack), ProvisionalStore (vault inbox candidates),
  LiveLearningEngine (detect_and_apply, untrusted blocking, token budgets).
  12 tests.
- **Slice 2** (c153e6c): agent.py live_learning hook (fires on user message
  before each run); cli.py wires the engine into the runtime, injects the
  context block into each turn, adds live-learn inspect/undo. 3 tests.

### B4.5 done-when status
- [x] Event schema: correction/preference/fact/constraint/tool_outcome/
      risk_signal/uncertainty_signal/repeated_pattern/explicit_memory
- [x] Scopes: turn/session/provisional/project/global
- [x] Per-turn micro-reflection (Speed 0/1): heuristic detector, no latency
- [x] Corrections/preferences apply to session immediately
- [x] Active constraints injected into next context build
- [x] Provisional candidates (Speed 2): implicit -> low confidence, inbox
- [x] Explicit "remember this" (Speed 3): durable candidate immediately
- [x] Untrusted content blocked from durable memory
- [x] Live learning toggle in config (live_learning: bool)
- [x] overseer live-learn inspect/undo
- [x] Reversible: undo stack
- [x] Latency budget: max_events_per_turn/session
- [x] Cannot override guardrails or bypass approvals (session-scoped only)

### B4.5 bugs found & fixed (bug-hunt pass)
- B007 unused loop var in test -> _

## B5 — Knowledge Layer (in progress)

### Slices done (committed + pushed)
- **Slice 1** (499ec1b): knowledge.py — MemoryCandidate (note_type/content/
  confidence/evidence/scope/trigger/salience), extract_candidates (heuristic
  extraction from episodic + live events), confidence tiers (explicit 0.9 >
  repeated 0.75 > implicit 0.4 > untrusted 0.1), salience (importance x
  recency x access). 12 tests.
- **Slice 2** (499ec1b): KnowledgeBase.consolidate — vault writes via governed
  write_note (type-specific frontmatter), dedup (facts by scope, others by
  trigger/content), conflict flagging to 99-Meta/ (never silent overwrite),
  evidence linking in every note. 12 tests.
- **Slice 3** (499ec1b): retrieve(query, note_types) — top-N salient notes;
  episodic.by_session (exact session match, not FTS); cli consolidate
  command. 2 tests.

### B5 done-when status
- [x] Reflection pipeline: consume episodic + live candidates
- [x] Confidence scoring: explicit > repeated verified > implicit
- [x] Salience scoring: importance x recency x access
- [x] Vault canonical storage: facts→30-Facts, preferences→50-Preferences,
      corrections→80-Corrections, skills→40-Skills, projects→60-Projects
- [x] Strict frontmatter governance (type-specific required fields)
- [x] Evidence linking (session id / artifact / user quote in every note)
- [x] Deduplication (update existing, never duplicate)
- [x] Conflict detection (99-Meta/ flag for human review, no silent overwrite)
- [x] Retrieval API (retrieve(query, note_types) -> top-N salient)
- [x] Untrusted content never high-confidence
- [x] overseer consolidate <id> command

### B5 bugs found & fixed (bug-hunt pass)
- EV_USER events got CONF_IMPLICIT (0.4) and were filtered -> CONF_EXPLICIT
- _parse_note split on the body's --- (evidence block) -> first --- pair
- fact dedup by content prefix missed same-scope facts -> scope match
- _is_conflict compared against full body incl. evidence -> first line only
- _write_note returned truncated id -> full OVR-<TYPE>-<hex>
- consolidate used FTS search instead of exact session match -> by_session()
- UnboundLocalError after ev->evt rename -> all references fixed

## B6 — Context Compiler (approved)

### Slices done (committed + pushed)
- **Slice 1** (34f0353): context_compiler.py — ContextItem (tier/content/value/
  tokens/snippet), ContextCompiler (budget + reserve, tiered assembly,
  eviction order Tier 4->3->2, never evicts Tier 0/1, progressive disclosure
  for long knowledge/environment items, stable prefix first for caching,
  telemetry). 7 tests.
- **Slice 2** (34f0353): agent.py compiler hook (_compile_context before each
  model call, backward compatible when None); cli.py _make_compiler bridges
  history -> tiered ContextItems -> budgeted ChatMessages. 2 tests.

### B6 done-when status
- [x] Context budget engine (max_tokens_per_turn, reserve for response)
- [x] Tiered assembly: pinned/adaptation/knowledge/environment/optional
- [x] Token accounting (len//4 conservative baseline)
- [x] Eviction order: Tier 4 -> 3 -> 2; Tier 0/1 never evicted
- [x] Progressive disclosure (snippets + link, not full notes)
- [x] Stable prefix caching (system + pinned at front, dynamic at end)
- [x] Loop integration (compiler before every model call)
- [x] Context telemetry (tokens used, budget, utilization)

### B6 bugs found & fixed (bug-hunt pass)
- progressive disclosure not applied during compile -> snippet for tier>=2
- test budgets too generous (items fit) -> tightened
- telemetry utilization rounding -> smaller budget in test
- mypy no-any-return on compiler hook -> typed annotation

## B7 — Recursive Learning: Pattern Miner & Skill Auto-Promotion (in progress)

### Slices done
- **Slice 1** (curator.py): SkillRegistry — governed 40-Skills writes via
  Vault.write_note, risk classification (high/low by tool), promotion gates
  (low-risk auto-promotes after 2 successes; high-risk always requires human
  approval and a 90% success threshold), in-place counter updates preserving
  note ID.
- **Slice 2** (miner.py): PatternMiner — episode chunking (per-session), feature
  extraction (tools, task types, error signal, corrections), deterministic
  clustering by tool+task key, evidence gates (>=3 independent verified
  successes; >=70% low-risk / >=90% high-risk), skill drafting, Correction
  Memory replay (rejects/conflict-flags drafts contradicting corrections).
- **Slice 3** (cli.py): `overseer mine` (power-aware, defers in eco), `overseer
  skills list`, `overseer skills promote <id>` (human approval gate).
- **Slice 4** (test_miner.py): 17 tests — evidence floor, success threshold,
  high-risk manual promotion, correction replay blocking, frontmatter
  validation, roundtrip persistence, CLI integration.

### B7 done-when status
- [x] PatternMiner consumes episodic events, chunks episodes, extracts features
- [x] Clusters similar episodes (tool+task key)
- [x] Evidence gates: >=3 independent verified successes; 70% low / 90% high
- [x] Never mines from unverified outcomes (session status must be done + no errors)
- [x] Skill drafts compiled to canonical 40-Skills notes with strict frontmatter
- [x] Curator flow: draft/proposed/active lifecycle
- [x] Low-risk auto-promotes after 2 manual adoptions
- [x] High-risk ALWAYS requires human approval
- [x] Correction Memory replayed during mining (conflict -> reject)
- [x] Mining is manual (`overseer mine`) + power-aware (defers in eco)
- [x] No skill bypasses the approval gate

### B7 bugs found & fixed (bug-hunt pass)
- `SkillRegistry.update()` minted a new note ID on every bump -> rewrote the
  existing file in place, preserving the OVR-SKILL id and counters.
- `mine` required a provider (failed without one) -> now builds the session
  store directly from config, no provider needed.
- Correction replay marker regex missed "do not use X" -> added do not /
  should not / is wrong patterns so the guard actually fires.
- Correction replay was wired to the transient in-memory live-learning engine
  -> now reads durable Correction Memory from the vault (B4.5/B5).

## Next
- B11 remaining: none blocking. Ready for Qwen review.
- Then B12 Hardening + public launch polish (multi-gateway, Homebrew tap, release).
