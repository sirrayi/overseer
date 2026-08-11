#!/usr/bin/env bash
# review_packet.sh — generate the strict-review packet for the Qwen review loop
# (plan Part 42.4). Emits VERBATIM content from git (git show), so the packet
# always matches the committed state — never the working tree.
#
# Usage: ./scripts/review_packet.sh [output_dir]   (default: ./review-packet)
set -euo pipefail

OUT="${1:-review-packet}"
mkdir -p "$OUT"
echo "==> generating review packet in $OUT"

# 00 — instructions for the reviewer (static)
cat > "$OUT/00-instructions.md" <<'EOF'
You are the strict reviewer for OVERSEER, a vault-native, self-improving,
verification-driven agent harness. You previously authored the master plan
(41 parts + build doctrine + efficiency governor + live learning engine).

This is a batch review packet. Review it against:
1. The batch's done-when criteria (from the master plan).
2. The non-negotiable architecture invariants (vault canonical, security
   continuous, learning from verified truth, memory governed, context
   compiled, efficiency mandatory, live learning safe, self-modification
   proposal-only, user final authority, simplicity).
3. The bug-hunt protocol (race conditions, path traversal, command
   injection, secret leakage, prompt injection, memory poisoning, cost
   runaway, infinite loops, broken resume, invalid frontmatter, etc.).

All file contents below are emitted verbatim from git (git show HEAD:path),
so they exactly match the committed state. If something looks mangled, it is
a real defect in the committed code — flag it.

Report: verdict (APPROVED / NEEDS FIXES), findings with severity
(critical/major/minor), concrete fixes, and any done-when criteria that are
unmet. Be strict. No praise. Findings only.
EOF

# 01 — batch status
{
  echo "# Batch Status"
  echo
  echo "Repo: https://github.com/sirrayi/overseer"
  echo "Branch: main"
  echo "HEAD: $(git rev-parse --short HEAD)"
  echo "Commit: $(git log -1 --format='%s')"
  echo "CI: $(gh run list --limit 1 --json status,conclusion,displayTitle --jq '.[0] | .status + " / " + (.conclusion // "pending") + " — " + .displayTitle' 2>/dev/null || echo 'gh unavailable')"
  echo
  echo "## Done-when evidence"
  echo "- repo public-safe: see 03 (committed tree) + 10 (gitleaks) + 11 (pip-audit)"
  echo "- README L3 guardrail: see 04"
  echo "- .gitignore: see 05"
  echo "- clean uv scaffold: see 06 (pyproject) + 12 (tests)"
  echo "- config + env overrides + validation: see 07 + 12"
  echo "- logging safe dir + redaction: see 08 + 12"
  echo "- vault generator idempotent/atomic/stable IDs: see 09 + 12"
  echo "- doctor validates config/vault/provider/permissions: see 09 + 12"
  echo "- eval skeleton: see 12 (golden tasks)"
  echo "- CI green: see 13"
  echo "- review_packet verbatim: see 14 (this script)"
} > "$OUT/01-batch-status.md"

# 02 — committed file tree (git ls-files, not filesystem)
git ls-files | sort > "$OUT/02-committed-tree.txt"
echo "committed files: $(git ls-files | wc -l | tr -d ' ')" >> "$OUT/02-committed-tree.txt"

# 03 — changed files since last tag / all commits
git log --oneline -10 > "$OUT/03-commits.txt" 2>/dev/null || true
git diff --stat HEAD~1 HEAD 2>/dev/null >> "$OUT/03-commits.txt" || true

# 04 — README (first 80 lines)
git show HEAD:README.md 2>/dev/null | head -80 > "$OUT/04-readme.txt" || echo "MISSING README.md" > "$OUT/04-readme.txt"

# 05 — .gitignore (full)
git show HEAD:.gitignore 2>/dev/null > "$OUT/05-gitignore.txt" || echo "MISSING .gitignore" > "$OUT/05-gitignore.txt"

# 06 — pyproject.toml (full)
git show HEAD:pyproject.toml 2>/dev/null > "$OUT/06-pyproject.toml" || echo "MISSING pyproject.toml" > "$OUT/06-pyproject.toml"

# 07 — key source files (verbatim from git)
for f in src/overseer/__init__.py src/overseer/__main__.py src/overseer/cli.py \
         src/overseer/config.py src/overseer/errors.py src/overseer/logging_setup.py \
         src/overseer/redact.py src/overseer/vault.py src/overseer/doctor.py; do
  {
    echo "===== $f ====="
    git show "HEAD:$f" 2>/dev/null || echo "MISSING $f"
    echo
  } >> "$OUT/07-key-sources.txt"
done

# 08 — tests (all test files verbatim)
for f in $(git ls-files 'tests/*.py'); do
  {
    echo "===== $f ====="
    git show "HEAD:$f" 2>/dev/null || echo "MISSING $f"
    echo
  } >> "$OUT/08-tests.txt"
done

# 09 — CI workflow (full)
git show HEAD:.github/workflows/ci.yml 2>/dev/null > "$OUT/09-ci.yml" || echo "MISSING ci.yml" > "$OUT/09-ci.yml"

# 10 — gitleaks local output
if command -v gitleaks >/dev/null 2>&1; then
  gitleaks git --redact -v 2>&1 | tail -20 > "$OUT/10-gitleaks.txt" || true
else
  echo "gitleaks not installed locally; CI runs it (see 09 + 13)" > "$OUT/10-gitleaks.txt"
fi

# 11 — pip-audit local output
if command -v pip-audit >/dev/null 2>&1; then
  uv export --no-dev 2>/dev/null | pip-audit -r /dev/stdin 2>&1 | tail -20 > "$OUT/11-pip-audit.txt" || true
else
  echo "pip-audit not installed locally; CI runs it (see 09 + 13)" > "$OUT/11-pip-audit.txt"
fi

# 12 — test + lint + type evidence (fresh run)
{
  echo "===== pytest (with names) ====="
  uv run pytest -v 2>&1 | tail -60
  echo
  echo "===== coverage ====="
  uv run pytest --cov=overseer --cov-report=term-missing 2>&1 | tail -25
  echo
  echo "===== ruff ====="
  uv run ruff check . 2>&1 | tail -5
  echo
  echo "===== ruff format ====="
  uv run ruff format --check . 2>&1 | tail -3
  echo
  echo "===== mypy ====="
  uv run mypy src/overseer 2>&1 | tail -5
  echo
  echo "===== bandit ====="
  uv run bandit -r src/overseer 2>&1 | tail -8
} > "$OUT/12-evidence.txt"

# 13 — CI run URL
gh run list --limit 1 --json databaseId,status,conclusion,displayTitle --jq '"https://github.com/sirrayi/overseer/actions/runs/" + .[0].databaseId + " — " + .[0].status + " / " + (.0.conclusion // "pending")' 2>/dev/null > "$OUT/13-ci-url.txt" || echo "gh unavailable" > "$OUT/13-ci-url.txt"

# 14 — this script (verbatim)
git show HEAD:scripts/review_packet.sh 2>/dev/null > "$OUT/14-review-packet.sh" || cp "$0" "$OUT/14-review-packet.sh"

# 15 — known problems + specific fear (from repo docs)
{
  echo "===== known problems (RISKS.md) ====="
  git show HEAD:RISKS.md 2>/dev/null | head -40 || echo "MISSING RISKS.md"
  echo
  echo "===== specific fear (IMPLEMENTATION_STATUS.md tail) ====="
  git show HEAD:IMPLEMENTATION_STATUS.md 2>/dev/null | tail -30 || echo "MISSING IMPLEMENTATION_STATUS.md"
} > "$OUT/15-known-and-fear.txt"

# 16 — combined single-file packet for easy copy-paste
{
  for f in 00-instructions.md 01-batch-status.md 02-committed-tree.txt 03-commits.txt \
           04-readme.txt 05-gitignore.txt 06-pyproject.toml 07-key-sources.txt \
           08-tests.txt 09-ci.yml 10-gitleaks.txt 11-pip-audit.txt 12-evidence.txt \
           13-ci-url.txt 14-review-packet.sh 15-known-and-fear.txt; do
    echo
    echo "################################################################"
    echo "### $f"
    echo "################################################################"
    echo
    cat "$OUT/$f"
  done
} > "$OUT/qwen-review-packet.md"

echo "==> done. packet: $OUT"
echo "==> single-file paste: $OUT/qwen-review-packet.md ($(wc -c < "$OUT/qwen-review-packet.md" | tr -d ' ') bytes)"
