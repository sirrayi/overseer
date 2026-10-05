# Overseer Life: security design and threat model (Phase 0.7, v1, 2026-10-05)

Companion to `2026-10-05-digital-life.md`. Status: DRAFT for owner review. Scope: the Rust core, connectors, agents,
automations, sync, and the Apple shell's security-relevant parts (browser, vault UI, approvals). Product copy will not say
"military grade"; this document lists the concrete controls and the tests that prove them.

## 1. Assets

| ID | Asset | Why it matters |
|---|---|---|
| A1 | Other apps' logins we read (Claude, Codex, Cursor, Devin, OpenCode, Pi CLI tokens) | Theft = account takeover; a careless refresh logs the user out of their CLI |
| A2 | Our own tokens and keys (X OAuth, admin API keys, bank consent, IMAP app passwords) | Theft = account access; some can spend money |
| A3 | Personal data (usage, spend, follower lists, transactions, receipts, browsing history) | Privacy; profiling |
| A4 | Action capability (post, buy, cancel, delete, change security settings) via APIs, browser, computer use | Irreversible real-world effects |
| A5 | Integrity of the event log, audit log, automations and approvals | Lets the user trust what happened and why |
| A6 | Keys: DB key, vault key, sync key | Protect everything above |

## 2. Adversaries and failure sources

| ID | Who / what | Likelihood | Notes |
|---|---|---|---|
| T1 | Hostile content in pages, emails, posts, DMs, API responses (prompt injection) | High | The dominant attack on agentic browsers in 2026 (Trail of Bits on Comet, Zenity PleaseFix, UW same-origin study) |
| T2 | Upstream endpoint returns malformed or hostile data, or changes meaning silently | High | Undocumented endpoints in particular |
| T3 | Malware running as the same macOS user | Medium | Can read user files and drive the UI; cannot use Secure-Enclave keys without user presence |
| T4 | Lost/stolen Mac or iPhone | Medium | Locked vs unlocked matters |
| T5 | Supply chain: crates, Synara ports, cua-driver build, web extensions | Medium | |
| T6 | Sync transport or cloud account compromise (CloudKit/iCloud) | Low–medium | Must only ever see ciphertext |
| T7 | Model providers receiving our prompts | Certain (by design) | Data minimisation, not secrecy, is the control |
| T8 | The agent itself over-reaching or misreading (no attacker) | High | Same controls as T1 |
| T9 | Shoulder surfing, screen sharing, screenshots | Medium | |

## 3. Trust boundaries

```
 web content ─B1─┐            ┌─B4─ model providers (LLM)
 remote APIs ─B2─┤            │
 other apps' ─B3─┼── Rust core (store, vault, connectors, agents, automations) ──B6── helpers (connector worker, cua-driver)
 credential      │            │
 stores          │            └─B5─ sync transport (CloudKit) ── iPhone
 user/approvals ─B7┘
```

## 4. Controls by boundary

### B1 Web content (in-app browser)
- Agent scripts run only in a dedicated `WKContentWorld`; pages cannot see or modify them.
- Everything read from a page is tagged untrusted and sets overseer's untrusted latch (Rule-of-Two in `perm.rs`).
- The agent is origin-scoped: it acts only on the origin the task names. Reading another origin, opening a new origin, or
  submitting a form cross-origin needs a grant (approval trust model, section 6).
- No `file://` navigation by agents; downloads go to a quarantine folder and are never opened by an agent.
- Profiles use separate `WKWebsiteDataStore` identifiers; an agent bound to profile P cannot read profile Q.
- Extensions (`WKWebExtension`) are user-installed only, never by an agent, and are listed with their permissions.

### B2 Remote APIs
- TLS only (rustls + platform verifier, as overseer-core). Each connector declares its origin allow-list; the HTTP layer
  refuses any other origin and any redirect leaving it.
- Response size caps and timeouts; parsers never panic; a failed parse is a visible `Error`/`Unavailable` status, never
  silently zero.
- Undocumented endpoints are labelled (`Provenance.documented = false`) and per-source opt-in with a kill switch.
- Spend-bearing APIs (X) enforce a per-day cap in the core before each request, in addition to the vendor's console limit.

### B3 Other apps' credential stores (standing connector rules, from Phase 0)
- R1: never call another app's OAuth token/refresh endpoint and never redeem its refresh token; an expired token is
  reported as `expired`.
- R2: open other apps' files and databases read-only (SQLite with `mode=ro`); never write, move or chmod them.
- R3: Keychain secret reads happen only after the user enables that source, when the user is present to answer a system
  dialog; attribute-only existence checks are allowed for discovery.
- R4: secrets never appear on argv, in logs, snapshots, errors, exports or probe output; only an 18-character SHA-256
  fingerprint identifies an account.
- R6: we do not spawn other providers' CLIs.

### B4 Model providers (LLM)
- Agents receive credential sentinels from overseer's broker (`cred.rs`), never raw secrets.
- Data minimisation by module: aggregated metrics may enter prompts; raw follower lists, transactions, receipts and browsing
  history enter only when the task needs them and the user allowed that agent to see that module.
- The user picks the model provider per agent; a "local model only" option is planned for sensitive modules.

### B5 Sync (Mac ↔ iPhone)
- Records are encrypted on the device (AEAD, random nonce per record, record id + type as associated data) before upload;
  CloudKit stores ciphertext only.
- The sync key lives in iCloud Keychain (synchronizable item). It is separate from the DB key, because Secure-Enclave keys
  and `ThisDeviceOnly` items cannot sync.
- A new device joins only after approval on an existing device.

### B6 Helpers
- Connector workers run as a separate process with no Keychain access of their own; the core passes them only the
  short-lived token for the call. (Phase 1 decision: XPC service vs plain child process; verified by test either way.)
- cua-driver is built from a pinned source commit with a checksum-verified patch (Synara's manifest pattern); computer use
  keeps Synara's guardrails: physical-Escape kill switch, input epochs, background-first, explicit foreground consent,
  protected authentication dialogs, and an action audit.

### B7 User and approvals
- Approval prompts show the exact action, target origin/account and data involved; they cannot be triggered or worded by
  page content.
- Approvals from the iPhone are signed by the device key and bound to one action id; replay is rejected.

## 5. Data at rest

| Item | Store | Protection |
|---|---|---|
| App database (snapshots, time series, event log) | SQLite with SQLCipher | Random 256-bit key, wrapped by a Secure-Enclave P-256 key; unwrap requires user presence (Touch ID or password) at unlock |
| Our secrets (A2) | Keychain | `kSecAttrAccessibleWhenUnlockedThisDeviceOnly` + access control requiring user presence; written through the Security framework API, never via `security` argv |
| Sync key | iCloud Keychain | Synchronizable item, separate from the DB key |
| Exports/backups | File | Encrypted with a user passphrase or recovery key; plaintext export needs an explicit confirmation |
| Logs | File | No secrets (test-enforced); personal values redacted unless debug logging is enabled for one session |

Open Phase 1 questions: SQLCipher crypto backend on Apple (CommonCrypto vs vendored OpenSSL) and its fit with `deny.toml`;
whether the event log lives inside SQLCipher or as AEAD-sealed JSONL segments like overseer's `events.jsonl`.

## 6. Approval trust model
- Grant scopes: once, this task, or always for (origin or connector) + action class. Grants expire and are revocable
  on one Permissions page; they extend overseer's persisted rules in `~/.overseer/rules` (deny rules still win).
- Always ask, no standing grant possible: payments and purchases, subscription cancellation, account deletion, changes to
  security settings (passwords, 2FA, recovery), sending money, posting publicly from a new origin.
- Automations run with the grants of the user who created them; if a needed grant is missing they pause and notify, never
  escalate.

## 7. Integrity and audit
- Every agent, automation and connector action is an event in the append-only log (overseer invariant 1).
- The audit log is hash-chained (each entry carries the hash of the previous one); a broken chain is shown, not repaired
  silently. (Overseer's "event payload hashing" is still deferred; this design depends on it.)

## 8. Platform hardening
- Hardened runtime, Developer ID signing and notarization once the Apple account exists (D10); until then, local ad-hoc
  builds only.
- Overseer's startup posture stays (`harden_startup`: umask 0o077, proxy-env scrub; private dirs).
- Auto-lock after idle and on sleep; privacy blur when screen sharing or on demand; sensitive values hidden in
  notifications and widgets by default.
- Supply chain: `cargo deny check` (licenses, advisories, duplicate versions), pinned Synara/Cua sources with checksums,
  NOTICE files, SBOM at release.

## 9. Verification (tests that must exist before the matching phase closes)

| Control | Test | Phase |
|---|---|---|
| R1 no foreign refresh | Expired-token fixtures assert zero requests to any token endpoint | 0 |
| R4 no secrets in output | Serialize every snapshot/error from fixtures and assert no fixture secret substring | 0 |
| Origin allow-list | Request or redirect to an unlisted origin fails | 0 |
| X spend cap | Over-cap request is refused before sending; UTC day rollover resets | 0 |
| DB at rest | DB file contains no known plaintext marker; opening without the key fails | 1 |
| Keychain writes | No secret on argv (process-list check during write) | 1 |
| Logs | Secret-leak scan of all logs after the full test suite | 1 |
| Browser isolation | Page script cannot read agent-world variables; profile P cannot read profile Q cookies | Frontend stage |
| Injection | overseer's injection corpus extended with page/email/post payloads; the exfil gate forces Ask | 4 |
| Approvals | Always-ask classes cannot be granted "always"; replayed iPhone approval rejected | 4 / 7 |
| Sync | CloudKit records contain no plaintext; tampered record rejected | 7 |
| External review | Independent pen test before any release to other users | 8 |

## 10. Residual risks (accepted for "me first")
- Same-user malware (T3) can read whatever the unlocked app shows and can drive the UI; Secure-Enclave wrapping only
  protects the DB key while the app is locked.
- Undocumented endpoints and scraping can break or violate a site's terms (accepted under D2; opt-in, labelled).
- Model providers see the data an agent is allowed to see (T7); minimisation reduces, not removes, this.
