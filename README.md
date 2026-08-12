# Overseer

**The overseer. Under the hood, the engine. Everything.**

Overseer is a vault-native, self-improving, verification-driven agent harness.
It is not a chatbot. It is a deterministic orchestrator that uses models as
components — with a canonical Obsidian-compatible vault as its memory, a
verification engine that checks work instead of guessing, and a recursive
learning loop that turns corrections, failures, and successes into reusable
skills.

> **The L3 guardrail — read this first.**
> Overseer may *propose* changes to its own prompts, thresholds, code, or
> learning rules. It must **never apply them silently**. All self-modification
> (L3) changes require **explicit human approval**, evidence, risk assessment,
> and rollback. The user is the final authority. Always.

## Status

Early development — Batch 2 (CLI + Session Experience) in progress. The
agent loop, provider abstraction, tool registry, and approval gate are
functional; the CLI is becoming a daily driver. See
[ROADMAP.md](ROADMAP.md) for the full master plan (44 parts, 13 batches)
and [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md) for what exists.

## Core ideas

- **The vault is canonical.** Markdown notes in the Obsidian-compatible vault
  are the durable source of truth. SQLite, FTS5, embeddings, caches, and
  indexes are derived and disposable — if a cache is deleted, overseer
  rebuilds it from the vault.
- **Security is continuous.** Safe defaults, approval gates, secret hygiene,
  path safety, and injection awareness from day one — not bolted on at the end.
- **Learning is based on verified truth.** No durable lessons from unverified
  outcomes. Record, verify, then learn.
- **Context is compiled, not dumped.** Every prompt is built to a budget with
  progressive disclosure and summarized tool output.
- **Efficiency is mandatory.** Token-frugal, cost-aware, power-aware,
  laptop-friendly. Eco, balanced, and performance modes.
- **Self-modification is proposal-only.** See the guardrail above.

## Quickstart

```bash
uv sync --dev
uv run overseer init --vault ~/overseer-vault
uv run overseer doctor
uv run overseer run "explain this repo"   # one-shot task
uv run overseer chat                      # interactive session
```

`overseer init` creates a compliant vault (00-Inbox through 99-Meta) plus a
sample config with placeholders. Real secrets go in environment variables
(`OVERSEER_*`), never in config files.

### Commands

| command | purpose |
|---|---|
| `overseer chat` | interactive session with the agent loop (streaming) |
| `overseer run <task>` | non-interactive single task |
| `overseer model` | inspect/switch provider model (secrets never shown) |
| `overseer tools` | list registered tools and schemas |
| `overseer config` | view/validate config safely |
| `overseer sessions` | list sessions (meta only) |
| `overseer trace <id>` | inspect a session transcript (redacted) |
| `overseer export <id>` | export a session as redacted markdown |
| `overseer doctor` | validate config, vault, provider, permissions |
| `overseer init` | create a vault + sample config |
| `overseer search <query>` | full-text search over session events (FTS5) |
| `overseer rebuild` | rebuild the episodic index from raw transcripts |
| `overseer memory` | stub — knowledge layer arrives in B5 |
| `overseer skills` | stub — recursive learning arrives in B7 |
| `overseer cron` | refused — scheduled execution needs B10 hardening |

Sessions persist under `<vault>/.overseer/sessions/`. Risky terminal
commands and writes outside the vault prompt for approval; denials are
logged. All output is redacted before display or export.

### Verification (B4)

Overseer detects the project environment (language, package manager, test
runner, linter, typechecker) from the standard manifests, generates a
cached repo map, and runs targeted checks. Failed checks become failure
cards fed back to the model; checkpointed writes are rolled back when
verification fails. Git tools (`repo_map`, `git_status`, `git_diff`,
`git_log`) are read-only; destructive git commands require approval.

## Development

```bash
uv run pytest -q        # tests
uv run ruff check .     # lint
uv run ruff format .    # format
uv run mypy src/overseer  # types
./scripts/review_packet.sh  # 9-item review packet for the Qwen review loop
```

## License

MIT — see [LICENSE](LICENSE).

## Privacy

Local-first by default. Session data stays on your machine unless you
explicitly enable hosted services. You can inspect, export, and delete
everything overseer learns.
