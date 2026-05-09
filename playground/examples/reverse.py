# SPDX-License-Identifier: Apache-2.0
"""reverse.py — in-place list reverse via two-pointer swap.

Stresses **STORE_SUBSCR + tracker + dirty + re-walk in isolation**.
Pure two-pointer swap loop, no recursion, no other containers. The
controlled experiment for measuring the v0.4 Patch-alias-needed cost
without confounders.

Expected trace shape: ~50 dirty_reconciled aliases for N=100 (one per
swap iteration). Recording time linear in number of swaps but each
swap pays a full O(N) re-walk → recorder runs O(N²) for this O(N)
algorithm. Smallest possible reproducer of the mid-prefix mutation
cost.
"""
from __future__ import annotations

import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "reverse.hindsight")

import hindsight


def reverse_in_place(arr: list[int]) -> None:
    i, j = 0, len(arr) - 1
    while i < j:
        arr[i], arr[j] = arr[j], arr[i]
        i += 1
        j -= 1


@hindsight.record
def main() -> list[int]:
    arr = list(range(100))
    reverse_in_place(arr)
    hindsight.note("reverse complete", n=len(arr), first=arr[0], last=arr[-1])
    return arr


if __name__ == "__main__":
    out = main()
    print(f"reversed: first={out[0]}, last={out[-1]}, n={len(out)}")
    assert out == list(range(99, -1, -1)), "reverse produced wrong output!"
