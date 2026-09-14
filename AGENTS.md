# Overseer — agent notes

Platform core for an agentic coding engine, built per `agent-harness-playbook.pdf`
(extracted text: `playbook.txt`). Read that file before changing architecture.

## Layout

- `crates/overseer-core` — engine: canonical turn IR (`ir.rs`), append-only
  JSONL event log (`event.rs`), usage ledger (`ledger.rs`), model profiles
  (`profile.rs`), tool registry + tools (`tools/`), provider trait +
  Anthropic adapter (`provider/`), ReAct loop with budgets (`agent.rs`)
- `crates/overseer-cli` — `overseer exec` headless/CI surface
- `crates/overseer-proto` — wire protocol types (stub)
- `crates/overseer-tui` — terminal frontend (stub)
- `eval/` — Inspect AI evaluation rig (scaffold)

## Invariants (do not violate)

1. Events are immutable; session state is a view over `events.jsonl`.
2. Stable prompt prefixes — nothing volatile (timestamps, session ids, git
   status) above the cache boundary.
3. Engine enforces budgets (steps, cost), never the model.
4. Tool results are budgeted: ~30K chars inline, then spill to file.
5. Read-before-edit is enforced by the harness, not the prompt.
6. Reasoning blocks are opaque — round-trip verbatim, never inspect/mutate.
7. Raw provider `stop_reason` is preserved end-to-end.

## Commands

- Build: `cargo build` (or `cargo build -p overseer-cli`)
- Test: `cargo test`
- Run: `ANTHROPIC_API_KEY=... cargo run -p overseer-cli -- exec "task"`
- JSONL event stream: add `--json`; resume: `--resume <session-dir>`
