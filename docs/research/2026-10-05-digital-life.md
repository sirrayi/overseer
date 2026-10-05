# Overseer Life: digital life dashboard + harness — research memo and plan (v2, 2026-10-05)

Status: APPROVED 2026-10-05. Phase 0 (backend part) in progress on `feat/life-phase0`.

## 0. Contract

Original request (verbatim): "we're gonna heavily borrow things from https://github.com/Emanuele-web04/synara its opensource
and its a great app/harness but we're also not just making a harness, i just came up with a new idea, digital life dashboard +
top of the line harness. top of the line harness is already what we're working on. we need to go ahead with the digital life
dashboard, need heavy planning for this so get into plan mode. i was thinking, a full fledged dashboard with almost all you
digital info. all your ai sub accounts, info on them, usage, billing, sessions, tokens, etc. all your social's infos, X
followers, following, increase/decrease counts everytime you open the app, current engagement metrics, overall every digital
subscription you have, all in one place to access/manage from, the whole app must have supreme top of the line computer use.
lean as possible, beautiful in every way, in app browser, light as possible so you dont even need another browser, everything
packed with tight, top of the line, military kind of grade security. automations, agents for specific tasks. etc. need a lot
of detailed research on this digital life dashboar dthing and how much and what more we can add"

User decisions so far:
| ID | Question | Answer |
|---|---|---|
| D1 | Audience | Me first, product later |
| D2 | Data collection | Everything, opt-in per source (official APIs + local AI logins + logged-in browser pages + computer use) |
| D3 | Stack | "rust obviously"; best performance on macOS AND Windows; macOS first, iOS soon (macOS/iOS now, Windows/Linux later) |
| D4 | Apple Developer account | Not yet; will get one soon |
| D5 | GitHub | PRs etc. are fine; do NOT trigger GitHub Actions (no minutes) |
| D6 | First social platforms | X first |
| D7 | Repo location | Inside the Overseer repo ("this is still a part of Overseer") |
| D8 | Base branch | Merge #51 then #52 first — done 2026-10-05: `review` = ac9077e (tree identical to 6cf02ad) |
| D9 | Order | Backend first; frontend and UI/UX later. Phase 0 runs its backend spikes now; the UI/browser spikes move to the start of the frontend stage |
| D10 | Apple Developer account | Later (enrollment step 0.11 deferred) |
| D11 | Bank / email providers | User will supply (needed for 0.9) |

This document: research memo, feature catalog ("what more can we add"), architecture, phased plan with acceptance criteria
and verification, risks, open decisions. Standing exclusions unless the user says otherwise: no GitHub Actions runs, no
spending, no account sign-ups, no writes to other apps' credentials or data.

## 1. Research findings (sourced)

### 1.1 Synara (github.com/Emanuele-web04/synara, cloned at /tmp/synara-research, HEAD f2069a6, 2026-10-05)
- License MIT; copyright "T3 Tools Inc." and "Emanuele Di Pietro" (it is a fork of T3 Code). Borrowing is allowed if the
  notice is kept. Its computer use builds Cua's driver (MIT, Cua AI Inc.) with a ~1 MB Synara patch to `libs/cua-driver/rust`
  (105 files; "native revision 39", driver 0.28.2) — `packages/shared/src/cuaDriverRelease.json`.
- Stack: Electron 43.4.1 desktop shell + Bun/Node server (Effect, SQLite) + React web UI. Heavy by design (bundles Chromium).
- Directly relevant pieces (all TypeScript; under a Rust/Swift plan these are ported, not linked):
  | Area | Files | What it does |
  |---|---|---|
  | AI plan usage | `apps/server/src/providerUsage/providers/{claude,codex,cursor,devin,droid,grok,opencode,antigravity}.ts` | Live plan/limit usage per account. Endpoints seen: `api.anthropic.com/api/oauth/usage`, `chatgpt.com/backend-api/wham/usage`, `api2.cursor.sh/...DashboardService/GetCurrentPeriodUsage`, Devin `SeatManagementService/GetUserStatus` (server.codeium.com), `api.eu.factory.ai/api/billing/limits`, `opencode.ai/zen/go/v1/usage`. Undocumented endpoints. |
  | Credentials | `providerUsage/credentials.ts`, `providerUsage/providers/localCredential.ts` | Reads CLI logins (JSON files, macOS Keychain via `security`), OAuth refresh with atomic write-back of rotated refresh tokens. |
  | Local token stats | `providerUsageSnapshot.ts`, `profileStats.ts`, `claudeTokenStats.ts` | Parses `~/.codex/sessions` and `~/.claude/projects` archives for 7d/30d tokens. |
  | Multi-account | `docs/providers/claude.md`, `providerAccountHomePath.ts` | Several Claude/Codex accounts side by side via per-account HOME. |
  | Browser vault | `apps/desktop/src/browserAutomation/{browserVault,vaultKeyProtection}.ts` | Credential vault; key protected by OS store or scrypt master password + AES-GCM. |
  | Cookie import | `apps/desktop/src/browserAutomation/browserCookieImport.ts` | Imports sessions from Chrome/Safari/Edge (via betterwright 2.7.3, MIT per npm; built on playwright-core, Apache-2.0). |
  | Browser automation | `browserAutomation/betterwright*.ts`, `cdpRuntime.ts`, `betterwrightNetworkGuard.ts` | CDP-driven in-app Chromium automation with network guard. |
  | Computer use | `apps/desktop/src/{cuaDriverHost,computerShield,escapeKillSwitchMonitor}.ts`, `docs/computer-use-cua/README.md` | Background input without stealing focus, physical-Escape kill switch, input epochs, foreground consent, audit history. macOS only. |
  | Automations | `apps/server/src/automation/*` | Scheduled agent runs attached to threads. |
- Synara's own AGENTS.md rules (TS/React) do not apply to our code; they are reference material.

### 1.2 Overseer today (this repo, `feat/overseer-next` @ 6cf02ad)
- Rust workspace: core engine, CLI, gateway daemon (Telegram/webhook channels, triggers), TUI + localhost web UI.
- Already has: event-sourced logs, budgets, permission gate + Rule-of-Two taint latches, memory v2, subagent tiers, MCP,
  QuickJS `run_code`, credential broker sentinels, cua-driver computer use (MCP over stdio), sandboxed bash.
- `overseer-core` uses `ureq` (sync, rustls), no tokio; 15 source files spawn processes — these must be feature-gated
  for iOS (App Store apps cannot spawn processes).
- CI: `ci.yml` (self-hosted Mac runner) and `release.yml` (GitHub-hosted ubuntu, uses minutes) BOTH run on `pull_request`.
  Pushing a non-main branch triggers nothing; opening/updating a PR triggers both.

### 1.3 Data-source feasibility (checked 2026-10-05)
| Source | Official access | Notes / cost |
|---|---|---|
| X | Pay-per-use credits, no subscription tiers since 2026 (docs.x.com pricing; third-party summaries dated Sep–Oct 2026) | Owned reads (your own app reading your own data: followers, following, posts, mentions) $0.001/resource; user read $0.010; 24 h UTC de-dup ("soft guarantee"); 3M post reads/month cap. Counts on open ≈ $0.001–0.01. Full follower diff costs N×$0.001 (5,000 followers ≈ $5 per full scan) — new followers are cheap (page until a known ID), unfollowers need a full scan. |
| YouTube | Data API v3 `channels.statistics` | Free quota (10,000 units/day). `subscriberCount` rounded to 3 significant figures. |
| Instagram | Instagram API with Instagram Login | Business/creator accounts only; personal accounts not supported; some metrics need ≥100 followers; up to 48 h delay. Personal accounts → browser-session connector. |
| TikTok | Login Kit `user.info.stats` | follower/following/likes/video counts for the logged-in user. |
| Bluesky | AT Protocol `app.bsky.actor.getProfile` | Public, free; followersCount/followsCount/postsCount. |
| Threads, LinkedIn, Reddit | Threads API insights; LinkedIn personal follower data not available to ordinary apps; Reddit API for personal scripts | LinkedIn → browser-session connector. (Threads/Reddit details to confirm in Phase 0.) |
| Anthropic API spend | Admin API `/v1/organizations/usage_report/messages`, `/v1/organizations/cost_report` | Needs an organization + admin key; "unavailable for individual accounts". Daily buckets. |
| OpenAI API spend | Admin API `/v1/organization/costs`, `/v1/organization/usage/*` | Admin key; daily cost buckets; spend limits API. |
| Claude / ChatGPT / Cursor / Devin / OpenCode plans | No official API | Synara's undocumented endpoints via the CLIs' own logins (see 1.1). Local CLIs present on this Mac: Claude Code, Codex, Cursor, Devin, Codeium/Windsurf, OpenCode, Pi, gh. |
| Bank (UK) | Open banking aggregators | GoCardless Bank Account Data closed to new personal sign-ups (Actual/Firefly issues, 2025). Enable Banking: free "restricted mode" for your own linked accounts — UK coverage UNVERIFIED (docs say EEA). TrueLayer needs business onboarding. Fallbacks: bank's own personal API (e.g. Monzo, Starling — to confirm), CSV/OFX import. |
| Email receipts | Gmail API / IMAP | Gmail read scopes are "restricted". An unverified (Testing-status) OAuth client works for you but its refresh tokens expire every 7 days (weekly re-consent) and it is capped at 100 users; removing that needs Google verification + annual CASA assessment. For "me first", IMAP with an app password (Gmail, iCloud, Outlook) avoids both. |
| Apple subscriptions | No API | Email receipts, or browser-session / computer-use connector on the Subscriptions page. |

### 1.4 UI stack (checked 2026-10-05)
- Pure-Rust GPU UIs: GPUI (Zed) is pre-1.0 with frequent breaking changes and Zed-internal accessibility; Iced/Slint/egui
  render their own widgets, have partial accessibility, and none embeds a browser engine. Dioxus desktop uses the same system
  webview as Tauri (one report: ~98 MiB idle for a todo app); its native renderer Blitz is pre-alpha.
- Tauri 2: Rust + system webview; small bundles and far lower memory than Electron in community benchmarks
  (figures vary 34–90 MB idle by app). Several webviews in one window still require the `unstable` feature.
- Native shell + Rust core over UniFFI is a proven pattern on Apple platforms (Krust, R-Shell, JayJay, Nimbus;
  Element X iOS uses matrix-rust-sdk through UniFFI — general knowledge). R-Shell ships both a native AppKit/SwiftUI
  macOS app and a Tauri build over one Rust core.
- Browser engine: on Apple platforms the only lean, production-grade engine is WebKit (WKWebView). Chromium/CEF adds
  ~150+ MB; Servo is embeddable (0.1.0 on crates.io Apr 2026, 0.6.0 by Sep 2026, MPL-2.0) but not ready to be someone's only browser.
- Browser parity on WebKit (what "don't need another browser" requires):
  | Capability | How | Gate |
  |---|---|---|
  | Extensions (Chrome/Firefox MV2/MV3) | `WKWebExtension` + `WKWebExtensionController`, public API since macOS 15.4 / iOS 18.4; used by third-party browsers (e.g. Nook, zer0) | None; every tab config must derive from the controller's base configuration |
  | Password autofill (iCloud Passwords, 1Password, …) | System autofill in WKWebView | Restricted `com.apple.developer.web-browser` entitlement (Apple approval). Fallback without it: our own vault filling via an isolated content world |
  | Passkeys / security keys on any site | WebKit handles WebAuthn | `com.apple.developer.web-browser.public-key-credential` entitlement: Account Holder of a paid account requests it, Apple reviews, app must behave as a real browser. Fallback: password + 2FA, or "open this login in Safari" |
  | Profiles, blocking, isolation | `WKWebsiteDataStore(forIdentifier:)` (macOS 14/iOS 17), `WKContentRuleList`, `WKContentWorld` | None |
  Tauri/wry does not expose `WKWebExtension`, per-identifier data stores or content worlds directly; they would need custom objc2 code.

### 1.5 Agentic-browser security (2026)
- Trail of Bits audit of Perplexity Comet (Feb 2026): four prompt-injection techniques extracted Gmail data.
- Zenity "PleaseFix" (Mar 2026): zero-click hijacks of Comet and other agentic browsers; local-file leakage fixed in Feb 2026.
- University of Washington study of seven agentic browsers (agent-security.cs.washington.edu/agentic_browsers_sop.html):
  agents with cross-origin access + prompt injection defeat the same-origin policy (proof of concept on ChatGPT Atlas).
- Consequence for us: page/email/social content is always untrusted; the agent's reach must be origin-scoped and
  consequential actions must need approval. Overseer's Rule-of-Two latches are the right base.

### 1.6 Market
Consumer "digital life" hubs exist but are shallow (subscription trackers: Nexpend, Abonesepeti; vault+bills: OneKey;
AI assistants: Visionary). None found combines AI-account telemetry, social analytics, subscriptions, a real browser, computer
use and local-first security. (Not finding a competitor is not proof none exists.)

## 2. Three-perspective analysis
1. User value: the killer loop is "open the app → see what changed since last time" (followers ±, AI quota left, spend,
   renewals, security alerts), then act in place (browser/agent). Speed of that first screen matters more than breadth.
2. Feasibility: AI accounts (Synara fetchers + local archives) and X/YouTube/GitHub/Bluesky are feasible now; Instagram
   personal, LinkedIn, Apple subscriptions and most "manage/cancel" actions depend on the in-app browser + agents.
3. Risk/economics: undocumented endpoints and page scraping break without notice and some sites' terms forbid them (D2
   accepted this with per-source opt-in); X diff cost scales with follower count; the agent + logged-in browser combination
   is the main security risk; App Store review may reject parts on iOS (scraping, JS execution) — distribution for "me
   first" can be TestFlight/ad hoc.

## 3. Recommended architecture (answers D3)

"Best performance on every platform" = one Rust core + a thin native shell per platform.

```
Apple shell (SwiftUI + AppKit/UIKit where needed; one codebase for macOS + iOS)
  Home ("since last open") · module dashboards · browser (WKWebView) · agents · automations · vault/lock
                 │  UniFFI: typed async calls + event stream
Rust core (new crates in the Overseer workspace)
  life-store    SQLCipher DB (encrypted at rest) + time-series snapshots + append-only event log (reuses overseer event model)
  life-vault    secrets behind a KeyStore trait: Keychain + Secure Enclave key wrapping on Apple; DPAPI/TPM later on Windows
  life-connect  Connector trait (describe/auth/fetch→Snapshot), scheduler, per-connector egress allowlist, budgets, fixtures
  life-metrics  deltas since last open/last view, trends, anomalies, cost forecasts
  life-agents   overseer-core agents with scoped tools/connectors, tiers, permission gate, Rule-of-Two, memory v2
  life-auto     triggers → actions on overseer-gateway (time, threshold, event, email, webhook) with approvals
  life-browser  browser-agent protocol (page snapshot, element refs, actions, proofs) — implemented by each shell's engine
  life-sync     E2E-encrypted replication of the event log; transport is pluggable
  life-ffi      UniFFI bindings (Swift now; C# or Tauri later)
macOS helpers: connector worker process(es) with minimal entitlements, Synara-patched cua-driver, LaunchAgent daemon
Windows later: WinUI 3 + WebView2 shell (UniFFI C# bindings, e.g. NordSecurity's uniffi-bindgen-cs) or a Tauri shell — decided then
```

Why not Tauri as the primary shell: the UI itself would live in a WebKit content process; native controls, energy use,
accessibility and deep WKWebView control (profiles, content rules, passkeys, trusted input) are easier natively, and SwiftUI
shares UI between macOS and iOS. Why not pure-Rust UI: no browser embedding, immature accessibility, API churn.
This recommendation is checked by measurement in Phase 0 (spike S1) before committing.
Hinge (from review): native wins because the in-app browser is the product's centre (extensions, profiles, content worlds,
entitlement-bearing WKWebView). If the browser were cut down to a utility pane, Tauri-everywhere desktops + a SwiftUI
iOS app would be the better fit for "macOS and Windows" — revisit if that scope changes.

Key mechanisms:
- Browser: WKWebView per tab; `WKWebsiteDataStore(forIdentifier:)` gives isolated profiles (e.g. two X accounts);
  `WKContentRuleList` for tracker/ad blocking; agent scripts run in a separate `WKContentWorld` so pages cannot see or
  tamper with them; on macOS, trusted input via real NSEvents/cua-driver because JS-dispatched events are `isTrusted=false`
  (some sites reject them); on iOS only JS-level actions are possible.
- Computer use (macOS): adopt Synara's patched cua-driver build (MIT) and port its guardrails (physical-Escape kill switch,
  input epochs, background-first, explicit foreground consent, protected auth dialogs, action audit).
- Security baseline ("military grade" made concrete and testable):
  local-first, no telemetry; DB encrypted at rest (SQLCipher) with the key wrapped by a Secure Enclave key and released by
  Touch ID/password; secrets only in Keychain (`WhenUnlockedThisDeviceOnly`, biometric access control), never in DB/logs;
  agents see credential sentinels, never raw tokens (overseer cred broker); connectors run in a separate low-privilege
  process with a per-connector domain allowlist; Rule-of-Two + origin-scoped browser agent + approval for consequential
  actions (send, post, buy, cancel, delete, change security settings); hash-chained audit log of every agent/automation
  action; auto-lock + privacy blur; signed, notarized, hardened-runtime builds; cargo-deny + SBOM; encrypted backups
  (passphrase/recovery key); written threat model (stolen device, malware as same user, malicious page/email/post,
  compromised connector/API, supply chain); independent pen test before any public release.
- Approval trust model (avoids approval fatigue): grants are scoped (once / this task / always for this origin + action
  type), expire, and are listed and revocable on one Permissions page (extends overseer's persisted "always" rules);
  money, security-setting and account-deletion actions always ask, on Mac or iPhone.
- Sync (Mac ↔ iPhone): recommended transport for Apple-only phase is CloudKit private database carrying only ciphertext,
  with the data key synced through iCloud Keychain; CloudKit silent pushes notify the phone. The core's sync trait lets
  Windows use another transport later. Mac stays the "hub" that runs automations and computer use; iPhone views, approves
  and runs API-only connectors.

## 4. Feature catalog ("everything we can add")

Core (requested): AI accounts · socials · subscriptions · in-app browser · computer use · automations · agents · security.

| Module | Features | Phase |
|---|---|---|
| Home | "Since last open" digest; changes ranked by importance; one-tap drill-down; daily brief | 1 |
| AI accounts | Plan, limits, reset countdowns per account (Claude, ChatGPT/Codex, Cursor, Devin/Windsurf, OpenCode…); API spend by day/model (OpenAI/Anthropic admin APIs, OpenRouter credits); tokens + sessions from local archives (Claude, Codex, Overseer, Devin); alerts; "best account right now" routing for Overseer tasks; API-key inventory (names/locations only) + leaked-key scan of logs with a rotation checklist | 2 |
| Socials | Followers/following with ± since last open; new followers / unfollowers; engagement rate, top posts, mentions; growth charts; anomaly alerts; drafts + scheduled posts with approval | 3 |
| Browser | v1: tabs, profiles, content blocking, extensions (WKWebExtension), own-vault autofill, downloads, history search, reader mode, session-connector SDK. v2: agent sidebar, "watch mode", system password autofill + passkeys (need Apple entitlements) | 1 / 4 |
| Subscriptions & money | Recurring-charge detection (bank, email receipts, App Store, manual); renewal calendar; trial-ending and price-rise alerts; unused-subscription detection; monthly/annual totals, multi-currency; agent-assisted cancel with approval | 5 |
| Automations & agents | Triggers (time, threshold, event, email, webhook) → actions (agent task, browser task, computer-use task, notify); templates; run history; budgets; specialist agents (accountant, social manager, security officer, subscription canceller, dev-ops) | 6 |
| iPhone | 7a read-only app + E2E sync; 7b approvals, notifications, widgets; 7c browser + on-device API connectors | 7a–7c |
| Identity & security | Account inventory; 2FA/passkey status per account; breach monitoring (HIBP); third-party app grants review (Google/GitHub/X); active sessions/devices; recovery-codes vault | 8 |
| Extras (later, pick) | Developer hub (GitHub PRs/CI, cloud bills, domains + SSL expiry, self-hosted runner status); storage quotas across clouds; inbox health + newsletter unsubscribe; data-broker opt-outs and GDPR export tracker; device inventory/backups; screen-time style app usage; calendar; software-license vault; warranties/documents; watch-only crypto; loyalty points; digital-legacy/emergency access; menu-bar glance; natural-language Q&A over all your data | 8+ |

## 5. Phased plan

Operating rules for every phase: work on branches off the agreed base; local gates only (no GitHub Actions — branch pushes
are fine; PRs only when asked, and every PR head commit carries `[skip actions]`, or the owner changes the PR triggers;
if required status checks are ever enabled, a skipped PR cannot merge until a non-skipped commit runs them);
each module ships thin first (read-only), then adds actions.

Critical path outside our control: Apple Developer enrollment → web-browser and passkey entitlement requests (Apple review,
Account Holder only) → system autofill/passkeys in the browser → logins to sites that require passkeys. Enrollment and the
requests should start in Phase 0 so the review runs in parallel with Phases 1–3. (D10: the user enrolls later; Apple's review
clock starts then.)

### Phase 0 — Decisions and spikes (local only)
Backend now (D9): 0.1, 0.5, 0.6, 0.7, 0.9, 0.10. Frontend stage, later: 0.2, 0.3, 0.4, 0.4b, 0.8; 0.11 with D10.
Phase 0 code lives in `crates/overseer-life` on `feat/life-phase0`; personal results stay in the ignored `notes/`.
| Step | Output | Done when |
|---|---|---|
| 0.1 | Base branch: #51 → #52 merged into `review` (D8) | Done 2026-10-05 |
| 0.2 S1 | Same Home screen + one browser pane in SwiftUI and in Tauri; measure idle RSS, energy impact, cold start, scroll smoothness on this M1 | Numbers recorded; stack confirmed or changed |
| 0.3 S2 | Rust ↔ Swift via UniFFI on macOS and iOS simulator; async + event stream | Round-trip works on both |
| 0.4 S3 | WKWebView: profiles, content rules, content-world agent script, page snapshot → element refs, trusted click on macOS | Demo on 3 real sites |
| 0.4b | Same spike: load one Chrome extension through `WKWebExtension` into a profile tab | Extension runs its content script and popup |
| 0.5 S4 | Connector feasibility with real accounts, read-only: Claude, Codex, Cursor, Devin usage (port Synara), X owned read, YouTube, GitHub, Bluesky. For X, record the actual credit debit of a counts call and a one-page follower read | Each returns a real snapshot or a recorded blocker; X cost per call measured, not taken from docs |
| 0.6 | overseer-core iOS build audit: which features must be gated (process spawning, sandbox, bash) | List + feature plan |
| 0.7 | Security design + threat model document | Reviewed and approved |
| 0.8 | Design language: typography, color, motion, card system, light/dark | Approved mock of Home |
| 0.9 S5 | Money sources: Enable Banking with your UK bank (does it list it?), your bank's own personal API if any, CSV/OFX import; email receipts via IMAP app password vs Gmail Testing-mode OAuth | One bank source and one email source chosen with evidence |
| 0.10 S6 | Build Synara's patched cua-driver (rev 39) and run Overseer's existing `computer` tool against `cua-driver mcp` | MCP contract works, or the port of Synara's host protocol is scoped |
| 0.11 | You: enroll in the Apple Developer Program; request the web-browser and passkey entitlements | Requests submitted |

### Phase 1 — Foundation + browser v1 (macOS; iOS must compile)
Crates life-store, life-vault, life-connect, life-metrics, life-ffi; app shell (sidebar, Home, settings, onboarding, lock
screen with Touch ID, menu-bar glance); "since last open" engine; connector SDK with fixtures and contract tests;
connector health page (last success, stale/broken, kill switch). Browser v1: tabs, profiles, content blocking,
extensions, own-vault autofill, downloads, history; session-connector SDK (declarative DOM extractors with self-check).
The browser comes first because it is the product's centre and the Instagram-personal/LinkedIn/Apple connectors need it.
Done when: encrypted DB verified (file unreadable without key), secrets never appear in DB/logs (test), Home renders from
fixtures, a session connector reads one logged-in page, performance budgets set from S1 numbers are met.

### Phase 2 — AI accounts hub
Port Synara fetchers to Rust (with attribution) + Admin APIs + local archives; multi-account; alerts; Overseer routing hook;
key inventory + leak scan (this alone would have caught the 09-20 key).
Done when: each of the user's AI accounts shows plan/limits/usage that matches its own dashboard on the same day.

### Phase 3 — Socials hub
X (counts on open; incremental new-follower scan; scheduled full diff with a spend cap), YouTube, GitHub, Bluesky first;
Instagram/TikTok/Threads/LinkedIn as their connectors land (API or browser session).
Done when: deltas match platform UIs; X spend per day stays under the cap.

### Phase 4 — Browser v2
Agent sidebar (origin-scoped), watch mode, agent fallback for broken extractors, approval trust model in the browser;
system password autofill and passkeys as soon as the entitlements are granted (fallbacks until then: own vault,
password + 2FA, "open in Safari").

### Phase 5 — Subscriptions & money
Bank and email sources chosen in 0.9, App Store (receipts or session connector), manual entry; detection and calendar;
cancel assistant with approval.

### Phase 6 — Automations & agents + computer use
Trigger/action engine on overseer-gateway; approvals on Mac (and iPhone in Phase 7); specialist agents; Synara-patched
cua-driver + guardrails.

### Phase 7 — iPhone (needs the Apple Developer account for device builds/TestFlight)
- 7a: read-only iPhone app sharing SwiftUI views + E2E sync (CloudKit ciphertext, key via iCloud Keychain).
- 7b: approvals from the phone, notifications, widgets.
- 7c: browser and on-device API connectors (refresh is opportunistic: iOS limits background work; the Mac stays the hub).

### Phase 8 — Identity & security module, hardening, external review

### Phase 9 — Windows, then Linux shells over the same core

## 6. Verification per change
- Rust: `cargo fmt`, `cargo clippy --workspace --all-targets`, `cargo test` for touched crates; `cargo deny check`.
- Swift: `xcodebuild test` (unit + snapshot tests) for macOS and iOS simulator.
- Live connector tests are ignored-by-default and run manually with real accounts (like overseer's `computer_live`).
- Security tests: secret-leak test, at-rest encryption test, injection corpus extended to page/email/social content,
  approval-gate tests for consequential actions.
- Performance: budgets from S1; measured with Instruments/`powermetrics` before each phase closes.
- Visual: screenshots of every screen in light/dark and both platforms reviewed before a phase closes.

## 7. Risks
| Risk | Mitigation |
|---|---|
| Undocumented endpoints/page layouts change | Per-connector opt-in + kill switch, recorded-fixture contract tests, visible "stale/broken" state |
| Terms of service (scraping, automation) | D2 accepted; label each source; read-only by default; user's own accounts only |
| Prompt injection through pages/emails/posts | Untrusted latch, origin-scoped agent, approvals, no cross-origin reads without grant |
| X cost grows with followers | Incremental scans, daily cap, X console spend limit |
| No Apple Developer account yet | Ad-hoc/self-signed local builds until enrolled; passkeys + notarization + TestFlight wait for it; use one stable signing identity so macOS privacy grants survive rebuilds (to verify) |
| overseer-core not iOS-ready | Feature-gate process tools; iOS runs API connectors + agents without bash/computer use |
| App Store review (iOS) | TestFlight/ad hoc while "me first"; review guidelines checked before any public listing |
| Scope explosion | Phase gates; each module ships read-only first |
| Synara license compliance | NOTICE file with Synara + Cua copyright; record which files were ported; check betterwright's LICENSE before porting anything from it |
| Connector upkeep (~15 sources on undocumented APIs/pages) is the long-term cost | Connector health page, weekly fixture refresh, broken-connector alert within one refresh, keep the count small until each is stable |
| Apple review on the critical path (entitlements, TestFlight) | Start enrollment/requests in Phase 0; every browser feature has a non-entitlement fallback |
| Approval fatigue makes the security model unusable | Scoped, expiring, revocable grants (section 3); only money/security/deletion always ask |
| Differentiators (browser agent, computer use, cancel agents) land late | Browser v1 moved to Phase 1; computer-use spike in Phase 0 |

## 8. Open decisions for the user
1. Bank(s) and email provider(s) for the money spike (0.9) — user will supply.
2. X live spike: needs an X developer app owned by the user's X account, a small credit purchase, and the user present for OAuth.
3. Keychain-backed connectors (Claude, Codex): first live read needs the user present (macOS may show a Keychain dialog).
4. Optional: change `release.yml` so PRs don't start GitHub-hosted jobs (`pr-run-mode = "skip"`), instead of `[skip actions]` on every PR.
5. Close #50 (fully contained in #51, now merged)?
6. The checkout sits in iCloud-synced Desktop ("Desktop - m1"); an earlier `.git/index 3` conflict copy came from that. Consider moving the repo out of iCloud sync or marking `target/` (8.8 GB) as not synced.

## 9. Review record
Plan gate, round 1: independent review (fresh context), kept locally at notes/2026-10-05-phase0/plan-review.md. Findings F1–F5 (material/minor-material)
confirmed and applied: F1 money spike added (0.9); F2 Gmail Testing-mode 7-day expiry stated; F3 browser parity matrix
(WKWebExtension found after the review, entitlements + fallbacks); F4 cua-driver MCP spike added (0.10); F5 X metering in
0.5. Minor F6 paths, F7 Servo version, F9 Phase 7 split, F10 required-checks note, F11 risks: applied. Premortem items
1, 3, 5, 7 added to risks; browser v1 moved to Phase 1. Unverified, carried: Enable Banking UK coverage, Threads/Reddit
API details, Instagram metric thresholds, betterwright's own LICENSE file.
