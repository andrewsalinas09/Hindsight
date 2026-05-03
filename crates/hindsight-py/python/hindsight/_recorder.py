# SPDX-License-Identifier: Apache-2.0
"""Recorder implementation: the @hindsight.record decorator.

The recorder turns on ``sys.monitoring`` for the duration of a decorated
function call, captures FUNCTION_ENTRY / FRAME_SNAPSHOT / LINE_DELTA /
FUNCTION_EXIT events for everything executed within that scope (including
transitive callees), and writes a ``.hindsight`` trace file when the
decorated function returns.

Scope-control parameters from ``docs/scope-control.md`` (depth, include,
exclude, conditional capture, skip blocks) are intentionally not exposed
here yet. v0 records *everything* the decorated scope reaches; that's the
floor we'll layer scope control on top of in a later session.

Out-of-scope events (BRANCH, EXCEPTION, NOTE, SCOPE_BOUNDARY,
FRAME_SWITCH) are also deferred. Generators / coroutines / threads are
not yet handled.
"""

from __future__ import annotations

import functools
import os
import platform
import sys
import time
import uuid
from pathlib import Path
from typing import Any, Callable

from ._core import TraceWriter

__all__ = ["record"]

# --- sys.monitoring tool registration ---------------------------------------
#
# Surprise from CPython: there are exactly six tool slots, indexed 0..5.
# `use_tool_id(6, ...)` raises `ValueError: invalid tool 6`. CPython
# reserves four of those slots and leaves two free for third-party tools:
#
#   0 = `sys.monitoring.DEBUGGER_ID`
#   1 = `sys.monitoring.COVERAGE_ID`
#   2 = `sys.monitoring.PROFILER_ID`
#   3 = (free)
#   4 = (free)
#   5 = `sys.monitoring.OPTIMIZER_ID`
#
# We claim slot 3. If the user's process has another tool already in slot
# 3 (rare in practice — it's typically a different debugger or a coverage
# tool, which would be in 0/1), `use_tool_id` raises and we propagate.
# Tools listed via `sys.monitoring.get_tool(3)` show up as "hindsight".
_TOOL_ID: int = 3
_TOOL_NAME: str = "hindsight"


# --- Single-recording-at-a-time guard ---------------------------------------
#
# `sys.monitoring` is a process-wide API. We support a single outermost
# recorded scope at a time; nested @record calls fall through to plain
# function calls. Multiple concurrent decorated functions across threads
# is out of scope for v0.
_state: "_RecorderState | None" = None

# Re-entrancy guard for callbacks. While a callback is running, we may end
# up calling user-defined `__repr__` (from inside the value conversion
# layer), which itself may execute Python and would otherwise trigger
# another PY_START / LINE / PY_RETURN cycle. The flag short-circuits that
# recursion. Single-threaded by design — the recorder doesn't claim thread
# safety and v0 is single-thread anyway.
_in_callback: bool = False


# --- Recorder state ----------------------------------------------------------


class _RecorderState:
    """Per-recording-session state. Created when a ``@record``-decorated
    function is called; thrown away when it returns.

    The state holds the writer, frame-tracking maps, source-file cache, and
    timing book-keeping. It is intentionally a plain class rather than a
    pyclass — the hot path runs Python-side (sys.monitoring callbacks) and
    has no GIL-detached entry points to defend.
    """

    __slots__ = (
        "writer",
        "output_path",
        "recording_start_ns",
        "_last_event_wall_ns",
        "_next_frame_id",
        "frame_id_by_pyframe",
        "last_value_id_by_local",
        "source_id_by_path",
        "skipped_files",
        "event_count",
        "skip_codes",
    )

    def __init__(self, output_path: str, skip_codes: frozenset) -> None:
        self.recording_start_ns = time.time_ns()
        self._last_event_wall_ns = self.recording_start_ns
        self.writer = TraceWriter(_build_metadata(self.recording_start_ns))
        self.output_path = output_path
        # Per-frame state. id(frame) is process-unique while the frame is
        # alive, and a frame is alive between PY_START and the matching
        # PY_RETURN. We clean these up at PY_RETURN.
        self._next_frame_id: int = 0
        self.frame_id_by_pyframe: dict[int, int] = {}
        # Per-frame "last value ID we recorded for each local name." Used
        # by LINE_DELTA to emit only the locals that changed since the
        # previous LINE / FRAME_SNAPSHOT in the same frame.
        self.last_value_id_by_local: dict[int, dict[str, int]] = {}
        # Cache of source-file-ID per filename, plus a negative cache for
        # filenames we've decided to skip (REPL inputs, exec'd strings,
        # frozen modules, missing files).
        self.source_id_by_path: dict[str, int] = {}
        self.skipped_files: set[str] = set()
        self.event_count: int = 0
        self.skip_codes = skip_codes

    def timestamp_delta_ns(self) -> int:
        now = time.time_ns()
        delta = now - self._last_event_wall_ns
        self._last_event_wall_ns = now
        # Negative deltas would indicate a non-monotonic wall clock; clamp
        # to zero so the writer doesn't see something it can't encode.
        return delta if delta >= 0 else 0

    def alloc_frame_id(self, frame: Any) -> int:
        fid = self._next_frame_id
        self._next_frame_id += 1
        self.frame_id_by_pyframe[id(frame)] = fid
        self.last_value_id_by_local[id(frame)] = {}
        return fid

    def get_or_intern_source(self, path: str) -> int | None:
        """Return the source_file_id for ``path``, or ``None`` if we should
        skip recording for code in this file (REPL input, missing, etc.)."""
        cached = self.source_id_by_path.get(path)
        if cached is not None:
            return cached
        if path in self.skipped_files:
            return None
        if not _is_real_source_path(path):
            self.skipped_files.add(path)
            return None
        try:
            content = Path(path).read_text(encoding="utf-8", errors="replace")
        except OSError:
            self.skipped_files.add(path)
            return None
        file_id = self.writer.add_source_file(path, content)
        self.source_id_by_path[path] = file_id
        return file_id


def _is_real_source_path(path: str) -> bool:
    """Reject paths that aren't real files on disk.

    sys.monitoring fires PY_START for every Python function, including
    those defined in REPL inputs (``<stdin>``), exec'd strings (``<string>``),
    frozen modules (``<frozen importlib._bootstrap>``), and other
    interpreter-internal locations whose filenames are surrounded by angle
    brackets. We can't capture source for them and don't want to record
    their interiors, so we skip them at the source-resolution boundary.
    """
    if not path or path.startswith("<"):
        return False
    return os.path.isfile(path)


def _build_metadata(start_ns: int) -> dict:
    """Construct the metadata dict TraceWriter wants. The shape is
    documented on `metadata_from_dict` in `src/lib.rs`."""
    return {
        "recorder": {
            "language": "python",
            "language_version": platform.python_version(),
            "recorder_version": "0.1.0",
            "platform": f"{sys.platform}-{platform.machine() or 'unknown'}",
        },
        "recording": {
            "program": " ".join([sys.executable, *sys.argv]),
            "working_directory": os.getcwd(),
            "scope_config": {"include": [], "exclude": [], "depth_limit": None},
        },
        "program": None,
        "trace_uuid": uuid.uuid4().bytes,
        "recording_start_ns": start_ns,
    }


# --- The decorator ----------------------------------------------------------


def record(func: Callable) -> Callable:
    """Decorate a function so that calling it records a trace.

    Usage::

        @hindsight.record
        def buggy(req):
            ...

        buggy(req)
        # writes ./trace.hindsight (or $HINDSIGHT_OUTPUT_PATH if set)

    Scope is "everything reachable from `func`": all transitively-called
    Python functions are recorded. Per-call exclusions, depth limits, and
    the parenthesized form ``@record(depth=...)`` come in a later session.
    """

    @functools.wraps(func)
    def wrapper(*args: Any, **kwargs: Any) -> Any:
        global _state

        if _state is not None:
            # Already inside a recorded scope — likely a nested @record. We
            # don't currently support stacking; just call through. (Once
            # context-manager scoping lands, this case may want to refine
            # to "extend the current recording.")
            return func(*args, **kwargs)

        output_path = os.environ.get("HINDSIGHT_OUTPUT_PATH", "trace.hindsight")
        skip_codes = _build_skip_codes(wrapper.__code__)
        state = _RecorderState(output_path=output_path, skip_codes=skip_codes)
        _state = state

        try:
            _activate_monitoring()
            return func(*args, **kwargs)
        finally:
            try:
                _deactivate_monitoring()
            finally:
                _state = None
                _finalize(state)

    return wrapper


# --- Skip-code computation --------------------------------------------------


def _build_skip_codes(wrapper_code: Any) -> frozenset:
    """Code objects whose events we silently drop.

    sys.monitoring fires events for our own Python code too — the
    ``wrapper`` closure, the callbacks, and helpers reached from them.
    Filtering by code-object identity is cheap and unambiguous; the
    alternative ("skip events whose filename matches our package path") is
    fragile for editable installs and zip imports.

    The user might call ``@record`` on functions defined inside this
    module (unusual but allowed). To keep that case from being silently
    swallowed we *only* skip code objects we explicitly enumerate, never
    by filename.
    """
    funcs = (
        wrapper_code,
        record,
        _activate_monitoring,
        _deactivate_monitoring,
        _finalize,
        _on_py_start,
        _on_py_return,
        _on_line,
        _capture_args,
        _capture_locals_into_changes,
        _is_real_source_path,
        _build_metadata,
        _safe_intern_value,
        _RecorderState.__init__,
        _RecorderState.timestamp_delta_ns,
        _RecorderState.alloc_frame_id,
        _RecorderState.get_or_intern_source,
    )
    return frozenset(
        f if not callable(f) else f.__code__
        for f in funcs
        if isinstance(f, type) or callable(f)
    )


# --- sys.monitoring lifecycle ----------------------------------------------


def _activate_monitoring() -> None:
    sys.monitoring.use_tool_id(_TOOL_ID, _TOOL_NAME)
    events = sys.monitoring.events
    sys.monitoring.register_callback(_TOOL_ID, events.PY_START, _on_py_start)
    sys.monitoring.register_callback(_TOOL_ID, events.PY_RETURN, _on_py_return)
    sys.monitoring.register_callback(_TOOL_ID, events.LINE, _on_line)
    sys.monitoring.set_events(
        _TOOL_ID, events.PY_START | events.PY_RETURN | events.LINE
    )


def _deactivate_monitoring() -> None:
    """Best-effort teardown. Called from the wrapper's ``finally`` so we
    must not raise; if monitoring was never activated (e.g., a different
    tool stole the slot) we silently move on."""
    try:
        sys.monitoring.set_events(_TOOL_ID, 0)
    except (ValueError, RuntimeError):
        pass
    events = sys.monitoring.events
    for ev in (events.PY_START, events.PY_RETURN, events.LINE):
        try:
            sys.monitoring.register_callback(_TOOL_ID, ev, None)
        except (ValueError, RuntimeError):
            pass
    try:
        sys.monitoring.free_tool_id(_TOOL_ID)
    except (ValueError, RuntimeError):
        pass


def _finalize(state: _RecorderState) -> None:
    end_ns = time.time_ns()
    state.writer.finish(state.output_path, end_ns)
    sys.stderr.write(
        f"hindsight: trace written to {state.output_path} "
        f"({state.event_count} events)\n"
    )


# --- sys.monitoring callbacks ----------------------------------------------


def _on_py_start(code: Any, instruction_offset: int) -> Any:
    """PY_START — a Python function began executing.

    Fires *inside* the new function's frame, so ``sys._getframe(1)`` is the
    frame the function just entered (``_getframe(0)`` is this callback's
    own frame). We allocate a frame_id, capture args from f_locals, emit
    FUNCTION_ENTRY, then emit a FRAME_SNAPSHOT with all locals so later
    LINE_DELTAs have a baseline to diff against.
    """
    global _in_callback
    state = _state
    if state is None or _in_callback or code in state.skip_codes:
        return
    file_id = state.get_or_intern_source(code.co_filename)
    if file_id is None:
        # Code from a non-disk source; record nothing. PY_RETURN will look
        # up id(frame), miss, and also do nothing. LINE same.
        return

    _in_callback = True
    try:
        frame = sys._getframe(1)
        frame_id = state.alloc_frame_id(frame)
        function_id = state.writer.intern_string(code.co_qualname)
        args = _capture_args(state, code, frame)

        delta = state.timestamp_delta_ns()
        state.writer.write_function_entry(
            timestamp_delta_ns=delta,
            frame_id=frame_id,
            function_id=function_id,
            source_file_id=file_id,
            line=code.co_firstlineno,
            args=args,
        )
        state.event_count += 1

        # Baseline FRAME_SNAPSHOT for subsequent LINE_DELTA diffing.
        last = state.last_value_id_by_local[id(frame)]
        snapshot_locals: list[tuple[int, int]] = []
        for name, value in frame.f_locals.items():
            value_id = _safe_intern_value(state, value)
            if value_id is None:
                continue
            name_id = state.writer.intern_string(name)
            snapshot_locals.append((name_id, value_id))
            last[name] = value_id
        state.writer.write_frame_snapshot(
            timestamp_delta_ns=0,
            frame_id=frame_id,
            line=code.co_firstlineno,
            locals=snapshot_locals,
        )
        state.event_count += 1
    finally:
        _in_callback = False


def _on_py_return(code: Any, instruction_offset: int, retval: Any) -> Any:
    """PY_RETURN — a Python function returned (normally; PY_THROW handles
    exception unwind, and we don't subscribe to that yet)."""
    global _in_callback
    state = _state
    if state is None or _in_callback or code in state.skip_codes:
        return
    _in_callback = True
    try:
        frame = sys._getframe(1)
        frame_key = id(frame)
        frame_id = state.frame_id_by_pyframe.pop(frame_key, None)
        if frame_id is None:
            # We never recorded a PY_START for this frame (skipped source
            # file, or PY_START fired before our state existed). Nothing
            # to emit, nothing to clean up.
            state.last_value_id_by_local.pop(frame_key, None)
            return
        state.last_value_id_by_local.pop(frame_key, None)

        return_value_id = _safe_intern_value(state, retval)
        if return_value_id is None:
            # Truly bizarre — even Summary fallback shouldn't fail. Use
            # the reserved None sentinel rather than corrupting the trace.
            return_value_id = 0  # NONE_VALUE_ID
        delta = state.timestamp_delta_ns()
        state.writer.write_function_exit(
            timestamp_delta_ns=delta,
            frame_id=frame_id,
            return_value=return_value_id,
        )
        state.event_count += 1
    finally:
        _in_callback = False


def _on_line(code: Any, line_number: int) -> Any:
    """LINE — a source line is about to execute. Emit LINE_DELTA capturing
    only the locals whose value-id has changed since the last event in
    this frame."""
    global _in_callback
    state = _state
    if state is None or _in_callback or code in state.skip_codes:
        return
    _in_callback = True
    try:
        frame = sys._getframe(1)
        frame_key = id(frame)
        frame_id = state.frame_id_by_pyframe.get(frame_key)
        if frame_id is None:
            # Frame was skipped (its source file didn't resolve), or LINE
            # fired before we got a PY_START for it — nothing to compare
            # against. Drop silently.
            return
        last = state.last_value_id_by_local[frame_key]
        changes = _capture_locals_into_changes(state, frame, last)

        delta = state.timestamp_delta_ns()
        state.writer.write_line_delta(
            timestamp_delta_ns=delta,
            line=line_number,
            changes=changes,
        )
        state.event_count += 1
    finally:
        _in_callback = False


# --- Capture helpers --------------------------------------------------------


def _capture_args(state: _RecorderState, code: Any, frame: Any) -> list[tuple[int, int]]:
    """Pull the function's parameters out of ``frame.f_locals`` for the
    FUNCTION_ENTRY ``args`` list.

    Python populates parameter slots before the first user instruction
    runs, so f_locals at PY_START already contains them. We pick out:

    - positional args (``co_varnames[:co_argcount]``)
    - keyword-only args (next ``co_kwonlyargcount`` slots)
    - ``*args`` (one slot, present iff ``CO_VARARGS`` is set)
    - ``**kwargs`` (one slot, present iff ``CO_VARKEYWORDS`` is set)

    For methods, ``self`` shows up as the first positional arg and is
    captured like any other; the value-conversion layer will Summary it
    (custom classes hit the summary fallback by design).
    """
    co_argcount = code.co_argcount
    co_kwonly = code.co_kwonlyargcount
    var_names = code.co_varnames
    flags = code.co_flags
    locs = frame.f_locals

    n_named = co_argcount + co_kwonly
    slot_indices = list(range(n_named))
    # CO_VARARGS = 0x04, CO_VARKEYWORDS = 0x08 in Include/code.h.
    if flags & 0x04 and n_named < len(var_names):
        slot_indices.append(n_named)
    if flags & 0x08:
        kwarg_idx = n_named + (1 if flags & 0x04 else 0)
        if kwarg_idx < len(var_names):
            slot_indices.append(kwarg_idx)

    args: list[tuple[int, int]] = []
    for idx in slot_indices:
        name = var_names[idx]
        if name not in locs:
            continue
        value_id = _safe_intern_value(state, locs[name])
        if value_id is None:
            continue
        name_id = state.writer.intern_string(name)
        args.append((name_id, value_id))
    return args


def _capture_locals_into_changes(
    state: _RecorderState, frame: Any, last: dict[str, int]
) -> list[tuple[int, int]]:
    """Compare the current f_locals against the per-frame "last value id"
    map and return the list of changes for LINE_DELTA. Updates ``last``
    in place so the next LINE_DELTA in this frame diffs against the
    state we just recorded."""
    changes: list[tuple[int, int]] = []
    for name, value in frame.f_locals.items():
        value_id = _safe_intern_value(state, value)
        if value_id is None:
            continue
        if last.get(name) == value_id:
            continue
        name_id = state.writer.intern_string(name)
        changes.append((name_id, value_id))
        last[name] = value_id
    return changes


def _safe_intern_value(state: _RecorderState, value: Any) -> int | None:
    """Intern ``value``, returning ``None`` on any failure rather than
    propagating into the user's program. The most likely failure is a
    user-defined ``__repr__`` that raises (the value-conversion layer
    falls back to Summary for unknown types, which calls ``repr()``)."""
    try:
        return state.writer.intern_value(value)
    except Exception:
        return None
