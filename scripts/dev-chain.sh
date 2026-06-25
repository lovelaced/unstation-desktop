#!/usr/bin/env bash
# Build + run a local statement-store dev chain for the chain_e2e tests.
#
# Uses the polkadot-sdk "kitchensink" `substrate-node`, which wires `pallet-statement` +
# the statement RPC + Alice = sudo UNCONDITIONALLY (no --enable-statement-store flag — that
# is the omni-node-only path), so the `testnet-provisioning` auto-provision in
# `init_statement_store` actually grants the test key a `:statement_allowance:` entry.
#
# There is NO prebuilt kitchensink binary anywhere (no Docker image, no `cargo install` —
# crates.io `staging-node-cli` is a reserved 0.0.0 placeholder; no arm64 image serves the
# current statement store), so the first build is ~1h cold even on Apple Silicon; the binary
# is then cached under $SDK_DIR/target/release and starts in seconds. (The only faster
# rebuild loop is omni-node + a hand-authored pallet-statement parachain runtime — hours of
# substrate work; not worth it for a one-time build.)
#
# The chosen ref must include the current statement-store RPC (statement_submit +
# statement_subscribeStatement) — landed Feb 2026 (polkadot-sdk PR #10452/#10690), so use
# `master` or a stable tag from >= 2026-03 (e.g. polkadot-stable2603-3). Pin for caching.
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
  # `staging-node-cli` is the kitchensink node package; its binary is `substrate-node`.
  ( cd "$SDK_DIR" && cargo build --release -p staging-node-cli --bin substrate-node )
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
