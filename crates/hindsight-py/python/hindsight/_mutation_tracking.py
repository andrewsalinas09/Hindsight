# SPDX-License-Identifier: Apache-2.0
"""Opcode-event mutation tracking for the recorder's container cache.

Stage 5 of `playground/recorder-overhead-design.md`. Layered on top of
the summary-fingerprint alias path (`_capture.py`):

- Without this layer, an alias whose summary fingerprint matches is
  marked ``summary_observed`` — honest but uncertain. A same-length
  in-place mutation would hide.
- With this layer, the recorder subscribes to ``sys.monitoring`` events
  for the bytecode opcodes that mutate containers (STORE_SUBSCR,
  LIST_APPEND, etc.) and the method calls that mutate them (`.append`,
  `.sort`, `.update`, ...). When such an event fires on a tracked
  container, the cache marks it dirty. The next capture reconciles by
  doing a full re-walk and tags the result ``dirty_reconciled``.
- Net effect: false-confident aliases become rare. Common cases (append
  loops, in-place index assignment) are correctly tracked. C-extension
  mutations remain in the ``uncertain_external`` bucket — sys.monitoring
  doesn't see them.

## What we subscribe to

CPython's PEP 669 ``sys.monitoring`` exposes the events we need:

- ``INSTRUCTION`` — one fire per bytecode instruction. Expensive when
  global; cheap when scoped per-code-object and locally disabled per-
  offset (the DISABLE-on-first-fire trick).
- ``CALL`` — one fire per Python-level call (including method calls on
  built-in types). Provides the callable and ``arg0`` (the receiver for
  bound methods).

We use ``sys.monitoring.set_local_events`` per code object to enable
events only on functions the recorder actually walks, plus the
DISABLE-on-first-fire trick to keep INSTRUCTION callbacks firing only
at mutation opcodes (not every instruction).

## Mutation opcodes

The opcodes that mutate built-in containers, identified by name across
CPython 3.12+. Offsets vary per code object; the recorder builds a
per-code-object map at install time using ``dis.get_instructions``.

- ``STORE_SUBSCR`` — `obj[key] = value`. Container is the second-from-top
  of the operand stack at the moment of the instruction. The static
  scanner records which local was loaded as the container.
- ``DELETE_SUBSCR`` — `del obj[key]`. Same shape.
- ``LIST_APPEND`` — used inside list comprehensions. Container is on the
  stack; static bytecode tells us which list local.
- ``SET_ADD``, ``MAP_ADD`` — same shape, for set/dict comprehensions.

For mutations that dispatch through a method call (``lst.append(x)``,
``dict.update(...)``, ``lst.sort()``), we use the CALL event. The CALL
callback's ``arg0`` is the receiver — if the receiver is in the cache
and the called method name is on a known-mutating list, mark it dirty.

## Limitations (documented and accepted)

- Object proxies / weird ``__getattr__`` schemes that hide method
  dispatch can evade the CALL-based detection. v0.3 doesn't try to
  handle these — they're rare and the worst-case is the alias is marked
  ``summary_observed`` rather than ``mutation_tracked``.
- C extensions doing in-place mutation via the C API (NumPy, ctypes)
  bypass both the INSTRUCTION and CALL events. Aliases stay
  ``summary_observed``. Future work: explicit per-type hooks if a real
  workload demands them.
- Mid-method-call mutations across nested frames: we mark the container
  dirty on CALL entry. If the called function mutates the container and
  then another frame captures it before the original returns, we still
  catch the mutation because the container is in the parent frame's
  cache marked dirty.
"""

from __future__ import annotations

import dis
import sys
from typing import Any

__all__ = [
    "INSTRUCTION_MUTATION_OPCODES",
    "METHOD_MUTATION_NAMES",
    "MODULE_FUNCTION_MUTATORS",
    "MODULE_FUNCTION_READONLY",
    "is_known_readonly_call",
    "is_known_mutating_call",
    "scan_mutation_offsets",
    "MutationOffsets",
    "install_mutation_events_for_code",
]


# Bytecode opcode names that mutate the *top* of the stack's container
# operand. Set membership is fast; we look up by opname not opcode number
# because opcode numbers shift between CPython versions.
INSTRUCTION_MUTATION_OPCODES: frozenset[str] = frozenset(
    {
        "STORE_SUBSCR",
        "DELETE_SUBSCR",
        "LIST_APPEND",
        "LIST_EXTEND",
        "SET_ADD",
        "SET_UPDATE",
        "MAP_ADD",
        "DICT_MERGE",
        "DICT_UPDATE",
    }
)


# Method names known to mutate the receiver. Conservative allowlist —
# false positives (a re-walk that wasn't strictly needed) are fine; false
# negatives (missed mutations) are not. Add new names as workloads
# surface them.
METHOD_MUTATION_NAMES: frozenset[str] = frozenset(
    {
        # list
        "append",
        "extend",
        "insert",
        "pop",
        "remove",
        "clear",
        "sort",
        "reverse",
        "__setitem__",
        "__delitem__",
        # dict
        "update",
        "popitem",
        "setdefault",
        # set
        "add",
        "discard",
        "intersection_update",
        "difference_update",
        "symmetric_difference_update",
    }
)


class MutationOffsets:
    """Static analysis result for one code object.

    For each STORE_SUBSCR / DELETE_SUBSCR / etc. offset, records:
    - which local variable holds the container being mutated
      (``container_locals``)
    - which local variable holds the subscript key (``key_locals``)

    Both are best-effort: if the bytecode pattern doesn't fit the simple
    ``LOAD_FAST value; LOAD_FAST container; LOAD_FAST key; STORE_SUBSCR``
    shape, the corresponding entry is None and the runtime falls back
    to coarser handling (mark dirty, re-walk on next capture).

    Built once per code object at PY_START via ``scan_mutation_offsets``,
    cached on ``_RecorderState.mutation_offsets_by_code``.
    """

    __slots__ = ("offsets", "container_locals", "key_locals")

    def __init__(
        self,
        offsets: set[int],
        container_locals: dict[int, str | None],
        key_locals: dict[int, str | None],
    ) -> None:
        # Set of bytecode offsets at which the recorder needs INSTRUCTION
        # events. Other offsets get DISABLE on first fire.
        self.offsets = offsets
        # Map from offset → local name that holds the container being
        # mutated, or None if the static scanner couldn't pin it down
        # (e.g., the container is the result of an expression).
        self.container_locals = container_locals
        # Map from offset → local name that holds the subscript key,
        # or None. When both this and the container are known at a
        # STORE_SUBSCR offset, the recorder can emit a Patch alias
        # (O(1) wire-format delta) instead of marking dirty for a full
        # re-walk.
        self.key_locals = key_locals

    def is_mutation_offset(self, offset: int) -> bool:
        return offset in self.offsets

    def container_local_at(self, offset: int) -> str | None:
        return self.container_locals.get(offset)

    def key_local_at(self, offset: int) -> str | None:
        return self.key_locals.get(offset)


def scan_mutation_offsets(code: Any) -> MutationOffsets:
    """Walk ``code``'s bytecode, identify mutation opcodes, and where
    possible determine which named local holds the container being
    mutated.

    For ``STORE_SUBSCR`` (and ``DELETE_SUBSCR``), the container is the
    operand pushed second-to-last before the opcode. The simple pattern
    Python compiles for ``lst[i] = x`` is::

        LOAD_FAST x      # value (for STORE_SUBSCR; absent for DELETE)
        LOAD_FAST lst    # container
        LOAD_FAST i      # key
        STORE_SUBSCR

    So the container's ``LOAD_FAST`` (or ``LOAD_DEREF``, ``LOAD_NAME``,
    ``LOAD_GLOBAL``) is the second-to-last load before the mutation. We
    walk backward from the opcode's offset and record the first
    LOAD_FAST argval we find at the right stack position.

    When the pattern doesn't fit (container is the result of a call,
    attribute access on something nontrivial, etc.), we record None and
    fall through to a coarse "dirty everything in the cache" path at
    runtime.

    For comprehension opcodes (``LIST_APPEND`` etc.) the container is
    typically a stack-local at a known stack depth; pinning it to a
    named local is harder. We record None and reconcile coarsely.
    """
    offsets: set[int] = set()
    container_locals: dict[int, str | None] = {}
    key_locals: dict[int, str | None] = {}

    instructions = list(dis.get_instructions(code))
    for i, inst in enumerate(instructions):
        if inst.opname not in INSTRUCTION_MUTATION_OPCODES:
            continue
        offsets.add(inst.offset)
        if inst.opname in ("STORE_SUBSCR", "DELETE_SUBSCR"):
            container_local, key_local = _subscr_locals(
                instructions, i, inst.opname
            )
            container_locals[inst.offset] = container_local
            key_locals[inst.offset] = key_local
        else:
            # Comprehension and bulk-update opcodes: don't try to pin a
            # local. Coarse reconciliation is the v0.3 fallback.
            container_locals[inst.offset] = None
            key_locals[inst.offset] = None
    return MutationOffsets(
        offsets=offsets,
        container_locals=container_locals,
        key_locals=key_locals,
    )


_LOAD_OPCODES = frozenset(
    {
        "LOAD_FAST",
        "LOAD_FAST_BORROW",
        "LOAD_FAST_CHECK",
        "LOAD_DEREF",
        "LOAD_NAME",
        "LOAD_GLOBAL",
        "LOAD_CONST",
        "LOAD_ATTR",
    }
)

_LOAD_FAST_OPCODES = frozenset(
    {
        "LOAD_FAST",
        "LOAD_FAST_BORROW",
        "LOAD_FAST_CHECK",
    }
)


def _subscr_locals(
    instructions: list[Any],
    subscr_index: int,
    opname: str,
) -> tuple[str | None, str | None]:
    """For STORE_SUBSCR / DELETE_SUBSCR at ``instructions[subscr_index]``,
    walk backward and try to pin BOTH the container local and the key
    local. Returns ``(container_local, key_local)``; either may be None
    if the bytecode pattern doesn't fit.

    Stack shape at STORE_SUBSCR: TOS = key, TOS-1 = container, TOS-2 = value.
    Stack shape at DELETE_SUBSCR: TOS = key, TOS-1 = container.

    Pattern that compiles for the simple case ``lst[i] = x``::

        LOAD_FAST x       # value (absent for DELETE)
        LOAD_FAST lst     # container
        LOAD_FAST i       # key
        STORE_SUBSCR

    The walk: instruction[i-1] is the key load, instruction[i-2] is the
    container load. If either is a simple LOAD_FAST*, we record its
    argval. Anything else (call result, attribute access of an
    expression, BINARY_OP) → None for that slot.
    """
    container_local: str | None = None
    key_local: str | None = None

    # instruction[subscr_index - 1] should be the key push.
    if subscr_index - 1 >= 0:
        key_inst = instructions[subscr_index - 1]
        if key_inst.opname in _LOAD_FAST_OPCODES:
            key_local = key_inst.argval
        # If it's a non-load opcode entirely, the simple shape doesn't
        # apply — but we still try the container slot below in case the
        # key was a small constant (rare; skip for now).

    # instruction[subscr_index - 2] should be the container push.
    if subscr_index - 2 >= 0:
        container_inst = instructions[subscr_index - 2]
        if container_inst.opname in _LOAD_FAST_OPCODES:
            container_local = container_inst.argval
        elif container_inst.opname not in _LOAD_OPCODES:
            # Non-load instruction in the container slot — pattern broken;
            # leave both None to be safe (the key we found above was
            # speculative since the stack shape doesn't match).
            container_local = None
            key_local = None

    return container_local, key_local


def install_mutation_events_for_code(
    tool_id: int,
    code: Any,
    mutation_offsets: MutationOffsets,
) -> None:
    """Enable LOCAL ``INSTRUCTION`` and ``CALL`` events on ``code``.

    The INSTRUCTION callback uses DISABLE-on-first-fire to silence
    non-mutation offsets, so steady-state cost is proportional to
    executed mutation instructions, not all instructions.

    If ``mutation_offsets.offsets`` is empty (no mutation opcodes in
    this code object), we still enable CALL — it's needed for
    method-call mutation detection on built-in types.

    Subscribing per-code-object is required: ``DISABLE`` only works for
    LOCAL events, not GLOBAL ones (PEP 669).
    """
    events = sys.monitoring.events
    mask = events.CALL
    if mutation_offsets.offsets:
        mask |= events.INSTRUCTION
    try:
        sys.monitoring.set_local_events(tool_id, code, mask)
    except (ValueError, RuntimeError):
        # Some code objects (e.g., built-ins, C-implemented frames) don't
        # support local events. Skip silently — those frames just don't
        # get the upgrade from summary_observed to mutation_tracked.
        pass


# ---------------------------------------------------------------------------
# Module-function call classification
# ---------------------------------------------------------------------------
#
# Method-call mutation detection (``METHOD_MUTATION_NAMES``) only catches
# methods dispatched on the receiver itself: ``lst.append(x)``,
# ``dict.update({})``. It does *not* catch the heapq pattern, where the
# container is passed as the *first argument* to a module-level function:
# ``heapq.heappush(lst, x)``. The CALL event fires with ``arg0 = lst`` but
# ``callable_obj.__name__ == "heappush"`` — not in our method allowlist.
#
# Without intervention, heapq.heappush would silently bypass the mutation
# tracker. The trace would report the heap as unchanged across operations
# that demonstrably mutated it. That's the kind of failure we cannot ship.
#
# Two layers of defense, in order of preference:
#
# 1. Explicit module-function allowlist: known mutators (heappush, shuffle,
#    insort) and known readonly (sorted, reversed, len) get classified
#    correctly with no overhead.
# 2. Fallback: any unknown function called with a tracked container as
#    arg0 is conservatively treated as potentially mutating — the
#    container is marked dirty. False positives cause unnecessary
#    re-walks; false negatives cannot occur.

# Functions that mutate their first argument. Each entry is (callable
# qualified name) — we resolve by inspecting ``callable_obj.__module__``
# and ``__qualname__``. Conservative: only listed when documented to mutate.
MODULE_FUNCTION_MUTATORS: frozenset[str] = frozenset(
    {
        # heapq — the entire push/pop API mutates the heap argument.
        "heapq.heappush",
        "heapq.heappop",
        "heapq.heapify",
        "heapq.heappushpop",
        "heapq.heapreplace",
        # random.shuffle — in-place shuffle.
        "random.shuffle",
        # bisect.insort — keeps a sorted list sorted by inserting in place.
        "bisect.insort",
        "bisect.insort_left",
        "bisect.insort_right",
    }
)


# Functions guaranteed to *not* mutate their first argument. Anything not
# in this set or the mutators set is treated as potentially mutating
# (conservative dirty-mark on next capture).
MODULE_FUNCTION_READONLY: frozenset[str] = frozenset(
    {
        # builtins
        "builtins.len",
        "builtins.sum",
        "builtins.min",
        "builtins.max",
        "builtins.any",
        "builtins.all",
        "builtins.sorted",
        "builtins.reversed",
        "builtins.list",
        "builtins.tuple",
        "builtins.set",
        "builtins.frozenset",
        "builtins.dict",
        "builtins.iter",
        "builtins.next",
        "builtins.print",
        "builtins.repr",
        "builtins.str",
        "builtins.hash",
        "builtins.id",
        "builtins.type",
        "builtins.isinstance",
        "builtins.issubclass",
        "builtins.callable",
        "builtins.getattr",
        "builtins.hasattr",
        "builtins.enumerate",
        "builtins.zip",
        "builtins.map",
        "builtins.filter",
        "builtins.range",
        # heapq readonly
        "heapq.nsmallest",
        "heapq.nlargest",
        "heapq.merge",
        # bisect readonly
        "bisect.bisect",
        "bisect.bisect_left",
        "bisect.bisect_right",
        # collections / itertools constructors that don't mutate inputs
        "collections.Counter",
        "collections.OrderedDict",
        "collections.defaultdict",
        # operator helpers
        "operator.itemgetter",
        "operator.attrgetter",
        "operator.methodcaller",
    }
)


def _qualified_callable_name(callable_obj: Any) -> str | None:
    """Return ``"<module>.<qualname>"`` for ``callable_obj``, or None if
    the callable doesn't have stable identification (lambdas, partials,
    etc.). Used to classify CALL events."""
    if callable_obj is None:
        return None
    module = getattr(callable_obj, "__module__", None)
    qualname = getattr(callable_obj, "__qualname__", None)
    if not module or not qualname:
        # Bound methods often have these via __func__. Try one level deeper.
        func = getattr(callable_obj, "__func__", None)
        if func is not None:
            module = getattr(func, "__module__", None) or module
            qualname = getattr(func, "__qualname__", None) or qualname
    if not module or not qualname:
        return None
    return f"{module}.{qualname}"


def is_known_mutating_call(callable_obj: Any) -> bool:
    """True iff ``callable_obj`` is a module-level function known to
    mutate its first argument. Used by the CALL handler to mark dirty
    on heapq.heappush(heap, x) and similar patterns where the container
    is passed as arg0 rather than dispatched as a method."""
    name = _qualified_callable_name(callable_obj)
    return name is not None and name in MODULE_FUNCTION_MUTATORS


def is_known_readonly_call(callable_obj: Any) -> bool:
    """True iff ``callable_obj`` is a module-level function known *not*
    to mutate its first argument. Used to suppress conservative
    dirty-marking for cheap operations like ``len(lst)``."""
    name = _qualified_callable_name(callable_obj)
    return name is not None and name in MODULE_FUNCTION_READONLY
