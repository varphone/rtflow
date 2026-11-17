#!/usr/bin/env python3
"""Enforce the rtflow workspace layering rules.

The workspace split made the module layers physical: each layer is a crate in
`crates/`, and Cargo itself rejects an upward dependency. This script adds a
readable check on top:

1. Every `crates/*/Cargo.toml` may only depend on crates in a strictly *lower*
   layer (the umbrella crate `rtflow` sits above everything).
2. The internal dependency graph must be acyclic.

Usage
-----
    python3 scripts/check-layering.py            # fail on violations
    python3 scripts/check-layering.py --matrix   # also print the edge matrix
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CRATES = ROOT / "crates"

# Layer of every workspace crate. A crate may depend only on lower layers.
LAYERS: dict[str, int] = {
    "rtflow-core": 0,
    "rtflow-media": 1,
    "rtflow-rtp": 2,
    "rtflow-rtcp": 3,
    "rtflow-srtp": 3,
    "rtflow-multicast": 3,
    "rtflow-security": 3,
    "rtflow-rtsp": 4,
    "rtflow-runtime": 5,
    "rtflow-framing": 6,
    "rtflow-mount": 6,
    "rtflow-pipeline": 6,
    "rtflow-http": 6,
    "rtflow-file": 6,
    "rtflow-storage": 7,
    "rtflow-metrics": 7,
    "rtflow-rtsp-client": 8,
    "rtflow-rtsp-server": 8,
    "rtflow-source": 8,
    "rtflow-hls": 9,
    "rtflow-dash": 10,
    "rtflow-http-flv": 9,
    "rtflow-rtmp": 9,
    "rtflow-srt": 9,
    "rtflow-onvif": 9,
    "rtflow-webrtc": 9,
    "rtflow-gst": 9,
    "rtflow-admin": 10,
    "rtflow-gateway": 10,
    "rtflow": 11,
    "rtflow-server": 12,
    "rtflow-testkit": 13,
}

# Internal crates may be declared as `name =` or as a workspace dependency
# (`[dependencies.rtflow-http]`), so both forms are recognised.
DEP_RE = re.compile(r'^(?:\[dependencies\.([a-z0-9_-]+)\]|([a-z0-9_-]*rtflow[a-z0-9_-]*)\s*=)', re.M)


def crate_dirs() -> dict[str, pathlib.Path]:
    found: dict[str, pathlib.Path] = {}
    for manifest in sorted(CRATES.glob("*/Cargo.toml")):
        text = manifest.read_text(encoding="utf-8")
        name = re.search(r'^name\s*=\s*"([^"]+)"', text, re.M)
        if name:
            # The package name is canonical; fall back to the directory when a
            # manifest inherits it from `[workspace.package]`.
            found[name.group(1)] = manifest
    return found


def dependencies(manifest: pathlib.Path) -> set[str]:
    """Internal crate dependencies (runtime only; dev-dependencies are skipped)."""
    text = manifest.read_text(encoding="utf-8")
    cut = text.find("[dev-dependencies]")
    runtime = text if cut < 0 else text[:cut]
    deps = {
        (table or inline).replace("_", "-")
        for table, inline in DEP_RE.findall(runtime)
        if (table or inline).startswith("rtflow")
    }
    return {dep for dep in deps if dep != manifest.parent.name}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--matrix", action="store_true")
    args = parser.parse_args()

    manifests = crate_dirs()
    edges: dict[str, set[str]] = {}
    problems = 0
    unknown: set[str] = set()
    for name, manifest in manifests.items():
        deps = dependencies(manifest)
        edges[name] = deps
        for dep in deps:
            if dep not in manifests:
                unknown.add(f"{name} -> {dep}")
                continue
            if dep == name:
                continue
            if LAYERS.get(dep, 99) >= LAYERS.get(name, 99):
                print(
                    f"error: {name} (layer {LAYERS.get(name)}) depends on "
                    f"{dep} (layer {LAYERS.get(dep)}): dependencies must point downwards"
                )
                problems += 1
    for item in sorted(unknown):
        print(f"error: unknown workspace dependency {item}")
        problems += 1

    # Cycle detection (DFS with colouring).
    state: dict[str, int] = {}

    def visit(node: str, path: list[str]) -> None:
        nonlocal problems
        colour = state.get(node, 0)
        if colour == 1:
            print("error: dependency cycle: " + " -> ".join(path + [node]))
            problems += 1
            return
        if colour == 2:
            return
        state[node] = 1
        for dep in sorted(edges.get(node, ())):
            if dep in edges:
                visit(dep, path + [node])
        state[node] = 2

    for name in sorted(edges):
        visit(name, [])

    if args.matrix:
        for name in sorted(edges, key=lambda item: (LAYERS.get(item, 99), item)):
            deps = ", ".join(sorted(edges[name])) or "-"
            print(f"{LAYERS.get(name, 99):>2} {name:<22} -> {deps}")

    if problems:
        print(f"\n{problems} layering violation(s)")
        return 1
    print("layering: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
