# SPDX-License-Identifier: Apache-2.0
"""numpy_demo.py — does the defaults exclusion actually work on real numpy?

Calls numpy from a recorded function. The trace should contain user-code
frames (the @record entry plus any helpers we wrote) but no frames from
inside numpy itself — those should be EXCLUDED via the
``defaults`` exclusion list, which expands to ``numpy.*`` etc.

If the trace ends up with thousands of numpy frames, the defaults
exclusion is broken (or pattern matching against numpy's actual
qualified names is missing something).
"""
from __future__ import annotations

import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "numpy_demo.hindsight")

import hindsight
import numpy as np


def normalize(arr: np.ndarray) -> np.ndarray:
    """User-code helper that delegates to numpy. Should appear in the
    trace; numpy's internals should not."""
    mean = arr.mean()
    std = arr.std()
    return (arr - mean) / std


def summarize(arr: np.ndarray) -> dict:
    """Another user-code helper. Builds a small dict of stats."""
    return {
        "n": int(arr.size),
        "mean": float(arr.mean()),
        "max": float(arr.max()),
        "min": float(arr.min()),
    }


@hindsight.record
def main() -> dict:
    raw = np.array([1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0])
    normalized = normalize(raw)
    stats = summarize(normalized)
    hindsight.note("numpy demo complete", **stats)
    return stats


if __name__ == "__main__":
    out = main()
    print(f"stats: {out}")
