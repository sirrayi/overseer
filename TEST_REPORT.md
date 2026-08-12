# Overseer — Test Report

> Updated after every test run (plan: tests meaningful, no coverage worship).

## B0 (2026-08-11)

### Test files
- tests/test_config.py — config validation, env overrides, failure clarity, sample config safety
- tests/test_redact.py — secret redaction patterns
- tests/test_vault.py — init layout, idempotency, guardrails content, frontmatter, containment, listing, slugify
- tests/test_doctor.py — doctor OK/fail paths, provider key check
- tests/test_cli.py — CLI smoke: version, init, doctor
- tests/golden_tasks.py — golden task fixtures (6 tasks)

### Run command
uv run pytest -q

### Result
PENDING — first run after uv sync.

### Coverage focus (critical paths only)
- config validation, redaction, vault writes, doctor failures, CLI smoke.

## B7 (2026-08-12)

### Test files
- tests/test_miner.py — evidence floor (n<3 rejected), success-threshold gating, high-risk manual promotion, correction replay blocking, frontmatter validation, roundtrip persistence, CLI integration

### Result
259 passed in ~1s (up from 242 — +17 B7 tests). All seven gates green:
ruff check, ruff format, mypy strict, pytest, bandit, gitleaks, pip-audit.

### Coverage focus (critical paths only)
- Evidence gates (never mine unverified / under-evidenced), high-risk approval, correction replay, skill frontmatter governance.
