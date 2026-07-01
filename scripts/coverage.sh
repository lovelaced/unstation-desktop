#!/usr/bin/env bash
# Measure test coverage of the deterministic engine (cargo-llvm-cov).
#
# Excludes two areas that can't be covered by the default `cargo test` run:
#   * transport-libdc/src — the real-WebRTC libdatachannel reactor (FFI); exercised only
#     by the #[ignore]d mesh_loopback e2e, which needs loopback UDP.
#   * unstation-node/src/main — the thin CLI entrypoint.
#
#   scripts/coverage.sh            # print summary + write HTML (target/llvm-cov/html)
#   scripts/coverage.sh --check    # gate: fail if line coverage < $COVERAGE_MIN (default 90)
#
# Needs cargo-llvm-cov + the llvm-tools component:
#   cargo install cargo-llvm-cov && rustup component add llvm-tools-preview
set -euo pipefail
cd "$(dirname "$0")/.."

IGNORE='transport-libdc/src|unstation-node/src/main'
THRESHOLD="${COVERAGE_MIN:-90}"

if [ "${1:-}" = "--check" ]; then
  echo "[coverage] gating engine line coverage at >= ${THRESHOLD}%"
  cargo llvm-cov --workspace --ignore-filename-regex "$IGNORE" --summary-only --fail-under-lines "$THRESHOLD"
else
  cargo llvm-cov --workspace --ignore-filename-regex "$IGNORE" --html
  echo "[coverage] HTML report → target/llvm-cov/html/index.html"
fi
