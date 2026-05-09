# SPDX-License-Identifier: Apache-2.0
"""merge_sort.py — recursive merge sort over a 100-element list.

A recorder stress test that exercises a different shape of work than
the perf.py append loop:

- **Recursive call tree.** N=100 produces ~199 ``merge_sort`` calls and
  99 ``merge`` calls across log2(100) ≈ 7 levels of recursion. Each
  frame creates and discards its own per-frame container cache,
  exercising the cache lifecycle and cross-frame alias bookkeeping.

- **Container churn, not mutation.** Pure merge sort (the version here)
  doesn't mutate via ``arr[i] = x`` — it builds new lists by appending.
  The opcode-level mutation tracker stays quiet; the alias path does
  the heavy lifting via Grown aliases for each ``result.append(...)``.

- **Method-call dispatch.** ``result.extend(left[i:])`` is a CALL event
  on a known-mutating method. Verifies the CALL-based dirty-marking
  path works alongside ``.append`` tracking.

- **Slicing.** ``arr[:mid]`` and ``arr[mid:]`` produce fresh list
  objects with new ``id()`` — each gets a first-time inline-list
  capture in the callee's frame, then the callee accumulates results
  via Grown aliases.

After recording, useful questions to ask Claude:

- "What did the call tree look like? How deep did the recursion go?"
- "How many calls to ``merge`` happened, and what were their input
  sizes?"
- "Walk me through the first merge that combined two 4-element halves."
- "Is the algorithm correct — did the final list end up sorted?"
- "Look at the values table — how many of the captures came through
  the alias path vs. fresh inline lists?"
"""

from __future__ import annotations

import os

# Pin a stable trace name so the MCP server can address it predictably.
# Comment this line out (or unset ``HINDSIGHT_OUTPUT_PATH``) to land in
# the default traces directory ``~/.hindsight/traces/`` instead.
# os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "merge_sort.hindsight")

import hindsight


def merge(left: list[int], right: list[int]) -> list[int]:
    """Merge two pre-sorted lists into a single sorted list.

    The classic two-pointer merge: walk both inputs in parallel,
    appending the smaller front element each step, then drain whichever
    side has elements left over.
    """
    result: list[int] = []
    i = 0
    j = 0
    while i < len(left) and j < len(right):
        if left[i] <= right[j]:
            result.append(left[i])
            i += 1
        else:
            result.append(right[j])
            j += 1
    # Drain the non-empty side. Only one of these extends does anything
    # (the other is a no-op on an empty slice), but writing both keeps
    # the code symmetric.
    result.extend(left[i:])
    result.extend(right[j:])
    return result


def merge_sort(arr: list[int]) -> list[int]:
    """Sort ``arr`` by recursive split-and-merge. Pure: returns a new
    list, never mutates the input."""
    if len(arr) <= 1:
        return list(arr)
    mid = len(arr) // 2
    left = merge_sort(arr[:mid])
    right = merge_sort(arr[mid:])
    return merge(left, right)


@hindsight.record
def main() -> list[int]:
    # A deterministic shuffled-looking permutation of 0..99 — interleaves
    # the high and low halves so neither subtree gets a trivially-sorted
    # input. The first few elements are [50, 0, 51, 1, 52, 2, ...].
    pattern: list[int] = []
    for i in range(50):
        pattern.append(i + 50)
        pattern.append(i)

    sorted_arr = merge_sort(pattern)

    # Sanity-check note that lands in the trace's notes table — useful
    # for the LLM to confirm the algorithm finished correctly without
    # having to walk the full 100-element output.
    is_sorted = all(
        sorted_arr[i] <= sorted_arr[i + 1] for i in range(len(sorted_arr) - 1)
    )
    hindsight.note(
        "merge sort complete",
        n=len(sorted_arr),
        first=sorted_arr[0],
        last=sorted_arr[-1],
        is_sorted=is_sorted,
    )
    return sorted_arr


if __name__ == "__main__":
    out = main()
    print(f"sorted: first={out[0]}, last={out[-1]}, n={len(out)}")
    assert out == sorted(out), "merge sort produced wrong output!"
