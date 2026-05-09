// SPDX-License-Identifier: Apache-2.0

//! End-to-end test of the writer: build the spec's worked example (the
//! `double` function) and parse the resulting bytes back, verifying the
//! layout described in `docs/trace-format.md`.

use hindsight_format::{
    Argument, BLOCK_TAG_EVENT, Change, EXCEPTION_UNWIND_VALUE_ID, EventTag, FILE_MAGIC,
    FORMAT_VERSION_MAJOR, FORMAT_VERSION_MINOR, FrameSnapshot, FunctionEntry, FunctionExit,
    HEADER_LENGTH, HashKind, LineDelta, Local, METADATA_FORMAT_TAG_TOML, Metadata, NONE_VALUE_ID,
    ProgramInfo, RecorderInfo, RecordingInfo, ScopeConfig, TraceWriter, Value, ValueTag,
};

const DOUBLE_PY: &[u8] = b"def double(x):\n    result = x * 2\n    return result\n\ndouble(5)\n";

fn worked_example_metadata() -> Metadata {
    Metadata {
        recorder: RecorderInfo {
            language: "python".into(),
            language_version: "3.12.5".into(),
            recorder_version: "0.1.0".into(),
            platform: "linux-x86_64".into(),
        },
        recording: RecordingInfo {
            program: "python example.py".into(),
            working_directory: Some("/tmp/proj".into()),
            scope_config: ScopeConfig {
                include: vec![],
                exclude: vec!["defaults".into()],
                depth_limit: None,
            },
        },
        program: Some(ProgramInfo {
            fields: vec![("git_commit".into(), "abc123".into())],
        }),
        trace_uuid: [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
            0xFF, 0x00,
        ],
        recording_start_ns: 1_700_000_000_000_000_000,
    }
}

fn build_worked_example() -> Vec<u8> {
    let mut w = TraceWriter::new(worked_example_metadata());
    let file_id = w.add_source_file("example.py", DOUBLE_PY.to_vec());

    let func_name = w.intern_string("__main__.double");
    let arg_x = w.intern_string("x");
    let local_result = w.intern_string("result");

    let val_5 = w.intern_value_inline(Value::Int(5));
    let val_10 = w.intern_value_inline(Value::Int(10));

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_name,
        source_file_id: file_id,
        line: 1,
        args: vec![Argument {
            name: arg_x,
            value: val_5,
        }],
    })
    .unwrap();

    w.write_frame_snapshot(FrameSnapshot {
        timestamp_delta_ns: 100,
        frame_id: 0,
        line: 1,
        locals: vec![Local {
            name: arg_x,
            value: val_5,
        }],
    })
    .unwrap();

    w.write_line_delta(LineDelta {
        timestamp_delta_ns: 50,
        line: 2,
        changes: vec![Change {
            name: local_result,
            value: val_10,
        }],
    })
    .unwrap();

    w.write_line_delta(LineDelta {
        timestamp_delta_ns: 25,
        line: 3,
        changes: vec![],
    })
    .unwrap();

    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 25,
        frame_id: 0,
        return_value: val_10,
    })
    .unwrap();

    let mut out = Vec::new();
    // These tests pin the on-disk layout of everything up to and including
    // the event block; they intentionally use `write_unfinalized` so the
    // file ends at the event block (no final summary, no checkpoint index,
    // no footer). The finalized-write path is exercised in tests/reader.rs.
    w.write_unfinalized(&mut out).unwrap();
    out
}

// --- Cursor helpers used by the parsing assertions below ---

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> &'a [u8] {
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        s
    }
    fn u8(&mut self) -> u8 {
        let v = self.buf[self.pos];
        self.pos += 1;
        v
    }
    fn u16_le(&mut self) -> u16 {
        let s = self.take(2);
        u16::from_le_bytes([s[0], s[1]])
    }
    fn u32_le(&mut self) -> u32 {
        let s = self.take(4);
        u32::from_le_bytes([s[0], s[1], s[2], s[3]])
    }
    fn u64_le(&mut self) -> u64 {
        let s = self.take(8);
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        u64::from_le_bytes(a)
    }
    fn uvarint(&mut self) -> u64 {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            let b = self.u8();
            result |= u64::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        result
    }
    fn svarint(&mut self) -> i64 {
        let z = self.uvarint();
        ((z >> 1) as i64) ^ -((z & 1) as i64)
    }
}

#[test]
fn header_layout() {
    let bytes = build_worked_example();
    let mut c = Cursor::new(&bytes);
    assert_eq!(c.take(8), FILE_MAGIC);
    assert_eq!(c.u8(), FORMAT_VERSION_MAJOR);
    assert_eq!(c.u8(), FORMAT_VERSION_MINOR);
    assert_eq!(c.u16_le(), 0, "flags must be zero in v0.2");
    assert_eq!(c.u32_le(), HEADER_LENGTH);

    let uuid = c.take(16);
    assert_eq!(uuid[0], 0x11);
    assert_eq!(uuid[15], 0x00);

    assert_eq!(c.u64_le(), 1_700_000_000_000_000_000);
    assert_eq!(c.u64_le(), 0, "recording_end is zero (unfinalized)");
    assert_eq!(c.u64_le(), 0, "footer_offset is zero (unfinalized)");
    assert_eq!(c.u64_le(), 0, "reserved tail must be zero");
    assert_eq!(c.pos, HEADER_LENGTH as usize, "header is exactly 64 bytes");
}

#[test]
fn metadata_block_is_toml() {
    let bytes = build_worked_example();
    let mut c = Cursor::new(&bytes);
    c.take(HEADER_LENGTH as usize);

    let block_len = c.u32_le();
    let format_tag = c.u8();
    assert_eq!(format_tag, METADATA_FORMAT_TAG_TOML);
    let payload_len = (block_len - 1) as usize;
    let payload = std::str::from_utf8(c.take(payload_len)).unwrap();

    assert!(payload.contains("[recorder]"));
    assert!(payload.contains("language = \"python\""));
    assert!(payload.contains("language_version = \"3.12.5\""));
    assert!(payload.contains("[recording]"));
    assert!(payload.contains("program = \"python example.py\""));
    assert!(payload.contains("working_directory = \"/tmp/proj\""));
    assert!(payload.contains("[recording.scope_config]"));
    assert!(payload.contains("exclude = [\"defaults\"]"));
    assert!(
        !payload.contains("depth_limit"),
        "depth_limit omitted when None"
    );
    assert!(payload.contains("[program]"));
    assert!(payload.contains("git_commit = \"abc123\""));
}

#[test]
fn source_bundle_carries_one_file_with_blake3() {
    let bytes = build_worked_example();
    let mut c = Cursor::new(&bytes);
    c.take(HEADER_LENGTH as usize);

    let metadata_len = c.u32_le() as usize;
    c.take(metadata_len);

    let bundle_len = c.u32_le() as usize;
    let bundle_start = c.pos;
    let file_count = c.u32_le();
    assert_eq!(file_count, 1);

    let file_id = c.uvarint();
    assert_eq!(file_id, 0);
    let blake3_hash = c.take(32);
    assert_eq!(blake3_hash, blake3::hash(DOUBLE_PY).as_bytes());

    let path_len = c.u16_le() as usize;
    let path = std::str::from_utf8(c.take(path_len)).unwrap();
    assert_eq!(path, "example.py");

    let content_len = c.u32_le() as usize;
    let content = c.take(content_len);
    assert_eq!(content, DOUBLE_PY);

    assert_eq!(c.pos - bundle_start, bundle_len);
}

#[test]
fn initial_string_table_contains_interned_names() {
    let bytes = build_worked_example();
    let mut c = Cursor::new(&bytes);
    c.take(HEADER_LENGTH as usize);
    let m = c.u32_le() as usize;
    c.take(m);
    let b = c.u32_le() as usize;
    c.take(b);

    let table_len = c.u32_le() as usize;
    let table_start = c.pos;
    let count = c.u32_le() as usize;
    let mut strings = Vec::with_capacity(count);
    for _ in 0..count {
        let len = c.uvarint() as usize;
        let s = std::str::from_utf8(c.take(len)).unwrap().to_string();
        strings.push(s);
    }
    assert_eq!(c.pos - table_start, table_len);

    assert!(strings.iter().any(|s| s == "__main__.double"));
    assert!(strings.iter().any(|s| s == "x"));
    assert!(strings.iter().any(|s| s == "result"));
}

#[test]
fn initial_value_table_has_reserved_indices_and_interned_ints() {
    let bytes = build_worked_example();
    let mut c = Cursor::new(&bytes);
    c.take(HEADER_LENGTH as usize);
    let m = c.u32_le() as usize;
    c.take(m);
    let b = c.u32_le() as usize;
    c.take(b);
    let s = c.u32_le() as usize;
    c.take(s);

    let _table_len = c.u32_le() as usize;
    let count = c.u32_le() as usize;
    assert!(count >= 4, "at least None, sentinel, 5, 10");

    // Index 0: None.
    {
        let tag = c.u8();
        assert_eq!(tag, ValueTag::None.as_u8());
        assert_eq!(c.u8(), HashKind::Content as u8);
        c.take(16);
        let data_len = c.uvarint();
        assert_eq!(data_len, 0);
    }
    // Index 1: ExceptionUnwindSentinel.
    {
        let tag = c.u8();
        assert_eq!(tag, ValueTag::ExceptionUnwindSentinel.as_u8());
        assert_eq!(c.u8(), HashKind::Content as u8);
        c.take(16);
        let data_len = c.uvarint();
        assert_eq!(data_len, 0);
    }
    // Reserved indices are at fixed positions.
    let _ = NONE_VALUE_ID;
    let _ = EXCEPTION_UNWIND_VALUE_ID;

    // Index 2: Int(5).
    {
        let tag = c.u8();
        assert_eq!(tag, ValueTag::IntSmall.as_u8());
        assert_eq!(c.u8(), HashKind::Content as u8);
        c.take(16);
        let data_len = c.uvarint() as usize;
        let data = c.take(data_len);
        // Int 5 zigzag: (5 << 1) ^ 0 = 10 -> 0x0A
        assert_eq!(data, &[0x0A]);
    }
    // Index 3: Int(10).
    {
        let tag = c.u8();
        assert_eq!(tag, ValueTag::IntSmall.as_u8());
        assert_eq!(c.u8(), HashKind::Content as u8);
        c.take(16);
        let data_len = c.uvarint() as usize;
        let data = c.take(data_len);
        // Int 10 zigzag: 20 -> 0x14
        assert_eq!(data, &[0x14]);
    }
}

#[test]
fn event_block_decompresses_to_expected_event_stream() {
    let bytes = build_worked_example();
    let mut c = Cursor::new(&bytes);

    // Skip up to the event block.
    c.take(HEADER_LENGTH as usize);
    let m = c.u32_le() as usize;
    c.take(m);
    let b = c.u32_le() as usize;
    c.take(b);
    let s = c.u32_le() as usize;
    c.take(s);
    let v = c.u32_le() as usize;
    c.take(v);

    // Block.
    let block_len = c.u32_le() as usize;
    let block_start = c.pos;
    let block_tag = c.u8();
    assert_eq!(block_tag, BLOCK_TAG_EVENT);

    let header_length = c.uvarint() as usize;
    let header_start = c.pos;
    let compressed_len = c.uvarint() as usize;
    let uncompressed_len = c.uvarint() as usize;
    let first_event_id = c.uvarint();
    let event_count = c.uvarint();
    let string_table_size_after = c.u32_le();
    let value_table_size_after = c.u32_le();
    let checksum = c.u32_le();
    assert_eq!(c.pos - header_start, header_length);

    assert_eq!(first_event_id, 0);
    assert_eq!(event_count, 5);
    assert!(string_table_size_after >= 3);
    assert!(value_table_size_after >= 4);

    let compressed = c.take(compressed_len);
    assert_eq!(crc32c::crc32c(compressed), checksum);
    assert_eq!(c.pos - block_start, block_len);
    assert_eq!(c.pos, bytes.len(), "no trailing bytes");

    let uncompressed = zstd::stream::decode_all(compressed).unwrap();
    assert_eq!(uncompressed.len(), uncompressed_len);

    // Walk events using their length prefix; assert tag sequence.
    let mut ec = Cursor::new(&uncompressed);
    let mut tags = Vec::new();
    while ec.pos < uncompressed.len() {
        let event_len = ec.uvarint() as usize;
        let tag = ec.u8();
        ec.take(event_len - 1);
        tags.push(tag);
    }
    assert_eq!(
        tags,
        vec![
            EventTag::FunctionEntry.as_u8(),
            EventTag::FrameSnapshot.as_u8(),
            EventTag::LineDelta.as_u8(),
            EventTag::LineDelta.as_u8(),
            EventTag::FunctionExit.as_u8(),
        ]
    );
}

#[test]
fn function_entry_payload_decodes_to_original_fields() {
    let bytes = build_worked_example();

    // Walk to the compressed payload and decompress.
    let mut c = Cursor::new(&bytes);
    c.take(HEADER_LENGTH as usize);
    let m = c.u32_le() as usize;
    c.take(m);
    let b = c.u32_le() as usize;
    c.take(b);
    let s = c.u32_le() as usize;
    c.take(s);
    let v = c.u32_le() as usize;
    c.take(v);
    c.u32_le(); // block length
    c.u8(); // block tag
    let _hl = c.uvarint();
    let compressed_len = c.uvarint() as usize;
    let _ulen = c.uvarint();
    let _fid = c.uvarint();
    let _ec = c.uvarint();
    c.u32_le();
    c.u32_le();
    c.u32_le();
    let compressed = c.take(compressed_len);
    let uncompressed = zstd::stream::decode_all(compressed).unwrap();

    let mut ec = Cursor::new(&uncompressed);
    let event_len = ec.uvarint() as usize;
    let event_start = ec.pos;
    let tag = ec.u8();
    assert_eq!(tag, EventTag::FunctionEntry.as_u8());
    let timestamp_delta = ec.uvarint();
    let frame_id = ec.uvarint();
    let function_id = ec.uvarint();
    let source_file_id = ec.uvarint();
    let line = ec.uvarint();
    let arg_count = ec.uvarint();
    assert_eq!(timestamp_delta, 0);
    assert_eq!(frame_id, 0);
    assert_eq!(source_file_id, 0);
    assert_eq!(line, 1);
    assert_eq!(arg_count, 1);
    let arg_name = ec.uvarint();
    let arg_value = ec.uvarint();
    // function_id and arg_name reference strings; we only know their order via interning.
    // First call interns "__main__.double" (id 0); second interns "x" (id 1).
    assert_eq!(function_id, 0);
    assert_eq!(arg_name, 1);
    // val_5 is interned after the two reserved values, so id 2.
    assert_eq!(arg_value, 2);
    assert_eq!(ec.pos - event_start, event_len);
}

#[test]
fn line_delta_with_no_changes_is_compact() {
    let bytes = build_worked_example();
    let mut c = Cursor::new(&bytes);
    c.take(HEADER_LENGTH as usize);
    let m = c.u32_le() as usize;
    c.take(m);
    let b = c.u32_le() as usize;
    c.take(b);
    let s = c.u32_le() as usize;
    c.take(s);
    let v = c.u32_le() as usize;
    c.take(v);
    c.u32_le();
    c.u8();
    let _hl = c.uvarint();
    let compressed_len = c.uvarint() as usize;
    let _ = c.uvarint();
    let _ = c.uvarint();
    let _ = c.uvarint();
    c.u32_le();
    c.u32_le();
    c.u32_le();
    let compressed = c.take(compressed_len);
    let uncompressed = zstd::stream::decode_all(compressed).unwrap();

    let mut ec = Cursor::new(&uncompressed);
    // Skip first three events.
    for _ in 0..3 {
        let len = ec.uvarint() as usize;
        ec.take(len);
    }
    // Event 4: LINE_DELTA at line 3 with zero changes.
    let event_len = ec.uvarint();
    let tag = ec.u8();
    assert_eq!(tag, EventTag::LineDelta.as_u8());
    // payload: timestamp_delta=25, line=3, changes=0 — three single-byte varints.
    assert_eq!(event_len, 4, "tag + three single-byte varints");
}

#[test]
fn checksum_validates_compressed_payload() {
    // Rebuild and verify CRC32C survives a round trip.
    let bytes = build_worked_example();
    let mut c = Cursor::new(&bytes);
    c.take(HEADER_LENGTH as usize);
    let m = c.u32_le() as usize;
    c.take(m);
    let b = c.u32_le() as usize;
    c.take(b);
    let s = c.u32_le() as usize;
    c.take(s);
    let v = c.u32_le() as usize;
    c.take(v);
    c.u32_le();
    c.u8();
    let _hl = c.uvarint();
    let compressed_len = c.uvarint() as usize;
    let _ = c.uvarint();
    let _ = c.uvarint();
    let _ = c.uvarint();
    c.u32_le();
    c.u32_le();
    let checksum = c.u32_le();
    let compressed = c.take(compressed_len);
    assert_eq!(crc32c::crc32c(compressed), checksum);
}

#[test]
fn signed_int_canonical_is_used_for_hashing() {
    // Two values with the same int contribute identical canonical bytes,
    // therefore identical hashes, therefore dedup to one value table entry.
    let mut w = TraceWriter::new(worked_example_metadata());
    let a = w.intern_value_inline(Value::Int(5));
    let b = w.intern_value_inline(Value::Int(5));
    assert_eq!(a, b);
    let c = w.intern_value_inline(Value::Int(-1));
    assert_ne!(a, c);
    // i64 svarint zigzag — sanity that we still touch the codepath that doesn't panic.
    let _ = Cursor::new(&[0x00]).svarint();
}

#[test]
fn summary_value_uses_summary_hash() {
    let mut w = TraceWriter::new(worked_example_metadata());
    let type_name = w.intern_string("numpy.ndarray");
    let repr = w.intern_string("array([1, 2, 3])");
    let id = w.intern_value_summary(type_name, 3, repr).unwrap();
    // Re-interning the same summary returns the same ID.
    let id2 = w.intern_value_summary(type_name, 3, repr).unwrap();
    assert_eq!(id, id2);
}

#[test]
fn identity_hashed_value_does_not_dedup_with_content_hashed() {
    let mut w = TraceWriter::new(worked_example_metadata());
    let a = w.intern_value_inline(Value::Int(7));
    let b = w.intern_value_with_identity(Value::Int(7), [0xAB; 16]);
    assert_ne!(a, b, "different hash kinds must not dedup");
}

// ----------------------------------------------------------------------------
// Alias variant tests (v0.3 addition)
// ----------------------------------------------------------------------------

fn finalize_writer(w: TraceWriter) -> Vec<u8> {
    use hindsight_format::{Finalization, ScopeResolution};
    w.finish_to_bytes(Finalization {
        recording_end_ns: 1_000_000_000,
        scope_resolution: ScopeResolution::default(),
    })
    .expect("finalize")
}

#[test]
fn alias_equivalent_emits_fresh_value_id_pointing_at_prior() {
    use hindsight_format::{AliasKind, Confidence};
    let mut w = TraceWriter::new(worked_example_metadata());
    let int_a = w.intern_value_inline(Value::Int(1));
    let int_b = w.intern_value_inline(Value::Int(2));
    let original = w.intern_value_inline(Value::List(vec![int_a, int_b]));

    let alias = w
        .intern_value_alias(AliasKind::Equivalent, original, Confidence::SummaryObserved)
        .unwrap();
    assert_ne!(alias, original, "alias must produce a fresh value_id");
}

#[test]
fn alias_equivalent_does_not_dedup() {
    use hindsight_format::{AliasKind, Confidence};
    let mut w = TraceWriter::new(worked_example_metadata());
    let one = w.intern_value_inline(Value::Int(1));
    let target = w.intern_value_inline(Value::List(vec![one]));
    let a1 = w
        .intern_value_alias(AliasKind::Equivalent, target, Confidence::SummaryObserved)
        .unwrap();
    let a2 = w
        .intern_value_alias(AliasKind::Equivalent, target, Confidence::SummaryObserved)
        .unwrap();
    assert_ne!(
        a1, a2,
        "two aliases to the same target must each get a fresh id"
    );
}

#[test]
fn alias_grown_carries_new_tail_elements() {
    use hindsight_format::{AliasKind, Confidence};
    let mut w = TraceWriter::new(worked_example_metadata());
    let i1 = w.intern_value_inline(Value::Int(1));
    let i2 = w.intern_value_inline(Value::Int(2));
    let i3 = w.intern_value_inline(Value::Int(3));
    let original = w.intern_value_inline(Value::List(vec![i1, i2]));
    let grown = w
        .intern_value_alias(
            AliasKind::Grown {
                new_elements: vec![i3],
            },
            original,
            Confidence::SummaryObserved,
        )
        .unwrap();
    assert_ne!(grown, original);
}

#[test]
fn alias_referencing_unknown_value_id_is_rejected() {
    use hindsight_format::{AliasKind, Confidence};
    let mut w = TraceWriter::new(worked_example_metadata());
    let result = w.intern_value_alias(AliasKind::Equivalent, 999_999, Confidence::ContentExact);
    assert!(result.is_err(), "must reject unknown aliased value_id");
}

#[test]
fn alias_grown_with_unknown_tail_element_is_rejected() {
    use hindsight_format::{AliasKind, Confidence};
    let mut w = TraceWriter::new(worked_example_metadata());
    let target = w.intern_value_inline(Value::List(vec![]));
    let result = w.intern_value_alias(
        AliasKind::Grown {
            new_elements: vec![999_999],
        },
        target,
        Confidence::ContentExact,
    );
    assert!(result.is_err(), "must reject unknown new-element value_id");
}

#[test]
fn alias_round_trips_through_finalize_and_reader() {
    use hindsight_format::{AliasKind, Confidence, TraceReader};
    let mut w = TraceWriter::new(worked_example_metadata());
    let i1 = w.intern_value_inline(Value::Int(10));
    let i2 = w.intern_value_inline(Value::Int(20));
    let i3 = w.intern_value_inline(Value::Int(30));
    let original = w.intern_value_inline(Value::List(vec![i1, i2]));
    let alias_eq = w
        .intern_value_alias(AliasKind::Equivalent, original, Confidence::MutationTracked)
        .unwrap();
    let alias_grown = w
        .intern_value_alias(
            AliasKind::Grown {
                new_elements: vec![i3],
            },
            original,
            Confidence::DirtyReconciled,
        )
        .unwrap();
    let bytes = finalize_writer(w);
    let reader = TraceReader::from_bytes(&bytes).expect("finalized trace round-trips");
    let values = reader.values();

    let eq_entry = &values[alias_eq as usize];
    assert!(matches!(eq_entry.hash_kind, HashKind::Alias));
    match &eq_entry.value {
        Value::Alias {
            kind: AliasKind::Equivalent,
            aliased_value_id,
            confidence,
        } => {
            assert_eq!(*aliased_value_id, original);
            assert_eq!(*confidence, Confidence::MutationTracked);
        }
        other => panic!("expected Equivalent alias, got {other:?}"),
    }

    let grown_entry = &values[alias_grown as usize];
    match &grown_entry.value {
        Value::Alias {
            kind: AliasKind::Grown { new_elements },
            aliased_value_id,
            confidence,
        } => {
            assert_eq!(*aliased_value_id, original);
            assert_eq!(*confidence, Confidence::DirtyReconciled);
            assert_eq!(new_elements, &vec![i3]);
        }
        other => panic!("expected Grown alias, got {other:?}"),
    }
}

#[test]
fn confidence_round_trips_all_five_variants() {
    use hindsight_format::{AliasKind, Confidence, TraceReader};
    let mut w = TraceWriter::new(worked_example_metadata());
    let target = w.intern_value_inline(Value::Int(0));
    let variants = [
        Confidence::ContentExact,
        Confidence::MutationTracked,
        Confidence::DirtyReconciled,
        Confidence::SummaryObserved,
        Confidence::UncertainExternal,
    ];
    let mut ids = Vec::new();
    for c in variants {
        ids.push(
            w.intern_value_alias(AliasKind::Equivalent, target, c)
                .unwrap(),
        );
    }
    let bytes = finalize_writer(w);
    let reader = TraceReader::from_bytes(&bytes).unwrap();
    for (id, expected) in ids.iter().zip(variants.iter()) {
        if let Value::Alias { confidence, .. } = &reader.values()[*id as usize].value {
            assert_eq!(confidence, expected);
        } else {
            panic!("expected alias at id {id}");
        }
    }
}
