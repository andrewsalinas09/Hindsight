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
    "scan_mutation_offsets",
    "MutationOffsets",
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
    """Static analysis result for one code object: which bytecode offsets
    are mutation opcodes, and (where statically determinable) which local
    variable holds the container being mutated.

    Built once per code object at PY_START via ``scan_mutation_offsets``,
    cached on ``_RecorderState.mutation_offsets_by_code``.
    """

    __slots__ = ("offsets", "container_locals")

    def __init__(
        self,
        offsets: set[int],
        container_locals: dict[int, str | None],
    ) -> None:
        # Set of bytecode offsets at which the recorder needs INSTRUCTION
        # events. Other offsets get DISABLE on first fire.
        self.offsets = offsets
        # Map from offset → local name that holds the container being
        # mutated, or None if the static scanner couldn't pin it down
        # (e.g., the container is the result of an expression).
        self.container_locals = container_locals

    def is_mutation_offset(self, offset: int) -> bool:
        return offset in self.offsets

    def container_local_at(self, offset: int) -> str | None:
        return self.container_locals.get(offset)


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

    instructions = list(dis.get_instructions(code))
    for i, inst in enumerate(instructions):
        if inst.opname not in INSTRUCTION_MUTATION_OPCODES:
            continue
        offsets.add(inst.offset)
        # Try to determine the container local for STORE/DELETE_SUBSCR
        # by walking backward through preceding LOAD instructions.
        if inst.opname in ("STORE_SUBSCR", "DELETE_SUBSCR"):
            container_locals[inst.offset] = _container_local_for_subscr(
                instructions, i, inst.opname
            )
        else:
            # Comprehension and bulk-update opcodes: don't try to pin a
            # local. Coarse reconciliation is the v0.3 fallback.
            container_locals[inst.offset] = None
    return MutationOffsets(offsets=offsets, container_locals=container_locals)


def _container_local_for_subscr(
    instructions: list[Any],
    subscr_index: int,
    opname: str,
) -> str | None:
    """For ``STORE_SUBSCR`` at ``instructions[subscr_index]``, find the
    LOAD_FAST that produced the container operand and return its argval
    (the local name).

    Stack shape at STORE_SUBSCR: TOS = key, TOS-1 = container, TOS-2 = value.
    Stack shape at DELETE_SUBSCR: TOS = key, TOS-1 = container.

    Walking backward: skip the key-producing instructions (one
    instruction in the simple case), then skip the container-producing
    instructions, then look at the value-producing instructions.
    Actually that's the wrong order — we want the *container*, which is
    the second pushed. So skip one push (the key), then the next push is
    the container.

    For the simple ``LOAD_FAST x; LOAD_FAST lst; LOAD_FAST i;
    STORE_SUBSCR`` pattern, this picks ``lst``.
    """
    # Walk backward from the STORE_SUBSCR. Skip the key (1 instruction
    # in the simple case), then return the next LOAD_FAST's argval.
    skipped_key = False
    for j in range(subscr_index - 1, max(-1, subscr_index - 8), -1):
        inst = instructions[j]
        # Only consider load-shaped opcodes; anything else (like a CALL
        # or BINARY_OP that produces the key) means we can't statically
        # walk this — bail.
        if inst.opname not in (
            "LOAD_FAST",
            "LOAD_FAST_BORROW",
            "LOAD_FAST_CHECK",
            "LOAD_DEREF",
            "LOAD_NAME",
            "LOAD_GLOBAL",
            "LOAD_CONST",
            "LOAD_ATTR",
        ):
            return None
        if not skipped_key:
            skipped_key = True
            continue
        # This should be the container's load.
        if inst.opname.startswith("LOAD_FAST"):
            return inst.argval
        # Not a simple local — punt.
        return None
    return None


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
