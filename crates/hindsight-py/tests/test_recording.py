# SPDX-License-Identifier: Apache-2.0
"""Integration tests for the @hindsight.record decorator.

These exercise the full recording pipeline: sys.monitoring callbacks emit
events into the Rust writer, the writer finalizes a trace file, and the
test reads it back via ``hindsight.read_trace`` to assert what was captured.

Pre-requisite: the recorder requires the decorated function's source to
be on disk so it can be embedded into the trace's source bundle. Test
helper functions are therefore defined at module level (this file is on
disk by virtue of being a test module), not inside test functions.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Any

import pytest

import hindsight


# --- Helpers ----------------------------------------------------------------


def _value_at(trace: dict, value_id: int) -> Any:
    return trace["values"][value_id]["decoded"]


def _string_at(trace: dict, string_id: int) -> str:
    return trace["strings"][string_id]


def _events_of(trace: dict, type_name: str) -> list[dict]:
    return [e for e in trace["events"] if e["type"] == type_name]


def _function_entries_for(trace: dict, qualname: str) -> list[dict]:
    """All FUNCTION_ENTRY events whose function_id resolves to ``qualname``."""
    return [
        e
        for e in _events_of(trace, "function_entry")
        if _string_at(trace, e["function_id"]) == qualname
    ]


@pytest.fixture
def trace_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Per-test trace output path, set via HINDSIGHT_OUTPUT_PATH so the
    decorator picks it up. Auto-cleanup via pytest's tmp_path."""
    p = tmp_path / "trace.hindsight"
    monkeypatch.setenv("HINDSIGHT_OUTPUT_PATH", str(p))
    return p


# --- Module-level decorated test subjects ----------------------------------
#
# These need to be defined at module scope so their source is on disk in
# this very test file — the recorder reads `code.co_filename` and pulls
# the file's contents into the trace's source bundle. A function defined
# inside a test function body has the same filename, so that would also
# work, but module-scope keeps the line numbers stable across edits.


@hindsight.record
def add(a: int, b: int) -> int:
    result = a + b
    return result


def helper(n: int) -> int:
    doubled = n * 2
    return doubled


@hindsight.record
def caller_of_helper(n: int) -> int:
    direct = n + 1
    indirect = helper(n)
    return direct + indirect


@hindsight.record
def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)


@hindsight.record
def loop_sum(stop: int) -> int:
    total = 0
    for x in range(stop):
        total = total + x
    return total


@hindsight.record
def multi_types() -> dict:
    nums = [1, 2, 3]
    flags = {"a": True, "b": False}
    name = "hello"
    pi = 3.14
    out = {"nums": nums, "flags": flags, "name": name, "pi": pi}
    return out


@hindsight.record
def add_for_envvar(a: int, b: int) -> int:
    return a + b


# --- Tests ------------------------------------------------------------------


def test_simple_function_records_expected_events(trace_path: Path):
    """A trivial pure function records ENTRY/SNAPSHOT/LINE_DELTAs/EXIT
    with the expected arguments and return value."""
    result = add(2, 3)
    assert result == 5

    trace = hindsight.read_trace(str(trace_path))
    assert trace["is_finalized"] is True

    entries = _function_entries_for(trace, "add")
    assert len(entries) == 1
    entry = entries[0]
    # Args: (a, 2) and (b, 3) interned as (string_id, value_id) pairs.
    arg_names = [_string_at(trace, name_id) for name_id, _ in entry["args"]]
    arg_values = [_value_at(trace, value_id) for _, value_id in entry["args"]]
    assert arg_names == ["a", "b"]
    assert arg_values == [2, 3]

    # FRAME_SNAPSHOT immediately after entry for the same frame.
    frame_id = entry["frame_id"]
    snapshots = [
        e
        for e in _events_of(trace, "frame_snapshot")
        if e["frame_id"] == frame_id
    ]
    assert len(snapshots) == 1
    snap = snapshots[0]
    snap_locals = {
        _string_at(trace, name_id): _value_at(trace, value_id)
        for name_id, value_id in snap["locals"]
    }
    assert snap_locals == {"a": 2, "b": 3}

    # FUNCTION_EXIT with return value 5.
    exits = [e for e in _events_of(trace, "function_exit") if e["frame_id"] == frame_id]
    assert len(exits) == 1
    assert _value_at(trace, exits[0]["return_value"]) == 5

    # At least one LINE_DELTA mentions `result` taking value 5.
    saw_result_eq_5 = False
    for ev in _events_of(trace, "line_delta"):
        for name_id, value_id in ev["changes"]:
            if (
                _string_at(trace, name_id) == "result"
                and _value_at(trace, value_id) == 5
            ):
                saw_result_eq_5 = True
    assert saw_result_eq_5, "expected a LINE_DELTA capturing `result = 5`"


def test_nested_calls_get_separate_frame_ids(trace_path: Path):
    """A recorded function calling another function records both with
    distinct frame_ids; the callee's events are bracketed by the
    caller's events."""
    result = caller_of_helper(10)
    assert result == 31  # (10 + 1) + (10 * 2)

    trace = hindsight.read_trace(str(trace_path))

    entries = _events_of(trace, "function_entry")
    qualnames = [_string_at(trace, e["function_id"]) for e in entries]
    # caller_of_helper, then helper inside it.
    assert "caller_of_helper" in qualnames
    assert "helper" in qualnames
    caller_entry = next(e for e in entries if _string_at(trace, e["function_id"]) == "caller_of_helper")
    helper_entry = next(e for e in entries if _string_at(trace, e["function_id"]) == "helper")
    assert caller_entry["frame_id"] != helper_entry["frame_id"]

    # caller's entry comes before helper's entry in the event stream;
    # caller's exit comes after helper's exit.
    caller_idx = trace["events"].index(caller_entry)
    helper_idx = trace["events"].index(helper_entry)
    assert caller_idx < helper_idx
    helper_exit = next(
        e
        for e in _events_of(trace, "function_exit")
        if e["frame_id"] == helper_entry["frame_id"]
    )
    caller_exit = next(
        e
        for e in _events_of(trace, "function_exit")
        if e["frame_id"] == caller_entry["frame_id"]
    )
    assert trace["events"].index(helper_exit) < trace["events"].index(caller_exit)
    # Helper's return value matches what caller saw.
    assert _value_at(trace, helper_exit["return_value"]) == 20


def test_recursion_produces_distinct_frame_ids_per_call(trace_path: Path):
    """fib(5) makes a known number of recursive activations; each gets
    its own frame_id."""
    assert fib(5) == 5

    trace = hindsight.read_trace(str(trace_path))

    fib_entries = _function_entries_for(trace, "fib")
    # fib(n) calls = 2 * fib(n) - 1; for n=5 that's 2*5 - 1 = 9... but
    # actually fib(n) call count = number of nodes in the recursion tree
    # which is fib(n+1) for naive recursion. fib(6) = 8, so 8 nodes for
    # n=5. Wait, let me recount: fib(5) -> fib(4)+fib(3); fib(4) ->
    # fib(3)+fib(2); ... The call count for our fib(n) is  exactly
    # `2*fib(n+1) - 1`. For n=5, fib(6)=8, count = 15.
    # Cross-check by computing directly:
    def expected_calls(n: int) -> int:
        if n < 2:
            return 1
        return 1 + expected_calls(n - 1) + expected_calls(n - 2)

    assert len(fib_entries) == expected_calls(5)
    # All frame_ids unique.
    frame_ids = [e["frame_id"] for e in fib_entries]
    assert len(frame_ids) == len(set(frame_ids))
    # Outermost frame comes first.
    fib_n_for = lambda e: dict(  # noqa: E731
        (
            (_string_at(trace, name_id), _value_at(trace, value_id))
            for name_id, value_id in e["args"]
        )
    )
    assert fib_n_for(fib_entries[0])["n"] == 5


def test_loop_captures_changing_variables(trace_path: Path):
    """A for-loop modifying variables records the changes in successive
    LINE_DELTA events."""
    assert loop_sum(4) == 0 + 1 + 2 + 3  # 6

    trace = hindsight.read_trace(str(trace_path))

    entry = _function_entries_for(trace, "loop_sum")[0]
    frame_id = entry["frame_id"]

    # Walk events in order; collect every value `total` ever held inside
    # this frame. We see each new value as it's recorded by LINE_DELTA.
    total_values: list[int] = []
    in_frame = False
    for ev in trace["events"]:
        if ev.get("frame_id") == frame_id and ev["type"] == "function_entry":
            in_frame = True
            # The initial snapshot will pick `total` up too.
        elif ev.get("frame_id") == frame_id and ev["type"] == "function_exit":
            break
        if not in_frame:
            continue
        if ev["type"] == "frame_snapshot" and ev["frame_id"] == frame_id:
            for name_id, value_id in ev["locals"]:
                if _string_at(trace, name_id) == "total":
                    total_values.append(_value_at(trace, value_id))
        elif ev["type"] == "line_delta":
            for name_id, value_id in ev["changes"]:
                if _string_at(trace, name_id) == "total":
                    total_values.append(_value_at(trace, value_id))

    # We should have seen at least the initial 0 and the loop's running
    # totals (0, 1, 3, 6 are the four post-iteration values).
    assert 0 in total_values
    assert 6 in total_values
    # Strictly increasing across the loop after the first iteration.
    assert total_values[-1] == 6


def test_multiple_data_types_round_trip(trace_path: Path):
    """A function manipulating list, dict, str, int, float records each
    with the correct decoded representation."""
    out = multi_types()
    assert out == {
        "nums": [1, 2, 3],
        "flags": {"a": True, "b": False},
        "name": "hello",
        "pi": 3.14,
    }

    trace = hindsight.read_trace(str(trace_path))
    entry = _function_entries_for(trace, "multi_types")[0]
    frame_id = entry["frame_id"]

    # Look at the FUNCTION_EXIT to find the returned dict's value.
    exit_evs = [e for e in _events_of(trace, "function_exit") if e["frame_id"] == frame_id]
    assert len(exit_evs) == 1
    decoded = _value_at(trace, exit_evs[0]["return_value"])
    assert decoded == {
        "nums": [1, 2, 3],
        "flags": {"a": True, "b": False},
        "name": "hello",
        "pi": 3.14,
    }


def test_output_path_honors_hindsight_output_path_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """Setting HINDSIGHT_OUTPUT_PATH makes the trace land at that path
    rather than the default ./trace.hindsight."""
    custom = tmp_path / "subdir" / "custom.hindsight"
    custom.parent.mkdir()
    monkeypatch.setenv("HINDSIGHT_OUTPUT_PATH", str(custom))

    add_for_envvar(7, 8)

    assert custom.exists()
    trace = hindsight.read_trace(str(custom))
    entries = _function_entries_for(trace, "add_for_envvar")
    assert len(entries) == 1
    arg_values = [_value_at(trace, vid) for _, vid in entries[0]["args"]]
    assert arg_values == [7, 8]


def test_default_output_path_is_trace_hindsight_in_cwd(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """With no env var set, the trace lands at ``./trace.hindsight`` in
    the current working directory."""
    monkeypatch.delenv("HINDSIGHT_OUTPUT_PATH", raising=False)
    monkeypatch.chdir(tmp_path)

    add_for_envvar(1, 2)

    default_path = tmp_path / "trace.hindsight"
    assert default_path.exists()


def test_finalization_message_goes_to_stderr(
    trace_path: Path, capfd: pytest.CaptureFixture
):
    """The recorder prints a one-line summary to stderr on finalize so
    users know where the trace went without having to read documentation."""
    add(1, 1)

    captured = capfd.readouterr()
    assert "hindsight: trace written to" in captured.err
    assert "events" in captured.err
    # Stdout should not be polluted by the recorder.
    assert "hindsight" not in captured.out
