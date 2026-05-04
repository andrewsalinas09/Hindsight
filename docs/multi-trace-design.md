# Multi-trace mode (v0.2)

## What changes

The MCP server today operates on a single indexed database, configured at startup. This document specifies the v0.2 architecture: the server points at a directory of traces, exposes them all, and supports comparing them.

The core shift: from "one server, one trace" to "one server, many traces." Every investigation tool gains an explicit trace identifier. The server is stateless — no global "currently selected trace" state — which makes correctness easier and enables comparison workflows that single-trace servers can't do at all.

## Why multi-trace matters

Single-trace mode forces a workflow mismatch. Users record code many times — each test run, each debug attempt, each iteration during development. A debugger that handles only the last recording forces them back to startup configuration every time. That friction kills the casual-question workflow that makes Hindsight valuable.

More importantly: **many of the most useful debugging questions are inherently multi-trace.** "Why does this fail in this environment but work in that one?" "What changed between yesterday's run and today's?" "Which iteration of my fix actually fixed the bug?" These are diff questions. Single-trace mode makes them unanswerable; multi-trace mode makes them natural.

The shape of the upgrade is: the server holds a directory of traces, all queryable simultaneously. Each tool call specifies which trace it operates on. New tools support listing and comparison. Existing tools gain a `trace_id` parameter.

## Design principles

A few principles that drive the specifics below:

**Stateless tool calls.** Every tool call is self-contained — it specifies the trace it operates on. No implicit "currently selected" state. The LLM threads `trace_id` through calls; current models do this correctly and the protocol overhead is negligible.

**Filename-based trace identity.** Trace IDs are the filename without extension (`trace_20260503_143022_153467800`). They're stable across server restarts, human-readable in narration, encode the recording timestamp, and don't require additional state.

**Lazy indexing.** Listing traces reads only the .hindsight file headers (no indexing required). Indexing happens on first investigation against an unindexed trace. Stale `.duckdb` files (older than their `.hindsight`) are detected and re-indexed automatically.

**Comparison as a first-class operation.** The server doesn't just allow you to look at two traces in succession; it has tools that operate on pairs of traces directly. Comparison-shaped questions get comparison-shaped answers.

## Architecture changes

### Server side

The server's connection model becomes a registry rather than a single connection.

```
TraceRegistry {
    directory: PathBuf,
    connections: HashMap<TraceId, DuckDB Connection>,  // lazily populated
    metadata_cache: HashMap<TraceId, TraceMetadata>,    // refreshed on listing
}
```

Behavior:

- On startup: scan directory for `.hindsight` files, populate metadata_cache by reading file headers (cheap, no indexing).
- On `list_traces()` call: re-scan directory, refresh metadata_cache, return list.
- On any investigation tool call with `trace_id`: look up connection in registry. If not present, ensure the trace is indexed (run indexer if `.duckdb` missing or stale), open connection, cache it.
- Connections are held open for the life of the server. This is fine for tens of traces; if it becomes a problem at hundreds, add LRU eviction. Not a v0.2 concern.

### Recorder side

The recorder's default output path changes so traces accumulate in a known location:

- If `HINDSIGHT_OUTPUT_PATH` is set, use that (current behavior, unchanged).
- Otherwise, write to `$HINDSIGHT_TRACES_DIR/trace_<timestamp>.hindsight`.
- If `HINDSIGHT_TRACES_DIR` is unset, default to `~/.hindsight/traces/` (create if needed).

This means by default every recorded trace lands in `~/.hindsight/traces/`. The MCP server points at the same directory by default. The user does nothing to wire them together.

### CLI changes

`hindsight serve` accepts three forms of argument, distinguished by what the path points to:

- **No argument or `--dir <path>`:** directory mode. Defaults to `~/.hindsight/traces/`. The server scans the directory for `.hindsight` files and exposes all of them via the multi-trace tool surface. Indexing happens lazily on first access to each trace.

- **A path to a `.hindsight` file:** single-file mode with auto-indexing. The server runs the indexer at startup, producing a `.duckdb` next to the source file, then operates as a single-trace server. The user never thinks about indexing as a step.

- **A path to a `.duckdb` file:** legacy single-file mode. Serves an already-indexed database directly. Backward compatible with current usage.

The detection is by extension. `.hindsight` triggers auto-indexing at startup; `.duckdb` skips it; a directory path triggers directory mode. The single-file modes are useful for sharing or inspecting specific traces; directory mode is the default for daily use.

#### Auto-indexing details for `.hindsight` mode

- The output `.duckdb` is the deterministic sibling: `foo.hindsight` indexes to `foo.duckdb` in the same directory.
- If `foo.duckdb` already exists and its mtime is at least as new as `foo.hindsight`, the existing index is reused and indexing is skipped. The user sees a "skipping reindex" log line.
- If the source directory isn't writable, the indexer falls back to a deterministic temp path of the form `<temp>/hindsight-<stem>-<hash>.duckdb`. The hash is over the canonicalized source path so repeat invocations target the same temp file.
- Indexing is synchronous and runs *before* the MCP server starts listening. If indexing fails (corrupt trace, malformed format, etc.) the process exits with a clear error message naming the failed file. The client never sees a half-broken server.
- Startup logs the path being indexed and the elapsed time once the index is ready (e.g. `indexed in 47ms → trace.duckdb`).

## New and changed tools

### `list_traces()` — new

Returns all traces in the directory with metadata. The first thing Claude calls when the user asks about "the latest trace" or "the trace from earlier."

```json
{
  "directory": "/Users/andrew/.hindsight/traces",
  "traces": [
    {
      "trace_id": "trace_20260503_143022_153467800",
      "recorded_at": "2026-05-03T14:30:22.153467800Z",
      "program": "examples/basic.py",
      "event_count": 159,
      "duration_ns": 482300,
      "indexed": true,
      "size_bytes": 4823,
      "scope_summary": {
        "recorded_function_count": 2,
        "excluded_function_count": 0
      }
    },
    {
      "trace_id": "trace_20260503_142718_891204500",
      "recorded_at": "2026-05-03T14:27:18.891204500Z",
      "program": "examples/recursion.py",
      "event_count": 287,
      "duration_ns": 583100,
      "indexed": false,
      "size_bytes": 8194
    }
  ]
}
```

The metadata is read from each .hindsight file's initial metadata block (no indexing). `indexed: true/false` tells Claude whether the next investigation will incur indexing time.

### `trace_info(trace_id)` — new

Returns detailed info about a single trace, including the resolved scope (which functions were recorded, which were excluded), the recorder version, the platform, etc. This is what Claude calls when the user asks "what's in this trace" or before deep investigation.

```json
{
  "trace_id": "trace_20260503_143022_153467800",
  "recorded_at": "...",
  "program": "examples/basic.py",
  "recorder_language": "python",
  "recorder_version": "0.1.0",
  "language_version": "3.12.7",
  "platform": "win32-AMD64",
  "event_count": 159,
  "duration_ns": 482300,
  "function_entry_count": 2,
  "line_event_count": 26,
  "branch_event_count": 24,
  "exception_event_count": 0,
  "note_event_count": 1,
  "recorded_functions": ["__main__.find_largest_below", "__main__.main"],
  "excluded_functions": [],
  "indexed": true
}
```

### Investigation tools — modified

All 11 existing tools gain a `trace_id` parameter as their first argument. The server uses `trace_id` to look up the appropriate database connection.

```
trace_variable(trace_id, name, frame_id, before_event_id=None)
find_call(trace_id, qualified_name, where=None, limit=10)
explain_branch(trace_id, event_id)
why_did_value_change(trace_id, name, frame_id, around_event_id)
find_iterations(trace_id, frame_id, loop_line)
exception_chain(trace_id, event_id)
get_call_tree(trace_id, frame_id, max_depth=None, include_args=true)
causal_slice(trace_id, value_id, max_depth=5)
get_source(trace_id, file_path, line_range=None)
run_sql(trace_id, query, max_rows=1000)
describe_schema(trace_id=None)   # trace_id optional; schema is the same across traces
```

`describe_schema` is the only exception — the schema is fixed across all traces, so the `trace_id` is optional and primarily used to confirm the trace is accessible.

## Comparison tools — new

These are the v0.2 capability that single-trace mode can't support. Each is a distinct shape of comparison question.

### `compare_traces(trace_id_a, trace_id_b)` — high-level diff

Returns a structural comparison of two traces: differences in event counts, function call patterns, branch outcomes, exceptions raised. The "what's different" overview.

```json
{
  "trace_a": {"trace_id": "...", "program": "examples/basic.py", "event_count": 159},
  "trace_b": {"trace_id": "...", "program": "examples/basic.py", "event_count": 167},
  "summary": {
    "event_count_delta": 8,
    "duration_ns_delta": 12700,
    "function_calls_delta": [
      {"qualified_name": "__main__.find_largest_below", "a_count": 1, "b_count": 1, "delta": 0},
      {"qualified_name": "__main__.helper", "a_count": 0, "b_count": 1, "delta": 1}
    ],
    "exception_delta": [
      {"exception_type": "builtins.ValueError", "a_count": 0, "b_count": 1, "delta": 1}
    ]
  },
  "narrative_hint": "Trace B has 8 more events, an additional call to __main__.helper, and a ValueError that didn't occur in Trace A."
}
```

This is the "what changed" overview. Claude uses it to orient before diving into specifics.

### `find_first_divergence(trace_id_a, trace_id_b)` — execution-level diff

Find the first event where two traces differ in execution. Both traces start at the same entry point; this tool walks them in parallel until they diverge.

```json
{
  "diverged": true,
  "divergence_point": {
    "common_event_count": 47,
    "trace_a_event": {
      "event_id": 47, "type": "branch_result", "line": 37, "taken": true,
      "function_name": "__main__.process"
    },
    "trace_b_event": {
      "event_id": 47, "type": "branch_result", "line": 37, "taken": false,
      "function_name": "__main__.process"
    }
  },
  "context": {
    "shared_history_summary": "Both traces executed the same 47 events identically up to the branch at line 37.",
    "trace_a_locals_at_divergence": {"x": "10", "threshold": "10"},
    "trace_b_locals_at_divergence": {"x": "9", "threshold": "10"},
    "narrative_hint": "Traces diverge at the branch on line 37. In trace A, x=10 made the branch true. In trace B, x=9 made it false."
  }
}
```

This is the "where did things go differently" question. Especially powerful for regression hunting: record a passing run and a failing run, ask `find_first_divergence`, get the exact event where they parted ways.

The implementation walks events in event_id order in both traces, comparing event signatures (type, function, line, key state). When two events differ in a meaningful way, that's the divergence point. "Meaningful" excludes timing differences and event_id mismatches that don't reflect behavior.

### `compare_variable_history(trace_id_a, trace_id_b, name, frame_qualified_name, frame_call_index=0)` — variable diff

For a specific variable in a specific call (matched by qualified name and call index across traces), return the side-by-side history.

```json
{
  "variable": "largest",
  "function": "__main__.find_largest_below",
  "trace_a_history": [
    {"event_id": 3, "value": "None"},
    {"event_id": 10, "value": "3"},
    {"event_id": 18, "value": "7"},
    {"event_id": 33, "value": "9"},
    {"event_id": 48, "value": "10"}
  ],
  "trace_b_history": [
    {"event_id": 3, "value": "None"},
    {"event_id": 10, "value": "3"},
    {"event_id": 18, "value": "7"},
    {"event_id": 33, "value": "9"}
  ],
  "diff_summary": {
    "first_divergence_position": 4,
    "trace_a_diverged_value": "10",
    "trace_b_diverged_value": null,
    "narrative_hint": "Both traces show identical history through value 9. In trace A, largest then became 10 (the bug). In trace B, the loop ended without further updates."
  }
}
```

This is the "did this variable behave differently" question, scoped tightly. Useful for "the same function ran in both traces; did this variable do the same thing in both?"

### `compare_function_calls(trace_id_a, trace_id_b, qualified_name)` — function diff

For a function called in both traces, show all calls and their arguments side-by-side. Useful for "did this function get called with the same inputs in both runs?"

```json
{
  "qualified_name": "__main__.process_item",
  "trace_a_calls": [
    {"call_index": 0, "argument_summary": "item={'id': 1}, factor=2", "duration_ns": 4500, "exit_kind": "returned"},
    {"call_index": 1, "argument_summary": "item={'id': 2}, factor=2", "duration_ns": 4200, "exit_kind": "returned"}
  ],
  "trace_b_calls": [
    {"call_index": 0, "argument_summary": "item={'id': 1}, factor=2", "duration_ns": 4400, "exit_kind": "returned"},
    {"call_index": 1, "argument_summary": "item={'id': 2}, factor=3", "duration_ns": 4100, "exit_kind": "raised"}
  ],
  "diff_summary": {
    "differing_calls": [
      {"call_index": 1, "diff": "factor differs (2 vs 3); exit_kind differs (returned vs raised)"}
    ]
  }
}
```

## Comparison implementation notes

The comparison tools are fundamentally walks over both traces' indexed databases. A few specifics:

**Connection handling for two traces.** Each comparison tool acquires both connections from the registry. The lock pattern is "lock A, lock B, run queries, release both." Risk of deadlock if two simultaneous comparisons request locks in opposite orders — solve by always locking in trace_id alphabetical order.

**Divergence detection in `find_first_divergence`.** Walk events in event_id order in both traces. Compare each pair: if event types match, function names match, lines match, then continue. When they don't, that's divergence. The "context" section includes the locals at that event in both traces (walk-backward queries on both connections).

**Frame matching across traces.** Comparison tools that operate on a "specific function call" need to match calls across traces. Use `(qualified_name, call_index)` as the matching key. The 0th call to `process_item` in trace A corresponds to the 0th call in trace B. This works for deterministic programs; for non-deterministic ones, the call_index is unreliable but it's still a useful approximation.

**Performance.** Comparison over million-event traces can be slow if naively implemented. The first-divergence walk should short-circuit as soon as divergence is found. Comparison summaries should aggregate via SQL (one query per trace, reduced in the server) rather than streaming all events.

**Source files.** Both traces have their own source bundles. If a function's source differs between traces (because the user edited code between runs), the comparison should surface this — `compare_traces` returns a `source_changes` field listing files whose content_hash differs between traces.

## The friction-free workflow

After v0.2 lands, daily usage looks like:

**One-time setup:**

```bash
# Add hindsight to Claude Code's MCP servers, pointing at default directory
claude mcp add hindsight --command hindsight --args serve

# Or, equivalently:
claude mcp add hindsight --command hindsight --args serve --args --dir --args ~/.hindsight/traces
```

**Forever after:**

```python
# In any Python project
import hindsight

@hindsight.record
def my_function():
    ...

if __name__ == "__main__":
    my_function()
```

```bash
python my_program.py
# Trace lands in ~/.hindsight/traces/trace_<timestamp>.hindsight
# Server (already running, attached to Claude) sees the new file
```

```
# In Claude Code:
> Look at the latest trace and explain why my_function returned None.
```

Claude calls `list_traces()`, picks most recent, calls investigation tools with that `trace_id`, narrates the answer.

For comparison:

```
> Compare the latest trace with the one before it. What changed?
```

Claude calls `list_traces()`, picks the two most recent, calls `compare_traces` and `find_first_divergence`, narrates the diff.

Three user actions: configure server (once), run program, ask question. Everything else is automatic.

### Single-file mode for ad-hoc cases

The directory mode above is the default for daily use, but the single-file modes cover scenarios where setting up a traces directory would be friction rather than convenience:

- **A colleague sends you a `.hindsight` file** and you want to query it without copying it into your traces directory or running `hindsight index` first. `hindsight serve received-trace.hindsight` does it in one step — auto-indexes at startup, then serves.
- **You're inspecting a specific trace pinned to a non-standard location** (e.g. attached to a bug report, archived alongside a release). Pointing serve directly at the file avoids polluting the everyday traces directory.
- **An MCP entry pinned to one trace** for a specific debugging investigation, while another entry watches the directory for everything else. Two server entries side-by-side, one stable, one rolling.

In all three cases the LLM's experience is the same — the trace_id is the filename stem, and every tool works as it would in directory mode. Only the discovery surface differs (one trace instead of many).

### Backward compatibility

Existing `hindsight serve <foo.duckdb>` invocations continue to work unchanged. Pre-existing scripts, MCP server configurations, and shell aliases that pass a `.duckdb` path don't break — the server detects the extension, skips the auto-indexing step, and serves the database directly. The only change for `.duckdb` callers is that the LLM now sees the trace addressed by `trace_id` (the filename stem) — but the existing tool-call shape with `trace_id` is straightforward for current models to use, and `describe_schema` still works without a `trace_id` for back-compat exploration.

## What this enables beyond what you've thought of

A few use cases that fall out of multi-trace mode that you might not have explicitly planned for:

**Bisecting regressions.** Record on each commit during development. When a bug appears, ask Claude to find the first trace that exhibits it. Claude can walk through traces in chronological order and check for the bug pattern.

**Comparing test runs.** Record a passing test and a failing test. `find_first_divergence` shows exactly where they diverged, often pinpointing the cause without further investigation.

**Performance regression detection.** Record before and after a refactor. Compare durations across function calls. Flag functions that got slower.

**Flaky test investigation.** Record many runs of a flaky test. Cluster traces by outcome (passed vs. failed). Find the events that consistently differ between groups.

**Production incident analysis.** If you're recording in production (sampled), comparison tools let you ask "what was different about the requests that errored compared to the ones that succeeded."

**Contributor onboarding.** A new contributor records their environment running a test that works in CI but fails locally. They share both traces. Comparison tools surface the environmental difference (Python version, library version, file path) that's causing the divergence.

None of these require new infrastructure beyond what's specified above. They're queries against the same multi-trace registry, asked in different forms. The substrate is general; the use cases compose.

## Implementation scope

The session that builds this is roughly:

1. `TraceRegistry` in the MCP server: directory scanning, metadata caching, lazy connection opening, lazy indexing
2. Recorder default-path change to `~/.hindsight/traces/`
3. CLI accepting `--dir` flag (and defaulting appropriately when unset)
4. `list_traces()` and `trace_info(trace_id)` tools
5. `trace_id` parameter added to all 11 existing tools
6. Three comparison tools: `compare_traces`, `find_first_divergence`, `compare_variable_history`
7. (Optional) `compare_function_calls` if time permits
8. Integration tests for multi-trace operations
9. Updated demo investigations covering at least one comparison flow
10. README updates for the zero-friction workflow

This is a meaningful session. Probably 4-6 hours of agent work. The comparison tools are where the complexity lives — single-trace tool changes are mechanical (add a parameter, route through registry), but `find_first_divergence` requires real algorithm design for the parallel walk.

## What to defer to v0.3

Two things I'd consider for later:

**Cross-trace causal slicing.** "This value in trace A came from a chain that's different than the chain in trace B." Possible but complex; the dependency walk has to be paired across traces. Genuinely useful but not v0.2.

**Trace tagging and search.** Adding labels to traces ("passing", "failing", "release-candidate-3") and querying by them. Easy to add later; not v0.2.

**Live trace following.** A mode where the server watches the directory and notifies Claude when a new trace appears. Mostly nice-to-have; the polling approach (`list_traces()` returns the latest) is functionally equivalent.

## The deeper point

What this design captures is that **once execution is data, the multi-trace operations come for free.** The single-trace MCP server is one configuration of a broader capability. Multi-trace adds the comparison primitives that turn isolated investigations into longitudinal analysis.

This is also where the "execution as data" framing pays off. With execution as data, you can do everything you'd do with two databases: union them, join them, diff them, query across them. The comparison tools are just specific shapes of those operations, packaged for the LLM to use naturally.

The single-trace server makes Hindsight a debugger. The multi-trace server makes Hindsight a debugging environment, where investigations span runs and history is queryable as deeply as individual executions are.

That's what to build next.
