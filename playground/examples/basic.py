# SPDX-License-Identifier: Apache-2.0
"""basic.py — a function with a subtle comparison bug.

The function `find_largest_below(values, threshold)` is supposed to return
the largest element strictly less than `threshold`. It has a one-character
bug: `<=` instead of `<`. On most inputs it returns a plausible-looking
answer that happens to be wrong only when one of the inputs equals the
threshold exactly.

The point of this example is to feel what it's like to debug with the
trace. After you record and index, query the LINE_DELTAs for the loop
to see which `item` values were considered, and which `largest` updates
happened. The bug becomes visible the moment you see a `largest` value
update to the threshold itself.
"""

from __future__ import annotations

import os

# Pin a stable trace name so the playground walkthrough can reference
# `basic.hindsight` directly. Without this, the recorder defaults to a
# timestamped per-recording filename. `setdefault` keeps an explicit
# user-set HINDSIGHT_OUTPUT_PATH winning.
os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "basic.hindsight")

import hindsight


@hindsight.record
def find_largest_below(values: list[int], threshold: int) -> int | None:
    largest: int | None = None
    for item in values:
        # BUG: should be `<`, not `<=`. Equal-to-threshold items
        # incorrectly qualify as "below" and shadow the real answer.
        if item <= threshold:
            if largest is None or item > largest:
                largest = item
    hindsight.note("scan complete", checked=len(values), winner=largest)
    return largest


if __name__ == "__main__":
    # The "obvious" call: every value is well below the threshold, so the
    # bug doesn't surface. Returns 9 either way.
    print("clean run:", find_largest_below([3, 7, 1, 9, 4], 100))

    # The triggering call: the threshold IS one of the values. The
    # function should return 9 (the largest strictly below 10) but the
    # bug makes it return 10.
    print("buggy run:", find_largest_below([3, 7, 1, 9, 4, 10], 10))
