#!/usr/bin/env bash
# Development only: runs Taskboard, the fake MCP app with its own OAuth,
# beside the development contract server, so you can connect it from the
# Console through hosted Connect and run an agent with its tools.
#
# Builds both, restores the reference fixture into a throwaway database,
# adds Taskboard live to Northwind's catalog (ready in Sandbox), and serves the API
# for a Console at $CONSOLE_ORIGIN (default http://localhost:5181) until you
# press Ctrl-C. Nothing leaves the machine: the backend reaches
# taskboard.example at 127.0.0.1 and nothing else.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
console_origin=${CONSOLE_ORIGIN:-http://localhost:5181}
taskboard_port=${TASKBOARD_PORT:-7443}
dir=$(mktemp -d)
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  rm -rf "$dir"
}
trap cleanup EXIT

(cd "$root/server" && cargo build --locked --quiet -p zupflow-contract -p zupflow-taskboard --features zupflow-contract/local-apps)
bin="$root/server/target/debug"

# Waits for a process to print `listening <port>`; the port.
port_of() {
  local log=$1 pid=$2 port=""
  for _ in $(seq 1 100); do
    port=$(sed -n 's/^listening \([0-9][0-9]*\)$/\1/p' "$log")
    [ -n "$port" ] && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
  [ -n "$port" ] || { cat "$log" >&2; return 1; }
  echo "$port"
}

"$bin/taskboard" --port "$taskboard_port" --certificate "$dir/taskboard.pem" >"$dir/taskboard.log" 2>&1 &
pids+=($!)
port_of "$dir/taskboard.log" "${pids[0]}" >/dev/null
taskboard="https://taskboard.example:$taskboard_port"

database="$dir/contract.sqlite"
"$bin/zupflow-contract-server" reset "$database"
"$bin/zupflow-contract-server" serve "$database" --console "$console_origin" \
  --local-app "taskboard.example:$taskboard_port=$dir/taskboard.pem" >"$dir/serve.log" 2>"$dir/serve.err" &
pids+=($!)
port=$(port_of "$dir/serve.log" "${pids[1]}")
api="http://127.0.0.1:$port"

# A request without a token is Northwind's owner, as with `local` sign-in.
call() {
  curl -fsS -X "$1" "$api$2" -H 'Content-Type: application/json' ${3:+--data "$3"}
}
json() {
  node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const v=JSON.parse(s);console.log(new Function("v", "return " + process.argv[1])(v))})' "$1"
}
org=$(call GET /v1/me | json 'v.data.organizations.find((o) => o.slug === "northwind").id')
app=$(call POST "/v1/organizations/$org/apps" \
  '{"kind":"mcp","name":"Taskboard live","slug":"taskboard-live","description":"A fake task app for development.","auth_type":"mcp_oauth"}' |
  json 'v.data.id')
call PATCH "/v1/organizations/$org/apps/$app" "{\"auth\":{\"type\":\"mcp_oauth\",\"scopes\":[{\"name\":\"tasks:read\",\"description\":\"See tasks\"},{\"name\":\"tasks:write\",\"description\":\"Change tasks\"}],\"identity_url\":\"$taskboard/me\"}}" >/dev/null
call PUT "/v1/organizations/$org/apps/$app/environments/sandbox/configuration" \
  "{\"mcp_server_url\":\"$taskboard/mcp\"}" >/dev/null

spki=""
if command -v openssl >/dev/null; then
  spki=$(openssl x509 -pubkey -noout -in "$dir/taskboard.pem" | openssl pkey -pubin -outform der |
    openssl dgst -sha256 -binary | base64)
fi

cat <<EOF

Taskboard  $taskboard   (its home page expires or revokes tokens)
API        $api   (Northwind, with Taskboard live ready in Sandbox)

Choose Taskboard live when connecting; Taskboard is the static reference fixture.

Start the Console against it, in another terminal:

  PORT=${console_origin##*:} VITE_ZUPFLOW_API_URL=$api pnpm dev

Your browser must reach taskboard.example at 127.0.0.1 and accept its
certificate, made at start for that name alone. With Chrome or Chromium, use a
profile of its own:

  chromium --user-data-dir="\$(mktemp -d)" \\
    --host-resolver-rules="MAP taskboard.example 127.0.0.1" \\
    ${spki:+--ignore-certificate-errors-spki-list=$spki }$console_origin

Or add "127.0.0.1 taskboard.example" to /etc/hosts and accept the certificate
warning once. Runs are answered by the scripted model: a line
"call list_tasks {}" or "call create_task {\"title\":\"Hello\"}" calls a tool.

Press Ctrl-C to stop.
EOF
wait "${pids[1]}"
