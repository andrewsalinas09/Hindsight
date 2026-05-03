# SPDX-License-Identifier: Apache-2.0
"""Round-trip integration test for the hindsight_recorder Python module.

Builds a small trace by hand via the TraceWriter API, finalizes it to disk,
reads it back via read_trace, and asserts the contents match what was written.
This is the killer test for the Python ↔ Rust boundary: every method we expose
on the writer is exercised, and the reader function (which mirrors the writer)
verifies the bytes were laid out correctly.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

import hindsight_recorder


TRACE_UUID = bytes(range(16))


def make_metadata(start_ns: int = 1_700_000_000) -> dict:
    """Minimal but complete metadata dict matching the recorder schema."""
    return {
        "recorder": {
            "language": "python",
            "language_version": "3.12.5",
            "recorder_version": "0.1.0",
            "platform": "test",
        },
        "recording": {
            "program": "pytest test_basic.py",
            "working_directory": None,
            "scope_config": {"include": [], "exclude": [], "depth_limit": None},
        },
        "program": None,
        "trace_uuid": TRACE_UUID,
        "recording_start_ns": start_ns,
    }


def test_module_loads():
    """Sanity check: the module name and the TraceWriter class are importable."""
    assert hindsight_recorder.__name__ == "hindsight_recorder"
    assert hasattr(hindsight_recorder, "TraceWriter")
    assert hasattr(hindsight_recorder, "read_trace")


def test_finish_to_bytes_returns_valid_trace():
    """A trace finalized to bytes starts with the file magic and isn't empty."""
    w = hindsight_recorder.TraceWriter(make_metadata())
    bytes_out = w.finish_to_bytes(1_700_001_000)
    assert isinstance(bytes_out, (bytes, bytearray))
    assert bytes_out[:8] == b"HNDSGHT\x00"
    assert len(bytes_out) > 64  # at least a header


def test_finished_writer_rejects_further_calls():
    """Once finished, methods raise RuntimeError rather than corrupt state."""
    w = hindsight_recorder.TraceWriter(make_metadata())
    w.finish_to_bytes(1_700_001_000)
    with pytest.raises(RuntimeError):
        w.intern_string("foo")
    with pytest.raises(RuntimeError):
        w.finish_to_bytes(1_700_001_000)


def test_intern_value_dedups_primitives():
    """Same Python value -> same ID; bool != int(0/1)."""
    w = hindsight_recorder.TraceWriter(make_metadata())
    assert w.intern_value(None) == 0  # NONE_VALUE_ID
    a = w.intern_value(42)
    b = w.intern_value(42)
    assert a == b
    c = w.intern_value(43)
    assert a != c
    # bool and int(1) must be distinct values per the writer's tag-aware dedup.
    true_id = w.intern_value(True)
    one_id = w.intern_value(1)
    assert true_id != one_id


def test_metadata_must_have_required_fields():
    """A malformed metadata dict raises a Python ValueError."""
    with pytest.raises(ValueError):
        hindsight_recorder.TraceWriter({})  # type: ignore[arg-type]
    md = make_metadata()
    del md["recorder"]
    with pytest.raises(ValueError):
        hindsight_recorder.TraceWriter(md)  # type: ignore[arg-type]


def test_round_trip_complete_trace(tmp_path: Path):
    """End-to-end: build a trace by hand, finalize to a file, read back, verify.

    This is the killer test the session A spec calls out: every writer method
    is exercised and every readable field is asserted against what we wrote.
    """
    trace_path = tmp_path / "trace.hindsight"

    w = hindsight_recorder.TraceWriter(make_metadata())

    # 1. Source file ------------------------------------------------
    src = "def double(x):\n    result = x * 2\n    return result\n"
    file_id = w.add_source_file("example.py", src)
    assert file_id == 0
    # Re-adding same path returns same ID (no overwrite).
    file_id_again = w.add_source_file("example.py", "DIFFERENT")
    assert file_id_again == file_id

    # 2. Strings ----------------------------------------------------
    fn_name_id = w.intern_string("__main__.double")
    arg_x_id = w.intern_string("x")
    local_result_id = w.intern_string("result")
    # Dedup verified.
    assert w.intern_string("__main__.double") == fn_name_id

    # 3. Values -----------------------------------------------------
    # Primitives:
    val_5 = w.intern_value(5)
    val_10 = w.intern_value(10)
    # A nested container, just to exercise auto-recursion:
    container_id = w.intern_value({"nums": [1, 2, 3], "flag": True})
    # A summary-fallback value: an arbitrary-class instance.
    class Custom:  # noqa: D401, E306
        def __repr__(self) -> str:
            return "Custom()"

    summary_id = w.intern_value(Custom())
    # A BigInt that doesn't fit in i64:
    big_id = w.intern_value(2**100)

    assert val_5 != val_10 != container_id != summary_id != big_id

    # 4. Events -----------------------------------------------------
    # Build a small program trace: function entry -> snapshot -> line -> exit.
    w.write_function_entry(
        timestamp_delta_ns=10,
        frame_id=0,
        function_id=fn_name_id,
        source_file_id=file_id,
        line=1,
        args=[(arg_x_id, val_5)],
    )
    w.write_frame_snapshot(
        timestamp_delta_ns=5,
        frame_id=0,
        line=1,
        locals=[(arg_x_id, val_5)],
    )
    w.write_line_delta(
        timestamp_delta_ns=20,
        line=2,
        changes=[(local_result_id, val_10)],
    )
    w.write_function_exit(
        timestamp_delta_ns=15,
        frame_id=0,
        return_value=val_10,
    )

    # 5. Finalize to file ------------------------------------------
    w.finish(str(trace_path), recording_end_ns=1_700_001_000)
    assert trace_path.exists()
    assert trace_path.stat().st_size > 64

    # 6. Read back and verify --------------------------------------
    trace = hindsight_recorder.read_trace(str(trace_path))

    assert trace["is_finalized"] is True
    assert trace["header"]["trace_uuid"] == TRACE_UUID
    assert trace["header"]["recording_start_ns"] == 1_700_000_000
    assert trace["header"]["recording_end_ns"] == 1_700_001_000

    # Metadata TOML carries through faithfully.
    assert "[recorder]" in trace["metadata_toml"]
    assert 'language = "python"' in trace["metadata_toml"]

    # Final summary records the events we wrote.
    assert "clean_shutdown = true" in trace["final_summary_toml"]
    assert "function_entry_events = 1" in trace["final_summary_toml"]
    assert "function_exit_events = 1" in trace["final_summary_toml"]
    assert "frame_snapshot_events = 1" in trace["final_summary_toml"]
    assert "line_events = 1" in trace["final_summary_toml"]

    # Source bundle includes our file with its content intact.
    assert len(trace["source_files"]) == 1
    sf = trace["source_files"][0]
    assert sf["path"] == "example.py"
    assert sf["content"] == src.encode("utf-8")
    assert len(sf["blake3_hash"]) == 32

    # String table contains everything we interned.
    strings = trace["strings"]
    assert "__main__.double" in strings
    assert "x" in strings
    assert "result" in strings

    # Values: the writer reserves [0]=None and [1]=ExceptionUnwindSentinel.
    values = trace["values"]
    assert values[0]["decoded"] is None
    # Find each value we wrote and verify its decoded form.
    decoded_by_id = {v["value_id"]: v for v in values}
    assert decoded_by_id[val_5]["decoded"] == 5
    assert decoded_by_id[val_10]["decoded"] == 10
    assert decoded_by_id[container_id]["decoded"] == {
        "nums": [1, 2, 3],
        "flag": True,
    }
    # The summary value comes back as a dict marker (not the original Python
    # object — that information is lost on serialization). type_name is the
    # class's `__qualname__`; for a class defined inside this test function
    # that includes the enclosing function in the path, e.g.
    # "test_round_trip_complete_trace.<locals>.Custom".
    summary_decoded = decoded_by_id[summary_id]["decoded"]
    assert summary_decoded["kind"] == "summary"
    assert summary_decoded["type_name"].endswith("Custom")
    assert summary_decoded["repr"] == "Custom()"
    # BigInt round-trips through int.from_bytes.
    assert decoded_by_id[big_id]["decoded"] == 2**100

    # Events round-trip in order with their fields preserved.
    events = trace["events"]
    assert len(events) == 4
    assert events[0]["type"] == "function_entry"
    assert events[0]["frame_id"] == 0
    assert events[0]["function_id"] == fn_name_id
    assert events[0]["source_file_id"] == file_id
    assert events[0]["line"] == 1
    assert events[0]["args"] == [(arg_x_id, val_5)]
    assert events[0]["timestamp_delta_ns"] == 10

    assert events[1]["type"] == "frame_snapshot"
    assert events[1]["locals"] == [(arg_x_id, val_5)]

    assert events[2]["type"] == "line_delta"
    assert events[2]["line"] == 2
    assert events[2]["changes"] == [(local_result_id, val_10)]

    assert events[3]["type"] == "function_exit"
    assert events[3]["return_value"] == val_10


def test_empty_trace_is_well_formed(tmp_path: Path):
    """A writer with no events / no sources still produces a valid file."""
    trace_path = tmp_path / "empty.hindsight"
    w = hindsight_recorder.TraceWriter(make_metadata())
    w.finish(str(trace_path), recording_end_ns=1_700_000_500)

    trace = hindsight_recorder.read_trace(str(trace_path))
    assert trace["is_finalized"] is True
    assert trace["events"] == []
    assert trace["source_files"] == []
    # Reserved values [0]=None, [1]=exception unwind sentinel are always present.
    assert len(trace["values"]) == 2


def test_intern_value_summary_with_explicit_args():
    """The explicit summary path lets the caller bypass the auto-summary fallback."""
    w = hindsight_recorder.TraceWriter(make_metadata())
    type_name_id = w.intern_string("numpy.ndarray")
    repr_id = w.intern_string("array([1, 2, 3])")
    sid = w.intern_value_summary(type_name_id, length=3, repr=repr_id)
    # Same args -> same ID.
    sid2 = w.intern_value_summary(type_name_id, length=3, repr=repr_id)
    assert sid == sid2


def test_intern_value_with_identity_requires_16_bytes():
    """Identity hash must be exactly 16 bytes; anything else raises ValueError."""
    w = hindsight_recorder.TraceWriter(make_metadata())
    with pytest.raises(ValueError):
        w.intern_value_with_identity(42, b"\x00" * 8)
    with pytest.raises(ValueError):
        w.intern_value_with_identity(42, b"\x00" * 32)
    # A 16-byte hash is accepted.
    sid = w.intern_value_with_identity(42, b"\x01" * 16)
    assert isinstance(sid, int)
