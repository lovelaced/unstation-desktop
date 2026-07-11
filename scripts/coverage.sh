#!/usr/bin/env bash
# Measure test coverage of the deterministic engine (cargo-llvm-cov).
#
# Excludes two areas that can't be covered by the default `cargo test` run:
#   * transport-libdc/src — the real-WebRTC libdatachannel reactor (FFI); exercised only
#     by the #[ignore]d mesh_loopback e2e, which needs loopback UDP.
#   * unstation-node/src/main — the thin CLI entrypoint.
#
#   scripts/coverage.sh            # print summary + write HTML (target/llvm-cov/html)
#   scripts/coverage.sh --check    # gate: fail if line coverage < $COVERAGE_MIN (default 85)
#
# Needs cargo-llvm-cov + the llvm-tools component:
#   cargo install cargo-llvm-cov && rustup component add llvm-tools-preview
set -euo pipefail
cd "$(dirname "$0")/.."

IGNORE='transport-libdc/src|unstation-node/src/main'
# Ratcheted 84 -> 90 (July 11). Deterministic synthetic fixtures for the H.264 media
# paths landed (segmenter sps 99% / h264_poc 99% / fmp4 92%, via a bit-encoder that is
# the exact inverse of the parsers, so no ffmpeg is needed for those), plus hls-server
# error/LL-reload tests, taking the engine to ~91.3%. The floor sits ~1.3% below that so
# platform variance can't flake the gate. NOTE: the coverage CI job installs ffmpeg;
# without it the segmenter's real-media lib.rs tests skip and the total falls under 90.
# RATCHET: only ever raise this floor, never lower it.
THRESHOLD="${COVERAGE_MIN:-90}"

if [ "${1:-}" = "--check" ]; then
  echo "[coverage] gating engine line coverage at >= ${THRESHOLD}%"
  cargo llvm-cov --workspace --ignore-filename-regex "$IGNORE" --summary-only --fail-under-lines "$THRESHOLD"
else
  cargo llvm-cov --workspace --ignore-filename-regex "$IGNORE" --html
  echo "[coverage] HTML report → target/llvm-cov/html/index.html"
fi
