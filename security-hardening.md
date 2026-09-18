> **Status:** deep-research synthesis, committed as-is 2026-09-18. No owning reviewer yet; a rewrite needs owner + audit input (P8-D corpus-docs slice).
>
# Overseer — Security Hardening Playbook

Implementation-grade research synthesis for making overseer lean but
near-impenetrable on macOS + Linux. Sections (a)–(g) follow the requested
deliverable structure. Verified facts cite primary sources inline; anything
unverified is flagged **[UNVERIFIED]**.

Grounding in current code:
- `crates/overseer-core/src/perm.rs` — L4 gate exists (substring denylist +
  canonicalized root containment, headless fail-closed). Upgrades specified
  in §d.
- `crates/overseer-core/src/event.rs` — append-only JSONL with
  `id`/`parent_id`/`ts_ms`; `ToolResult.denied` already logged. Extensions
  in §e.
- Dep tree: 144 crates; `ureq 3.4.2` (`rustls` + `platform-verifier`) →
  `rustls 0.23.45` → `ring 0.17.14` (patched ≥0.17.12 ✓); `zeroize 1.9.0`
  already in tree. `cargo audit` clean today.
- `cargo geiger` on overseer-core: unsafe concentrated in `ring` (376/444
  fns), `security-framework` (2081), `libc`, `serde_json`, `bytes`,
  `memchr`, `httparse`, `core-foundation`, `uuid`. First-party code has
  **zero** unsafe — `forbid` is achievable per-crate, not transitively.

---

## (a) Supply-chain specification

### a.1 Tool chain and exact CI

Four gates, each with a distinct job; none is redundant:

| Tool | Job | Gate |
|---|---|---|
| `cargo audit` | RustSec advisory scan of `Cargo.lock` | any vuln = fail |
| `cargo deny` | policy: bans, licenses, sources, advisories, build/exec files | policy violation = fail |
| `cargo vet` | human-review coverage of the tree | unvetted delta = fail |
| `cargo geiger` (warn-only) | unsafe census for review triage | report only |

Pin tool versions in CI (install via `cargo install --locked` or
taiki-e/install-action with SHA-pinned actions):

```yaml
# .github/workflows/supply-chain.yml (fragment)
- run: cargo audit --deny warnings
- run: cargo deny check advisories bans licenses sources
- run: cargo vet --locked
- run: cargo geiger --compact > geiger.txt || true   # informational
```

`cargo audit` treats every unwithdrawn advisory as a failure; use
`.cargo/audit.toml` only for documented exceptions:

```toml
# .cargo/audit.toml — every ignore needs a reason + removal condition.
[advisories]
ignore = [
    # "RUSTSEC-XXXX-YYYY",  # reason; remove when <condition>
]
informational_warnings = ["unmaintained", "unsound"]
```

Current status: no ignores needed. The two `ring` unmaintained
informationals (RUSTSEC-2025-0007 withdrawn 2025-02-22, -0010) resolved —
rustls team co-maintains ring; no exception on file. RUSTSEC-2025-0009
(ring vuln) is patched at our 0.17.14.

### a.2 `deny.toml` — ship this

Modeled on Codex's real `codex-rs/deny.toml` (sources deny-unknown, git
requires `rev`, dup-version warn) plus license allowlist:

```toml
# deny.toml
[graph]
all-features = false
no-default-features = false

[advisories]
# NB (verified in current docs): `vulnerability`, `unsound`, `notice`,
# `severity-threshold` were REMOVED — those classes now always emit
# errors. Don't set them; they are hard errors if present.
db-urls = ["https://github.com/RustSec/advisory-db"]
db-path = "~/.cargo/advisory-dbs"
yanked = "deny"
unmaintained = "workspace"   # fail only for workspace-direct deps
# `unsound`: docs conflict — listed in the default block but also named
# among removed fields. Omit it and pin the version; vulns always fail.
maximum-db-staleness = "P30D"
unused-ignored-advisory = "warn"
ignore = []

[bans]
multiple-versions = "warn"      # duplicates warn — rustc/rustls force some
wildcards = "deny"              # no `*` version reqs in workspace manifests
highlight = "all"
workspace-default-features = "warn"
external-default-features = "warn"
# Executable/script files smuggled in crates — defense vs payload crates:
deny = []
wrappers = []
skip = []
skip-tree = []

[sources]
unknown-registry = "deny"
unknown-git = "deny"
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
allow-git = []
required-git-spec = "rev"       # any git dep must pin an immutable rev

[licenses]
version = 2
confidence-threshold = 0.8
allow = [
    "MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause",
    "ISC", "Zlib", "Unicode-3.0", "Unicode-DFS-2016",
    "CC0-1.0", "MPL-2.0", "BSL-1.0", "OpenSSL",
]
exceptions = []
clarify = []
private = { ignore = false, registries = [] }
```

Notes:
- `unknown-registry`/`unknown-git = deny` is the highest-leverage line: it
  prevents dependency-confusion and unverifiable sources outright.
- `required-git-spec = "rev"` makes any future git dep immutable by
  construction (Codex does this; allows specific repos via `allow-git`).
- Keep `multiple-versions = "warn"` — denying duplicates outright is not
  practical once rustls/serde-ecosystem splits appear (Codex also warns).
- `[bans].deny` specific crates only when you have a reason (Codex bans
  `reqwest` outside wrappers to force one HTTP client). For overseer:
  consider banning `openssl-sys`-dependents implicitly by license/source
  policy — already effectively covered since nothing pulls it.
- Newer cargo-deny also exposes checks for crates shipping executables and
  build-script anomalies; enable in the version you pin (fields:
  `[bans] executables`, `build-executables`, `interpreted-scripts` —
  verify names against the pinned version's docs; several were tracked in
  EmbarkStudios/cargo-deny issues #43/#763/#571).

### a.3 `cargo-vet` — ship with Mozilla import

Setup once:

```bash
cargo vet init
mkdir -p supply-chain
cat >> supply-chain/config.toml <<'EOF'
# cargo-vet: every dep must be certified safe-to-run by us or an import.
[imports.mozilla]
url = "https://raw.githubusercontent.com/mozilla/supply-chain/main/audits.toml"
criteria = "safe-to-run"
EOF
cargo vet   # resolves exemptions for whatever isn't covered
```

- `imports.lock` is generated; CI runs `cargo vet --locked` (no fetch).
- Exemptions live in `supply-chain/config.toml` `[exemptions]` with
  version + notes. Treat every exemption as a TODO: expire on next
  version bump, require PR review.
- Protect `supply-chain/` with CODEOWNERS — it is a security file.
- Import is non-transitive by design; Mozilla's aggregate is the only
  recommended import today. If you later trust another org's audits, add
  their `audits.toml` explicitly (cargo-vet docs: "Importing audits",
  "Multiple repositories").
- Use `safe-to-run` (not `safe-to-deploy`) — overseer runs deps but
  doesn't deploy them into a network service.

### a.4 Dependency update policy — cooldown against fresh-publish attacks

The xz/paectl-style attack window is "malicious release younger than N
days". Three mechanisms, use all that apply:

1. **Cargo itself (preferred, now stable):** RFC 3923 *merged 2026-05-18*;
   stabilization PR rust-lang/cargo#17335 is milestone **1.100.0**.
   Config keys per the RFC: `registry.global-min-publish-age` and
   `resolver.incompatible-publish-age`. When the toolchain is ≥1.100, add
   to `.cargo/config.toml`:
   ```toml
   [registry]
   global-min-publish-age = "7 days"
   ```
   Semantics: the resolver skips versions younger than the age unless
   already in `Cargo.lock`. Local toolchain here is cargo 1.98.1 → adopt
   at the pinned dist toolchain (§b pins `rust-toolchain-version`).
   **[Verify exact key names against the 1.100 docs at implementation —
   PR #17353 removed the earlier `registry.min-publish-age` name during
   stabilization.]**
2. **Dependabot `cooldown`** (native, zero-infra, works on private repo):
   ```yaml
   # .github/dependabot.yml
   version: 2
   updates:
     - package-ecosystem: "cargo"
       directory: "/"
       schedule: { interval: "weekly" }
       cooldown:
         default-days: 7
         semver-major-days: 14
         include: ["*"]
         exclude: []        # e.g. allow same-day for a hot fix crate
     - package-ecosystem: "github-actions"
       directory: "/"
       schedule: { interval: "weekly" }
       cooldown: { default-days: 7 }
   ```
3. **Renovate `minimumReleaseAge`** — stronger configurability; crates.io
   datasource supplies timestamps from index `pubtime`/API `created_at`
   (Renovate 42 requires a timestamp to apply the age). For this *private*
   repo, hosted Renovate isn't free → self-host cost makes Dependabot the
   lean default. Switch if cooldown granularity is ever insufficient.
   cargo-deny itself has **no** cooldown feature; don't look for it there.

Lockfile hygiene regardless of tool: `Cargo.lock` committed; CI uses
`--locked`; every dep-bump PR is reviewed like code (cargo-vet exemptions
delta is the review surface). Emergency advisory bypass: a maintainer may
land an under-age bump by explicit comment + `cargo update -p`, which
records it in the lockfile — the exception path is the lockfile, not a
flag.

### a.5 SBOM + crate provenance

- Generate CycloneDX per release via cargo-dist's `cargo-cyclonedx`
  integration (§b config) — covers both "SBOM exists" and "provenance of
  the artifact". `cargo auditable` embeds dep metadata into the binary
  itself (RustSec/consumers can recover the exact tree from the artifact).
  Also emerging: nightly cargo `-Z sbom` writes SBOM precursor files;
  watch, don't depend on yet.
- **Crates.io trusted publishing** (RFC 3691, shipped) is *publish-side*:
  OIDC from GitHub Actions mints a short-lived publish token, verified
  against repo/workflow/environment claims. It hardens *our* future
  `cargo publish` (use `rust-lang/crates-io-auth-action@v1`,
  `id-token: write`, environment protection) — it does **not** give
  consumers signature verification of downloaded crates.
- **[UNVERIFIED]** Claims that crates.io adopted Sigstore signing of all
  packages and that Cargo verifies bundles by default appeared in
  secondary sources only; no primary confirmation found. Treat consumer-
  side crate signature verification as **not shipped**; the integrity
  chain today is `Cargo.lock` checksums + sparse-index TLS + your vet/
  audit gates. Provenance of *what you download* is still "registry
  served this tarball matching this sha256".

### a.6 Vendoring decision

Do **not** vendor by default — repo bloat, review friction, and it does
not by itself verify provenance (a vendored tree is only as good as the
lockfile it came from). Vendor only if one of these becomes true:
- release must build fully offline/reproducibly from the repo alone;
- a dep must be patched locally;
- registry availability becomes a release blocker.
If vendored: `cargo vendor --sync` + `.cargo/config.toml` source
replacement, and the vendor dir gets CODEOWNERS + a CI job proving
`cargo build --locked` uses it.

### a.7 Dependency discipline (lean mandate)

- Hard budget: ≤160 crates in `Cargo.lock` (now 144), binary ≤8 MiB
  stripped release. Gate in CI:
  `cargo bloat --release --crates | head -20`, plus a dep-count check
  (`cargo tree --prefix none | sort -u | wc -l`).
- Every new dep must justify against binary size, RSS, startup, and
  maintenance burden. Reject features you don't use:
  `default-features = false` (already done for ureq).
- Duplicates are a smell, not a crime — the `[bans]` warn makes them
  visible in every PR.
- `cargo geiger` stays informational: 158 warnings today, concentrated in
  crypto/FFI/SIMD foundations (ring, security-framework, libc, memchr,
  bytes, httparse, serde_json). That is the *expected* shape — flag only
  new unsafe in unexpected places (e.g., a JSON lib or a parser gaining
  unsafe is a review trigger).

### a.8 cargo-crev — optional signal, never a gate

cargo-crev is a distributed cryptographic review web (proof repos, trust
graph, `cargo crev verify`). Still self-described WIP; trust roots are
social. Use it as an *advisory* signal when triaging a new dep
(`cargo crev crate info <name>`), never as a CI gate and never as a
release blocker — too few auditors, no SLA, and a wrong trust root is
worse than none.

---

## (b) Release-hardening specification

### b.1 cargo-dist — `dist-workspace.toml`

```toml
[workspace]
members = ["cargo:."]

[dist]
cargo-dist-version = "0.32.0"      # pin; bump deliberately
ci = "github"
installers = ["shell", "powershell"]
targets = [
  "aarch64-apple-darwin",
  "x86_64-apple-darwin",
  "x86_64-unknown-linux-gnu",
  "aarch64-unknown-linux-gnu",
  "x86_64-unknown-linux-musl",     # optional fully-static variant
  "aarch64-unknown-linux-musl",
]
rust-toolchain-version = "1.100.0" # ≥1.100 → enables min-publish-age (a.4)
precise-builds = true
cargo-auditable = true             # embed dep metadata in binaries
cargo-cyclonedx = true             # CycloneDX SBOM per target
omnibor = true                     # OmniBOR artifact IDs
github-attestations = true         # Sigstore build provenance via GH
github-attestations-phase = "host"
github-attestations-filters = ["--tag", "--source-sha"]
min-glibc-version = { "*" = "2.31" }  # tune to support floor; uv does this
strip = true
pr-run-mode = "plan"               # PRs run dist plan only; releases cut tags
```

### b.2 Cargo profile (in root `Cargo.toml`)

```toml
[profile.dist]
inherits = "release"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
opt-level = 3            # keep 3; use "z" only if size gate demands it
overflow-checks = true   # cheap integrity net for parsing/accounting math
debug = false
```

Rationale: `panic = "abort"` removes the unwind path and shrinks the
binary; `overflow-checks` in a non-optimizing-profile is a few % cost and
converts silent wraparound in parsing/budget accounting into a crash —
worth it for a harness that processes untrusted JSON. If profiling shows
hot-path cost, scope it via `[profile.dist.package.overseer-core]`-style
per-package override instead of disabling globally.

### b.3 macOS signing + notarization

- Certificate: **Developer ID Application** (not Mac App Store, not
  ad-hoc). Manage in CI via a temporary keychain from a notarized-p12
  secret; never ship the p12 in the repo.
- Sign each Mach-O: `codesign --force --timestamp --options runtime \
  --sign "Developer ID Application: …" overseer`.
  `--options runtime` enables the **hardened runtime**, required for
  notarization.
- **Entitlements: none.** A pure CLI needs no hardened-runtime
  exceptions. Explicitly do **not** grant:
  - `com.apple.security.get-task-allow` (debug-attach; dev-cert only,
    would fail notarization intent anyway),
  - `com.apple.security.cs.disable-library-validation` (only needed if
    loading third-party dylibs — overseer doesn't),
  - `allow-jit` / `allow-unsigned-executable-memory` (Codex ships these
    because its runtime needs them — we don't; don't copy blindly).
  Ship an empty entitlements plist or omit `--entitlements` entirely.
- Notarize with **`notarytool`** (altool is retired):
  `xcrun notarytool submit overseer-macos-aarch64.zip \
    --keychain-profile "overseer-notary" --wait`.
  Store the profile via `xcrun notarytool store-credentials` using App
  Store Connect API key creds kept in CI secrets.
- Stapling: `xcrun stapler staple` works on `.pkg`/`.app`/`.dmg` — **not**
  on a bare Mach-O. For a CLI distributed as a tarball/zip, notarize the
  archive that contains the signed binary; the ticket lives on Apple's
  servers and Gatekeeper validates online on first run. If you ship a
  `.pkg` installer, staple that.
- Verify in CI before publish:
  `codesign --verify --strict --verbose=4`,
  `codesign -dvvv` (authority + timestamp + runtime flag),
  `spctl -a -t execute -vv overseer` on the signed binary.

### b.4 Linux hardening — what rustc gives vs. what to check

Per the rustc exploit-mitigations doc, on x86_64/aarch64 GNU targets the
defaults already give you: PIE, NX stack/heap, read-only relocations +
immediate binding (full RELRO), stack-clash protection, and position-
independent code. What is **not** on by default in stable Rust: stack-
smashing canaries (`-Z stack-protector` remains nightly/target-gated —
skip it, don't fight the toolchain) and `panic=immediate-abort`.

So the Linux policy is *verify, don't twiddle*: in release CI run
`readelf -h` (ET_DYN), `readelf -lW | grep GNU_STACK` (RW, not RWE),
`readelf -dW | grep -E 'BIND_NOW|FLAGS'`, and `file` on each artifact;
fail if any check regresses. `checksec` if available. Do not inject
`-C link-arg` hardening flags blindly — the musl and older-glibc targets
differ and cargo-dist already produces sane flags; inspection is the gate.

musl targets: ship them as the "static" flavor for portability, but keep
glibc builds as primary — musl's resolver/alloc behavior differs and most
users' distros have a compatible glibc.

### b.5 GitHub Actions release pipeline

- Pin every third-party action by **full SHA** (uv's dist-workspace.toml
  does this); Dependabot covers `github-actions` ecosystem for bumps.
- Workflow permissions: `permissions: {}` at top, grant per-job only:
  `contents: write` (release), `id-token: write` (attestation + future
  trusted publishing), `attestations: write`. No `pull-requests: write`.
- Tag-triggered release only (`v*`); use a GitHub **environment** with
  required reviewers for the publish job. NOTE: this is a private repo on
  the free tier — branch/tag protection rules may be limited; the AGENTS.md
  ladder (review→dev→main) is enforced socially. Compensate: release job
  requires `github.ref` to be a tag AND the env approval, so a random push
  can't mint signed artifacts.
- GitHub Artifact Attestations (via cargo-dist `github-attestations`):
  produces Sigstore-signed build provenance consumers can check with
  `gh attestation verify overseer -o sirrayi`. This is your tamper-
  evidence for releases.

### b.6 SLSA — honest claim

GitHub-hosted builds + OIDC attestations + pinned actions ≈ **SLSA
Build L2** territory (hosted build service, signed provenance). Do **not**
claim L3 — that requires hardened/ephemeral builder isolation guarantees
GitHub-hosted runners don't formally provide. Say: "signed provenance
verifiable via `gh attestation verify`"; never "SLSA 3". Source:
slsa.dev spec + cargo-dist attestation docs.

### b.7 Reproducible builds

Rust is *favorable* to reproducibility (no timestamps/absolute paths in
output by default, deterministic crate graph given the lockfile), and
`precise-builds` pins the exact plan — but **bit-for-bit reproducibility
is a property you measure, not assume**. Process: build twice from the
same tag on clean runners, `sha256` compare; publish the checksums in the
release notes either way. Keep provenance (attestation) and
reproducibility (rebuild-and-compare) as separate claims — the first is
solid today, the second is a measured bonus.

---

## (c) Runtime-hardening specification

### c.1 Secret material

- `zeroize` (already in tree via rustls): `Zeroizing<String>` /
  `SecretString` for API keys and any provider credential in memory;
  `#[derive(ZeroizeOnDrop)]` on structs holding them.
- `secrecy` — **optional**. zeroize already covers wipe-on-drop; secrecy
  adds a `Debug`-redacting wrapper. Take secrecy only if you find secrets
  leaking into logs/errors — otherwise the extra dep isn't justified
  under the lean mandate. Decision: **zeroize only**, and enforce
  "no secret in `Debug`/`Display`" by code review + a test that formats
  the credential store.
- Never write secrets to `events.jsonl` (§e redaction), never put the
  API key in argv/env of spawned tools (env is inherited by children —
  pass via a non-inherited channel or explicitly strip before spawn).
- `mlock`/`VirtualLock` for the key buffer: best-effort hardening only —
  `RLIMIT_MEMLOCK` is typically 8 MiB-locked-limited and the call can
  legitimately fail; treat failure as non-fatal. Skip entirely if the
  credential lifetime is "read once, send once" — the wipe-on-drop is the
  real guarantee.

### c.2 Process hygiene at startup (the Codex pattern)

Implement a small `harden.rs` run as the *first* statement of `main`,
mirroring `codex-rs/process-hardening/src/lib.rs` (verified source):

- Linux: `prctl(PR_SET_DUMPABLE, 0)` + `setrlimit(RLIMIT_CORE, 0)` +
  remove all `LD_*` env vars (loader injection surface).
- macOS: `setrlimit(RLIMIT_CORE, 0)` + remove all `DYLD_*` env vars
  (dyld interpose/injection surface) + **optional** `ptrace(PT_DENY_ATTACH)`.
- Rationale: core dumps write API keys and conversation memory to disk;
  `*_LIBRARY_PATH`/`DYLD_INSERT_LIBRARIES` are trivial injection vectors
  when overseer spawns subprocesses that inherit env. Removing them early
  also protects children.
- `PT_DENY_ATTACH`: make it a `--debug-block` opt-in or release-only
  toggle, **not** default — this is a developer tool; blocking debuggers
  by default breaks legitimate `lldb` work and is only cosmetic against a
  root-capable attacker anyway.
- These calls need `unsafe` (libc FFI). Confine to this one module:
  `#![deny(unsafe_code)]` crate-wide + `#[allow(unsafe_code)]` on the
  hardening module only — you get "no unsafe anywhere except the audited
  FFI shim", which is honest, unlike claiming the whole dep tree is
  unsafe-free (it isn't: ring/security-framework).

### c.3 Integer overflow & panic policy

- `overflow-checks = true` in `[profile.dist]` (b.2). Security-sensitive
  arithmetic — byte counters, budget accounting, truncation offsets,
  depth/length tracking — additionally uses `checked_*`/`saturating_*`
  explicitly so correctness doesn't depend on a profile flag.
- `panic = "abort"`: acceptable because overseer is a short-lived process
  whose crash is a clean failure; ensure the event log `flush()`es before
  any path that can panic (the TurnEnd fsync discipline already exists).

### c.4 Untrusted-input bounds (provider JSON & tool input)

- Response body byte cap **before** parsing: e.g. 16 MiB streamed limit
  (ureq `BodyReader` bounded) — prevents memory-bomb JSON.
- Keep `serde_json`'s default recursion limit (128) — do **not** enable
  the `unbounded_depth` feature; nested-structure DoS is real.
- Application-level caps on top of the parser: max object fields per
  object, max string length, max array length, max tool-input size —
  enforce in schema validation, not just byte size.
- Tool outputs are already budgeted (~30K chars inline → spill file) —
  keep that invariant; add a hard cap on total bytes *read from disk*
  into context per turn.

### c.5 Command execution limits

- Wall-clock timeout per bash call (default e.g. 600s, configurable) —
  kill process group on expiry.
- Output byte cap with truncation + spill (mirrors tool-result budgeting).
- Process-count / fork guard: resource limit (`RLIMIT_NPROC`) on the
  spawned process group where the OS allows; fork-bomb patterns are also
  denylisted (§d).
- `RLIMIT_FSIZE` cap so a runaway write can't fill the disk silently.

### c.6 Fuzz targets (`cargo fuzz`, libFuzzer)

Prioritize parsers that touch attacker-influenced bytes:
1. **Event-log replayer** — `Event::deserialize` over arbitrary JSONL
   lines; must never panic, must tolerate torn tail lines.
2. **Tool-input parser** — JSON tool_use blocks → typed args.
3. **Permission pattern parser** — glob/rule parsing (§d) — the highest
   value target: a panic or wrong-match here is a bypass.
4. **Bash classifier** — the command splitter/normalizer; seed corpus
   must include the evasion battery in §d.4.
5. **Provider-response parser** — SSE stream + message JSON.
6. **Path normalization** — canonicalize/containment logic vs `..`,
   symlinks, unicode tricks.
Seed corpora from real sessions + the evasion list; run `cargo fuzz` in
CI on a schedule (nightly), not per-PR.

---

## (d) Permission-engine specification

Replaces the current `perm.rs` substring denylist. Design borrows:
**OpenCode** for the rule algebra (action/resource/effect, last-match,
multi-resource fold), **Codex** for shell parsing and decision lattice,
**Claude Code** for layer precedence + managed policy + mode semantics.

### d.1 Typed action/resource algebra

```rust
enum Action { Read, Edit, Bash, Glob, Grep, WebFetch, WebSearch,
              Subagent, Skill, ExternalDirectory, Mcp(String), Execute }
struct Rule { action: Action, resource: Glob, effect: Effect }
enum Effect { Allow, Ask, Deny }
```

Resource strings are normalized before matching:
- paths → canonical absolute (or location-relative), `~`/`$HOME`
  expanded at config load (OpenCode does exactly this);
- bash → the classifier's normalized command string(s), **not** raw text;
- `external_directory` is a distinct action: any read/edit outside the
  working root must first pass `external_directory` on the canonical dir
  boundary — this is how you keep a narrow root without hardcoding "~".

Match semantics (OpenCode): `*` = any chars incl. `/`, `?` = one char,
else literal; whole-value match; a shell pattern ending in `" *"` also
matches the bare command (`"git status *"` ⇔ `git status` + args).

### d.2 Verdict composition — pick ONE semantics and document it

**Within a layer:** last matching rule wins (OpenCode). Users write a
broad rule then append exceptions; order encodes specificity.

**Across layers — strict precedence, most restrictive wins:**

```
managed  >  user  >  session  >  project  >  builtin-default
```

Each layer evaluates to a verdict; effective verdict = the **most
restrictive** non-`Allow` verdict found scanning managed→project, with
this rule: **a Deny or Ask from a higher layer can never be widened by a
lower layer.** Concretely:

1. Collect each layer's last-match verdict (NoMatch if nothing hits).
2. Any managed `Deny` → final `Deny` (hard floor — Claude managed policy).
3. Any `Deny` at any layer → `Deny`.
4. Any `Ask` (and no `Deny`) → `Ask`.
5. Else any `Allow` → `Allow`.
6. All NoMatch → mode default: `Ask` interactive, `Deny` headless.

Why this over pure last-match: Codex and Claude both resolve to
most-restrictive across the *decision* lattice while using order for
*pattern* specificity — it composes safely (a managed deny is absolute)
while ordered rules keep ergonomics. Document it; ambiguity here is a
vuln class.

**Multi-resource ops** (patch touching N files, compound bash): fold per
resource — any `Deny` → `Deny`; else any `Ask` → `Ask`; else `Allow`.
(OpenCode's exact rule; Codex does the same on split commands — the
strictest constituent wins.)

**Untrusted project config:** if the workspace is not yet trusted, ignore
`Allow`/`Ask` in the project layer — only `Deny` from project files
applies (a checked-in repo file must never widen authority; Claude's
workspace-trust model). `external_directory` grants follow the same rule.

### d.3 Modes / presets (mirror Codex `sandbox_mode` + Claude modes)

- `read-only` — read/grep/glob allow; edit/bash deny-by-default.
- `workspace-write` (default) — edits inside root; bash gated by rules;
  network off unless enabled.
- `danger-full-access` — eval/bench mode; current `Policy::allow_all`.
- `plan` — read-only + no side-effects at all.
- Headless/CI: **no match → Deny** (already correct in perm.rs); `dontAsk`
  semantics = every `Ask` becomes `Deny` with a structured reason the
  model can act on. Never silently widen.

Network is a separate axis (OpenCode webfetch/websearch actions; Codex
`network_access` flag + domain policy): a bash rule must not be the
network boundary — enforce egress in the sandbox layer (§f), and give
`webfetch` its own URL allowlist rules.

### d.4 Bash classification — parse, don't substring-match

The current `cmd.contains("rm -rf /")` approach is a first wall only.
Replace with a two-stage classifier modeled on Codex's
`shell-command` crate (verified source):

**Stage 1 — argv normalization & wrapper stripping.** Tokenize argv;
resolve the executable to canonical basename + realpath (so `/bin/rm`,
`./rm`, a `rm` on PATH all match the `rm` rule). Strip known wrappers
recursively, depth cap 8 (Codex's value): `env`, `sudo`(+flags),
`timeout`, `nice`, `nohup`, `stdbuf`, `command`, `builtin`, `watch`,
`xargs`(with parsed `-I` behavior), `find -exec`/`-delete`, and shell
entrypoints `sh|bash|zsh|dash -c|-lc` (incl. absolute paths).

**Stage 2 — script splitting (conservative AST).** For a `-c` script,
split on top-level `&&`, `||`, `;`, `|` into constituent commands — but
**only** when every constituent is literal words. **Fail closed → treat
the whole invocation as one un-parseable command** when the script
contains: `$( )`/backticks command substitution, `<( )`/`>( )` process
substitution, redirections `> >> < <<`, variables/assignments `X=...`,
globs in positions that matter, `eval`, `exec`, `source`/`.`, control
flow (`if`/`for`/`while`/`case`/`until`, `&`, `||` inside subshells),
functions, heredocs, or parse errors. Codex's exact policy: unsupported
syntax → evaluate the *entire shell invocation* as a single command
(conservative, never splits something it can't fully model).

Each constituent command then runs through the rule matcher; strictest
result wins (d.2 multi-resource fold). Approval keys on the **normalized
command**, so `bash -c 'rm -rf /'` and `rm -rf /` hit the same rule —
kills the classic `bash -c` evasion that beats prefix matching.

**Deny-on-match battery (keep from perm.rs, expand):** forced recursive
delete (`rm -rf|-fr|--force` forms), writes outside root, writes to
`~/.ssh`, shell RC files (`~/.bashrc`/`zshrc`/`profile`), `~/.config`,
`$PATH` dirs, `/etc`, credential dirs; `curl|wget … |sh|bash|zsh`;
`git push --force|-f` and `--force-with-lease` on protected branches;
`git push --delete`; `chmod 777`; `sudo`; `ssh/scp/rsync` to non-
allowlisted hosts; package-manager script installs (`apt|brew install`
pipe forms); `docker`/mount/`nsenter`/`unshare`/firewall/`launchctl`/
`systemctl`; `kill`/`pkill` on non-child PIDs; `mkfs`, `dd of=/dev`,
`:(){:|:&};:` fork bomb; `--no-preserve-root`; `git config --global`.

**Explicit "unknown → Ask/Deny" path:** anything unparseable or not
covered is `Ask` (interactive) / `Deny` (headless) — never `Allow`.
Command-length cap ~10k chars before analysis → too long = `Ask`
(Claude's bound; bounded analysis is a feature).

**Evasion test battery (fuzz corpus + unit tests):** `bash -c`,
`sh -c 'x; rm -rf /'`, `bash -lc`, absolute paths `/bin/rm -rf /`,
`env rm -rf`, `sudo -S …`, `xargs rm`, `find . -delete`,
`eval "$(…)"`, `$(rm -rf /)`, backticks, `rm -rf\ /` quoting tricks,
newline injection, unicode/homoglyph args, `cd / && rm -rf .`,
`ln -s` + traversal, `dd`, `tee` to protected paths, `git -C . push -f`,
`git -c … push --force`.

**Standing caveat (say it in the docs):** text rules on Bash are *not* a
security boundary — `Bash(curl *)` doesn't stop `/usr/bin/curl` run
through a novel wrapper, and no rule set enumerates all dangerous
commands. Claude's docs admit this; OpenCode's own docs say prefer a
narrow allowlist over enumerating danger. The sandbox layer (§f) is the
real boundary; the classifier exists to *route* and to catch the obvious
footguns deterministically.

### d.5 Approval grants & UX

- Grant scope: exact normalized invocation, or an explicit bounded prefix
  rule — **never** "approve bash" broadly. Session-scoped by default;
  persistent only via writing a rule to user config (an explicit action,
  not a checkbox side-effect).
- Approval prompt shows: normalized command (post-wrapper-strip),
  working dir, affected paths/domains, the rule and layer that fired,
  the risk reason, and the exact scope+duration of the grant. Never show
  secret env values.
- Headless: `Ask` → structured `Deny` + reason to the model + a
  `PermissionDecision` event; nonzero exit if the turn can't proceed.
- `dontAsk`/managed mode: auto-deny anything that would prompt —
  fail closed, always.

---

## (e) Audit-log specification

Extend the existing `events.jsonl` `EventKind` (it's already the single
source of truth — keep one stream, don't fork a second log):

```rust
PermissionDecision {
    call_id: String,           // ties to ToolCallStart/ToolResult
    tool: String,
    input_hash: String,        // sha256 of canonical JSON — not raw input
    display: String,           // redacted, capped rendering
    cwd: String,
    resources: Vec<String>,    // canonical paths / domains / commands
    rule_id: Option<String>,   // which rule fired, if any
    layer: String,             // managed|user|session|project|default
    verdict: String,           // allow|ask|deny|approve|deny-user|fail|timeout
    risk_reason: Option<String>,
    mode: String,              // permission mode in force
    network: Option<NetTarget>,// host/port/domain for egress decisions
}
PermissionGrant { rule: Rule, scope: String, duration: String, source: String }
PolicyLoad { path: String, layer: String, rules: u32, trusted: bool }
```

Plus fields on existing events: `ToolResult` already has `denied` — add
`exit_status`, `signal`, `out_bytes`, `truncated`, `duration_ms`.

Rules:
- **Redaction is structural, not best-effort:** secrets never enter the
  event — log `input_hash` + a `display` built from the normalized,
  scrubbed form (env values stripped, auth headers removed). If a field
  can't be safely rendered, log its hash + length only.
- **Integrity:** add `prev_hash` per event (sha256 of the prior line) —
  a one-line hash chain gives tamper-evidence for near-zero cost. Keep
  `id`/`parent_id` semantics; hash chain runs alongside.
- **Append-only & crash-tolerant:** append-only fd, `flush()`+fsync at
  `TurnEnd` (already the durable-tail point); the replayer must tolerate
  a torn last line (a crash mid-write leaves a partial tail — skip it,
  don't fail the session). Fuzz target (c.6) covers this.
- **Retention:** cap inline content (Claude uses 60KB; we use the 30K-
  char inline→spill budget already) and never retain full raw provider
  responses by default.
- **SIEM path (later, optional):** map `PermissionDecision`→Claude's
  `tool_decision` event shape if OTLP export is ever added — but the
  local JSONL is sufficient for now; don't pull in an OTel dep stack.
- Denied actions, permission grants, and egress denials are **always**
  logged — they're the forensic trail for "what did the agent try".

---

## (f) Platform protections & what not to build

### macOS
- **TCC-protected dirs** (`~/Desktop`, `~/Documents`, `~/Downloads`):
  reads/writes there are governed by macOS *and* our rules — gate them
  behind explicit `external_directory` grants in policy; don't rely on
  TCC alone and don't pre-authorize them in the default profile.
- No entitlements (b.3); hardened runtime on.

### Linux
- Optionally add a seccomp-baseline for spawned commands later
  (Anthropic's srt blocks AF_UNIX via BPF; Codex uses Landlock + a
  bubblewrap-esque mount policy). For lean v1: rely on permission rules +
  process limits; sandboxing is the documented next layer, not yet code.

### Cross-cutting
- **Refuse root:** exit unless `--allow-root` is passed. Running the
  harness as root makes every guard moot.
- **No self-modification:** never write to the install/binary dir; the
  update path is "download new signed artifact", never in-place rewrite.
- **MCP (when/if added):** server identity pinned (name+hash/source),
  per-tool permission rules (`<server>_<tool>` action, OpenCode model),
  tool descriptions treated as untrusted prompt text, connect events in
  the audit log. Do not ship MCP without the trust model.

### What NOT to build (lean mandate)
- A full shell interpreter for classification — the conservative literal-
  splitter + fail-closed unknown covers it.
- A custom kernel sandbox / custom crypto / custom provenance scheme —
  seatbelt/landlock exist; GitHub attestations + Apple notarization exist.
- Mandatory `mlock`, aggressive anti-debug as default — costs real DX,
  stops nobody with root.
- cargo-crev as a gate; OTel stack for local audit; heavy telemetry deps.
- Raw-prefix shell rules as the only boundary; full prompt/response
  capture in the log; running as root; unverified self-update.

---

## (g) Sources

Primary (verified during research):
- cargo-deny config/checks: embarkstudios.github.io/cargo-deny/checks/{cfg,advisories,bans,sources,licenses}.html ; github.com/EmbarkStudios/cargo-deny
- cargo-vet: mozilla.github.io/cargo-vet/{how-it-works,importing-audits,multiple-repositories}.html ; github.com/mozilla/supply-chain (audits.toml import)
- cargo-crev: github.com/crev-dev/cargo-crev ; crates.io/crates/cargo-crev (WIP status)
- crates.io trusted publishing: crates.io/docs/trusted-publishing ; rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html ; tracking rust-lang/crates.io#10247, #12361
- Cargo min-publish-age: RFC text rust-lang/rfcs@master/text/3923-…; RFC PR #3923 (merged 2026-05-18); stabilization rust-lang/cargo#17335 (milestone 1.100.0); rename fix #17353; enforcement gap #17246; unstable `-Z sbom`, `-Z lockfile-publish-time` in cargo unstable docs
- Dependabot cooldown: docs.github.com/…/dependabot-options-reference (`cooldown` key)
- Renovate minimumReleaseAge: docs.renovatebot.com/key-concepts/minimum-release-age ; crates.io timestamp support (pubtime/created_at)
- Codex: developers.openai.com/codex/{config-reference,rules,security} ; raw.githubusercontent.com/openai/codex/main/codex-rs/{deny.toml, .cargo/audit.toml, execpolicy/README.md, shell-command/src/{bash.rs,command_safety/is_dangerous_command.rs}, process-hardening/src/lib.rs} ; .github/scripts/macos-signing/codex.entitlements.plist
- Claude Code: code.claude.com/docs/en/{permissions,monitoring-usage,sandboxing,managed-settings,settings}
- OpenCode v2: opencode.ai/v2/docs/permissions ; opencode.ai/docs/permissions (v1)
- cargo-dist: axodotdev.github.io/cargo-dist/book/ + reference/config.html ; attestation PR axodotdev/cargo-dist#1012
- uv release config: raw.githubusercontent.com/astral-sh/uv/main/{Cargo.toml,dist-workspace.toml}
- rustc mitigations: dev-doc.rust-lang.org/rustc/exploit-mitigations.html ; codegen options + cargo profiles docs
- serde_json recursion: docs.rs/serde_json (Deserializer, `disable_recursion_limit`, default 128)
- Apple notarization: developer.apple.com/documentation/security/notarizing-macos-software-before-distribution (+ customizing workflow)
- Process hardening: github.com/niluxv/secmem-proc ; CERT MEM06-C
- Sandbox references (saved in /Users/chf/harness-research/sources/): codex-seatbelt_*.sbpl, codex-linux-{landlock,bwrap}.rs, nono-macos.rs, srt-seccomp.ts

Flagged unverified: crates.io consumer-side Sigstore verification "shipped
by default" claims — no primary source; treat as not shipped.
