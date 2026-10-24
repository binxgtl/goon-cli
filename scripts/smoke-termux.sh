#!/usr/bin/env bash
# Termux smoke test (v0.5, P15). Run manually on a device:
#   bash scripts/smoke-termux.sh              # install dependencies, then build and test
#   bash scripts/smoke-termux.sh --no-install # reuse an existing node_modules
#
# This is not part of CI; it exercises the Termux-first paths that need a real Android
# environment (process groups, POSIX modes, no native addons).
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "${1:-}" != "--no-install" ]; then
  npm ci --ignore-scripts --no-audit --no-fund
fi
npm run build
node --test tests/eventlog.test.mjs tests/stub-provider.test.mjs tests/golden.test.mjs
