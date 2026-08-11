# Overseer — Risk Register

> Living risk list (plan Part 9: risks with mitigations). Updated per batch.

| # | Risk | Likelihood | Impact | Mitigation | Status |
|---|---|---|---|---|---|
| R-01 | Secret leakage into repo/logs/exports | Medium | Critical | .gitignore, gitleaks CI, RedactingFormatter, redact() in exports, no secrets in config | Active |
| R-02 | Prompt injection via untrusted content | High | High | Content classification (B1+), untrusted content cannot instruct, injection test suite (B10) | Planned |
| R-03 | Memory poisoning (false memories) | Medium | High | Confidence gating, untrusted content low-confidence only, evidence links, user inspection | Planned |
| R-04 | Learning from unverified outcomes | Medium | High | Verification engine before recursive learning (B4 before B5), verified-truth rule | Planned |
| R-05 | Vault bloat / giant notes | Medium | Medium | Atomic notes, summaries in vault, raw logs in .overseer, prune engine | Planned |
| R-06 | Cost runaway | Medium | High | Token/cost budgets, escalation limits, power modes, telemetry | Planned |
| R-07 | Complexity creep | High | Medium | Batch discipline, review packets, simplicity invariant, Part 42 doctrine | Active |
| R-08 | SQLite locking / corruption | Low | Medium | WAL mode (B3), locking tests, derived-cache rebuild from vault | Planned |
| R-09 | Silent self-modification | Low | Critical | L3 proposal-only, human approval, audit log, Guardrails.md | Active (guardrail documented) |
| R-10 | Path traversal in vault writer | Low | High | _contained() containment check, tests | Mitigated (B0) |
| R-11 | Malformed tool calls crash loop | Medium | Medium | Structured parsing + repair, error taxonomy (B1) | Planned |
| R-12 | Adapter training on battery | Low | Medium | Power-aware training gate (B8) | Planned |
| R-13 | Public release with broken quickstart | Medium | Medium | Golden tasks, docs, CI, review packets | Planned |
