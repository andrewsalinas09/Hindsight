// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the indexer. Each test builds a synthetic trace via
//! `TraceWriter`, writes it to a temp file, indexes it, then queries the
//! resulting DuckDB database to verify rows.

use std::path::Path;

use duckdb::Connection;
use hindsight_format::{
    Argument, BoundaryType, BranchResult, Change, EXCEPTION_UNWIND_VALUE_ID, ExceptionRaised,
    Finalization, FrameSnapshot, FunctionEntry, FunctionExit, Kwarg, LineDelta, Local, Metadata,
    Note, RecorderInfo, RecordingInfo, ScopeBoundary, ScopeConfig, ScopeResolution, TraceWriter,
    Value,
};
use hindsight_index::Indexer;

const SAMPLE_SRC: &str = "def demo():\n    x = 1\n    return x\n";

fn metadata(start_ns: u64) -> Metadata {
    Metadata {
        recorder: RecorderInfo {
            language: "python".into(),
            language_version: "3.12.5".into(),
            recorder_version: "0.1.0".into(),
            platform: "linux-x86_64".into(),
        },
        recording: RecordingInfo {
            program: "python demo.py".into(),
            working_directory: Some("/tmp/proj".into()),
            scope_config: ScopeConfig {
                include: vec![],
                exclude: vec!["defaults".into()],
                depth_limit: None,
            },
        },
        program: None,
        trace_uuid: [0xCD; 16],
        recording_start_ns: start_ns,
    }
}

fn finalize(end_ns: u64) -> Finalization {
    Finalization {
        recording_end_ns: end_ns,
        scope_resolution: ScopeResolution {
            recorded_functions: vec!["demo.demo".into()],
            excluded_functions: vec![],
            skip_blocks_observed: 0,
            depth_clips_observed: 0,
        },
    }
}

/// Write `bytes` to a temp file with a unique name, returning the path.
fn write_tmp(bytes: &[u8], suffix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let unique = format!(
        "hindsight-index-test-{}-{}-{}",
        pid,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        suffix
    );
    let path = dir.join(unique);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn count_str(conn: &Connection, sql: &str) -> Option<String> {
    conn.query_row(sql, [], |r| r.get::<_, Option<String>>(0))
        .unwrap()
}

/// Drive a fully-finalized trace through the indexer. Returns the DB path so
/// the caller can keep querying.
fn index_to_db(trace_bytes: &[u8]) -> std::path::PathBuf {
    let trace = write_tmp(trace_bytes, ".hindsight");
    let db = write_tmp(b"", ".duckdb");
    // Indexer deletes the file first; pre-creating it is fine.
    Indexer::index(&trace, &db).unwrap();
    db
}

// ---- Tests --------------------------------------------------------------

#[test]
fn function_entry_produces_events_and_frames_rows() {
    let mut w = TraceWriter::new(metadata(1_000));
    let file_id = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let arg_name = w.intern_string("x");
    let arg_val = w.intern_value_inline(Value::Int(42));

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: file_id,
        line: 1,
        args: vec![Argument {
            name: arg_name,
            value: arg_val,
        }],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 5,
        frame_id: 0,
        return_value: arg_val,
    })
    .unwrap();

    let bytes = w.finish_to_bytes(finalize(10_000)).unwrap();
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events"), 2);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM frames"), 1);
    let exit_kind: String = conn
        .query_row("SELECT exit_kind FROM frames WHERE frame_id = 0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(exit_kind, "returned");
    let dur: i64 = conn
        .query_row(
            "SELECT duration_ns FROM frames WHERE frame_id = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dur, 5);
    let arg_summary: String = conn
        .query_row(
            "SELECT argument_summary FROM frames WHERE frame_id = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(arg_summary, "x=42");
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM event_args"), 1);
    // event_locals also gets the arg.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM event_locals"), 1);
}

#[test]
fn line_delta_only_changed_locals_in_event_locals() {
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let x_name = w.intern_string("x");
    let y_name = w.intern_string("y");
    let v1 = w.intern_value_inline(Value::Int(1));
    let v2 = w.intern_value_inline(Value::Int(2));

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 1,
        args: vec![],
    })
    .unwrap();
    w.write_frame_snapshot(FrameSnapshot {
        timestamp_delta_ns: 0,
        frame_id: 0,
        line: 1,
        locals: vec![
            Local {
                name: x_name,
                value: v1,
            },
            Local {
                name: y_name,
                value: v1,
            },
        ],
    })
    .unwrap();
    // Only x changes.
    w.write_line_delta(LineDelta {
        timestamp_delta_ns: 1,
        line: 2,
        changes: vec![Change {
            name: x_name,
            value: v2,
        }],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: v2,
    })
    .unwrap();

    let bytes = w.finish_to_bytes(finalize(100)).unwrap();
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    // FRAME_SNAPSHOT contributes 2; LINE_DELTA contributes 1.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM event_locals"), 3);
    // Only x has a row from the line_delta event.
    let line_delta_locals = count(
        &conn,
        "SELECT COUNT(*) FROM event_locals el JOIN events e ON el.event_id = e.event_id \
         WHERE e.type = 'line_delta'",
    );
    assert_eq!(line_delta_locals, 1);
}

#[test]
fn branch_event_populates_branches_table() {
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let none_v = w.intern_value_inline(Value::None);

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 1,
        args: vec![],
    })
    .unwrap();
    w.write_branch_result(BranchResult {
        timestamp_delta_ns: 1,
        line: 2,
        taken: true,
    })
    .unwrap();
    w.write_branch_result(BranchResult {
        timestamp_delta_ns: 1,
        line: 2,
        taken: false,
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: none_v,
    })
    .unwrap();

    let bytes = w.finish_to_bytes(finalize(100)).unwrap();
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM branches"), 2);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM events WHERE type = 'branch_result'"
        ),
        2,
    );
    let taken_count = count(&conn, "SELECT COUNT(*) FROM branches WHERE taken = true");
    assert_eq!(taken_count, 1);
}

#[test]
fn exception_event_populates_exceptions_table_and_raised_exit_kind() {
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let exc_type = w.intern_string("builtins.ValueError");
    let type_name = w.intern_string("ValueError");
    let repr_str = w.intern_string("ValueError('bad')");
    let exc_value = w.intern_value_summary(type_name, 0, repr_str).unwrap();

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 1,
        args: vec![],
    })
    .unwrap();
    w.write_exception_raised(ExceptionRaised {
        timestamp_delta_ns: 1,
        line: 2,
        exception_type: exc_type,
        exception_value: exc_value,
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: EXCEPTION_UNWIND_VALUE_ID,
    })
    .unwrap();

    let bytes = w.finish_to_bytes(finalize(100)).unwrap();
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM exceptions"), 1);
    let exit_kind: String = conn
        .query_row("SELECT exit_kind FROM frames WHERE frame_id = 0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(exit_kind, "raised");
    let exception_type: String = conn
        .query_row("SELECT exception_type FROM exceptions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(exception_type, "builtins.ValueError");
}

#[test]
fn note_event_with_kwargs_populates_notes_and_kwargs() {
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let none_v = w.intern_value_inline(Value::None);
    let msg = w.intern_string("processing");
    let kw_name = w.intern_string("count");
    let kw_val = w.intern_value_inline(Value::Int(42));

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 1,
        args: vec![],
    })
    .unwrap();
    w.write_note(Note {
        timestamp_delta_ns: 1,
        line: 2,
        message: msg,
        kwargs: vec![Kwarg {
            name: kw_name,
            value: kw_val,
        }],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: none_v,
    })
    .unwrap();

    let bytes = w.finish_to_bytes(finalize(100)).unwrap();
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM notes"), 1);
    let msg_db: String = conn
        .query_row("SELECT message FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(msg_db, "processing");
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM note_kwargs"), 1);
    let kw_int: i64 = conn
        .query_row(
            "SELECT v.int_value FROM note_kwargs nk JOIN values v ON nk.value_id = v.value_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kw_int, 42);
}

#[test]
fn scope_boundary_populates_scope_boundaries_table() {
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let none_v = w.intern_value_inline(Value::None);
    let reason = w.intern_string("user-requested skip");

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 1,
        args: vec![],
    })
    .unwrap();
    w.write_scope_boundary(ScopeBoundary {
        timestamp_delta_ns: 1,
        boundary_type: BoundaryType::EnteredSkip,
        reason,
    })
    .unwrap();
    w.write_scope_boundary(ScopeBoundary {
        timestamp_delta_ns: 1,
        boundary_type: BoundaryType::ExitedSkip,
        reason,
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: none_v,
    })
    .unwrap();

    let bytes = w.finish_to_bytes(finalize(100)).unwrap();
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM scope_boundaries"), 2);
    let bt: String = conn
        .query_row(
            "SELECT boundary_type FROM scope_boundaries ORDER BY event_id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bt, "entered_skip");
}

#[test]
fn container_value_produces_value_elements_rows() {
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let one = w.intern_value_inline(Value::Int(1));
    let two = w.intern_value_inline(Value::Int(2));
    let three = w.intern_value_inline(Value::Int(3));
    let list_id = w.intern_value_inline(Value::List(vec![one, two, three]));
    let arg_name = w.intern_string("xs");

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 1,
        args: vec![Argument {
            name: arg_name,
            value: list_id,
        }],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: list_id,
    })
    .unwrap();

    let bytes = w.finish_to_bytes(finalize(100)).unwrap();
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    let elem_count = count(
        &conn,
        &format!("SELECT COUNT(*) FROM value_elements WHERE container_value_id = {list_id}"),
    );
    assert_eq!(elem_count, 3);
    let total_int_values = count(&conn, "SELECT COUNT(*) FROM values WHERE type_tag = 'int'");
    assert_eq!(total_int_values, 3);
    let container_type: String = conn
        .query_row(
            &format!("SELECT type_tag FROM values WHERE value_id = {list_id}"),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(container_type, "list");
}

#[test]
fn summary_value_populates_summary_length_not_container_length() {
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let none_v = w.intern_value_inline(Value::None);
    let type_name_id = w.intern_string("numpy.ndarray");
    let repr_id = w.intern_string("array([1, 2, 3, ...])");
    // Summary length 1024 — represents bytes / element count / etc.,
    // semantically distinct from container element count.
    let summary_id = w.intern_value_summary(type_name_id, 1024, repr_id).unwrap();
    let arg_name = w.intern_string("arr");

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 1,
        args: vec![Argument {
            name: arg_name,
            value: summary_id,
        }],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: none_v,
    })
    .unwrap();
    let bytes = w.finish_to_bytes(finalize(100)).unwrap();
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    // Summary value should populate summary_length, not container_length.
    let summary_length: Option<i64> = conn
        .query_row(
            &format!("SELECT summary_length FROM values WHERE value_id = {summary_id}"),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(summary_length, Some(1024));
    let container_length: Option<i64> = conn
        .query_row(
            &format!("SELECT container_length FROM values WHERE value_id = {summary_id}"),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(container_length, None);
    let type_name: String = conn
        .query_row(
            &format!("SELECT type_name FROM values WHERE value_id = {summary_id}"),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(type_name, "numpy.ndarray");
}

#[test]
fn nested_calls_compute_correct_depth_and_call_index() {
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let outer_id = w.intern_string("demo.outer");
    let inner_id = w.intern_string("demo.inner");
    let none_v = w.intern_value_inline(Value::None);

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: outer_id,
        source_file_id: fid,
        line: 1,
        args: vec![],
    })
    .unwrap();
    // Two calls to inner.
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 1,
        frame_id: 1,
        function_id: inner_id,
        source_file_id: fid,
        line: 2,
        args: vec![],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 1,
        return_value: none_v,
    })
    .unwrap();
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 1,
        frame_id: 2,
        function_id: inner_id,
        source_file_id: fid,
        line: 2,
        args: vec![],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 2,
        return_value: none_v,
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: none_v,
    })
    .unwrap();

    let bytes = w.finish_to_bytes(finalize(100)).unwrap();
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    let outer_depth: i32 = conn
        .query_row("SELECT depth FROM frames WHERE frame_id = 0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(outer_depth, 0);
    let inner1_depth: i32 = conn
        .query_row("SELECT depth FROM frames WHERE frame_id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(inner1_depth, 1);
    let inner1_idx: i32 = conn
        .query_row(
            "SELECT call_index FROM frames WHERE frame_id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inner1_idx, 0);
    let inner2_idx: i32 = conn
        .query_row(
            "SELECT call_index FROM frames WHERE frame_id = 2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inner2_idx, 1);
    // Parent linkage.
    let inner1_parent: i64 = conn
        .query_row(
            "SELECT parent_frame_id FROM frames WHERE frame_id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inner1_parent, 0);
}

#[test]
fn source_files_get_hashed_and_stored() {
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let _ = fid;
    let func_id = w.intern_string("demo.demo");
    let none_v = w.intern_value_inline(Value::None);
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: 0,
        line: 1,
        args: vec![],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: none_v,
    })
    .unwrap();
    let bytes = w.finish_to_bytes(finalize(100)).unwrap();
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    let path: String = conn
        .query_row("SELECT path FROM source_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(path, "demo.py");
    let hash: String = conn
        .query_row("SELECT content_hash FROM source_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    let line_count: i32 = conn
        .query_row("SELECT line_count FROM source_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(line_count, 3);
}

#[test]
fn idempotency_yields_identical_databases() {
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let none_v = w.intern_value_inline(Value::None);
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 1,
        args: vec![],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: none_v,
    })
    .unwrap();
    let bytes = w.finish_to_bytes(finalize(100)).unwrap();
    let trace = write_tmp(&bytes, ".hindsight");
    let db = write_tmp(b"", ".duckdb");
    Indexer::index(&trace, &db).unwrap();
    let first_counts = collect_counts(&db);
    Indexer::index(&trace, &db).unwrap();
    let second_counts = collect_counts(&db);
    assert_eq!(first_counts, second_counts);
}

fn collect_counts(db: &Path) -> Vec<(String, i64)> {
    let conn = Connection::open(db).unwrap();
    let tables = [
        "events",
        "frames",
        "event_locals",
        "event_args",
        "note_kwargs",
        "values",
        "value_elements",
        "source_files",
        "branches",
        "exceptions",
        "notes",
        "scope_boundaries",
        "trace_metadata",
        "recorded_functions",
        "excluded_functions",
    ];
    tables
        .iter()
        .map(|t| {
            let n = count(&conn, &format!("SELECT COUNT(*) FROM {t}"));
            (t.to_string(), n)
        })
        .collect()
}

#[test]
fn unfinalized_trace_indexes_with_null_summary_fields() {
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 1,
        args: vec![],
    })
    .unwrap();
    // Note: no FUNCTION_EXIT — the frame is "still_running" at the abrupt
    // end of the trace. We also use write_unfinalized to omit the summary.
    let mut bytes = Vec::new();
    w.write_unfinalized(&mut bytes).unwrap();

    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();
    let still_running = count(
        &conn,
        "SELECT COUNT(*) FROM frames WHERE exit_kind = 'still_running'",
    );
    assert_eq!(still_running, 1);
    let total_events = count_str(&conn, "SELECT total_events FROM trace_metadata");
    assert!(
        total_events.is_none(),
        "total_events should be NULL for unfinalized traces, got {total_events:?}"
    );
}

#[test]
fn trace_metadata_row_inserted() {
    let mut w = TraceWriter::new(metadata(123));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let none_v = w.intern_value_inline(Value::None);
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 1,
        args: vec![],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: none_v,
    })
    .unwrap();
    let bytes = w.finish_to_bytes(finalize(456)).unwrap();
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM trace_metadata"), 1);
    let lang: String = conn
        .query_row("SELECT recorder_language FROM trace_metadata", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(lang, "python");
    let start: i64 = conn
        .query_row("SELECT recording_start_ns FROM trace_metadata", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(start, 123);
    let end: i64 = conn
        .query_row("SELECT recording_end_ns FROM trace_metadata", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(end, 456);
    // Recorded functions table should also be populated.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM recorded_functions"), 1);
}
