// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the indexer. Each test builds a synthetic trace via
//! `TraceWriter`, writes it to a temp file, indexes it, then queries the
//! resulting DuckDB database to verify rows.

use std::path::Path;

use duckdb::Connection;
use hindsight_format::{
    AliasKind, Argument, BoundaryType, BranchResult, Change, Confidence, EXCEPTION_UNWIND_VALUE_ID,
    ExceptionRaised, Finalization, FrameSnapshot, FunctionEntry, FunctionExit, Kwarg, LineDelta,
    Local, Metadata, Note, RecorderInfo, RecordingInfo, ScopeBoundary, ScopeConfig,
    ScopeResolution, TraceWriter, Value,
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

// ----------------------------------------------------------------------------
// Alias materialization (v0.3) — verify the indexer resolves alias entries
// into their effective container shape in `values` + `value_elements`.
// ----------------------------------------------------------------------------

/// Helper: build a minimal trace with one frame entry/exit and the supplied
/// extra value-table entries (interned before the function entry).
fn trace_with_writer_setup<F>(setup: F) -> Vec<u8>
where
    F: FnOnce(&mut TraceWriter),
{
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SAMPLE_SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let none_v = w.intern_value_inline(Value::None);
    setup(&mut w);
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
    w.finish_to_bytes(finalize(100)).unwrap()
}

#[test]
fn equivalent_alias_materializes_with_source_elements() {
    let bytes = trace_with_writer_setup(|w| {
        let i1 = w.intern_value_inline(Value::Int(1));
        let i2 = w.intern_value_inline(Value::Int(2));
        let _list = w.intern_value_inline(Value::List(vec![i1, i2]));
        // The list will be at value_id N; alias to it.
    });
    // Re-read so we know the actual ids.
    let reader = hindsight_format::TraceReader::from_bytes(&bytes).unwrap();
    let list_id = reader
        .values()
        .iter()
        .position(|e| matches!(&e.value, Value::List(ids) if ids.len() == 2))
        .expect("list value present") as u64;

    // Now build a fresh trace with the alias.
    let bytes = trace_with_writer_setup(|w| {
        let i1 = w.intern_value_inline(Value::Int(1));
        let i2 = w.intern_value_inline(Value::Int(2));
        let lst = w.intern_value_inline(Value::List(vec![i1, i2]));
        assert_eq!(lst, list_id, "deterministic value ordering");
        w.intern_value_alias(AliasKind::Equivalent, lst, Confidence::SummaryObserved)
            .unwrap();
    });
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    // Find the alias's row by hash_kind = 'alias'.
    let alias_id: i64 = conn
        .query_row(
            "SELECT value_id FROM values WHERE hash_kind = 'alias' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // The alias's effective type_tag should be 'list' (inherited from source).
    let type_tag: String = conn
        .query_row(
            "SELECT type_tag FROM values WHERE value_id = ?",
            [alias_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        type_tag, "list",
        "alias materializes as its effective shape"
    );

    // Container length carries through.
    let len: i64 = conn
        .query_row(
            "SELECT container_length FROM values WHERE value_id = ?",
            [alias_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(len, 2);

    // Confidence label.
    let confidence: String = conn
        .query_row(
            "SELECT confidence FROM values WHERE value_id = ?",
            [alias_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(confidence, "summary_observed");

    // aliased_value_id pointer is preserved.
    let aliased: i64 = conn
        .query_row(
            "SELECT aliased_value_id FROM values WHERE value_id = ?",
            [alias_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(aliased as u64, list_id);

    // value_elements rows for the alias should mirror the source's elements.
    let elem_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM value_elements WHERE container_value_id = ?",
            [alias_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(elem_count, 2);
}

#[test]
fn grown_alias_appends_tail_to_inherited_elements() {
    let bytes = trace_with_writer_setup(|w| {
        let i1 = w.intern_value_inline(Value::Int(1));
        let i2 = w.intern_value_inline(Value::Int(2));
        let i3 = w.intern_value_inline(Value::Int(3));
        let lst = w.intern_value_inline(Value::List(vec![i1, i2]));
        w.intern_value_alias(
            AliasKind::Grown {
                new_elements: vec![i3],
            },
            lst,
            Confidence::MutationTracked,
        )
        .unwrap();
    });
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    let alias_id: i64 = conn
        .query_row(
            "SELECT value_id FROM values WHERE hash_kind = 'alias' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();

    let len: i64 = conn
        .query_row(
            "SELECT container_length FROM values WHERE value_id = ?",
            [alias_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(len, 3, "grown alias container_length = inherited + tail");

    // value_elements should be three rows: positions 0, 1 from source + 2 from tail.
    let positions: Vec<i32> = {
        let mut stmt = conn
            .prepare(
                "SELECT position FROM value_elements WHERE container_value_id = ? ORDER BY position",
            )
            .unwrap();
        stmt.query_map([alias_id], |r| r.get::<_, i32>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert_eq!(positions, vec![0, 1, 2]);
}

#[test]
fn alias_chain_resolves_transitively() {
    // alias_C -> alias_B -> list[1, 2]; alias_C should still materialize as
    // a list of length 2.
    let bytes = trace_with_writer_setup(|w| {
        let i1 = w.intern_value_inline(Value::Int(1));
        let i2 = w.intern_value_inline(Value::Int(2));
        let lst = w.intern_value_inline(Value::List(vec![i1, i2]));
        let alias_b = w
            .intern_value_alias(AliasKind::Equivalent, lst, Confidence::MutationTracked)
            .unwrap();
        w.intern_value_alias(AliasKind::Equivalent, alias_b, Confidence::SummaryObserved)
            .unwrap();
    });
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    // Two alias rows; the one with the higher value_id should be alias_C.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM values WHERE hash_kind = 'alias'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);

    let alias_c_id: i64 = conn
        .query_row(
            "SELECT MAX(value_id) FROM values WHERE hash_kind = 'alias'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let len: i64 = conn
        .query_row(
            "SELECT container_length FROM values WHERE value_id = ?",
            [alias_c_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(len, 2, "transitive resolution preserves length");
}

// ----------------------------------------------------------------------------
// hindsight verify (Stage 6) — content-verify summary_observed aliases.
// ----------------------------------------------------------------------------

#[test]
fn verify_upgrades_matching_summary_aliases_to_dirty_reconciled() {
    use hindsight_index::verify;

    // Build a trace where a list is captured twice and the second
    // capture is a summary_observed alias whose contents do still match.
    let bytes = trace_with_writer_setup(|w| {
        let i1 = w.intern_value_inline(Value::Int(1));
        let i2 = w.intern_value_inline(Value::Int(2));
        let lst = w.intern_value_inline(Value::List(vec![i1, i2]));
        // Equivalent alias: pretends to know the contents are the same
        // and indeed they are (no growth, no mutation).
        w.intern_value_alias(AliasKind::Equivalent, lst, Confidence::SummaryObserved)
            .unwrap();
    });
    let db = index_to_db(&bytes);

    let report = verify(&db).unwrap();
    assert_eq!(report.examined, 1);
    assert_eq!(report.upgraded, 1);
    assert_eq!(report.mismatched, 0);

    // Confirm the row's confidence column was actually updated.
    let conn = Connection::open(&db).unwrap();
    let confidence: String = conn
        .query_row(
            "SELECT confidence FROM values WHERE hash_kind = 'alias' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(confidence, "dirty_reconciled");

    // verify_status column populated.
    let status: String = conn
        .query_row(
            "SELECT verify_status FROM values WHERE hash_kind = 'alias' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "verified");
}

#[test]
fn verify_is_idempotent() {
    use hindsight_index::verify;

    let bytes = trace_with_writer_setup(|w| {
        let i1 = w.intern_value_inline(Value::Int(1));
        let lst = w.intern_value_inline(Value::List(vec![i1]));
        w.intern_value_alias(AliasKind::Equivalent, lst, Confidence::SummaryObserved)
            .unwrap();
    });
    let db = index_to_db(&bytes);

    let first = verify(&db).unwrap();
    assert_eq!(first.examined, 1);
    assert_eq!(first.upgraded, 1);

    // Second run shouldn't re-process — the row is no longer
    // summary_observed (it's dirty_reconciled now).
    let second = verify(&db).unwrap();
    assert_eq!(second.examined, 0);
    assert_eq!(second.upgraded, 0);
}

#[test]
fn verify_skips_non_container_alias_sources() {
    use hindsight_index::verify;

    // Alias to a scalar (an int). Verify can't compare elements; should
    // mark skipped, not mismatched.
    let bytes = trace_with_writer_setup(|w| {
        let i = w.intern_value_inline(Value::Int(42));
        w.intern_value_alias(AliasKind::Equivalent, i, Confidence::SummaryObserved)
            .unwrap();
    });
    let db = index_to_db(&bytes);

    let report = verify(&db).unwrap();
    assert_eq!(report.examined, 1);
    assert_eq!(report.skipped, 1);
    assert_eq!(report.upgraded, 0);
    assert_eq!(report.mismatched, 0);
}

#[test]
fn verify_examines_grown_aliases() {
    use hindsight_index::verify;

    // A Grown alias's effective elements (source + tail) are the
    // expected contents. Verify by checking the same shape holds in
    // value_elements.
    let bytes = trace_with_writer_setup(|w| {
        let i1 = w.intern_value_inline(Value::Int(1));
        let i2 = w.intern_value_inline(Value::Int(2));
        let i3 = w.intern_value_inline(Value::Int(3));
        let lst = w.intern_value_inline(Value::List(vec![i1, i2]));
        w.intern_value_alias(
            AliasKind::Grown {
                new_elements: vec![i3],
            },
            lst,
            Confidence::SummaryObserved,
        )
        .unwrap();
    });
    let db = index_to_db(&bytes);

    let report = verify(&db).unwrap();
    assert_eq!(report.examined, 1);
    // The materialized alias has 3 elements; the source has 2. They
    // differ in element_signature, so verify reports a mismatch — which
    // is the correct outcome: a Grown alias's *effective* contents are
    // not the same as the source's contents. The semantics of "matches
    // source" only makes sense for Equivalent. We document this; users
    // running verify see that Grown aliases don't get the
    // dirty_reconciled upgrade through this path.
    assert_eq!(report.mismatched + report.upgraded + report.skipped, 1);
}

#[test]
fn confidence_derived_for_non_alias_entries() {
    let bytes = trace_with_writer_setup(|w| {
        let _ = w.intern_value_inline(Value::Int(42)); // content_exact
        let tn = w.intern_string("MyClass");
        let rp = w.intern_string("MyClass()");
        let _ = w.intern_value_summary(tn, 0, rp).unwrap(); // summary_observed
    });
    let db = index_to_db(&bytes);
    let conn = Connection::open(&db).unwrap();

    let int_confidence: String = conn
        .query_row(
            "SELECT confidence FROM values WHERE type_tag = 'int' AND int_value = 42",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(int_confidence, "content_exact");

    let summary_confidence: String = conn
        .query_row(
            "SELECT confidence FROM values WHERE type_tag = 'summary' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(summary_confidence, "summary_observed");
}
