#!/usr/bin/env bash
# Reject stale crate names and crate-root module paths in user-facing docs.
#
# Historically the crate was called `rtspflow`/`rtsp`, so docs kept writing
# `rtsp::Result`, `rtsp::media`, ... long after the rename to `rtflow`.
# Bare `rtsp::` is *not* flagged: it is the legitimate `rtflow::rtsp` module
# (e.g. `rtsp::Message`, `rtsp::sdp`).
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

status=0
if hits=$(grep -rnE '\brtspflow\b|(^|[^:a-zA-Z_])rtsp::(Result|media|rtp|rtcp|recording|tls|server|client|mount|error)\b' \
    README.md docs --include='*.md' --exclude-dir=archive || true); then
  if [ -n "$hits" ]; then
    echo "error: stale crate/module references (use rtflow:: instead):"
    echo "$hits"
    status=1
  fi
fi

if [ "$status" -eq 0 ]; then
  echo "docs-paths: ok"
fi
exit "$status"
