#!/usr/bin/env bash
# Run the Unstation test suite locally — much faster than waiting on CI.
#
# The fast tiers need NO chain and run in seconds. --chain adds the real statement-store
# e2e (boots a local --dev node; one-time node build, see dev-chain.sh). --paseo adds the
# public-Paseo smoke (needs network + a provisioned key).
#
#   scripts/test-all.sh                     # engine suite + real-WebRTC mesh + scale sim
#   scripts/test-all.sh --bench             # + criterion benchmarks
#   scripts/test-all.sh --chain             # + real local-chain e2e
#   scripts/test-all.sh --paseo             # + public-Paseo smoke
#   scripts/test-all.sh --all               # everything
#
# Env: NODE_WS / RPC_URL override the dev-node endpoints (a node already running there is
# reused instead of booting a new one).
set -uo pipefail # NOT -e: run every tier, then summarize failures.
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

WITH_BENCH=0 WITH_CHAIN=0 WITH_PASEO=0
for a in "$@"; do
  case "$a" in
    --bench) WITH_BENCH=1 ;;
    --chain) WITH_CHAIN=1 ;;
    --paseo) WITH_PASEO=1 ;;
    --all) WITH_BENCH=1 WITH_CHAIN=1 WITH_PASEO=1 ;;
    *) echo "unknown arg: $a (use --bench/--chain/--paseo/--all)" >&2; exit 2 ;;
  esac
done

FAILED=()
section() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }
step() { # step "label" cmd...
  local label="$1"; shift
  section "$label"
  if "$@"; then echo "  ✓ $label"; else echo "  ✗ $label"; FAILED+=("$label"); fi
}

NODE_PID=""
cleanup() { [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null; return 0; }
trap cleanup EXIT

# ---- fast tiers (no chain) ----
step "engine suite (cargo test --workspace)" \
  cargo test --workspace
step "real-WebRTC mesh over loopback" \
  env UNSTATION_BIND_ADDR=127.0.0.1 cargo test -p transport-libdc --test mesh_loopback -- --ignored --nocapture
step "real-WebRTC wide fan-out (1 publisher → 8 viewers, 2 leave mid-stream)" \
  env UNSTATION_BIND_ADDR=127.0.0.1 cargo test -p transport-libdc --test fanout_loopback -- --ignored --nocapture
step "scale sim (metrics)" \
  cargo test -p unstation-core --test scale_sim -- --nocapture
step "netsim impairment hardening (matrix + relay + fuzz over lossy/laggy links)" \
  cargo test -p unstation-core --lib netsim -- --ignored --nocapture

if [ "$WITH_BENCH" = 1 ]; then
  step "benchmarks (criterion)" \
    cargo bench -p unstation-core
fi

# ---- real local chain ----
if [ "$WITH_CHAIN" = 1 ]; then
  RPC_URL="${RPC_URL:-http://127.0.0.1:9944}"
  section "local real-chain e2e (statement store)"
  if RPC_URL="$RPC_URL" TIMEOUT=3 .github/scripts/wait-for-rpc.sh >/dev/null 2>&1; then
    echo "  using the node already running at $RPC_URL"
  else
    echo "  booting a local dev node (one-time build if needed; see dev-chain.sh) …"
    scripts/dev-chain.sh build || FAILED+=("dev-chain build")
    scripts/dev-chain.sh run > "$ROOT/dev-node.log" 2>&1 &
    NODE_PID=$!
    if ! RPC_URL="$RPC_URL" TIMEOUT=120 .github/scripts/wait-for-rpc.sh; then
      echo "  ✗ dev node never became ready (see dev-node.log)"; FAILED+=("dev node boot")
    fi
  fi
  if RPC_URL="$RPC_URL" TIMEOUT=2 .github/scripts/wait-for-rpc.sh >/dev/null 2>&1; then
    # Grant the e2e key's statement allowance (runtime-correct, via polkadot-js).
    NODE_WS="${NODE_WS:-ws://127.0.0.1:9944}" scripts/provision-allowance.sh \
      || FAILED+=("provision allowance")
    ( cd crates/unstation-chain \
      && NODE_WS="${NODE_WS:-ws://127.0.0.1:9944}" \
         cargo test --test chain_e2e -- --ignored --nocapture ) \
      && echo "  ✓ chain e2e" || { echo "  ✗ chain e2e"; FAILED+=("chain e2e"); }
    # Volunteer seed e2e: the REAL unstation-node binary discovers a publisher
    # session over the real chain, dials it over real WebRTC, and caches the live
    # window. (It provisions its own key via provision-allowance.sh.)
    ( cd crates/unstation-node \
      && NODE_WS="${NODE_WS:-ws://127.0.0.1:9944}" \
         cargo test --test seed_e2e -- --ignored --nocapture ) \
      && echo "  ✓ seed e2e" || { echo "  ✗ seed e2e"; FAILED+=("seed e2e"); }
  fi
fi

# ---- public Paseo ----
if [ "$WITH_PASEO" = 1 ]; then
  step "public-Paseo smoke" \
    bash -c 'cd crates/unstation-chain && cargo test --test paseo_smoke -- --ignored --nocapture'
fi

# ---- summary ----
section "summary"
if [ "${#FAILED[@]}" -gt 0 ]; then
  printf '\033[31mFAILED:\033[0m\n'
  printf '  - %s\n' "${FAILED[@]}"
  exit 1
fi
echo -e "\033[32mall tiers green\033[0m"
