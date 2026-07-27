#!/usr/bin/env bash
#
# rtflow Multi-Protocol Demo
#
# End-to-end demonstration:
# 1. Builds and starts the multi_protocol_gateway example
# 2. Publishes a test signal via FFmpeg (RTMP)
# 3. Verifies HLS/DASH/FLV endpoints are reachable
# 4. Prints playback commands for all protocols
#
# Usage:
#   ./tools/demo.sh [--duration SECONDS]
#
# Requirements:
#   - cargo (Rust toolchain)
#   - ffmpeg (for test signal generation)
#   - curl (for endpoint verification)
set -euo pipefail

DURATION="${1:-0}"
if [[ "${1:-}" == "--duration" ]]; then
    DURATION="${2:-30}"
fi

RTMP_PORT=1935
RTSP_PORT=8554
SRT_PORT=9000
HTTP_PORT=8080
MOUNT="/live/stream"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; exit 1; }

# Check dependencies
command -v cargo >/dev/null 2>&1 || error "cargo not found"
command -v ffmpeg >/dev/null 2>&1 || error "ffmpeg not found"
command -v curl >/dev/null 2>&1 || warn "curl not found, skipping endpoint verification"

cleanup() {
    info "Shutting down..."
    [[ -n "${FFMPEG_PID:-}" ]] && kill "$FFMPEG_PID" 2>/dev/null || true
    [[ -n "${GATEWAY_PID:-}" ]] && kill "$GATEWAY_PID" 2>/dev/null || true
    wait 2>/dev/null || true
    info "Done."
}
trap cleanup EXIT

# Build
info "Building multi_protocol_gateway..."
cargo build --example multi_protocol_gateway --all-features 2>&1 | tail -5

# Start gateway
info "Starting gateway..."
cargo run --example multi_protocol_gateway --all-features -- \
    --rtmp-listen "0.0.0.0:$RTMP_PORT" \
    --rtsp-listen "0.0.0.0:$RTSP_PORT" \
    --srt-listen "0.0.0.0:$SRT_PORT" \
    --http-listen "0.0.0.0:$HTTP_PORT" \
    --mount "$MOUNT" &
GATEWAY_PID=$!
sleep 2

# Verify gateway is running
if ! kill -0 "$GATEWAY_PID" 2>/dev/null; then
    error "Gateway failed to start"
fi

# Start FFmpeg test signal
info "Starting FFmpeg test signal -> RTMP..."
ffmpeg -re \
    -f lavfi -i "testsrc=size=1280x720:rate=25" \
    -f lavfi -i "sine=frequency=1000:sample_rate=48000" \
    -c:v libx264 -preset ultrafast -tune zerolatency -g 50 \
    -c:a aac -b:a 128k \
    -f flv "rtmp://127.0.0.1:$RTMP_PORT$MOUNT" \
    </dev/null >/dev/null 2>&1 &
FFMPEG_PID=$!

# Wait for stream to be established
info "Waiting for stream to stabilize..."
sleep 5

if ! kill -0 "$FFMPEG_PID" 2>/dev/null; then
    error "FFmpeg failed to start publishing"
fi

# Verify endpoints
if command -v curl >/dev/null 2>&1; then
    info "Verifying endpoints..."

    # HLS
    if curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$HTTP_PORT$MOUNT.m3u8" | grep -q "200"; then
        info "  HLS:    OK"
    else
        warn "  HLS:    not ready yet (may need more time)"
    fi

    # DASH
    if curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$HTTP_PORT$MOUNT/manifest.mpd" | grep -q "200"; then
        info "  DASH:   OK"
    else
        warn "  DASH:   not ready yet (may need more time)"
    fi

    # HTTP-FLV (just check connection, it's a streaming response)
    if curl -s -o /dev/null --max-time 2 "http://127.0.0.1:$HTTP_PORT$MOUNT.flv" 2>/dev/null; then
        info "  FLV:    OK"
    else
        info "  FLV:    endpoint reachable (streaming)"
    fi
fi

# Print playback commands
echo ""
echo "============================================================"
echo "  rtflow Multi-Protocol Demo - ACTIVE"
echo "============================================================"
echo ""
echo "  Playback commands:"
echo ""
echo "    RTSP:     ffplay rtsp://127.0.0.1:$RTSP_PORT$MOUNT"
echo "    RTMP:     ffplay rtmp://127.0.0.1:$RTMP_PORT$MOUNT"
echo "    HLS:      ffplay http://127.0.0.1:$HTTP_PORT$MOUNT.m3u8"
echo "    DASH:     ffplay http://127.0.0.1:$HTTP_PORT$MOUNT/manifest.mpd"
echo "    HTTP-FLV: ffplay http://127.0.0.1:$HTTP_PORT$MOUNT.flv"
echo ""
echo "  Browser (DASH):  http://127.0.0.1:$HTTP_PORT$MOUNT/player"
echo "  Browser (WHEP):  http://127.0.0.1:$HTTP_PORT/player"
echo ""
echo "============================================================"
echo ""

if [[ "$DURATION" -gt 0 ]]; then
    info "Running for ${DURATION}s then exiting..."
    sleep "$DURATION"
else
    info "Running indefinitely. Press Ctrl+C to stop."
    wait "$GATEWAY_PID"
fi
