#!/usr/bin/env bash
#
# mock-whip.sh — a stand-in for an OBS 30+ WHIP publisher, for the sub-second path.
#
# OBS 30+ speaks WHIP (WebRTC-HTTP ingestion, RFC 9725); this is ffmpeg 8.1+'s `whip`
# muxer configured to match: H.264 over WebRTC RTP, DTLS, single HTTP offer/answer.
# Point it at the WHIP URL Unstation shows on "Go live" (WHIP mode) and you get a real
# sub-second contribution feed without installing OBS.
#
# Usage:
#   1. In Unstation, choose "Go live", pick the WHIP ingest, and copy the WHIP URL.
#   2. Run this with that URL:
#        scripts/mock-whip.sh -u http://127.0.0.1:PORT/whip
#   3. It streams a moving test pattern (with a running clock) until you press Ctrl-C.
#
#   scripts/mock-whip.sh -u <url>              # default: test pattern 1280x720@30
#   scripts/mock-whip.sh -u <url> -i clip.mp4  # stream a file (looped) instead
#   scripts/mock-whip.sh -u <url> -t 10        # one-shot: 10 s then exit (handy for tests)
#   scripts/mock-whip.sh -u <url> -s 1920x1080 -r 60 -b 6000
#
# KNOWN LIMITATION (ffmpeg <= 8.1): ffmpeg's whip muxer never answers mid-session STUN
# consent checks (RFC 7675), so any compliant WebRTC receiver — ours included — expires
# the session after exactly 30 s ("Consent expired for candidate pair"). This script
# auto-reconnects, so the stream continues with a ~1 s hiccup every 30 s. Real encoders
# (OBS 30+, libdatachannel-based) answer consent and hold indefinitely; ffmpeg git master
# has consent support and will too. See whip-ingest/tests/ffmpeg_whip.rs (the canary soak).
#
# WHIP has no stream key — the URL identifies the session. No `nounset` (macOS bash 3.2).
set -eo pipefail

URL=""
FILE=""
DURATION=""
SIZE=1280x720
FPS=30
BV=4500          # video bitrate (kbps)

usage() { sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0; }

while getopts ":u:i:t:s:r:b:h" opt; do
  case "$opt" in
    u) URL="$OPTARG" ;;
    i) FILE="$OPTARG" ;;
    t) DURATION="$OPTARG" ;;
    s) SIZE="$OPTARG" ;;
    r) FPS="$OPTARG" ;;
    b) BV="$OPTARG" ;;
    h) usage ;;
    *) echo "unknown option -$OPTARG" >&2; exit 2 ;;
  esac
done

command -v ffmpeg >/dev/null 2>&1 || { echo "ffmpeg not found — install it (brew install ffmpeg)"; exit 1; }
ffmpeg -hide_banner -muxers 2>/dev/null | grep -q whip || { echo "this ffmpeg has no 'whip' muxer — need ffmpeg 8.1+"; exit 1; }
[ -z "$URL" ] && { echo "give the WHIP URL with -u (copy it from Unstation's Go-live WHIP panel)"; exit 2; }

# ~1 s keyframe interval — low-latency, and matches the app's 1 s segment cadence.
GOP="$FPS"

if [ -n "$FILE" ]; then
  [ -f "$FILE" ] || { echo "file not found: $FILE"; exit 1; }
  INPUTS=(-stream_loop -1 -re -i "$FILE")
else
  INPUTS=(-re -f lavfi -i "testsrc=size=${SIZE}:rate=${FPS}")
fi

# H.264 only (the WHIP endpoint receives a single recvonly video track). zerolatency +
# no B-frames matches the low-latency contribution profile.
ENCODE=(
  -c:v libx264 -preset veryfast -tune zerolatency -profile:v baseline -pix_fmt yuv420p
  -bf 0 -g "$GOP" -keyint_min "$GOP" -sc_threshold 0
  -b:v "${BV}k" -maxrate "${BV}k" -bufsize "$(( BV * 2 ))k"
  -f whip
)

DUR_ARG=()
[ -n "$DURATION" ] && DUR_ARG=(-t "$DURATION")

echo "mock WHIP → ${URL}"
echo "  source : ${FILE:-test pattern ${SIZE}@${FPS}}   video: ${BV}kbps  gop: ${GOP}"
[ -z "$DURATION" ] && echo "  (make sure 'Go live' (WHIP) is active in Unstation; Ctrl-C to stop)"

trap 'echo; echo "mock WHIP stopped."; exit 0' INT

while true; do
  set +e
  ffmpeg -hide_banner -loglevel info "${INPUTS[@]}" ${DUR_ARG[@]+"${DUR_ARG[@]}"} "${ENCODE[@]}" "$URL"
  rc=$?
  set -e
  if [ -n "$DURATION" ]; then
    exit "$rc"                      # one-shot mode (tests)
  fi
  echo "… WHIP session ended (rc=$rc; ffmpeg <= 8.1 caps at 30 s — see header). Reconnecting…"
  sleep 0.5
done
