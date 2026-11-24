#!/usr/bin/env bash
# Keyless interoperability checks: the official MCP Python SDK against the
# echo example over stdio and over Streamable HTTP (with and without the token).
#
#   cargo build --examples
#   MCP_PY=/path/to/venv/bin/python interop/run.sh
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target/debug/examples/echo_server
MCP_PY=${MCP_PY:-python3}
TOKEN=interop-token
URL=http://127.0.0.1:3400/mcp

"$MCP_PY" -I interop/mcp_client.py stdio "$BIN"

trap 'kill $(jobs -p) 2>/dev/null || true' EXIT
PORT=3400 MCP_TOKEN=$TOKEN "$BIN" http &
for _ in $(seq 120); do
  status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL" || true)
  [ "$status" != "000" ] && break
  sleep 0.5
done

"$MCP_PY" -I interop/mcp_client.py http "$URL" "$TOKEN"
echo "== a call without the token is refused"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}')
[ "$status" = 401 ] || { echo "expected 401, got $status" >&2; exit 1; }

echo "all interop checks passed"
