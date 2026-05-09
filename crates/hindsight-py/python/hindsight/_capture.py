# SPDX-License-Identifier: Apache-2.0
"""Container-aware value interning with summary-fingerprint aliasing.

Reduces the recorder's per-capture cost from O(N) (content-hash the whole
container on every line event) to:

- O(1) for re-captures whose summary fingerprint matches the prior capture
- O(k) for re-captures where the container grew by k elements (append-loop
  fast path)
- O(N) only when the fingerprint changes (length differs or endpoint
  identity changed), for first-time captures, and for non-container values

The motivating measurement, the rejected alternatives, and the design
rationale are in ``playground/recorder-overhead-design.md``.

## Confidence

Every alias the recorder emits carries a confidence label per the wire
format spec (``docs/trace-format.md`` §"Confidence semantics"). v0.3 of
this module emits two:

- ``content_exact`` for full intern paths (writer content-hashes the
  whole value, identical to pre-v0.3 behavior).
- ``summary_observed`` for length-and-endpoint-matched aliases. Honest
  signal that *if* the container was mutated in place between captures
  in a way that preserved length and endpoint identity, the trace would
  not show it. Stage 5 of the recorder design (opcode-event mutation
  tracking) upgrades these to ``mutation_tracked`` or
  ``dirty_reconciled`` once it ships.

## Per-frame cache lifecycle

The ``_ContainerCache`` instance lives for one frame's lifetime — the
recorder constructs it at PY_START, populates it on captures, and drops
it at PY_RETURN/PY_UNWIND. CPython's ``id()`` reuse only matters within
that lifetime; if a container is freed and a new one takes the same id
*inside* the frame, the cache hit will produce a wrong alias. v0.3
accepts this rare edge case; opcode tracking (Stage 5) will close it.

## What's *not* optimized in this stage

- Dicts and sets get only the "same length" alias optimization — no
  growth detection (Python doesn't expose key insertion as a single op
  the recorder can detect cheaply). This is the right tradeoff for
  perf.py's list-append pattern.
- Non-container values still go through the full intern_value path. They
  are O(1) to hash anyway (scalars), so the alias path would add overhead
  without benefit.
- Strings, bytes, big ints: full intern. Scalar-shaped from the cache's
  perspective.
"""

from __future__ import annotations

from typing import Any

__all__ = ["ContainerCache", "smart_intern_value"]


class _CacheEntry:
    """One per-frame cache row. Tracks enough about a previously-captured
    container to decide whether a re-capture can be aliased.

    The ``dirty`` flag is set by the opcode-event mutation tracker
    (``_mutation_tracking``) when an INSTRUCTION or CALL event indicates
    this container was just mutated. The next capture sees ``dirty``,
    bypasses the alias path, does a full re-walk, and emits
    ``dirty_reconciled`` confidence — the alias's freshness was
    actively re-verified by reading the live container, not just
    inferred from a fingerprint.
    """

    __slots__ = (
        "value_id",
        "length",
        "last_element_pyid",
        "kind",
        "element_ids",
        "dirty",
    )

    def __init__(
        self,
        value_id: int,
        length: int,
        last_element_pyid: int | None,
        kind: str,
        element_ids: list[int],
    ) -> None:
        self.value_id = value_id
        self.length = length
        # Python id() of the last element at capture time. Used as the
        # endpoint half of the summary fingerprint. None for empty
        # containers and for sets (which don't have a meaningful "last").
        self.last_element_pyid = last_element_pyid
        # "list" | "tuple" | "set" | "frozenset" | "dict"
        self.kind = kind
        # Per-element value_ids the recorder previously interned. Needed
        # so a Grown alias's tail can be re-used: the recorder only
        # interns the *new* tail elements and the writer cross-references
        # the previously-interned prior elements via the alias relation.
        # For dicts these are alternating key, value, key, value... ids.
        self.element_ids = element_ids
        # Set by the mutation tracker. Cleared on next reconciling capture.
        self.dirty = False


class ContainerCache:
    """Per-frame map from Python ``id(obj)`` to a [_CacheEntry].

    Cleared when the owning frame exits. The recorder owns one of these
    per RECORDING-mode frame and threads it through ``smart_intern_value``.
    """

    __slots__ = ("_entries",)

    def __init__(self) -> None:
        self._entries: dict[int, _CacheEntry] = {}

    def lookup(self, py_id: int) -> _CacheEntry | None:
        return self._entries.get(py_id)

    def remember(self, py_id: int, entry: _CacheEntry) -> None:
        self._entries[py_id] = entry

    def forget(self, py_id: int) -> None:
        self._entries.pop(py_id, None)

    def mark_dirty(self, py_id: int) -> None:
        """Called by the mutation tracker when an opcode/CALL event
        indicates the container with this Python id has been mutated.
        The next capture of this container will re-walk fully and emit
        ``dirty_reconciled`` confidence.

        Marking a container we don't have cached is a no-op — the next
        capture will be a fresh first-walk anyway.
        """
        entry = self._entries.get(py_id)
        if entry is not None:
            entry.dirty = True

    def mark_all_dirty(self) -> None:
        """Coarse fallback: a mutation event fired but we couldn't pin
        the affected container statically. Mark every cached container
        dirty. The cost is one re-walk per cached container on the next
        capture pass.
        """
        for entry in self._entries.values():
            entry.dirty = True

    def clear(self) -> None:
        self._entries.clear()

    def __len__(self) -> int:  # for tests
        return len(self._entries)


# Container kinds the alias path supports. Tuples are immutable so the
# alias path on tuples is technically wasted work (a tuple's content
# can't change), but it's correct and rare enough not to special-case.
_LIST_KINDS = ("list", "tuple")
_SET_KINDS = ("set", "frozenset")
_DICT_KINDS = ("dict",)


def _container_kind(obj: Any) -> str | None:
    """Return one of ``"list"|"tuple"|"set"|"frozenset"|"dict"`` for
    container types, or ``None`` for everything else (scalars, strings,
    user objects, etc.). Subclasses are *not* recognized — the alias
    path is opt-in to exact built-in containers, since custom subclasses
    may override mutation semantics in ways the cache can't anticipate."""
    t = type(obj)
    if t is list:
        return "list"
    if t is tuple:
        return "tuple"
    if t is set:
        return "set"
    if t is frozenset:
        return "frozenset"
    if t is dict:
        return "dict"
    return None


def _last_element_pyid(obj: Any, kind: str) -> int | None:
    """The Python id() of the container's "last" element, used as the
    endpoint half of the summary fingerprint. None for empty containers
    and for sets (which have no order)."""
    if kind in _SET_KINDS:
        return None  # No meaningful order.
    try:
        n = len(obj)
    except TypeError:
        return None
    if n == 0:
        return None
    if kind in _LIST_KINDS:
        return id(obj[-1])
    if kind == "dict":
        # CPython 3.7+ dicts preserve insertion order. The last key is the
        # most recently inserted. ``next(reversed(...))`` is O(1).
        try:
            last_key = next(reversed(obj))
        except StopIteration:
            return None
        return id(last_key)
    return None


def smart_intern_value(
    writer: Any,
    cache: ContainerCache | None,
    value: Any,
) -> int | None:
    """Intern ``value``, using ``cache`` to alias re-captures of the same
    container when the summary fingerprint matches.

    Returns the value_id, or None if interning failed (the writer raises
    on user ``__repr__`` errors etc.; we swallow as the recorder did before).

    If ``cache`` is None, falls through to a plain ``writer.intern_value``
    — used by callers (e.g., ``hindsight.note(...)``) that don't have a
    per-frame cache.
    """
    if cache is None:
        return _safe_intern(writer, value)

    kind = _container_kind(value)
    if kind is None:
        return _safe_intern(writer, value)

    py_id = id(value)
    try:
        new_length = len(value)
    except TypeError:
        return _safe_intern(writer, value)
    new_last = _last_element_pyid(value, kind)
    prior = cache.lookup(py_id)

    if prior is not None and prior.kind == kind:
        if prior.dirty:
            # Mutation observed. Two cheap incremental paths first:
            #
            # 1. List/tuple grew at the tail (append, extend, list-comp
            #    accumulation) — emit a Grown alias for just the new
            #    elements with mutation_tracked confidence. O(k) in the
            #    tail length, NOT O(N) in the container size.
            #
            # 2. Otherwise (length unchanged or shrunk, or non-list) —
            #    fall back to a full re-walk and emit dirty_reconciled.
            #    Still correct, just slower.
            if (
                kind in _LIST_KINDS
                and new_length > prior.length
                and _list_prefix_unchanged_likely(value, prior, new_length)
            ):
                return _emit_grown_list_alias(
                    writer,
                    cache,
                    value,
                    py_id,
                    prior,
                    kind,
                    new_length,
                    new_last,
                    confidence="mutation_tracked",
                )
            return _full_intern(
                writer,
                cache,
                value,
                py_id,
                kind,
                new_length,
                new_last,
                confidence_for_full_walk="dirty_reconciled",
            )
        if kind in _LIST_KINDS:
            if new_length == prior.length and new_last == prior.last_element_pyid:
                # Same id, same length, same endpoint identity, cache is
                # clean → the cached value_id is still valid. Return it
                # directly. We deliberately do NOT emit a fresh alias
                # entry here: every LINE event in the loop body would
                # otherwise produce one redundant alias per tracked
                # container, even though nothing changed semantically.
                # The recorder's `_capture_locals_into_changes` will see
                # the same value_id as last time and correctly skip
                # re-recording the local — preserving the schema's
                # "deltas only" model.
                return prior.value_id
            if (
                new_length > prior.length
                and _list_prefix_unchanged_likely(value, prior, new_length)
            ):
                # Append-only growth (or what looks like it from the
                # endpoint check): intern only the new tail and emit a
                # Grown alias.
                return _emit_grown_list_alias(
                    writer, cache, value, py_id, prior, kind, new_length, new_last
                )
            # Length shrunk, or endpoint changed — full re-walk.
        elif kind in _SET_KINDS:
            if new_length == prior.length:
                # Same fingerprint, clean cache → reuse value_id (see
                # the LIST_KINDS branch above for the rationale).
                return prior.value_id
        elif kind == "dict":
            if new_length == prior.length and new_last == prior.last_element_pyid:
                # Same fingerprint, clean cache → reuse value_id.
                return prior.value_id

    # Fall through: first capture of this id, container kind changed
    # (unlikely but possible if id() is reused), or fingerprint mismatch
    # we can't alias. Walk the value fully.
    return _full_intern(writer, cache, value, py_id, kind, new_length, new_last)


def _list_prefix_unchanged_likely(value: Any, prior: _CacheEntry, new_length: int) -> bool:
    """Cheap check: is the prefix [0..prior.length] of ``value`` likely
    the same as last time? We compare ``id(value[prior.length - 1])`` to
    the prior cached last element id. If they match, the prefix's
    last-known element is still in place, which is strong evidence the
    list grew at the tail rather than being mutated mid-prefix.

    Not a guarantee — a same-id replacement at that exact index would
    fool this check. v0.3 accepts that as the documented uncertainty;
    Stage 5 (opcode tracking) eliminates the question by observing
    mutations directly.
    """
    if prior.length == 0:
        return True  # No prefix to check.
    if new_length < prior.length:
        return False
    try:
        return id(value[prior.length - 1]) == prior.last_element_pyid
    except (IndexError, TypeError):
        return False


def _emit_grown_list_alias(
    writer: Any,
    cache: ContainerCache,
    value: Any,
    py_id: int,
    prior: _CacheEntry,
    kind: str,
    new_length: int,
    new_last: int | None,
    confidence: str = "summary_observed",
) -> int | None:
    """Intern only the new tail elements and emit an alias whose source
    is the prior container plus the tail.

    ``confidence`` defaults to ``summary_observed`` for the fingerprint-
    based growth path (we *infer* it was an append from the prefix
    check). Pass ``mutation_tracked`` when the caller has observed the
    mutation directly (e.g., after a CALL event marked the cache
    dirty) — at that point growth detection is no longer an inference.
    """
    new_tail_ids: list[int] = []
    for i in range(prior.length, new_length):
        try:
            elem = value[i]
        except (IndexError, TypeError):
            # Defensive: race-y mutations could change length under us.
            # Fall back to full walk.
            return _full_intern(writer, cache, value, py_id, kind, new_length, new_last)
        elem_id = _safe_intern(writer, elem)
        if elem_id is None:
            return _full_intern(writer, cache, value, py_id, kind, new_length, new_last)
        new_tail_ids.append(elem_id)

    value_id = writer.intern_value_alias_grown(
        prior.value_id, new_tail_ids, confidence
    )
    # Update cache: the alias's effective element_ids are prior + new_tail.
    combined = list(prior.element_ids)
    combined.extend(new_tail_ids)
    cache.remember(
        py_id,
        _CacheEntry(
            value_id=value_id,
            length=new_length,
            last_element_pyid=new_last,
            kind=kind,
            element_ids=combined,
        ),
    )
    return value_id


def _full_intern(
    writer: Any,
    cache: ContainerCache,
    value: Any,
    py_id: int,
    kind: str,
    new_length: int,
    new_last: int | None,
    confidence_for_full_walk: str = "content_exact",
) -> int | None:
    """Walk the container fully via the writer's intern_value path
    (content-hashes the canonical representation), then record the
    capture in the cache so subsequent re-captures can alias.

    For first captures this is the unavoidable O(N). For subsequent
    captures it's hit only when the fingerprint mismatched, which means
    something genuinely changed and re-capturing is the right thing.

    ``confidence_for_full_walk`` controls how the result is reported.
    The default ``content_exact`` is correct for first captures and for
    fingerprint-mismatch re-walks. Pass ``dirty_reconciled`` when the
    re-walk happened because the mutation tracker fired — the recorder
    *re-verified* the contents after observing a mutation, which is a
    stronger signal than a passive content hash. Internally we emit the
    fresh content as a content-hashed entry, then wrap it in an
    Equivalent alias with the requested confidence so the cache and
    downstream tools see the upgraded label.
    """
    # First, intern each child individually so we can record their value_ids
    # for the cache. This is the same work the writer would do internally,
    # but tracking the ids lets a future Grown alias know what's already
    # there.
    fresh_value_id: int | None = None
    child_ids_recorded: list[int] = []

    if kind in _LIST_KINDS:
        child_ids: list[int] = []
        for elem in value:
            cid = smart_intern_value(writer, cache, elem)
            if cid is None:
                # Drop the whole capture if any child fails to intern.
                return None
            child_ids.append(cid)
        try:
            fresh_value_id = writer.intern_value_container_from_children(kind, child_ids)
        except Exception:
            return None
        child_ids_recorded = child_ids
    elif kind in _SET_KINDS:
        child_ids = []
        for elem in value:
            cid = smart_intern_value(writer, cache, elem)
            if cid is None:
                return None
            child_ids.append(cid)
        try:
            fresh_value_id = writer.intern_value_container_from_children(kind, child_ids)
        except Exception:
            return None
        child_ids_recorded = child_ids
    elif kind == "dict":
        # Flat list of alternating key, value ids.
        child_ids = []
        for k, v in value.items():
            kid = smart_intern_value(writer, cache, k)
            vid = smart_intern_value(writer, cache, v)
            if kid is None or vid is None:
                return None
            child_ids.append(kid)
            child_ids.append(vid)
        try:
            fresh_value_id = writer.intern_value_container_from_children("dict", child_ids)
        except Exception:
            return None
        child_ids_recorded = child_ids
    else:
        # Unknown container kind — fall back to plain intern.
        return _safe_intern(writer, value)

    if fresh_value_id is None:
        return None

    # Wrap the fresh content in an alias to upgrade the confidence label
    # when needed. content_exact = use the fresh entry as-is (no extra
    # alias). dirty_reconciled = wrap with an Equivalent alias whose
    # confidence reflects "we re-verified after observing a mutation."
    if confidence_for_full_walk == "content_exact":
        emitted_value_id = fresh_value_id
    else:
        try:
            emitted_value_id = writer.intern_value_alias_equivalent(
                fresh_value_id, confidence_for_full_walk
            )
        except Exception:
            # If the alias emit fails, fall back to the bare content
            # entry — wrong label, but at least the value is captured.
            emitted_value_id = fresh_value_id

    cache.remember(
        py_id,
        _CacheEntry(
            value_id=emitted_value_id,
            length=new_length,
            last_element_pyid=new_last,
            kind=kind,
            element_ids=child_ids_recorded,
        ),
    )
    return emitted_value_id

    return _safe_intern(writer, value)


def _safe_intern(writer: Any, value: Any) -> int | None:
    """Mirror of the recorder's prior ``_safe_intern_value``: swallow
    failures (most often a user ``__repr__`` raising) by returning None
    and letting the caller drop the capture rather than crash the
    program."""
    try:
        return writer.intern_value(value)
    except Exception:
        return None
