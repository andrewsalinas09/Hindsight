// SPDX-License-Identifier: Apache-2.0

//! Static prose descriptions for the tables and most-used columns of the
//! indexed schema. The `describe_schema` tool stitches these together with
//! the live DuckDB schema to produce LLM-facing documentation.

pub struct TableDescription {
    pub name: &'static str,
    pub description: &'static str,
    pub columns: &'static [(&'static str, &'static str)],
}

/// Tables documented in `docs/indexer-schema.md`. The order chosen here
/// puts the central tables first so the LLM sees the most important parts
/// at the top of any rendering.
pub const TABLES: &[TableDescription] = &[
    TableDescription {
        name: "events",
        description: "Every event in the trace. The central table — most queries touch it. Each row \
             carries denormalized convenience fields (function_name, source_file, line) plus \
             type-specific columns populated based on `type`.",
        columns: &[
            ("event_id", "Globally unique, monotonically increasing"),
            (
                "type",
                "function_entry, function_exit, line_delta, frame_snapshot, branch_result, exception_raised, note, scope_boundary",
            ),
            (
                "frame_id",
                "The activation this event belongs to (FK → frames)",
            ),
            ("timestamp_ns", "Absolute nanoseconds since recording start"),
            (
                "source_file",
                "Denormalized source path for the event; NULL when not applicable",
            ),
            (
                "line",
                "Source line; NULL for events without a line attribution",
            ),
            (
                "function_name",
                "Denormalized qualified name of the frame's function",
            ),
            ("return_value_id", "Set on type='function_exit'"),
            ("branch_taken", "Set on type='branch_result'"),
            (
                "exception_type",
                "Qualified type name; set on type='exception_raised'",
            ),
            (
                "exception_value_id",
                "Value of the raised exception; set on type='exception_raised'",
            ),
            ("note_message", "Set on type='note'"),
            (
                "boundary_type",
                "Set on type='scope_boundary' (entered_skip/exited_skip/...)",
            ),
            ("boundary_reason", "Set on type='scope_boundary'"),
        ],
    },
    TableDescription {
        name: "frames",
        description: "One row per function activation — the natural unit of scoping for most debugging \
             questions. Recursive joins on parent_frame_id give the call tree.",
        columns: &[
            ("frame_id", "PK"),
            ("function_name", "Bare function name"),
            (
                "qualified_name",
                "Module-qualified name (e.g., 'mymod.process_item')",
            ),
            (
                "source_file",
                "Path to the source file this function was defined in",
            ),
            (
                "parent_frame_id",
                "Calling frame; NULL for the root recorded frame",
            ),
            ("entry_event_id", "FUNCTION_ENTRY event_id"),
            (
                "exit_event_id",
                "FUNCTION_EXIT event_id; NULL if frame didn't exit",
            ),
            ("exit_kind", "'returned', 'raised', or 'still_running'"),
            ("depth", "Distance from root frame (0-indexed)"),
            (
                "call_index",
                "N-th call to this qualified_name in the trace (0-indexed)",
            ),
            (
                "duration_ns",
                "Wall time of the activation; NULL if didn't exit",
            ),
            (
                "argument_summary",
                "Truncated repr of the FUNCTION_ENTRY args, e.g. 'items=[1,2,3], factor=2'",
            ),
        ],
    },
    TableDescription {
        name: "event_locals",
        description: "Every captured local at every event. Expanded from LINE_DELTA and FRAME_SNAPSHOT \
             events at indexing time. To find a variable's value at an arbitrary point, walk \
             backward — only changes are captured.",
        columns: &[
            ("event_id", "Event that captured this local"),
            ("frame_id", "Denormalized for fast frame-scoped queries"),
            ("name", "Variable name"),
            ("value_id", "FK → values"),
        ],
    },
    TableDescription {
        name: "event_args",
        description: "Function arguments at FUNCTION_ENTRY events. Preserves position; for name-based \
             lookup, event_locals is sufficient since args are reflected there too.",
        columns: &[
            ("event_id", "FUNCTION_ENTRY event_id"),
            ("position", "0-indexed argument position"),
            ("name", "Argument name"),
            ("value_id", "FK → values"),
        ],
    },
    TableDescription {
        name: "values",
        description: "Every value referenced anywhere in the trace, with type-specific columns populated \
             based on type_tag.",
        columns: &[
            ("value_id", "PK"),
            (
                "type_tag",
                "none, bool, int, big_int, float, string, bytes, list, dict, set, cycle_ref, summary, type_ref, exception_unwind_sentinel",
            ),
            ("hash_kind", "'content', 'summary', or 'identity'"),
            ("hash_hex", "xxhash3-128 in big-endian hex (32 chars)"),
            ("bool_value", "Populated when type_tag='bool'"),
            ("int_value", "Populated when type_tag='int'"),
            (
                "big_int_hex",
                "Populated when type_tag='big_int'; two's complement BE hex",
            ),
            ("float_value", "Populated when type_tag='float'"),
            ("string_value", "Populated when type_tag='string'"),
            ("bytes_value", "Populated when type_tag='bytes'"),
            ("container_length", "Element count for inline list/dict/set"),
            (
                "type_name",
                "e.g., 'numpy.ndarray', 'list', 'MyClass' (often set on summaries)",
            ),
            ("repr_text", "Truncated repr() output for summaries"),
            ("summary_length", "Recorder-defined length for summaries"),
            (
                "type_ref_name",
                "Name of the referenced type when type_tag='type_ref'",
            ),
        ],
    },
    TableDescription {
        name: "value_elements",
        description: "Elements of container values. For dicts, key_value_id is the dict key; for \
             lists/sets/tuples, key_value_id is NULL and position is the index.",
        columns: &[
            ("container_value_id", "FK → values"),
            ("position", "0-indexed (arbitrary order for sets)"),
            ("key_value_id", "Only populated for dicts"),
            ("element_value_id", "FK → values"),
        ],
    },
    TableDescription {
        name: "source_files",
        description: "Full source code content of files referenced in the trace.",
        columns: &[
            ("path", "PK"),
            ("content_hash", "blake3-256 hex"),
            ("content", "Full file text"),
            ("line_count", "Number of source lines"),
        ],
    },
    TableDescription {
        name: "branches",
        description: "Denormalized view of BRANCH_RESULT events.",
        columns: &[
            ("event_id", "PK (FK → events)"),
            ("frame_id", ""),
            ("function_name", "Denormalized"),
            ("source_file", ""),
            ("line", "Source line of the branching opcode"),
            ("taken", "True iff the condition's true-branch was taken"),
            ("timestamp_ns", ""),
        ],
    },
    TableDescription {
        name: "exceptions",
        description: "Denormalized view of EXCEPTION_RAISED events.",
        columns: &[
            ("event_id", "PK (FK → events)"),
            ("frame_id", ""),
            ("function_name", "Denormalized"),
            ("source_file", ""),
            ("line", "Line where the exception was raised"),
            (
                "exception_type",
                "Qualified type name (e.g., 'builtins.ValueError')",
            ),
            ("exception_value_id", "FK → values"),
            ("timestamp_ns", ""),
        ],
    },
    TableDescription {
        name: "notes",
        description: "Denormalized view of NOTE events. Kwargs live in note_kwargs.",
        columns: &[
            ("event_id", "PK"),
            ("frame_id", ""),
            ("function_name", ""),
            ("source_file", ""),
            ("line", ""),
            ("message", "First positional arg to hindsight.note(...)"),
            ("timestamp_ns", ""),
        ],
    },
    TableDescription {
        name: "note_kwargs",
        description: "Keyword arguments attached to NOTE events.",
        columns: &[
            ("event_id", "FK → notes/events"),
            ("name", "Kwarg name"),
            ("value_id", "FK → values"),
        ],
    },
    TableDescription {
        name: "scope_boundaries",
        description: "Denormalized view of SCOPE_BOUNDARY events: paired entered_X / exited_X markers.",
        columns: &[
            ("event_id", "PK"),
            ("frame_id", ""),
            (
                "boundary_type",
                "entered_skip / exited_skip / entered_excluded / exited_excluded / entered_depth_clipped / exited_depth_clipped",
            ),
            ("reason", "Informational"),
            ("timestamp_ns", ""),
        ],
    },
    TableDescription {
        name: "trace_metadata",
        description: "Single row of recorder/recording metadata for the trace.",
        columns: &[
            ("recorder_language", ""),
            ("recorder_version", ""),
            ("language_version", ""),
            ("platform", ""),
            ("program", ""),
            ("working_directory", ""),
            ("trace_uuid", "32-char hex"),
            ("recording_start_ns", ""),
            ("recording_end_ns", "NULL if recording was unfinalized"),
        ],
    },
];

pub struct QueryPattern {
    pub name: &'static str,
    pub description: &'static str,
    pub sql: &'static str,
}

/// Hand-picked example queries that double as documentation and few-shot
/// material for the LLM.
pub const QUERY_PATTERNS: &[QueryPattern] = &[
    QueryPattern {
        name: "Walk-backward variable lookup",
        description: "Find a variable's value at an arbitrary point. event_locals only captures changes; \
             walk back to find the most recent capture. Use the trace_variable tool when you \
             want the full history.",
        sql: "SELECT v.* FROM event_locals el \
              JOIN values v ON el.value_id = v.value_id \
              WHERE el.frame_id = ? AND el.name = ? AND el.event_id <= ? \
              ORDER BY el.event_id DESC LIMIT 1",
    },
    QueryPattern {
        name: "Iteration count for a loop",
        description: "How many times did the loop at <line> run inside this specific frame? Counts \
             LINE_DELTA events on the loop header.",
        sql: "SELECT COUNT(*) FROM events \
              WHERE frame_id = ? AND type = 'line_delta' AND line = ?",
    },
    QueryPattern {
        name: "All calls to a function with durations",
        description: "Use this to see how a function was used across the trace.",
        sql: "SELECT call_index, entry_event_id, duration_ns, argument_summary, exit_kind \
              FROM frames WHERE qualified_name = ? ORDER BY call_index",
    },
    QueryPattern {
        name: "Branches at a source line",
        description: "Which way did each evaluation of an `if` go?",
        sql: "SELECT event_id, function_name, line, taken \
              FROM branches WHERE source_file LIKE ? AND line = ? ORDER BY event_id",
    },
    QueryPattern {
        name: "Exceptions of a given type",
        description: "Filter the exceptions table by qualified type name.",
        sql: "SELECT e.*, v.repr_text AS exception_repr \
              FROM exceptions e JOIN values v ON e.exception_value_id = v.value_id \
              WHERE e.exception_type = ?",
    },
    QueryPattern {
        name: "Recursive call tree from a frame",
        description: "Walk the children of a root frame using parent_frame_id.",
        sql: "WITH RECURSIVE tree AS ( \
                SELECT frame_id, qualified_name, depth FROM frames WHERE frame_id = ? \
                UNION ALL \
                SELECT f.frame_id, f.qualified_name, f.depth FROM frames f \
                JOIN tree t ON f.parent_frame_id = t.frame_id \
              ) SELECT * FROM tree ORDER BY frame_id",
    },
    QueryPattern {
        name: "Function call counts",
        description: "Hot path discovery — which functions ran the most?",
        sql: "SELECT qualified_name, COUNT(*) AS call_count \
              FROM frames GROUP BY qualified_name ORDER BY call_count DESC",
    },
    QueryPattern {
        name: "Slowest frames",
        description: "Frames sorted by wall time.",
        sql: "SELECT qualified_name, call_index, duration_ns, exit_kind \
              FROM frames WHERE duration_ns IS NOT NULL ORDER BY duration_ns DESC LIMIT 10",
    },
    QueryPattern {
        name: "Find an integer value by content",
        description: "type_tag and int_value are both indexed.",
        sql: "SELECT value_id, type_tag, int_value FROM values \
              WHERE type_tag = 'int' AND int_value = ?",
    },
    QueryPattern {
        name: "Notes with their kwargs",
        description: "Inspect every hindsight.note(...) call with its structured kwargs.",
        sql: "SELECT n.event_id, n.message, n.line, nk.name AS kwarg_name, \
                     v.type_tag, v.int_value, v.float_value, v.string_value, v.repr_text \
              FROM notes n LEFT JOIN note_kwargs nk ON n.event_id = nk.event_id \
              LEFT JOIN values v ON nk.value_id = v.value_id ORDER BY n.event_id",
    },
];
