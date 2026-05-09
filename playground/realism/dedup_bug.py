# SPDX-License-Identifier: Apache-2.0
"""dedup_bug.py — a real-shaped bug for hindsight to find.

Pattern: a "dedup" helper uses a mutable default argument as the
"already seen" set. Looks innocent. Gets shared across all calls
because Python evaluates default arguments *once at def time*, not at
each call. Real production code has been written with exactly this
shape.

Expected behavior of the demo:
- ``process_batch([1, 2, 3])`` returns ``[1, 2, 3]`` — first time we
  see those items.
- ``process_batch([1, 2, 3])`` again, in a fresh-feeling new batch,
  *should* also return ``[1, 2, 3]``.

Actual behavior:
- The second call returns ``[]``. Every item has already been
  ``collect_unique``-checked because ``seen`` persists across batches.

Hindsight should let us prove this from the trace alone: the ``seen``
parameter at every entry to ``collect_unique`` should be the same
underlying object (same value_id), accumulating items across calls.
"""
from __future__ import annotations

import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "dedup_bug.hindsight")

import hindsight


def collect_unique(x: int, seen: set[int] = set()) -> bool:
    """Return True iff ``x`` hasn't been seen before.

    The bug: ``seen=set()`` is evaluated once when the function is
    defined. Every call to ``collect_unique`` without an explicit
    ``seen=`` argument receives the *same* set object — they all
    accumulate into one shared bucket.
    """
    if x in seen:
        return False
    seen.add(x)
    return True


def process_batch(items: list[int]) -> list[int]:
    """Filter ``items`` to those we haven't seen before."""
    accepted: list[int] = []
    for item in items:
        if collect_unique(item):
            accepted.append(item)
    return accepted


@hindsight.record
def main() -> tuple[list[int], list[int]]:
    batch1 = process_batch([1, 2, 3])
    batch2 = process_batch([1, 2, 3])
    return batch1, batch2


if __name__ == "__main__":
    a, b = main()
    print(f"batch1 = {a}")
    print(f"batch2 = {b}")
    if a != b:
        print(f"BUG: expected batch1 == batch2, got {a!r} != {b!r}")
