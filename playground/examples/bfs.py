# SPDX-License-Identifier: Apache-2.0
"""bfs.py — breadth-first search over a small directed graph.

Stresses **multiple tracked containers per frame, all mutating in
concert**. The cache holds 3+ entries (visited set, frontier deque,
order list); CALL events on ``.add``, ``.append``, ``.popleft`` mark
each container dirty independently; per-iteration the recorder
reconciles all three.

Expected trace shape: a richer alias mix than single-container tests.
- The set gets same-length aliases when no element is added (no growth path for sets).
- The deque (a `collections.deque`, not a builtin list) falls through to the summary path — useful sanity check that non-builtin containers degrade gracefully.
- The order list grows monotonically and gets Grown aliases.

If ``mark_dirty_across_frame_caches`` has any pathological scaling,
this test would catch it (multi-container per frame).
"""
from __future__ import annotations

import collections
import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "bfs.hindsight")

import hindsight


def bfs(graph: dict[str, list[str]], start: str) -> list[str]:
    visited: set[str] = {start}
    frontier: collections.deque[str] = collections.deque([start])
    order: list[str] = []
    while frontier:
        node = frontier.popleft()
        order.append(node)
        for neighbor in graph.get(node, []):
            if neighbor not in visited:
                visited.add(neighbor)
                frontier.append(neighbor)
    return order


@hindsight.record
def main() -> list[str]:
    # A small directed graph: rooms of a small house.
    graph: dict[str, list[str]] = {
        "entry": ["foyer"],
        "foyer": ["kitchen", "living"],
        "kitchen": ["pantry", "dining"],
        "living": ["dining", "study"],
        "dining": ["kitchen", "living"],
        "study": ["library"],
        "library": ["study"],
        "pantry": [],
    }
    order = bfs(graph, "entry")
    hindsight.note("bfs complete", visited=len(order), start="entry")
    return order


if __name__ == "__main__":
    out = main()
    print(f"visited {len(out)} rooms in order: {out}")
    assert out[0] == "entry"
    assert set(out) == {
        "entry",
        "foyer",
        "kitchen",
        "living",
        "dining",
        "study",
        "library",
        "pantry",
    }
