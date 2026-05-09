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
    """`lst.append(x)` should fire CALL with arg0=lst, marking dirty.
    The next capture re-walks fully and emits ``dirty_reconciled`` — the
    mutation tracker correctly observed the change.

    Note: this *replaces* the Grown alias optimization for tracked
    mutations. That's the right tradeoff: dirty_reconciled is the
    higher-confidence label and represents an active re-verification.
    The Grown alias path remains useful for mutations the tracker
    *can't* see (e.g., ctypes, NumPy buffer ops)."""

    @hindsight.record
    def append_things():
        lst: list = []
        lst.append("a")
        lst.append("b")
        lst.append("c")
        return lst

    append_things()
    trace = read_trace(trace_path)
    list_entries = [v for v in trace["values"] if v["tag"] == 0x07]
    dirty = _aliases_with_confidence(trace, "dirty_reconciled")
    # Each append should produce a dirty_reconciled alias because CALL
    # on `.append` marked the list dirty before the next capture.
    assert len(dirty) >= 2, (
        f"appends should produce dirty_reconciled aliases (one per re-capture "
        f"after a tracked mutation), got {len(dirty)}"
    )
    # Underlying list captured inline at least once (first capture).
    assert len(list_entries) >= 1


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


def test_dict_update_marks_dirty(trace_path: Path):
    """`d[k] = v` is STORE_SUBSCR; `d.update(...)` is a CALL on a
    known-mutating method. Both should mark dirty."""

    @hindsight.record
    def update_dict():
        d = {"a": 1}
        d["b"] = 2  # STORE_SUBSCR
        d.update({"c": 3})  # CALL on update
        return d

    update_dict()
    trace = read_trace(trace_path)
    dirty = _aliases_with_confidence(trace, "dirty_reconciled")
    assert dirty, "dict mutation should produce dirty_reconciled aliases"


def test_non_mutating_call_does_not_force_re_walk(trace_path: Path):
    """`len(lst)` and `lst.copy()` are not mutating. The cache should
    stay clean and the alias path should keep emitting summary_observed
    (or content_exact for first captures) — not dirty_reconciled."""

    @hindsight.record
    def read_only_calls():
        lst = [1, 2, 3]
        n = len(lst)
        copy = lst.copy()
        return n, copy

    read_only_calls()
    trace = read_trace(trace_path)
    # If the tracker were over-eager it would flood with
    # dirty_reconciled. We expect at most a small number (the first
    # capture's content_exact + maybe some summary_observed aliases).
    dirty = _aliases_with_confidence(trace, "dirty_reconciled")
    # `lst.copy()` is not in METHOD_MUTATION_NAMES so it shouldn't
    # mark dirty. (`len()` doesn't dispatch as a method call on lst.)
    assert len(dirty) == 0, (
        f"non-mutating calls should not produce dirty_reconciled, got {len(dirty)}"
    )


def test_in_place_index_assignment_in_loop_each_capture_dirty_reconciled(
    trace_path: Path,
):
    """The motivating correctness regression case: `for i: lst[i] = ...`
    in a loop. Without the mutation tracker, every iteration would emit
    a summary_observed alias even though the contents changed. With the
    tracker, every iteration's capture sees the dirty flag and re-walks."""

    @hindsight.record
    def squarish():
        lst = [1, 2, 3, 4, 5]
        for i in range(len(lst)):
            lst[i] = lst[i] * 2
        return lst

    squarish()
    trace = read_trace(trace_path)
    dirty = _aliases_with_confidence(trace, "dirty_reconciled")
    summary = _aliases_with_confidence(trace, "summary_observed")
    # The 5-iteration loop should produce multiple dirty_reconciled
    # aliases as each STORE_SUBSCR fires and the next LINE event walks
    # the list.
    assert len(dirty) >= 3, (
        f"in-place index loop should produce many dirty_reconciled aliases, "
        f"got {len(dirty)} (summary_observed: {len(summary)})"
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
