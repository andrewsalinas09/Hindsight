# SPDX-License-Identifier: Apache-2.0
"""data_processing.py — filter+aggregate over a list of dicts, with a bug.

The pipeline takes a list of order dicts, filters down to "shipped"
orders, and sums their totals. There's a real bug: a misspelled key
(`"totals"` instead of `"total"`) causes `.get(...)` to return `None`,
and the running sum silently treats those orders as zero.

The bug is invisible from the output because `sum_shipped_revenue` will
just return a smaller number than expected — there's no exception, no
"missing key" warning. The trace, however, makes it obvious: the
LINE_DELTA on the summing line shows `revenue` failing to grow even
though the order WAS recognized as shipped.

A productive query: walk-backward to see what the running `revenue`
was at each LINE_DELTA inside the loop, vs. how many orders were seen.
"""

from __future__ import annotations

import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "data_processing.hindsight")

import hindsight


SAMPLE_ORDERS = [
    {"id": 1, "status": "shipped", "total": 19.99},
    {"id": 2, "status": "pending", "total": 5.00},
    {"id": 3, "status": "shipped", "total": 42.50},
    {"id": 4, "status": "cancelled", "total": 99.99},
    {"id": 5, "status": "shipped", "total": 7.25},
]


def is_shipped(order: dict) -> bool:
    return order.get("status") == "shipped"


@hindsight.record
def sum_shipped_revenue(orders: list[dict]) -> float:
    revenue = 0.0
    counted = 0
    for order in orders:
        if not is_shipped(order):
            continue
        # BUG: typo "totals" — the real key is "total".
        # .get() returns None on miss, and `None + 0.0` would raise,
        # so we coerce to 0.0. That coercion is what hides the bug:
        # we keep counting the order but never add its money.
        amount = order.get("totals") or 0.0
        revenue += amount
        counted += 1
    hindsight.note("done", orders_seen=len(orders), shipped_counted=counted, revenue=revenue)
    return revenue


if __name__ == "__main__":
    expected = 19.99 + 42.50 + 7.25  # what an honest impl would return
    actual = sum_shipped_revenue(SAMPLE_ORDERS)
    print(f"expected: {expected:.2f}")
    print(f"actual:   {actual:.2f}")
