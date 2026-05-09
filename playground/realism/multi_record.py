# SPDX-License-Identifier: Apache-2.0
"""multi_record.py — two ``@record`` entry points called sequentially.

The recorder uses a module-global ``_state``; if the cleanup at one
recording's end is incomplete, the second recording would either crash
on activation or end up with stale frame info / cache state. This
script exercises:

- Two sequential ``@record`` calls produce two distinct trace files.
- Calling a ``@record`` function from inside another active recording
  passes through (nested ``@record`` is documented as no-op).
- Sharing a helper function between both records does not leak state.
"""
from __future__ import annotations

import os

# Distinct output paths so we can index both. The recorder reads
# HINDSIGHT_OUTPUT_PATH at the start of each call, so swapping the env
# var between calls is the supported way to direct multiple recordings.

import hindsight


def shared_helper(n: int) -> int:
    return n * 2 + 1


@hindsight.record
def first(n: int) -> int:
    a = shared_helper(n)
    b = shared_helper(a)
    return a + b


@hindsight.record
def second(n: int) -> int:
    a = shared_helper(n + 1)
    b = shared_helper(a + 1)
    return a + b


@hindsight.record
def outer(n: int) -> int:
    # Nested @record should pass through and produce no new trace.
    return first(n) + 100


if __name__ == "__main__":
    os.environ["HINDSIGHT_OUTPUT_PATH"] = "first.hindsight"
    print("first(3) =", first(3))

    os.environ["HINDSIGHT_OUTPUT_PATH"] = "second.hindsight"
    print("second(3) =", second(3))

    os.environ["HINDSIGHT_OUTPUT_PATH"] = "outer.hindsight"
    print("outer(3) =", outer(3))
