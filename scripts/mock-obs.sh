#!/usr/bin/env bash
#
# mock-obs.sh — a stand-in for OBS that streams into Unstation's publisher ingest.
#
# OBS is just a standard RTMP publisher, so this is ffmpeg configured to match
# OBS's output (H.264/AAC over RTMP-FLV, realtime, ~2 s keyframes). Point it at the
# same ingest the app opens on "Go live" and you get a real live feed without
# installing OBS.
#
# Usage:
#   1. In Unstation, click "Go live instead →" (this starts the RTMP ingest).
#   2. Run this script. It retries until the ingest is up, then streams a moving
#      test pattern (with a running clock) until you press Ctrl-C.
#
#   scripts/mock-obs.sh                 # default: rtmp://127.0.0.1:21935/live/unstation
#   scripts/mock-obs.sh -i clip.mp4     # stream a file (looped) instead of the test pattern
#   scripts/mock-obs.sh -t 10           # one-shot: stream 10 s then exit (handy for tests)
#   scripts/mock-obs.sh -p 21935 -k unstation -s 1920x1080 -r 60 -b 6000
#
# No `nounset` (-u): macOS ships bash 3.2, which errors on empty-array expansion under -u.
set -eo pipefail

HOST=127.0.0.1
PORT=21935
APP=live
KEY=unstation
URL=""
FILE=""
DURATION=""
SIZE=1280x720
FPS=30
BV=4500          # video bitrate (kbps), OBS-typical

usage() { sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0; }

while getopts ":u:p:k:i:t:s:r:b:h" opt; do
  case "$opt" in
    u) URL="$OPTARG" ;;
    p) PORT="$OPTARG" ;;
    k) KEY="$OPTARG" ;;
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
[ -z "$URL" ] && URL="rtmp://${HOST}:${PORT}/${APP}/${KEY}"

GOP=$(( FPS * 2 ))   # ~2 s keyframe interval, like OBS

# Inputs: a looped file, or a moving test pattern (testsrc has a built-in clock)
# plus a 440 Hz tone, so the encoder sends A+V exactly like OBS.
if [ -n "$FILE" ]; then
  [ -f "$FILE" ] || { echo "file not found: $FILE"; exit 1; }
  INPUTS=(-stream_loop -1 -re -i "$FILE")
else
  INPUTS=(-re -f lavfi -i "testsrc=size=${SIZE}:rate=${FPS}" -f lavfi -i "sine=frequency=440:sample_rate=44100")
fi

ENCODE=(
  -c:v libx264 -preset veryfast -profile:v main -pix_fmt yuv420p
  -g "$GOP" -keyint_min "$GOP" -sc_threshold 0
  -b:v "${BV}k" -maxrate "${BV}k" -bufsize "$(( BV * 2 ))k"
  -c:a aac -b:a 160k -ar 44100
  -f flv
)

DUR_ARG=()
[ -n "$DURATION" ] && DUR_ARG=(-t "$DURATION")

echo "mock OBS → ${URL}"
echo "  source : ${FILE:-test pattern ${SIZE}@${FPS} + 440Hz tone}   video: ${BV}kbps  gop: ${GOP}"
[ -z "$DURATION" ] && echo "  (make sure 'Go live' is active in Unstation — retrying until the ingest answers; Ctrl-C to stop)"

trap 'echo; echo "mock OBS stopped."; exit 0' INT

while true; do
  set +e
  ffmpeg -hide_banner -loglevel info "${INPUTS[@]}" ${DUR_ARG[@]+"${DUR_ARG[@]}"} "${ENCODE[@]}" "$URL"
  rc=$?
  set -e
  if [ -n "$DURATION" ]; then
    exit "$rc"                      # one-shot mode (tests)
  fi
  echo "… ingest not ready or stream ended (rc=$rc). Retrying in 2 s — click 'Go live' in Unstation."
  sleep 2
done
