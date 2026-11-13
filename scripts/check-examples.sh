#!/usr/bin/env bash
# Build every example (feature-gated ones are skipped by Cargo) and smoke-test
# the CLI of the ones that run with the default feature set.
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

cargo build --quiet --examples

examples=(
  authenticated_server
  file_server
  hls_server
  http_flv_server
  live_stream_demo
  local_source
  pipeline_server
  proxy_server
  publisher_server
  recording_vod_server
  mem_probe
  play_client
  gstreamer_live_play
  ffmpeg_live_publish
  ipc_camera
)

for example in "${examples[@]}"; do
  if [[ "$example" == "local_source" ]]; then
    cargo run --quiet --example "$example" --features test-source -- --help >/dev/null
  else
    cargo run --quiet --example "$example" -- --help >/dev/null
  fi
  printf 'example %s: CLI OK\n' "$example"
done

printf 'gated examples (built only with their features): rtmp_server, srt_server, dash_server, webrtc_server, multi_protocol_gateway\n'
