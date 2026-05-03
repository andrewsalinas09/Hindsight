# SPDX-License-Identifier: Apache-2.0
"""Tests for scope control: hindsight.toml loading, include/exclude
pattern matching, depth limits, and the hindsight.skip() context
manager.

Notes for readers:

- The recorder builds qualified names from ``frame.f_globals["__name__"]``
  + ``code.co_qualname``. Pytest imports test modules under their basename
  when there's no ``__init__.py`` next to them, so functions defined
  here have qualified name ``test_scope.<co_qualname>``. Patterns in
  these tests use that prefix.
- Each test runs in its own ``tmp_path`` and uses ``HINDSIGHT_OUTPUT_PATH``
  / ``HINDSIGHT_CONFIG`` (set via ``monkeypatch``) to keep state
  isolated. The decorator picks up both env vars at call time.
"""

from __future__ import annotations

import sys
import textwrap
import tomllib
from pathlib import Path

import pytest

import hindsight
from hindsight._config import (
    DEFAULT_EXCLUSIONS,
    ScopeConfig,
    find_config_path,
    load_config,
    parse_config,
)


# --- Module-level decorated test subjects ---------------------------------


def helper_a(n: int) -> int:
    return n + 1


def helper_b(n: int) -> int:
    return n * 2


@hindsight.record
def calls_two_helpers(n: int) -> int:
    a = helper_a(n)
    b = helper_b(n)
    return a + b


@hindsight.record
def deep_chain(n: int) -> int:
    return level_1(n)


def level_1(n: int) -> int:
    return level_2(n)


def level_2(n: int) -> int:
    return level_3(n)


def level_3(n: int) -> int:
    return level_4(n)


def level_4(n: int) -> int:
    return n + 1


@hindsight.record
def with_skip_block(n: int) -> int:
    before = n + 1
    with hindsight.skip():
        skipped = n * 100
    after = n + 2
    return before + after + skipped


@hindsight.record
def with_nested_skip_blocks(n: int) -> int:
    before = n + 1
    with hindsight.skip():
        outer_inside = n * 10
        with hindsight.skip():
            inner = n * 100
    after = n + 2
    return before + after + inner + outer_inside


def callee_inside_skip(x: int) -> int:
    inner_local = x * 3
    return inner_local


@hindsight.record
def calls_function_inside_skip(n: int) -> int:
    before = n + 1
    with hindsight.skip():
        result = callee_inside_skip(n)
    after = n + 2
    return before + after + result


@hindsight.record
def trivial(n: int) -> int:
    return n + 1


# --- Helpers ---------------------------------------------------------------


def _value_at(trace: dict, value_id: int):
    return trace["values"][value_id]["decoded"]


def _string_at(trace: dict, string_id: int) -> str:
    return trace["strings"][string_id]


def _events_of(trace: dict, type_name: str) -> list[dict]:
    return [e for e in trace["events"] if e["type"] == type_name]


def _function_entry_qualnames(trace: dict) -> list[str]:
    return [
        _string_at(trace, e["function_id"]) for e in _events_of(trace, "function_entry")
    ]


def _boundary_events(trace: dict) -> list[tuple[int, str]]:
    """Return ``(boundary_type, reason_string)`` for each SCOPE_BOUNDARY."""
    return [
        (e["boundary_type"], _string_at(trace, e["reason"]))
        for e in _events_of(trace, "scope_boundary")
    ]


@pytest.fixture
def trace_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    p = tmp_path / "trace.hindsight"
    monkeypatch.setenv("HINDSIGHT_OUTPUT_PATH", str(p))
    # Clear any inherited config so each test starts from defaults.
    monkeypatch.delenv("HINDSIGHT_CONFIG", raising=False)
    return p


def _set_config(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    include: list[str] | None = None,
    exclude: list[str] | None = None,
    depth_limit: int | None = None,
) -> Path:
    """Write a hindsight.toml in ``tmp_path`` and point HINDSIGHT_CONFIG
    at it. Returns the toml path."""
    cfg = tmp_path / "hindsight.toml"
    body = "[scope]\n"
    if include is not None:
        body += f"include = {include!r}\n"
    if exclude is not None:
        body += f"exclude = {exclude!r}\n"
    if depth_limit is not None:
        body += f"depth_limit = {depth_limit}\n"
    cfg.write_text(body)
    monkeypatch.setenv("HINDSIGHT_CONFIG", str(cfg))
    return cfg


# --- Config parser tests ---------------------------------------------------


def test_parse_config_basic(tmp_path: Path):
    cfg = tmp_path / "hindsight.toml"
    cfg.write_text(
        textwrap.dedent(
            """
            [scope]
            include = ["myapp.*"]
            exclude = ["myapp.helpers.*"]
            depth_limit = 3
            """
        )
    )
    result = parse_config(cfg)
    assert result.include == ["myapp.*"]
    assert result.exclude == ["myapp.helpers.*"]
    assert result.depth_limit == 3


def test_parse_config_defaults_token_expands(tmp_path: Path):
    cfg = tmp_path / "hindsight.toml"
    cfg.write_text(
        textwrap.dedent(
            """
            [scope]
            exclude = ["defaults", "myapp.x"]
            """
        )
    )
    result = parse_config(cfg)
    # The "defaults" token expands inline, preserving order. The user's
    # extra pattern comes after.
    assert result.exclude[: len(DEFAULT_EXCLUSIONS)] == DEFAULT_EXCLUSIONS
    assert result.exclude[-1] == "myapp.x"


def test_parse_config_malformed_toml_raises(tmp_path: Path):
    cfg = tmp_path / "hindsight.toml"
    cfg.write_text("[scope\ninvalid")  # missing closing bracket
    with pytest.raises(ValueError, match="TOML parse error"):
        parse_config(cfg)


def test_parse_config_wrong_types_raises(tmp_path: Path):
    cfg = tmp_path / "hindsight.toml"
    cfg.write_text('[scope]\nexclude = ["ok", 42]')
    with pytest.raises(ValueError, match="must be a string"):
        parse_config(cfg)


def test_parse_config_negative_depth_limit_raises(tmp_path: Path):
    cfg = tmp_path / "hindsight.toml"
    cfg.write_text("[scope]\ndepth_limit = -1")
    with pytest.raises(ValueError, match="non-negative"):
        parse_config(cfg)


def test_find_config_env_var_wins(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """HINDSIGHT_CONFIG takes precedence over a hindsight.toml in cwd."""
    cwd_cfg = tmp_path / "hindsight.toml"
    cwd_cfg.write_text("[scope]\nexclude = ['cwd.*']")
    elsewhere_cfg = tmp_path / "elsewhere.toml"
    elsewhere_cfg.write_text("[scope]\nexclude = ['elsewhere.*']")
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("HINDSIGHT_CONFIG", str(elsewhere_cfg))
    found = find_config_path()
    assert found == elsewhere_cfg


def test_find_config_walks_up_to_git_boundary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """Walk-up search stops at the first parent containing ``.git``,
    even if that parent doesn't have a hindsight.toml."""
    project = tmp_path / "project"
    nested = project / "subdir" / "deeper"
    nested.mkdir(parents=True)
    (project / ".git").mkdir()  # mark project root
    # Place a toml at project (will be found from nested via walk-up)
    (project / "hindsight.toml").write_text("[scope]\nexclude = ['x.*']")
    # And another one in tmp_path (above the .git boundary). Walk-up
    # must NOT cross the .git, so this should never be considered.
    (tmp_path / "hindsight.toml").write_text("[scope]\nexclude = ['too_high']")

    monkeypatch.chdir(nested)
    monkeypatch.delenv("HINDSIGHT_CONFIG", raising=False)
    found = find_config_path()
    assert found == project / "hindsight.toml"


def test_find_config_returns_none_when_nothing_found(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    project = tmp_path / "project"
    project.mkdir()
    (project / ".git").mkdir()
    monkeypatch.chdir(project)
    monkeypatch.delenv("HINDSIGHT_CONFIG", raising=False)
    assert find_config_path() is None
    # And load_config returns the empty config in that case.
    cfg = load_config()
    assert cfg.include == []
    assert cfg.exclude == []
    assert cfg.depth_limit is None


def test_scope_config_pattern_matching():
    cfg = ScopeConfig(
        include=["myapp.*"],
        exclude=["myapp.helpers.*", "json.*"],
        depth_limit=None,
    )
    assert cfg.matches_include("myapp.foo")
    assert not cfg.matches_include("other.bar")
    matched, pattern = cfg.matches_exclude("json.dumps")
    assert matched and pattern == "json.*"
    matched, pattern = cfg.matches_exclude("myapp.foo")
    assert not matched and pattern is None


# --- Missing-__name__ behavior --------------------------------------------
#
# When ``frame.f_globals`` lacks ``__name__``, the recorder builds the
# qualified name as ``?.<co_qualname>`` (literal ``?`` prefix). The
# ``?`` carries no glob meaning on the input side of fnmatch — it's
# just a character. These tests lock in the matching behavior across
# the four config shapes so a future refactor can't silently filter
# (or unfilter) these frames.
#
# The behavior is documented in detail on ``_recorder._qualified_name``.


def test_missing_module_qualname_matches_no_default_excludes():
    """Scenario 1: with `exclude=["defaults"]` (the user-supplied
    defaults config), a `?.some_func` frame matches none of the
    default patterns. With an empty include list it falls through to
    "record by default."""
    cfg = ScopeConfig(include=[], exclude=DEFAULT_EXCLUSIONS, depth_limit=None)
    assert not cfg.matches_include("?.some_func")
    matched, pattern = cfg.matches_exclude("?.some_func")
    assert not matched, (
        f"missing-module qualname should not match a default pattern, "
        f"but matched {pattern!r}"
    )


def test_missing_module_qualname_excluded_under_strict_include():
    """Scenario 2: with `include=["myapp.*"]`, `?.some_func` doesn't
    match the include filter. Per the resolution rules in
    `_resolve_mode_for_new_frame`, a non-empty include with no match
    means "out of scope" — the frame is treated as EXCLUDED with
    reason "not in include patterns"."""
    cfg = ScopeConfig(include=["myapp.*"], exclude=[], depth_limit=None)
    assert not cfg.matches_include("?.some_func")


def test_missing_module_qualname_recorded_under_exclude_only():
    """Scenario 3: with `exclude=["numpy.*"]` and no include,
    `?.some_func` doesn't match the exclude pattern, so it falls
    through to recorded-by-default."""
    cfg = ScopeConfig(include=[], exclude=["numpy.*"], depth_limit=None)
    assert not cfg.matches_include("?.some_func")
    matched, _ = cfg.matches_exclude("?.some_func")
    assert not matched


def test_missing_module_qualname_can_be_targeted_explicitly():
    """A user who wants to capture missing-module frames under a strict
    include config can add `?.*` to the include list. This is the
    documented escape hatch."""
    cfg = ScopeConfig(include=["myapp.*", "?.*"], exclude=[], depth_limit=None)
    assert cfg.matches_include("?.some_func")
    assert cfg.matches_include("myapp.foo")
    # Frames with a real module name still don't accidentally match `?.*`
    # — fnmatch's `?` matches exactly one character, and a module name
    # like `test_scope` is more than one character before the dot.
    assert not fnmatch_one_char("?", "test_scope")


def fnmatch_one_char(pat: str, s: str) -> bool:
    """Cross-check helper documenting the fnmatch semantics relied on
    above: pattern ``?`` matches exactly one character."""
    import fnmatch as _fn

    return _fn.fnmatchcase(s, pat)


# --- Runtime scope tests ---------------------------------------------------


def test_no_config_records_everything_by_default(trace_path: Path):
    """With no hindsight.toml the recorder records all transitively-
    called user functions (modulo non-disk-source frames)."""
    calls_two_helpers(5)
    trace = hindsight.read_trace(str(trace_path))

    qualnames = _function_entry_qualnames(trace)
    assert "test_scope.calls_two_helpers" in qualnames
    assert "test_scope.helper_a" in qualnames
    assert "test_scope.helper_b" in qualnames
    # No SCOPE_BOUNDARY events because nothing was excluded.
    assert _boundary_events(trace) == []


def test_include_pattern_filters_to_matching_functions(
    trace_path: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """``include = [...]`` records only matching functions; non-matching
    functions are emitted as SCOPE_BOUNDARY entered_excluded with reason
    ``not in include patterns``."""
    _set_config(
        tmp_path,
        monkeypatch,
        # Match the decorated function itself plus helper_a, but not helper_b.
        include=["test_scope.calls_two_helpers", "test_scope.helper_a"],
    )
    calls_two_helpers(5)
    trace = hindsight.read_trace(str(trace_path))

    qualnames = _function_entry_qualnames(trace)
    assert "test_scope.calls_two_helpers" in qualnames
    assert "test_scope.helper_a" in qualnames
    assert "test_scope.helper_b" not in qualnames

    # helper_b should appear as a SCOPE_BOUNDARY entered_excluded.
    boundaries = _boundary_events(trace)
    enters = [b for b in boundaries if b[0] == 0x03]
    exits = [b for b in boundaries if b[0] == 0x04]
    assert len(enters) == 1 and "not in include patterns" in enters[0][1]
    assert len(exits) == 1


def test_exclude_pattern_emits_boundary_for_excluded_call(
    trace_path: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """``exclude = [...]`` excludes matching functions; we emit a pair
    of SCOPE_BOUNDARY events around their call but no interior events."""
    _set_config(
        tmp_path,
        monkeypatch,
        exclude=["test_scope.helper_b"],
    )
    calls_two_helpers(5)
    trace = hindsight.read_trace(str(trace_path))

    qualnames = _function_entry_qualnames(trace)
    assert "test_scope.helper_a" in qualnames
    assert "test_scope.helper_b" not in qualnames

    boundaries = _boundary_events(trace)
    enters = [b for b in boundaries if b[0] == 0x03]
    exits = [b for b in boundaries if b[0] == 0x04]
    assert len(enters) == 1 and enters[0][1] == "matched pattern: test_scope.helper_b"
    assert len(exits) == 1


def test_defaults_token_excludes_listed_libraries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """The literal ``"defaults"`` token in exclude expands to the
    DEFAULT_EXCLUSIONS list. The resolved ScopeConfig contains every
    pattern from that list."""
    _set_config(tmp_path, monkeypatch, exclude=["defaults"])
    cfg = load_config()
    for pat in DEFAULT_EXCLUSIONS:
        assert pat in cfg.exclude


def test_depth_limit_clips_deep_calls(
    trace_path: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """With ``depth_limit = 2`` the decorated function (depth 0) and
    its first two layers of callees record fully; level_3 onwards is
    DEPTH_CLIPPED."""
    _set_config(tmp_path, monkeypatch, depth_limit=2)
    deep_chain(0)
    trace = hindsight.read_trace(str(trace_path))

    qualnames = _function_entry_qualnames(trace)
    # Recorded: deep_chain(0), level_1(1), level_2(2). Clipped: level_3,
    # level_4 (already in level_3's subtree).
    assert "test_scope.deep_chain" in qualnames
    assert "test_scope.level_1" in qualnames
    assert "test_scope.level_2" in qualnames
    assert "test_scope.level_3" not in qualnames
    assert "test_scope.level_4" not in qualnames

    boundaries = _boundary_events(trace)
    enters = [b for b in boundaries if b[0] == 0x05]
    exits = [b for b in boundaries if b[0] == 0x06]
    # Exactly one entered/exited pair: the level_3 boundary. level_4 is
    # in the SKIPPED_SUBTREE underneath level_3 and emits no events.
    assert len(enters) == 1
    assert len(exits) == 1
    assert "depth limit 2 exceeded" in enters[0][1]


def test_skip_block_local_appears_in_first_post_skip_line_delta(trace_path: Path):
    """**Locks in the documented post-skip semantic.**

    A local assigned inside a ``hindsight.skip()`` block must appear
    with its post-skip value in the first LINE_DELTA *after* the block.
    The block itself emits no events, but recording resumes when the
    block exits and the user wants to see the variables the block
    produced — otherwise they couldn't reason about anything that
    depended on the skip's output.

    Implementation note: this works because the recorder's per-frame
    ``last_value_id_by_local`` map isn't updated while skip_depth > 0,
    so the diff at the next LINE_DELTA picks up the skip-introduced
    local as "new since last seen." A future refactor that
    snapshot-and-restored locals across the skip block would silently
    break this contract — hence this test.

    With ``with_skip_block(5)`` (source layout in this file)::

        before = n + 1                # line 78  → before = 6
        with hindsight.skip():        # line 79  → entered_skip boundary
            skipped = n * 100         # line 80  → suppressed; skipped = 500
                                      #          → exited_skip boundary
        after = n + 2                 # line 81
        return ...                    # line 82

    LINE events fire *before* the line executes, so the first
    LINE_DELTA after ``exited_skip`` corresponds to entering line 81 —
    at which point ``skipped`` is in f_locals (assigned by the skip
    block) but ``after`` hasn't been computed yet. ``after`` shows up
    in the next LINE_DELTA, when execution moves to line 82.

    The contract this test locks in is just the first half: ``skipped``
    appears in the first post-skip LINE_DELTA with value 500.
    """
    with_skip_block(5)
    trace = hindsight.read_trace(str(trace_path))

    events = trace["events"]
    exit_idx = next(
        i
        for i, e in enumerate(events)
        if e["type"] == "scope_boundary" and e["boundary_type"] == 0x02
    )
    first_post_skip = next(
        (e for e in events[exit_idx + 1 :] if e["type"] == "line_delta"),
        None,
    )
    assert first_post_skip is not None, (
        "expected at least one LINE_DELTA after the exited_skip boundary"
    )

    changes = {
        _string_at(trace, name_id): _value_at(trace, value_id)
        for name_id, value_id in first_post_skip["changes"]
    }
    assert "skipped" in changes, (
        f"`skipped` (assigned inside the skip block) should appear in the "
        f"first post-skip LINE_DELTA; got {sorted(changes)!r}"
    )
    assert changes["skipped"] == 500


def test_skip_context_manager_suppresses_inner_events(trace_path: Path):
    """``with hindsight.skip():`` emits an entered_skip / exited_skip
    pair around the block, and no LINE events fire while we're inside
    it.

    Note on what *does* leak: a local assigned inside the skip block
    (``skipped = n * 100``) reappears in the *next* LINE_DELTA after the
    block, because that delta diffs against the pre-block locals. This
    is intentional — the user almost certainly wants to see the
    post-skip state of variables when recording resumes (otherwise they
    couldn't reason about anything that depended on the skip's
    output). The contract is "no events fire *during* the block," not
    "the block leaves no observable trace."
    """
    with_skip_block(5)
    trace = hindsight.read_trace(str(trace_path))

    events = trace["events"]
    boundaries = _boundary_events(trace)
    enters = [b for b in boundaries if b[0] == 0x01]
    exits = [b for b in boundaries if b[0] == 0x02]
    assert len(enters) == 1 and "skip block" in enters[0][1]
    assert len(exits) == 1

    # Find the entered_skip / exited_skip events in order and check
    # that no line_delta event sits between them.
    enter_idx = next(
        i
        for i, e in enumerate(events)
        if e["type"] == "scope_boundary" and e["boundary_type"] == 0x01
    )
    exit_idx = next(
        i
        for i, e in enumerate(events)
        if e["type"] == "scope_boundary" and e["boundary_type"] == 0x02
    )
    inside = events[enter_idx + 1 : exit_idx]
    assert all(e["type"] != "line_delta" for e in inside), (
        "no LINE_DELTA events should fire inside a hindsight.skip() block"
    )

    # Variables assigned before and after the skip block do show up in
    # LINE_DELTAs.
    seen_names: set[str] = set()
    for ev in _events_of(trace, "line_delta"):
        for name_id, _ in ev["changes"]:
            seen_names.add(_string_at(trace, name_id))
    assert "before" in seen_names
    assert "after" in seen_names


def test_nested_skip_blocks_emit_only_outermost_boundary(trace_path: Path):
    """Nested ``hindsight.skip()`` blocks should not double-emit
    boundary events — only the outermost enter/exit fires."""
    with_nested_skip_blocks(5)
    trace = hindsight.read_trace(str(trace_path))

    boundaries = _boundary_events(trace)
    enters = [b for b in boundaries if b[0] == 0x01]
    exits = [b for b in boundaries if b[0] == 0x02]
    assert len(enters) == 1
    assert len(exits) == 1


def test_function_called_inside_skip_is_not_recorded(trace_path: Path):
    """A function called from within a hindsight.skip() block is part
    of the SKIPPED_SUBTREE: no FUNCTION_ENTRY, no SCOPE_BOUNDARY for
    the call itself."""
    calls_function_inside_skip(7)
    trace = hindsight.read_trace(str(trace_path))

    qualnames = _function_entry_qualnames(trace)
    assert "test_scope.calls_function_inside_skip" in qualnames
    assert "test_scope.callee_inside_skip" not in qualnames

    # Only one pair of skip boundaries — the with-block itself. No
    # entered_excluded for callee_inside_skip.
    boundaries = _boundary_events(trace)
    skip_enters = [b for b in boundaries if b[0] == 0x01]
    excluded_enters = [b for b in boundaries if b[0] == 0x03]
    assert len(skip_enters) == 1
    assert len(excluded_enters) == 0


def test_skip_with_no_active_recording_is_noop(tmp_path: Path):
    """Calling hindsight.skip() outside a @record-decorated function
    must not crash and must produce no events."""
    # No trace is being written; this should be a no-op.
    with hindsight.skip():
        x = 42  # noqa: F841


def test_resolved_scope_in_final_summary(trace_path: Path):
    """The final summary block carries recorded_functions,
    excluded_functions, skip_blocks_observed, and depth_clips_observed
    so a reader can attribute the trace's contents."""
    with_skip_block(5)

    trace = hindsight.read_trace(str(trace_path))
    final_toml = trace["final_summary_toml"]
    assert final_toml is not None
    parsed = tomllib.loads(final_toml)

    # ``with_skip_block`` is the only recorded function (it has no
    # callees in scope).
    recorded = parsed["final"]["scope_resolved"]["recorded_functions"]
    assert "test_scope.with_skip_block" in recorded

    # One skip block was entered.
    assert parsed["final"]["scope_resolved"]["skip_blocks_observed"] == 1
    # No depth clips, no excludes in this run.
    assert parsed["final"]["scope_resolved"]["depth_clips_observed"] == 0
    assert parsed["final"]["scope_resolved"]["excluded_functions"] == []


def test_resolved_scope_records_excluded_functions(
    trace_path: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """When an exclude pattern matches, the resolved scope captures
    both the qualified name and the matching pattern."""
    _set_config(tmp_path, monkeypatch, exclude=["test_scope.helper_b"])
    calls_two_helpers(5)
    trace = hindsight.read_trace(str(trace_path))
    parsed = tomllib.loads(trace["final_summary_toml"])

    excluded = parsed["final"]["scope_resolved"]["excluded_functions"]
    assert any(
        e["name"] == "test_scope.helper_b" and e["matched_pattern"] == "test_scope.helper_b"
        for e in excluded
    )


def test_include_match_overrides_exclude_match(
    trace_path: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """Decision (3) from the session brief: explicit include match wins
    over exclude match. Config like ``include=[helper_a], exclude=
    [defaults, helper_a]`` records helper_a despite the exclude."""
    _set_config(
        tmp_path,
        monkeypatch,
        include=["test_scope.calls_two_helpers", "test_scope.helper_a"],
        # helper_a is in include — it should override this exclude.
        exclude=["test_scope.helper_a", "test_scope.helper_b"],
    )
    calls_two_helpers(5)
    trace = hindsight.read_trace(str(trace_path))
    qualnames = _function_entry_qualnames(trace)
    assert "test_scope.helper_a" in qualnames  # include wins
    assert "test_scope.helper_b" not in qualnames  # excluded (not in include)


def test_outer_record_with_no_config_uses_unlimited_depth(trace_path: Path):
    """Sanity: with no hindsight.toml and no env var, depth_limit is
    None and arbitrary depth is recorded."""
    deep_chain(0)
    trace = hindsight.read_trace(str(trace_path))
    qualnames = _function_entry_qualnames(trace)
    for name in (
        "test_scope.deep_chain",
        "test_scope.level_1",
        "test_scope.level_2",
        "test_scope.level_3",
        "test_scope.level_4",
    ):
        assert name in qualnames
    # No depth-clip boundaries.
    assert not any(b[0] == 0x05 for b in _boundary_events(trace))
