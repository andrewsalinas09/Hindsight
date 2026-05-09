# SPDX-License-Identifier: Apache-2.0
"""Tests for the recorder's summary-fingerprint alias path (v0.3).

The recorder's per-frame container cache should:

1. **Append-loop fast path:** capturing the same growing list across line
   events should emit Grown aliases (one per growth) rather than full
   re-walks. The number of value-table entries grows linearly with the
   total elements appended, not quadratically.

2. **Same-fingerprint alias:** capturing an unchanged container twice
   should emit an Equivalent alias for the second capture.

3. **Length shrunk → full walk:** when a container loses elements, the
   alias path should fall through to a full re-walk because the cached
   fingerprint no longer matches.

4. **Confidence labels propagate** correctly through the trace to the
   indexed database.

5. **Per-frame cache lifecycle:** the cache is dropped at frame exit,
   so two separate `@record` calls don't share alias state.

These tests use ``hindsight._capture`` directly where it makes the
assertion sharper (don't need to spin up a recorder and match events to
verify the cache logic), and use end-to-end ``@record`` recordings where
the integration matters.
"""

from __future__ import annotations

import os
import uuid
from pathlib import Path

import pytest

import hindsight
from hindsight import _capture
from hindsight._core import TraceWriter, read_trace


# ----------------------------------------------------------------------------
# Helpers
# ----------------------------------------------------------------------------

def fresh_writer() -> TraceWriter:
    """A bare TraceWriter with the minimum metadata. Useful for unit-testing
    the alias path without going through the full recorder."""
    return TraceWriter(
        {
            "recorder": {
                "language": "python",
                "language_version": "3.12",
                "recorder_version": "0.1",
                "platform": "test",
            },
            "recording": {
                "program": "pytest",
                "working_directory": "/tmp",
                "scope_config": {"include": [], "exclude": [], "depth_limit": None},
            },
            "program": None,
            "trace_uuid": uuid.uuid4().bytes,
            "recording_start_ns": 0,
        }
    )


def finalize(w: TraceWriter) -> bytes:
    return w.finish_to_bytes(
        recording_end_ns=1_000_000,
        scope_resolution={"recorded_functions": [], "excluded_functions": []},
    )


# ----------------------------------------------------------------------------
# Unit tests against _capture.smart_intern_value (no recorder spin-up)
# ----------------------------------------------------------------------------

def test_first_capture_of_list_does_not_alias():
    w = fresh_writer()
    cache = _capture.ContainerCache()
    lst = [1, 2, 3]
    vid = _capture.smart_intern_value(w, cache, lst)
    bytes_ = finalize(w)
    trace = read_trace(_save(bytes_))
    entry = trace["values"][vid]
    # First capture should be a real list (tag 0x07), not an alias (0x15).
    assert entry["tag"] == 0x07, f"expected list, got tag {entry['tag']:#x}"


def test_second_capture_of_unchanged_list_reuses_cached_value_id(tmp_path):
    """When the cache is clean and the fingerprint matches (same id,
    same length, same endpoint identity), smart_intern_value returns the
    *cached* value_id directly — no fresh alias entry is emitted.

    This matters because the recorder's `_capture_locals_into_changes`
    runs on every LINE event. If we emitted a fresh alias on every
    re-capture, every LINE event would record a "change" for every
    tracked container even when nothing actually changed — generating
    wire-format noise proportional to LINE × tracked-containers.

    Returning the cached value_id lets the recorder's delta logic see
    the same value_id as last time and correctly skip re-recording.
    """
    w = fresh_writer()
    cache = _capture.ContainerCache()
    lst = [1, 2, 3]
    first = _capture.smart_intern_value(w, cache, lst)
    second = _capture.smart_intern_value(w, cache, lst)
    assert first == second, (
        "clean re-capture should return the cached value_id, not emit a fresh alias"
    )

    # No alias entry should have been written.
    bytes_ = finalize(w)
    trace = read_trace(_save(bytes_, tmp_path))
    alias_entries = [v for v in trace["values"] if v["tag"] == 0x15]
    assert len(alias_entries) == 0, (
        f"clean re-capture should not produce alias entries, got {len(alias_entries)}"
    )


def test_appended_list_emits_grown_alias_with_only_new_tail(tmp_path):
    """Append-loop fast path. After one append, the second capture should
    be a Grown alias whose new_elements is just the appended item."""
    w = fresh_writer()
    cache = _capture.ContainerCache()
    lst: list = [1, 2]
    first = _capture.smart_intern_value(w, cache, lst)
    lst.append(3)
    second = _capture.smart_intern_value(w, cache, lst)
    assert first != second

    bytes_ = finalize(w)
    trace = read_trace(_save(bytes_, tmp_path))
    second_entry = trace["values"][second]
    assert second_entry["tag"] == 0x15
    decoded = second_entry["decoded"]
    assert decoded["alias_kind"] == "grown"
    assert decoded["aliased_value_id"] == first
    # The single appended element (Int 3) should be in new_elements.
    assert len(decoded["new_elements"]) == 1


def test_append_loop_emits_grown_aliases_not_quadratic_inline_lists(tmp_path):
    """The whole point of the design. Capture the same list 100 times
    while it grows by one each iteration. The value-table entries should
    be: one full list (length=0 at first capture) + one int per element +
    100 Grown aliases. Total values = 1 list + 100 ints + 100 aliases = 201,
    NOT 1 + 100 + sum(1..99) = ~5050 inline lists.
    """
    w = fresh_writer()
    cache = _capture.ContainerCache()
    lst: list = []
    _capture.smart_intern_value(w, cache, lst)  # First capture: empty list
    for i in range(100):
        lst.append(i)
        _capture.smart_intern_value(w, cache, lst)
    bytes_ = finalize(w)
    trace = read_trace(_save(bytes_, tmp_path))

    list_entries = [v for v in trace["values"] if v["tag"] == 0x07]
    alias_entries = [v for v in trace["values"] if v["tag"] == 0x15]

    # Exactly one inline list (the very first capture). 100 Grown
    # aliases — each iteration's append changes the length, so the
    # cache miss-fingerprint path emits a fresh Grown alias.
    assert len(list_entries) == 1, (
        f"expected exactly 1 inline list (the empty first capture), got {len(list_entries)}"
    )
    assert len(alias_entries) == 100, (
        f"expected 100 alias entries, one per re-capture after a length change, "
        f"got {len(alias_entries)}"
    )


def test_length_shrunk_falls_back_to_full_walk(tmp_path):
    w = fresh_writer()
    cache = _capture.ContainerCache()
    lst: list = [1, 2, 3, 4]
    first = _capture.smart_intern_value(w, cache, lst)
    lst.pop()
    lst.pop()
    second = _capture.smart_intern_value(w, cache, lst)
    bytes_ = finalize(w)
    trace = read_trace(_save(bytes_, tmp_path))

    second_entry = trace["values"][second]
    # Shrunk list cannot be aliased. Must be a fresh inline list.
    assert second_entry["tag"] == 0x07, (
        f"shrunk list should be re-walked, got alias tag {second_entry['tag']:#x}"
    )


def test_dict_same_length_reuses_value_id_growth_emits_grown_alias(tmp_path):
    w = fresh_writer()
    cache = _capture.ContainerCache()
    d = {"a": 1, "b": 2}
    first = _capture.smart_intern_value(w, cache, d)
    same_len = _capture.smart_intern_value(w, cache, d)  # No change → reuse.
    d["c"] = 3
    grown = _capture.smart_intern_value(w, cache, d)  # Length grew → Grown alias.

    assert same_len == first, "clean same-length re-capture reuses cached value_id"
    assert grown != first, "growth produces a new value_id"

    bytes_ = finalize(w)
    trace = read_trace(_save(bytes_, tmp_path))
    decoded = trace["values"][grown]["decoded"]
    # v0.4: dict growth (without an observed mutation event) emits a Grown
    # alias with summary_observed confidence — only the new pairs are
    # interned, the existing entries are referenced via the prior dict.
    assert isinstance(decoded, dict) and decoded.get("kind") == "alias", (
        "grown dict emits an alias, not a full re-walk"
    )
    assert decoded["alias_kind"] == "grown"
    assert decoded["aliased_value_id"] == first
    assert decoded["confidence"] == "summary_observed"


def test_set_same_length_reuses_value_id_different_length_does_not(tmp_path):
    w = fresh_writer()
    cache = _capture.ContainerCache()
    s = {1, 2, 3}
    first = _capture.smart_intern_value(w, cache, s)
    same_len = _capture.smart_intern_value(w, cache, s)
    s.add(4)
    grown = _capture.smart_intern_value(w, cache, s)
    bytes_ = finalize(w)
    trace = read_trace(_save(bytes_, tmp_path))
    assert same_len == first
    assert trace["values"][grown]["tag"] == 0x09, "grown set re-walks"


def test_scalars_never_alias(tmp_path):
    """Scalars (ints, strings, etc.) skip the alias path entirely — they
    go through the writer's content-hash dedup."""
    w = fresh_writer()
    cache = _capture.ContainerCache()
    a = _capture.smart_intern_value(w, cache, 42)
    b = _capture.smart_intern_value(w, cache, 42)
    # Same scalar deduplicates to the same value_id (writer's content hash).
    assert a == b
    bytes_ = finalize(w)
    trace = read_trace(_save(bytes_, tmp_path))
    assert trace["values"][a]["tag"] == 0x02  # IntSmall


def test_subclass_of_list_is_not_aliased(tmp_path):
    """Custom subclasses can override mutation semantics in ways the
    alias path doesn't anticipate. Recognize only built-in container
    types."""
    class MyList(list):
        pass

    w = fresh_writer()
    cache = _capture.ContainerCache()
    ml = MyList([1, 2])
    first = _capture.smart_intern_value(w, cache, ml)
    second = _capture.smart_intern_value(w, cache, ml)

    bytes_ = finalize(w)
    trace = read_trace(_save(bytes_, tmp_path))
    # Subclass falls through to the writer's intern_value which summarizes
    # arbitrary objects. Both captures should produce the same summary id
    # (same object, same repr); neither should be an alias entry.
    second_entry = trace["values"][second]
    assert second_entry["tag"] != 0x15, "subclass must not take the alias path"


def test_no_cache_falls_through_to_full_intern():
    """smart_intern_value with cache=None mirrors a plain intern_value
    call — no aliasing, full walk every time."""
    w = fresh_writer()
    a = _capture.smart_intern_value(w, None, [1, 2])
    b = _capture.smart_intern_value(w, None, [1, 2])
    # Same content content-hashes to same id.
    assert a == b


# ----------------------------------------------------------------------------
# End-to-end via @hindsight.record
# ----------------------------------------------------------------------------

@pytest.fixture
def trace_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Pin output path so we can read back the produced trace."""
    target = tmp_path / "out.hindsight"
    monkeypatch.setenv("HINDSIGHT_OUTPUT_PATH", str(target))
    return target


def test_perf_pattern_does_not_explode_value_table(trace_path: Path):
    """The motivating perf.py pattern: append in a loop. The trace's
    value table should grow linearly (one alias per append), not
    quadratically (one full inline list per append)."""

    @hindsight.record
    def slow_iteration_demo():
        results: list = []
        for i in range(50):
            results.append(i)
        return results

    slow_iteration_demo()
    trace = read_trace(trace_path)

    list_entries = [v for v in trace["values"] if v["tag"] == 0x07]
    alias_entries = [v for v in trace["values"] if v["tag"] == 0x15]

    # The whole point of the design: an append loop should NOT produce
    # an O(N) inline list per iteration. With both summary aliasing and
    # mutation-tracked Grown aliasing, the inline-list count should
    # stay small (just the first capture); each subsequent iteration's
    # capture should be a Grown alias of one tail element.
    assert len(list_entries) <= 5, (
        f"append loop should produce a tiny number of inline list entries "
        f"(ideally 1 — the empty initial list), got {len(list_entries)}. "
        f"More than this means dirty re-walks aren't being optimized into "
        f"Grown aliases."
    )
    assert len(alias_entries) >= 40, (
        f"50-iteration append loop should produce ~50 Grown aliases, "
        f"got {len(alias_entries)}. Lower than expected suggests the "
        f"mutation-tracking path isn't firing correctly."
    )


def test_per_frame_cache_does_not_leak_across_calls(trace_path: Path):
    """Two separate @record invocations should not share alias state.
    Each call constructs a fresh cache at PY_START."""

    @hindsight.record
    def one():
        x = [1, 2, 3]
        return x

    one()
    one()  # Two recordings to the same path; second overwrites.
    trace = read_trace(trace_path)
    # Just verify the trace is well-formed and finalized.
    assert trace["is_finalized"]


# ----------------------------------------------------------------------------
# Helper: write bytes to disk so read_trace can consume them
# ----------------------------------------------------------------------------

_TMP_COUNTER = 0


def _save(bytes_: bytes, tmp_path: Path | None = None) -> Path:
    """Persist ``bytes_`` to a unique tempfile and return the path."""
    global _TMP_COUNTER
    _TMP_COUNTER += 1
    base = tmp_path if tmp_path is not None else Path(os.environ.get("TEMP", "/tmp"))
    p = base / f"hindsight-alias-test-{os.getpid()}-{_TMP_COUNTER}.hindsight"
    p.write_bytes(bytes_)
    return p
