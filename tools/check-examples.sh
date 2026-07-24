#!/usr/bin/env bash
set -euo pipefail

examples=(
  local_source
  file_server
  play_client
  proxy_server
  publisher_server
  authenticated_server
  pipeline_server
  ffmpeg_live_publish
  gstreamer_live_play
  live_stream_demo
)

for example in "${examples[@]}"; do
  cargo run --quiet --example "$example" -- --help >/dev/null
  printf 'example %s: CLI OK\n' "$example"
done
