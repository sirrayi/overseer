#!/usr/bin/env bash
# Build overseer-roadmap.pdf from src/*.md.
set -euo pipefail

cd "$(dirname "$0")"
mkdir -p build

for f in src/*.md; do
  cat "$f"
  printf '\n'
done > build/roadmap.md

pandoc build/roadmap.md \
  --from markdown+pipe_tables+fenced_divs+yaml_metadata_block \
  --to typst --standalone --wrap=none \
  --template template.typ \
  --metadata-file src/00-meta.yaml \
  --lua-filter filters/tables.lua \
  --lua-filter filters/callouts.lua \
  --output build/roadmap.typ

typst compile --root . build/roadmap.typ overseer-roadmap.pdf

pdfinfo overseer-roadmap.pdf | awk '/^Pages/ { print "Pages: " $2 }'
