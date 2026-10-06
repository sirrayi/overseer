# where things stand (2026-10-06)

all overseer work lives in `/Users/chf/Desktop/Desktop - m1/overseer` and on [github.com/sirrayi/overseer](https://github.com/sirrayi/overseer). the default branch is `review`.

## what changed since 09-30

- #51 (refresh) and #52 (memory v2, tool economy, subagent tiers) are merged into `review`. the merge commits carry their deferred lists.
- overseer life started: the plan, the security design and phase 0 of the backend are in. see [the plan](research/2026-10-05-digital-life.md), [the security design](research/2026-10-05-life-security.md) and [phase 0 results](research/2026-10-05-life-phase0-results.md).
- the repo has a readme now.
- the repo is public.

## overseer life: what's waiting

| step | waiting on |
|---|---|
| live x test (`crates/overseer-life/examples/x_spike.rs`) | you: an x developer app of type native app, callback `http://127.0.0.1:8723/callback`, a few dollars of credit, and the client id. it costs about $0.03 and revokes its token at the end |
| subscriptions from bank and email | you: which bank and which email provider |
| apple developer account | later. needed for signed builds, passkeys and password autofill in the built-in browser |
| phase 1 backend | encrypted storage, the vault, the connector framework, "since last open" deltas |
| frontend | after the backend. native swiftui shell for mac and iphone over the same rust core |

## harness: next steps

1. first live run with a real model. everything since the refresh has only been tested against mock providers. run a tui or web session with memory on and check recall, reminders, `tools`/`run_code` and a subagent with `tier`/`verify`.
2. live computer-use test. turn on **CuaDriver** under accessibility and under screen recording in system settings, run `cua-driver permissions grant`, then the ignored `computer_live` test. synara's patched driver has the same 57 tools, so it can replace the installed one.
3. small fixes found in phase 0: `browser_type` should require `ref`, `zoom` should require `x1..y2`, and a few args overseer sends aren't in the driver's schemas (details in the phase 0 results).
4. benchmarks stay on hold until you lift it.
5. deferred work is listed in the #52 merge commit: persisted memory index, recurring reminders, hard purge, anthropic `defer_loading`, `computer` and memory writes from `run_code`, cross-provider verifiers, resume across a rebased parent, multi-process sessions.

known flaky test: `tools::computer::cua::tests::observe_renders_elements_and_remembers_tokens` failed once with ETXTBSY under load.

## decisions you might revisit

- memory is on by default. `--no-memory` turns it off, `--bare` and the eval rig never use it.
- episode notes keep the session's first prompt, redacted.
- the heavy tier uses the priciest model in the family. only `consult` uses it by default.
- resident tool specs are 92 chars over the 6,000 target, accepted.
- the `release.yml` `plan` job fails on every pr because it never gets a machine. `pr-run-mode = "skip"` in `dist-workspace.toml` would stop that.
- #50 is fully contained in #51 and can be closed.

## how to resume

- build and test: `cargo build`, `cargo test --workspace`.
- run: `cargo run -p overseer-cli -- web` (opens a tab, ports 8641-8660) or `-- tui`.
- don't trigger github actions. pushing a branch is fine. a pr needs `[skip actions]` in its newest commit.
- local notes and personal results live in `notes/`, which git ignores.

## local state

- nothing is running.
- outside this folder, safe to delete whenever: worktrees `~/.cache/overseer-{integrate,te,st,next}` (use `git worktree remove`), `~/.cache/overseer-cloud-launch`, build caches `~/.cache/overseer-{integrate,next}-target` (about 4 GB), synara's driver build in `~/.cache/overseer-life`, and the old subagent worktrees in `~/.overseer/sessions/1789914292383/subagents/wt-1..4` (their staged changes are just the pre-09-24 tree, already in git).
- a verified backup of all of that is in `notes/2026-10-04-consolidation/`.
- this folder is in icloud-synced desktop, and `target/` is about 8.8 GB. moving the repo out of icloud would avoid sync conflicts like the old `.git/index 3`.

## security to-do

- rotate the provider api key pasted in the 09-20 session. it's still in local session logs.
