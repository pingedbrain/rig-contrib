#!/usr/bin/env bash
# Runs every keyless example once and fails if any step exits non-zero.
# No model is called: model keys and model ids are removed from the environment.
# Usage: ci/examples.sh   (from anywhere; needs curl and timeout)
set -euo pipefail

cd "$(dirname "$0")/.."

# Nothing may go live: drop model keys and model selectors from the environment.
while IFS= read -r line; do
  name=${line%%=*}
  case $name in
    *_API_KEY | *_KEY | RIG_MODEL | OPENROUTER_MODEL | MCP_TOKEN | A2A_TOKEN) unset "$name" ;;
  esac
done < <(env)

TARGET=${CARGO_TARGET_DIR:-target}
BIN="$TARGET/debug/examples"
WORK=$(mktemp -d)
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '\n== %s\n' "$1"; }
ok() { printf 'ok: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }

# Start a server in the background and remember its PID for cleanup.
serve() {
  local name=$1
  shift
  "$@" >"$WORK/$name.log" 2>&1 &
  pids+=("$!")
  SERVER_PID=$!
}

# Poll a URL until it answers, for at most 30 seconds.
wait_for() {
  local url=$1 i
  for i in $(seq 1 30); do
    if curl -fsS -m 2 "$url" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  cat "$WORK"/*.log >&2 || true
  fail "no answer from $url"
}

stop() {
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
}

step "build every example once"
timeout 900 cargo build --workspace --examples --all-features

step "rig-a2a remote_tool (in-process A2A server, scripted models)"
timeout 120 "$BIN/remote_tool" >"$WORK/remote_tool.out"
grep -q "According to the Harbor Library desk" "$WORK/remote_tool.out" || fail "remote_tool reply"
ok "remote_tool"

step "rig-a2a a2a_echo_server + call"
PORT=3300 serve a2a_echo "$BIN/a2a_echo_server"
wait_for http://127.0.0.1:3300/.well-known/agent-card.json
timeout 60 "$BIN/call" http://127.0.0.1:3300 "Hi" --expect pong >/dev/null
stop
ok "a2a_echo_server answers pong"

step "rig-evals evals"
timeout 300 "$BIN/evals" >/dev/null
ok "evals"

step "rig-multiagent swarm_graph"
timeout 120 "$BIN/swarm_graph" >/dev/null
ok "swarm_graph"

step "rig-otel console"
timeout 120 "$BIN/console" >/dev/null
ok "console"

step "rig-agentcore serve (GET /ping, POST /invocations)"
PORT=3201 serve agentcore "$BIN/serve"
wait_for http://127.0.0.1:3201/ping
curl -fsS -m 10 http://127.0.0.1:3201/ping | grep -q '"Healthy"' || fail "ping is not Healthy"
curl -fsS -m 30 http://127.0.0.1:3201/invocations \
  -H 'content-type: application/json' -d '{"prompt":"Hi"}' | grep -q 'Scripted reply' \
  || fail "invocations reply"
stop
ok "serve answers /ping and /invocations"

step "rig-mcp-server mcp_echo_server http (initialize over Streamable HTTP)"
env -u MCP_TOKEN PORT=3400 "$BIN/mcp_echo_server" http >"$WORK/mcp_http.log" 2>&1 &
SERVER_PID=$!
pids+=("$SERVER_PID")
init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"ci","version":"0"}}}'
for i in $(seq 1 30); do
  curl -fsS -m 2 -X POST http://127.0.0.1:3400/mcp \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -d "$init" >"$WORK/mcp_init.out" 2>/dev/null && break
  sleep 1
done
grep -q '"protocolVersion"' "$WORK/mcp_init.out" || fail "mcp_echo_server http initialize"
stop
ok "mcp_echo_server http initialize"

# Stdio servers: send initialize, the initialized notification and tools/list, then
# close stdin. The server must answer both requests and exit 0.
stdio_check() {
  local name=$1 expect=$2
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"ci","version":"0"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' |
    timeout 30 "$BIN/$name" >"$WORK/$name.stdio" || fail "$name stdio exit"
  grep -q '"protocolVersion"' "$WORK/$name.stdio" || fail "$name initialize reply"
  grep -q "\"name\":\"$expect\"" "$WORK/$name.stdio" || fail "$name tools/list reply lacks $expect"
}

step "rig-mcp-server typed_tool (stdio)"
stdio_check typed_tool convert_temperature
ok "typed_tool stdio initialize and tools/list"

step "rig-mcp-server mcp_echo_server (stdio)"
stdio_check mcp_echo_server echo
ok "mcp_echo_server stdio initialize and tools/list"

step "all keyless examples passed"
