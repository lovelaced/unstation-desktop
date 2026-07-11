#!/usr/bin/env bash
# Grant a statement-store WRITE allowance to an account on a local --dev node.
#
# Sets the `:statement_allowance:`++account storage key to SCALE(StatementAllowance{
# max_count: u32, max_size: u32 }) via a sudo `system.setStorage`, signed by Alice (sudo
# on a --dev chain). Uses polkadot-js-api, which reads the node's LIVE metadata — so the
# extrinsic is correct for the kitchensink dev runtime. (The SDK's own testnet auto-
# provision is hardcoded for the production "Paseo People Next" runtime and does NOT match
# a local kitchensink node, which is why we provision out-of-band here.)
#
# Usage: scripts/provision-allowance.sh [account_hex] [max_count] [max_size]
#   account_hex defaults to the chain_e2e test account (sr25519 pubkey of seed [11u8;32]).
# Env: NODE_WS (default ws://127.0.0.1:9944).
set -euo pipefail

NODE_WS="${NODE_WS:-ws://127.0.0.1:9944}"
# Default = sr25519 pubkey of crypto::keypair_from_seed([11u8;32]) — the chain_e2e identity.
ACCT="${1:-00a590445cf3222978070c4392b6add324208303aa18c0c1f84388739a1a8267}"
ACCT="${ACCT#0x}"
COUNT="${2:-100000}"
SIZE="${3:-104857600}" # 100 MiB

PREFIX_HEX="$(printf ':statement_allowance:' | xxd -p | tr -d '\n')"
KEY="0x${PREFIX_HEX}${ACCT}"
le32() { local v=$1; printf '%02x%02x%02x%02x' $((v & 255)) $(((v >> 8) & 255)) $(((v >> 16) & 255)) $(((v >> 24) & 255)); }
VALUE="0x$(le32 "$COUNT")$(le32 "$SIZE")"

echo "[provision] granting statement allowance to 0x${ACCT} (count=${COUNT}, size=${SIZE}) on ${NODE_WS}"
# Retry: concurrent provisioners (parallel e2e tests) race on Alice's nonce — the
# loser's extrinsic dies with "1014: Priority is too low". A short backoff and a
# fresh nonce fetch resolve it; anything still failing after 3 tries is real.
for attempt in 1 2 3; do
  if npx --yes @polkadot/api-cli@latest --ws "$NODE_WS" --sudo --seed "//Alice" \
       tx.system.setStorage "[[\"$KEY\",\"$VALUE\"]]"; then
    echo "[provision] done"
    exit 0
  fi
  echo "[provision] attempt ${attempt} failed (nonce race with a parallel provisioner?) — retrying" >&2
  sleep $((attempt * 2))
done
echo "[provision] failed after 3 attempts" >&2
exit 1
