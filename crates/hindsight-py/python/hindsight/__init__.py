# SPDX-License-Identifier: Apache-2.0
"""Hindsight: an AI-native debugger for Python programs.

Public surface:

- ``@hindsight.record`` decorates a function. The decorated function is
  recorded — every call, every line, every variable change — and a
  ``.hindsight`` trace file is written when the function returns. See
  ``ARCHITECTURE.md`` and ``docs/scope-control.md`` for the wider product.
- ``with hindsight.skip():`` suspends recording for a block inside a
  ``@record``-decorated function. Useful for excluding hot inner loops
  the user doesn't care to trace.

The lower-level building blocks are also available:

- ``TraceWriter`` and ``read_trace`` — the Rust-backed writer/reader from
  ``hindsight._core``. Most users won't need these directly; they exist for
  testing and for advanced users who want to author traces by hand.
"""

from __future__ import annotations

from ._core import TraceWriter, read_trace
from ._recorder import record, skip

__all__ = ["record", "skip", "TraceWriter", "read_trace"]
