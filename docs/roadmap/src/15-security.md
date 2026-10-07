# Part III — Supporting Systems

## 13. Security and Trust

### 13.1 The bar

The "lethal trifecta" (private data, untrusted content, an exfiltration channel) is a session property, and a taint bit is the highest-leverage security feature **[src: playbook thesis 8]**. Agentic browsers were broken repeatedly in 2026 through prompt injection (Trail of Bits on Comet, Zenity's zero-click hijacks, the University of Washington same-origin study) **[src: Life plan §1.5]**. Credentials must never enter context; consequential actions need approval unless the owner deliberately decides otherwise.

Overseer's security posture is already unusual for a harness: Rule-of-Two latches, sandboxed bash with network off, credential broker sentinels, a hardened localhost UI, an injection corpus in the tests, and taint that persists across resume. Long runs and automation widen the attack surface in time and in reach, so the posture must widen with them.

### 13.2 Workstreams

#### S1 — The egress proxy

**What.** The open item since v1: a loopback proxy with domain allowlists, so commands and tools can reach the network selectively instead of not at all.

**Design.** A small proxy thread in the run process (no new runtime) bound to loopback with a per-run token; the sandbox allows connections only to it; it enforces per-run domain policy compiled by P2 (allow, ask, deny per domain and method), logs every request as an event (host, method, bytes, verdict), and blocks known exfiltration patterns (large uploads to unknown hosts under the untrusted latch). Commands needing the network (package installs, `git fetch`) get it through policy rather than `--no-sandbox`. The `web` tool (T10) and future connectors route through the same proxy.

**Platforms differ, so S1 lands in two parts.**

- **S1a, macOS (W4).** Seatbelt profiles can allow outbound connections to one loopback port while denying everything else, so "only the proxy" is directly expressible.
- **S1b, Linux (W10, with Z5).** `bwrap` network isolation is all or nothing: `--unshare-net` gives the sandbox an empty network namespace with no route to a proxy outside it. Reaching the proxy needs extra machinery, chosen in S1b's decision record from: a user-mode network stack for the namespace (`pasta` or `slirp4netns`) restricted to the proxy, or a Unix socket bind-mounted into the sandbox with a minimal relay inside it. Until S1b lands, Linux keeps today's deny-all egress; nothing silently opens.

::: gate
On macOS (S1a): sandboxed `curl` to an allowed domain succeeds and is logged; to a denied domain fails with a clear message; under an armed untrusted latch, a POST to a new domain asks; DNS rebinding and direct-IP bypass attempts fail.
:::

#### S2 — Finish the credential broker

**What.** Consent grants are recorded but rate and window enforcement is not wired (`cred.rs`).

**Design.** Enforce per-credential rate limits and time windows at the broker; scoped capability tokens per run; brokered secrets never in child environments (invariant 25); a leak test that scans every event, spill, artifact, journal and snapshot for sentinel-resolved values.

#### S3 — A threat model for long runs and automation

**What.** Extend the Life security design's threat model (`docs/research/2026-10-05-life-security.md`) to the harness's new shape.

| New threat | Mitigation |
|---|---|
| Injection read at hour 60 steers the rest of a long run | Latches persist (have) and propagate through artifacts, blackboard and workflow steps (invariant 19); goal audits (O9) |
| A tainted subagent poisons shared artifacts | Artifact taint (O3); consumers' latches arm |
| A webhook or channel message triggers harmful automation | Signatures, untrusted floor, approval before external effects downstream (A6) |
| Auto-resume replays a harmful or duplicate effect | Effect journal (R7) |
| A malicious repository's AGENTS.md, MCP file, hooks or skills | Hash-pinned trust (P1, T5, P10) |
| The supervisor or daemon is abused as a confused deputy | The daemon stays thin and credential-free; its control socket is owner-only with request deadlines (have); every spawn carries the originating trust level |
| Full automation removes approvals | Owner's decision; shadow-verdict journal (S4), sensitive-surface permissions (P6), kill switch |
| Persistent agents accumulate excess privilege | Per-agent permission profiles, grant expiry, periodic review prompts in the digest |

#### S4 — The full-automation audit trail

**What.** The owner chose a full-automation mode that never asks. The record must make that safe to live with.

**Design.** Every call that would have asked under the per-action mode is recorded with a `ShadowVerdict { would_have: ask, rule, class, taint }`; a daily digest section summarizes them by class; `overseer audit --mode full-auto --since …` lists them; any external, money or identity effect performed under full automation is highlighted. Nothing is blocked by this; it only makes the decisions visible after the fact.

#### S5 — Signed builds and provenance

**What.** Signed, notarized macOS builds; reproducible release builds; an SBOM; release provenance attestation. Waits on the Apple Developer account (owner action) for signing; reproducibility and the SBOM can start now.

#### S6 — The injection corpus, extended

**What.** `tests/injection_asr.rs` gates the latches today. Extend it with content types the new pillars introduce: AGENTS.md payloads, artifact and blackboard payloads, workflow trigger payloads, MCP tool descriptions, memory recall content, web pages through the `web` tool, screen text from computer use. Track attack success rate per category; any regression fails the gate.

#### S7 — Secrets hygiene

**What.** Scan session logs, spills and archives for secrets that slipped in through owner input (the 2026-09-20 pasted key is the motivating case), redact on request, and remind the owner to rotate. The provider key rotation itself is an owner action already on the to-do list.

#### S8 — Sandbox hardening

**What.** Linux: add Landlock and seccomp filters alongside `bwrap` where available; gVisor as a pinned runtime option (exists as a runtime name). macOS: keep `sandbox-exec` profiles minimal and tested; track Apple's direction on the deprecated profile language and plan for an Endpoint Security or container-based alternative if it is removed.

#### S9 — A standing red-team programme

**What.** The fix2 cycle proved the value: adversarial review found real bugs in every area. Make it periodic: at every wave close, an independent adversarial review of the wave's delta (locally, honouring the cloud posture), findings tracked to closure with the same rigour as fix2.

#### S10 — The kill switch everywhere

**What.** Invariant 24. A single owner action stops every run and workflow: TUI key, web button, `overseer stop --all`, a Telegram command, and a `STOP` file the daemon watches (exists for the daemon). Latency target: every agent at its next boundary within 2 seconds; every child process killed within 5.

::: gate
From each frontend, the kill switch stops five concurrent runs and two workflow instances within the target latency, verified by a test harness.
:::

#### S11 — Data at rest, retention and backup

**What.** Session logs, journals, artifacts, spills and memory stores hold the owner's code, conversations and sometimes personal data. Today they are plaintext under owner-only permissions in `~/.overseer`. A week-long run multiplies how much accumulates.

**Design.**

- **Retention.** Owner-configured retention per data class (sessions, spills, artifacts, workflow instances), applied by `overseer gc` (R11): compress, archive, then delete only on the owner's explicit command or retention rule. Memory stores are excluded; they have their own lifecycle (M3).
- **Encryption at rest (optional).** Archived segments and backups encrypted with a key in the macOS Keychain (Linux: the secret service), matching the Life security design's approach. Live logs stay plaintext for performance and debuggability unless the owner opts in.
- **Backup and restore.** `overseer backup` writes an encrypted bundle of memory stores, policy, agents, workflows and selected sessions; `overseer restore` verifies hash chains before importing.
- **Export and erase.** Export everything about a project; erase a project's sessions and memory in one audited command.
- **Secrets.** The leak scan (S7) runs over backups before they are written.

::: gate
Retention rules applied to a synthetic year of sessions leave exactly the expected set; a backup restores to a fresh home directory with every hash chain verifying; an encrypted archive is unreadable without the key.
:::

### 13.3 What could break

::: risk
**Security friction drives the owner to full automation everywhere.** Then the per-action protections never run. Mitigation: per-action mode tuned so prompts are rare (≤ 2 per 100 calls, P-metrics), scoped grants, and good defaults.
:::

::: risk
**The egress proxy becomes a bypass.** Mitigation: the sandbox profile only allows the proxy's loopback port; the proxy authenticates the run token; tests cover bypass attempts.
:::
