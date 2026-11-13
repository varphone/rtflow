#!/usr/bin/env python3
"""Structural guardrail for the repository layout.

Checks three invariants that the layout plan (P1-P5) burns down:

1. `#[path = "..."]` attributes in `src/` — a module's file must match its
   module path.
2. Files larger than 800 lines — god modules are split over time.
3. Unexpected top-level repository entries — the root stays tidy.

Existing debt lives in `scripts/tree-baseline.json`; the script only fails on
*new* debt and prints a note when a baseline entry has been paid off (remove
it from the baseline then). Generate it with `--write-baseline`.

Usage:
    python3 scripts/check-tree.py
    python3 scripts/check-tree.py --write-baseline
"""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
BASELINE = ROOT / "scripts" / "tree-baseline.json"
MAX_LINES = 800
TRACKED_PREFIXES = ("src/",)


def tracked_files() -> list[str]:
    out = subprocess.run(
        ["git", "-C", str(ROOT), "ls-files", "-z"],
        check=True,
        capture_output=True,
    ).stdout
    return [p for p in out.decode().split("\0") if p]


def collect() -> dict[str, list[str]]:
    files = tracked_files()
    path_attributes = []
    oversized = []
    for rel in files:
        if not rel.endswith(".rs") or not rel.startswith(TRACKED_PREFIXES):
            continue
        path = ROOT / rel
        text = path.read_text(encoding="utf-8", errors="replace")
        if "#[path" in text:
            path_attributes.append(rel)
        lines = text.count("\n") + (0 if text.endswith("\n") else 1)
        if lines > MAX_LINES:
            oversized.append(f"{rel} ({lines} lines)")
    root_entries = sorted({rel.split("/")[0] for rel in files})
    return {
        "path_attributes": sorted(path_attributes),
        "oversized_files": sorted(oversized),
        "root_entries": root_entries,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write-baseline", action="store_true")
    args = parser.parse_args()

    current = collect()
    if args.write_baseline:
        BASELINE.write_text(json.dumps(current, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        print(f"wrote {BASELINE.relative_to(ROOT)}")
        return 0

    allowed = json.loads(BASELINE.read_text(encoding="utf-8"))
    problems = 0
    for key, label in (
        ("path_attributes", "#[path] attribute"),
        ("oversized_files", "oversized file"),
        ("root_entries", "unexpected root entry"),
    ):
        new = sorted(set(current[key]) - set(allowed.get(key, [])))
        stale = sorted(set(allowed.get(key, [])) - set(current[key]))
        for item in new:
            print(f"error: new {label}: {item}")
            problems += 1
        for item in stale:
            print(f"note: baseline entry fixed, drop it from {BASELINE.name}: {item}")

    if problems:
        print(f"\n{problems} layout violation(s)")
        return 1
    print("tree: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
