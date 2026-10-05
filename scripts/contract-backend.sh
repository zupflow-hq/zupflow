#!/usr/bin/env bash
# Runs the shared contract suite against the backend.
#
# Builds the development-only contract server, restores the reference fixture
# into a fresh database, serves it on a loopback port the system picks, and
# points the suite at it. Cases that need an operation the backend doesn't
# serve yet are skipped by name. Never uses a deployed server or a real
# database.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
dir=$(mktemp -d)
server=""
trap '[ -n "$server" ] && kill "$server" 2>/dev/null; rm -rf "$dir"' EXIT

(cd "$root/server" && cargo build --locked --quiet -p zupflow-contract)
bin="$root/server/target/debug/zupflow-contract-server"
database="$dir/contract.sqlite"
"$bin" reset "$database"

# The server announces the port it got, so nothing else that holds a port can
# stand in for it.
"$bin" serve "$database" >"$dir/serve.log" &
server=$!
port=""
for _ in $(seq 1 100); do
  port=$(sed -n 's/^listening \([0-9][0-9]*\)$/\1/p' "$dir/serve.log")
  [ -n "$port" ] && break
  kill -0 "$server" 2>/dev/null || break
  sleep 0.1
done
if [ -z "$port" ]; then
  echo "the contract server didn't start" >&2
  exit 1
fi

export ZUPFLOW_CONTRACT_URL="http://127.0.0.1:$port"
export ZUPFLOW_CONTRACT_RESET_COMMAND="'$bin' reset '$database'"
export ZUPFLOW_CONTRACT_OPERATIONS="$("$bin" operations)"
export ZUPFLOW_CONTRACT_TOKEN_OWNER="contract:owner"
export ZUPFLOW_CONTRACT_TOKEN_DEVELOPER="contract:developer"
export ZUPFLOW_CONTRACT_TOKEN_VIEWER="contract:viewer"
export ZUPFLOW_CONTRACT_TOKEN_OUTSIDER="contract:outsider"

cd "$root" && pnpm --filter @zupflow/contract-tests exec vitest run src/remote.test.ts "$@"
