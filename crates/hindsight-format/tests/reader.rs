// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the trace reader.
//!
//! Two flavors:
//! - **Round-trip**: build a trace via [`TraceWriter`], serialize, parse via
//!   [`TraceReader`], and assert the parsed structure matches what the writer
//!   produced.
//! - **Error**: hand-corrupt a writer-produced trace and confirm the reader
//!   surfaces a clear, specific error rather than silently misparsing.

use std::io::Cursor;

use hindsight_format::{
    Argument, BLOCK_TAG_CHECKPOINT, BLOCK_TAG_EVENT, BLOCK_TAG_TABLE_SNAPSHOT,
    BLOCK_TAG_TABLE_UPDATE, BoundaryType, BranchResult, Change, EXCEPTION_UNWIND_VALUE_ID, Event,
    ExceptionRaised, ExcludedFunction, FOOTER_LENGTH, Finalization, FormatError, FrameSnapshot,
    FrameSwitch, FrameSwitchReason, FunctionEntry, FunctionExit, HEADER_LENGTH, HashKind, Kwarg,
    LineDelta, Local, Metadata, NONE_VALUE_ID, Note, ProgramInfo, RecorderInfo, RecordingInfo,
    ScopeBoundary, ScopeConfig, ScopeResolution, TraceReader, TraceWriter, Value, WriterConfig,
};

const FILE_OFFSET_HEADER_LEN: usize = 12;
const FILE_OFFSET_HEADER_FLAGS: usize = 10;
const FILE_OFFSET_VERSION: usize = 8;
const FILE_OFFSET_MAGIC: usize = 0;
const HEADER_OFFSET_RECORDING_END: usize = 40;
const HEADER_OFFSET_FOOTER_OFFSET: usize = 48;
const RECORDING_START_NS: u64 = 1_700_000_000_000_000_000;
const RECORDING_END_NS: u64 = 1_700_000_001_234_567_890;

fn fixture_metadata() -> Metadata {
    Metadata {
        recorder: RecorderInfo {
            language: "python".into(),
            language_version: "3.12.5".into(),
            recorder_version: "0.1.0".into(),
            platform: "linux-x86_64".into(),
        },
        recording: RecordingInfo {
            program: "python script.py".into(),
            working_directory: Some("/work".into()),
            scope_config: ScopeConfig {
                include: vec![],
                exclude: vec!["defaults".into()],
                depth_limit: None,
            },
        },
        program: Some(ProgramInfo {
            fields: vec![("git_commit".into(), "abc123".into())],
        }),
        trace_uuid: [0xCC; 16],
        recording_start_ns: RECORDING_START_NS,
    }
}

fn default_finalization() -> Finalization {
    Finalization {
        recording_end_ns: RECORDING_END_NS,
        scope_resolution: ScopeResolution::default(),
    }
}

/// Finalize the writer with the default Finalization (no scope resolution).
fn finalize(w: TraceWriter) -> Vec<u8> {
    w.finish_to_bytes(default_finalization()).unwrap()
}

/// Emit the writer as an unfinalized stream — no final summary, no footer.
fn unfinalized(w: TraceWriter) -> Vec<u8> {
    let mut out = Vec::new();
    w.write_unfinalized(&mut out).unwrap();
    out
}

// ---------- finalized round-trip ----------

#[test]
fn round_trip_finalized_empty_trace() {
    let bytes = finalize(TraceWriter::new(fixture_metadata()));

    let r = TraceReader::from_bytes(&bytes).unwrap();
    let h = r.header();
    assert_eq!(h.format_version_major, 0);
    assert_eq!(h.format_version_minor, 3);
    assert_eq!(h.flags, 0);
    assert_eq!(h.header_length, HEADER_LENGTH);
    assert_eq!(h.trace_uuid, [0xCC; 16]);
    assert_eq!(h.recording_start_ns, RECORDING_START_NS);
    assert_eq!(
        h.recording_end_ns, RECORDING_END_NS,
        "finalized trace patches recording_end"
    );
    assert!(h.footer_offset > 0, "finalized trace patches footer_offset");

    let m = r.metadata();
    assert_eq!(m.format_tag, 0x01);
    assert!(m.payload.contains("[recorder]"));
    assert!(m.payload.contains("language = \"python\""));
    assert!(m.payload.contains("git_commit = \"abc123\""));

    assert!(r.source_files().is_empty());
    assert!(r.strings().is_empty());

    assert_eq!(r.values().len(), 2);
    assert!(matches!(r.values()[0].value, Value::None));
    assert!(matches!(
        r.values()[1].value,
        Value::ExceptionUnwindSentinel
    ));

    assert!(r.events().is_empty());
    assert_eq!(
        r.event_blocks().len(),
        1,
        "even an empty trace has one event block"
    );
    assert_eq!(r.event_blocks()[0].first_event_id, 0);
    assert_eq!(r.event_blocks()[0].event_count, 0);

    assert!(r.is_finalized());
    let summary = r.final_summary().expect("finalized trace has summary");
    assert!(summary.payload.contains("[final]"));
    assert!(summary.payload.contains("clean_shutdown = true"));
    assert!(summary.payload.contains("total_events = 0"));
    assert!(summary.payload.contains("[final.statistics]"));
    assert!(summary.payload.contains("trace_duration_ns = 1234567890"));

    assert!(r.checkpoints().is_empty());
    let footer = r.footer().expect("finalized trace has footer");
    assert_eq!(footer.footer_length, FOOTER_LENGTH);
    assert!(footer.final_summary_offset < footer.checkpoint_index_offset);
}

/// Round-trip via the public `Read` API rather than `from_bytes`.
#[test]
fn round_trip_via_read_trait() {
    let bytes = finalize(TraceWriter::new(fixture_metadata()));
    let r = TraceReader::new(Cursor::new(&bytes)).unwrap();
    assert_eq!(r.values().len(), 2);
    assert!(r.is_finalized());
}

#[test]
fn round_trip_value_table_covers_all_supported_value_types() {
    let mut w = TraceWriter::new(fixture_metadata());

    let type_name = w.intern_string("numpy.ndarray");
    let repr = w.intern_string("array([1,2,3])");

    let v_bool = w.intern_value_inline(Value::Bool(true));
    let v_int = w.intern_value_inline(Value::Int(-42));
    let v_bigint = w.intern_value_inline(Value::BigInt(vec![0x01, 0x00]));
    let v_float = w.intern_value_inline(Value::Float(3.5));
    let v_string = w.intern_value_inline(Value::String("hello".into()));
    let v_bytes = w.intern_value_inline(Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    let v_typeref = w.intern_value_inline(Value::TypeRef(type_name));
    let v_cycle = w.intern_value_inline(Value::CycleRef(2));
    let v_list = w.intern_value_inline(Value::List(vec![v_bool, v_int]));
    let v_set = w.intern_value_inline(Value::Set(vec![v_bytes]));
    let v_dict = w.intern_value_inline(Value::Dict(vec![(v_string, v_int)]));
    let v_summary = w.intern_value_summary(type_name, 3, repr).unwrap();
    let v_identity = w.intern_value_with_identity(Value::Int(7), [0xAB; 16]);

    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    let values = r.values();

    assert_eq!(values.len(), 2 + 13);

    assert!(matches!(values[v_bool as usize].value, Value::Bool(true)));
    assert!(matches!(values[v_int as usize].value, Value::Int(-42)));
    match &values[v_bigint as usize].value {
        Value::BigInt(b) => assert_eq!(b, &vec![0x01, 0x00]),
        v => panic!("expected BigInt, got {v:?}"),
    }
    match values[v_float as usize].value {
        Value::Float(f) => assert_eq!(f, 3.5),
        ref v => panic!("expected Float, got {v:?}"),
    }
    match &values[v_string as usize].value {
        Value::String(s) => assert_eq!(s, "hello"),
        v => panic!("expected String, got {v:?}"),
    }
    match &values[v_bytes as usize].value {
        Value::Bytes(b) => assert_eq!(b, &vec![0xDE, 0xAD, 0xBE, 0xEF]),
        v => panic!("expected Bytes, got {v:?}"),
    }
    match values[v_typeref as usize].value {
        Value::TypeRef(id) => assert_eq!(id, type_name),
        ref v => panic!("expected TypeRef, got {v:?}"),
    }
    match values[v_cycle as usize].value {
        Value::CycleRef(d) => assert_eq!(d, 2),
        ref v => panic!("expected CycleRef, got {v:?}"),
    }
    match &values[v_list as usize].value {
        Value::List(ids) => assert_eq!(ids, &vec![v_bool, v_int]),
        v => panic!("expected List, got {v:?}"),
    }
    match &values[v_set as usize].value {
        Value::Set(ids) => assert_eq!(ids, &vec![v_bytes]),
        v => panic!("expected Set, got {v:?}"),
    }
    match &values[v_dict as usize].value {
        Value::Dict(pairs) => assert_eq!(pairs, &vec![(v_string, v_int)]),
        v => panic!("expected Dict, got {v:?}"),
    }
    match values[v_summary as usize].value {
        Value::Summary {
            type_name: tn,
            length,
            repr: rp,
        } => {
            assert_eq!(tn, type_name);
            assert_eq!(length, 3);
            assert_eq!(rp, repr);
        }
        ref v => panic!("expected Summary, got {v:?}"),
    }

    assert_eq!(values[v_int as usize].hash_kind, HashKind::Content);
    assert_eq!(values[v_summary as usize].hash_kind, HashKind::Summary);
    assert_eq!(values[v_identity as usize].hash_kind, HashKind::Identity);
    assert_eq!(values[v_identity as usize].hash, [0xAB; 16]);

    assert!(matches!(values[NONE_VALUE_ID as usize].value, Value::None));
    assert!(matches!(
        values[EXCEPTION_UNWIND_VALUE_ID as usize].value,
        Value::ExceptionUnwindSentinel
    ));
}

#[test]
fn round_trip_worked_example_events() {
    let mut w = TraceWriter::new(fixture_metadata());

    let file_id = w.add_source_file(
        "example.py",
        b"def double(x):\n    result = x * 2\n    return result\n".to_vec(),
    );
    let func_name = w.intern_string("__main__.double");
    let arg_x = w.intern_string("x");
    let local_result = w.intern_string("result");
    let v5 = w.intern_value_inline(Value::Int(5));
    let v10 = w.intern_value_inline(Value::Int(10));

    let entry = FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_name,
        source_file_id: file_id,
        line: 1,
        args: vec![Argument {
            name: arg_x,
            value: v5,
        }],
    };
    let snap = FrameSnapshot {
        timestamp_delta_ns: 100,
        frame_id: 0,
        line: 1,
        locals: vec![Local {
            name: arg_x,
            value: v5,
        }],
    };
    let line2 = LineDelta {
        timestamp_delta_ns: 50,
        line: 2,
        changes: vec![Change {
            name: local_result,
            value: v10,
        }],
    };
    let line3 = LineDelta {
        timestamp_delta_ns: 25,
        line: 3,
        changes: vec![],
    };
    let exit = FunctionExit {
        timestamp_delta_ns: 25,
        frame_id: 0,
        return_value: v10,
    };

    w.write_function_entry(entry.clone()).unwrap();
    w.write_frame_snapshot(snap.clone()).unwrap();
    w.write_line_delta(line2.clone()).unwrap();
    w.write_line_delta(line3.clone()).unwrap();
    w.write_function_exit(exit.clone()).unwrap();

    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();

    assert_eq!(
        r.event_blocks().len(),
        1,
        "5 events fit comfortably in one block"
    );
    assert_eq!(r.event_blocks()[0].event_count, 5);
    assert_eq!(r.event_blocks()[0].first_event_id, 0);

    let events = r.events();
    assert_eq!(events.len(), 5);
    assert_eq!(events[0], Event::FunctionEntry(entry));
    assert_eq!(events[1], Event::FrameSnapshot(snap));
    assert_eq!(events[2], Event::LineDelta(line2));
    assert_eq!(events[3], Event::LineDelta(line3));
    assert_eq!(events[4], Event::FunctionExit(exit));

    assert_eq!(r.source_files().len(), 1);
    let f = &r.source_files()[0];
    assert_eq!(f.path, "example.py");
    assert_eq!(*blake3::hash(&f.content).as_bytes(), f.blake3_hash);

    let summary = r.final_summary().unwrap();
    assert!(summary.payload.contains("function_entry_events = 1"));
    assert!(summary.payload.contains("function_exit_events = 1"));
    assert!(summary.payload.contains("frame_snapshot_events = 1"));
    assert!(summary.payload.contains("line_events = 2"));
    assert!(summary.payload.contains("total_events = 5"));
}

#[test]
fn round_trip_multiple_source_files_and_strings_preserve_order() {
    let mut w = TraceWriter::new(fixture_metadata());

    let f0 = w.add_source_file("a.py", b"a = 1\n".to_vec());
    let f1 = w.add_source_file("b.py", b"b = 2\n".to_vec());
    let f2 = w.add_source_file("c.py", b"c = 3\n".to_vec());

    let s0 = w.intern_string("alpha");
    let s1 = w.intern_string("beta");
    let s2 = w.intern_string("gamma");

    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();

    assert_eq!(r.source_files().len(), 3);
    assert_eq!(r.source_files()[f0 as usize].path, "a.py");
    assert_eq!(r.source_files()[f1 as usize].path, "b.py");
    assert_eq!(r.source_files()[f2 as usize].path, "c.py");

    assert_eq!(r.strings()[s0 as usize], "alpha");
    assert_eq!(r.strings()[s1 as usize], "beta");
    assert_eq!(r.strings()[s2 as usize], "gamma");
}

// ---------- new event types ----------

#[test]
fn round_trip_branch_result_events() {
    let mut w = TraceWriter::new(fixture_metadata());
    let true_taken = BranchResult {
        timestamp_delta_ns: 5,
        line: 12,
        taken: true,
    };
    let false_taken = BranchResult {
        timestamp_delta_ns: 7,
        line: 14,
        taken: false,
    };
    w.write_branch_result(true_taken.clone()).unwrap();
    w.write_branch_result(false_taken.clone()).unwrap();
    w.write_branch_result(true_taken.clone()).unwrap();

    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.events().len(), 3);
    assert_eq!(r.events()[0], Event::BranchResult(true_taken.clone()));
    assert_eq!(r.events()[1], Event::BranchResult(false_taken));
    assert_eq!(r.events()[2], Event::BranchResult(true_taken));
    assert!(
        r.final_summary()
            .unwrap()
            .payload
            .contains("branch_events = 3")
    );
}

#[test]
fn round_trip_exception_raised_event() {
    let mut w = TraceWriter::new(fixture_metadata());
    let exc_type = w.intern_string("ValueError");
    let exc_summary_type = w.intern_string("ValueError");
    let repr = w.intern_string("ValueError('bad input')");
    let exc_value = w.intern_value_summary(exc_summary_type, 0, repr).unwrap();

    let raised = ExceptionRaised {
        timestamp_delta_ns: 42,
        line: 99,
        exception_type: exc_type,
        exception_value: exc_value,
    };
    w.write_exception_raised(raised.clone()).unwrap();

    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.events().len(), 1);
    assert_eq!(r.events()[0], Event::ExceptionRaised(raised));
    assert!(
        r.final_summary()
            .unwrap()
            .payload
            .contains("exception_events = 1")
    );
}

#[test]
fn round_trip_note_event_with_kwargs() {
    let mut w = TraceWriter::new(fixture_metadata());
    let msg = w.intern_string("processed batch");
    let count_name = w.intern_string("count");
    let status_name = w.intern_string("status");
    let count_value = w.intern_value_inline(Value::Int(42));
    let status_value = w.intern_value_inline(Value::String("ok".into()));

    let note = Note {
        timestamp_delta_ns: 10,
        line: 50,
        message: msg,
        kwargs: vec![
            Kwarg {
                name: count_name,
                value: count_value,
            },
            Kwarg {
                name: status_name,
                value: status_value,
            },
        ],
    };
    w.write_note(note.clone()).unwrap();

    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.events()[0], Event::Note(note));
    assert!(
        r.final_summary()
            .unwrap()
            .payload
            .contains("note_events = 1")
    );
}

#[test]
fn round_trip_note_event_without_kwargs() {
    let mut w = TraceWriter::new(fixture_metadata());
    let msg = w.intern_string("checkpoint");
    let note = Note {
        timestamp_delta_ns: 0,
        line: 1,
        message: msg,
        kwargs: vec![],
    };
    w.write_note(note.clone()).unwrap();

    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.events()[0], Event::Note(note));
}

#[test]
fn round_trip_scope_boundary_event() {
    let mut w = TraceWriter::new(fixture_metadata());
    let reason = w.intern_string("matched pattern: numpy.*");
    let entered = ScopeBoundary {
        timestamp_delta_ns: 1,
        boundary_type: BoundaryType::EnteredExcluded,
        reason,
    };
    let exited = ScopeBoundary {
        timestamp_delta_ns: 2,
        boundary_type: BoundaryType::ExitedExcluded,
        reason,
    };
    w.write_scope_boundary(entered.clone()).unwrap();
    w.write_scope_boundary(exited.clone()).unwrap();

    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.events()[0], Event::ScopeBoundary(entered));
    assert_eq!(r.events()[1], Event::ScopeBoundary(exited));
    assert!(
        r.final_summary()
            .unwrap()
            .payload
            .contains("scope_boundary_events = 2")
    );
}

#[test]
fn round_trip_frame_switch_event() {
    let mut w = TraceWriter::new(fixture_metadata());
    let switch = FrameSwitch {
        timestamp_delta_ns: 8,
        old_frame_id: 3,
        new_frame_id: 7,
        reason: FrameSwitchReason::GeneratorYield,
    };
    w.write_frame_switch(switch.clone()).unwrap();

    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.events()[0], Event::FrameSwitch(switch));
    assert!(
        r.final_summary()
            .unwrap()
            .payload
            .contains("frame_switch_events = 1")
    );
}

#[test]
fn round_trip_all_event_types_in_one_trace() {
    // One of each, exercises that the per-event-type counter doesn't get
    // wires crossed.
    let mut w = TraceWriter::new(fixture_metadata());
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    let arg = w.intern_string("a");
    let exc_type = w.intern_string("ValueError");
    let msg = w.intern_string("hi");
    let reason = w.intern_string("excluded");
    let v0 = NONE_VALUE_ID;

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: f,
        source_file_id: file_id,
        line: 1,
        args: vec![Argument {
            name: arg,
            value: v0,
        }],
    })
    .unwrap();
    w.write_frame_snapshot(FrameSnapshot {
        timestamp_delta_ns: 0,
        frame_id: 0,
        line: 1,
        locals: vec![],
    })
    .unwrap();
    w.write_line_delta(LineDelta {
        timestamp_delta_ns: 0,
        line: 2,
        changes: vec![],
    })
    .unwrap();
    w.write_branch_result(BranchResult {
        timestamp_delta_ns: 0,
        line: 3,
        taken: true,
    })
    .unwrap();
    w.write_exception_raised(ExceptionRaised {
        timestamp_delta_ns: 0,
        line: 4,
        exception_type: exc_type,
        exception_value: v0,
    })
    .unwrap();
    w.write_note(Note {
        timestamp_delta_ns: 0,
        line: 5,
        message: msg,
        kwargs: vec![],
    })
    .unwrap();
    w.write_scope_boundary(ScopeBoundary {
        timestamp_delta_ns: 0,
        boundary_type: BoundaryType::EnteredSkip,
        reason,
    })
    .unwrap();
    w.write_frame_switch(FrameSwitch {
        timestamp_delta_ns: 0,
        old_frame_id: 0,
        new_frame_id: 1,
        reason: FrameSwitchReason::AsyncTaskSwitch,
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 0,
        frame_id: 0,
        return_value: v0,
    })
    .unwrap();

    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.events().len(), 9);
    let summary = r.final_summary().unwrap();
    assert!(summary.payload.contains("function_entry_events = 1"));
    assert!(summary.payload.contains("function_exit_events = 1"));
    assert!(summary.payload.contains("frame_snapshot_events = 1"));
    assert!(summary.payload.contains("line_events = 1"));
    assert!(summary.payload.contains("branch_events = 1"));
    assert!(summary.payload.contains("exception_events = 1"));
    assert!(summary.payload.contains("note_events = 1"));
    assert!(summary.payload.contains("scope_boundary_events = 1"));
    assert!(summary.payload.contains("frame_switch_events = 1"));
    assert!(summary.payload.contains("total_events = 9"));
}

// ---------- finalization-specific tests ----------

#[test]
fn finalized_header_has_recording_end_and_footer_offset_patched() {
    let bytes = finalize(TraceWriter::new(fixture_metadata()));
    let recording_end = u64::from_le_bytes(
        bytes[HEADER_OFFSET_RECORDING_END..HEADER_OFFSET_RECORDING_END + 8]
            .try_into()
            .unwrap(),
    );
    let footer_offset = u64::from_le_bytes(
        bytes[HEADER_OFFSET_FOOTER_OFFSET..HEADER_OFFSET_FOOTER_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    assert_eq!(recording_end, RECORDING_END_NS);
    assert!(footer_offset > 0);
    assert_eq!(
        footer_offset as usize,
        bytes.len() - FOOTER_LENGTH as usize,
        "footer must be the last 32 bytes",
    );
}

#[test]
fn unfinalized_header_recording_end_stays_zero() {
    let bytes = unfinalized(TraceWriter::new(fixture_metadata()));
    let recording_end = u64::from_le_bytes(
        bytes[HEADER_OFFSET_RECORDING_END..HEADER_OFFSET_RECORDING_END + 8]
            .try_into()
            .unwrap(),
    );
    let footer_offset = u64::from_le_bytes(
        bytes[HEADER_OFFSET_FOOTER_OFFSET..HEADER_OFFSET_FOOTER_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    assert_eq!(recording_end, 0);
    assert_eq!(footer_offset, 0);
}

#[test]
fn unfinalized_trace_round_trip_reports_no_summary_or_footer() {
    let mut w = TraceWriter::new(fixture_metadata());
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: f,
        source_file_id: file_id,
        line: 1,
        args: vec![],
    })
    .unwrap();
    let bytes = unfinalized(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.events().len(), 1);
    assert!(!r.is_finalized());
    assert!(r.final_summary().is_none());
    assert!(r.footer().is_none());
    assert!(r.checkpoints().is_empty());
    assert_eq!(r.header().recording_end_ns, 0);
    assert_eq!(r.header().footer_offset, 0);
}

#[test]
fn truncated_finalized_trace_with_only_event_block_is_readable_as_unfinalized() {
    // Simulate a recorder that crashed *after* writing the event block but
    // *before* writing the final summary / footer. The header still says
    // unfinalized (recording_end = 0, footer_offset = 0) because we only
    // patch those at the very end. So truncating to the end of the event
    // block leaves a valid unfinalized trace.
    let bytes = unfinalized(TraceWriter::new(fixture_metadata()));
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert!(!r.is_finalized());
    assert!(r.events().is_empty());
    assert!(r.final_summary().is_none());
    assert!(r.footer().is_none());
}

#[test]
fn finalized_trace_with_scope_resolution_renders_excluded_functions() {
    let scope = ScopeResolution {
        recorded_functions: vec!["myapp.process".into(), "myapp.parse".into()],
        excluded_functions: vec![ExcludedFunction {
            name: "numpy.dot".into(),
            matched_pattern: "numpy.*".into(),
        }],
        skip_blocks_observed: 5,
        depth_clips_observed: 0,
    };
    let bytes = TraceWriter::new(fixture_metadata())
        .finish_to_bytes(Finalization {
            recording_end_ns: RECORDING_END_NS,
            scope_resolution: scope,
        })
        .unwrap();
    let r = TraceReader::from_bytes(&bytes).unwrap();
    let summary = r.final_summary().unwrap();
    assert!(summary.payload.contains("[final.scope_resolved]"));
    assert!(
        summary
            .payload
            .contains(r#"recorded_functions = ["myapp.process", "myapp.parse"]"#)
    );
    assert!(
        summary.payload.contains(
            r#"excluded_functions = [{ name = "numpy.dot", matched_pattern = "numpy.*" }]"#
        )
    );
    assert!(summary.payload.contains("skip_blocks_observed = 5"));
    assert!(summary.payload.contains("depth_clips_observed = 0"));
}

#[test]
fn footer_offsets_match_section_starts_in_finalized_trace() {
    let mut w = TraceWriter::new(fixture_metadata());
    // Add a tiny bit of stuff so the offsets aren't identical to the empty
    // trace.
    w.add_source_file("x.py", b"hi".to_vec());
    w.intern_string("k");
    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    let footer = r.footer().unwrap();
    // Sanity: both offsets fall inside the file and in the right order.
    assert!((footer.final_summary_offset as usize) < bytes.len());
    assert!((footer.checkpoint_index_offset as usize) < bytes.len());
    assert!(footer.final_summary_offset < footer.checkpoint_index_offset);
    assert!(footer.checkpoint_index_offset < r.header().footer_offset);
}

// ---------- error tests ----------

#[test]
fn malformed_header_wrong_magic() {
    let mut bytes = finalize(TraceWriter::new(fixture_metadata()));
    bytes[FILE_OFFSET_MAGIC] = b'X';
    let result = TraceReader::from_bytes(&bytes);
    assert!(matches!(result, Err(FormatError::BadMagic { .. })));
}

#[test]
fn malformed_header_wrong_version() {
    let mut bytes = finalize(TraceWriter::new(fixture_metadata()));
    bytes[FILE_OFFSET_VERSION] = 9;
    let result = TraceReader::from_bytes(&bytes);
    assert!(matches!(
        result,
        Err(FormatError::UnsupportedVersion { major: 9, minor: 3 })
    ));
}

#[test]
fn malformed_header_wrong_length() {
    let mut bytes = finalize(TraceWriter::new(fixture_metadata()));
    bytes[FILE_OFFSET_HEADER_LEN..FILE_OFFSET_HEADER_LEN + 4]
        .copy_from_slice(&128u32.to_le_bytes());
    let result = TraceReader::from_bytes(&bytes);
    assert!(matches!(
        result,
        Err(FormatError::BadHeaderLength {
            expected: 64,
            got: 128
        })
    ));
}

#[test]
fn malformed_header_nonzero_flags() {
    let mut bytes = finalize(TraceWriter::new(fixture_metadata()));
    bytes[FILE_OFFSET_HEADER_FLAGS] = 1;
    let result = TraceReader::from_bytes(&bytes);
    assert!(matches!(
        result,
        Err(FormatError::ReservedFieldNonzero("header flags"))
    ));
}

#[test]
fn truncated_file_header() {
    let bytes = finalize(TraceWriter::new(fixture_metadata()));
    let truncated = &bytes[..32];
    let result = TraceReader::from_bytes(truncated);
    assert!(matches!(result, Err(FormatError::Truncated)));
}

#[test]
fn truncated_mid_event_block_in_unfinalized_trace() {
    // For the event-block-truncation case we need an unfinalized trace,
    // because in a finalized trace the last bytes are the footer.
    let bytes = unfinalized(TraceWriter::new(fixture_metadata()));
    let truncated = &bytes[..bytes.len() - 5];
    let result = TraceReader::from_bytes(truncated);
    assert!(result.is_err(), "expected an error on truncated trace");
}

#[test]
fn checksum_mismatch_when_compressed_payload_corrupted() {
    let mut bytes = unfinalized(TraceWriter::new(fixture_metadata()));
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    let result = TraceReader::from_bytes(&bytes);
    assert!(matches!(result, Err(FormatError::ChecksumMismatch { .. })));
}

#[test]
fn checksum_mismatch_when_block_length_corrupted() {
    let mut bytes = finalize(TraceWriter::new(fixture_metadata()));
    let after_tables = locate_event_block_start(&bytes);
    bytes[after_tables] ^= 0xFF;
    let result = TraceReader::from_bytes(&bytes);
    assert!(
        result.is_err(),
        "expected an error after mangling block length"
    );
}

#[test]
fn nonexistent_source_file_id_in_function_entry() {
    let mut w = TraceWriter::new(fixture_metadata());
    let func_name = w.intern_string("f");
    let v0 = NONE_VALUE_ID;
    let arg_name = w.intern_string("x");
    let file_id = w.add_source_file("dummy.py", b"".to_vec());
    let entry = FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_name,
        source_file_id: file_id,
        line: 1,
        args: vec![Argument {
            name: arg_name,
            value: v0,
        }],
    };
    w.write_function_entry(entry).unwrap();
    // Use an unfinalized trace for tampering: shifting bundle bytes would
    // invalidate the footer offsets in a finalized file, causing the
    // reader's footer pre-peek to fail before it reaches the
    // FUNCTION_ENTRY's reference. Unfinalized has no footer to confuse.
    let bytes = unfinalized(w);

    TraceReader::from_bytes(&bytes).unwrap();

    // Tamper: rewrite the source bundle to be empty so the FUNCTION_ENTRY's
    // source_file_id reference dangles.
    let mut bad = bytes.clone();
    let bundle_offset = HEADER_LENGTH as usize + metadata_total_len(&bad);
    let bundle_len_field = u32::from_le_bytes([
        bad[bundle_offset],
        bad[bundle_offset + 1],
        bad[bundle_offset + 2],
        bad[bundle_offset + 3],
    ]);
    let bundle_body_start = bundle_offset + 4;
    let bundle_body_end = bundle_body_start + bundle_len_field as usize;
    let mut rewritten = Vec::new();
    rewritten.extend_from_slice(&bad[..bundle_offset]);
    rewritten.extend_from_slice(&4u32.to_le_bytes());
    rewritten.extend_from_slice(&0u32.to_le_bytes());
    rewritten.extend_from_slice(&bad[bundle_body_end..]);
    bad = rewritten;

    let result = TraceReader::from_bytes(&bad);
    assert!(
        matches!(result, Err(FormatError::UnknownFileId(0))),
        "expected UnknownFileId, got {result:?}"
    );
}

#[test]
fn nonexistent_value_id_in_function_exit() {
    let mut w = TraceWriter::new(fixture_metadata());
    let func = w.intern_string("f");
    let file_id = w.add_source_file("a.py", b"".to_vec());
    let v = w.intern_value_inline(Value::Int(7));
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func,
        source_file_id: file_id,
        line: 1,
        args: vec![],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: v,
    })
    .unwrap();
    // Same rationale as the source-file-id tampering test above: shifting
    // value-table bytes would invalidate the footer's offsets in a finalized
    // file, so the tamper-the-table test runs against an unfinalized trace.
    let bytes = unfinalized(w);

    let bad = corrupt_value_table_size(&bytes);
    let result = TraceReader::from_bytes(&bad);
    // Either error is acceptable: the value-ref check inside event parsing
    // fires first (UnknownValueId), but if the event block lacks a
    // referencing event the table-size-after cross-check (TableSizeMismatch)
    // would catch it. Both prove the corruption is rejected.
    assert!(
        matches!(
            result,
            Err(FormatError::UnknownValueId(_)) | Err(FormatError::TableSizeMismatch { .. })
        ),
        "expected UnknownValueId or TableSizeMismatch, got {result:?}"
    );
}

#[test]
fn corrupt_footer_magic_is_detected() {
    let mut bytes = finalize(TraceWriter::new(fixture_metadata()));
    let footer_start = bytes.len() - FOOTER_LENGTH as usize;
    bytes[footer_start] = b'X';
    let result = TraceReader::from_bytes(&bytes);
    assert!(matches!(result, Err(FormatError::BadFooterMagic { .. })));
}

#[test]
fn corrupt_footer_length_is_detected() {
    let mut bytes = finalize(TraceWriter::new(fixture_metadata()));
    let footer_start = bytes.len() - FOOTER_LENGTH as usize;
    bytes[footer_start + 8..footer_start + 12].copy_from_slice(&128u32.to_le_bytes());
    let result = TraceReader::from_bytes(&bytes);
    assert!(matches!(
        result,
        Err(FormatError::BadFooterLength {
            expected: 32,
            got: 128
        })
    ));
}

#[test]
fn header_footer_offset_pointing_past_end_is_detected() {
    let mut bytes = finalize(TraceWriter::new(fixture_metadata()));
    // Patch the header's footer_offset to a clearly-wrong value.
    let bogus_offset: u64 = (bytes.len() as u64) - 16; // 16 bytes too early
    bytes[HEADER_OFFSET_FOOTER_OFFSET..HEADER_OFFSET_FOOTER_OFFSET + 8]
        .copy_from_slice(&bogus_offset.to_le_bytes());
    let result = TraceReader::from_bytes(&bytes);
    // Either form of "footer offset is invalid" is acceptable: the reader
    // peeks the footer at the claimed offset to learn where the block
    // stream ends. A bogus offset can land on bytes that look like a
    // truncated/garbage footer (BadFooterMagic / BadFooterLength /
    // Truncated) or, if it survives the peek, surface as the cross-check
    // mismatch (HeaderFooterOffsetMismatch). The test's intent is only to
    // confirm the reader rejects the trace.
    assert!(
        matches!(
            result,
            Err(FormatError::HeaderFooterOffsetMismatch { .. })
                | Err(FormatError::BadFooterMagic { .. })
                | Err(FormatError::BadFooterLength { .. })
                | Err(FormatError::Truncated)
        ),
        "expected a footer-offset failure, got {result:?}"
    );
}

// ---------- helpers ----------

fn metadata_total_len(bytes: &[u8]) -> usize {
    let off = HEADER_LENGTH as usize;
    let len = u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
    4 + len as usize
}

fn source_bundle_total_len(bytes: &[u8], offset: usize) -> usize {
    let len = u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]);
    4 + len as usize
}

fn length_prefixed_total_len(bytes: &[u8], offset: usize) -> usize {
    let len = u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]);
    4 + len as usize
}

fn locate_event_block_start(bytes: &[u8]) -> usize {
    let mut off = HEADER_LENGTH as usize;
    off += metadata_total_len(bytes);
    off += source_bundle_total_len(bytes, off);
    off += length_prefixed_total_len(bytes, off);
    off += length_prefixed_total_len(bytes, off);
    off
}

fn corrupt_value_table_size(bytes: &[u8]) -> Vec<u8> {
    let mut off = HEADER_LENGTH as usize;
    off += metadata_total_len(bytes);
    off += source_bundle_total_len(bytes, off);
    off += length_prefixed_total_len(bytes, off);
    let value_table_off = off;
    let value_table_len = u32::from_le_bytes([
        bytes[value_table_off],
        bytes[value_table_off + 1],
        bytes[value_table_off + 2],
        bytes[value_table_off + 3],
    ]) as usize;
    let after_value_table = value_table_off + 4 + value_table_len;

    let mut out = Vec::new();
    out.extend_from_slice(&bytes[..value_table_off]);
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&bytes[after_value_table..]);
    out
}

/// Walks the post-prelude block stream of a finalized trace and counts the
/// number of blocks of each tag emitted into the file. Used by
/// multi-block round-trip tests.
fn count_blocks_by_tag(bytes: &[u8]) -> std::collections::HashMap<u8, usize> {
    use std::collections::HashMap;
    let mut counts: HashMap<u8, usize> = HashMap::new();

    // Locate the start of the block stream and the end (= final summary
    // offset, read from the footer).
    let mut off = HEADER_LENGTH as usize;
    off += metadata_total_len(bytes);
    off += source_bundle_total_len(bytes, off);
    off += length_prefixed_total_len(bytes, off);
    off += length_prefixed_total_len(bytes, off);
    let block_stream_start = off;

    let footer_offset = u64::from_le_bytes(
        bytes[HEADER_OFFSET_FOOTER_OFFSET..HEADER_OFFSET_FOOTER_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    let footer_start = footer_offset as usize;
    let final_summary_offset = u64::from_le_bytes(
        bytes[footer_start + 20..footer_start + 28]
            .try_into()
            .unwrap(),
    ) as usize;

    let mut pos = block_stream_start;
    while pos < final_summary_offset {
        let block_len =
            u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
        let tag = bytes[pos + 4];
        *counts.entry(tag).or_insert(0) += 1;
        pos += 4 + block_len as usize;
    }
    counts
}

// ---------- multi-block / table updates / snapshots / checkpoints ----------

/// Build a writer with a tiny event-block size so even short traces split
/// across multiple blocks. Other thresholds left at defaults — for the
/// short tests they won't trigger checkpoints.
fn small_blocks_config() -> WriterConfig {
    WriterConfig {
        event_block_size_bytes: 32,
        ..WriterConfig::default()
    }
}

#[test]
fn default_config_small_trace_emits_single_event_block() {
    // Backward-compat shape check: a trace small enough to fit under the
    // default 32 KiB threshold produces exactly one event block, zero
    // checkpoint/update/snapshot blocks. (If this ever fires for new
    // reasons, we've inadvertently broken byte shape for the common case.)
    let bytes = finalize(TraceWriter::new(fixture_metadata()));
    let counts = count_blocks_by_tag(&bytes);
    assert_eq!(counts.get(&BLOCK_TAG_EVENT).copied().unwrap_or(0), 1);
    assert_eq!(counts.get(&BLOCK_TAG_CHECKPOINT).copied().unwrap_or(0), 0);
    assert_eq!(counts.get(&BLOCK_TAG_TABLE_UPDATE).copied().unwrap_or(0), 0);
    assert_eq!(
        counts.get(&BLOCK_TAG_TABLE_SNAPSHOT).copied().unwrap_or(0),
        0
    );
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.event_blocks().len(), 1);
    // total_blocks counts event + final summary.
    assert!(
        r.final_summary()
            .unwrap()
            .payload
            .contains("total_blocks = 2"),
    );
}

#[test]
fn small_block_size_splits_events_across_multiple_blocks() {
    let mut w = TraceWriter::with_config(fixture_metadata(), small_blocks_config());
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let func = w.intern_string("f");

    // Each FUNCTION_ENTRY with no args is a few bytes; with the 32-byte
    // threshold we'll flush every couple of events.
    let n_events = 30;
    for i in 0..n_events {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: i,
            frame_id: i,
            function_id: func,
            source_file_id: file_id,
            line: 1 + (i as u32 % 100),
            args: vec![],
        })
        .unwrap();
    }

    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.events().len(), n_events as usize);
    let blocks = r.event_blocks();
    assert!(
        blocks.len() > 1,
        "expected multi-block split; got {} blocks",
        blocks.len()
    );

    // Block boundaries are consistent: each block's first_event_id +
    // event_count equals the next block's first_event_id, and totals match.
    let mut expected_id = 0u64;
    let mut total_events = 0u64;
    for blk in blocks {
        assert_eq!(blk.first_event_id, expected_id);
        expected_id += blk.event_count;
        total_events += blk.event_count;
    }
    assert_eq!(total_events as usize, r.events().len());
}

#[test]
fn intern_after_first_flush_emits_table_update_block() {
    // Force a flush after a single event by setting the threshold very low.
    // Then intern a new string and emit another event — the second event
    // block must be preceded by a table update block.
    let mut w = TraceWriter::with_config(
        fixture_metadata(),
        WriterConfig {
            event_block_size_bytes: 1, // force a flush after every event
            ..WriterConfig::default()
        },
    );
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f1 = w.intern_string("f1");

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: f1,
        source_file_id: file_id,
        line: 1,
        args: vec![],
    })
    .unwrap();

    // Now intern a new string after the first event has flushed.
    let f2 = w.intern_string("f2");
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 1,
        frame_id: 1,
        function_id: f2,
        source_file_id: file_id,
        line: 2,
        args: vec![],
    })
    .unwrap();

    let bytes = finalize(w);
    let counts = count_blocks_by_tag(&bytes);
    assert!(
        counts.get(&BLOCK_TAG_TABLE_UPDATE).copied().unwrap_or(0) >= 1,
        "expected at least one table update block, got counts {counts:?}"
    );

    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.events().len(), 2);
    assert_eq!(r.strings(), &["f1".to_string(), "f2".to_string()]);
}

#[test]
fn no_table_updates_when_nothing_new_interned_after_first_flush() {
    // Same forced-flush scenario, but interning happens entirely *before*
    // any events are recorded. The reader's "no 1:1 between event and
    // update blocks" expectation: zero update blocks, multiple event
    // blocks.
    let mut w = TraceWriter::with_config(
        fixture_metadata(),
        WriterConfig {
            event_block_size_bytes: 1,
            ..WriterConfig::default()
        },
    );
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");

    for i in 0..5u64 {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: i,
            frame_id: i,
            function_id: f,
            source_file_id: file_id,
            line: 1,
            args: vec![],
        })
        .unwrap();
    }

    let bytes = finalize(w);
    let counts = count_blocks_by_tag(&bytes);
    assert_eq!(
        counts.get(&BLOCK_TAG_TABLE_UPDATE).copied().unwrap_or(0),
        0,
        "no update blocks when nothing new interned after flush; counts {counts:?}",
    );
    assert!(counts.get(&BLOCK_TAG_EVENT).copied().unwrap_or(0) > 1);
}

#[test]
fn checkpoints_emitted_at_event_count_interval() {
    // Force checkpoints every 2 events; default event_block_size keeps each
    // recording-loop iteration in one block boundary check.
    let mut w = TraceWriter::with_config(
        fixture_metadata(),
        WriterConfig {
            event_block_size_bytes: 1, // flush per event so checkpoint check fires often
            checkpoint_interval_events: 2,
            checkpoint_interval_ns: u64::MAX,
            snapshot_interval_checkpoints: u32::MAX, // suppress snapshots
        },
    );
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    for i in 0..6u64 {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: 1,
            frame_id: i,
            function_id: f,
            source_file_id: file_id,
            line: 1,
            args: vec![],
        })
        .unwrap();
    }

    let bytes = finalize(w);
    let counts = count_blocks_by_tag(&bytes);
    let n_checkpoints = counts.get(&BLOCK_TAG_CHECKPOINT).copied().unwrap_or(0);
    // 6 events / 2 per checkpoint = 3 checkpoints.
    assert_eq!(n_checkpoints, 3, "block counts: {counts:?}");

    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert_eq!(r.checkpoints().len(), 3);
    // Each checkpoint's event_id is non-decreasing and bounded by event count.
    let mut prev = 0u64;
    for cp in r.checkpoints() {
        assert!(cp.event_id >= prev);
        prev = cp.event_id;
    }
    assert!(prev <= r.events().len() as u64);
}

#[test]
fn checkpoints_emitted_at_wall_clock_interval() {
    // Use ns-only trigger; events have small per-event deltas so we can hit
    // the threshold at known points.
    let mut w = TraceWriter::with_config(
        fixture_metadata(),
        WriterConfig {
            event_block_size_bytes: 1,
            checkpoint_interval_events: u64::MAX, // suppress event-count trigger
            checkpoint_interval_ns: 100,
            snapshot_interval_checkpoints: u32::MAX,
        },
    );
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    // Each event adds 50ns. After 2 events = 100ns → checkpoint.
    for i in 0..6u64 {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: 50,
            frame_id: i,
            function_id: f,
            source_file_id: file_id,
            line: 1,
            args: vec![],
        })
        .unwrap();
    }
    let bytes = finalize(w);
    let counts = count_blocks_by_tag(&bytes);
    assert_eq!(
        counts.get(&BLOCK_TAG_CHECKPOINT).copied().unwrap_or(0),
        3,
        "expected 3 checkpoints (every 100ns over 6×50ns), counts {counts:?}",
    );
}

#[test]
fn snapshots_emitted_every_n_checkpoints() {
    // Checkpoints every 1 event; snapshot every 2 checkpoints.
    let mut w = TraceWriter::with_config(
        fixture_metadata(),
        WriterConfig {
            event_block_size_bytes: 1,
            checkpoint_interval_events: 1,
            checkpoint_interval_ns: u64::MAX,
            snapshot_interval_checkpoints: 2,
        },
    );
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    for i in 0..6u64 {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: 1,
            frame_id: i,
            function_id: f,
            source_file_id: file_id,
            line: 1,
            args: vec![],
        })
        .unwrap();
    }
    let bytes = finalize(w);
    let counts = count_blocks_by_tag(&bytes);
    let n_checkpoints = counts.get(&BLOCK_TAG_CHECKPOINT).copied().unwrap_or(0);
    let n_snapshots = counts.get(&BLOCK_TAG_TABLE_SNAPSHOT).copied().unwrap_or(0);
    assert_eq!(n_checkpoints, 6, "{counts:?}");
    // Snapshot fires when checkpoints_since_last_snapshot >= 2: between
    // checkpoint 2 and checkpoint 3 (1st snapshot), then between checkpoint
    // 4 and 5 (2nd snapshot), and between checkpoint 6 and... but trace
    // ends. So 2 snapshots.
    assert_eq!(n_snapshots, 2, "{counts:?}");

    let r = TraceReader::from_bytes(&bytes).unwrap();
    // Reader applied snapshots without the table contents drifting.
    assert_eq!(r.strings().len(), 1);
    assert_eq!(r.events().len(), 6);
}

#[test]
fn snapshot_offsets_in_checkpoint_index_match_emitted_snapshots() {
    let mut w = TraceWriter::with_config(
        fixture_metadata(),
        WriterConfig {
            event_block_size_bytes: 1,
            checkpoint_interval_events: 1,
            checkpoint_interval_ns: u64::MAX,
            snapshot_interval_checkpoints: 2,
        },
    );
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    for i in 0..5u64 {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: 1,
            frame_id: i,
            function_id: f,
            source_file_id: file_id,
            line: 1,
            args: vec![],
        })
        .unwrap();
    }
    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    let cps = r.checkpoints();
    // At least the first checkpoint should have snapshot_offset == 0
    // (sentinel for "use initial tables" — no snapshot yet).
    assert_eq!(cps[0].snapshot_offset, 0);
    // Some later checkpoint should have a non-zero snapshot_offset, and
    // when non-zero the byte at that offset should be a block-length u32
    // followed by the snapshot tag.
    let cp_with_snapshot = cps
        .iter()
        .find(|cp| cp.snapshot_offset != 0)
        .expect("at least one checkpoint with explicit snapshot");
    let off = cp_with_snapshot.snapshot_offset as usize;
    assert_eq!(bytes[off + 4], BLOCK_TAG_TABLE_SNAPSHOT);
}

// ---------- seeking ----------

#[test]
fn seek_to_event_id_uses_index_anchor_when_available() {
    let mut w = TraceWriter::with_config(
        fixture_metadata(),
        WriterConfig {
            event_block_size_bytes: 1,
            checkpoint_interval_events: 2,
            checkpoint_interval_ns: u64::MAX,
            snapshot_interval_checkpoints: u32::MAX,
        },
    );
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    for i in 0..10u64 {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: 1,
            frame_id: i,
            function_id: f,
            source_file_id: file_id,
            line: 1,
            args: vec![],
        })
        .unwrap();
    }
    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    assert!(!r.checkpoints().is_empty());

    // Seek to event 5 — should use the checkpoint at event_id <= 5 as
    // anchor.
    let cursor = r.seek_to_event_id(5).unwrap();
    assert_eq!(cursor.first_event_id(), Some(5));
    let anchor = cursor.seek_anchor().expect("seek used a checkpoint");
    assert!(anchor.event_id <= 5);
    // The cursor's slice begins at index 5.
    assert_eq!(cursor.start_index(), 5);
    assert_eq!(cursor.events().len(), 5);
}

#[test]
fn seek_to_event_id_zero_anchor_is_none() {
    let mut w = TraceWriter::with_config(
        fixture_metadata(),
        WriterConfig {
            event_block_size_bytes: 1,
            checkpoint_interval_events: 2,
            checkpoint_interval_ns: u64::MAX,
            snapshot_interval_checkpoints: u32::MAX,
        },
    );
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    for i in 0..5u64 {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: 1,
            frame_id: i,
            function_id: f,
            source_file_id: file_id,
            line: 1,
            args: vec![],
        })
        .unwrap();
    }
    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();

    // Seek to event 0 — no checkpoint is at event_id <= 0 except possibly
    // none (since checkpoints fire after events have been recorded), so
    // seek_anchor should be None.
    let cursor = r.seek_to_event_id(0).unwrap();
    assert_eq!(cursor.first_event_id(), Some(0));
    assert_eq!(cursor.start_index(), 0);
    // Either no checkpoints precede event 0 or the writer's earliest
    // checkpoint is at event_id > 0; either way the seek uses no anchor.
    assert!(cursor.seek_anchor().is_none());
}

#[test]
fn seek_to_event_id_past_end_errors() {
    let mut w = TraceWriter::new(fixture_metadata());
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    for i in 0..3u64 {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: 1,
            frame_id: i,
            function_id: f,
            source_file_id: file_id,
            line: 1,
            args: vec![],
        })
        .unwrap();
    }
    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    let result = r.seek_to_event_id(99);
    assert!(matches!(result, Err(FormatError::SeekPastEnd { .. })));
}

#[test]
fn seek_to_wall_clock_uses_index_anchor() {
    let mut w = TraceWriter::with_config(
        fixture_metadata(),
        WriterConfig {
            event_block_size_bytes: 1,
            checkpoint_interval_events: 2,
            checkpoint_interval_ns: u64::MAX,
            snapshot_interval_checkpoints: u32::MAX,
        },
    );
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    for i in 0..10u64 {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: 100,
            frame_id: i,
            function_id: f,
            source_file_id: file_id,
            line: 1,
            args: vec![],
        })
        .unwrap();
    }
    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    let recording_start = r.header().recording_start_ns;
    // Event 5 has wall-clock = recording_start + 6 * 100 = +600ns. Seek
    // there — wall clock target +500 should land at or just past event 5.
    let target = recording_start + 500;
    let cursor = r.seek_to_wall_clock(target).unwrap();
    assert!(cursor.start_index() <= 5, "should not overshoot event 5");
    let anchor = cursor.seek_anchor().expect("seek used a checkpoint");
    assert!(anchor.wall_clock_ns <= target);
}

#[test]
fn seek_anchor_proves_index_was_consulted() {
    // A buggy seek that always returns from index 0 would still produce
    // events()[0] = first event. But the cursor's seek_anchor would be
    // None, exposing the bug. This test is the canary for that.
    let mut w = TraceWriter::with_config(
        fixture_metadata(),
        WriterConfig {
            event_block_size_bytes: 1,
            checkpoint_interval_events: 3,
            checkpoint_interval_ns: u64::MAX,
            snapshot_interval_checkpoints: u32::MAX,
        },
    );
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    for i in 0..9u64 {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: 1,
            frame_id: i,
            function_id: f,
            source_file_id: file_id,
            line: 1,
            args: vec![],
        })
        .unwrap();
    }
    let bytes = finalize(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    // Seek to event 6 — there should be a checkpoint at event_id <= 6.
    let cursor = r.seek_to_event_id(6).unwrap();
    let anchor_idx = cursor.seek_anchor_index().expect("anchor index present");
    assert!(anchor_idx > 0, "anchor checkpoint should not be the first");
    let anchor = &r.checkpoints()[anchor_idx];
    assert!(
        anchor.event_id <= 6,
        "anchor.event_id ({}) must be <= seek target (6)",
        anchor.event_id
    );
}

// ---------- total_blocks reflects all-block-types accounting ----------

#[test]
fn total_blocks_includes_checkpoints_updates_and_snapshots() {
    let mut w = TraceWriter::with_config(
        fixture_metadata(),
        WriterConfig {
            event_block_size_bytes: 1,
            checkpoint_interval_events: 1,
            checkpoint_interval_ns: u64::MAX,
            snapshot_interval_checkpoints: 2,
        },
    );
    let file_id = w.add_source_file("x.py", b"".to_vec());
    let f = w.intern_string("f");
    for i in 0..4u64 {
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: 1,
            frame_id: i,
            function_id: f,
            source_file_id: file_id,
            line: 1,
            args: vec![],
        })
        .unwrap();
    }
    let bytes = finalize(w);
    let counts = count_blocks_by_tag(&bytes);
    let observed_total: usize = counts.values().sum();
    let r = TraceReader::from_bytes(&bytes).unwrap();
    let summary = r.final_summary().unwrap();
    // total_blocks = sum of all tagged blocks + 1 for the final summary.
    let expected_total = observed_total + 1;
    let needle = format!("total_blocks = {expected_total}");
    assert!(
        summary.payload.contains(&needle),
        "expected `{needle}` in summary; got:\n{}",
        summary.payload
    );
}
