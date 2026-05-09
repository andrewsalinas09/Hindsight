# SPDX-License-Identifier: Apache-2.0
"""dijkstra.py — Dijkstra's shortest-path algorithm with heapq.

Stresses **heap operations writing to arbitrary positions**. Each
``heappush`` percolates up, each ``heappop`` percolates down — both
fire multiple STORE_SUBSCR events on the heap list, marking it dirty
per heap op. Plus the distances dict gets STORE_SUBSCR for every
relaxation.

Combines the in-place mutation pattern of quicksort with the
dict-insertion pattern of memo_fib in a single algorithm — a realistic
multi-container production workload.

Expected trace shape: dirty_reconciled aliases on the heap list per
operation, fresh dict captures per distance update. Recording overhead
proportional to (heap-ops + distance-updates), tractable for small
graphs.
"""
from __future__ import annotations

import heapq
import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "dijkstra.hindsight")

import hindsight


def dijkstra(graph: dict[str, dict[str, int]], start: str) -> dict[str, float]:
    dist: dict[str, float] = {start: 0.0}
    heap: list[tuple[float, str]] = [(0.0, start)]
    while heap:
        d, node = heapq.heappop(heap)
        if d > dist.get(node, float("inf")):
            continue
        for neighbor, weight in graph.get(node, {}).items():
            new_dist = d + weight
            if new_dist < dist.get(neighbor, float("inf")):
                dist[neighbor] = new_dist
                heapq.heappush(heap, (new_dist, neighbor))
    return dist


@hindsight.record
def main() -> dict[str, float]:
    # Small weighted graph. The interesting property: A→B direct is 4,
    # but A→C→B is 1+2=3, so the algorithm must prefer the indirect
    # path. Same for A→D (direct is uncomputed; via C→B→D is 1+2+1=4).
    graph: dict[str, dict[str, int]] = {
        "A": {"B": 4, "C": 1},
        "B": {"D": 1, "C": 2},
        "C": {"B": 2, "D": 5},
        "D": {"E": 3},
        "E": {},
    }
    dist = dijkstra(graph, "A")
    hindsight.note(
        "dijkstra complete",
        nodes=len(dist),
        start="A",
        dist_to_E=dist.get("E", -1.0),
    )
    return dist


if __name__ == "__main__":
    out = main()
    print(f"distances from A: {dict(sorted(out.items()))}")
    assert out["A"] == 0.0
    assert out["B"] == 3.0, f"expected B=3 (via C), got {out['B']}"
    assert out["C"] == 1.0
    assert out["D"] == 4.0
    assert out["E"] == 7.0
