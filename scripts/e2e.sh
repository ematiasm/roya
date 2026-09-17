#!/usr/bin/env bash
#
# The one command that runs the browser suite.
#
# It needs no Node toolchain: Python is managed by uv and the browser is
# Playwright's Chromium (see e2e/README.md for the one-time download). The Rust
# binary is built once by the suite's own fixtures, then spawned directly, so
# any extra arguments are passed straight to pytest:
#
#   scripts/e2e.sh                 # the whole suite, headless
#   scripts/e2e.sh -k scan         # one test
#   scripts/e2e.sh --headed        # debug a failure with a visible browser
#
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"

cd "${repo_root}/e2e"
exec uv run pytest "$@"
