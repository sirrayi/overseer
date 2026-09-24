# Overseer — agent notes

Platform core for an agentic coding engine, built per `agent-harness-playbook.pdf`
(extracted text: `playbook.txt`). Read that file before changing architecture.

## Layout

- `crates/overseer-core` — engine: canonical turn IR (`ir.rs`), append-only
  JSONL event log (`event.rs`), usage ledger (`ledger.rs`), model profiles
  (`profile.rs`), tool registry + tools (`tools/`), provider trait +
  Anthropic + OpenAI-compatible + Gemini adapters (`provider/`; Gemini
  pairs tool calls by name, thought/thoughtSignature parts ride the
  opaque Reasoning block; `--provider
  opencode` name hosted OpenAI-compat endpoints — env keys OPENCODE_API_KEY, opencode sends the required `x-opencode-session`
  header; opencode muse-spark-* is Responses-API-only and routed to the
  `provider/responses.rs` adapter (encrypted reasoning round-trips as
  opaque Reasoning blocks, `store:false` — the event log replays full
  history), ReAct loop with
  budgets (`agent.rs`), stuck
  detector (`stuck.rs`), L4 permission gate (`perm.rs`), deterministic
  compaction (`compact.rs`). `Effort` (min..max) maps per provider;
  `small_model` covers aux calls (consolidation) with escalate-to-main;
  each stuck trip bumps effort one notch. `memory.rs` is the
  git-versioned INDEX.md store; `consolidate` dedupes it via a
  small-tier call. `skills.rs` keeps SKILL.md metadata resident and
  loads bodies on demand through the `skill` tool,
  provenance-wrapped. `repomap.rs` builds a bounded (~1K token)
  ranked symbol index behind `repo_map`/`symbol` tools. `task` has
  read/write/background modes — writers isolate into git worktrees,
  background results land via SubagentDone notices (max 4 in flight).
  Rule-of-Two (perm.rs): untrusted-content + sensitive-data latches
  arm the exfil gate (side effects force Ask); tool results enter the
  model view provenance-wrapped. Bash calls run
  under a platform sandbox by default (macOS `sandbox-exec`, Linux
  `bwrap`): deny-by-default network, writes confined to the workspace +
  temp dirs, common secret dirs (`~/.ssh` etc.) read-denied. Falls back
  to unsandboxed exec with a visible warning when no backend exists;
  `--no-sandbox` disables. A loopback egress proxy with domain
  allowlists is still open — v1 denies all egress.
  `harden.rs` is the startup posture: the CLI calls `harden_startup()`
  first (umask 0o077 + proxy-env scrub) and session/daemon roots are
  pinned owner-only via `ensure_private_dir`. The injection-ASR corpus
  (`tests/injection_asr.rs`) gates the Rule-of-Two latches in CI, and
  `cargo deny check` (deny.toml, `.cargo/audit.toml` locally) runs in the
  supply-chain job.
  `session.rs` enumerates sessions (SessionStart id/cwd/model, recency,
  previews, checkpoint lists) and implements `fork` (copied log cut at a
  boundary + surviving checkpoints + fresh SessionStart); `rewind.rs` is
  the shared restore implementation used by `overseer rewind` and the
  TUI's `/rewind`.
- arsenal modules (P8-B/P8-C ports — pure patterns, no servers/runtimes):
  memory/graph (`memory.rs`, `graph.rs`, `cognee.rs`, `press.rs`,
  `rag.rs`), retrieval (`tools/struct_search.rs` — ast-grep + semgrep
  shell-out; `tsitter.rs`, feature `tree-sitter`, default-off, zero
  dependencies: the grammar/query registry a real binding registers
  against), search/scrape (`web.rs`, `extract.rs`, `crawl.rs`),
  browser/agent surface (`browse.rs`, `webagent.rs`), sandboxes and
  grants (`backends.rs` — e2b trait, provider enum, runtime selection,
  toolhive grants, context-forge grants), `mcp.rs` (minimal stdio
  JSON-RPC client), `refine.rs` (textgrad bounded loop),
  `evalcfg.rs`/`observability.rs` (B2). `bash` honours `--runtime
  <native|seatbelt|bubblewrap|gvisor>`: a pinned runtime that is
  unavailable FAILS the call with the requirement named (never a silent
  downgrade to unsandboxed exec); unset keeps the platform default.
  Deferred sites are marked `// DEFERRED(owner)` in each module header.
- `crates/overseer-cli` — `overseer exec` headless/CI surface; bare
  `overseer` / `overseer tui` launches the interactive TUI (same flags).
  `--continue`/`-c` resumes the newest session recorded for the cwd,
  `--last` the newest anywhere; both fall back to a fresh session.
  `--bare` is hermetic CI mode: implies `--json`, throwaway session in
  the temp dir (never `~/.overseer`), no persisted rules, and rejects
  combination with `--resume`/`--continue`/`--last`/`--session`.
  `--runtime <name>` pins the bash sandbox backend (validated at parse
  time; an unavailable runtime fails the call rather than downgrading).
- `crates/overseer-proto` — wire protocol types (request/notification)
- `crates/overseer-tui` — ratatui/crossterm TUI (library). Agent runs on a
  worker thread; UI renders `Event`s over a channel. Fixed-height
  `Viewport::Inline` region (ratatui inline height is init-only) +
  `insert_before` for scrollback handoff; DECRQM/XTVERSION probe; BSU/ESU
  frame batching when sync output probes positive. `Control` =
  interrupt + steer queue (checked at tool-launch boundaries only —
  skipped calls get synthetic results so tool pairing survives);
  `Policy::gate` routes Ask verdicts to a human dialog (200 ms
  anti-misclick, arrows/Enter, never steals text keys) with typed
  previews (bash cmd / edit diff / write head); "always" persists a
  rule to `~/.overseer/rules` (deny rules still win; headless stays
  fail-closed). Keys: Esc interrupt, Shift+Tab mode cycle, Ctrl+T
  plan, Ctrl+X cancel queued, Ctrl+S stash, Ctrl+_ undo, Ctrl+W
  del-word, Ctrl+P sessions, Ctrl+O search, Ctrl+Y copy last reply
  (OSC 52), Alt+E/`/edit` $EDITOR draft, Tab completes `/cmd` or
  `@path`, `!cmd` runs shell locally (never sent to the model),
  ? help, /help /quit /sessions /tree /fork /rewind /diff /approve
  /search. `/tree` renders the fork forest (SessionStart.parent edges)
  DFS parents-first; the picker shows ⤶ forks and git-branch metadata
  (computed once at open, wide mode). `/diff` rows carry +/− counts and
  structured hunks — Tab previews, ←→ selects a hunk, space marks it
  rejected, Enter applies marked rejects (partial revert) or the whole
  snapshot; transcript overlay Tab expands collapsed tool blocks
  (16-line cap). `/` menu + completion use subsequence fuzzy match.
  Session switches and rewinds rebuild the agent via
  `WorkerCmd::SwitchSession` → `Agent::resume` (never mid-run); the
  transcript reseeds from the new log. Modal overlays
  (sessions/rewind/transcript/diff) own the live region while open —
  key hints render last so top-clipping can't hide them. `/diff`
  reads checkpoint manifests (earliest snapshot per path), Enter
  reverts a file after stashing current content into
  `checkpoints/revert-stash`. `/approve` exits plan mode and submits
  "implement the plan". Toasts self-expire in the status area.
  Polish layer (2.8): `theme.rs` is a runtime palette
  (`OVERSEER_THEME=mono|default|high-contrast`; NO_COLOR/TERM=dumb →
  mono); DECSET-1004 focus tracking gates BEL+OSC 9/99/777
  notifications (unfocused only, escape-sanitized); OSC 133 prompt
  marks + OSC 8 file links emit raw into the scrollback stream (muxes
  off via `caps.osc`); REDUCE_MOTION freezes the spinner;
  `--no-tui` is a plain-text REPL sharing the same worker (`run_line`,
  asks answered by one-line replies, /quit or Ctrl+D exits).
- `eval/` — Inspect AI evaluation rig (scaffold)

## Invariants (do not violate)

1. Events are immutable; session state is a view over `events.jsonl`.
2. Stable prompt prefixes — nothing volatile (timestamps, session ids, git
   status) above the cache boundary.
3. Engine enforces budgets (steps, cost), never the model.
4. Tool results are budgeted: ~30K chars inline, then spill to file.
5. Read-before-edit is enforced by the harness, not the prompt.
6. Reasoning blocks are opaque — round-trip verbatim, never inspect/mutate.
7. Raw provider `stop_reason` is preserved end-to-end.
8. Compaction is a view over the event log, never a mutation; summaries are
   derived mechanically from raw events (never re-summarized), and the
   recency tail always starts on a ModelResponse boundary so tool pairing
   survives the cut.
9. Checkpoints live in `<session>/checkpoints/e<user-input-event-id>/` —
   one per user prompt. `write`/`edit` snapshot each file BEFORE its
   first touch into `files/` + `manifest.jsonl` (`existed:false` → rewind
   deletes it). `overseer rewind <session-dir> [--checkpoint n]
   [--mode code|conversation|both|summarize]` restores files and/or
   truncates the log at the boundary. Blind spot: `bash` side effects
   are NOT snapshotted — only write/edit paths are recorded.

## Commands

- Build: `cargo build` (or `cargo build -p overseer-cli`)
- Test: `cargo test` (TUI snapshots: `cargo test -p overseer-tui`;
  regenerate with `INSTA_UPDATE=always`)
- Run: `ANTHROPIC_API_KEY=... cargo run -p overseer-cli -- exec "task"`
- TUI: `cargo run -p overseer-cli` (bare) or `-- tui [exec flags]`
- JSONL event stream: add `--json`; resume: `--resume <session-dir>`

## Git workflow (github.com/sirrayi/overseer, private)

Branch ladder — promotion flows upward, work flows downward:

```
main   ← tagged releases only; never push directly
dev    ← stress-testing / dev-release builds cut from here
review ← default branch; all PRs target here first
*      ← feature branches fork off review
```

- Branch off `review` with typed names: `feat/<slug>`, `fix/<slug>`,
  `chore/<slug>`, `docs/<slug>`, `eval/<slug>`
- Open PR → `review`. Fix conflicts + final polish there.
- `review` → `dev` merge gates a dev release (stress testing).
- `dev` → `main` only when a release is finalized.
- Direct pushes to `main` are forbidden by convention (no Pro-tier
  protection available on a private repo — enforced socially).
- CI runs on PRs and on pushes to `main`; keep main pushes rare to
  conserve Actions minutes.

## Standing rule: deferred-item comments (confirmed 2026-09-18)

Everything we do ships with comments on anything left out for later:
- Every PR body ends with a "Deferred" section naming each leftover with status + owner.
- Every merge commit message carries the same deferred list.
- Every code site that defers work carries an adjacent comment (`// DEFERRED(<owner>): <what> — <gate>`).
- Format follows the 42-PR follow-up sweep of 2026-09-18 (FIXED SINCE / STILL OPEN / INTENTIONAL / SUPERSEDED).
