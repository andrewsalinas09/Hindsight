# SPDX-License-Identifier: Apache-2.0
"""numpy_internals.py — record into a third-party library on purpose.

This demo *deliberately overrides* the defaults exclusion. We want to
see one level of numpy's own bytecode in the trace, with everything
deeper clipped via ``depth_limit``.

The mechanism is two pieces of the same config:

1. ``include`` is set to ``["__main__.*", "numpy.*"]``. Per
   ``docs/scope-control.md``, an explicit ``include`` match wins over
   any exclude match — even the ``"defaults"`` token's expansion to
   ``numpy.*``. The cost: a non-empty ``include`` list means *anything
   that doesn't match* is excluded too, so we have to list our own user
   code patterns (``__main__.*``) explicitly. That's the documented
   contract — include is a positive filter.

2. ``depth_limit = 1`` caps the recursion at one frame below the
   ``@record`` entry. Our entry point is ``main`` (depth 0). Whatever
   ``main`` calls directly is depth 1 (recorded). Calls *those*
   functions make are depth 2 (DEPTH_CLIPPED). The trace shows one
   level of numpy internals and stops.

Why bother with depth_limit at all? Without it, recording into numpy
generates huge traces (thousands of internal calls). Depth-limited
recording lets you peek inside a library without drowning in detail —
a mode that's useful when you're debugging an integration ("did numpy
call _amax with the right argument?") rather than numpy itself.
"""
from __future__ import annotations

import os
import tempfile
import textwrap

# Pin the trace path before importing hindsight so the recorder picks
# it up on the first @record call.
os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "numpy_internals.hindsight")

# Inline config — independent of any hindsight.toml in the surrounding
# directory tree. Two-pattern include + depth cap.
_cfg_dir = tempfile.mkdtemp(prefix="hindsight_npint_")
_cfg_path = os.path.join(_cfg_dir, "hindsight.toml")
with open(_cfg_path, "w") as f:
    f.write(
        textwrap.dedent(
            """
            [scope]
            include = ["__main__.*", "numpy.*"]
            exclude = []
            depth_limit = 2
            """
        ).strip()
    )
os.environ["HINDSIGHT_CONFIG"] = _cfg_path

import hindsight
import numpy as np


@hindsight.record
def main() -> dict:
    arr = np.array([10.0, 20.0, 30.0, 40.0, 50.0])
    # Each of these is a depth-1 call into numpy. They get recorded;
    # whatever they call internally is depth 2 → DEPTH_CLIPPED.
    return {
        "mean": float(arr.mean()),
        "std": float(arr.std()),
        "max": float(arr.max()),
    }


if __name__ == "__main__":
    print(main())
