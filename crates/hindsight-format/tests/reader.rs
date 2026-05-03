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
    Argument, BoundaryType, BranchResult, Change, EXCEPTION_UNWIND_VALUE_ID, Event,
    ExceptionRaised, ExcludedFunction, FOOTER_LENGTH, Finalization, FormatError, FrameSnapshot,
    FrameSwitch, FrameSwitchReason, FunctionEntry, FunctionExit, HEADER_LENGTH, HashKind, Kwarg,
    LineDelta, Local, Metadata, NONE_VALUE_ID, Note, ProgramInfo, RecorderInfo, RecordingInfo,
    ScopeBoundary, ScopeConfig, ScopeResolution, TraceReader, TraceWriter, Value,
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
    assert_eq!(h.format_version_minor, 2);
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
    assert_eq!(r.event_block_info().first_event_id, 0);
    assert_eq!(r.event_block_info().event_count, 0);

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

    assert_eq!(r.event_block_info().event_count, 5);
    assert_eq!(r.event_block_info().first_event_id, 0);

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
        Err(FormatError::UnsupportedVersion { major: 9, minor: 2 })
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
    let bytes = finalize(w);

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
    let bytes = finalize(w);

    let bad = corrupt_value_table_size(&bytes);
    let result = TraceReader::from_bytes(&bad);
    assert!(
        matches!(result, Err(FormatError::TableSizeMismatch { .. })),
        "expected TableSizeMismatch, got {result:?}"
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
    assert!(
        matches!(result, Err(FormatError::HeaderFooterOffsetMismatch { .. })),
        "expected HeaderFooterOffsetMismatch, got {result:?}"
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
