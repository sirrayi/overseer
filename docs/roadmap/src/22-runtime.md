## 20. Runtime, Performance and Distribution

### 20.1 Where we stand

Core has no tokio and no reqwest; it uses `ureq` with rustls and `regex-lite`; the binary is about 5.5 MB; QuickJS added 1.26 MB under a 1.5 MB gate **[src: pillar refresh, tool economy record]**. cargo-dist is configured but installers are off; the `release.yml` plan job fails on every PR. `overseer-core --no-default-features` and `overseer-life` compile for the iOS simulator; QuickJS is the only blocker there **[src: Life Phase 0 results]**.

### 20.2 Workstreams

#### Z1 — Budgets enforced by the build

**What.** Invariant 22. A local gate script (no Actions spend) that fails when any budget is exceeded:

| Budget | Limit |
|---|---|
| Release binary size (all default features) | ≤ 15 MB |
| Cold start to first prompt render (TUI, M1) | ≤ 50 ms |
| Idle RSS of the TUI with no session | ≤ 30 MB |
| Run process RSS after rejuvenation (R17) | ≤ 150 MB at steady state |
| Resident startup tokens | ≤ 2,000 |
| Static prompt base | ≤ 1,000 characters (existing pin) |
| Dependency count growth per wave | Reported; any new dependency needs a decision record line |

The startup, RSS and binary limits are targets until W0 measures today's values on the owner's M1. Where a measurement already exceeds a target, the W1 decision record sets the path to it rather than silently relaxing the number.

#### Z2 — Signed and notarized builds

**What.** Developer ID signing and notarization for macOS (owner's Apple Developer account), stable signing identity so privacy grants survive updates, hardened runtime.

#### Z3 — Installers and safe self-update

**What.** Shell and Homebrew installers from cargo-dist; `overseer update` that installs side by side, keeps the previous version for draining runs (R13), and never replaces the binary under a running process. Setting `pr-run-mode = "skip"` in `dist-workspace.toml` stops the failing PR job (owner decision pending, Chapter 26).

#### Z4 — iOS gating

**What.** A `host-tools` feature in core that removes process spawning, sandboxing and bash for iOS builds; `code-mode` off on iOS; Life's `security`/`sqlite3` paths gated; memory falls back to plain files without git. This is the engine half of the Life iPhone app (F).

#### Z5 — Linux parity

**What.** Every feature at parity on Linux: systemd user unit for the supervisor, inotify triggers, Landlock and seccomp (S8), notification backends, power and sleep handling where meaningful.

#### Z6 — Windows plan

**What.** Deferred by the owner's ordering (Life decision: Windows later). The plan records the blockers so nothing built now makes Windows harder: Unix-only pieces (gateway control socket, `flock`, `sandbox-exec`/`bwrap`) sit behind traits; path handling avoids Unix assumptions in new code.

#### Z7 — Soak and leak testing

**What.** R17's soak runs as a release gate: 24 hours before every `dev` release, 100 hours before every `main` release, on the owner's Mac, local only.

#### Z8 — Move the repository out of iCloud

**What.** The checkout lives in iCloud-synced Desktop with an 8.8 GB `target/`; an earlier `.git/index 3` conflict came from that. Moving it (owner action) removes a class of corruption risk that a 100-hour run writing logs and worktrees would make much worse.

#### Z9 — Licences and attribution

**What.** Overseer ports ideas and code from MIT-licensed projects (Synara's usage readers, Hermes' review prompts and threat patterns, cua-driver) with notices. Every workstream that ports code records the source, commit and licence in the file header and the NOTICE file; `cargo deny` checks dependency licences (have); a checklist item in every PR review confirms attribution for ported material.
