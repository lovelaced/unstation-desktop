#!/usr/bin/env bash
# Install an Unstation volunteer seed/relay as a systemd service on a Linux VPS.
#
#   curl -fsSL https://raw.githubusercontent.com/lovelaced/unstation-desktop/master/scripts/seed/install.sh \
#     | sudo bash
#
# Installs an OPEN relay by default: the seed announces spare capacity and helps
# carry whatever streams recruit it — no stream name needed. Pass --stream to pin
# specific streams instead.
#
# Downloads the latest `seed-v*` release binary for this machine's architecture
# (verified against the release's SHA256SUMS), creates the `unstation` system user,
# signs the seed in with your Polkadot app (a QR appears in the terminal — the seed's
# statement-store writes need an on-chain allowance, granted by the phone at pairing),
# then installs + starts the `unstation-seed` systemd service and prints how to watch it.
# Re-running is safe: it updates the binary, keeps the identity, and rewrites the unit.
#
# Options:
#   --stream <name>      pin a stream to seed (repeatable; default: open relay).
#                        Names must not contain the characters | or &
#   --max-streams <n>    open relay: most streams served at once (default 8)
#   --budget-mbps <n>    total uplink to donate across streams (default 50)
#   --binary <path|url>  skip the release download; install this binary instead
#                        (e.g. one you built from source — see the README)
#   --repo <owner/name>  GitHub repo to fetch releases from (default lovelaced/unstation-desktop)
#   --tag <seed-vX.Y.Z>  pin a specific seed release (default: newest seed-v* tag)
#   --no-pair            skip the sign-in step (pair manually later; the service
#                        exits with a how-to message until you do)
#
# Env: set UNSTATION_NODE_MNEMONIC to use a pre-provisioned account instead of
# pairing (stored root-readable-only in /var/lib/unstation-seed/env, never in argv).
#
# Requirements: Debian/Ubuntu-ish VPS with systemd, a public IP, and inbound UDP
# allowed (WebRTC uses ephemeral UDP ports — no TCP ports, domains, or certs needed).
set -euo pipefail

STREAMS=() BUDGET=50 MAX_STREAMS=8 BINARY="" REPO="lovelaced/unstation-desktop" TAG="" NO_PAIR=0
while [ $# -gt 0 ]; do
  case "$1" in
    --stream)      STREAMS+=("${2:?}"); shift 2 ;;
    --max-streams) MAX_STREAMS="${2:?}"; shift 2 ;;
    --budget-mbps) BUDGET="${2:?}"; shift 2 ;;
    --binary)      BINARY="${2:?}"; shift 2 ;;
    --repo)        REPO="${2:?}"; shift 2 ;;
    --tag)         TAG="${2:?}"; shift 2 ;;
    --no-pair)     NO_PAIR=1; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
# The unit file is templated with sed — keep stream names out of its metacharacters.
for s in ${STREAMS[@]+"${STREAMS[@]}"}; do
  case "$s" in *'|'*|*'&'*) echo "error: stream names must not contain | or &" >&2; exit 2 ;; esac
done
if [ "${#STREAMS[@]}" -gt 0 ]; then
  STREAM_ARGS=""; for s in "${STREAMS[@]}"; do STREAM_ARGS="$STREAM_ARGS --stream $s"; done
  STREAM_ARGS="${STREAM_ARGS# }"
  MODE="streams: ${STREAMS[*]}"
else
  STREAM_ARGS=""
  MODE="open relay"
fi
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

# ---- sign-in (the seed's chain writes need an on-chain allowance) ----
# Runs as the service user so the identity files land owned by it, and outside
# systemd so the unit's sandboxing doesn't apply. Order of preference:
# operator-provided mnemonic > already-paired identity > interactive QR pairing.
PAIRED=0
if [ -n "${UNSTATION_NODE_MNEMONIC:-}" ]; then
  install -o unstation -g unstation -m 0600 /dev/null /var/lib/unstation-seed/env
  printf 'UNSTATION_NODE_MNEMONIC=%s\n' "$UNSTATION_NODE_MNEMONIC" > /var/lib/unstation-seed/env
  echo "[seed] identity: pre-provisioned account from UNSTATION_NODE_MNEMONIC (saved to /var/lib/unstation-seed/env)"
  PAIRED=1
elif [ -f /var/lib/unstation-seed/slot_secret ]; then
  echo "[seed] identity: already signed in (slot key in /var/lib/unstation-seed) — keeping it"
  PAIRED=1
elif [ "$NO_PAIR" = 1 ]; then
  echo "[seed] --no-pair: skipping sign-in — the service will exit with instructions until you pair"
elif [ -r /dev/tty ] && [ -w /dev/tty ]; then
  echo ""
  echo "[seed] sign this seed in with the Polkadot app on your phone — a QR code is"
  echo "[seed] about to appear; scan it from the app (times out after ~2 minutes)."
  if runuser -u unstation -- env HOME=/var/lib/unstation-seed \
       UNSTATION_NODE_KEY_DIR=/var/lib/unstation-seed \
       /usr/local/bin/unstation-node pair </dev/tty >/dev/tty 2>&1; then
    PAIRED=1
  else
    echo "[seed] pairing did not complete — you can retry any time (command below)"
  fi
else
  echo "[seed] no terminal available for the sign-in QR — pair manually (command below)"
fi

# ---- systemd unit (template lives next to this script in the repo; embedded here so
#      the installer works standalone via curl) ----
UNIT=/etc/systemd/system/unstation-seed.service
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-/nonexistent}")" 2>/dev/null && pwd || true)"
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/unstation-seed.service" ]; then
  sed -e "s|@MODE@|$MODE|g" -e "s|@STREAM_ARGS@|$STREAM_ARGS|g" \
      -e "s|@BUDGET@|$BUDGET|g" -e "s|@MAX_STREAMS@|$MAX_STREAMS|g" \
      "$SCRIPT_DIR/unstation-seed.service" > "$UNIT"
else
  cat > "$UNIT" <<EOF
[Unit]
Description=Unstation volunteer seed ($MODE)
After=network-online.target
Wants=network-online.target

[Service]
User=unstation
Group=unstation
# Bare (no args) = open relay: announces spare capacity and serves whatever streams
# publishers recruit it onto. --stream <name> pins specific streams instead.
ExecStart=/usr/local/bin/unstation-node $STREAM_ARGS
Restart=on-failure
RestartSec=10
# SIGINT (not the default SIGTERM) so shutdown withdraws from the volunteer
# rendezvous and closes peer connections cleanly; give it time to do so.
KillSignal=SIGINT
TimeoutStopSec=20
# Exit 78 = not signed in / misconfigured — restarting cannot fix it; run
# `sudo -u unstation UNSTATION_NODE_KEY_DIR=/var/lib/unstation-seed unstation-node pair`
# then `systemctl restart unstation-seed`.
RestartPreventExitStatus=78
Environment=UNSTATION_NODE_KEY_DIR=/var/lib/unstation-seed
EnvironmentFile=-/var/lib/unstation-seed/env
Environment=UNSTATION_NODE_BUDGET_MBPS=$BUDGET
Environment=UNSTATION_NODE_MAX_STREAMS=$MAX_STREAMS
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
if [ "$PAIRED" = 1 ]; then
  echo "[seed] ✓ unstation-seed is running ($MODE, budget: ${BUDGET} Mbps total)"
  if [ "${#STREAMS[@]}" -gt 0 ]; then
    echo "[seed] expect:      'joining swarm for' then '[seg] seq=… verified' lines once the stream is live"
  else
    echo "[seed] expect:      'open relay: volunteering' now; 'joining swarm for' lines as streams recruit it"
  fi
else
  echo "[seed] ! installed but not signed in yet. The service exits with status 78 and stays"
  echo "[seed]   stopped until you pair (systemctl will show \"failed\" — that's expected)."
  echo "[seed]   Finish setup:"
  echo "[seed]     1) sudo -u unstation UNSTATION_NODE_KEY_DIR=/var/lib/unstation-seed unstation-node pair"
  echo "[seed]     2) sudo systemctl restart unstation-seed"
fi
echo "[seed] watch it:    journalctl -fu unstation-seed"
echo "[seed] firewall:    inbound UDP must be allowed (WebRTC ephemeral ports); no TCP ports needed"
echo "[seed] change it:   edit $UNIT, then: systemctl daemon-reload && systemctl restart unstation-seed"
