#!/usr/bin/env bash
# Build a universal (Apple Silicon + Intel) macOS .dmg locally — no CI, no tokens,
# no org approvals. Use this when you can't get a CI credential for the private SDK.
#
# Needs (all on your own machine, no special access):
#   • the chain SDK checked out as a sibling of this repo
#   • Rust, Node + pnpm
#   • optional: git-cliff (brew install git-cliff) for nice notes, gh (brew install gh) to publish
#
# Usage:
#   scripts/release-macos.sh                 # just build; prints the .dmg path to AirDrop
#   scripts/release-macos.sh v0.1.0          # build + cut a GitHub release (needs gh, logged in)
#
# Build target (default = universal, for release). For fast test iteration on an
# Apple-Silicon Mac, build arm64-only — skips the x86_64 OpenSSL compile + lipo:
#   UNSTATION_BUILD_TARGET=aarch64-apple-darwin scripts/release-macos.sh
set -euo pipefail

VERSION="${1:-}"
TARGET="${UNSTATION_BUILD_TARGET:-universal-apple-darwin}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SDK="$(cd "$ROOT/.." && pwd)/useragent-kit"
if [ ! -d "$SDK" ]; then
  echo "error: expected the chain SDK at $SDK"
  echo "       clone it next to this repo:  git clone <chain-sdk-url> $SDK"
  exit 1
fi

echo "==> ensuring Rust targets for $TARGET"
case "$TARGET" in
  universal-apple-darwin) rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null ;;
  *)                      rustup target add "$TARGET" >/dev/null ;;
esac

echo "==> building the macOS DMG for $TARGET (this takes a while)"
(cd desktop && pnpm install && pnpm tauri build --target "$TARGET")

DMG="$(find "desktop/src-tauri/target/$TARGET/release/bundle/dmg" -name '*.dmg' | head -1)"
[ -n "$DMG" ] || { echo "error: no .dmg was produced"; exit 1; }
echo "==> built: $DMG"

if [ -z "$VERSION" ]; then
  cat <<EOF

Done. To test on a second Mac, AirDrop or copy this file:
  $DMG

On that Mac: drag Unstation to Applications, then clear the unsigned-app quarantine once:
  xattr -dr com.apple.quarantine /Applications/Unstation.app

Heads up: this is a PRIVATE testnet build — it embeds a Paseo dev key so pairing and
discovery work before real personhood lands. Keep it to your own machines; don't post
it publicly.
EOF
  exit 0
fi

NOTES="$(mktemp)"
if command -v git-cliff >/dev/null 2>&1; then
  echo "==> generating release notes with git-cliff"
  git-cliff --config cliff.toml --tag "$VERSION" --unreleased --output "$NOTES"
else
  echo "==> git-cliff not found (brew install git-cliff) — using a minimal note"
  printf '## %s\n' "$VERSION" > "$NOTES"
fi

if command -v gh >/dev/null 2>&1; then
  echo "==> publishing release $VERSION (creates the tag too)"
  gh release create "$VERSION" "$DMG" --title "$VERSION" --notes-file "$NOTES" --prerelease
  echo "==> done — download the .dmg from the release on your second Mac"
else
  echo "gh CLI not installed (brew install gh). Either install it and re-run, or just"
  echo "AirDrop the .dmg directly: $DMG"
fi
