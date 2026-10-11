#!/usr/bin/env bash
# One shared build, then fresh loopback processes and databases for each case.
set -euo pipefail

browser_root=$(cd "$(dirname "$0")/.." && pwd)
export CARGO_INCREMENTAL=0
cd "$browser_root/server"
cargo build --locked -p zupflow-contract -p zupflow-taskboard \
  --features zupflow-contract/local-apps --target-dir "$browser_root/server/target"
cd "$browser_root"
exec node packages/contract-tests/browser/run.mjs "$@"
