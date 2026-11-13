#!/usr/bin/env bash
# Reject stale crate names and module paths in user-facing documentation.
#
# Historically the crate was called `rtspflow`/`rtsp`; docs and README kept
# referencing `rtsp::...` and `rtspflow` long after the rename to `rtflow`.
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

status=0
# `rtsp::` is the crate name used in old doc snippets; the current crate is
# `rtflow` (the RTSP *module* is `rtflow::rtsp`, which is fine).
if hits=$(grep -rnE '\brtspflow\b|(^|[^a-z_])rtsp::' README.md docs --include='*.md' \
    --exclude-dir=archive || true); then
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
