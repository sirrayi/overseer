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

Early development — Batch 0 (Foundation) in progress. Not yet usable as an
agent. See [ROADMAP.md](ROADMAP.md) for the full master plan (44 parts,
13 batches) and [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md) for
what exists.

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

## Quickstart (Batch 0 scope)

```bash
uv sync --dev
uv run overseer init --vault ~/overseer-vault
uv run overseer doctor
uv run overseer version
```

`overseer init` creates a compliant vault (00-Inbox through 99-Meta) plus a
sample config with placeholders. Real secrets go in environment variables
(`OVERSEER_*`), never in config files.

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
