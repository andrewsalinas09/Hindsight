# SPDX-License-Identifier: Apache-2.0
"""``hindsight.toml`` discovery and parsing.

Resolution order, exactly as documented in ``docs/scope-control.md``:

1. ``HINDSIGHT_CONFIG`` environment variable, if set, is the file path.
2. ``hindsight.toml`` in the current working directory.
3. Walk up parent directories, stopping at filesystem root or at the
   first parent containing a ``.git`` entry (project boundary).
4. If nothing is found, the recorder runs with no include/exclude
   filtering and unlimited depth — equivalent to an empty ``[scope]``.

The schema for v0 is just three keys under ``[scope]``::

    [scope]
    include = ["myapp.*"]
    exclude = ["defaults", "myapp.helpers.*"]
    depth_limit = 3   # or omit entirely for unlimited

The literal token ``"defaults"`` in ``exclude`` expands to
``DEFAULT_EXCLUSIONS``. Any pattern listed in ``include`` overrides a
later exclude match: include wins over exclude wins over recording-by-
default.

Other ``[capture]``-style sections from ``docs/scope-control.md`` are
deferred to a later session and silently ignored here.
"""

from __future__ import annotations

import fnmatch
import os
import tomllib
from pathlib import Path
from typing import Any

__all__ = [
    "DEFAULT_EXCLUSIONS",
    "ScopeConfig",
    "load_config",
    "find_config_path",
    "parse_config",
]


# Hardcoded for v0. This list is what ``defaults`` expands to. We keep it
# conservative — high-traffic libraries that would otherwise drown a
# trace in third-party callsites the user almost never wants to inspect
# at line level. The architecture doc envisions shipping the list as a
# TOML file inside the package; deferring that until the list is large
# enough to be worth indirecting.
DEFAULT_EXCLUSIONS: list[str] = [
    "numpy.*",
    "pandas.*",
    "torch.*",
    "tensorflow.*",
    "scipy.*",
    "sklearn.*",
    "matplotlib.*",
    "PIL.*",
    "urllib3.*",
    "requests.*",
    "logging.*",
    "asyncio.*",
]


class ScopeConfig:
    """Resolved scope configuration. Pattern lists are post-expansion
    (the ``defaults`` token has already been replaced with the contents
    of :data:`DEFAULT_EXCLUSIONS`)."""

    __slots__ = ("include", "exclude", "depth_limit")

    def __init__(
        self,
        include: list[str] | None = None,
        exclude: list[str] | None = None,
        depth_limit: int | None = None,
    ) -> None:
        self.include = list(include or [])
        self.exclude = list(exclude or [])
        self.depth_limit = depth_limit

    @classmethod
    def empty(cls) -> "ScopeConfig":
        return cls()

    def matches_include(self, qualname: str) -> bool:
        """True iff ``qualname`` matches any include pattern.

        With an empty include list there's no positive include filter,
        so this returns False — the caller falls through to exclude
        handling, where "no exclude match" means "record by default."
        """
        return any(fnmatch.fnmatchcase(qualname, p) for p in self.include)

    def matches_exclude(self, qualname: str) -> tuple[bool, str | None]:
        """Returns ``(True, pattern)`` for the first matching exclude
        pattern, or ``(False, None)`` if no exclude pattern matches."""
        for p in self.exclude:
            if fnmatch.fnmatchcase(qualname, p):
                return True, p
        return False, None

    def __repr__(self) -> str:
        return (
            f"ScopeConfig(include={self.include!r}, "
            f"exclude={self.exclude!r}, depth_limit={self.depth_limit!r})"
        )


def load_config() -> ScopeConfig:
    """Find and parse ``hindsight.toml``. Returns an empty
    :class:`ScopeConfig` if no config file is found."""
    path = find_config_path()
    if path is None:
        return ScopeConfig.empty()
    return parse_config(path)


def find_config_path() -> Path | None:
    """Resolve the path to ``hindsight.toml``, or ``None`` if not found.

    The walk-up loop stops at either the filesystem root or the first
    directory that contains a ``.git`` entry. A directory containing
    ``.git`` is treated as the project boundary even if it doesn't have
    a ``hindsight.toml``: we don't reach above the project just because
    the user forgot to create one. (The same dir's toml *is* honored —
    we check the toml first, then the .git boundary.)
    """
    env = os.environ.get("HINDSIGHT_CONFIG")
    if env:
        # An explicit env var path that doesn't exist is a user error
        # rather than a "no config found" — but the brief doesn't say to
        # raise, so we return the path either way and let the caller
        # observe the error when reading. Empirically pretty rare; if
        # we get bug reports we'll tighten this.
        return Path(env)

    here = Path.cwd().resolve()
    while True:
        candidate = here / "hindsight.toml"
        if candidate.is_file():
            return candidate
        if (here / ".git").exists():
            # Project root reached without a toml — stop walking.
            return None
        parent = here.parent
        if parent == here:
            # Filesystem root.
            return None
        here = parent


def parse_config(path: Path) -> ScopeConfig:
    """Parse ``hindsight.toml`` at ``path``.

    Raises ``ValueError`` (with the path embedded) on any malformed
    input. Per the session-3 decision: silent fallback to defaults
    would hide config bugs, which are by far the most common cause of
    "the trace doesn't have what I expected." Crashing forces the
    user to fix their config.
    """
    try:
        with open(path, "rb") as f:
            data = tomllib.load(f)
    except OSError as e:
        raise ValueError(f"hindsight.toml at {path}: {e}") from e
    except tomllib.TOMLDecodeError as e:
        raise ValueError(f"hindsight.toml at {path}: TOML parse error: {e}") from e

    scope = data.get("scope", {})
    if not isinstance(scope, dict):
        raise ValueError(
            f"hindsight.toml at {path}: [scope] must be a table, got {type(scope).__name__}"
        )

    include_raw = scope.get("include", [])
    exclude_raw = scope.get("exclude", [])
    depth_limit = scope.get("depth_limit")

    include = _coerce_pattern_list(include_raw, key="scope.include", path=path)
    exclude = _expand_defaults(
        _coerce_pattern_list(exclude_raw, key="scope.exclude", path=path)
    )

    if depth_limit is not None and not isinstance(depth_limit, int):
        raise ValueError(
            f"hindsight.toml at {path}: scope.depth_limit must be an integer or "
            f"omitted, got {type(depth_limit).__name__}"
        )
    if isinstance(depth_limit, int) and depth_limit < 0:
        raise ValueError(
            f"hindsight.toml at {path}: scope.depth_limit must be non-negative, got {depth_limit}"
        )

    return ScopeConfig(include=include, exclude=exclude, depth_limit=depth_limit)


def _coerce_pattern_list(value: Any, *, key: str, path: Path) -> list[str]:
    """Validate that ``value`` is a list[str]. The TOML parser already
    rejects most type mismatches, but a stray integer in a list slips
    through (TOML allows mixed-type arrays in some dialects)."""
    if not isinstance(value, list):
        raise ValueError(
            f"hindsight.toml at {path}: {key} must be a list of strings, "
            f"got {type(value).__name__}"
        )
    out: list[str] = []
    for i, p in enumerate(value):
        if not isinstance(p, str):
            raise ValueError(
                f"hindsight.toml at {path}: {key}[{i}] must be a string, "
                f"got {type(p).__name__}"
            )
        out.append(p)
    return out


def _expand_defaults(patterns: list[str]) -> list[str]:
    """Replace each ``"defaults"`` token with the contents of
    :data:`DEFAULT_EXCLUSIONS`, preserving order. Other patterns pass
    through unchanged. Duplicates are *not* deduplicated — fnmatch
    short-circuits on the first match anyway."""
    out: list[str] = []
    for p in patterns:
        if p == "defaults":
            out.extend(DEFAULT_EXCLUSIONS)
        else:
            out.append(p)
    return out
