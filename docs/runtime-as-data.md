# Runtime behavior as data

## What this is

Hindsight is described in the project documentation as a debugger. That's true and it's the right framing for users encountering the project for the first time. This document captures something that became clear during early use: **the debugger is one application of a more general capability**, and articulating that capability matters for how the project develops.

The general capability: program execution becomes a structured database that can be queried with SQL. Once execution is data, the accumulated tooling and habits for working with data — querying, aggregating, comparing, hypothesizing — apply to runtime behavior. This has not previously been routine because programs did not produce queryable records of their execution. Now they do.

The implications of this capability are larger than the debugging use case. Performance analysis, correctness verification, structural analysis, comparative analysis across runs, and hypothesis checking all become applications of the same substrate. This document records what's been observed empirically and lays out the broader frame the project sits within.

## The discovery

The project shipped its end-to-end pipeline (recorder → trace format → indexer → DuckDB schema) in a working state and was tested against a recursive Fibonacci program. The test program is deliberately small:

```python
def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

@hindsight.record
def main(target: int) -> int:
    hindsight.note("starting fib", n=target)
    result = fib(target)
    hindsight.note("fib finished", n=target, value=result)
    return result

main(6)
```

Querying the resulting trace surfaced findings that go beyond debugging.

### The redundant work is exactly Fibonacci-shaped

```sql
SELECT
    v.int_value AS n_value,
    COUNT(*) AS times_computed
FROM frames f
JOIN event_args ea ON f.entry_event_id = ea.event_id
JOIN values v ON ea.value_id = v.value_id
WHERE f.qualified_name = '__main__.fib'
  AND ea.position = 0
GROUP BY v.int_value
ORDER BY n_value;
```

Result:

```
n_value │ times_computed
────────┼───────────────
      0 │      5
      1 │      8
      2 │      5
      3 │      3
      4 │      2
      5 │      1
      6 │      1
```

The number of times `fib(k)` was called when computing `fib(6)` follows Fibonacci itself: `1, 1, 2, 3, 5, 8`. This is a known mathematical property of naive recursive Fibonacci that is normally derived from the recurrence. The trace observes it directly, without computation.

### The cost of the redundancy is quantifiable

```sql
WITH fib_calls AS (
    SELECT f.frame_id, f.duration_ns, n.int_value AS n_arg, ret_v.int_value AS return_value
    FROM frames f
    JOIN event_args ea ON f.entry_event_id = ea.event_id AND ea.position = 0
    JOIN values n ON ea.value_id = n.value_id
    LEFT JOIN events exit_e ON f.exit_event_id = exit_e.event_id
    LEFT JOIN values ret_v ON exit_e.return_value_id = ret_v.value_id
    WHERE f.qualified_name = '__main__.fib'
)
SELECT
    n_arg, return_value,
    COUNT(*) AS times_computed,
    SUM(duration_ns) AS total_ns,
    SUM(duration_ns) - MIN(duration_ns) AS could_have_saved_ns
FROM fib_calls
GROUP BY n_arg, return_value
HAVING COUNT(*) > 1
ORDER BY could_have_saved_ns DESC;
```

Result:

```
n_arg │ return │ times │ total_ns │ could_have_saved_ns
──────┼────────┼───────┼──────────┼────────────────────
    2 │      1 │     5 │   165600 │              134500
    3 │      2 │     3 │   183200 │              126000
    4 │      3 │     2 │   242700 │              123000
    1 │      1 │     8 │    51800 │               46000
    0 │      0 │     5 │    31100 │               25200
```

Total potential savings: ~454,700ns out of 483,200ns total runtime. About 94% of the program's execution was redundant work. Memoization would have made the program 16x faster.

This is not a theoretical claim about asymptotic complexity. It is a measurement of a specific run with specific numbers. The conversation about whether memoization is worth it stops being a discussion about code review opinions and becomes a comparison of two numbers.

### Time per call exhibits the recurrence structure

```
n=0: 6220ns avg (5 calls)
n=1: 6475ns avg (8 calls)
n=2: 33120ns avg (5 calls)
n=3: 61067ns avg (3 calls)
n=4: 121350ns avg (2 calls)
n=5: 206100ns avg (1 call)
n=6: 483200ns avg (1 call)
```

Each level roughly doubles average duration, because `fib(n) = fib(n-1) + fib(n-2)` is dominated by `fib(n-1)`. The doubling property of the recurrence shows up directly in the timing data. The trace makes the algorithm's runtime structure observable rather than derived.

### The full call tree is renderable

```sql
WITH RECURSIVE call_tree AS (
    SELECT frame_id, qualified_name, depth, entry_event_id
    FROM frames WHERE parent_frame_id IS NULL
    UNION ALL
    SELECT f.frame_id, f.qualified_name, f.depth, f.entry_event_id
    FROM frames f
    JOIN call_tree ct ON f.parent_frame_id = ct.frame_id
)
SELECT REPEAT('  ', depth) || qualified_name || ...
FROM call_tree ORDER BY entry_event_id;
```

Produces:

```
__main__.main
  __main__.fib(n=6)
    __main__.fib(n=5)
      __main__.fib(n=4)
        __main__.fib(n=3)
          __main__.fib(n=2)
            __main__.fib(n=1)
            __main__.fib(n=0)
          __main__.fib(n=1)
        __main__.fib(n=2)
          __main__.fib(n=1)
          __main__.fib(n=0)
      __main__.fib(n=3)
        __main__.fib(n=2)
          __main__.fib(n=1)
          __main__.fib(n=0)
        __main__.fib(n=1)
    __main__.fib(n=4)
      __main__.fib(n=3)
        __main__.fib(n=2)
          __main__.fib(n=1)
          __main__.fib(n=0)
        __main__.fib(n=1)
      __main__.fib(n=2)
        __main__.fib(n=1)
        __main__.fib(n=0)
```

This is the textbook diagram, generated as a side effect of the trace plus a recursive CTE. The redundant subtrees are visible: `fib(n=4)` appears as its own subtree once, and also inside `fib(n=5)`. `fib(n=3)` appears three times. The asymmetry of the tree (left side deeper than right) reflects the recurrence: `fib(n-1)` always recurses deeper than `fib(n-2)`.

This visualization would take real effort to produce by hand. The trace produces it as a byproduct.

## The framing this implies

Each of the queries above answers a question that was previously hard to ask routinely. To find redundant work in naive Fibonacci, traditionally one would either reason about the recurrence on paper or instrument the code to count calls. The trace makes the redundancy a single SQL query. The same is true of the other queries: timing distributions, call tree structure, branch outcomes by argument value, deepest recursion moments — each is a question that could in principle be answered by other means but rarely was, because the friction was too high.

The pattern: **questions that are individually small but cumulatively expensive to answer become routine when execution is queryable.** The cumulative effect is a different relationship with one's own code. Things one would have shrugged off as "probably fine" become checkable. Curiosity about runtime behavior becomes affordable.

This is the substrate change. The debugger is one application of it. The other applications come for free, in the sense that they are queries against the same database rather than separate tools.

## Applications beyond debugging

Several applications fall out of the substrate without requiring new code:

### Performance analysis

Questions like "which function is slowest on average," "what's the distribution of call durations for this function," "what fraction of total runtime was spent in this subtree" are all SQL aggregations against the `frames` table. The data needed for profiling is already captured. A profiler is a UI on top of this data; the underlying questions are queries.

```sql
-- Functions sorted by total time spent, including time in callees
SELECT qualified_name, COUNT(*) AS calls,
       SUM(duration_ns) AS total_ns, AVG(duration_ns) AS avg_ns
FROM frames
WHERE duration_ns IS NOT NULL
GROUP BY qualified_name
ORDER BY total_ns DESC;
```

### Coverage and hypothesis checking

Questions like "did this code path ever execute," "did variable x ever exceed the expected range," "was this branch ever taken in the false direction" are queries about whether specific events appear in the trace. Coverage tools answer line coverage; the trace answers richer hypothesis questions.

```sql
-- Did this code path with the equality case ever exercise
SELECT COUNT(*) FROM branches b
JOIN event_locals el ON b.event_id - 1 = el.event_id
JOIN values v ON el.value_id = v.value_id
WHERE el.name = 'item' AND v.int_value = 10;
```

### Verification of code claims

Questions like "is this function actually O(n) in this argument," "is this loop running the expected number of iterations," "are these two functions called in the order their contract specifies" are checkable against the trace. Claims about runtime behavior that previously required reasoning about code can be verified empirically.

### Comparison across runs

The schema indexes one trace per database file. To compare two runs, open both databases and run a join query. Bisecting a regression goes from "carefully think about what might have changed" to "diff two traces." Test flakiness investigation goes from "add logging and re-run many times" to "record many runs and find the events that differ."

### Education and code understanding

Recording execution of a textbook algorithm and querying the trace produces a level of detail that the textbook can't provide. Students see exactly what their code did, not just what the textbook says it should do. The Fibonacci redundancy demonstration above is itself an example: the inefficiency of naive recursion becomes a measurement, not a claim.

## What this means for the project

These applications surface as user discoveries when the substrate is good enough. The current project plan (ship the debugger framing first) is correct for adoption: people understand what a debugger is, and they can encounter the broader applications themselves once the tool is in their hands. The discipline is to resist expanding scope into adjacent applications during initial development — the substrate enables them, but the wedge is the debugger.

Operationally, this means:

- v0 ships the debugger. The MCP server's tools are debugging tools. The wedge is "ask questions about a buggy program in natural language."
- v0.1+ adds tools for the common non-debugging questions users surface. Each tool is a SQL pattern that comes up often enough to deserve its own name.
- v0.2+ adds comparison across traces. Two-trace queries become a primitive rather than a manual database-juggling exercise.
- v1+ may add domain-specific framings for adjacent applications (a profiler view, a coverage view) but these sit on the same substrate. They are not separate tools.

The substrate is the moat. The applications are the surface. Build the surface incrementally; preserve the substrate's generality.

## What this means for positioning

The debugger framing is the right starting pitch. "Hindsight is a better debugger" is grokked immediately. The deeper claim — "Hindsight makes runtime behavior queryable" — is true but takes longer to convey and risks sounding abstract.

The recommended trajectory:

- Initial pitch: "AI-native Python debugger. Ask questions about your program's execution in natural language."
- After users have it in their hands: "Hindsight gives you structured access to runtime behavior. Debugging is the obvious application; profiling, verification, and comparison work too."
- After the broader applications take hold: "Hindsight makes runtime behavior data."

Each framing is true. The progression matches user understanding rather than racing ahead to the most general claim.

## The empirical observation

The Fibonacci example above was not constructed to make a point about substrate generality. It was constructed to be a small recursion demo, exercising the recorder's handling of nested calls. The query results revealed properties of the algorithm that go beyond debugging: the redundancy is Fibonacci-shaped, the time-per-call doubles each level, the memoization savings are 94% of total runtime. These were not the questions the demo was designed to answer. They were noticed during exploration.

This is the empirical evidence that the substrate is general. When you query a trace produced by a debugger-focused tool, you get answers to questions outside the debugger's intent. The tool's purpose was debugging; the substrate is broader.

The pattern holds for other example programs in the playground (basic.py reveals an off-by-one bug story; data_processing.py reveals which input items got dropped; exception_demo.py reveals exception propagation across frames). Each was designed to demonstrate a debugging capability. Each, in practice, also demonstrates capabilities outside debugging: timing patterns, structural properties, behavior distributions.

## Closing

The project is on track. The substrate is real. The applications surface as users discover them. The debugger framing is the right wedge, but the underlying capability is more general. Hold both views in mind: ship the debugger; preserve the substrate.

The next major session is the MCP server, which makes the substrate accessible via natural language. After that, the project is shareable. The first users will encounter it as a debugger and may discover it is more. That sequence is correct.

This document exists to record the moment the bigger frame became visible, including the actual data that made it visible. When it's time to write the README, the launch post, or the demo script, draw on this. The numbers are real and the queries are real. The framing does not need to be invented; it needs to be conveyed.
