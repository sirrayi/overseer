# Overseer — Security Notes

> Security is continuous (invariant 2). Updated per batch.

## B0 status (2026-08-11)

### In place
- .gitignore excludes: .env*, *.pem/*.key/*.p12, auth.json, credentials/, token.json, secrets/, .overseer/, logs/, artifacts/, caches, .local-docs/ (private machine audit).
- gitleaks secret scan in CI (gitleaks-action@v2).
- pip-audit dependency scan in CI.
- RedactingFormatter on all overseer loggers (secrets never reach logs).
- redact() covers: OpenAI/Anthropic keys, GitHub tokens, AWS keys, Bearer tokens, generic api_key/token/secret/password assignments, private key blocks.
- No secrets in config.yaml — placeholders only; OVERSEER_* env vars for real values.
- Path containment in vault writer (_contained): traversal raises VaultError.
- Doctor checks provider key env var presence (clear failure, no secret exposure).
- Sample config test asserts no secret patterns.

### Planned (later batches)
- Content classification + untrusted-content rules (B1).
- Approval gates for terminal/file tools (B1).
- Prompt injection test suite (B10).
- Memory poisoning tests (B10).
- Secret leakage suite (B10).
- Backup/restore + data governance (B10).

### Known gaps (honest)
- redact() is pattern-based; novel secret formats may slip through until the suite grows.
- No sandboxing yet (B1: subprocess arg lists, timeouts, output caps).
- No approval prompts yet (B1).
