# Overseer — Decision Records

> Every non-obvious choice, with context and consequences (plan: decision records).

## D-001: Python 3.11 + uv + src layout (2026-08-11)
- Context: stack choice for the harness.
- Decision: Python 3.11, uv, src layout, pyproject.toml, ruff, pytest, mypy.
- Alternatives: Go (single binary, better concurrency), TypeScript (MCP ecosystem).
- Why: the hard parts (memory, learning, patterns) are Python-ecosystem; agent loops are I/O-bound; Chief is Python-native; Hermes on disk is a free reference.
- Consequences: heavier runtime than Go; future Go migration possible for a thin client.

## D-002: Vault-native canonical memory (2026-08-11, from Master Plan V2)
- Context: where durable memory lives.
- Decision: Obsidian-compatible markdown vault is canonical; SQLite/FTS5/embeddings are derived caches.
- Why: human-auditable, git-trackable, Obsidian-compatible, rebuildable from vault.
- Consequences: heavier writes; atomic notes mandatory; .overseer/ is disposable.

## D-003: No secrets in config.yaml (2026-08-11)
- Context: provider keys.
- Decision: config.yaml holds placeholders; real keys come from OVERSEER_* env vars.
- Why: public repo safety; gitleaks in CI.
- Consequences: doctor checks env var presence; users must set env vars.

## D-004: Redaction from day one (2026-08-11)
- Context: logs, exports, session notes.
- Decision: RedactingFormatter on all overseer loggers; redact() used before any output.
- Why: security is continuous (invariant 2); secrets must never reach logs.
- Consequences: log messages are mutated at format time; args cleared.

## D-005: Idempotent vault init (2026-08-11)
- Context: `overseer init` behavior.
- Decision: init is safe to run twice; second run creates nothing.
- Why: users may re-run init after partial setup; CI validates templates.
- Consequences: init returns created-file list; tests assert idempotency.

## D-006: Atomic note writes (2026-08-11)
- Context: vault note durability.
- Decision: temp file in same dir + os.replace; cleanup on failure.
- Why: no partial notes on crash; plan requires atomic writes.
- Consequences: slightly more I/O; correctness wins.

## D-007: Path containment in vault writer (2026-08-11)
- Context: note paths from titles.
- Decision: _contained() resolves and rejects paths escaping the vault root.
- Why: path traversal defense (invariant 2, bug-hunt protocol).
- Consequences: titles are slugified; traversal attempts raise VaultError.

## D-008: CI pins setup-uv@v9.0.0 (SHA) (2026-08-11)
- Context: GitHub Actions.
- Decision: pin astral-sh/setup-uv to commit c771a70e (v9.0.0); gitleaks-action@v2.
- Why: verified current versions; supply-chain hygiene.
- Consequences: manual bump when upgrading.
