#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
fixture_dir="$repo_dir/tests/fixtures"
mkdir -p "$fixture_dir"

common_video=(
  -hide_banner -loglevel error -y
  -f lavfi -i "testsrc=size=64x64:rate=5"
  -f lavfi -i "sine=frequency=1000:sample_rate=48000"
  -t 0.4 -shortest
  -c:v libx264 -preset ultrafast -tune zerolatency
  -pix_fmt yuv420p -g 1 -threads 1
  -c:a aac -b:a 32k -ar 48000 -ac 2
  -fflags +bitexact
)

ffmpeg "${common_video[@]}" "$fixture_dir/h264-aac.mp4"
ffmpeg -hide_banner -loglevel error -y \
  -i "$fixture_dir/h264-aac.mp4" -map 0 -c copy \
  -fflags +bitexact "$fixture_dir/h264-aac.mov"
ffmpeg -hide_banner -loglevel error -y \
  -i "$fixture_dir/h264-aac.mp4" -map 0 -c copy \
  -movflags frag_keyframe+empty_moov -fflags +bitexact \
  "$fixture_dir/h264-aac-fragmented.mp4"
ffmpeg -hide_banner -loglevel error -y \
  -i "$fixture_dir/h264-aac.mp4" -map 0 -c copy \
  -fflags +bitexact "$fixture_dir/h264-aac.mkv"
ffmpeg -hide_banner -loglevel error -y \
  -i "$fixture_dir/h264-aac.mp4" -map 0 -c copy \
  -fflags +bitexact -f mpegts "$fixture_dir/h264-aac.ts"
perl -e '$index = 0; while (read(STDIN, $packet, 188) == 188) { print pack("N", $index++), $packet; }' \
  < "$fixture_dir/h264-aac.ts" \
  > "$fixture_dir/h264-aac.m2ts"
ffmpeg -hide_banner -loglevel error -y \
  -i "$fixture_dir/h264-aac.mp4" -map 0:v:0 -c copy \
  -bsf:v h264_mp4toannexb -f h264 "$fixture_dir/video.h264"
ffmpeg -hide_banner -loglevel error -y \
  -i "$fixture_dir/h264-aac.mp4" -map 0:a:0 -c copy \
  -f adts "$fixture_dir/audio.aac"

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "testsrc=size=64x64:rate=5" -t 0.4 \
  -c:v libx265 -preset ultrafast -pix_fmt yuv420p -g 1 \
  -x265-params "pools=1:frame-threads=1:log-level=error" \
  -an -fflags +bitexact "$fixture_dir/h265.mp4"
ffmpeg -hide_banner -loglevel error -y \
  -i "$fixture_dir/h265.mp4" -map 0 -c copy \
  -fflags +bitexact "$fixture_dir/h265.mkv"
ffmpeg -hide_banner -loglevel error -y \
  -i "$fixture_dir/h265.mp4" -map 0 -c copy \
  -fflags +bitexact -f mpegts "$fixture_dir/h265.ts"
ffmpeg -hide_banner -loglevel error -y \
  -i "$fixture_dir/h265.mp4" -map 0:v:0 -c copy \
  -bsf:v hevc_mp4toannexb -f hevc "$fixture_dir/video.h265"

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "sine=frequency=440:sample_rate=8000" -t 0.2 \
  -c:a pcm_alaw -ac 1 -fflags +bitexact "$fixture_dir/pcma.wav"
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "sine=frequency=660:sample_rate=8000" -t 0.2 \
  -c:a pcm_mulaw -ac 1 -fflags +bitexact "$fixture_dir/pcmu.wav"
ffmpeg -hide_banner -loglevel error -y \
  -i "$fixture_dir/pcma.wav" -map 0:a:0 -c copy \
  -f alaw "$fixture_dir/audio.pcma"
ffmpeg -hide_banner -loglevel error -y \
  -i "$fixture_dir/pcmu.wav" -map 0:a:0 -c copy \
  -f mulaw "$fixture_dir/audio.pcmu"

(
  cd "$fixture_dir"
  sha256sum \
    h264-aac.mp4 h264-aac.mov h264-aac-fragmented.mp4 \
    h264-aac.mkv h264-aac.ts h264-aac.m2ts \
    video.h264 audio.aac h265.mp4 h265.mkv h265.ts video.h265 \
    pcma.wav pcmu.wav audio.pcma audio.pcmu > SHA256SUMS
)
