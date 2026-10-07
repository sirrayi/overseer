## 11. Policy: Principles, Rules, AGENTS.md and the Instruction Compiler

The owner named principles, rules and AGENTS.md as sub-pillars. This chapter treats them as one system, because that is what they are: every source of instruction and permission the harness obeys. Today they are scattered. The target is a compiler.

### 11.1 The bar

Production system prompts are assembled from section builders with a static/dynamic cache boundary and per-model variant flags; instructions are a compiler problem **[src: playbook thesis 8]**. There is no cryptographic separation between instructions and data, so the hierarchy is defence in depth: provenance-marked tool results, probabilistic detectors, deterministic tool-call gating, and human approval for irreversible actions. Prose rules shape attempts; only harness enforcement decides outcomes.

Every serious harness reads a project instruction file (AGENTS.md has become the cross-tool convention; Claude Code reads CLAUDE.md). Overseer, which keeps one of the most detailed AGENTS.md files of any project, does not read it at all.

### 11.2 Where we stand

| Source | How it reaches the model or the gate today |
|---|---|
| Built-in contract and safety segments | Static prompt segments (`prompt.rs`) |
| Memory CORE and INDEX | Resident memory segment, assembled once per agent |
| Skills | Metadata index segment; bodies on demand |
| Persona | Approved bodies only, as a segment |
| Modes | Six built-ins: tool removal, edit globs, a prompt fragment as a nudge |
| Microagents | Substring triggers, per-turn nudges, capped |
| Hooks | `hooks.json` data rules at three points |
| Persisted rules | `~/.overseer/rules`, exact match, append-only, no deny |
| Autonomy ladder and presets | Flags and defaults in `perm.rs` |
| AGENTS.md | **Not read** |
| Consent for computer use | Per-call Ask today; owner decisions recorded, not implemented |

Each source is consulted where it is used, with its own matching rules and no shared notion of precedence, scope, provenance or explanation.

### 11.3 Target: one compiler, two outputs

```
 sources                                   compiled policy (per agent, fingerprinted)
 ─────────────────────────────────         ─────────────────────────────────────────
 built-in constitution  ─┐                 ┌─▶ prompt segments, ordered, with provenance
 user AGENTS.md          │                 │     (static ones above the cache boundary)
 project AGENTS.md       │                 │
 nested AGENTS.md        ├─▶  COMPILER  ───┼─▶ permission table: match → effect, scope,
 policy.toml (user/proj) │   precedence,   │     expiry, provenance
 modes · microagents     │   conflicts,    │
 hooks · consent modes   │   lint          ├─▶ hook table (typed events)
 memory CORE · persona   │                 │
 profile overrides      ─┘                 └─▶ effective config values (budgets, away, retry)
```

The compiler runs once per agent build (start, resume, model switch, handoff), like prompt assembly today, so the static prefix stays byte-stable within a context. Its output is fingerprinted; a change between builds is logged with a diff summary.

### 11.4 Workstreams

#### P1 — The AGENTS.md loader

**What.** Read project instructions, in the cross-tool format, with the right trust and placement.

**Discovery.**

| Level | Location | Placement |
|---|---|---|
| User | `~/.overseer/AGENTS.md` | Static prefix (stable per session) |
| Project root | `<git toplevel>/AGENTS.md` | Static prefix |
| Compatibility imports | `CLAUDE.md`, `.github/copilot-instructions.md`, `.cursorrules` at the root, read only if AGENTS.md is absent or imports them | Static prefix |
| Nested | `AGENTS.md` in any directory under the root | Injected below the cache boundary the first time the agent reads or edits a path under that directory, provenance-wrapped as `agents-md:<path>`, once per context |
| Imports | `@path` lines (the convention some tools use) resolving inside the repository only | Inlined at the importing file's position, depth-limited |

**Size.** The root file's resident share is capped (default 8,000 characters). Longer files are split by heading: the first section and a heading index stay resident, and sections load on demand through a `policy op=section` read, the way skills load bodies.

**Trust.** A repository's AGENTS.md is written by whoever wrote the repository. For the owner's own repositories that is trusted; for a freshly cloned third-party repository it is untrusted input that sits in the system prompt. Rule: the first time a project's AGENTS.md (by content hash) is seen, interactive sessions ask once to trust it; headless and detached runs treat an untrusted file as provenance-wrapped context below the boundary instead of a system segment, and arm nothing else. Trust is stored per hash, so any change re-asks. Because the owner's own repositories change their AGENTS.md often (Overseer's changes with most PRs), the owner can instead trust a repository by its path and remote, which covers every version of its files; that grant is listed and revocable like any other (P4). Path and remote are values a repository can change, so the grant is bound to the repository's identity as well (its canonical git directory and the device and inode of that directory), and a fresh clone or a remote change at the same path invalidates it.

**Format.** Plain Markdown. No special syntax is required; optional frontmatter may declare machine-readable parts (verify commands, heavy commands for the scheduler O18, protected paths), which the compiler lifts into config with provenance.

::: gate
Fixture repositories for each discovery case; the root file appears in the static prefix and the prefix stays byte-stable across turns; nested files inject once on first touch; an untrusted file never lands above the cache boundary; Overseer's own `AGENTS.md` is read correctly and its section index fits the cap.
:::

#### P2 — The instruction compiler

**What.** Invariant 17. One pipeline that turns every source into ordered prompt segments, a permission table, a hook table and effective config.

**Precedence.** From strongest to weakest:

1. **Engine invariants and hard denies.** Not overridable by any file (containment, secret handling, budgets exist). The owner's explicit consent configuration can relax gates where the owner has decided so (P5), recorded as such.
2. **Owner configuration** (user `policy.toml`, user AGENTS.md, consent modes).
3. **Project configuration** (project `policy.toml`, project AGENTS.md, nested AGENTS.md), within what the owner allows.
4. **Modes and microagents.**
5. **Built-in defaults.**

Within a level: deny beats ask beats allow, and a more specific match beats a general one. A project can tighten anything; it can loosen only what the owner's configuration delegates (`allow_project_relax = [...]`).

**Conflicts.** Contradictory instructions (one file says "always run tests with `--release`", another "never") are detected for the machine-readable parts and reported by `overseer policy lint`; prose conflicts cannot be detected reliably, so the compiled prompt orders sources by precedence and states that order to the model.

**Per-model variants.** The compiler can select segment variants per model family (the Codex practice of per-model tuned prompts), each variant tested on the rig.

**Commands.** `overseer policy show` (the compiled result with provenance per line), `overseer policy explain <tool> <resource>` (which rule decides and why), `overseer policy lint`, `overseer policy diff` (between two builds).

::: gate
Every current source is migrated into the compiler with behaviour unchanged (golden tests on gate verdicts and prompt bytes for a corpus of configurations); afterwards no module reads a raw rules source directly (enforced by a lint).
:::

#### P3 — Rules engine v2

**What.** Replace the bash-only, exact-match, append-only rules file with real rules.

**Design.** `policy.toml` at user and project level:

```toml
[[rule]]
match  = { tool = "bash", command = "cargo test*" }
effect = "allow"
scope  = "always"
reason = "tests are safe here"

[[rule]]
match  = { tool = "write", path = "migrations/**" }
effect = "ask"

[[rule]]
match  = { class = "ExternalComms", origin = "untrusted" }
effect = "deny"
```

Matchers: tool, glob or regex on the resource (command, path, URL, MCP tool name), irreversibility class, autonomy domain, taint state, origin (interactive, workflow, gateway channel), agent role. Effects: allow, ask, deny. Scopes and expiry from P4. `~/.overseer/rules` lines migrate automatically into equivalent rules, and the old file becomes read-only. The **[fix2]** grant keys for non-bash tools are the starting point.

#### P4 — Scoped, expiring, revocable grants

**What.** Approval fatigue makes security unusable; permanent grants make it meaningless. Grants need scopes.

**Scopes.** `once` (this call), `task` (this subagent or plan item), `run` (this run, including after resume), `session`, `origin+action` (for example, this website and this kind of action), `always`. Each grant carries an expiry (time, turn count, or run end), a reason, and its provenance (who granted it, from which frontend). `overseer grants` lists and revokes; the web UI and Overseer Life's Permissions page show the same table (F4).

::: decision
Money, security-setting and account-deletion actions always ask, in every mode except full automation where the owner has configured otherwise (Life security design approval trust model, 2026-10-05, and the consent decision of 2026-10-07).
:::

#### P5 — Consent modes

**What.** Implement the owner's computer-use consent decisions (2026-10-07) as general modes the compiler understands.

| Mode | Behaviour |
|---|---|
| **Per-action (default)** | The first screen read and the first mutating action in a task each ask; approval grants `task` scope for that domain; later calls in the same task do not ask |
| **Full automation** | Never asks, including the Rule-of-Two floor, per the owner's decision; secrets and sensitive surfaces follow the owner's dedicated permissions (P6) |

Under full automation, nothing is silent in the record: every call that would have asked is journaled with the verdict that would have applied (S4), the kill switch remains (invariant 24), and the mode is shown in every frontend's status line.

::: gate
The consent-modes branch (planned after integration) lands with tests for both modes across computer, bash, file and MCP domains; the per-task grant expires at task end; full automation produces the shadow-verdict journal.
:::

#### P6 — Dedicated permissions for secrets and sensitive surfaces

**What.** The owner's decision: in full automation, secrets and similar surfaces are controlled by dedicated, user-configured permissions rather than a hard-coded denylist.

**Design.** Named categories, each with allow, ask or deny, configured per mode:

| Category | Examples |
|---|---|
| `secrets` | Typing a brokered secret; reading secret-bearing files |
| `password_managers` | 1Password, Keychain Access, Passwords, Bitwarden windows |
| `system_security` | System Settings privacy panes, SecurityAgent dialogs |
| `payments` | Checkout pages, payment apps, the `Money` class |
| `identity` | Account settings, 2FA setup, recovery codes, the `Identity` class |
| `messaging_send` | Sending email, posts, messages |

Onboarding (X8) asks the owner to set these when full automation is first enabled; no default is imposed silently. Categories are matched by bundle identifier, application name normalization, URL patterns and tool classes, so renaming an application cannot launder it (the Synara technique, reused as a matcher rather than an unconditional denylist).

#### P7 — User-defined modes

**What.** Modes as files: `~/.overseer/modes/<name>.toml` and project equivalents, declaring tool set, edit globs, prompt fragment, default tier and policy overlays. The six built-ins become files shipped with the binary.

#### P8 — Stream rules

**What.** omp's TTSR idea **[src]**: a regex or syntax match on the model's output stream aborts the generation, injects a rule, and retries from the same point. Rules cost no context until they fire.

**Design.** Rules declared in policy (`[[stream_rule]] pattern = "…" message = "…"`). On a match during streaming, the partial response is discarded (never logged, consistent with R2's partial-stream rule), the rule text is appended as a provenance-wrapped nudge, and the request is retried. A per-run cap prevents loops. Measured on the rig before shipping any default rules.

#### P9 — Contextual rules (microagents, version 2)

**What.** Microagents trigger on crude substrings. Make triggers precise.

**Design.** Triggers by path glob (the agent touched `**/*.sql`), file type, tool use, word-bounded keywords, plan item tags, and `always`. Bodies inject below the boundary once per context, capped as today. Existing microagent files keep working.

#### P10 — Hooks, version 2

**What.** Typed hook events with an optional script form.

**Events.** Before and after a tool call, before and after a model call, run start and end, compaction, approval requested and resolved, effect intent (R7), journal milestone.

**Forms.** Data rules (as today), and script hooks that receive JSON on stdin and return JSON, run in the sandbox with the bash environment allowlist and a hard timeout, failing open or closed as each hook declares. Script hooks from project configuration need the same hash-pinned trust as project MCP (T5).

#### P11 — Explain any decision

**What.** Invariant 18. `overseer why <event-id>` (and a "why" affordance on every UI element) explains: for a permission verdict, the rule and its provenance; for an eviction or compaction, the planner trace (C7); for a retry or failover, the failure class and policy; for a routing choice, the rule that picked the tier.

#### P12 — Managing permissions

**What.** A permissions page in the web UI and TUI, and the `overseer grants` and `overseer policy` commands: every rule and grant with scope, expiry, provenance and last use; revoke in one action. The same model backs Overseer Life's Permissions page (F4).

#### P13 — Principles enforced by the build

**What.** Turn the constitution into checks where possible.

| Check | Enforces |
|---|---|
| Prefix hygiene lint (have) | Invariant 2 |
| Every `DEFERRED(` marker has an owner and a gate | Deferral principle |
| Every blocking call wrapped in a deadline or allow-listed (R3) | Invariant 14 |
| Every new event kind declares skippability; golden logs replay (R13) | Invariant 21 |
| Every store write goes through the chokepoint (M10) | Invariants 19 and 25 |
| No module reads raw policy sources (P2) | Invariant 17 |
| Size, RSS and startup budgets (Z1) | Invariant 22 |

#### P14 — The instruction hierarchy as defence in depth

**What.** Make the hierarchy explicit to the model and enforced by the engine: system segments are instructions; everything inside a provenance wrapper is data. The compiled prompt states this once, briefly. The gate never trusts the model's claim that an instruction came from the owner. Injection research is tracked and the corpus extended (S6).

#### P15 — Git safety defaults

**What.** Long unattended runs touch git constantly. Some git operations destroy work or publish it, and they need defaults that hold in every mode unless the owner configures otherwise.

**Defaults compiled into every policy.**

| Operation | Default |
|---|---|
| Force push, history rewrite of published branches, branch deletion on a remote | Deny |
| Push to `main`, `dev` or the default branch | Deny (the branch ladder in `AGENTS.md`) |
| Push to any other remote branch | Ask (ExternalComms) |
| `git reset --hard`, `git clean -fdx`, checkout over uncommitted changes in the owner's tree | Ask; allowed silently inside the agent's own worktrees |
| Commits | Allowed in the agent's worktrees, with an agent trailer so authorship is clear; commit signing follows the owner's git config |
| Pull requests | Ask; repository conventions such as Overseer's `[skip actions]` rule come from its AGENTS.md frontmatter and are applied automatically |

### 11.5 What could break

::: risk
**A malicious repository steers the agent through its AGENTS.md.** Mitigation: hash-pinned trust before the file enters the system prompt; untrusted files are data, below the boundary; engine invariants are never overridable by project files.
:::

::: risk
**The compiler becomes a bottleneck of complexity.** Mitigation: it is a pure function with golden tests; every source migrates behaviour-preserving first; new semantics land one at a time.
:::

::: risk
**Full automation removes the safety floor by design.** That is the owner's decision and this roadmap honours it. Mitigation within that decision: the shadow-verdict journal, dedicated sensitive-surface permissions set at onboarding, the always-visible mode indicator, and the kill switch.
:::

### 11.6 Metrics

| Metric | Target |
|---|---|
| Approval prompts per 100 tool calls in per-action mode on the rig workload | ≤ 2 |
| Decisions with a complete `why` explanation | 100% |
| Policy sources read outside the compiler | 0 |
