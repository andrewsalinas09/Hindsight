# SPDX-License-Identifier: Apache-2.0
"""aggregator.py — streaming aggregation with a stateful class.

Stresses **the recorder's behavior on user-defined classes**. Custom
class instances aren't in the alias path's allowlist (only built-in
list/tuple/set/frozenset/dict are recognized) — they fall through to
the writer's summary fallback. Every line that observes ``self`` gets
summarized rather than aliased.

Expected trace shape: the ``self`` argument shows up as a Summary
(``<Aggregator object>``) per capture, no Grown aliases on
``self.records``, no mutation tracking on instance attributes. Useful
for understanding how the recorder degrades on user-class state.

Possibly motivates adding a ``__hindsight_track__`` opt-in protocol
for user classes that want per-attribute aliasing behavior.
"""
from __future__ import annotations

import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "aggregator.hindsight")

import hindsight


class Aggregator:
    def __init__(self) -> None:
        self.count = 0
        self.total = 0.0
        self.records: list[float] = []

    def feed(self, value: float) -> None:
        self.count += 1
        self.total += value
        self.records.append(value)

    def average(self) -> float:
        return self.total / self.count if self.count else 0.0


@hindsight.record
def main() -> tuple[float, int]:
    agg = Aggregator()
    for v in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]:
        agg.feed(v)
    avg = agg.average()
    hindsight.note(
        "aggregation complete",
        count=agg.count,
        total=agg.total,
        average=avg,
    )
    return avg, agg.count


if __name__ == "__main__":
    avg, count = main()
    print(f"averaged {count} values: {avg}")
    assert avg == 5.5, f"expected average 5.5, got {avg}"
    assert count == 10
