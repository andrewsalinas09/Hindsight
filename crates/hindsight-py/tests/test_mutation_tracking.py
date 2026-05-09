# SPDX-License-Identifier: Apache-2.0
"""Tests for the recorder's opcode-event mutation tracker (Stage 5).

The tracker is layered on top of the summary-fingerprint alias path.
What it adds:

- ``STORE_SUBSCR`` (``lst[i] = x``, ``dict[k] = v``) → cache marks the
  target container dirty; next capture re-walks and emits
  ``dirty_reconciled`` confidence.
- Method-call mutations (``lst.append(...)``, ``lst.sort()``,
  ``dict.update(...)``) → CALL event marks the receiver dirty.

What it correctly *avoids*:

- INSTRUCTION events at non-mutation offsets get ``DISABLE`` on first
  fire; steady-state cost is proportional to executed mutation
  instructions, not all instructions.
- Calls to non-mutating methods (``lst.copy()``, ``len(lst)``) don't
  mark anything dirty.

These tests assert on the recorded trace's confidence labels, since
that's the contract: mutations the tracker observed must show up as
``dirty_reconciled``; mutations it missed must show up as
``summary_observed`` (honest uncertainty).
"""

from __future__ import annotations

from pathlib import Path

import pytest

import hindsight
from hindsight._core import read_trace


@pytest.fixture
def trace_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    target = tmp_path / "out.hindsight"
    monkeypatch.setenv("HINDSIGHT_OUTPUT_PATH", str(target))
    return target


def _aliases_with_confidence(trace: dict, confidence: str) -> list[dict]:
    """Filter the trace's value entries to alias entries with the given
    confidence label. Returns the decoded dicts."""
    return [
        v["decoded"]
        for v in trace["values"]
        if v["tag"] == 0x15 and v["decoded"]["confidence"] == confidence
    ]


def test_store_subscr_marks_container_dirty_and_reconciles(trace_path: Path):
    """`lst[i] = x` should fire STORE_SUBSCR, mark `lst` dirty, and the
    next capture should re-walk + emit dirty_reconciled."""

    @hindsight.record
    def mutate_in_place():
        lst = [10, 20, 30]
        lst[1] = 99  # STORE_SUBSCR — should be tracked
        return lst

    mutate_in_place()
    trace = read_trace(trace_path)

    dirty = _aliases_with_confidence(trace, "dirty_reconciled")
    # At least one capture after the mutation should be dirty_reconciled.
    assert dirty, "expected at least one dirty_reconciled alias after STORE_SUBSCR"


def test_append_method_call_is_tracked(trace_path: Path):
    """`lst.append(x)` fires CALL → marks dirty → the next capture sees
    "dirty + length grew" and emits a Grown alias with
    ``mutation_tracked`` confidence (the fast path: only the new tail
    element is interned, no full re-walk).

    This is the v0.3 ship of the recorder-overhead design: appends are
    O(k) per iteration, not O(N). For mutations that aren't growth
    (sort, in-place index assign), the recorder falls back to a full
    re-walk and emits ``dirty_reconciled``."""

    @hindsight.record
    def append_things():
        lst: list = []
        lst.append("a")
        lst.append("b")
        lst.append("c")
        return lst

    append_things()
    trace = read_trace(trace_path)
    grown_mutation_tracked = [
        v["decoded"]
        for v in trace["values"]
        if v["tag"] == 0x15
        and v["decoded"]["alias_kind"] == "grown"
        and v["decoded"]["confidence"] == "mutation_tracked"
    ]
    # Each tracked append should produce a Grown alias with
    # mutation_tracked confidence.
    assert len(grown_mutation_tracked) >= 2, (
        f"appends should produce mutation_tracked Grown aliases, got {len(grown_mutation_tracked)}"
    )


def test_sort_method_marks_dirty(trace_path: Path):
    """`lst.sort()` is the canonical mid-prefix mutation that the
    summary fingerprint can't catch. With the mutation tracker, the
    next capture should re-walk and report dirty_reconciled."""

    @hindsight.record
    def sort_in_place():
        lst = [3, 1, 4, 1, 5, 9, 2, 6]
        lst.sort()  # In-place mutation, no length change.
        return lst

    sort_in_place()
    trace = read_trace(trace_path)
    dirty = _aliases_with_confidence(trace, "dirty_reconciled")
    assert dirty, "lst.sort() should produce dirty_reconciled aliases"


def test_dict_update_emits_mutation_tracked_aliases(trace_path: Path):
    """`d[k] = v` is STORE_SUBSCR (with statically-pinned container+key);
    `d.update(...)` is a CALL on a known-mutating method. Both should be
    observed as mutations and trigger the dict-Grown alias path
    (mutation_tracked confidence) rather than fall through to a full
    re-walk (dirty_reconciled)."""

    @hindsight.record
    def update_dict():
        d = {"a": 1}
        d["b"] = 2  # STORE_SUBSCR — observed → dirty → length grew → Grown alias.
        d.update({"c": 3})  # CALL — observed → dirty → length grew → Grown alias.
        return d

    update_dict()
    trace = read_trace(trace_path)
    tracked = _aliases_with_confidence(trace, "mutation_tracked")
    assert tracked, (
        "dict mutation with growth should produce mutation_tracked Grown aliases"
    )


def test_non_mutating_call_does_not_force_re_walk(trace_path: Path):
    """`len(lst)` is in the module-function readonly allowlist — it does
    not mark dirty, so the alias path stays clean. Other readonly
    helpers (``sorted(lst)``, ``sum(lst)``) behave the same way."""

    @hindsight.record
    def read_only_calls():
        lst = [1, 2, 3]
        n = len(lst)
        s = sum(lst)
        m = max(lst)
        return n, s, m

    read_only_calls()
    trace = read_trace(trace_path)
    dirty = _aliases_with_confidence(trace, "dirty_reconciled")
    # len/sum/max are in MODULE_FUNCTION_READONLY → no dirty mark.
    assert len(dirty) == 0, (
        f"readonly-classified calls should not produce dirty_reconciled, "
        f"got {len(dirty)}"
    )


def test_in_place_index_assignment_in_loop_emits_patch_aliases(
    trace_path: Path,
):
    """The motivating correctness regression case: `for i: lst[i] = ...`
    in a loop. With v0.4's Patch alias path, each STORE_SUBSCR with
    statically-pinned container *and* key emits a Patch alias
    (mutation_tracked confidence) at the next capture — O(1) wire
    delta, not a full re-walk."""

    @hindsight.record
    def squarish():
        lst = [1, 2, 3, 4, 5]
        for i in range(len(lst)):
            lst[i] = lst[i] * 2
        return lst

    squarish()
    trace = read_trace(trace_path)
    tracked = _aliases_with_confidence(trace, "mutation_tracked")
    summary = _aliases_with_confidence(trace, "summary_observed")
    dirty = _aliases_with_confidence(trace, "dirty_reconciled")
    # 5 iterations × 1 STORE_SUBSCR = 5 Patch aliases (chained off the
    # previous capture). The exact count depends on per-LINE capture
    # ordering, but ≥3 is a safe lower bound that catches regressions.
    assert len(tracked) >= 3, (
        f"in-place index loop should produce mutation_tracked Patch "
        f"aliases, got {len(tracked)} "
        f"(dirty_reconciled: {len(dirty)}, summary_observed: {len(summary)})"
    )


def test_mutation_in_callee_marks_caller_cache(trace_path: Path):
    """A function that mutates a passed-in list affects the caller's
    cached snapshot of that list. When the caller's frame re-captures
    it, we should see dirty_reconciled — not a stale summary alias."""

    def mutate_helper(lst):
        lst.append("from_callee")

    @hindsight.record
    def caller():
        items = [1, 2, 3]
        mutate_helper(items)
        # Force a line in the caller after the mutation:
        sentinel = "after_helper"
        return items

    caller()
    trace = read_trace(trace_path)
    # Some capture in the caller's frame after `mutate_helper` returns
    # should be dirty_reconciled, because the cache for `items` was
    # marked dirty by the CALL event on `lst.append` even though the
    # call happened inside a non-recorded helper.
    dirty = _aliases_with_confidence(trace, "dirty_reconciled")
    grown = [
        v["decoded"]
        for v in trace["values"]
        if v["tag"] == 0x15 and v["decoded"]["alias_kind"] == "grown"
    ]
    # Either dirty_reconciled OR a Grown alias whose growth captures
    # the new tail correctly. Both outcomes preserve correctness; the
    # test is really "the trace reflects the appended item somewhere
    # rather than reporting an unchanged 3-item list."
    assert dirty or grown, (
        "callee-mutation should be reflected as either dirty_reconciled "
        "or a Grown alias capturing the new tail"
    )


def test_recording_with_no_mutations_pays_no_re_walk_cost(trace_path: Path):
    """A function that builds a list once and reads it many times
    should produce mostly content_exact (the first capture) and
    summary_observed aliases (subsequent captures), with zero
    dirty_reconciled — proving the DISABLE-on-first-fire trick keeps
    INSTRUCTION events from firing on non-mutation opcodes after the
    first execution."""

    @hindsight.record
    def read_only():
        lst = [1, 2, 3, 4, 5]
        a = lst[0]
        b = lst[1]
        c = lst[2]
        d = lst[3]
        e = lst[4]
        return a + b + c + d + e

    read_only()
    trace = read_trace(trace_path)
    dirty = _aliases_with_confidence(trace, "dirty_reconciled")
    assert len(dirty) == 0, (
        f"read-only function should produce no dirty_reconciled aliases, got {len(dirty)}"
    )
