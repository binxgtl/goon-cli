#!/usr/bin/env bash
# Termux smoke test. Run manually on a device:
#   bash scripts/smoke-termux.sh            # test the binary bin/goon.mjs resolves (GOON_BINARY,
#                                           # an installed vendor/ binary or a local cargo build)
#   bash scripts/smoke-termux.sh --build    # build from source first (pkg install rust)
#
# This is not part of CI; it exercises the Termux paths that need a real Android environment
# (process groups, POSIX modes, the Termux shell and TMPDIR).
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "${1:-}" = "--build" ]; then
  cargo build --release --locked
fi
if [ -z "${GOON_BINARY:-}" ] && [ ! -x target/release/goon ] && [ ! -x target/debug/goon ]; then
  echo "No goon binary: run with --build, or set GOON_BINARY to an installed goon." >&2
  exit 1
fi
npm ci --ignore-scripts --no-audit --no-fund
node bin/goon.mjs --version
node --test tests/stub-provider.test.mjs tests/golden.test.mjs "tests/e2e/*.test.mjs"
