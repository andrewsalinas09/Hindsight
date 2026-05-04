# SPDX-License-Identifier: Apache-2.0
"""exception_demo.py — exception propagation across a small call chain.

`run` calls `validate_and_parse` which calls `parse_age`. The deepest
function raises ValueError on bad input. The middle function does NOT
catch it — the exception unwinds two frames before `run`'s try/except
recovers.

In the trace this looks like:
- An EXCEPTION_RAISED event at the line in parse_age that raises.
- A FUNCTION_EXIT event for parse_age with the unwind sentinel as the
  return value (frames.exit_kind = 'raised' as a result).
- Same for validate_and_parse.
- A FUNCTION_EXIT for run with a NORMAL return value (frames.exit_kind
  = 'returned'), because `run` caught the exception.

The exceptions table has one row per RAISE; a useful query joins it to
frames to see which exit_kind each propagating frame ended with.
"""

from __future__ import annotations

import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "exception_demo.hindsight")

import hindsight


def parse_age(raw: str) -> int:
    # int() raises ValueError if `raw` isn't a parseable integer. We
    # also enforce a domain check that raises explicitly.
    n = int(raw)
    if n < 0 or n > 200:
        raise ValueError(f"age out of plausible range: {n}")
    return n


def validate_and_parse(record: dict) -> dict:
    # Note we deliberately do NOT wrap parse_age in try/except. We want
    # the exception to propagate up through this frame so the trace
    # shows what unwind looks like.
    name = record.get("name", "<unknown>")
    age = parse_age(record["age"])
    return {"name": name, "age": age}


@hindsight.record
def run(records: list[dict]) -> list[dict]:
    parsed: list[dict] = []
    for record in records:
        try:
            parsed.append(validate_and_parse(record))
        except ValueError as e:
            hindsight.note(
                "skipped invalid record",
                record_id=record.get("id"),
                reason=str(e),
            )
            continue
    return parsed


if __name__ == "__main__":
    inputs = [
        {"id": 1, "name": "Alice", "age": "30"},
        {"id": 2, "name": "Bob", "age": "not-a-number"},   # int() raises
        {"id": 3, "name": "Carol", "age": "250"},          # domain check raises
        {"id": 4, "name": "Dave", "age": "42"},
    ]
    survivors = run(inputs)
    print(f"survived: {len(survivors)} of {len(inputs)}")
    for s in survivors:
        print(f"  {s}")
