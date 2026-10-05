# Overseer — agent notes

Platform core for an agentic coding engine, built per `docs/agent-harness-playbook.pdf`.
Read it, and the pillar refresh `docs/research/2026-09-29-pillars.md`, before changing
architecture.

## Layout

- `crates/overseer-core` — engine: canonical turn IR (`ir.rs`), append-only
  JSONL event log (`event.rs`), usage ledger (`ledger.rs`), model profiles
  (`profile.rs`), tool registry + tools (`tools/`), provider trait +
  Anthropic + OpenAI-compatible + Gemini adapters (`provider/`; Gemini
  pairs tool calls by name, thought/thoughtSignature parts ride the
  opaque Reasoning block; `--provider opencode` names a hosted
  OpenAI-compat endpoint — env key OPENCODE_API_KEY, and every call sends
  the required `x-opencode-session`
  header; opencode muse-spark-* is Responses-API-only and routed to the
  `provider/responses.rs` adapter (encrypted reasoning round-trips as
  opaque Reasoning blocks, `store:false` — the event log replays full
  history), ReAct loop with
  budgets (`agent.rs`), stuck
  detector (`stuck.rs`), L4 permission gate (`perm.rs`), deterministic
  compaction (`compact.rs`). `Effort` (min..max) maps per provider;
  `small_model` covers aux calls (consolidation) with escalate-to-main;
  each stuck trip bumps effort one notch. `memory/` is memory v2: two
  git-versioned stores, user (`$OVERSEER_HOME/memory`, default
  `~/.overseer`) and project (`…/projects/<slug>-<hash8>/memory`, keyed by
  the git toplevel found without spawning; `--memory` keeps v1's
  `<cwd>/memory` instead, `--no-memory`/`--bare` turn memory off). Notes
  live in five layers (profile/episodic/semantic/procedural/prospective)
  with an ADD-only INDEX.md each. The resident `memory` op tool
  (`tools/memory_tool.rs`) searches (in-memory BM25F fused by RRF with
  Petrov activation from `.index/uses.jsonl` and confidence), gets,
  remembers (≤5 writes per user turn; per-layer write bars in `perm.rs`;
  under the untrusted latch writes go to `proposals/`, never indexed) and
  forgets (expires, never deletes). On user input the engine may inject a
  `MemoryNotice` (recall of ≤3 notes, or a prospective `at:`/`kw:`
  reminder; `path:` reminders queue to the loop boundary), and each run
  end rewrites an `episodic/session-…` note derived from the log.
  Subagents get filtered read-only copies and no recall/reminders/
  episodes. Under `--full-access`, `Policy::allow_all` skips the memory
  gate arm and its write bars; the tool itself still refuses subagent
  writes.
  `overseer consolidate` dedupes each INDEX via a small-tier call
  (ADD-only for live pointers — any the model omits are restored),
  compacts the use journal and distils new episodes into ≤5 validated
  semantic/procedural notes.
  Prompt caching: Anthropic gets explicit breakpoints on the tools tail,
  the last cacheable system segment, and a rolling one on the last
  eligible block of the last message; OpenAI profiles send
  `prompt_cache_key` (the session id); DeepSeek-style
  `prompt_cache_hit/miss_tokens` and Responses `cache_write_tokens` are
  parsed. The static system prefix (memory INDEX/CORE, skills, persona,
  MCP line) is assembled ONCE per Agent — mid-session edits apply from
  the next session/resume, so a memory write never rewrites the cache.
  `skills.rs` keeps SKILL.md metadata resident and
  loads bodies on demand through the `skill` tool,
  provenance-wrapped. `repomap.rs` builds a bounded (~1K token)
  ranked symbol index (one header per file) behind `repo_map`/`symbol`
  tools. `task` has
  read/write/verify/consult modes on light/standard/heavy tiers —
  writers isolate into git worktrees, verify returns a parsed verdict,
  consult is one no-tools call; each run's cap is carved from the
  parent's remaining budget and its spend settles into the parent
  ledger; background results land via SubagentDone notices (max
  `max_bg_subagents`, default 4, in flight). Task ids are
  session-monotonic `task-N` dirs under `subagents/` with a `task.json`
  sidecar (mode, tier, worktree, cap, spend) that resume reuses. Readers
  get `readonly` (`read`, `grep`, `glob`, `memory`), verifiers
  `readonly_with_bash`, writers `core_in` at their worktree; every mode
  with tools, resumes included, sees memory v2's filtered user/project
  copies under its task dir with writes denied (`memory_readonly`).
  Rule-of-Two (perm.rs): untrusted-content + sensitive-data latches
  arm the exfil gate (side effects force Ask); tool results enter the
  model view provenance-wrapped. Bash calls run
  under a platform sandbox by default (macOS `sandbox-exec`, Linux
  `bwrap`): deny-by-default network, writes confined to the workspace +
  temp dirs, common secret dirs (`~/.ssh` etc.) read-denied. Falls back
  to unsandboxed exec with a visible warning when no backend exists;
  `--no-sandbox` disables. A loopback egress proxy with domain
  allowlists is still open — v1 denies all egress. `diagnostics`
  checkers and `verify_cmd` run inside the same sandbox with the bash env
  allowlist (no `*_API_KEY` reaches `build.rs`). File tools: `write`
  refuses to overwrite a file not read this session; write/edit resolve
  a contained canonical target (parent canonicalized + checked) and open
  it with `O_NOFOLLOW`; semgrep runs offline (`--metrics=off`, local
  configs inside cwd only). Resident specs: `bash`, `read`, `write`,
  `edit`, `grep`, `glob`, `task`, `tools`, `run_code` (code-mode builds),
  `memory` (always native, never reached through `tools` or `run_code`),
  and `skill` only when a skill exists (one file-existence detector
  shared with the prompt's skills segment). Deferred tools (`computer`,
  `struct_search`, `diagnostics` when usable; `repo_map`, `symbol`,
  `plan`; every MCP tool) stay in `base_specs` — modes, `--no-tools` and
  `check_args` see them — but not in the advertised `specs`; they are
  found and called through `tools` (`tools/tools_tool.rs`: `op=search`
  → schemas, `op=call` re-enters `ToolRegistry::call` under the inner
  name, so the whole pipeline and the gate key on the inner tool;
  `tools::effective_call` unwraps it wherever a name drives behaviour,
  while events keep the outer call). `run_code` (`tools/run_code.rs`,
  core feature `code-mode`, on by default) runs a JavaScript function
  body in embedded QuickJS with no `std`/`os` modules or module loader;
  every effect is a `tools.*` sub-call re-entering `call` with the script
  flag set (read dedup off, 1 MB inline cap, no spill; everything else
  identical), bounded by heap/stack/deadline/sub-call/byte/print caps,
  and audited as `ScriptCall` events that never enter the model view.
  All of this is computed once per registry, so the spec array stays
  byte-stable and name-sorted; a startup-token test pins the resident
  spec + static prompt sizes.
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
  TUI's `/rewind` (manifest paths must resolve inside the workspace or
  the restore is refused). `EventLog::replay` tolerates only a torn
  final line; a corrupt earlier line is an error naming the line, and
  session listing skips such a log with a warning. New fields on an
  existing `EventKind` variant must be `#[serde(default)]` so old logs
  replay.
- arsenal modules (ports kept only where wired — the rest was removed and
  lives in git history): `memory/`; retrieval
  (`tools/struct_search.rs` — ast-grep + semgrep shell-out); runtime
  selection (`backends.rs` — `SandboxRuntime::parse` + `check_runtime`);
  `cred.rs` (credential broker: sentinels are HMAC-SHA256 under a
  per-process key; consent grants are recorded for audit — manifest +
  `ConsentGranted` — but rate/window enforcement is not wired);
  `mcp.rs` (minimal stdio JSON-RPC client) plus `mcp_config.rs` (the
  `~/.overseer/mcp.json` server list: `${VAR}` env expansion, per-server
  `trust: read|ask`). The client is wired in as the internal, never
  advertised op tool `mcp` (`tools/mcp_tool.rs`); the model reaches MCP
  tools through `tools` by their `mcp__<server>__<tool>` names, which
  re-enter `mcp op=call` so its trust/taint lanes apply unchanged (old
  logs' direct `mcp` calls still dispatch). Servers spawn
  lazily on first use and are dropped when a call fails (next use
  respawns); a name that would shadow a resident tool is skipped, never
  callable; children get an env allowlist (PATH/HOME + the pairs the
  config declares — never the parent's keys); a `trust: read` server's
  calls skip the approval ladder, everything else rides the
  external-comms lane. Discovered definitions never enter
  `ToolRegistry.specs`, so the advertised array is byte-stable. `bash` honours
  `--runtime <native|seatbelt|bubblewrap|gvisor>`: a pinned runtime that is
  unavailable FAILS the call with the requirement named (never a silent
  downgrade to unsandboxed exec); unset keeps the platform default.
  Deferred sites are marked `// DEFERRED(owner)` in each module header.

Computer use: `tools/computer.rs` + `tools/computer/{cua,shape}.rs` — a cua-driver MCP backend (`OVERSEER_COMPUTER_DRIVER` or `cua-driver` on PATH; `cua-driver mcp` over stdio so macOS TCC attributes to CuaDriver.app; session label `ovs-<8>`; lazy spawn, drop-and-respawn on transport failure; env PATH+HOME only) serves every action: apps/windows/launch/observe (AX tree, no screenshot)/screenshot/zoom/click/type/key/set/scroll/drag/menu/verify and browser/browser_click/browser_type/navigate (Chromium via CDP refs; other browsers via the window's AX tree). Observations always latch the untrusted taint; acts are InternalWrite; `navigate` is ExternalComms. The `OVERSEER_COMPUTER_{STRUCTURED,A11Y,PIXEL}` helper protocol is a fallback only (DEFERRED for removal).
- `crates/overseer-cli` — `overseer exec` headless/CI surface; bare
  `overseer` / `overseer tui` launches the interactive TUI (same flags).
  `--continue`/`-c` resumes the newest session recorded for the cwd,
  `--last` the newest anywhere; both fall back to a fresh session.
  `--bare` is hermetic CI mode: implies `--json`, throwaway session in
  the temp dir (never `~/.overseer`), no persisted rules, and rejects
  combination with `--resume`/`--continue`/`--last`/`--session`.
  `--runtime <name>` pins the bash sandbox backend (validated at parse
  time; an unavailable runtime fails the call rather than downgrading).
  `main.rs` is dispatch + usage; subcommands live in `src/cmd/<name>.rs`
  with one flag parser in `src/args.rs` (`--flag value` and
  `--flag=value`; unknown flags and unknown `--provider` values are
  errors, exit 2). `overseer web [tui flags] [--port <n>] [--no-open]` is
  the browser surface (same as `overseer tui --web`): scans 8641–8660
  unless a port is pinned and opens a tab unless `--no-open` / SSH /
  headless. Provider keys use provider-specific names only
  (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GOOGLE_API_KEY` or
  `GEMINI_API_KEY` / `OPENCODE_API_KEY`): env first, then the same name
  in the credential payload. There is no project-level key (owner
  decision 2026-09-25); core's child-env strip list keeps
  `OVERSEER_API_KEY` only as a defensive strip.
- `crates/overseer-gateway` — always-on daemon (`daemon.rs`, single-
  threaded tick loop; clears its own `STOP` killswitch at start;
  `apply_config` reload keeps trigger state by id). `ctl.rs` Unix control
  socket (request lines capped at 64 KiB). Channels:
  `channels/telegram.rs` (Bot API, 15 s timeout on every call) and
  `channels/webhook.rs` (rate-limited). `inbox.rs`/`outbox.rs` durable
  item stores (inbox journals before writing). Triage + `gate.rs`
  attention gating (`OVERSEER_TZ_OFFSET_MIN`). Untrusted spawn floor
  (`spawn.rs`): channel input never reaches the act tier directly.
  Unix-only.
- `crates/overseer-proto` — wire protocol types (request/notification)
- `crates/overseer-tui` — ratatui/crossterm TUI (library). Agent runs on a
  worker thread; UI renders `Event`s over a channel. Two surfaces share
  one `App` state machine (`app.rs` + `app/{input,overlay,panel,render,
  shell}.rs`, `UiMode`): `run` (the default)
  owns the whole window on the alternate screen — transcript
  region over a 2-row prompt over a 1-row footer, `tbuf` line buffer
  with PageUp/PageDn/wheel scroll and a `↑N` footer marker, live stack
  (dialog/overlay/queue/indicator) pinned to the region bottom, and a
  plain-text transcript handoff into scrollback on exit. `run_inline`
  (`--inline`) keeps the old contract: fixed-height `Viewport::Inline`
  region (ratatui inline height is init-only) + `insert_before` for
  scrollback handoff. `run_web_with` (`overseer web`, web.rs) draws
  the same Full surface into a `TestBackend` and streams the buffer as
  JSON over a std-only localhost server (SSE frames out, input in via
  `on_ct_event`). Its security model (a shell-capable agent sits behind
  it): a per-install token (`~/.overseer/web/token`, 0600 in a 0700 dir)
  rides the URL fragment `#t=` into localStorage — never a request line
  or Referer; `/events` + `/input` require it (`?t=` / `X-Overseer-Token`,
  constant-time); every request needs a loopback `Host` (DNS-rebinding
  defence) and cross-site `Origin`/`Sec-Fetch-Site` is refused; ≤32
  connections (SSE included), head ≤16 KiB and body ≤64 KiB (checked
  before allocation) under one 10 s wall-clock deadline; each SSE client
  has its own writer thread (a stalled tab never blocks the drive loop);
  CSP `default-src 'self'` holds because the renderer uses DOM + CSSOM,
  never innerHTML. Debug builds serve `web/` disk-first (a refresh, not
  a rebuild); release builds serve only embedded assets. PWA: manifest +
  icons, so localhost installs as an app window (full-bleed in
  standalone mode). The mark lives in ONE file, `web/mark.svg` (the
  bowtie, bold cut) — favicon, icons, panel button, empty/locked states
  and `MARK_GLYPH` (`⋈`) all follow it; icons are rendered from it with
  headless Chrome (22%-radius `#17181b` tile, mark at 60% `#f5f5f6`).
  DECRQM/XTVERSION probe; BSU/ESU
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
  (`OVERSEER_THEME=graphite|default|mono|high-contrast`; truecolor
  terminals and the web get `graphite`, 256-colour `default`, 16-colour
  `ansi`, NO_COLOR/TERM=dumb `mono`). Design rule: the conversation is the
  one primary voice — colour marks state only (tool `●` ok/err, `◌`
  running, `!` warn), tool and meta lines are indented 2 and recede, the
  run summary is one quiet right-aligned line (`19 steps · 1m 12s ·
  $0.042 · 87% cached`), the active panel tab is underline + white, and
  an empty session shows only the mark + `overseer`. Contrast on
  `#1e1f24`: text 11.3:1, dim 4.75:1 (faint is decorative only).
  DECSET-1004 focus tracking gates BEL+OSC 9/99/777
  notifications (unfocused only, escape-sanitized); OSC 133 prompt
  marks + OSC 8 file links emit raw into the scrollback stream (muxes
  off via `caps.osc`); REDUCE_MOTION freezes the spinner;
  `--no-tui` is a plain-text REPL sharing the same worker (`run_line`,
  asks answered by one-line replies, /quit or Ctrl+D exits).
- `eval/` — evaluation rig: local taskspecs + scheduler + run-store
  (`rig/`), external adapters (`rig/benchmarks/` — swe_bench, tau2, lcb,
  terminal_bench/swe_rebench via harbor, swe_live, polyglot), a custom
  harbor agent (`rig/harbor_agents/overseer_agent.py` runs the release
  binary inside task containers), the live memory/orch stress suite
  (`stress/suite.py`), and the Devin-Cloud worker orchestrator
  (`cloud/driver.py` — shard matrix + `cloud/shard_ids.py` deterministic
  id slicing + `cloud/relay.py` localhost x-opencode-session injector
  for clients that can't set headers).

## Invariants (do not violate)

1. Events are immutable; session state is a view over `events.jsonl`.

   The event hash chain is structural — it covers (id, parent_id, type tag, prev hash), not payload bytes: it detects reordered/inserted/dropped/re-typed events, not edited payloads.
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
- Web: `cargo run -p overseer-cli -- web [--port <n>] [--no-open]` (prints
  and opens the tokenized `http://127.0.0.1:<port>/#t=…` URL)
- JSONL event stream: add `--json`; resume: `--resume <session-dir>`
- Heavy work (full test suite, release builds, benchmarks) runs on Devin
  Cloud VMs, not the owner's 16 GB M1: push the branch, then
  `devin --cloud -p --prompt-file <brief.md>` from a checkout under a
  trusted workspace (`~` is trusted; `/tmp` is not). Local = debug builds
  and narrow gates only. CI (`ci.yml`) runs on the self-hosted Mac runner
  for same-repo PRs and `main` pushes — fork PRs are refused.

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
