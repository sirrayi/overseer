# Overseer — agent notes

Platform core for an agentic coding engine, built per `agent-harness-playbook.pdf`
(extracted text: `playbook.txt`). Read that file before changing architecture.

## Layout

- `crates/overseer-core` — engine: canonical turn IR (`ir.rs`), append-only
  JSONL event log (`event.rs`), usage ledger (`ledger.rs`), model profiles
  (`profile.rs`), tool registry + tools (`tools/`), provider trait +
  Anthropic + OpenAI-compatible adapters (`provider/`), ReAct loop with
  budgets (`agent.rs`), stuck detector (`stuck.rs`), L4 permission gate
  (`perm.rs`), deterministic compaction (`compact.rs`). Bash calls run
  under a platform sandbox by default (macOS `sandbox-exec`, Linux
  `bwrap`): deny-by-default network, writes confined to the workspace +
  temp dirs, common secret dirs (`~/.ssh` etc.) read-denied. Falls back
  to unsandboxed exec with a visible warning when no backend exists;
  `--no-sandbox` disables. A loopback egress proxy with domain
  allowlists is still open — v1 denies all egress.
  `session.rs` enumerates sessions (SessionStart id/cwd/model, recency,
  previews, checkpoint lists) and implements `fork` (copied log cut at a
  boundary + surviving checkpoints + fresh SessionStart); `rewind.rs` is
  the shared restore implementation used by `overseer rewind` and the
  TUI's `/rewind`.
- `crates/overseer-cli` — `overseer exec` headless/CI surface; bare
  `overseer` / `overseer tui` launches the interactive TUI (same flags).
  `--continue`/`-c` resumes the newest session recorded for the cwd,
  `--last` the newest anywhere; both fall back to a fresh session.
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
  ? help, /help /quit /sessions /fork /rewind /diff /approve /search.
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
