# Overseer — agent notes

Platform core for an agentic coding engine, built per `agent-harness-playbook.pdf`
(extracted text: `playbook.txt`). Read that file before changing architecture.

## Layout

- `crates/overseer-core` — engine: canonical turn IR (`ir.rs`), append-only
  JSONL event log (`event.rs`), usage ledger (`ledger.rs`), model profiles
  (`profile.rs`), tool registry + tools (`tools/`), provider trait +
  Anthropic + OpenAI-compatible adapters (`provider/`), ReAct loop with
  budgets (`agent.rs`), stuck detector (`stuck.rs`), L4 permission gate
  (`perm.rs`), deterministic compaction (`compact.rs`)
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
8. Compaction is a view over the event log, never a mutation; summaries are
   derived mechanically from raw events (never re-summarized), and the
   recency tail always starts on a ModelResponse boundary so tool pairing
   survives the cut.

## Commands

- Build: `cargo build` (or `cargo build -p overseer-cli`)
- Test: `cargo test`
- Run: `ANTHROPIC_API_KEY=... cargo run -p overseer-cli -- exec "task"`
- JSONL event stream: add `--json`; resume: `--resume <session-dir>`

## Git workflow (github.com/sirrayi/overseer, private)

Branch ladder — promotion flows upward, work flows downward:

```
main   ← tagged releases only; never push directly
dev    ← stress-testing / dev-release builds cut from here
review ← default branch; all PRs target here first
*      ← feature branches fork off review
```

- Branch off `review` with typed names: `feat/<slug>`, `fix/<slug>`,
  `chore/<slug>`, `docs/<slug>`, `eval/<slug>`
- Open PR → `review`. Fix conflicts + final polish there.
- `review` → `dev` merge gates a dev release (stress testing).
- `dev` → `main` only when a release is finalized.
- Direct pushes to `main` are forbidden by convention (no Pro-tier
  protection available on a private repo — enforced socially).
- CI runs on PRs and on pushes to `main`; keep main pushes rare to
  conserve Actions minutes.
