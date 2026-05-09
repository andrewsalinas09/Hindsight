# SPDX-License-Identifier: Apache-2.0
"""memo_fib.py — memoized Fibonacci for n=20.

Stresses **dict insertion tracking + recursive cross-frame mutability
of a long-lived dict**. Each cache miss does ``memo[n] = ...``  — a
STORE_SUBSCR on the dict that the mutation tracker observes, marking
it dirty. The current design has no Grown alias variant for dicts, so
each insertion forces a full re-walk → fresh inline dict capture.

Expected trace shape: many fresh inline dict captures, each
progressively larger. This is the dict-side equivalent of the perf.py
problem solved for lists. Concrete motivation for adding a Grown
alias variant for dicts in v0.4.

n=20 produces 20 cache misses; the memo dict ends with 20 entries.
Smaller than fib(30) to keep the trace manageable while still showing
the pattern.
"""
from __future__ import annotations

import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "memo_fib.hindsight")

import hindsight


def fib(n: int, memo: dict[int, int]) -> int:
    if n < 2:
        return n
    if n in memo:
        return memo[n]
    memo[n] = fib(n - 1, memo) + fib(n - 2, memo)
    return memo[n]


@hindsight.record
def main() -> int:
    memo: dict[int, int] = {}
    result = fib(20, memo)
    hindsight.note(
        "memo_fib complete",
        n=20,
        result=result,
        cache_size=len(memo),
    )
    return result


if __name__ == "__main__":
    out = main()
    print(f"fib(20) = {out}")
    assert out == 6765, f"expected fib(20)=6765, got {out}"
