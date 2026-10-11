#!/usr/bin/env bash
# Run the shared API suite and the local Taskboard journeys serially. Each
# restores the reference fixture in its own temporary database and cleans up
# its processes; no configured server, database or real provider is used.
set -euo pipefail

integration_root=$(cd "$(dirname "$0")/.." && pwd)
export CARGO_INCREMENTAL=0

"$integration_root/scripts/contract-backend.sh"
cd "$integration_root/server"
cargo test --locked -p zupflow-contract --features zupflow-contract/local-apps \
  --test local_apps --test taskboard_end_to_end
"$integration_root/scripts/connect-browser.sh"
