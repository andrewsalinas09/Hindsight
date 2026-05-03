# SPDX-License-Identifier: Apache-2.0
"""Run a battery of demonstration queries against an indexed demo trace.

Usage::

    python run_demo_queries.py path/to/trace.duckdb

Each section prints the query and its result. The queries are designed to
exercise the schema's design — frame-scoped iteration counts, branch
filters, recursive call-tree traversal, value lookups by content, and so
on.
"""

from __future__ import annotations

import sys
from pathlib import Path

import duckdb


def section(title: str) -> None:
    print()
    print("=" * 78)
    print(title)
    print("=" * 78)


def show(label: str, sql: str, params: tuple = ()) -> None:
    print(f"\n--- {label} ---")
    print(sql.strip())
    rows = duckdb.connect(DB).execute(sql, params).fetchall()
    if not rows:
        print("(no rows)")
        return
    cursor = duckdb.connect(DB).execute(sql, params)
    headers = [d[0] for d in cursor.description]
    cursor.close()
    widths = [
        max(len(str(h)), *(len(str(r[i])) for r in rows)) for i, h in enumerate(headers)
    ]
    print(" | ".join(h.ljust(widths[i]) for i, h in enumerate(headers)))
    print("-+-".join("-" * w for w in widths))
    for r in rows:
        print(" | ".join(str(c).ljust(widths[i]) for i, c in enumerate(r)))


def main() -> None:
    global DB
    if len(sys.argv) != 2:
        print("usage: run_demo_queries.py <trace.duckdb>")
        sys.exit(2)
    DB = sys.argv[1]
    if not Path(DB).exists():
        print(f"error: database not found: {DB}")
        sys.exit(1)

    section("1. Event type distribution")
    show(
        "Event counts by type",
        "SELECT type, COUNT(*) AS n FROM events GROUP BY type ORDER BY n DESC",
    )

    section("2. Function call counts")
    show(
        "Calls per qualified_name (descending)",
        """
        SELECT qualified_name, COUNT(*) AS call_count
        FROM frames
        GROUP BY qualified_name
        ORDER BY call_count DESC
        """,
    )

    section("3. The demo() entry frame")
    show(
        "Frames where the bare function name is 'demo'",
        """
        SELECT frame_id, qualified_name, depth, call_index, exit_kind,
               duration_ns, argument_summary
        FROM frames
        WHERE function_name = 'demo'
        """,
    )

    section("4. Loop-iteration count (frame-scoped)")
    # Find the demo frame, then count line_delta events on the loop line.
    show(
        "demo()'s for-loop body line_delta count",
        """
        WITH demo_frame AS (
            SELECT frame_id, source_file FROM frames WHERE function_name = 'demo'
        )
        SELECT COUNT(*) AS iterations
        FROM events e, demo_frame d
        WHERE e.frame_id = d.frame_id
          AND e.source_file = d.source_file
          AND e.type = 'line_delta'
        """,
    )

    section("5. All branch decisions")
    show(
        "branches table",
        """
        SELECT event_id, function_name, line, taken, timestamp_ns
        FROM branches
        ORDER BY event_id
        """,
    )

    section("6. All notes")
    show(
        "notes table",
        """
        SELECT event_id, function_name, line, message, timestamp_ns
        FROM notes
        ORDER BY event_id
        """,
    )

    section("7. Notes joined to their kwargs and values")
    show(
        "notes JOIN note_kwargs JOIN values",
        """
        SELECT n.message, nk.name AS kwarg_name,
               v.type_tag, v.int_value, v.string_value, v.repr_text
        FROM notes n
        JOIN note_kwargs nk ON n.event_id = nk.event_id
        JOIN values v ON nk.value_id = v.value_id
        ORDER BY n.event_id, nk.name
        """,
    )

    section("8. All raised exceptions")
    show(
        "exceptions table",
        """
        SELECT e.exception_type, e.function_name, e.line,
               v.repr_text AS exception_repr
        FROM exceptions e
        JOIN values v ON e.exception_value_id = v.value_id
        ORDER BY e.event_id
        """,
    )

    section("9. Walk-backward 'find x at event Y' query")
    # Find the latest captured value of `total` in the demo frame.
    show(
        "Most recent value of `total` at the latest event in demo()",
        """
        WITH demo_frame AS (
            SELECT frame_id FROM frames WHERE function_name = 'demo' LIMIT 1
        ),
        latest_event AS (
            SELECT MAX(event_id) AS event_id
            FROM events e, demo_frame d
            WHERE e.frame_id = d.frame_id
        )
        SELECT el.event_id AS at_event,
               v.type_tag, v.int_value, v.string_value
        FROM event_locals el
        JOIN values v ON el.value_id = v.value_id
        JOIN demo_frame d ON el.frame_id = d.frame_id
        JOIN latest_event le ON el.event_id <= le.event_id
        WHERE el.name = 'total'
        ORDER BY el.event_id DESC
        LIMIT 1
        """,
    )

    section("10. Recursive call tree from demo()")
    show(
        "Call tree starting at demo's frame",
        """
        WITH RECURSIVE root AS (
            SELECT frame_id, qualified_name, depth, parent_frame_id
            FROM frames
            WHERE function_name = 'demo'
            LIMIT 1
        ),
        tree AS (
            SELECT frame_id, qualified_name, depth FROM root
            UNION ALL
            SELECT f.frame_id, f.qualified_name, f.depth
            FROM frames f
            JOIN tree t ON f.parent_frame_id = t.frame_id
        )
        SELECT * FROM tree ORDER BY frame_id
        """,
    )

    section("11. Find values of an int = 5")
    show(
        "values where int_value = 5",
        "SELECT value_id, type_tag, int_value FROM values WHERE int_value = 5",
    )

    section("11b. Summary values (summary_length is *not* container_length)")
    show(
        "values rows of type 'summary' with their type_name + summary_length + repr",
        """
        SELECT value_id, type_name, summary_length, repr_text
        FROM values
        WHERE type_tag = 'summary'
        ORDER BY value_id
        """,
    )

    section("12. Function-entry args for the demo() call")
    show(
        "event_args for the demo() entry",
        """
        SELECT ea.position, ea.name,
               v.type_tag, v.int_value, v.string_value, v.repr_text
        FROM event_args ea
        JOIN frames f ON ea.event_id = f.entry_event_id
        JOIN values v ON ea.value_id = v.value_id
        WHERE f.function_name = 'demo'
        ORDER BY ea.position
        """,
    )

    section("13. Trace metadata summary")
    show(
        "trace_metadata row",
        """
        SELECT recorder_language, recorder_version, language_version, platform,
               total_events, function_entry_count, line_event_count,
               branch_event_count, exception_event_count, note_event_count
        FROM trace_metadata
        """,
    )

    section("14. Recorded vs. excluded function lists")
    show("recorded_functions", "SELECT * FROM recorded_functions")
    show("excluded_functions", "SELECT * FROM excluded_functions")


if __name__ == "__main__":
    main()
