#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
python3 scripts/check-layering.py
python3 scripts/check-tree.py
./scripts/check-docs-paths.sh
cargo package --workspace --allow-dirty --list >/dev/null
cargo package --workspace --allow-dirty
