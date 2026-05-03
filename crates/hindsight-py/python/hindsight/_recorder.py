# SPDX-License-Identifier: Apache-2.0
"""Recorder implementation: the @hindsight.record decorator and
hindsight.skip() context manager.

The recorder turns on ``sys.monitoring`` for the duration of a decorated
function call, captures FUNCTION_ENTRY / FRAME_SNAPSHOT / LINE_DELTA /
FUNCTION_EXIT events for everything in scope, emits SCOPE_BOUNDARY
events at scope transitions, and writes a ``.hindsight`` trace file
when the decorated function returns.

Scope is determined per-frame at PY_START. A frame can be in one of
four modes:

- ``RECORDING``: events are captured normally.
- ``EXCLUDED``: function matched an exclude pattern; we emit
  SCOPE_BOUNDARY at enter and exit, and *no* interior events.
- ``DEPTH_CLIPPED``: function is past ``depth_limit`` from the
  decorated frame; otherwise like EXCLUDED.
- ``SKIPPED_SUBTREE``: parent frame is in a ``hindsight.skip()`` block,
  or parent is itself non-recording. No events, no boundary.

A frame's mode is sticky for its entire lifetime (decision (a) from the
session brief: once a subtree is excluded, its descendants stay
excluded — even if they'd match an include pattern individually).

Out-of-scope for v0: BRANCH_RESULT and EXCEPTION_RAISED events,
generators / coroutines, threads, the ``[capture]`` config section.
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

from . import _config
from ._config import ScopeConfig
from ._core import TraceWriter

__all__ = ["record", "skip"]

# --- sys.monitoring tool registration ---------------------------------------
#
# Tool ID 3 (one of the two slots CPython leaves free for third-party
# tools — see session B's notes). Slots 0/1/2/5 are reserved for
# debuggers/coverage/profilers/the JIT.
_TOOL_ID: int = 3
_TOOL_NAME: str = "hindsight"

# --- SCOPE_BOUNDARY type tags (mirrored from the trace format spec) ---------
_BOUNDARY_ENTERED_SKIP = 0x01
_BOUNDARY_EXITED_SKIP = 0x02
_BOUNDARY_ENTERED_EXCLUDED = 0x03
_BOUNDARY_EXITED_EXCLUDED = 0x04
_BOUNDARY_ENTERED_DEPTH_CLIPPED = 0x05
_BOUNDARY_EXITED_DEPTH_CLIPPED = 0x06

# --- Per-frame mode constants ----------------------------------------------
_MODE_RECORDING = "recording"
_MODE_EXCLUDED = "excluded"
_MODE_DEPTH_CLIPPED = "depth_clipped"
_MODE_SKIPPED_SUBTREE = "skipped_subtree"


# --- Single-recording-at-a-time guard ---------------------------------------
_state: "_RecorderState | None" = None

# Re-entrancy guard for callbacks. While a callback runs we may invoke
# user-defined ``__repr__`` (via the value-conversion layer's summary
# fallback), which is itself Python and would otherwise re-fire
# PY_START / LINE / PY_RETURN. The flag short-circuits that recursion.
_in_callback: bool = False


# --- Per-frame info ---------------------------------------------------------


class _FrameInfo:
    """One entry per Python frame the recorder has seen.

    Stored in ``state.frame_info_by_pyframe`` keyed by ``id(frame)`` for
    the frame's lifetime (PY_START until matching PY_RETURN).
    """

    __slots__ = (
        "mode",
        "frame_id",
        "depth",
        "skip_depth",
        "qualname",
        "matched_pattern",
        "boundary_emitted_at_enter",
    )

    def __init__(
        self,
        mode: str,
        frame_id: int | None,
        depth: int,
        qualname: str,
    ) -> None:
        self.mode = mode
        # `frame_id` is the trace's stable function activation ID; only
        # populated for RECORDING mode (other modes don't get
        # FUNCTION_ENTRY/EXIT events).
        self.frame_id = frame_id
        self.depth = depth
        # Number of currently-active hindsight.skip() blocks owned by
        # this frame. Only meaningful for RECORDING; child frames
        # entered while skip_depth > 0 inherit SKIPPED_SUBTREE mode.
        self.skip_depth = 0
        self.qualname = qualname
        self.matched_pattern: str | None = None
        # True iff we emitted an entered_excluded / entered_depth_clipped
        # boundary at PY_START. We need it because the matching exit
        # boundary at PY_RETURN should fire only for frames that owned
        # the boundary entry (not for SKIPPED_SUBTREE frames inside an
        # already-excluded scope).
        self.boundary_emitted_at_enter = False


# --- Recorder state ---------------------------------------------------------


class _RecorderState:
    """Per-recording-session state."""

    __slots__ = (
        "writer",
        "output_path",
        "config",
        "recording_start_ns",
        "_last_event_wall_ns",
        "_next_frame_id",
        "frame_info_by_pyframe",
        "last_value_id_by_local",
        "source_id_by_path",
        "skipped_files",
        "event_count",
        "skip_codes",
        "recorded_qualnames",
        "excluded_records",
        "skip_blocks_observed",
        "depth_clips_observed",
    )

    def __init__(
        self,
        output_path: str,
        skip_codes: frozenset,
        config: ScopeConfig,
    ) -> None:
        self.recording_start_ns = time.time_ns()
        self._last_event_wall_ns = self.recording_start_ns
        self.writer = TraceWriter(_build_metadata(self.recording_start_ns, config))
        self.output_path = output_path
        self.config = config
        self._next_frame_id: int = 0
        self.frame_info_by_pyframe: dict[int, _FrameInfo] = {}
        self.last_value_id_by_local: dict[int, dict[str, int]] = {}
        self.source_id_by_path: dict[str, int] = {}
        self.skipped_files: set[str] = set()
        self.event_count: int = 0
        self.skip_codes = skip_codes
        # Resolved-scope tallies, drained into the trace's final summary
        # at finalization.
        self.recorded_qualnames: set[str] = set()
        self.excluded_records: list[tuple[str, str]] = []
        self.skip_blocks_observed: int = 0
        self.depth_clips_observed: int = 0

    def timestamp_delta_ns(self) -> int:
        now = time.time_ns()
        delta = now - self._last_event_wall_ns
        self._last_event_wall_ns = now
        return delta if delta >= 0 else 0

    def alloc_frame_id(self) -> int:
        fid = self._next_frame_id
        self._next_frame_id += 1
        return fid

    def get_or_intern_source(self, path: str) -> int | None:
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

    def find_tracked_ancestor(self, frame: Any) -> _FrameInfo | None:
        """Walk ``frame.f_back`` to find the nearest ancestor we've
        registered. Used at PY_START to inherit mode/depth when the
        immediate parent wasn't tracked (e.g., it lived in a non-disk
        source file and was silently passed over)."""
        f = frame.f_back
        while f is not None:
            info = self.frame_info_by_pyframe.get(id(f))
            if info is not None:
                return info
            f = f.f_back
        return None


def _is_real_source_path(path: str) -> bool:
    """Reject paths that aren't real files on disk (REPL, exec, frozen)."""
    if not path or path.startswith("<"):
        return False
    return os.path.isfile(path)


def _build_metadata(start_ns: int, config: ScopeConfig) -> dict:
    """Construct the metadata dict TraceWriter wants. The scope_config
    sub-table mirrors the ScopeConfig the recorder actually used so a
    reader can attribute the trace's contents to the right rules."""
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
            "scope_config": {
                "include": list(config.include),
                "exclude": list(config.exclude),
                "depth_limit": config.depth_limit,
            },
        },
        "program": None,
        "trace_uuid": uuid.uuid4().bytes,
        "recording_start_ns": start_ns,
    }


# --- The decorator ----------------------------------------------------------


def record(func: Callable) -> Callable:
    """Decorate a function so calling it records a trace.

    The recorder reads ``hindsight.toml`` (resolved per ``_config``) at
    each call to obtain include/exclude patterns and ``depth_limit``.
    The output path comes from ``$HINDSIGHT_OUTPUT_PATH`` or defaults
    to ``./trace.hindsight``.
    """

    @functools.wraps(func)
    def wrapper(*args: Any, **kwargs: Any) -> Any:
        global _state

        if _state is not None:
            # Already inside a recorded scope. Stacking @record isn't
            # supported — pass through to the underlying function.
            return func(*args, **kwargs)

        output_path = os.environ.get("HINDSIGHT_OUTPUT_PATH", "trace.hindsight")
        config = _config.load_config()
        skip_codes = _build_skip_codes(wrapper.__code__)
        state = _RecorderState(
            output_path=output_path, skip_codes=skip_codes, config=config
        )
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


# --- The skip() context manager --------------------------------------------


class _SkipContextManager:
    """Returned by :func:`skip`. Increments the calling frame's
    ``skip_depth`` on enter; decrements on exit. Only the outermost
    enter/exit emits SCOPE_BOUNDARY events (nested skip blocks are
    no-ops — we're already skipping)."""

    __slots__ = ("_owner_pyframe", "_was_outermost")

    def __init__(self) -> None:
        # The frame that owns this skip block (the one whose body
        # contains the `with hindsight.skip():` statement). Only
        # captured if we actually act — otherwise __exit__ is a no-op.
        self._owner_pyframe: int | None = None
        self._was_outermost: bool = False

    def __enter__(self) -> "_SkipContextManager":
        global _in_callback
        state = _state
        if state is None:
            return self  # No active recording — skip() outside @record is a no-op.

        # The caller frame is the one with the `with` statement.
        caller = sys._getframe(1)
        info = state.frame_info_by_pyframe.get(id(caller))
        if info is None or info.mode != _MODE_RECORDING:
            # Calling skip() from a non-RECORDING frame doesn't change
            # anything: the frame's events are already suppressed. We
            # don't need to balance anything in __exit__ either.
            return self

        self._owner_pyframe = id(caller)
        prev = info.skip_depth
        info.skip_depth = prev + 1
        if prev == 0:
            self._was_outermost = True
            _in_callback = True
            try:
                reason_id = state.writer.intern_string("user-requested skip block")
                delta = state.timestamp_delta_ns()
                state.writer.write_scope_boundary(
                    delta, _BOUNDARY_ENTERED_SKIP, reason_id
                )
                state.event_count += 1
                state.skip_blocks_observed += 1
            finally:
                _in_callback = False
        return self

    def __exit__(self, exc_type, exc_val, exc_tb) -> bool:
        global _in_callback
        state = _state
        if state is None or self._owner_pyframe is None:
            return False
        info = state.frame_info_by_pyframe.get(self._owner_pyframe)
        if info is None:
            # Owner frame already returned (shouldn't happen with
            # well-formed `with` blocks, but be defensive).
            return False
        info.skip_depth = max(0, info.skip_depth - 1)
        if self._was_outermost:
            _in_callback = True
            try:
                reason_id = state.writer.intern_string("user-requested skip block")
                delta = state.timestamp_delta_ns()
                state.writer.write_scope_boundary(
                    delta, _BOUNDARY_EXITED_SKIP, reason_id
                )
                state.event_count += 1
            finally:
                _in_callback = False
        return False


def skip() -> _SkipContextManager:
    """Suspend recording for the duration of a ``with`` block.

    Inside ``with hindsight.skip():``:

    - LINE events from the enclosing frame are suppressed.
    - Functions called from the block (and their entire subtrees) are
      not recorded; no FUNCTION_ENTRY / SCOPE_BOUNDARY events are
      emitted for them.
    - Nested ``hindsight.skip()`` blocks are no-ops; only the outermost
      enter/exit emits boundary events.

    Skip mode is **sticky per-frame**: a function called from inside a
    skip block remains excluded for its entire lifetime, even if the
    skip block exits before that function returns. (See decision (4)
    in the session brief.)

    Outside an active ``@hindsight.record`` scope, ``skip()`` is a
    no-op.
    """
    return _SkipContextManager()


# --- Skip-code computation --------------------------------------------------


def _build_skip_codes(wrapper_code: Any) -> frozenset:
    """Code objects whose events the callbacks silently drop. Includes
    the wrapper closure, the callback functions, and helpers reached
    from them."""
    funcs = (
        wrapper_code,
        record,
        skip,
        _activate_monitoring,
        _deactivate_monitoring,
        _finalize,
        _on_py_start,
        _on_py_return,
        _on_line,
        _resolve_mode_for_new_frame,
        _qualified_name,
        _capture_args,
        _capture_locals_into_changes,
        _is_real_source_path,
        _build_metadata,
        _safe_intern_value,
        _RecorderState.__init__,
        _RecorderState.timestamp_delta_ns,
        _RecorderState.alloc_frame_id,
        _RecorderState.get_or_intern_source,
        _RecorderState.find_tracked_ancestor,
        _SkipContextManager.__init__,
        _SkipContextManager.__enter__,
        _SkipContextManager.__exit__,
        _FrameInfo.__init__,
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
    """Best-effort teardown. Called from the wrapper's ``finally``."""
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
    scope_resolution = {
        "recorded_functions": sorted(state.recorded_qualnames),
        "excluded_functions": list(state.excluded_records),
        "skip_blocks_observed": state.skip_blocks_observed,
        "depth_clips_observed": state.depth_clips_observed,
    }
    state.writer.finish(state.output_path, end_ns, scope_resolution)
    sys.stderr.write(
        f"hindsight: trace written to {state.output_path} "
        f"({state.event_count} events)\n"
    )


# --- Mode resolution --------------------------------------------------------


def _qualified_name(code: Any, frame: Any) -> str:
    """Build the fully-qualified function name used for include/exclude
    pattern matching.

    Format: ``<module>.<co_qualname>``. Examples:

    - top-level function ``foo`` in module ``myapp.helpers`` →
      ``myapp.helpers.foo``
    - method ``bar`` on class ``MyClass`` in same module →
      ``myapp.helpers.MyClass.bar``
    - lambda → ``myapp.helpers.<some_owner>.<lambda>``
    - listcomp → ``myapp.helpers.<some_owner>.<listcomp>`` (3.12+)
    - module-level code → ``myapp.helpers.<module>``

    Missing ``__name__`` (the ``"?"`` prefix)
    -----------------------------------------

    If ``frame.f_globals`` doesn't have a ``__name__`` key (or it's
    falsy / raises on access), we use the literal string ``"?"`` as the
    module placeholder. The qualified name then looks like
    ``?.<co_qualname>`` (e.g. ``?.some_func``).

    This is rare in practice — `exec()` and `eval()` typically receive
    a globals dict with ``__name__`` set to ``__main__`` or whatever
    the user passed. The case mostly arises from hand-rolled
    invocations like ``exec(code, {})``.

    These frames are matched against include/exclude patterns by the
    same fnmatch rules as any other qualname. Importantly, ``"?"`` is a
    literal character on the *input* side of fnmatch — it has no glob
    meaning when it appears in the qualname being matched. So:

    - **No config / `exclude=["defaults"]`:** ``?.some_func`` doesn't
      match any default pattern (``numpy.*``, ``pandas.*``, ...) and
      include is empty → recorded.
    - **`include=["myapp.*"]`:** ``?.some_func`` doesn't match
      ``myapp.*`` and the include filter is active → EXCLUDED with
      reason ``"not in include patterns"``.
    - **`exclude=["numpy.*"]` (no include):** ``?.some_func`` doesn't
      match → recorded.
    - **`include=["myapp.*"]` plus `exclude=["numpy.*"]`:** same as
      include-only — non-match against include wins → EXCLUDED.

    A user who wants to capture missing-module frames under a strict
    include config should add ``"?.*"`` to the include list. Users who
    want to exclude them similarly add ``"?.*"`` to exclude. We
    deliberately don't special-case missing-module frames as
    "always-record user code" — the rule is consistent across all
    qualnames, which makes the matching behavior predictable.
    """
    module_name = "?"
    try:
        module_name = frame.f_globals.get("__name__") or "?"
    except Exception:
        pass
    return f"{module_name}.{code.co_qualname}"


def _resolve_mode_for_new_frame(
    state: _RecorderState, frame: Any, qualname: str
) -> tuple[str, int, str | None]:
    """Compute (mode, depth, matched_pattern) for a freshly-entered frame.

    Resolution order:

    1. If the parent frame's effective mode is non-RECORDING (excluded,
       depth-clipped, or RECORDING-but-skipping), inherit
       ``SKIPPED_SUBTREE``. This is the sticky-exclusion rule (decision
       (a) from the session brief).
    2. Else compute candidate depth = parent_depth + 1 (or 0 if no
       parent is tracked, i.e. this is the outermost decorated frame).
    3. Apply rules in order:
       - explicit include match → RECORDING (overrides exclude/depth)
       - depth > depth_limit → DEPTH_CLIPPED
       - explicit exclude match → EXCLUDED
       - else → RECORDING by default
    """
    parent_info = state.find_tracked_ancestor(frame)
    if parent_info is None:
        # Outermost recorded frame (no tracked ancestor — the wrapper
        # isn't tracked, and the user's caller chain above it isn't
        # either).
        parent_recording_effectively = True
        parent_depth = -1
    else:
        parent_recording_effectively = (
            parent_info.mode == _MODE_RECORDING and parent_info.skip_depth == 0
        )
        parent_depth = parent_info.depth

    if not parent_recording_effectively:
        # Sticky: anything inside an excluded / depth-clipped / skipped
        # subtree stays in that subtree until the enclosing frame
        # returns. We don't emit a boundary for inner frames; the
        # boundary was emitted when the subtree was first entered.
        return _MODE_SKIPPED_SUBTREE, parent_depth + 1, None

    new_depth = parent_depth + 1
    config = state.config

    # Step 1: include / exclude resolution. Per session brief decision
    # (3) and `docs/scope-control.md`, an explicit include match always
    # wins — it overrides both exclude patterns and the implicit
    # "non-matching is excluded" effect of a non-empty include list. A
    # non-empty include with no match means "out of scope"; an empty
    # include falls through to exclude.
    include_match = config.matches_include(qualname)
    if not include_match:
        if config.include:
            # Active include filter and this function doesn't match —
            # treat as excluded with a synthetic reason.
            return _MODE_EXCLUDED, new_depth, "not in include patterns"
        excluded, pattern = config.matches_exclude(qualname)
        if excluded:
            return _MODE_EXCLUDED, new_depth, pattern

    # Step 2: depth limit. Applies even to include-matched functions —
    # depth is a structural cap, not a filter. (If the user wrote a
    # depth_limit and an include pattern, both are honored independently.)
    if config.depth_limit is not None and new_depth > config.depth_limit:
        return _MODE_DEPTH_CLIPPED, new_depth, None

    return _MODE_RECORDING, new_depth, None


# --- sys.monitoring callbacks ----------------------------------------------


def _on_py_start(code: Any, instruction_offset: int) -> Any:
    """PY_START — a Python function began executing.

    The frame being entered is ``sys._getframe(1)`` (``_getframe(0)`` is
    this callback itself). We resolve the frame's mode and either:

    - emit FUNCTION_ENTRY + FRAME_SNAPSHOT (RECORDING), or
    - emit SCOPE_BOUNDARY entered_excluded / entered_depth_clipped, or
    - emit nothing (SKIPPED_SUBTREE).

    Either way we register the frame in ``frame_info_by_pyframe`` so
    PY_RETURN knows what to do.
    """
    global _in_callback
    state = _state
    if state is None or _in_callback or code in state.skip_codes:
        return

    _in_callback = True
    try:
        frame = sys._getframe(1)
        qualname = _qualified_name(code, frame)
        mode, depth, pattern = _resolve_mode_for_new_frame(state, frame, qualname)

        info = _FrameInfo(mode=mode, frame_id=None, depth=depth, qualname=qualname)
        info.matched_pattern = pattern

        if mode == _MODE_RECORDING:
            file_id = state.get_or_intern_source(code.co_filename)
            if file_id is None:
                # Source isn't on disk; we can't intern it. Don't track
                # this frame at all — children will walk past it via
                # frame.f_back to find a tracked ancestor.
                return
            frame_id = state.alloc_frame_id()
            info.frame_id = frame_id
            state.frame_info_by_pyframe[id(frame)] = info
            state.last_value_id_by_local[id(frame)] = {}
            state.recorded_qualnames.add(qualname)

            # Per the trace format spec: function_id references the
            # *fully* qualified name (module-prefixed), not just
            # ``code.co_qualname``. The AI consumer needs the module to
            # disambiguate top-level functions across files.
            function_id = state.writer.intern_string(qualname)
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
            return

        if mode == _MODE_EXCLUDED:
            reason_str = (
                f"matched pattern: {pattern}" if pattern else "excluded by config"
            )
            reason_id = state.writer.intern_string(reason_str)
            delta = state.timestamp_delta_ns()
            state.writer.write_scope_boundary(
                delta, _BOUNDARY_ENTERED_EXCLUDED, reason_id
            )
            state.event_count += 1
            info.boundary_emitted_at_enter = True
            state.frame_info_by_pyframe[id(frame)] = info
            state.excluded_records.append((qualname, pattern or ""))
            return

        if mode == _MODE_DEPTH_CLIPPED:
            limit = state.config.depth_limit
            reason_str = f"depth limit {limit} exceeded"
            reason_id = state.writer.intern_string(reason_str)
            delta = state.timestamp_delta_ns()
            state.writer.write_scope_boundary(
                delta, _BOUNDARY_ENTERED_DEPTH_CLIPPED, reason_id
            )
            state.event_count += 1
            info.boundary_emitted_at_enter = True
            state.frame_info_by_pyframe[id(frame)] = info
            state.depth_clips_observed += 1
            return

        # SKIPPED_SUBTREE: track the frame but emit nothing.
        state.frame_info_by_pyframe[id(frame)] = info
    finally:
        _in_callback = False


def _on_py_return(code: Any, instruction_offset: int, retval: Any) -> Any:
    """PY_RETURN — a Python function returned (normally; PY_THROW is a
    different event we don't subscribe to yet)."""
    global _in_callback
    state = _state
    if state is None or _in_callback or code in state.skip_codes:
        return

    _in_callback = True
    try:
        frame = sys._getframe(1)
        frame_key = id(frame)
        info = state.frame_info_by_pyframe.pop(frame_key, None)
        if info is None:
            # Untracked frame (source wasn't on disk, or PY_START fired
            # before our state existed). Ensure the locals map doesn't
            # leak.
            state.last_value_id_by_local.pop(frame_key, None)
            return
        state.last_value_id_by_local.pop(frame_key, None)

        if info.mode == _MODE_RECORDING:
            return_value_id = _safe_intern_value(state, retval)
            if return_value_id is None:
                return_value_id = 0  # NONE_VALUE_ID — last-resort fallback.
            delta = state.timestamp_delta_ns()
            state.writer.write_function_exit(
                timestamp_delta_ns=delta,
                frame_id=info.frame_id,
                return_value=return_value_id,
            )
            state.event_count += 1
            return

        if info.mode == _MODE_EXCLUDED and info.boundary_emitted_at_enter:
            reason_str = (
                f"matched pattern: {info.matched_pattern}"
                if info.matched_pattern
                else "excluded by config"
            )
            reason_id = state.writer.intern_string(reason_str)
            delta = state.timestamp_delta_ns()
            state.writer.write_scope_boundary(
                delta, _BOUNDARY_EXITED_EXCLUDED, reason_id
            )
            state.event_count += 1
            return

        if info.mode == _MODE_DEPTH_CLIPPED and info.boundary_emitted_at_enter:
            limit = state.config.depth_limit
            reason_str = f"depth limit {limit} exceeded"
            reason_id = state.writer.intern_string(reason_str)
            delta = state.timestamp_delta_ns()
            state.writer.write_scope_boundary(
                delta, _BOUNDARY_EXITED_DEPTH_CLIPPED, reason_id
            )
            state.event_count += 1
            return

        # SKIPPED_SUBTREE: no exit event — the boundary was owned by
        # whichever ancestor first transitioned out of recording.
    finally:
        _in_callback = False


def _on_line(code: Any, line_number: int) -> Any:
    """LINE — a source line is about to execute. Emit LINE_DELTA only if
    the frame is RECORDING and not currently in a skip block."""
    global _in_callback
    state = _state
    if state is None or _in_callback or code in state.skip_codes:
        return
    _in_callback = True
    try:
        frame = sys._getframe(1)
        frame_key = id(frame)
        info = state.frame_info_by_pyframe.get(frame_key)
        if info is None or info.mode != _MODE_RECORDING:
            return
        if info.skip_depth > 0:
            # The frame is currently inside a hindsight.skip() block —
            # suppress LINE events until the block exits. Function
            # calls from inside the skip block become SKIPPED_SUBTREE
            # via the resolution rules in `_resolve_mode_for_new_frame`.
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
    FUNCTION_ENTRY ``args`` list. Covers positional, keyword-only,
    ``*args``, and ``**kwargs`` slots."""
    co_argcount = code.co_argcount
    co_kwonly = code.co_kwonlyargcount
    var_names = code.co_varnames
    flags = code.co_flags
    locs = frame.f_locals

    n_named = co_argcount + co_kwonly
    slot_indices = list(range(n_named))
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
    """Diff the current ``f_locals`` against ``last`` and return changes."""
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
    """Intern, swallowing any failure (most likely a user ``__repr__``
    that raises). Better to drop a value than crash the program."""
    try:
        return state.writer.intern_value(value)
    except Exception:
        return None
