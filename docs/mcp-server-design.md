# MCP server design (v0.1)

This document specifies the Hindsight MCP server: the tools it exposes, what each tool does, the structured outputs each returns, and the principles that guided the design. It is the contract between the server implementation and LLM clients (Claude Desktop, Claude Code, other MCP clients).

## Design principles

The MCP server's job is to make Hindsight feel like a debugger, not a database wrapper. The tools are the verbs of debugging — "trace this variable," "explain this branch," "show me the call tree" — rather than thin wrappers over single SQL queries.

Tools earn their place in one of three ways:

1. **They do something SQL can't.** Source code reading, tree traversal returned as nested data, dependency walking that requires source parsing.
2. **They bundle a multi-query investigation pattern.** What would otherwise be 3-5 chained SQL queries becomes one tool call with a structured result.
3. **They return outputs shaped for narrative use.** Not raw rows, but rows enriched with the context the LLM needs to explain the result to a user.

Tools that are thin wrappers over single SQL queries (`get_event(id)`, `list_functions()`) do not earn their place. The LLM can produce that SQL trivially via `run_sql`. Adding such tools dilutes the surface and obscures which tools are valuable.

The MCP server exposes 11 tools across three groups:

- **Foundational (3):** `describe_schema`, `run_sql`, `get_source` — escape hatches and source access
- **Investigation tools (7):** `trace_variable`, `find_call`, `explain_branch`, `why_did_value_change`, `find_iterations`, `exception_chain`, `get_call_tree` — the debugging verbs
- **Composite source-aware (1):** `causal_slice` — the canonical "operations beyond SQL" tool

The investigation tools are the heart of the server. The foundational tools are the safety net.

## How the LLM should use these tools

The LLM should reach for an investigation tool when the user's question matches its question shape. When the question doesn't fit any tool, the LLM falls back to `run_sql`. When the LLM needs to read source code as part of its reasoning (which is often), it uses `get_source`. When the LLM is uncertain about the schema, it consults `describe_schema`.

Investigation tools return structured output that includes the data plus context the LLM needs to explain the result. The LLM should narrate findings to the user rather than dumping the structured output. The structured output is for LLM consumption; the user-facing answer is prose.

The composite `causal_slice` tool is the most expensive and should be reserved for "why is this value what it is" questions where shallow inspection won't suffice.

## Foundational tools

### `describe_schema()`

**Purpose:** Return the indexed database's schema with prose descriptions of each table and column. Called once at the start of a debugging session to orient the LLM, or whenever the LLM is uncertain about how to query the schema.

**Parameters:** None.

**Returns:**
```json
{
  "tables": [
    {
      "name": "events",
      "description": "Every event in the trace. The central table — most queries touch it.",
      "columns": [
        {"name": "event_id", "type": "BIGINT", "description": "Globally unique, monotonically increasing"},
        {"name": "type", "type": "VARCHAR", "description": "One of: function_entry, function_exit, line_delta, frame_snapshot, branch_result, exception_raised, note, scope_boundary"},
        // ...
      ]
    },
    // ...
  ],
  "common_query_patterns": [
    {
      "name": "Walk-backward variable lookup",
      "sql": "SELECT v.* FROM event_locals el JOIN values v ON el.value_id = v.value_id WHERE el.frame_id = ? AND el.name = ? AND el.event_id <= ? ORDER BY el.event_id DESC LIMIT 1",
      "description": "Find a variable's value at an arbitrary point. event_locals only captures changes; walk back to find the most recent capture."
    },
    // ...
  ]
}
```

**Implementation:** Pull schema from DuckDB's information_schema, augment with prose descriptions stored alongside the server (the server has a static map of table → description and column → description). Include 5-10 common query patterns as examples — these serve as both documentation and few-shot prompts for the LLM.

### `run_sql(query, max_rows=1000)`

**Purpose:** Execute a read-only SQL query against the indexed database. The escape hatch for any question that doesn't fit a typed tool.

**Parameters:**
- `query` (string, required): The SQL query to execute. Must be SELECT-only.
- `max_rows` (int, optional, default 1000): Maximum number of rows to return.

**Returns:**
```json
{
  "columns": ["event_id", "type", "function_name"],
  "rows": [
    [1, "function_entry", "__main__.main"],
    // ...
  ],
  "row_count": 26,
  "truncated": false
}
```

**Implementation:**
- Validate the query is read-only. Reject anything containing INSERT, UPDATE, DELETE, DROP, CREATE, ALTER, ATTACH, or PRAGMA. Use DuckDB's parser if possible; fall back to keyword-based validation.
- Execute with a row limit equal to `max_rows + 1` to detect truncation.
- Return structured rows. Include a `truncated` field so the LLM knows if results were cut off.
- Catch SQL errors and return them in a structured way the LLM can react to.

**Errors:** Return SQL errors as structured responses, not exceptions. The LLM will read the error and retry with corrected SQL.

### `get_source(file_path, line_range=None)`

**Purpose:** Return source code from the trace's source bundle. Used constantly during investigations to understand what code did what.

**Parameters:**
- `file_path` (string, required): The file path as stored in the trace (e.g., `/path/to/myfile.py`).
- `line_range` (tuple of two ints, optional): `[start, end]` 1-indexed inclusive. If omitted, returns the whole file.

**Returns:**
```json
{
  "file_path": "/path/to/myfile.py",
  "content": "def find_largest_below(values, threshold):\n    largest = None\n    for item in values:\n        if item <= threshold:\n            ...",
  "start_line": 30,
  "end_line": 45,
  "total_lines": 67
}
```

**Implementation:**
- Query `source_files` table for the content.
- Slice to the requested line range if specified.
- Return the content with line numbers preserved (so the LLM can correlate with events).

**Errors:** If the file isn't in the trace's source bundle, return a structured "not found" response with available file paths.

## Investigation tools

### `trace_variable(name, frame_id, before_event_id=None)`

**Purpose:** Return the full history of a variable in a frame. Answers "what values did X take during this call?"

**Parameters:**
- `name` (string, required): The variable name.
- `frame_id` (int, required): The frame to scope to.
- `before_event_id` (int, optional): If specified, only returns captures at or before this event. Useful for "what was X up until point Y."

**Returns:**
```json
{
  "name": "largest",
  "frame_id": 0,
  "captures": [
    {
      "event_id": 3,
      "line": 33,
      "source_line": "    largest = None",
      "value": {
        "type_tag": "none",
        "display": "None",
        "value_id": 12
      }
    },
    {
      "event_id": 10,
      "line": 33,
      "source_line": "                largest = item",
      "value": {
        "type_tag": "int",
        "display": "3",
        "value_id": 27
      },
      "context": {
        "item": {"type_tag": "int", "display": "3"}
      }
    },
    // ... continues
  ],
  "total_captures": 5,
  "frame_summary": {
    "qualified_name": "__main__.find_largest_below",
    "argument_summary": "values=[<6 items>], threshold=10"
  }
}
```

**The `context` field is what makes this tool earn its place.** Beyond just listing the variable's values, the tool fetches the source line for each capture and identifies other locals captured at the same event that might be relevant. The LLM gets the narrative timeline ready to convey, not raw rows to interpret.

**Implementation:**
- Query `event_locals` filtered by frame_id and name.
- Join to `values` for the value details.
- Join to `events` for the line number.
- Look up the source line from `source_files`.
- For each capture, fetch other locals captured at the same event_id (filter to a small set: locals with names that appear in the source line, or 1-2 step neighbors). This is the "context" enrichment.
- If `before_event_id` is set, filter captures to event_id <= that.

### `find_call(qualified_name, where=None, limit=10)`

**Purpose:** Find specific function activations by criteria. Answers "which call to X are we talking about?"

**Parameters:**
- `qualified_name` (string, required): The function's qualified name (e.g., `__main__.process_item`).
- `where` (object, optional): Structured filter criteria. Supports:
  - `call_index` (int): Exactly the Nth call (0-indexed)
  - `argument_contains` (string): Substring match against `argument_summary`
  - `raised_exception` (bool): True for frames with exit_kind='raised'
  - `min_duration_ns` (int): Only frames longer than this
  - `parent_qualified_name` (string): Only frames called from this function
- `limit` (int, optional, default 10): Max number of results.

**Returns:**
```json
{
  "matches": [
    {
      "frame_id": 0,
      "call_index": 0,
      "depth": 0,
      "argument_summary": "values=[<6 items>], threshold=10",
      "duration_ns": 251500,
      "exit_kind": "returned",
      "entry_event_id": 0,
      "exit_event_id": 49,
      "parent_frame_id": null
    }
  ],
  "total_matches": 1,
  "qualified_name": "__main__.find_largest_below"
}
```

**Implementation:**
- Build a SQL query against `frames` from the structured filter.
- Return frames in entry_event_id order.
- The structured filter avoids the LLM having to remember argument_summary's exact format or the `<= duration` syntax.

### `explain_branch(event_id)`

**Purpose:** Given a branch event, return everything needed to understand why it went the way it did.

**Parameters:**
- `event_id` (int, required): The event_id of a BRANCH_RESULT event.

**Returns:**
```json
{
  "event_id": 43,
  "frame_id": 0,
  "function_name": "__main__.find_largest_below",
  "line": 37,
  "taken": true,
  "source_context": {
    "lines": [
      {"line": 35, "content": "    for item in values:"},
      {"line": 36, "content": "        # BUG: should be `<`, not `<=`."},
      {"line": 37, "content": "        if item <= threshold:"},
      {"line": 38, "content": "            if largest is None or item > largest:"},
      {"line": 39, "content": "                largest = item"}
    ],
    "branch_line": 37
  },
  "locals_at_branch": {
    "item": {"type_tag": "int", "display": "10", "value_id": 89},
    "threshold": {"type_tag": "int", "display": "10", "value_id": 8},
    "largest": {"type_tag": "int", "display": "9", "value_id": 73}
  },
  "next_events": [
    {"event_id": 44, "type": "line_delta", "line": 38, "summary": "..."},
    {"event_id": 45, "type": "branch_result", "line": 37, "taken": false}
  ]
}
```

**The local values captured at the branch point are the key contribution.** The LLM sees the branch was `taken=true` *and* knows what the relevant variables were at that moment. The source context (a few lines around the branch) lets the LLM understand what condition was being tested.

**Implementation:**
- Look up the branch event in `branches`.
- Fetch source lines from a window around the branch line.
- Walk backward through `event_locals` for the frame to find the most recent value of each local mentioned in the source line. (Parse the source line to extract identifiers; this is a simple regex extraction, not full Python parsing.)
- Fetch a few following events for context.

### `why_did_value_change(name, frame_id, around_event_id)`

**Purpose:** Explain what caused a variable to change at a specific point. Answers "why did X get this value here?"

**Parameters:**
- `name` (string, required): The variable that changed.
- `frame_id` (int, required): The frame.
- `around_event_id` (int, required): An event_id near the change. The tool finds the closest LINE_DELTA where this variable changed at or before this event.

**Returns:**
```json
{
  "name": "largest",
  "frame_id": 0,
  "change_event": {
    "event_id": 48,
    "line": 33,
    "source_line": "                largest = item",
    "previous_value": {"type_tag": "int", "display": "9", "captured_at_event": 33},
    "new_value": {"type_tag": "int", "display": "10"}
  },
  "context_at_change": {
    "item": {"type_tag": "int", "display": "10"},
    "threshold": {"type_tag": "int", "display": "10"},
    "values": {"type_tag": "list", "display": "[<6 items>]"}
  },
  "preceding_branches": [
    {"event_id": 43, "line": 37, "taken": true, "source_line": "        if item <= threshold:"},
    {"event_id": 46, "line": 38, "taken": true, "source_line": "            if largest is None or item > largest:"}
  ],
  "narrative_hint": "The assignment at event 48 happened because the branches at events 43 and 46 both evaluated true. At that point, item=10, threshold=10, and largest=9."
}
```

**This tool composes several queries into the kind of focused answer a debugging investigation produces.** It identifies the change, finds the prior value, fetches the relevant locals at the change point, and includes the branches that allowed the change to happen. The `narrative_hint` is a pre-composed sentence the LLM can use directly or rewrite.

**Implementation:**
- Find the LINE_DELTA event for `name` in `frame_id` at or before `around_event_id`. This is the change_event.
- Find the previous LINE_DELTA for the same name (the previous_value).
- Fetch the source line at the change.
- Fetch the values of other locals at the change_event.
- Find branch_result events in the same frame between the previous LINE_DELTA and the current one. These are the conditions that allowed the change.
- Compose the narrative_hint from the structured data.

### `find_iterations(frame_id, loop_line)`

**Purpose:** Given a frame and the line of a loop, return one row per iteration with the iteration's key state. Answers "what did this loop actually do?"

**Parameters:**
- `frame_id` (int, required): The frame containing the loop.
- `loop_line` (int, required): The source line of the loop header (e.g., the `for x in items:` line).

**Returns:**
```json
{
  "frame_id": 0,
  "loop_line": 35,
  "iteration_count": 6,
  "iterations": [
    {
      "iteration_index": 0,
      "first_event_id": 5,
      "loop_variable": {"name": "item", "value": {"type_tag": "int", "display": "3"}},
      "locals_changed": {
        "largest": {"type_tag": "int", "display": "3", "previous": "None"}
      },
      "branches_taken": [
        {"line": 37, "taken": true},
        {"line": 38, "taken": true}
      ]
    },
    {
      "iteration_index": 1,
      "first_event_id": 12,
      "loop_variable": {"name": "item", "value": {"type_tag": "int", "display": "7"}},
      "locals_changed": {
        "largest": {"type_tag": "int", "display": "7", "previous": "3"}
      },
      "branches_taken": [
        {"line": 37, "taken": true},
        {"line": 38, "taken": true}
      ]
    },
    // ... 4 more iterations
  ]
}
```

**This tool addresses the "12 times across two function calls" problem from the schema design discussion.** Iteration counting is frame-scoped. The output gives the LLM a structured per-iteration view that makes it easy to spot which iteration something went wrong.

**Implementation:**
- Find LINE_DELTA events on `loop_line` in `frame_id` ordered by event_id. Each is the start of an iteration.
- For each iteration, the events between this LINE_DELTA and the next LINE_DELTA on the same line constitute the iteration body.
- Within each iteration, find the loop variable change (the variable assigned at the loop header line — typically detectable as the local with the same line attribution).
- Find other locals that changed within the iteration body.
- Find branch results within the iteration body.

**Edge case:** Loops that exit early (break) or are exited via exception have a final iteration that's incomplete. Handle gracefully — last iteration's body extends to the frame's exit_event_id.

### `exception_chain(event_id)`

**Purpose:** Given an exception raise event, return the propagation chain showing where it was caught (or wasn't).

**Parameters:**
- `event_id` (int, required): The event_id of an EXCEPTION_RAISED event.

**Returns:**
```json
{
  "exception_type": "builtins.ZeroDivisionError",
  "exception_repr": "ZeroDivisionError('division by zero')",
  "raise_event_id": 47,
  "propagation": [
    {
      "frame_id": 2,
      "qualified_name": "__main__.compute",
      "raise_line": 40,
      "exit_kind": "raised",
      "exit_event_id": 49
    },
    {
      "frame_id": 0,
      "qualified_name": "__main__.demo",
      "raise_line": 32,
      "exit_kind": "returned",
      "exit_event_id": 56,
      "caught_at": {
        "line": 33,
        "source": "    except ZeroDivisionError:"
      }
    }
  ],
  "ultimately_caught": true,
  "catching_frame": 0
}
```

**Implementation:**
- Look up the exception_value_id from the raise event.
- Find all EXCEPTION_RAISED events with the same exception_value_id (these are the propagation events as the exception walks the stack).
- For each propagating frame, look up its exit_kind. The first frame with exit_kind='returned' is where the exception was caught.
- Find the source line for the catching frame's recovery (look for the line after the last EXCEPTION_RAISED in that frame, which should be in an except block).

### `get_call_tree(frame_id, max_depth=None, include_args=true)`

**Purpose:** Return the call tree starting from a frame as nested structured data. Answers "what's the call structure here?"

**Parameters:**
- `frame_id` (int, required): The root of the tree.
- `max_depth` (int, optional): Maximum depth from the root. Default unlimited.
- `include_args` (bool, optional, default true): Whether to include argument summaries.

**Returns:**
```json
{
  "frame_id": 0,
  "qualified_name": "__main__.main",
  "argument_summary": "target=6",
  "duration_ns": 583000,
  "exit_kind": "returned",
  "depth": 0,
  "children": [
    {
      "frame_id": 1,
      "qualified_name": "__main__.fib",
      "argument_summary": "n=6",
      "duration_ns": 487000,
      "exit_kind": "returned",
      "depth": 1,
      "children": [
        {
          "frame_id": 2,
          "qualified_name": "__main__.fib",
          "argument_summary": "n=5",
          "depth": 2,
          "children": [
            // ... continues recursively
          ]
        }
      ]
    }
  ]
}
```

**Implementation:**
- Recursive query against `frames` walking parent_frame_id.
- Build the tree structure server-side (DuckDB recursive CTE returns flat rows; the server assembles them into nested JSON).
- If `max_depth` is set, prune the tree at that depth.

## Composite source-aware tool

### `causal_slice(value_id, max_depth=5)`

**Purpose:** Walk backward from a value to find what produced it, recursively up to `max_depth` levels. The canonical "operations beyond SQL" tool — requires source parsing to extract dependencies.

**Parameters:**
- `value_id` (int, required): The value to walk back from.
- `max_depth` (int, optional, default 5): How far back to walk.

**Returns:**
```json
{
  "root_value": {"value_id": 89, "type_tag": "int", "display": "10", "captured_at_event": 48},
  "captured_as": {"name": "largest", "frame_id": 0, "line": 33, "source": "                largest = item"},
  "depends_on": [
    {
      "name": "item",
      "value": {"value_id": 88, "type_tag": "int", "display": "10"},
      "captured_at_event": 42,
      "source_line": "    for item in values:",
      "depends_on": [
        {
          "name": "values",
          "value": {"value_id": 5, "type_tag": "list", "display": "[<6 items>]"},
          "captured_at_event": 0,
          "source_line": "def find_largest_below(values, threshold):",
          "depends_on": [],
          "note": "Function argument"
        }
      ]
    }
  ],
  "depth_reached": 2,
  "truncated": false
}
```

**Implementation:**
- Find the most recent event_locals row that captured this value_id with a name.
- Fetch the source line at that capture.
- Parse the source line to extract identifiers on the right-hand side of the assignment. (For `largest = item`, the dependency is `item`. For `result = a + b * c`, dependencies are `a`, `b`, `c`.)
- For each dependency name, walk backward in event_locals to find its value at or before the capture event.
- Recurse up to max_depth.
- Stop when reaching function arguments, constants, or values not captured in the trace.

**Source parsing:** Use Python's `ast` module to parse the source line and extract `Name` nodes from the right-hand side. This handles most assignments cleanly. For complex expressions (calls, subscripts, attribute access), extract conservatively and note in the result what kind of dependency it is.

**Limitations:** Can't follow dependencies through C extensions, side effects through mutated objects, or values produced by `eval`/`exec`. Note these in the output when detected.

## Implementation notes

### Server framework

Use the `rmcp` Rust crate (current MCP SDK for Rust). Implement as a separate crate `crates/hindsight-mcp` in the workspace.

### Database connection

The server takes the path to an indexed DuckDB database as a startup argument or environment variable. Open it read-only. Hold the connection for the life of the server.

### Tool registration

Each tool is registered with rmcp's tool registration API. Tool descriptions (the `description` field for the LLM) should be specific enough that the LLM picks the right tool from the question alone. Aim for one-sentence descriptions that include the question shape: "Return the full history of a variable in a frame — answers 'what values did X take during this call?'"

### Output formatting

All tools return JSON with consistent shape conventions:

- Times in nanoseconds as integers (`duration_ns`, `timestamp_ns`)
- Values as objects with `type_tag`, `display`, `value_id`, and type-specific fields
- Source lines as objects with `line` (int) and `content` (string)
- Source context as arrays of source line objects
- Frame references include `qualified_name` for human readability when shown alongside `frame_id`

The LLM should not have to guess what fields exist. Use consistent naming.

### Error handling

Errors return a structured response, never an exception:

```json
{
  "error": "frame_not_found",
  "message": "No frame with frame_id=999 exists in this trace.",
  "suggested_action": "Use find_call to locate frames by qualified_name."
}
```

The LLM reads the error and the suggestion, then takes corrective action.

### Performance

For traces with millions of events, the investigation tools should remain interactive (sub-second response). Most of the heavy lifting is single SQL queries with appropriate indexes, which DuckDB handles fast. The exceptions:

- `causal_slice` can be slow for deep walks. Cap `max_depth` defaults at 5.
- `get_call_tree` for huge subtrees could return MB of JSON. Cap depth or row count.
- `find_iterations` for loops with thousands of iterations should paginate or summarize.

When a tool would return more data than is useful, truncate and indicate truncation in the response.

## Tool selection heuristics

To help the LLM pick the right tool, here are the question shapes that map to each:

| User question | Tool |
|---|---|
| "What values did X take?" | `trace_variable` |
| "Which call to X are we looking at?" | `find_call` |
| "Why did this if-statement go this way?" | `explain_branch` |
| "Why did X end up being this value?" | `why_did_value_change` |
| "What did this loop do?" | `find_iterations` |
| "Where did this exception come from?" | `exception_chain` |
| "What's the call structure?" | `get_call_tree` |
| "What produced this value, working backward?" | `causal_slice` |
| "Show me the source code for X" | `get_source` |
| "Anything more complex" | `run_sql` |

The LLM should read the user's question, match it against these shapes, and pick the matching tool. When in doubt, fall back to `run_sql`.

## Demo investigations the server should handle well

To validate the tool design, here are real investigations the server should handle gracefully. Each is a question we've actually asked during development.

### Investigation 1: The off-by-one bug

User asks: "Why did `find_largest_below([3,7,1,9,4,10], 10)` return 10 instead of 9?"

Expected LLM workflow:
1. `find_call("__main__.find_largest_below", where={"argument_contains": "threshold=10"})` → finds frame_id 0
2. `trace_variable("largest", frame_id=0)` → sees largest went None→3→7→9→10
3. `why_did_value_change("largest", frame_id=0, around_event_id=48)` → reveals the change happened when item=10, threshold=10, with branches at lines 37 and 38 both evaluating true
4. `get_source(file, [35, 40])` → reads the source, sees `if item <= threshold:`
5. Narrates: "The `<=` comparison let item=10 through despite threshold=10. Should be `<`."

### Investigation 2: The recursion redundancy

User asks: "Is naive Fibonacci as wasteful as it's supposed to be?"

Expected LLM workflow:
1. `run_sql("SELECT v.int_value AS n, COUNT(*) FROM frames f JOIN event_args ea ON f.entry_event_id = ea.event_id JOIN values v ON ea.value_id = v.value_id WHERE f.qualified_name = '__main__.fib' GROUP BY n ORDER BY n")` → distribution of n values
2. `run_sql("...")` for the redundancy savings query
3. Narrates: "Yes — 94% of runtime is redundant. Specific savings would be 454µs out of 483µs. The pattern follows Fibonacci itself."

(This investigation uses `run_sql` because the questions don't fit typed tools cleanly. That's fine — `run_sql` is the escape hatch.)

### Investigation 3: The exception chain

User asks: "What happened with the ZeroDivisionError?"

Expected LLM workflow:
1. `run_sql("SELECT * FROM exceptions ORDER BY event_id LIMIT 5")` → finds the raise event_ids
2. `exception_chain(first_raise_event_id)` → gets the full propagation
3. Narrates: "ZeroDivisionError raised in compute at line 40. Propagated to demo at line 32, where it was caught by the except clause. demo returned normally."

### Investigation 4: The data processing bug

User asks: "Why is revenue zero in the data_processing example?"

Expected LLM workflow:
1. `find_call("__main__.sum_shipped_revenue")` → finds the frame
2. `trace_variable("revenue", frame_id=...)` → sees revenue stayed at 0.0 the whole time
3. `find_iterations(frame_id, loop_line)` → sees the loop iterated but revenue never changed
4. `get_source(...)` → reads the loop body, sees `order["totals"]` (typo for "total")
5. `run_sql("SELECT * FROM event_locals WHERE name LIKE '%total%'")` → confirms there's no key called "totals" in any captured value
6. Narrates: "The loop runs 4 times but revenue never updates because `order['totals']` is a misspelling of `order['total']`. The dict lookup returns None, the addition fails silently."

These four investigations exercise most of the tool surface and validate that the design supports real debugging workflows.

## What this enables

After this server ships:

- A user with Claude Desktop or Claude Code points it at an indexed trace
- They ask debugging questions in natural language
- Claude picks the right tool, runs it, reads the output, narrates the answer
- The investigation feels like having a senior engineer pair-debug with them, except the senior engineer has perfect recall of every event in the program's execution

This is the threshold where Hindsight stops being a project and starts being a tool. The substrate has been there since the indexer landed; the MCP server is what makes the substrate accessible.

The 11 tools are not the final list. They're v0.1. As real users investigate real bugs, common patterns will surface that deserve their own tools. The `find_call.where` filter will grow new criteria. New investigation tools will join the list. The escape hatch (`run_sql`) ensures nothing is blocked while the typed surface evolves.

But for v0.1, these 11 tools are the right starting point. They cover the question shapes that have come up repeatedly during development. They earn their place by encoding investigation patterns rather than wrapping queries. They return outputs structured for narrative use.

That's what we're building.
