#!/usr/bin/env bash
# Build + run a local statement-store dev chain for the chain_e2e tests.
#
# Uses the polkadot-sdk "kitchensink" `substrate-node`, which wires `pallet-statement` +
# the statement RPC + Alice = sudo (so the `testnet-provisioning` auto-provision in
# `init_statement_store` actually grants the test key a `:statement_allowance:` entry).
# There is NO prebuilt kitchensink binary, so the first build is ~1h; the binary is cached
# under $SDK_DIR/target/release afterwards and starts in seconds.
#
# The chosen ref must be recent enough to include the current statement-store API
# (statement_submit + statement_subscribeStatement, allowance-as-storage-key — landed
# early 2026). Pin a stable tag for reproducible CI caching; master always has the latest.
#
# Usage:
#   scripts/dev-chain.sh build   # clone (pinned) + build substrate-node (slow, once)
#   scripts/dev-chain.sh run     # run it: --dev, offchain indexing on, RPC at :9944
#   scripts/dev-chain.sh         # build if needed, then run
#
# Env: POLKADOT_SDK_REF (branch/tag, default master), POLKADOT_SDK_DIR (checkout dir).
set -euo pipefail

SDK_REF="${POLKADOT_SDK_REF:-master}"
SDK_DIR="${POLKADOT_SDK_DIR:-$HOME/.cache/unstation/polkadot-sdk}"
BIN="$SDK_DIR/target/release/substrate-node"

build() {
  if [ ! -d "$SDK_DIR/.git" ]; then
    echo "[dev-chain] cloning polkadot-sdk @ $SDK_REF (shallow) into $SDK_DIR …"
    git clone --depth 1 --branch "$SDK_REF" https://github.com/paritytech/polkadot-sdk "$SDK_DIR"
  fi
  echo "[dev-chain] building substrate-node (first build ~1h; cached afterwards) …"
  ( cd "$SDK_DIR" && cargo build --release --bin substrate-node )
  echo "[dev-chain] built: $BIN"
}

run() {
  if [ ! -x "$BIN" ]; then
    echo "[dev-chain] node not built yet — run: $0 build" >&2
    exit 1
  fi
  echo "[dev-chain] running substrate-node --dev (Alice = sudo); RPC at ws://127.0.0.1:9944"
  exec "$BIN" \
    --dev \
    --enable-offchain-indexing true \
    --rpc-port 9944 \
    --rpc-cors all \
    --rpc-external
}

case "${1:-all}" in
  build) build ;;
  run) run ;;
  all)
    [ -x "$BIN" ] || build
    run
    ;;
  *)
    echo "usage: $0 [build|run]" >&2
    exit 1
    ;;
esac
