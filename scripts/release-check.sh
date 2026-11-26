#!/usr/bin/env bash
# Release gate for the whole workspace.
#
# Requires: rustc >= the workspace MSRV, FFmpeg + GStreamer development files
# (the package verification build compiles rtflow-gst), and network access for
# cargo-deny's advisory database.
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

# ---- Version consistency -------------------------------------------------
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
if [[ -z "$version" ]]; then
    echo "release-check: cannot read the workspace version from Cargo.toml" >&2
    exit 1
fi
mismatched=$(grep -E 'path = "crates/' Cargo.toml | grep 'version = "' | grep -v "version = \"$version\"" || true)
if [[ -n "$mismatched" ]]; then
    echo "release-check: internal dependency version != $version:" >&2
    echo "$mismatched" >&2
    exit 1
fi
if ! grep -q "^## \[$version\]" CHANGELOG.md; then
    echo "release-check: CHANGELOG.md has no '## [$version]' section" >&2
    exit 1
fi
printf 'release-check: version %s consistent\n' "$version"

# ---- Formatting and lints ------------------------------------------------
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings

# ---- Tests ---------------------------------------------------------------
cargo test --workspace --all-targets --no-fail-fast
cargo test --workspace --all-features --no-fail-fast
cargo test -p rtflow --no-default-features --lib
cargo check -p rtflow --no-default-features --lib

# ---- Dependency policy ---------------------------------------------------
# advisories carries reviewed, documented ignores (see deny.toml).
cargo deny check advisories licenses bans sources

# ---- Documentation -------------------------------------------------------
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# ---- Layout guardrails ---------------------------------------------------
python3 scripts/check-layering.py
python3 scripts/check-tree.py
./scripts/check-docs-paths.sh

# ---- Packaging -----------------------------------------------------------
# Members mix `publish = true`, `["crates-io"]` and `false`, so the target
# registry must be explicit.
cargo package --workspace --allow-dirty --list --registry crates-io >/dev/null
cargo package --workspace --allow-dirty --registry crates-io

printf 'release-check: OK (%s)\n' "$version"
