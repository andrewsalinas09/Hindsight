# SPDX-License-Identifier: Apache-2.0
"""quicksort.py — in-place quicksort over a 100-element list.

The "evil twin" of merge_sort.py — same problem, opposite implementation.
Stresses **mid-prefix mutation in tight loops**: each partition step does
``arr[i], arr[j] = arr[j], arr[i]`` — two STORE_SUBSCR opcodes that the
mutation tracker observes, marking the array dirty. Next capture sees
length unchanged and falls through to a full re-walk → ``dirty_reconciled``.

Expected trace shape: many ``dirty_reconciled`` aliases, many
``content_exact`` lists, recording overhead measurably worse than
merge_sort despite native quicksort being faster than merge sort. This
is the canonical motivation for the v0.4 Patch alias variant.
"""
from __future__ import annotations

import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "quicksort.hindsight")

import hindsight


def partition(arr: list[int], lo: int, hi: int) -> int:
    pivot = arr[hi]
    i = lo - 1
    for j in range(lo, hi):
        if arr[j] <= pivot:
            i += 1
            arr[i], arr[j] = arr[j], arr[i]
    arr[i + 1], arr[hi] = arr[hi], arr[i + 1]
    return i + 1


def quicksort(arr: list[int], lo: int, hi: int) -> None:
    if lo < hi:
        p = partition(arr, lo, hi)
        quicksort(arr, lo, p - 1)
        quicksort(arr, p + 1, hi)


@hindsight.record
def main() -> list[int]:
    pattern: list[int] = []
    for i in range(50):
        pattern.append(i + 50)
        pattern.append(i)
    quicksort(pattern, 0, len(pattern) - 1)
    is_sorted = all(pattern[i] <= pattern[i + 1] for i in range(len(pattern) - 1))
    hindsight.note(
        "quicksort complete",
        n=len(pattern),
        first=pattern[0],
        last=pattern[-1],
        is_sorted=is_sorted,
    )
    return pattern


if __name__ == "__main__":
    out = main()
    print(f"sorted: first={out[0]}, last={out[-1]}, n={len(out)}")
    assert out == sorted(out), "quicksort produced wrong output!"
