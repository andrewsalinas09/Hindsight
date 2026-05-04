# SPDX-License-Identifier: Apache-2.0
"""recursion.py — naive Fibonacci to exercise the call tree.

Naive recursive Fibonacci is a textbook example of a recursive call
tree. `fib(6)` makes 25 calls in total: every node spawns two children
and the duplication is what makes the function exponential. After
recording, the `frames` table has one row per call, with `call_index`
counting how many times `fib` has been called so far and `parent_frame_id`
giving you the tree structure.

There's no bug here. The point is to query a real call tree:
- count the calls per qualified name (frames GROUP BY)
- walk the tree from the root frame (recursive CTE)
- find calls with a specific argument (frames WHERE argument_summary LIKE ...)
"""

from __future__ import annotations

import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "recursion.hindsight")

import hindsight


def fib(n: int) -> int:
    if n < 2:
        # Leaf nodes — no recursive calls below this point.
        return n
    return fib(n - 1) + fib(n - 2)


@hindsight.record
def main(target: int) -> int:
    hindsight.note("starting fib", n=target)
    result = fib(target)
    hindsight.note("fib finished", n=target, value=result)
    return result


if __name__ == "__main__":
    target = 6
    print(f"fib({target}) =", main(target))
