# Where we left off (2026-09-30)

All Overseer work lives in this checkout: `/Users/chf/Desktop/Desktop - m1/overseer`, on branch `feat/overseer-next`. Everything is pushed to `github.com/sirrayi/overseer`.

## The two open PRs (merge in this order)

| PR | Branch → base | What's in it | CI (Mac runner) |
|---|---|---|---|
| [#51](https://github.com/sirrayi/overseer/pull/51) | `feat/overseer-refresh` → `review` | Web/TUI redesign plus bowtie logo, web security hardening, prompt caching, cua-driver computer use, clean-code sweep | green, except `plan` |
| [#52](https://github.com/sirrayi/overseer/pull/52) | `feat/overseer-next` → `feat/overseer-refresh` | Memory v2, tool economy, subagent tiers (stacked on #51) | green, except `plan` |

- Neither PR is merged.
- `plan` (cargo-dist on GitHub-hosted runners) fails on every PR, including #50 since 09-20. It never gets a machine and isn't caused by this work. The fix is `pr-run-mode = "skip"` in `dist-workspace.toml`; that's the owner's call.
- #50 (`eval/cloud-benchmark-matrix`) is fully contained in #51. Close it once #51 merges.
- `feat/memory-v2`, `feat/tool-economy` and `feat/subagent-tiers` stay on GitHub for reference. They're already merged into `feat/overseer-next`.

## What's built

The research behind every choice is in `docs/research/`:
- `2026-09-29-pillars.md`: the overview, with a status section at the top
- `2026-09-30-memory-v2.md`
- `2026-09-30-tool-economy.md`
- `2026-09-30-subagent-tiers.md`

**Memory v2**
- Two stores, both outside the workspace: a user store in `~/.overseer/memory` and a project store in `~/.overseer/projects/<slug>-<hash8>/memory`.
- One `memory` tool with ops search, get, remember and forget.
- Ranking: BM25F, fused by RRF with ACT-R/Petrov activation and confidence.
- Auto-recall brings up to 3 notes into a turn, and reminders fire on `at:`, `kw:` or `path:` triggers. Both are recorded as events, so resume replays identically.
- Each session gets a deterministic episode note, and `overseer consolidate` distils each episode once.
- Secrets are redacted at write time, and writes made after untrusted content arrives are quarantined.

**Tool economy**
- Rarely used tools and all MCP tools sit behind one `tools` op tool (search/call).
- `run_code` runs model-written scripts in QuickJS, with no filesystem, network or process access; every sub-call still passes the permission gate.

**Subagent tiers**
- Subagent budgets now compose, which fixes a real bug where every child got the parent's whole budget.
- Two more existing bugs fixed:
  - task ids restarted at 1 every step, so the second spawn failed;
  - background tasks left behind by a crash held the fan-out slots forever.
- Light, standard and heavy tiers, with one escalation step on failure.
- A fresh-context `verify` mode with tamper evidence, plus `resume` and `consult`.

**Measured on a clean VM**

| | Before | Now |
|---|---|---|
| Resident tool specs (every optional tool, MCP and skills present) | 9,886 chars | 6,092 chars (~1.5K tokens); 5,739 without skills |
| Base static prompt | 929 chars | 615 chars |
| Release binary | 7.0 MB | 8.94 MB (`run_code` is 1.26 MB of that; `--no-default-features` builds without it) |
| Tests | 763 | 928 passed, 0 failed |
| Memory index build at 500 / 5K / 20K notes | — | 10 / 86 / 367 ms |
| Memory search at 500 / 5K / 20K notes | — | 0.13 / 1.6 / 7.3 ms |

## Next steps, in priority order

1. **Merge #51, then #52.** Owner.
2. **First live run with a real model.** Everything since the refresh has only been tested against mock providers. Run a TUI or web session with memory on and check recall, reminders, `tools`/`run_code` and a subagent with `tier`/`verify`.
3. **Live computer-use test.** In System Settings → Privacy & Security, enable **CuaDriver** under Accessibility and under Screen & System Audio Recording. Then run `cua-driver permissions grant`, followed by the ignored `computer_live` test. It opens TextEdit in the background, types `overseer`, verifies it, and closes without saving. It also shows whether pixel clicks land in the right place.
4. **Benchmarks stay on hold** until the owner lifts it. When they resume:
   - first a baseline with the existing `eval/` rig;
   - then LongMemEval-S for memory (at least 3 runs; skip LoCoMo, which has a broken answer key);
   - tier and verify ablations;
   - an accuracy check of deferred-tool calling.
5. **Deferred work**, listed in full in #52's Deferred section. The main items:
   - a persisted memory index (5K and 20K notes miss their 80 and 300 ms build targets);
   - recurring reminders;
   - hard purge of memory;
   - native Anthropic `defer_loading`;
   - `computer` and memory writes from `run_code`;
   - cross-provider verifiers;
   - resume across a rebased parent;
   - multi-process sessions.
6. **Known flaky test**, older than this work: `tools::computer::cua::tests::observe_renders_elements_and_remembers_tokens` failed once with ETXTBSY under parallel load.

## Decisions made that the owner may revisit

- **Memory is on by default.**
  - `--no-memory` turns it off.
  - `--memory` keeps the old store inside the workspace (`<cwd>/memory`).
  - `--bare` and the eval rig always run without memory.
- **Episode notes keep the session's first prompt,** redacted.
- **Tier models.** The heavy tier is the priciest model in the profile table for the family, so for Anthropic that's `claude-fable-5`. Only `consult` uses heavy by default; `verify` uses the session's normal model. `--heavy-model` overrides the choice.
- **Tool-spec target.** The resident tool specs are 92 chars over the 6,000 target. That was accepted: the remaining specs are already tight.
- **Logo.** The bold bowtie is the global mark, in the single file `crates/overseer-tui/web/mark.svg`.

## How to resume

- **Build and test:** `cargo build`, `cargo test --workspace`. TUI snapshots: `cargo test -p overseer-tui`.
- **Run:** `cargo run -p overseer-cli -- web`. It scans ports 8641–8660 and opens a browser tab. Use `-- tui` for the terminal UI.
- **Heavy work on Devin cloud:** run `devin --cloud -p --prompt-file <brief.md>` from a trusted directory under `~` (this checkout works). Keep local work to debug builds and narrow gates.
- **Workflow this project used:**
  1. decision record with sourced research;
  2. brief;
  3. pre-review of the brief against the code;
  4. implementation on a cloud VM;
  5. hostile line-by-line review;
  6. follow-up fixes;
  7. `--no-ff` integration;
  8. full gates on a clean VM;
  9. PR with a Deferred section.
- **Last session's briefs, reviews, cloud reports, logo exploration and screenshots** are in `notes/2026-09-30-session/`. It's local only: `/notes/` is listed in `.git/info/exclude`.

## Local state

- **No local servers are running.** Both were stopped when the session ended.
  - **Web app:** `cargo run -p overseer-cli -- web`. It prints a tokenized `http://127.0.0.1:<port>/#t=…` URL and opens it; the token is in `~/.overseer/web/token`. To reopen the demo session, add `--resume notes/2026-09-30-session/demo-session/1789920214220`.
  - **Logo sheet:** `cd notes/2026-09-30-session/brand && python3 -m http.server 8650 --bind 127.0.0.1`.
- **Leftovers outside this folder.** All are clean, fully pushed, and safe to delete whenever:
  - worktrees `~/.cache/overseer-{integrate,te,st,next}` (remove them with `git worktree remove`);
  - `~/.cache/overseer-cloud-launch`, an old clone used only to launch cloud sessions;
  - build caches `~/.cache/overseer-integrate-target` (2.7 GB) and `~/.cache/overseer-next-target` (1.3 GB);
  - old subagent worktrees under `~/.overseer/sessions/1789914292383/subagents/wt-1..4`.
- **Untracked files in the repo root** (`HANDOFF.md` from an earlier session, `devin-harness-internals.txt`, `multi-agent-stress-prompts.pdf`) are left as they were.
- **Browser (`Search.app`).** Its cached DeepSeek-whale icon for `127.0.0.1` was moved aside to `~/Library/Application Support/Search/icons/127.0.0.1.png.deepseek-whale.bak`. Restart the browser to pick up the bowtie favicon.

## Security to-do

- **Rotate the provider API key** that was pasted in the 09-20 session. It still sits in local session logs.
