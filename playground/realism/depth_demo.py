# SPDX-License-Identifier: Apache-2.0
"""depth_demo.py — depth_limit clips a deep call chain.

Builds a 6-frame chain (level_0 → level_1 → ... → level_5) and runs it
under a config with ``depth_limit = 3``. The first three levels below
the @record entry should be RECORDED; deeper frames should turn into
DEPTH_CLIPPED scope boundaries.

We write a temp ``hindsight.toml`` and point ``HINDSIGHT_CONFIG`` at it
so this run is independent of any toml in the surrounding directory.
"""
from __future__ import annotations

import os
import tempfile
import textwrap

# Build a config tailored for this demo.
_cfg_dir = tempfile.mkdtemp(prefix="hindsight_depth_")
_cfg_path = os.path.join(_cfg_dir, "hindsight.toml")
with open(_cfg_path, "w") as f:
    f.write(
        textwrap.dedent(
            """
            [scope]
            include = []
            exclude = []
            depth_limit = 3
            """
        ).strip()
    )
os.environ["HINDSIGHT_CONFIG"] = _cfg_path
os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "depth_demo.hindsight")

import hindsight


def level_5(n: int) -> int:
    return n + 1


def level_4(n: int) -> int:
    return level_5(n) * 2


def level_3(n: int) -> int:
    return level_4(n) + 3


def level_2(n: int) -> int:
    return level_3(n) - 1


def level_1(n: int) -> int:
    return level_2(n)


@hindsight.record
def level_0(n: int) -> int:
    return level_1(n)


if __name__ == "__main__":
    print("level_0(10) =", level_0(10))
