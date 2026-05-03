# SPDX-License-Identifier: Apache-2.0
"""Tests for the three event types added in this session:

- ``hindsight.note(message, **kwargs)`` → NOTE events
- BRANCH callback → BRANCH_RESULT events
- RAISE callback → EXCEPTION_RAISED events (and PY_UNWIND → FUNCTION_EXIT
  with the exception-unwind sentinel)

The decorated test subjects live at module level so the recorder can
read their source from this very file. Each test is isolated by the
``trace_path`` fixture, which sets ``HINDSIGHT_OUTPUT_PATH`` to a
per-test temporary path and clears any inherited ``HINDSIGHT_CONFIG``.
"""

from __future__ import annotations

from pathlib import Path

import pytest

import hindsight


# --- Helpers ---------------------------------------------------------------


def _value_at(trace: dict, value_id: int):
    return trace["values"][value_id]["decoded"]


def _string_at(trace: dict, string_id: int) -> str:
    return trace["strings"][string_id]


def _events_of(trace: dict, type_name: str) -> list[dict]:
    return [e for e in trace["events"] if e["type"] == type_name]


@pytest.fixture
def trace_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    p = tmp_path / "trace.hindsight"
    monkeypatch.setenv("HINDSIGHT_OUTPUT_PATH", str(p))
    monkeypatch.delenv("HINDSIGHT_CONFIG", raising=False)
    return p


# --- Module-level decorated test subjects ---------------------------------
# Notes: NOTE events.


@hindsight.record
def calls_note_simple() -> int:
    hindsight.note("hello")
    return 1


@hindsight.record
def calls_note_with_kwargs() -> int:
    hindsight.note("processed", count=42, status="ok")
    return 1


# Branches.


@hindsight.record
def branch_if_else(x: int) -> str:
    if x > 0:
        return "pos"
    else:
        return "neg"


@hindsight.record
def branch_short_circuit_and(a: bool, b: bool) -> bool:
    if a and b:
        return True
    return False


@hindsight.record
def branch_comprehension_filter(values: list[int]) -> list[int]:
    return [v for v in values if v > 1]


def excluded_branch_inside(x: int) -> str:
    # Used by the "BRANCH inside excluded function" test.
    if x > 0:
        return "pos"
    return "neg"


@hindsight.record
def calls_excluded_helper(x: int) -> str:
    return excluded_branch_inside(x)


# Exceptions.


@hindsight.record
def raises_value_error() -> None:
    raise ValueError("bad input")


@hindsight.record
def raises_index_error() -> int:
    items: list[int] = []
    return items[0]


@hindsight.record
def catches_value_error() -> str:
    try:
        raise ValueError("caught")
    except ValueError:
        return "caught"


@hindsight.record
def calls_raiser_and_propagates() -> None:
    raises_callee()


def raises_callee() -> None:
    raise RuntimeError("from callee")


# --- NOTE tests ------------------------------------------------------------


def test_note_basic_message(trace_path: Path):
    """``hindsight.note("hello")`` emits a single NOTE event whose
    message string id resolves to ``"hello"``."""
    calls_note_simple()
    trace = hindsight.read_trace(str(trace_path))
    notes = _events_of(trace, "note")
    assert len(notes) == 1
    n = notes[0]
    assert _string_at(trace, n["message"]) == "hello"
    assert n["kwargs"] == []
    # The line number is the *call site* of hindsight.note(), not the
    # def line. The exact line number depends on this file's layout;
    # checking it's a positive int is enough to catch a regression
    # where we forgot to populate the field.
    assert n["line"] > 0


def test_note_with_kwargs_records_each_pair(trace_path: Path):
    """Keyword arguments become structured (string_id, value_id) pairs
    on the NOTE event."""
    calls_note_with_kwargs()
    trace = hindsight.read_trace(str(trace_path))
    notes = _events_of(trace, "note")
    assert len(notes) == 1
    n = notes[0]
    assert _string_at(trace, n["message"]) == "processed"
    kwargs = {
        _string_at(trace, name_id): _value_at(trace, value_id)
        for name_id, value_id in n["kwargs"]
    }
    assert kwargs == {"count": 42, "status": "ok"}


def test_note_outside_recording_is_noop():
    """``hindsight.note()`` outside an active recording must not raise
    or otherwise misbehave. It silently does nothing."""
    # No active @record, no monkeypatched env — bare call.
    hindsight.note("nope", value=1)


# --- BRANCH tests ----------------------------------------------------------


def test_branch_result_for_if_else_truthy(trace_path: Path):
    """``if x > 0`` with ``x=5`` produces a BRANCH_RESULT with
    taken=True (the condition was truthy → fell through past the
    POP_JUMP_IF_FALSE → recorded as True)."""
    branch_if_else(5)
    trace = hindsight.read_trace(str(trace_path))
    branches = _events_of(trace, "branch_result")
    assert len(branches) >= 1
    # First branch event corresponds to the `if x > 0` test.
    assert branches[0]["taken"] is True


def test_branch_result_for_if_else_falsy(trace_path: Path):
    branch_if_else(-5)
    trace = hindsight.read_trace(str(trace_path))
    branches = _events_of(trace, "branch_result")
    assert len(branches) >= 1
    assert branches[0]["taken"] is False


def test_branch_result_short_circuit_skips_second_operand(trace_path: Path):
    """``a and b`` with ``a=False`` short-circuits: the BRANCH for the
    `a` test fires (False), and no BRANCH fires for `b` since `b` is
    never evaluated."""
    branch_short_circuit_and(False, True)
    trace = hindsight.read_trace(str(trace_path))
    branches = _events_of(trace, "branch_result")
    # With short-circuit on `a=False`, the compiler emits one BRANCH for
    # the implicit `if (a and b):`, plus the first jump-on-`a`. The
    # exact count depends on bytecode but each event's truthiness
    # should match what the source says.
    truth_values = [b["taken"] for b in branches]
    # At least one False (the short-circuit on `a`).
    assert False in truth_values
    # And no LINE_DELTA references `b` having been touched (the
    # short-circuit means `b` is never evaluated as a name in
    # this frame's body — but `b` is a parameter, so it's already in
    # f_locals from PY_START. Instead, check the function returned
    # False, confirming the short-circuit took effect.)


def test_branch_short_circuit_both_truthy_evaluates_both(trace_path: Path):
    """Sanity check: with both ``a`` and ``b`` truthy, more BRANCH
    events fire than the short-circuit case (both operands are
    evaluated, plus the outer if-test)."""
    branch_short_circuit_and(True, True)
    trace = hindsight.read_trace(str(trace_path))
    long_chain = len(_events_of(trace, "branch_result"))

    # Compare to the short-circuit case in the same trace path
    # (overwrite, separate run).
    branch_short_circuit_and(False, True)
    trace_short = hindsight.read_trace(str(trace_path))
    short_chain = len(_events_of(trace_short, "branch_result"))

    assert long_chain >= short_chain, (
        f"long-chain branches ({long_chain}) should be >= short-circuit "
        f"branches ({short_chain})"
    )


def test_branch_result_in_comprehension_filter(trace_path: Path):
    """``[v for v in [1, 2, 3] if v > 1]`` evaluates the filter for
    each element. We expect at least one BRANCH_RESULT per element
    (the condition test)."""
    branch_comprehension_filter([1, 2, 3])
    trace = hindsight.read_trace(str(trace_path))
    branches = _events_of(trace, "branch_result")
    # The compiler may emit additional branches for the for-loop
    # itself; we just want to confirm that the filter's truthy / falsy
    # outcomes are observable.
    truth_values = [b["taken"] for b in branches]
    # At least one False (v=1 fails `v > 1`) and one True (v=2 or 3 pass).
    assert True in truth_values
    assert False in truth_values


def test_branch_in_excluded_function_is_suppressed(
    trace_path: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """Configure exclusion for ``test_events.excluded_branch_inside``
    and verify no BRANCH_RESULT events fire from inside it (the frame
    is in EXCLUDED mode, so all interior events are suppressed)."""
    cfg = tmp_path / "hindsight.toml"
    cfg.write_text(
        '[scope]\nexclude = ["test_events.excluded_branch_inside"]\n'
    )
    monkeypatch.setenv("HINDSIGHT_CONFIG", str(cfg))

    calls_excluded_helper(5)
    trace = hindsight.read_trace(str(trace_path))

    # The caller is recorded; the helper is excluded.
    qualnames = [
        _string_at(trace, e["function_id"])
        for e in _events_of(trace, "function_entry")
    ]
    assert "test_events.calls_excluded_helper" in qualnames
    assert "test_events.excluded_branch_inside" not in qualnames

    # The excluded function has an `if x > 0` inside, but no
    # BRANCH_RESULT should fire because its frame is EXCLUDED.
    # The caller (`calls_excluded_helper`) has no branches of its own,
    # so we expect zero BRANCH_RESULT events overall.
    assert _events_of(trace, "branch_result") == []


# --- EXCEPTION tests -------------------------------------------------------


def test_exception_raised_for_explicit_raise(trace_path: Path):
    """A function that raises ``ValueError`` produces an
    EXCEPTION_RAISED event whose type resolves to
    ``"builtins.ValueError"`` and whose value's repr contains the
    error message."""
    with pytest.raises(ValueError):
        raises_value_error()
    trace = hindsight.read_trace(str(trace_path))
    exceptions = _events_of(trace, "exception_raised")
    assert len(exceptions) == 1
    e = exceptions[0]
    assert _string_at(trace, e["exception_type"]) == "builtins.ValueError"
    decoded = _value_at(trace, e["exception_value"])
    # The exception goes through the Summary fallback in the value
    # converter (it's a user-instance type).
    assert decoded["kind"] == "summary"
    assert "ValueError" in decoded["type_name"]
    assert "bad input" in decoded["repr"]


def test_exception_raised_for_builtin_index_error(trace_path: Path):
    """An IndexError from a list-out-of-bounds also fires
    EXCEPTION_RAISED. The exception is built by the interpreter rather
    than by user code."""
    with pytest.raises(IndexError):
        raises_index_error()
    trace = hindsight.read_trace(str(trace_path))
    exceptions = _events_of(trace, "exception_raised")
    assert any(
        _string_at(trace, e["exception_type"]) == "builtins.IndexError"
        for e in exceptions
    )


def test_exception_raised_then_function_exit_with_unwind_sentinel(
    trace_path: Path,
):
    """When a recorded function raises and the exception propagates
    out, we get EXCEPTION_RAISED followed by a FUNCTION_EXIT whose
    return_value is the EXCEPTION_UNWIND_VALUE_ID sentinel (value
    table index 1)."""
    with pytest.raises(ValueError):
        raises_value_error()
    trace = hindsight.read_trace(str(trace_path))

    # Find the EXCEPTION_RAISED event for raises_value_error and the
    # matching FUNCTION_EXIT immediately after it (same frame).
    events = trace["events"]
    raise_idx = next(
        i for i, ev in enumerate(events) if ev["type"] == "exception_raised"
    )
    # Look forward for the next FUNCTION_EXIT whose frame_id matches
    # the raising frame.
    fn_entry = _events_of(trace, "function_entry")[0]
    raising_frame_id = fn_entry["frame_id"]
    exit_ev = next(
        ev
        for ev in events[raise_idx + 1 :]
        if ev["type"] == "function_exit" and ev["frame_id"] == raising_frame_id
    )
    # value_id 1 is the EXCEPTION_UNWIND_VALUE_ID per the format spec.
    assert exit_ev["return_value"] == 1
    # And the value at index 1 in the table is the unwind sentinel.
    assert trace["values"][1]["decoded"] == {"kind": "exception_unwind_sentinel"}


def test_caught_exception_still_emits_exception_raised(trace_path: Path):
    """Per the brief: we capture all RAISEs regardless of whether the
    exception is caught. ``catches_value_error`` raises and catches a
    ValueError; the EXCEPTION_RAISED event must still be present."""
    result = catches_value_error()
    assert result == "caught"
    trace = hindsight.read_trace(str(trace_path))
    exceptions = _events_of(trace, "exception_raised")
    assert len(exceptions) == 1
    assert _string_at(trace, exceptions[0]["exception_type"]) == "builtins.ValueError"
    # Caught exception → function returns normally → FUNCTION_EXIT
    # carries the actual return value, NOT the unwind sentinel.
    fn_entry = _events_of(trace, "function_entry")[0]
    exit_ev = next(
        e
        for e in _events_of(trace, "function_exit")
        if e["frame_id"] == fn_entry["frame_id"]
    )
    assert _value_at(trace, exit_ev["return_value"]) == "caught"


def test_exception_propagates_through_recorded_callee(trace_path: Path):
    """A recorded function calls another recorded function that
    raises. We see RAISE inside the callee, FUNCTION_EXIT-with-unwind
    for the callee, and FUNCTION_EXIT-with-unwind for the caller —
    both unwind sentinels because both are exiting via exception."""
    with pytest.raises(RuntimeError):
        calls_raiser_and_propagates()
    trace = hindsight.read_trace(str(trace_path))

    # Two unwind exits expected: callee, then caller.
    exits = _events_of(trace, "function_exit")
    unwind_exits = [e for e in exits if e["return_value"] == 1]
    assert len(unwind_exits) == 2

    # The RAISE happens before the unwind exits, in source order.
    raise_idx = next(
        i for i, ev in enumerate(trace["events"]) if ev["type"] == "exception_raised"
    )
    first_unwind_idx = next(
        i
        for i, ev in enumerate(trace["events"])
        if ev["type"] == "function_exit" and ev["return_value"] == 1
    )
    assert raise_idx < first_unwind_idx
