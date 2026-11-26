#!/usr/bin/env bash
# Keyless interoperability checks between rig-a2a, Strands Agents and the
# official A2A Python SDK. Every agent is deterministic: no model, no API key.
#
#   cargo build --examples
#   STRANDS_PY=/path/to/strands/venv/bin/python A2A_V1_PY=/path/to/a2a/venv/bin/python interop/run.sh
#
# The two Python interpreters need different environments: Strands 1.x pins
# a2a-sdk<0.4 (A2A v0.3), the v1.0 check needs a2a-sdk 1.x. See the
# requirements-*.txt files next to this script.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target/debug/examples
STRANDS_PY=${STRANDS_PY:-python3}
A2A_V1_PY=${A2A_V1_PY:-python3}
TOKEN=interop-token
RIG=http://127.0.0.1:3300
STRANDS=http://127.0.0.1:9300
STRANDS_COMPLIANT=http://127.0.0.1:9301

trap 'kill $(jobs -p) 2>/dev/null || true' EXIT
PORT=3300 A2A_TOKEN=$TOKEN "$BIN/echo_server" &
PORT=9300 "$STRANDS_PY" -I interop/strands_echo_server.py 2>/dev/null &
PORT=9301 COMPLIANT=1 "$STRANDS_PY" -I interop/strands_echo_server.py &

wait_for() {
  for _ in $(seq 120); do
    curl -sf "$1/.well-known/agent-card.json" >/dev/null && return
    sleep 0.5
  done
  echo "no agent card at $1" >&2
  exit 1
}
wait_for "$RIG"
wait_for "$STRANDS"
wait_for "$STRANDS_COMPLIANT"

check() { echo "== $1"; shift; "$@"; }
refused() {
  echo "== $1"
  shift
  if "$@" >/dev/null 2>&1; then
    echo "answered without a token" >&2
    exit 1
  fi
}

check "rig -> Strands (A2A v0.3, Strands' default streaming)" \
  "$BIN/call" "$STRANDS" "hola rig" --expect "echo: hola rig"
check "rig -> Strands (A2A v0.3, spec-compliant streaming)" \
  "$BIN/call" "$STRANDS_COMPLIANT" "hola rig" --expect "echo: hola rig"
check "Strands -> rig (A2A v0.3, bearer)" \
  "$STRANDS_PY" -I interop/strands_client.py "$RIG/v03" hola pong "$TOKEN"
check "a2a-sdk 1.x -> rig (A2A v1.0, streaming, bearer)" \
  "$A2A_V1_PY" -I interop/a2a_v1_client.py "$RIG" hola pong "$TOKEN"
check "rig -> rig (A2A v1.0)" "$BIN/call" "$RIG" hola --token "$TOKEN" --expect pong
check "rig -> rig (A2A v0.3)" "$BIN/call" "$RIG/v03" hola --token "$TOKEN" --expect pong
refused "Strands without the token is refused" \
  "$STRANDS_PY" -I interop/strands_client.py "$RIG/v03" hola pong
refused "a2a-sdk 1.x without the token is refused" \
  "$A2A_V1_PY" -I interop/a2a_v1_client.py "$RIG" hola pong

echo "all interop checks passed"
