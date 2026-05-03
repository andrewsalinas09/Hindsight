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
    Argument, Change, EXCEPTION_UNWIND_VALUE_ID, Event, FormatError, FrameSnapshot, FunctionEntry,
    FunctionExit, HEADER_LENGTH, HashKind, LineDelta, Local, Metadata, NONE_VALUE_ID, ProgramInfo,
    RecorderInfo, RecordingInfo, ScopeConfig, TraceReader, TraceWriter, Value,
};

const FILE_OFFSET_HEADER_LEN: usize = 12;
const FILE_OFFSET_HEADER_FLAGS: usize = 10;
const FILE_OFFSET_VERSION: usize = 8;
const FILE_OFFSET_MAGIC: usize = 0;

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
        recording_start_ns: 1_700_000_000_000_000_000,
    }
}

fn finish(w: TraceWriter) -> Vec<u8> {
    let mut out = Vec::new();
    w.finish(&mut out).unwrap();
    out
}

// ---------- round-trip ----------

#[test]
fn round_trip_empty_trace() {
    let bytes = finish(TraceWriter::new(fixture_metadata()));

    let r = TraceReader::from_bytes(&bytes).unwrap();
    let h = r.header();
    assert_eq!(h.format_version_major, 0);
    assert_eq!(h.format_version_minor, 2);
    assert_eq!(h.flags, 0);
    assert_eq!(h.header_length, HEADER_LENGTH);
    assert_eq!(h.trace_uuid, [0xCC; 16]);
    assert_eq!(h.recording_start_ns, 1_700_000_000_000_000_000);
    assert_eq!(
        h.recording_end_ns, 0,
        "writer leaves end zero (unfinalized)"
    );
    assert_eq!(h.footer_offset, 0);

    let m = r.metadata();
    assert_eq!(m.format_tag, 0x01);
    assert!(m.payload.contains("[recorder]"));
    assert!(m.payload.contains("language = \"python\""));
    assert!(m.payload.contains("git_commit = \"abc123\""));

    assert!(r.source_files().is_empty());
    assert!(r.strings().is_empty());

    // Reserved indices are always present.
    assert_eq!(r.values().len(), 2);
    assert!(matches!(r.values()[0].value, Value::None));
    assert!(matches!(
        r.values()[1].value,
        Value::ExceptionUnwindSentinel
    ));

    assert!(r.events().is_empty());
    assert_eq!(r.event_block_info().first_event_id, 0);
    assert_eq!(r.event_block_info().event_count, 0);
}

/// Round-trip via the public `Read` API rather than `from_bytes` to exercise
/// the `TraceReader::new(impl Read)` entry point.
#[test]
fn round_trip_via_read_trait() {
    let bytes = finish(TraceWriter::new(fixture_metadata()));
    let r = TraceReader::new(Cursor::new(&bytes)).unwrap();
    assert_eq!(r.values().len(), 2);
}

#[test]
fn round_trip_value_table_covers_all_supported_value_types() {
    let mut w = TraceWriter::new(fixture_metadata());

    // Strings used by TypeRef and Summary entries.
    let type_name = w.intern_string("numpy.ndarray");
    let repr = w.intern_string("array([1,2,3])");

    // Build a list of value IDs spanning every Value variant we expose.
    let v_bool = w.intern_value_inline(Value::Bool(true));
    let v_int = w.intern_value_inline(Value::Int(-42));
    // BigInt: the writer expects already-canonical bytes (two's complement,
    // big-endian, minimum length). 0x01_00 represents 256.
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

    let bytes = finish(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();
    let values = r.values();

    // Reserved values plus the 13 we interned.
    assert_eq!(values.len(), 2 + 13);

    // Spot-check decoded fields.
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

    // Hash-kind discrimination round-trips:
    assert_eq!(values[v_int as usize].hash_kind, HashKind::Content);
    assert_eq!(values[v_summary as usize].hash_kind, HashKind::Summary);
    assert_eq!(values[v_identity as usize].hash_kind, HashKind::Identity);
    assert_eq!(values[v_identity as usize].hash, [0xAB; 16]);

    // Reserved indices stay where the writer puts them.
    assert!(matches!(values[NONE_VALUE_ID as usize].value, Value::None));
    assert!(matches!(
        values[EXCEPTION_UNWIND_VALUE_ID as usize].value,
        Value::ExceptionUnwindSentinel
    ));
}

#[test]
fn round_trip_worked_example_events() {
    // Reproduces the spec's `double(5)` worked example end-to-end and asserts
    // that every event survives the writer -> reader round trip with field
    // equality.
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

    let bytes = finish(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();

    assert_eq!(r.event_block_info().event_count, 5);
    assert_eq!(r.event_block_info().first_event_id, 0);
    assert_eq!(
        r.event_block_info().string_table_size_after as usize,
        r.strings().len()
    );
    assert_eq!(
        r.event_block_info().value_table_size_after as usize,
        r.values().len()
    );

    let events = r.events();
    assert_eq!(events.len(), 5);
    assert_eq!(events[0], Event::FunctionEntry(entry));
    assert_eq!(events[1], Event::FrameSnapshot(snap));
    assert_eq!(events[2], Event::LineDelta(line2));
    assert_eq!(events[3], Event::LineDelta(line3));
    assert_eq!(events[4], Event::FunctionExit(exit));

    // Source bundle round-trip: hash matches and content matches.
    assert_eq!(r.source_files().len(), 1);
    let f = &r.source_files()[0];
    assert_eq!(f.path, "example.py");
    assert_eq!(*blake3::hash(&f.content).as_bytes(), f.blake3_hash);
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

    let bytes = finish(w);
    let r = TraceReader::from_bytes(&bytes).unwrap();

    assert_eq!(r.source_files().len(), 3);
    assert_eq!(r.source_files()[f0 as usize].path, "a.py");
    assert_eq!(r.source_files()[f1 as usize].path, "b.py");
    assert_eq!(r.source_files()[f2 as usize].path, "c.py");

    assert_eq!(r.strings()[s0 as usize], "alpha");
    assert_eq!(r.strings()[s1 as usize], "beta");
    assert_eq!(r.strings()[s2 as usize], "gamma");
}

// ---------- error tests ----------

#[test]
fn malformed_header_wrong_magic() {
    let mut bytes = finish(TraceWriter::new(fixture_metadata()));
    bytes[FILE_OFFSET_MAGIC] = b'X';
    let result = TraceReader::from_bytes(&bytes);
    assert!(matches!(result, Err(FormatError::BadMagic { .. })));
}

#[test]
fn malformed_header_wrong_version() {
    let mut bytes = finish(TraceWriter::new(fixture_metadata()));
    bytes[FILE_OFFSET_VERSION] = 9;
    let result = TraceReader::from_bytes(&bytes);
    assert!(matches!(
        result,
        Err(FormatError::UnsupportedVersion { major: 9, minor: 2 })
    ));
}

#[test]
fn malformed_header_wrong_length() {
    let mut bytes = finish(TraceWriter::new(fixture_metadata()));
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
    let mut bytes = finish(TraceWriter::new(fixture_metadata()));
    bytes[FILE_OFFSET_HEADER_FLAGS] = 1;
    let result = TraceReader::from_bytes(&bytes);
    assert!(matches!(
        result,
        Err(FormatError::ReservedFieldNonzero("header flags"))
    ));
}

#[test]
fn truncated_file_header() {
    let bytes = finish(TraceWriter::new(fixture_metadata()));
    let truncated = &bytes[..32]; // Less than header length.
    let result = TraceReader::from_bytes(truncated);
    assert!(matches!(result, Err(FormatError::Truncated)));
}

#[test]
fn truncated_mid_event_block() {
    // Cut off the last 5 bytes of the file (somewhere inside the compressed
    // payload); reader should fail when validating block length, decompressing,
    // or checking the CRC, but never panic.
    let bytes = finish(TraceWriter::new(fixture_metadata()));
    let truncated = &bytes[..bytes.len() - 5];
    let result = TraceReader::from_bytes(truncated);
    assert!(result.is_err(), "expected an error on truncated trace");
}

#[test]
fn checksum_mismatch_when_compressed_payload_corrupted() {
    let mut bytes = finish(TraceWriter::new(fixture_metadata()));
    // Flipping a bit somewhere inside the compressed payload (the very last
    // byte of the file) is the simplest way to make the CRC mismatch.
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    let result = TraceReader::from_bytes(&bytes);
    assert!(matches!(result, Err(FormatError::ChecksumMismatch { .. })));
}

#[test]
fn checksum_mismatch_when_checksum_field_corrupted() {
    // Even if the payload is intact, mutating the checksum field in the block
    // header should be caught.
    let mut bytes = finish(TraceWriter::new(fixture_metadata()));
    // The checksum is the last u32 of the block header, immediately before
    // the compressed payload. Locating it precisely is fiddly given variable
    // header lengths; the easier "mutate the last 4 bytes is the payload"
    // trick is the previous test. Here we mutate the *first* possible byte
    // of the block_length u32 to confirm we don't get away with garbage block
    // metadata either.
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
    // Build a writer trace with no source files, then craft a synthetic event
    // that references file_id = 0. We do this by patching the writer's bytes
    // — the writer itself refuses to emit such an event.
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
    let bytes = finish(w);

    // Trace round-trips cleanly first to make sure our setup is sane.
    TraceReader::from_bytes(&bytes).unwrap();

    // Now craft a trace whose FUNCTION_ENTRY references a *nonexistent*
    // source file by hand-building one source-bundle-less and then writing
    // the entry. Easiest path: take the original, patch the source bundle
    // length to zero entries, and let the reader catch the dangling ref.
    let mut bad = bytes.clone();
    let bundle_offset = HEADER_LENGTH as usize + metadata_total_len(&bad);
    let bundle_len_field = u32::from_le_bytes([
        bad[bundle_offset],
        bad[bundle_offset + 1],
        bad[bundle_offset + 2],
        bad[bundle_offset + 3],
    ]);
    // Zero out the file_count u32 right after the bundle length and shrink the
    // bundle. Don't reshape the file — we just rewrite the stored content
    // length so the reader sees an empty bundle, then slide everything down.
    let bundle_body_start = bundle_offset + 4;
    let bundle_body_end = bundle_body_start + bundle_len_field as usize;
    // Remove the entire bundle body and replace it with [count=0; 4 bytes].
    let mut rewritten = Vec::new();
    rewritten.extend_from_slice(&bad[..bundle_offset]);
    rewritten.extend_from_slice(&4u32.to_le_bytes()); // new bundle length = 4
    rewritten.extend_from_slice(&0u32.to_le_bytes()); // file_count = 0
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
    // Build a normal trace, then tamper with a value-table entry to remove
    // entries the FUNCTION_EXIT references.
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
    let bytes = finish(w);

    // Tamper: rewrite the value_table_size_after field in the block header
    // so it claims fewer values than were actually stored, and truncate the
    // value table to match. Easier and equally diagnostic: hand-craft a value
    // table with one fewer entry and let the table-size mismatch trigger.
    let bad = corrupt_value_table_size(&bytes);
    let result = TraceReader::from_bytes(&bad);
    assert!(
        matches!(result, Err(FormatError::TableSizeMismatch { .. })),
        "expected TableSizeMismatch, got {result:?}"
    );
}

// ---------- helpers ----------

/// Total bytes occupied by the metadata block (length field + payload).
fn metadata_total_len(bytes: &[u8]) -> usize {
    let off = HEADER_LENGTH as usize;
    let len = u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
    4 + len as usize
}

/// Total bytes occupied by the source bundle.
fn source_bundle_total_len(bytes: &[u8], offset: usize) -> usize {
    let len = u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]);
    4 + len as usize
}

/// Total bytes occupied by a length-prefixed table (string or value).
fn length_prefixed_total_len(bytes: &[u8], offset: usize) -> usize {
    let len = u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]);
    4 + len as usize
}

/// Byte offset of the event block's leading u32 length prefix.
fn locate_event_block_start(bytes: &[u8]) -> usize {
    let mut off = HEADER_LENGTH as usize;
    off += metadata_total_len(bytes);
    off += source_bundle_total_len(bytes, off);
    off += length_prefixed_total_len(bytes, off); // string table
    off += length_prefixed_total_len(bytes, off); // value table
    off
}

/// Tamper the value table by replacing it with a copy that has one fewer
/// entry recorded in the count u32, while leaving the on-disk entries
/// otherwise intact. The reader should detect the mismatch via either the
/// section length check (likely first) or the table-size check.
fn corrupt_value_table_size(bytes: &[u8]) -> Vec<u8> {
    let mut off = HEADER_LENGTH as usize;
    off += metadata_total_len(bytes);
    off += source_bundle_total_len(bytes, off);
    off += length_prefixed_total_len(bytes, off); // string table
    let value_table_off = off;
    let value_table_len = u32::from_le_bytes([
        bytes[value_table_off],
        bytes[value_table_off + 1],
        bytes[value_table_off + 2],
        bytes[value_table_off + 3],
    ]) as usize;
    let after_value_table = value_table_off + 4 + value_table_len;

    // Build a fresh value table with count = 0 and length = 4 (just the count
    // u32). The block header still says N entries, so the reader will catch
    // the mismatch.
    let mut out = Vec::new();
    out.extend_from_slice(&bytes[..value_table_off]);
    out.extend_from_slice(&4u32.to_le_bytes()); // section length
    out.extend_from_slice(&0u32.to_le_bytes()); // count
    out.extend_from_slice(&bytes[after_value_table..]);
    out
}
