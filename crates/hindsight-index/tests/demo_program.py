# SPDX-License-Identifier: Apache-2.0
"""Demo program exercised by the indexer for the demo-query battery.

When run, this script produces a `.hindsight` trace containing:

- a top-level `demo()` recorded function with several locals
- recursion via `compute()` calls
- a branch (`if item > 3`)
- a loop (`for item in items`)
- a `hindsight.note(...)` with kwargs
- an exception (ZeroDivisionError) caught and handled

The trace is written to `$HINDSIGHT_OUTPUT_PATH` (default `trace.hindsight`).
"""

from __future__ import annotations

import hindsight


@hindsight.record
def demo(threshold: int = 3) -> int:
    items = [1, 2, 3, 4, 5]
    total = 0
    for item in items:
        if item > threshold:
            hindsight.note("filtering large item", item=item)
            continue
        total += item

    try:
        result = compute(total, divisor=0)
    except ZeroDivisionError:
        result = 0

    return result


def compute(total: int, divisor: int) -> float:
    return total / divisor


if __name__ == "__main__":
    print(demo())
