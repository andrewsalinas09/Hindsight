-- common.sql — a battery of useful queries against an indexed Hindsight trace.
--
-- Open one of the .duckdb files produced by `hindsight index`:
--
--     duckdb trace.duckdb
--
-- ...then paste any of these. Each query has a short comment describing
-- what it answers and which schema tables it uses. The schema itself is
-- documented in docs/indexer-schema.md.
--
-- Note on qualified_name: when you run an example as `python examples/foo.py`,
-- the interpreter sets the module's __name__ to "__main__", so functions
-- defined in that script are recorded as "__main__.<func>". Helper modules
-- imported from elsewhere keep their real module path. The queries below
-- use "__main__.<func>" for the entry-point examples; adjust as needed.


-- ============================================================================
-- 1. Event type distribution.
--
-- Quick sanity check: how many of each event type does the trace contain?
-- A high `line_delta` count with no `branch_result` rows means the program
-- ran straight-line code; lots of `function_entry` means lots of calls.
-- ============================================================================
SELECT type, COUNT(*) AS n
FROM events
GROUP BY type
ORDER BY n DESC;


-- ============================================================================
-- 2. Function call counts.
--
-- Which functions ran most? Useful for spotting a hot recursive path
-- or an unexpected number of repeated calls.
-- ============================================================================
SELECT qualified_name, COUNT(*) AS call_count
FROM frames
GROUP BY qualified_name
ORDER BY call_count DESC;


-- ============================================================================
-- 3. Find a specific frame by qualified_name and call_index.
--
-- Replace '__main__.find_largest_below' / 0 with whatever you're
-- looking for. call_index is 0-based: the first call to a function has
-- call_index = 0, the second has 1, and so on.
-- ============================================================================
SELECT frame_id, depth, exit_kind, duration_ns, argument_summary
FROM frames
WHERE qualified_name = '__main__.find_largest_below'
  AND call_index = 0;


-- ============================================================================
-- 4. Loop-iteration count for a specific frame.
--
-- Counts the LINE_DELTA events on a particular source line inside one
-- frame. A typical use: "how many times did the for-loop header at line 33
-- run during this call?" — once per iteration, since LINE fires at the
-- start of each pass through the loop.
-- ============================================================================
SELECT COUNT(*) AS iterations
FROM events
WHERE frame_id = (
        SELECT frame_id FROM frames
        WHERE qualified_name = '__main__.find_largest_below'
          AND call_index = 0
      )
  AND type = 'line_delta'
  AND line = 33;  -- the for-loop header in examples/basic.py


-- ============================================================================
-- 5. All branches at a specific source line, with their decisions.
--
-- Useful for "which way did the if at line 37 (`if largest is None or
-- item > largest:`) go?" — one row per evaluation, with `taken`
-- reflecting the truth value of the condition.
--
-- A real Python quirk: BRANCH events are attributed to the source line
-- of the branching opcode, which the bytecode compiler chooses based
-- on PEP 657 location info. For short-circuit `and`/`or` and some
-- comparison forms, the line a branch reports on can shift one or two
-- lines from the user's mental model. If a line you expect to see is
-- missing, run `SELECT DISTINCT line FROM branches WHERE source_file
-- LIKE '%basic.py' ORDER BY line` to see which lines actually have
-- branch rows, and broaden your filter to a small range.
-- ============================================================================
SELECT event_id, function_name, line, taken, timestamp_ns
FROM branches
WHERE source_file LIKE '%basic.py'
  AND line = 37
ORDER BY event_id;


-- ============================================================================
-- 6. Walk-backward "what was variable X at event Y in frame F".
--
-- The wire format only captures locals when they change. To find a
-- variable's value at an arbitrary event, find the most recent capture
-- AT OR BEFORE that event. Replace ?frame_id and ?event_id with the
-- frame and event you care about.
-- ============================================================================
SELECT el.event_id AS captured_at, v.*
FROM event_locals el
JOIN values v ON el.value_id = v.value_id
WHERE el.frame_id = 0       -- ?frame_id
  AND el.name = 'largest'   -- ?variable
  AND el.event_id <= 100    -- ?event_id (any number ≥ this caps the walk)
ORDER BY el.event_id DESC
LIMIT 1;


-- ============================================================================
-- 7. Recursive CTE for the call tree, starting at one frame.
--
-- DuckDB supports WITH RECURSIVE. The query starts at the frame named
-- in the anchor and follows parent_frame_id back-references to find
-- everything it called transitively. Useful for "show me the subtree
-- under this specific call to fib".
-- ============================================================================
WITH RECURSIVE root AS (
    SELECT frame_id, qualified_name, depth, parent_frame_id
    FROM frames
    WHERE qualified_name = '__main__.main'
    LIMIT 1
),
tree AS (
    SELECT frame_id, qualified_name, depth FROM root
    UNION ALL
    SELECT f.frame_id, f.qualified_name, f.depth
    FROM frames f
    JOIN tree t ON f.parent_frame_id = t.frame_id
)
SELECT * FROM tree ORDER BY frame_id;


-- ============================================================================
-- 8. All exceptions raised, with the exception's repr.
--
-- The `exceptions` table has one row per EXCEPTION_RAISED event. The
-- exception value itself lives in `values` (typically as a Summary
-- with the exception's repr text).
-- ============================================================================
SELECT e.exception_type, e.function_name, e.line, e.timestamp_ns,
       v.repr_text AS exception_repr
FROM exceptions e
JOIN values v ON e.exception_value_id = v.value_id
ORDER BY e.event_id;


-- ============================================================================
-- 9. All notes with kwargs joined to their values.
--
-- `hindsight.note("...", key=value)` from user code. The kwargs are
-- structured key/value pairs you can join to the `values` table to
-- pull out the actual primitive (or summary) the user passed.
-- ============================================================================
SELECT n.event_id, n.message, n.line,
       nk.name AS kwarg_name,
       v.type_tag, v.int_value, v.float_value, v.string_value, v.repr_text
FROM notes n
LEFT JOIN note_kwargs nk ON n.event_id = nk.event_id
LEFT JOIN values v ON nk.value_id = v.value_id
ORDER BY n.event_id, nk.name;


-- ============================================================================
-- 10. Find values by content — e.g., all integers equal to 42.
--
-- The columnar storage makes this fast even on big traces: int_value
-- is its own indexed column. Swap in the predicate you care about
-- (string_value LIKE '%foo%', float_value > 1e6, type_tag = 'list', ...).
-- ============================================================================
SELECT value_id, type_tag, int_value
FROM values
WHERE type_tag = 'int'
  AND int_value = 42;


-- ============================================================================
-- 11. Frames sorted by duration, slowest first.
--
-- "What took the longest?" — duration_ns is exit_timestamp - entry_timestamp.
-- NULL for frames that didn't exit before recording ended.
-- ============================================================================
SELECT qualified_name, call_index, duration_ns, exit_kind
FROM frames
WHERE duration_ns IS NOT NULL
ORDER BY duration_ns DESC
LIMIT 10;


-- ============================================================================
-- 12. Functions called more than N times.
--
-- The N here is 5 — adjust as you like. Common pattern for spotting a
-- hot path you didn't expect.
-- ============================================================================
SELECT qualified_name, COUNT(*) AS call_count
FROM frames
GROUP BY qualified_name
HAVING COUNT(*) > 5
ORDER BY call_count DESC;


-- ============================================================================
-- BONUS: All values of a single variable over time, with their hashes.
--
-- For mutable objects, distinct content_hashes mean the variable was
-- mutated in place. Distinct value_ids mean it was reassigned.
-- ============================================================================
SELECT el.event_id, el.value_id, v.hash_kind, v.hash_hex,
       v.type_tag, v.int_value, v.float_value, v.string_value
FROM event_locals el
JOIN values v ON el.value_id = v.value_id
WHERE el.name = 'revenue'
  AND el.frame_id = (
        SELECT frame_id FROM frames
        WHERE qualified_name = '__main__.sum_shipped_revenue'
        LIMIT 1
      )
ORDER BY el.event_id;
