#!/usr/bin/env bash
# Block until a Substrate/Polkadot node answers JSON-RPC on $RPC_URL, then exit 0.
# Probes `system_health` (returns a result even on a --dev node with 0 peers — do NOT
# gate on peer count). Times out after $TIMEOUT seconds.
set -euo pipefail

RPC_URL="${RPC_URL:-http://127.0.0.1:9944}"
TIMEOUT="${TIMEOUT:-180}"
start=$(date +%s)

while true; do
  if resp=$(curl -fsS -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"system_health","params":[],"id":1}' \
        "$RPC_URL" 2>/dev/null) && printf '%s' "$resp" | grep -q '"result"'; then
    echo "RPC up at $RPC_URL: $resp"
    exit 0
  fi
  if (( $(date +%s) - start >= TIMEOUT )); then
    echo "RPC at $RPC_URL not ready within ${TIMEOUT}s" >&2
    exit 1
  fi
  sleep 2
done
