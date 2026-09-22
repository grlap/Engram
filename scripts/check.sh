#!/usr/bin/env sh
set -eu

# Full repository quality gate.
repo_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd)
cd "$repo_root"

exec node scripts/test-launcher.mjs full "$@"
