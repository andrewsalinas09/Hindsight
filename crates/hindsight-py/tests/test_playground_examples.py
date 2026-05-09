# SPDX-License-Identifier: Apache-2.0
"""Regression test for the playground examples.

Each script under ``playground/examples/*.py`` is recorded end-to-end and
its trace is inspected for:

- total event count (recorder's tail message)
- per-(hash_kind, confidence) value-table breakdown

The expected counts are pinned in :data:`EXPECTED` and reflect the v0.4
recorder's behavior. They are *not* magic numbers — they encode where
each example exercises the alias path:

- ``mutation_tracked`` counts: the Patch / Grown alias paths fired.
  These should *not* drop without explanation; if they do, an
  optimization broke or a static-analysis pattern stopped matching.
- ``dirty_reconciled`` counts: full re-walks after observed mutation.
  These should not *grow* without explanation; growth means we lost a
  Patch/Grown opportunity and fell back to coarse reconciliation.
- ``content_exact`` counts: first-time captures and fingerprint-mismatch
  re-walks. These move with the program's actual data.
- ``summary_observed`` counts: same-fingerprint re-captures the recorder
  *can't* prove are unchanged. Should remain very small (only fires
  when no mutation event was observed but the fingerprint matched).

If a count drifts, investigate before bumping the expected value:
- Drift up in ``dirty_reconciled`` ↔ down in ``mutation_tracked``: a
  patch path stopped matching (e.g., an analyzer pattern regressed).
- Drift up in ``summary_observed``: an opcode/CALL hook stopped
  catching a mutation site.
- Drift in ``content_exact``: usually benign — the program itself
  changed, or the per-frame cache lifecycle changed.

To regenerate the table after an intentional change, run:

    pytest crates/hindsight-py/tests/test_playground_examples.py -v -s

then read the actual breakdown from the failure message and update
:data:`EXPECTED`.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path

import pytest

import hindsight  # noqa: F401  — ensures the extension is importable
from hindsight._core import read_trace


REPO_ROOT = Path(__file__).resolve().parents[3]
EXAMPLES_DIR = REPO_ROOT / "playground" / "examples"


# Pinned regression baseline. Keys: example stem. Values: dict of
# ``(hash_kind, confidence)`` → expected count.
EXPECTED: dict[str, dict[tuple[str, str], int]] = {
    "reverse": {
        ("alias", "dirty_reconciled"): 1,
        ("alias", "mutation_tracked"): 100,
        ("content", "content_exact"): 105,
    },
    "quicksort": {
        ("alias", "dirty_reconciled"): 247,
        ("alias", "mutation_tracked"): 1506,
        ("content", "content_exact"): 136,
        ("summary", "summary_observed"): 1,
    },
    "memo_fib": {
        ("alias", "mutation_tracked"): 20,
        ("content", "content_exact"): 55,
    },
    "n_queens": {
        ("alias", "dirty_reconciled"): 1194,
        ("alias", "mutation_tracked"): 181,
        ("content", "content_exact"): 168,
    },
    "merge_sort": {
        ("alias", "dirty_reconciled"): 100,
        ("alias", "mutation_tracked"): 662,
        ("content", "content_exact"): 434,
        ("summary", "summary_observed"): 1,
    },
    "bfs": {
        ("alias", "dirty_reconciled"): 16,
        ("alias", "mutation_tracked"): 8,
        ("content", "content_exact"): 28,
        ("summary", "summary_observed"): 13,
    },
    "dijkstra": {
        ("alias", "dirty_reconciled"): 42,
        ("alias", "mutation_tracked"): 8,
        ("content", "content_exact"): 53,
    },
    "aggregator": {
        ("content", "content_exact"): 16,
        ("summary", "summary_observed"): 1,
    },
    "parser": {
        ("alias", "mutation_tracked"): 7,
        ("content", "content_exact"): 26,
        ("summary", "summary_observed"): 1,
    },
}


HASH_KIND_NAME = {0x01: "content", 0x02: "summary", 0x03: "identity", 0x04: "alias"}


def _classify(value_entry: dict) -> tuple[str, str]:
    """Return ``(hash_kind, confidence)`` for a value table entry as
    ``read_trace`` exposes them. The confidence label for non-alias
    entries is derived from the hash_kind (matches ``derive_confidence``
    in the format crate)."""
    hash_kind = HASH_KIND_NAME.get(value_entry["hash_kind"], "unknown")
    decoded = value_entry["decoded"]
    if isinstance(decoded, dict) and decoded.get("kind") == "alias":
        return (hash_kind, decoded["confidence"])
    if hash_kind == "content":
        return (hash_kind, "content_exact")
    if hash_kind == "summary":
        return (hash_kind, "summary_observed")
    if hash_kind == "identity":
        return (hash_kind, "uncertain_external")
    return (hash_kind, "unknown")


def _run_example(stem: str, tmp_path: Path) -> Path:
    """Run ``playground/examples/<stem>.py`` as a subprocess with
    ``HINDSIGHT_OUTPUT_PATH`` pointing into ``tmp_path``. Returns the
    written trace path."""
    script = EXAMPLES_DIR / f"{stem}.py"
    if not script.exists():
        pytest.skip(f"example {script} missing")
    out_path = tmp_path / f"{stem}.hindsight"
    env = dict(os.environ)
    env["HINDSIGHT_OUTPUT_PATH"] = str(out_path)
    # The example scripts use ``os.environ.setdefault(...)`` — already-set
    # values win, so HINDSIGHT_OUTPUT_PATH from env is honored.
    result = subprocess.run(
        [sys.executable, str(script)],
        env=env,
        capture_output=True,
        text=True,
        check=False,
        timeout=180,
    )
    assert result.returncode == 0, (
        f"{stem} exited {result.returncode}\n"
        f"stdout:\n{result.stdout}\n"
        f"stderr:\n{result.stderr}"
    )
    assert out_path.exists(), f"{stem} did not write trace at {out_path}"
    return out_path


@pytest.mark.parametrize("stem", sorted(EXPECTED))
def test_example_value_breakdown(stem: str, tmp_path: Path) -> None:
    """Each example produces the pinned (hash_kind, confidence) breakdown."""
    trace_path = _run_example(stem, tmp_path)
    trace = read_trace(trace_path)

    actual = Counter(_classify(v) for v in trace["values"])
    expected = Counter(EXPECTED[stem])

    if dict(actual) != dict(expected):
        diff_lines = []
        all_keys = set(actual) | set(expected)
        for key in sorted(all_keys):
            a, e = actual.get(key, 0), expected.get(key, 0)
            if a != e:
                diff_lines.append(f"  {key!s}: expected {e}, got {a}")
        pytest.fail(
            f"{stem}: value-breakdown drift.\n"
            + "\n".join(diff_lines)
            + f"\n\nFull actual:\n  {dict(sorted(actual.items()))}\n"
            f"Full expected:\n  {dict(sorted(expected.items()))}"
        )
