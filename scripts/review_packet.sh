#!/usr/bin/env bash
# review_packet.sh — generate the 9-item review packet for the Qwen review loop
# (plan Part 42.4: review packet format). Run from the repo root.
set -euo pipefail

OUT="${1:-review-packet-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT"

echo "==> generating review packet in $OUT"

# 1. Batch completed (from IMPLEMENTATION_STATUS.md head)
head -40 IMPLEMENTATION_STATUS.md > "$OUT/01-batch-status.md" 2>/dev/null || true

# 2. Current file tree
find . -type f -not -path './.git/*' -not -path './.venv/*' -not -path '*/__pycache__/*' \
  | sort > "$OUT/02-file-tree.txt"

# 3. Changed files (git diff stat + names)
git diff --stat HEAD > "$OUT/03-changed-files.txt" 2>/dev/null || true
git status --short >> "$OUT/03-changed-files.txt" 2>/dev/null || true

# 4. Key source files (agent loop, provider, tool registry, approval gate,
#    memory write path, context compiler, vault parsing, security policy)
for f in src/overseer/*.py; do
  echo "===== $f =====" >> "$OUT/04-key-sources.txt"
  cat "$f" >> "$OUT/04-key-sources.txt"
done

# 5. Test summary
uv run pytest -q 2>&1 | tail -20 > "$OUT/05-test-summary.txt" || true

# 6. Lint/type output
uv run ruff check . 2>&1 | tail -20 > "$OUT/06-lint.txt" || true
uv run mypy src/overseer 2>&1 | tail -20 > "$OUT/06-mypy.txt" || true

# 7. Runtime trace (one real task trace, secrets removed)
if [ -f logs/overseer.log ]; then
  tail -100 logs/overseer.log > "$OUT/07-trace.txt"
else
  echo "no logs yet" > "$OUT/07-trace.txt"
fi

# 8. Known problems (from RISKS.md)
cat RISKS.md > "$OUT/08-known-problems.md" 2>/dev/null || true

# 9. Specific fear (from IMPLEMENTATION_STATUS.md tail)
tail -30 IMPLEMENTATION_STATUS.md > "$OUT/09-specific-fear.md" 2>/dev/null || true

echo "==> done. packet: $OUT"
echo "==> send to Qwen: tree + key sources + test summary + lint + trace + known problems + specific fear"
