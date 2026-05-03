# Indexer schema (v0.1)

This document specifies the DuckDB schema produced by the Hindsight indexer. It is the contract between the indexer and SQL consumers (humans at a SQL prompt, the MCP server's `run_sql` tool, future analysis tools).

## Design principles

The schema optimizes for **query ergonomics over storage efficiency**. DuckDB is columnar — unused columns are nearly free for queries that don't touch them. The cost of denormalization is paid once at indexing time; the benefit is paid every time a question is asked.

Specifically:

- **Names appear directly, not just IDs.** Function names, variable names, file paths, type names — all denormalized into the rows where they're queried. Users shouldn't have to join through a `strings` table for common filters.

- **Frames are first-class.** Every event carries `frame_id`. The `frames` table has one row per function activation with computed fields (depth, call_index, duration_ns, exit_kind) that make frame-scoped queries natural.

- **Event subtypes get their own tables.** Branches, exceptions, notes, and scope boundaries are conceptually distinct from generic events and have different columns of interest. They appear in dedicated tables alongside the generic `events` table.

- **Locals are expanded, not delta-encoded.** Every captured local at every event is a row in `event_locals`. The wire format's delta encoding is a storage optimization; the schema undoes it for query simplicity.

- **Values have type-specific columns.** An int value has an int column populated; a string value has a string column populated. Querying "find values where the int is between 1 and 100" doesn't require parsing encoded bytes.

- **Source files are stored whole.** The full source content is queryable, supporting source-aware tools without extra file I/O.

The schema is **versioned with the indexer**, not with the wire format. As the indexer evolves to better support common query patterns, the schema can change without affecting the wire format. Indexed databases are regenerable from traces; we don't optimize for forward compatibility of the indexed form.

## Top-level tables

### `events`

The central table. One row per event in the trace.

```sql
CREATE TABLE events (
    event_id              BIGINT PRIMARY KEY,
    type                  VARCHAR NOT NULL,        -- 'function_entry', 'function_exit', 'line_delta', 'frame_snapshot', 'branch_result', 'exception_raised', 'note', 'scope_boundary'
    frame_id              BIGINT NOT NULL,
    timestamp_ns          BIGINT NOT NULL,         -- absolute ns since recording start
    source_file           VARCHAR,                 -- denormalized from source_files; NULL if event has no source attribution
    line                  INTEGER,                 -- line number at this event; NULL if not applicable

    -- denormalized convenience fields
    function_name         VARCHAR,                 -- the qualified name of the frame's function (denormalized from frames.qualified_name)

    -- type-specific fields, populated based on type:
    return_value_id       BIGINT,                  -- type='function_exit'
    branch_taken          BOOLEAN,                 -- type='branch_result'
    exception_type        VARCHAR,                 -- type='exception_raised'; qualified type name
    exception_value_id    BIGINT,                  -- type='exception_raised'
    note_message          VARCHAR,                 -- type='note'
    boundary_type         VARCHAR,                 -- type='scope_boundary': 'entered_skip', 'exited_skip', 'entered_excluded', 'exited_excluded', 'entered_depth_clipped', 'exited_depth_clipped'
    boundary_reason       VARCHAR                  -- type='scope_boundary'
);

CREATE INDEX events_frame_id ON events(frame_id);
CREATE INDEX events_type ON events(type);
CREATE INDEX events_function_name ON events(function_name);
CREATE INDEX events_source_line ON events(source_file, line);
CREATE INDEX events_timestamp ON events(timestamp_ns);
```

**Notes:**

- `event_id` is the global monotonic event ID from the wire format. Stable across re-indexing of the same trace.
- `timestamp_ns` is absolute (computed by summing deltas during indexing), not delta from previous event. Querying time ranges works directly.
- `function_name` is denormalized for fast filtering; e.g., `WHERE function_name = 'process_item'` doesn't require a join.
- Type-specific fields are sparse — most rows have NULLs in most type-specific columns. DuckDB handles this efficiently in columnar storage.
- For `function_entry` events, the function's arguments are in a separate `event_args` table (since each entry can have multiple args).
- For `line_delta` and `frame_snapshot` events, captured locals are in `event_locals` (since each event captures multiple locals).
- For `note` events with kwargs, the kwargs are in `note_kwargs`.

### `frames`

One row per function activation. Frames are the natural unit of scoping for most debugging questions.

```sql
CREATE TABLE frames (
    frame_id              BIGINT PRIMARY KEY,
    function_name         VARCHAR NOT NULL,        -- bare function name (e.g., 'process_item')
    qualified_name        VARCHAR NOT NULL,        -- module-qualified (e.g., 'myapp.handlers.process_item')
    source_file           VARCHAR NOT NULL,
    parent_frame_id       BIGINT,                  -- NULL for the root recorded frame

    -- lifecycle
    entry_event_id        BIGINT NOT NULL,         -- the FUNCTION_ENTRY event_id
    exit_event_id         BIGINT,                  -- the FUNCTION_EXIT event_id; NULL if frame didn't exit before recording ended
    exit_kind             VARCHAR,                 -- 'returned', 'raised', 'still_running'

    -- derived fields useful for queries
    depth                 INTEGER NOT NULL,        -- distance from root frame (root = 0)
    call_index            INTEGER NOT NULL,        -- N-th call to this qualified_name in the trace (0-indexed)
    duration_ns           BIGINT,                  -- exit timestamp - entry timestamp; NULL if didn't exit

    -- argument summary for human-readable identification
    argument_summary      VARCHAR                  -- e.g., 'items=[1,2,3], factor=2'; truncated repr of args
);

CREATE INDEX frames_qualified_name ON frames(qualified_name);
CREATE INDEX frames_function_name ON frames(function_name);
CREATE INDEX frames_parent_frame_id ON frames(parent_frame_id);
CREATE INDEX frames_source_file ON frames(source_file);
```

**Notes:**

- `function_name` is the bare name; `qualified_name` includes the module. For methods, qualified_name is `module.ClassName.method_name`.
- `parent_frame_id` is the frame_id of the calling function. Recursive CTEs over this column give you the call tree.
- `exit_kind` distinguishes normal returns ('returned'), exception propagation ('raised'), and frames that were still active when recording ended ('still_running').
- `call_index` lets users say "the 3rd call to process_item": `WHERE qualified_name = 'myapp.process_item' AND call_index = 2`.
- `argument_summary` is a truncated repr of the FUNCTION_ENTRY args, useful for finding a specific call among many: `WHERE argument_summary LIKE '%user_id=42%'`.

### `event_locals`

Every captured local at every event. Expanded from LINE_DELTA and FRAME_SNAPSHOT events at indexing time.

```sql
CREATE TABLE event_locals (
    event_id              BIGINT NOT NULL,
    frame_id              BIGINT NOT NULL,         -- denormalized from events for fast frame-scoped queries
    name                  VARCHAR NOT NULL,
    value_id              BIGINT NOT NULL,

    PRIMARY KEY (event_id, name)
);

CREATE INDEX event_locals_frame_name ON event_locals(frame_id, name);
CREATE INDEX event_locals_value_id ON event_locals(value_id);
CREATE INDEX event_locals_name ON event_locals(name);
```

**Notes:**

- Every event that captures locals (FUNCTION_ENTRY, FRAME_SNAPSHOT, LINE_DELTA, NOTE) contributes rows here.
- LINE_DELTA's "only changed locals" semantic is preserved: an event_locals row exists for a given (event_id, name) only if that event captured that name. To find a variable's value at an arbitrary point, walk backward.
- The walk-backward query: `SELECT value_id FROM event_locals WHERE frame_id = ? AND name = 'x' AND event_id <= ? ORDER BY event_id DESC LIMIT 1`.
- For arguments: a FUNCTION_ENTRY's args are reflected here too (each arg is a captured local). The separate `event_args` table preserves arg order, but for value lookup, event_locals is sufficient.

### `event_args`

Function arguments at FUNCTION_ENTRY events. Preserves position, since args are ordered (unlike locals, which are name-keyed).

```sql
CREATE TABLE event_args (
    event_id              BIGINT NOT NULL,
    position              INTEGER NOT NULL,        -- 0-indexed
    name                  VARCHAR NOT NULL,
    value_id              BIGINT NOT NULL,

    PRIMARY KEY (event_id, position)
);

CREATE INDEX event_args_event_id ON event_args(event_id);
```

**Notes:**

- Only present for events of type `function_entry`.
- Position is preserved for queries like "what was the first argument" or "the value at position 2."
- Arg names also appear in event_locals, so name-based lookup works through that table.

### `note_kwargs`

Keyword arguments attached to NOTE events.

```sql
CREATE TABLE note_kwargs (
    event_id              BIGINT NOT NULL,
    name                  VARCHAR NOT NULL,
    value_id              BIGINT NOT NULL,

    PRIMARY KEY (event_id, name)
);

CREATE INDEX note_kwargs_event_id ON note_kwargs(event_id);
```

**Notes:**

- Populated from `hindsight.note(message, **kwargs)` calls.
- The note's message is in `events.note_message`; this table holds the structured kwargs.

### `values`

Every value referenced anywhere in the trace, with type-specific columns populated based on the value's type.

```sql
CREATE TABLE values (
    value_id              BIGINT PRIMARY KEY,
    type_tag              VARCHAR NOT NULL,        -- 'none', 'bool', 'int', 'big_int', 'float', 'string', 'bytes', 'list', 'dict', 'set', 'cycle_ref', 'summary', 'type_ref', 'exception_unwind_sentinel'
    hash_kind             VARCHAR NOT NULL,        -- 'content', 'summary', 'identity'
    hash_hex              VARCHAR NOT NULL,        -- xxhash3-128 in big-endian hex (32 chars)

    -- type-specific value columns; only one is populated per row, based on type_tag
    bool_value            BOOLEAN,                 -- type='bool'
    int_value             BIGINT,                  -- type='int' (fits in i64)
    big_int_hex           VARCHAR,                 -- type='big_int'; two's complement BE hex
    float_value           DOUBLE,                  -- type='float'
    string_value          VARCHAR,                 -- type='string'
    bytes_value           BLOB,                    -- type='bytes'
    container_length      BIGINT,                  -- type='list','dict','set'; element count
    cycle_ref_depth       INTEGER,                 -- type='cycle_ref'

    -- summary fields (for type='summary' or any inlined-with-summary container)
    type_name             VARCHAR,                 -- e.g., 'numpy.ndarray', 'list', 'MyClass'
    repr_text             VARCHAR,                 -- truncated repr() output for summaries
    summary_length        BIGINT,                  -- type='summary'; type-defined length measure (bytes for strings/bytes; element count for collection summaries; recorder-defined for arbitrary objects)

    -- type ref (for type='type_ref')
    type_ref_name         VARCHAR
);

CREATE INDEX values_type_tag ON values(type_tag);
CREATE INDEX values_hash ON values(hash_kind, hash_hex);
CREATE INDEX values_type_name ON values(type_name);
CREATE INDEX values_int_value ON values(int_value);
CREATE INDEX values_string_value ON values(string_value);
```

**Notes:**

- The columnar storage means rows of different types don't waste space on irrelevant columns.
- `hash_hex` is the 16-byte xxhash3-128 in big-endian hex (32 characters). Equality of values across the trace is detectable by `hash_kind = 'content' AND hash_hex = ?`.
- For mutable objects (lists, dicts, sets that change over time), the `hash_kind` distinguishes content (snapshot of contents) from identity (Python object identity). The same mutable object across mutations has the same identity_hash but different content_hashes.
- Container values reference their elements via `value_elements`, not via inline columns.
- The `repr_text` field is populated for summaries and provides the truncated string representation users see when querying large values.
- `container_length` and `summary_length` are intentionally separate columns even though both store an integer count. They mean different things — `container_length` is element count for inline lists/dicts/sets, while `summary_length` is the recorder's type-defined length measure (byte count for summarized strings, dimensional info for arrays, an arbitrary recorder-chosen number for opaque objects). Squatting one column for both would conflate "lists with > 100 items" and "byte buffers > 100 bytes" under the same query, which is a footgun.

### `value_elements`

Elements of container values (lists, dicts, sets, tuples).

```sql
CREATE TABLE value_elements (
    container_value_id    BIGINT NOT NULL,
    position              INTEGER NOT NULL,        -- 0-indexed; arbitrary order for sets
    key_value_id          BIGINT,                  -- only populated for dicts
    element_value_id      BIGINT NOT NULL,

    PRIMARY KEY (container_value_id, position)
);

CREATE INDEX value_elements_container ON value_elements(container_value_id);
CREATE INDEX value_elements_element ON value_elements(element_value_id);
CREATE INDEX value_elements_key ON value_elements(key_value_id);
```

**Notes:**

- For lists and tuples: `key_value_id` is NULL; `position` is the list index.
- For sets and frozensets: `key_value_id` is NULL; `position` is arbitrary (sets are unordered in Python).
- For dicts: `key_value_id` is the dict key's value_id; `element_value_id` is the corresponding dict value's value_id.
- Recursive containers (lists of lists, etc.) are navigable by joining `value_elements` to itself.
- The `value_elements_element` index supports "where is this value referenced" queries.

### `source_files`

The full source code content of files referenced in the trace.

```sql
CREATE TABLE source_files (
    path                  VARCHAR PRIMARY KEY,
    content_hash          VARCHAR NOT NULL,        -- blake3-256 in hex
    content               TEXT NOT NULL,
    line_count            INTEGER NOT NULL
);
```

**Notes:**

- `content` is the full file text. Querying specific lines is `SELECT content FROM source_files WHERE path = ?` followed by line splitting in the consumer.
- For a tool to read lines N through M of a file, the natural pattern is `WHERE path = ?` and let the application slice. (DuckDB has functions for line operations, but the pattern depends on what tools the MCP server exposes.)
- `content_hash` lets users verify that source matches what was recorded (e.g., comparing against current on-disk content).

### `branches`

A denormalized view of BRANCH_RESULT events. The information also exists in `events`, but a dedicated table makes branch-specific queries clean.

```sql
CREATE TABLE branches (
    event_id              BIGINT PRIMARY KEY,
    frame_id              BIGINT NOT NULL,
    function_name         VARCHAR NOT NULL,        -- denormalized
    source_file           VARCHAR NOT NULL,
    line                  INTEGER NOT NULL,
    taken                 BOOLEAN NOT NULL,
    timestamp_ns          BIGINT NOT NULL
);

CREATE INDEX branches_frame_line ON branches(frame_id, line);
CREATE INDEX branches_source_line ON branches(source_file, line);
CREATE INDEX branches_function ON branches(function_name);
```

**Notes:**

- One row per BRANCH_RESULT event.
- `taken` is the truth value of the condition (true = the branch's "true" path was taken, false = the "false" path).
- For compound conditions (`a and b`), each operand's BRANCH event appears as a separate row. The line tells you which operand.

### `exceptions`

A denormalized view of EXCEPTION_RAISED events.

```sql
CREATE TABLE exceptions (
    event_id              BIGINT PRIMARY KEY,
    frame_id              BIGINT NOT NULL,
    function_name         VARCHAR NOT NULL,
    source_file           VARCHAR NOT NULL,
    line                  INTEGER NOT NULL,
    exception_type        VARCHAR NOT NULL,        -- qualified type name (e.g., 'builtins.ValueError')
    exception_value_id    BIGINT NOT NULL,         -- value_id of the exception instance
    timestamp_ns          BIGINT NOT NULL
);

CREATE INDEX exceptions_type ON exceptions(exception_type);
CREATE INDEX exceptions_frame ON exceptions(frame_id);
CREATE INDEX exceptions_function ON exceptions(function_name);
```

**Notes:**

- One row per EXCEPTION_RAISED event.
- Exceptions are recorded when raised, regardless of whether they're caught later.
- To find caught vs. uncaught exceptions, join with the frame's exit_kind: a frame that exits with `exit_kind = 'returned'` had any raised exceptions caught; one with `exit_kind = 'raised'` had an exception propagate out.

### `notes`

A denormalized view of NOTE events. Just the events with `type = 'note'` plus their kwargs.

```sql
CREATE TABLE notes (
    event_id              BIGINT PRIMARY KEY,
    frame_id              BIGINT NOT NULL,
    function_name         VARCHAR NOT NULL,
    source_file           VARCHAR NOT NULL,
    line                  INTEGER NOT NULL,
    message               VARCHAR NOT NULL,
    timestamp_ns          BIGINT NOT NULL
);

CREATE INDEX notes_frame ON notes(frame_id);
CREATE INDEX notes_function ON notes(function_name);
```

**Notes:**

- Kwargs are in `note_kwargs(event_id, name, value_id)`.
- `notes.message` is the first positional argument to `hindsight.note()`.

### `scope_boundaries`

A denormalized view of SCOPE_BOUNDARY events.

```sql
CREATE TABLE scope_boundaries (
    event_id              BIGINT PRIMARY KEY,
    frame_id              BIGINT NOT NULL,
    boundary_type         VARCHAR NOT NULL,        -- 'entered_skip', 'exited_skip', 'entered_excluded', 'exited_excluded', 'entered_depth_clipped', 'exited_depth_clipped'
    reason                VARCHAR,
    timestamp_ns          BIGINT NOT NULL
);

CREATE INDEX scope_boundaries_frame ON scope_boundaries(frame_id);
CREATE INDEX scope_boundaries_type ON scope_boundaries(boundary_type);
```

**Notes:**

- Six boundary types, paired (entered_X / exited_X).
- `reason` is informational: for excluded functions, the matched pattern; for depth-clipped, the depth limit; for skip blocks, typically just 'user-initiated'.
- `frame_id` is the recorded frame in which the boundary was *observed*. For `entered_excluded` / `entered_depth_clipped` boundaries this is the calling (recorded) frame, not the excluded callee — the callee never gets a frame row because it isn't recorded. For `entered_skip` / `exited_skip` it is the frame containing the `with hindsight.skip():` block. The same convention is used for `events.frame_id` on scope_boundary rows.

## Trace metadata

The trace's metadata block (recorder version, language, configured scope) is stored as a single row in a `trace_metadata` table.

```sql
CREATE TABLE trace_metadata (
    -- recorder info
    recorder_language     VARCHAR NOT NULL,
    recorder_version      VARCHAR NOT NULL,
    language_version      VARCHAR NOT NULL,
    platform              VARCHAR NOT NULL,

    -- recording info
    program               VARCHAR NOT NULL,
    working_directory     VARCHAR,
    trace_uuid            VARCHAR NOT NULL,        -- 32-char hex
    recording_start_ns    BIGINT NOT NULL,
    recording_end_ns      BIGINT,                  -- NULL if recording was unfinalized

    -- scope config (as configured)
    include_patterns      VARCHAR,                 -- comma-separated, NULL if empty
    exclude_patterns      VARCHAR,                 -- comma-separated
    depth_limit           INTEGER,                 -- NULL = unlimited

    -- scope resolution (from final summary, NULL if unfinalized)
    skip_blocks_observed  INTEGER,
    depth_clips_observed  INTEGER,

    -- statistics (from final summary, NULL if unfinalized)
    total_events          BIGINT,
    total_blocks          INTEGER,
    trace_duration_ns     BIGINT,
    function_entry_count  BIGINT,
    line_event_count      BIGINT,
    branch_event_count    BIGINT,
    exception_event_count BIGINT,
    note_event_count      BIGINT
);
```

**Notes:**

- One row per trace. Always exactly one row.
- `recording_end_ns` is NULL for unfinalized traces (recording was interrupted).
- The scope-resolution lists (`recorded_functions`, `excluded_functions`) from the final summary are stored separately:

```sql
CREATE TABLE recorded_functions (qualified_name VARCHAR PRIMARY KEY);
CREATE TABLE excluded_functions (
    qualified_name VARCHAR PRIMARY KEY,
    matched_pattern VARCHAR NOT NULL
);
```

## Common query patterns

The schema is designed for these queries to be natural. Examples:

### "What was variable x at event Y in frame F?"

```sql
SELECT v.* FROM event_locals el
JOIN values v ON el.value_id = v.value_id
WHERE el.frame_id = F
  AND el.name = 'x'
  AND el.event_id <= Y
ORDER BY el.event_id DESC
LIMIT 1;
```

### "How many times did the loop at line 47 run, in this specific call to process_items?"

```sql
SELECT COUNT(*) FROM events
WHERE frame_id = ?
  AND source_file = 'myapp/handlers.py'
  AND line = 47
  AND type = 'line_delta';
```

### "All calls to process_items, with their durations"

```sql
SELECT call_index, entry_event_id, duration_ns, argument_summary, exit_kind
FROM frames
WHERE qualified_name = 'myapp.process_items'
ORDER BY call_index;
```

### "Which branches at line 84 evaluated false?"

```sql
SELECT * FROM branches
WHERE source_file = 'myapp/auth.py'
  AND line = 84
  AND taken = false;
```

### "All exceptions of type ValueError"

```sql
SELECT e.*, v.repr_text AS exception_repr
FROM exceptions e
JOIN values v ON e.exception_value_id = v.value_id
WHERE e.exception_type = 'builtins.ValueError';
```

### "The call tree starting from frame F"

```sql
WITH RECURSIVE tree AS (
    SELECT frame_id, qualified_name, depth, 0 AS tree_depth FROM frames WHERE frame_id = F
    UNION ALL
    SELECT f.frame_id, f.qualified_name, f.depth, t.tree_depth + 1
    FROM frames f
    JOIN tree t ON f.parent_frame_id = t.frame_id
)
SELECT * FROM tree ORDER BY frame_id;
```

### "Functions called more than 100 times"

```sql
SELECT qualified_name, COUNT(*) AS call_count
FROM frames
GROUP BY qualified_name
HAVING COUNT(*) > 100
ORDER BY call_count DESC;
```

### "Find values where an int is between 1 and 100"

```sql
SELECT * FROM values
WHERE type_tag = 'int'
  AND int_value BETWEEN 1 AND 100;
```

### "All notes with their kwargs"

```sql
SELECT n.message, n.line, n.function_name, nk.name AS kwarg_name, v.* AS kwarg_value
FROM notes n
LEFT JOIN note_kwargs nk ON n.event_id = nk.event_id
LEFT JOIN values v ON nk.value_id = v.value_id
ORDER BY n.event_id;
```

### "Was variable x ever mutated to have non-empty contents?"

For mutable values, the same identity_hash with different content_hashes indicates mutation:

```sql
WITH x_values AS (
    SELECT el.event_id, el.value_id, v.hash_kind, v.hash_hex
    FROM event_locals el
    JOIN values v ON el.value_id = v.value_id
    WHERE el.name = 'x'
    ORDER BY el.event_id
)
SELECT * FROM x_values;
```

The user (or LLM) examines the sequence of hash_hex values to see when content changed.

## Operations the schema doesn't directly support

These are intentionally tools, not tables, because they require operations beyond SQL:

- **Causal slicing**: walking backward through events to identify dependencies for a value, parsing source code to extract variable references. Implemented as the `causal_slice` MCP tool which composes SQL queries plus source parsing.
- **Source-aware questions**: e.g., "what type does this function annotation say x is." The MCP server exposes a tool to retrieve source; the LLM combines source content with trace data through multi-step reasoning.
- **Cross-trace comparison**: comparing two indexed traces. Implemented as a separate tool that opens both databases and runs comparison queries.
- **Verification against intent files**: reading markdown intent alongside source and trace, then checking alignment. The LLM does this reasoning; the schema provides the underlying data.

These tools live in the MCP server and use SQL queries against this schema as their substrate.

## Indexer responsibilities

To populate this schema from a `.hindsight` trace, the indexer must:

1. Read the trace using `hindsight-format`'s reader API.
2. Materialize events into the `events` table with denormalized fields populated.
3. Walk events to build the `frames` table, computing `parent_frame_id` from the call stack at each FUNCTION_ENTRY, `depth` from the parent's depth + 1, `call_index` by counting prior calls to the same qualified_name, `duration_ns` from FUNCTION_EXIT timestamp - FUNCTION_ENTRY timestamp.
4. Track running locals state per frame to expand LINE_DELTA into `event_locals` rows correctly. FRAME_SNAPSHOT events reset baseline; LINE_DELTA events add the changed locals.
5. Decode each value into the appropriate `values` row based on its type tag. Recursively populate `value_elements` for containers.
6. Populate type-specific tables (`branches`, `exceptions`, `notes`, `note_kwargs`, `scope_boundaries`, `event_args`).
7. Store source files in `source_files`.
8. Populate `trace_metadata` from the trace's metadata block and final summary.
9. Create indexes after data load.

The indexer should be **idempotent**: running it twice on the same trace produces the same database. The simplest way is to delete and recreate the database file each time.

The indexer should be **resilient to unfinalized traces**: if the trace lacks a final summary or footer, the indexer populates what it can and leaves the unavailable fields NULL.

## Schema evolution

The schema is versioned via a `schema_version` row in `trace_metadata` (or a dedicated `schema_info` table). When the indexer changes the schema, it bumps the version. Consumers can check the version to know what columns and tables are available.

Backward compatibility for the schema is **not** a goal at the same level as the wire format. Indexed databases are regenerable; if the schema changes, users re-index. The wire format is the stable contract; the schema is allowed to evolve more freely.

## Status

This is v0.1 of the schema. Items likely to evolve:

- Additional indexes based on observed query patterns.
- Possible materialized views for very common multi-table queries (e.g., "events with frame info").
- Possible additional denormalization if the LLM's queries frequently need joins not currently supported by direct columns.
- A `value_paths` table for finding nested values by path (e.g., "the third element of the list at this position in this dict") — currently requires recursive joins on `value_elements`.
- Time-based partitioning if traces grow large enough that single-table scans become slow.

Each evolution can ship in an indexer release; users re-index to benefit. The wire format and the recorder don't change.
