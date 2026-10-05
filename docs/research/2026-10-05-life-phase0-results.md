# Overseer Life: Phase 0 results (backend part, 2026-10-05)

Plan: `2026-10-05-digital-life.md`. Security design: `2026-10-05-life-security.md`.
Raw, personal results (usage values, tool lists, audit output) stay local in the ignored `notes/2026-10-05-phase0/`.

## Status by step

| Step | Result | Status |
|---|---|---|
| 0.1 Base branch | #51 then #52 merged into `review` (ac9077e); tree identical to the tested tip 6cf02ad; no workflow runs triggered | Done |
| 0.5 AI connectors | `crates/overseer-life`: Rust ports of Synara's Claude, Codex, Cursor, Devin and OpenCode usage readers plus Claude/Codex local archive totals. One live read-only run: Cursor, Devin and OpenCode returned real, plausible usage (percentages in 0–100, resets in the future). Claude and Codex report `needs_auth`: on this Mac `~/.claude` and `~/.codex` hold only `skills/`, no logins or archives | Done for installed tools |
| 0.5 X connector | OAuth 2.0 PKCE (public client), counts, incremental and full follower scans, per-day cost cap with corrected owned-read pricing; tested against a mock server. `examples/x_spike.rs` is ready for the live run | Waiting on the user (X developer app + credits, present for login) |
| 0.5 YouTube/GitHub/Bluesky | Moved to Phase 3 (D6: X first) | Deferred |
| 0.6 iOS audit | `overseer-core --no-default-features` and `overseer-life` compile for `aarch64-apple-ios-sim`. The only blocker is QuickJS (`code-mode`): `rquickjs-sys` ships no iOS bindings. 15 production process-spawn sites listed for a `host-tools` feature gate | Done |
| 0.7 Security design | `2026-10-05-life-security.md` | Draft, needs owner review |
| 0.9 Money sources | — | Waiting on the user's bank and email providers |
| 0.10 Computer use | Synara's patched cua-driver (0.28.2, rev 39) builds from the pinned commit with its checksum-verified patch. Its MCP tool surface is identical to the installed driver (0.26.1): 57 tools, byte-identical schemas, so Overseer's `computer` tool can switch drivers without API changes | Done (no desktop actions run) |

## Connector rules proven by tests
- No other app's OAuth token/refresh endpoint is ever called; expired tokens report `expired` with zero requests.
- Secrets never appear in snapshots, errors or probe output (fixture-secret scan test); only 18-character fingerprints.
- Origin allow-list per call, no redirects, 1 MiB response cap, 64 KiB request cap, 10 s timeout, 429 → `rate_limited`.
- The live probe is capped at one request per source by the transport.
- Subprocess calls (`security`, `sqlite3`) time out and are killed; Keychain secret reads need an explicit opt-in.
- Cursor's database is opened read-only. SQLite still maps the existing `-shm` file, so its mtime changed during the probe; nothing was written to the database.

## Findings to act on later
- X pricing: `/2/users/me` is a $0.010 user read; followers/following are $0.001 owned reads only when the authenticated user owns the developer app. Whether `/2/users/me` debits per request or per resource is confirmed by the live spike (comparison with the Developer Console).
- X does not document newest-first follower ordering; the incremental "new followers" scan depends on it and is verified in the live spike.
- The X spend ledger is per process; Phase 1 persists it so the daily cap spans processes.
- Overseer `computer` tool vs driver schemas (existing code, fix in a separate change):
  - `browser_type` requires `ref` in the driver; Overseer only requires `text`.
  - `zoom` requires `x1..y2`; Overseer does not enforce them.
  - `session` is sent on every call but undeclared in 7 tool schemas (tolerated at runtime today).
  - `from_zoom` is undeclared on `right_click`/`double_click`; `x`/`y` undeclared on `set_value`.
- iOS: add a `host-tools` feature to overseer-core and drop `code-mode` from iOS builds; gate overseer-life's `security`/`sqlite3` paths; memory falls back to plain files without git.

## Gate evidence (2026-10-05, this Mac)
`cargo fmt --all --check` clean; `cargo clippy -p overseer-life --all-targets --examples -- -D warnings` clean;
`cargo test -p overseer-life` 53 passed; `cargo check --workspace --all-targets` clean; `cargo deny check` ok;
`cargo check -p overseer-life --target aarch64-apple-ios-sim` clean.
