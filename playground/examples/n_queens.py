# SPDX-License-Identifier: Apache-2.0
"""n_queens.py — backtracking N-queens for N=6.

Stresses **cross-frame cache invalidation + symmetric tail mutations**.
Each recursive call appends a queen position, recurses, then pops if
the recursion fails. The same ``queens`` list is shared across many
frames; the parent's cache and the child's cache both reference it;
mutations need to propagate cleanly across frames via
``mark_dirty_across_frame_caches``.

Expected trace shape: ~equal Grown alias (append) and dirty_reconciled
(pop) counts. Every recursive call exercises the cross-frame dirty
marking. If the per-frame cache lifecycle has bugs, this is where they
show up — wrong dirty propagation, missed mutations, incorrect cache
state after pop.

N=6 has exactly 4 distinct solutions.
"""
from __future__ import annotations

import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "n_queens.hindsight")

import hindsight


def is_safe(queens: list[int], col: int) -> bool:
    row = len(queens)
    for prev_row, prev_col in enumerate(queens):
        if prev_col == col:
            return False
        if abs(prev_col - col) == row - prev_row:
            return False
    return True


def solve(n: int, queens: list[int], solutions: list[list[int]]) -> None:
    if len(queens) == n:
        solutions.append(list(queens))
        return
    for col in range(n):
        if is_safe(queens, col):
            queens.append(col)
            solve(n, queens, solutions)
            queens.pop()


@hindsight.record
def main() -> list[list[int]]:
    solutions: list[list[int]] = []
    solve(6, [], solutions)
    hindsight.note("n_queens complete", n=6, solutions_found=len(solutions))
    return solutions


if __name__ == "__main__":
    out = main()
    print(f"found {len(out)} solutions for N=6: {out}")
    assert len(out) == 4, f"expected 4 solutions for N=6, got {len(out)}"
