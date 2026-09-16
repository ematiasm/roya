#!/usr/bin/env bash
# Regenerate static/tailwind.css from assets/tailwind.css.
#
# The Tailwind CSS v4 standalone CLI is a dev-time tool only: the generated
# file is committed so `cargo run` works without it installed.
#
# Usage:
#   scripts/build-css.sh
#   TAILWINDCSS=/path/to/tailwindcss scripts/build-css.sh
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TAILWINDCSS="${TAILWINDCSS:-tailwindcss}"

exec "$TAILWINDCSS" \
  --input assets/tailwind.css \
  --output static/tailwind.css \
  --minify
