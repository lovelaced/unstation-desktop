#!/usr/bin/env bash
# Install an Unstation volunteer seed/relay as a systemd service on a Linux VPS.
#
#   curl -fsSL https://raw.githubusercontent.com/lovelaced/unstation-desktop/master/scripts/seed/install.sh \
#     | sudo bash -s -- --stream <stream-name>
#
# Downloads the latest `seed-v*` release binary for this machine's architecture
# (verified against the release's SHA256SUMS), creates the `unstation` system user,
# installs + starts the `unstation-seed` systemd service, and prints how to watch it.
# Re-running is safe: it updates the binary and rewrites the unit.
#
# Options:
#   --stream <name>      REQUIRED — the stream to seed (same name viewers type)
#   --budget-mbps <n>    uplink to donate (default 50)
#   --binary <path|url>  skip the release download; install this binary instead
#                        (e.g. one you built from source — see the README)
#   --repo <owner/name>  GitHub repo to fetch releases from (default lovelaced/unstation-desktop)
#   --tag <seed-vX.Y.Z>  pin a specific seed release (default: newest seed-v* tag)
#
# Requirements: Debian/Ubuntu-ish VPS with systemd, a public IP, and inbound UDP
# allowed (WebRTC uses ephemeral UDP ports — no TCP ports, domains, or certs needed).
set -euo pipefail

STREAM="" BUDGET=50 BINARY="" REPO="lovelaced/unstation-desktop" TAG=""
while [ $# -gt 0 ]; do
  case "$1" in
    --stream)      STREAM="${2:?}"; shift 2 ;;
    --budget-mbps) BUDGET="${2:?}"; shift 2 ;;
    --binary)      BINARY="${2:?}"; shift 2 ;;
    --repo)        REPO="${2:?}"; shift 2 ;;
    --tag)         TAG="${2:?}"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
[ -n "$STREAM" ] || { echo "error: --stream <name> is required" >&2; exit 2; }
[ "$(id -u)" -eq 0 ] || { echo "error: run as root (sudo)" >&2; exit 2; }
command -v systemctl >/dev/null || { echo "error: systemd required" >&2; exit 2; }

case "$(uname -m)" in
  x86_64|amd64)   ARCH=x86_64 ;;
  aarch64|arm64)  ARCH=aarch64 ;;
  *) echo "error: unsupported architecture $(uname -m) (x86_64/aarch64 binaries only — build from source, see README)" >&2; exit 2 ;;
esac

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT

# ---- obtain the binary ----
if [ -n "$BINARY" ]; then
  case "$BINARY" in
    http://*|https://*) echo "[seed] downloading $BINARY"; curl -fSL -o "$WORK/unstation-node" "$BINARY" ;;
    *)                  cp "$BINARY" "$WORK/unstation-node" ;;
  esac
else
  if [ -z "$TAG" ]; then
    # Newest seed-v* tag: app releases share this repo, so releases/latest won't do.
    TAG="$(curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=100" \
      | grep -oE '"tag_name": *"seed-v[^"]*"' | head -1 | cut -d'"' -f4)"
    [ -n "$TAG" ] || { echo "error: no seed-v* release found in $REPO (build from source or pass --binary)" >&2; exit 1; }
  fi
  ASSET="unstation-node-$ARCH-linux"
  BASE="https://github.com/$REPO/releases/download/$TAG"
  echo "[seed] downloading $ASSET from $TAG"
  curl -fSL -o "$WORK/unstation-node" "$BASE/$ASSET"
  curl -fSL -o "$WORK/SHA256SUMS" "$BASE/SHA256SUMS"
  echo "[seed] verifying checksum"
  (cd "$WORK" && cp unstation-node "$ASSET" && grep " $ASSET\$" SHA256SUMS | sha256sum -c -)
fi
chmod +x "$WORK/unstation-node"

# ---- user + dirs + binary ----
id -u unstation >/dev/null 2>&1 || useradd --system --home /var/lib/unstation-seed --shell /usr/sbin/nologin unstation
install -d -o unstation -g unstation -m 0750 /var/lib/unstation-seed
install -m 0755 "$WORK/unstation-node" /usr/local/bin/unstation-node

# ---- systemd unit (template lives next to this script in the repo; embedded here so
#      the installer works standalone via curl) ----
UNIT=/etc/systemd/system/unstation-seed.service
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-/nonexistent}")" 2>/dev/null && pwd || true)"
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/unstation-seed.service" ]; then
  sed -e "s|@STREAM@|$STREAM|g" -e "s|@BUDGET@|$BUDGET|g" "$SCRIPT_DIR/unstation-seed.service" > "$UNIT"
else
  cat > "$UNIT" <<EOF
[Unit]
Description=Unstation volunteer seed ($STREAM)
After=network-online.target
Wants=network-online.target

[Service]
User=unstation
Group=unstation
ExecStart=/usr/local/bin/unstation-node $STREAM
Restart=always
RestartSec=5
Environment=UNSTATION_NODE_KEY_DIR=/var/lib/unstation-seed
Environment=UNSTATION_NODE_BUDGET_MBPS=$BUDGET
Environment=RUST_LOG=info
StateDirectory=unstation-seed
ReadWritePaths=/var/lib/unstation-seed
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
NoNewPrivileges=true
MemoryMax=1G

[Install]
WantedBy=multi-user.target
EOF
fi

systemctl daemon-reload
systemctl enable --now unstation-seed

echo ""
echo "[seed] ✅ unstation-seed is running (stream: $STREAM, budget: ${BUDGET} Mbps)"
echo "[seed] watch it:    journalctl -fu unstation-seed"
echo "[seed] expect:      'identity: persisted key' → 'joining swarm' → segment 'verified' lines once the stream is live"
echo "[seed] firewall:    inbound UDP must be allowed (WebRTC ephemeral ports); no TCP ports needed"
echo "[seed] change it:   edit $UNIT, then: systemctl daemon-reload && systemctl restart unstation-seed"
