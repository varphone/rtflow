#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
python3 scripts/check-layering.py --baseline scripts/layering-baseline.json
python3 scripts/check-tree.py
./scripts/check-docs-paths.sh
cargo package --allow-dirty --list >/dev/null
cargo package --allow-dirty
