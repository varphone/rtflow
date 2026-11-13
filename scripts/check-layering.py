#!/usr/bin/env python3
"""Enforce the rtflow module layering rules.

Rules
-----
1. A module may only depend on modules in a *lower* layer (higher number ->
   lower number). Depending on a module in a higher layer is a violation.
2. Same-layer cross-group dependencies are allowed but reported, because they
   usually mean two peers should be split or moved.
3. The dependency graph must be acyclic: every strongly connected component
   (SCC) with more than one node is a violation, regardless of layers.

Dependencies are extracted from `crate::...` paths and `use crate::{...}`
statements in non-test code; line comments, doc comments and `#[cfg(test)]`
modules are ignored.

Usage
-----
    python3 scripts/check-layering.py            # fail on violations
    python3 scripts/check-layering.py --matrix   # also print the edge matrix
    python3 scripts/check-layering.py --write-baseline
    python3 scripts/check-layering.py --baseline scripts/layering-baseline.json

`--baseline` only fails when a violation is *not* listed in the baseline, so
existing debt can be burned down incrementally (P1-P3 of the layout plan).
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = ROOT / "src"
BASELINE = ROOT / "scripts" / "layering-baseline.json"

# Layer assignment of the *target* layout (see docs/architecture/modules.md).
LAYERS: dict[str, int] = {
    "core": 0,
    "media": 1,
    "segment": 2,
    "rtsp": 2,
    "rtp": 2,
    "rtcp": 2,
    "srtp": 2,
    "security": 2,
    "runtime": 3,
    "pipeline": 3,
    "mount": 3,
    "file": 3,
    "http": 4,
    "client": 4,
    "proxy": 4,
    "metrics": 5,
    "server": 6,
    "storage": 6,
    "protocols": 6,
    "gateway": 7,
}

# crate-level module name -> target group. Keeps the check useful while the
# physical layout is still being migrated (P1 moves files, not public paths).
ALIASES: dict[str, str] = {
    "error": "core",
    "identity": "core",
    "observe": "core",
    "auth": "rtsp",
    "transport": "rtsp",
    "token_auth": "rtsp",
    "multicast": "rtsp",
    "rtsp": "rtsp",
    "protocol": "rtsp",
    "sdp": "rtsp",
    "range": "rtsp",
    "hls": "protocols",
    "dash": "protocols",
    "http_flv": "protocols",
    "rtmp": "protocols",
    "srt": "protocols",
    "webrtc": "protocols",
    "gst": "protocols",
    "pipeline": "pipeline",
    "mount": "mount",
    "metrics": "metrics",
    "http": "http",
    "client": "client",
    "proxy": "proxy",
    "server": "server",
    "gateway": "gateway",
    "file": "file",
    "storage": "storage",
    "media": "media",
    "rtp": "rtp",
    "rtcp": "rtcp",
    "srtp": "srtp",
    "security": "security",
    "runtime": "runtime",
}

PATH_GROUPS: dict[str, str] = {
    "media": "media",
    "protocol": "rtsp",
    "rtsp": "rtsp",
    "protocols": "protocols",
    "pipeline": "pipeline",
    "mount": "mount",
    "metrics": "metrics",
    "http": "http",
    "client": "client",
    "proxy": "proxy",
    "server": "server",
    "gateway": "gateway",
    "file": "file",
    "storage": "storage",
    "rtp": "rtp",
    "rtcp": "rtcp",
    "srtp": "srtp",
    "security": "security",
    "runtime": "runtime",
}

ROOT_FILE_GROUPS = {
    "error": "core",
    "identity": "core",
    "observe": "core",
    "mount": "mount",
}

USE_CREATE_RE = re.compile(r"use\s+crate::[^;]*;", re.S)
CRATE_PATH_RE = re.compile(r"crate::([a-z][a-z0-9_]*)")
MODULE_PATH_RE = re.compile(r"\b([a-z][a-z0-9_]*)::")


def strip_comments(text: str) -> str:
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


def module_group(path: pathlib.Path) -> str | None:
    rel = path.relative_to(SRC)
    if len(rel.parts) == 1:
        return ROOT_FILE_GROUPS.get(rel.stem)
    first = rel.parts[0]
    return PATH_GROUPS.get(first)


def referenced_modules(text: str) -> set[str]:
    names: set[str] = set(CRATE_PATH_RE.findall(text))
    for statement in USE_CREATE_RE.findall(text):
        names.update(MODULE_PATH_RE.findall(statement))
    return names


def collect_edges() -> dict[tuple[str, str], int]:
    edges: dict[tuple[str, str], int] = {}
    for path in sorted(SRC.rglob("*.rs")):
        group = module_group(path)
        if group is None:  # lib.rs and future root helpers
            continue
        text = path.read_text(encoding="utf-8")
        cut = text.find("#[cfg(test)]")
        if cut > 0:
            text = text[:cut]
        text = strip_comments(text)
        for name in referenced_modules(text):
            target = ALIASES.get(name)
            if target is None or target == group:
                continue
            key = (group, target)
            edges[key] = edges.get(key, 0) + 1
    return edges


def strongly_connected(edges: dict[tuple[str, str], int]) -> list[list[str]]:
    graph: dict[str, set[str]] = {}
    nodes: set[str] = set()
    for (a, b) in edges:
        graph.setdefault(a, set()).add(b)
        nodes.update((a, b))

    index: dict[str, int] = {}
    low: dict[str, int] = {}
    stack: list[str] = []
    on_stack: set[str] = set()
    counter = [0]
    components: list[list[str]] = []

    def visit(node: str) -> None:
        index[node] = low[node] = counter[0]
        counter[0] += 1
        stack.append(node)
        on_stack.add(node)
        for nxt in graph.get(node, ()):
            if nxt not in index:
                visit(nxt)
                low[node] = min(low[node], low[nxt])
            elif nxt in on_stack:
                low[node] = min(low[node], index[nxt])
        if low[node] == index[node]:
            component: list[str] = []
            while True:
                top = stack.pop()
                on_stack.discard(top)
                component.append(top)
                if top == node:
                    break
            if len(component) > 1:
                components.append(sorted(component))

    for node in sorted(nodes):
        if node not in index:
            visit(node)
    return components


def violations(edges: dict[tuple[str, str], int]) -> dict[str, list]:
    upward = []
    same_layer = []
    for (a, b), count in sorted(edges.items()):
        la, lb = LAYERS.get(a), LAYERS.get(b)
        if la is None or lb is None:
            continue
        if la < lb:
            upward.append([a, b, count])
        elif la == lb:
            same_layer.append([a, b, count])
    return {
        "cycles": strongly_connected(edges),
        "upward_edges": upward,
        "same_layer_edges": same_layer,
    }


def normalise(found: dict[str, list]) -> dict[str, list]:
    cycles = sorted(" <-> ".join(sorted(c)) for c in found["cycles"])
    upward = sorted(f"{a} -> {b}" for a, b, _ in found["upward_edges"])
    return {"cycles": cycles, "upward_edges": upward}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=pathlib.Path, help="tolerate debt listed here")
    parser.add_argument("--write-baseline", action="store_true")
    parser.add_argument("--matrix", action="store_true", help="print the edge matrix")
    args = parser.parse_args()

    edges = collect_edges()
    if args.matrix:
        print(f"{'from':<12} -> {'to':<12} count  layer")
        for (a, b), count in sorted(edges.items()):
            print(f"{a:<12} -> {b:<12} {count:5d}  {LAYERS.get(a)} -> {LAYERS.get(b)}")

    found = violations(edges)
    debt = normalise(found)

    if args.write_baseline:
        BASELINE.write_text(json.dumps(debt, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        print(f"wrote {BASELINE.relative_to(ROOT)}")
        return 0

    for edge in found["same_layer_edges"]:
        print(f"note: same-layer dependency {edge[0]} -> {edge[1]} ({edge[2]} refs)")

    problems = 0
    if args.baseline:
        allowed = json.loads(args.baseline.read_text(encoding="utf-8"))
        new_cycles = sorted(set(debt["cycles"]) - set(allowed.get("cycles", [])))
        new_upward = sorted(set(debt["upward_edges"]) - set(allowed.get("upward_edges", [])))
        stale = sorted(set(allowed.get("cycles", [])) - set(debt["cycles"])) + sorted(
            set(allowed.get("upward_edges", [])) - set(debt["upward_edges"])
        )
        for item in new_cycles:
            print(f"error: new dependency cycle: {item}")
            problems += 1
        for item in new_upward:
            print(f"error: new upward dependency: {item}")
            problems += 1
        for item in stale:
            print(f"note: baseline entry fixed, drop it from {args.baseline.name}: {item}")
    else:
        for item in debt["cycles"]:
            print(f"error: dependency cycle: {item}")
            problems += 1
        for item in debt["upward_edges"]:
            print(f"error: upward dependency: {item}")
            problems += 1

    if problems:
        print(f"\n{problems} layering violation(s)")
        return 1
    print("layering: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
