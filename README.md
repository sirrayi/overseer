# overseer

overseer is an agent harness written in rust. it runs coding agents in your terminal or your browser, saves every session as an append-only event log, and puts hard limits on what an agent can spend, touch or run.

it's also growing into something bigger. **overseer life** is a dashboard for your whole digital life: your ai subscriptions and how much of each you've used, your socials and how they changed since you last looked, every subscription you pay for, a built-in browser, computer use, and agents that take care of the boring stuff. the harness is the engine under all of it.

early days and moving fast. macos and linux for now.

## what the harness does

- runs agents on anthropic, any openai-compatible api, gemini or opencode
- shell commands run in a sandbox (`sandbox-exec` on macos, `bwrap` on linux) with the network off by default
- asks you before anything risky, and remembers the answers you tell it to keep
- memory that carries across sessions, split into your own notes and per-project notes
- subagents with light, standard and heavy tiers, plus a fresh-eyes `verify` mode
- snapshots files before it edits them, so you can rewind a session
- mcp servers, prompt caching, and computer use on macos through cua-driver
- an always-on daemon that can take work from telegram or a webhook

there's a terminal ui, a browser ui on localhost, and a headless mode for scripts and ci. every session lives under `~/.overseer`.

## getting started

you need a recent stable rust toolchain.

```sh
git clone https://github.com/sirrayi/overseer
cd overseer
cargo build --release

export ANTHROPIC_API_KEY=...   # or OPENAI_API_KEY, GEMINI_API_KEY, OPENCODE_API_KEY

./target/release/overseer                          # terminal ui
./target/release/overseer web                      # browser ui, opens a tab
./target/release/overseer exec "fix the failing test"
```

`overseer --help` lists the rest (memory, rewind, stats, mcp, daemon). run the tests with `cargo test --workspace`.

## overseer life

phase 0 of the backend is in. `crates/overseer-life` reads your plan usage from cursor, devin, opencode, claude code and codex, using the logins those tools already keep on your machine. it also has an x connector for followers, following and post counts, with a daily spending cap so the x api bill can't run away.

it never refreshes another app's login, never writes to another app's files, and never puts a secret in a log or on screen. try it:

```sh
cargo run -p overseer-life --example probe              # shows which logins it can find
cargo run -p overseer-life --example probe -- all --live    # one usage request per source
```

up next: the live x test, subscriptions from your bank and email receipts, then the encrypted storage and vault. after that comes the mac and iphone app, a native swiftui shell on top of the same rust core.

the full plan is in [docs/research/2026-10-05-digital-life.md](docs/research/2026-10-05-digital-life.md) and the security design is in [docs/research/2026-10-05-life-security.md](docs/research/2026-10-05-life-security.md).

## what's where

| path | what it is |
|---|---|
| `crates/overseer-core` | the engine: event log, providers, tools, permissions, memory, subagents |
| `crates/overseer-cli` | the `overseer` command |
| `crates/overseer-tui` | terminal ui and the localhost browser ui |
| `crates/overseer-gateway` | the always-on daemon and its channels |
| `crates/overseer-proto` | wire types |
| `crates/overseer-life` | overseer life connectors |
| `eval/` | benchmark rig that runs overseer against a control agent on the same tasks |
| `docs/` | the harness playbook, research notes and [where things stand](docs/LEAVING-OFF.md) |

`AGENTS.md` has the detailed notes for anyone (or any agent) working on the code.

## credits

the usage readers in overseer life are ported from [synara](https://github.com/Emanuele-web04/synara) (MIT, by t3 tools and emanuele di pietro). computer use runs on the driver from [cua](https://github.com/trycua/cua) (MIT).

## license

MIT or Apache-2.0, your pick (set in `Cargo.toml`).
