// SPDX-License-Identifier: Apache-2.0

//! Buffered writer that produces a `.hindsight` trace file.
//!
//! Scope for this revision: file header, initial metadata, source bundle,
//! initial string and value tables, and one event block containing
//! FUNCTION_ENTRY, FUNCTION_EXIT, FRAME_SNAPSHOT, and LINE_DELTA events.
//! Footer, final summary, checkpoints, table updates, and table snapshots
//! are not yet written; the resulting file is a valid but unfinalized trace
//! (header `recording_end` and `footer_offset` are zero).

use std::collections::HashMap;
use std::io::Write;

use crate::canonical::{
    CanonicalLookup, canonical_identity, canonical_inline, canonical_summary, hash_bytes,
};
use crate::encoding::write_value_data;
use crate::error::{FormatError, Result};
use crate::event::{Event, FrameSnapshot, FunctionEntry, FunctionExit, LineDelta, write_event};
use crate::metadata::Metadata;
use crate::value::{HashKind, StringId, Value, ValueId, ValueTag};
use crate::varint::{uvarint_len, write_uvarint};

pub type FileId = u64;

/// File magic for `.hindsight` files. ASCII `"HNDSGHT\0"`.
pub const FILE_MAGIC: [u8; 8] = *b"HNDSGHT\0";

pub const FORMAT_VERSION_MAJOR: u8 = 0;
pub const FORMAT_VERSION_MINOR: u8 = 2;
pub const HEADER_LENGTH: u32 = 64;

/// Format tag for the metadata block payload (TOML in v0.2).
pub const METADATA_FORMAT_TAG_TOML: u8 = 0x01;

/// Block tag for an event block.
pub const BLOCK_TAG_EVENT: u8 = 0x01;

/// Reserved value table indices, per spec.
pub const NONE_VALUE_ID: ValueId = 0;
pub const EXCEPTION_UNWIND_VALUE_ID: ValueId = 1;

#[derive(Debug)]
struct StoredValue {
    value: Value,
    encoded_data: Vec<u8>,
    canonical_repr: Vec<u8>,
    hash_kind: HashKind,
    hash: [u8; 16],
}

#[derive(Debug)]
struct SourceFile {
    file_id: FileId,
    path: String,
    content: Vec<u8>,
    blake3_hash: [u8; 32],
}

/// Dedup key for value entries. Includes the type tag because two values with
/// different tags can have identical canonical bytes (e.g., Bool(false) and
/// Int(0) both canonicalize to `[0]`; None and ExceptionUnwindSentinel both
/// canonicalize to empty).
type ValueKey = (ValueTag, HashKind, [u8; 16]);

/// Buffered trace writer. Build up sources, strings, values, and events; call
/// [`finish`](Self::finish) to emit the file.
pub struct TraceWriter {
    metadata: Metadata,

    sources: Vec<SourceFile>,
    source_lookup: HashMap<String, FileId>,

    strings: Vec<String>,
    string_lookup: HashMap<String, StringId>,

    values: Vec<StoredValue>,
    value_lookup: HashMap<ValueKey, ValueId>,

    events: Vec<Event>,
}

impl CanonicalLookup for TraceWriter {
    fn value_canonical(&self, id: ValueId) -> &[u8] {
        &self.values[id as usize].canonical_repr
    }
    fn string(&self, id: StringId) -> &str {
        &self.strings[id as usize]
    }
}

impl TraceWriter {
    pub fn new(metadata: Metadata) -> Self {
        let mut w = Self {
            metadata,
            sources: Vec::new(),
            source_lookup: HashMap::new(),
            strings: Vec::new(),
            string_lookup: HashMap::new(),
            values: Vec::new(),
            value_lookup: HashMap::new(),
            events: Vec::new(),
        };
        w.populate_reserved_values();
        w
    }

    fn populate_reserved_values(&mut self) {
        let none_id = self.intern_value_inline(Value::None);
        debug_assert_eq!(none_id, NONE_VALUE_ID);
        let sentinel_id = self.intern_value_unique(
            Value::ExceptionUnwindSentinel,
            HashKind::Content,
            hash_bytes(&[]),
            Vec::new(),
        );
        debug_assert_eq!(sentinel_id, EXCEPTION_UNWIND_VALUE_ID);
    }

    /// Add a source file. Returns the file ID. Duplicate paths return the
    /// existing ID without overwriting content.
    pub fn add_source_file(
        &mut self,
        path: impl Into<String>,
        content: impl Into<Vec<u8>>,
    ) -> FileId {
        let path = path.into();
        if let Some(&id) = self.source_lookup.get(&path) {
            return id;
        }
        let content = content.into();
        let blake3_hash = *blake3::hash(&content).as_bytes();
        let file_id = self.sources.len() as FileId;
        self.source_lookup.insert(path.clone(), file_id);
        self.sources.push(SourceFile {
            file_id,
            path,
            content,
            blake3_hash,
        });
        file_id
    }

    /// Intern a string. Same content returns the same ID.
    pub fn intern_string(&mut self, s: impl Into<String>) -> StringId {
        let s = s.into();
        if let Some(&id) = self.string_lookup.get(&s) {
            return id;
        }
        let id = self.strings.len() as StringId;
        self.string_lookup.insert(s.clone(), id);
        self.strings.push(s);
        id
    }

    /// Intern an inline value (hash kind = Content). Container variants must
    /// reference value IDs that are already interned.
    pub fn intern_value_inline(&mut self, value: Value) -> ValueId {
        if matches!(value, Value::Summary { .. }) {
            panic!("Summary values must be interned via intern_value_summary");
        }
        self.assert_child_ids_exist(&value);
        let canonical = canonical_inline(&value, self as &dyn CanonicalLookup);
        let hash = hash_bytes(&canonical);
        self.intern_value_unique(value, HashKind::Content, hash, canonical)
    }

    /// Intern a summary value (hash kind = Summary).
    pub fn intern_value_summary(
        &mut self,
        type_name: StringId,
        length: u64,
        repr: StringId,
    ) -> Result<ValueId> {
        self.expect_string(type_name)?;
        self.expect_string(repr)?;
        let canonical = canonical_summary(type_name, length, repr, self as &dyn CanonicalLookup);
        let hash = hash_bytes(&canonical);
        Ok(self.intern_value_unique(
            Value::Summary {
                type_name,
                length,
                repr,
            },
            HashKind::Summary,
            hash,
            canonical,
        ))
    }

    /// Intern a value with a caller-provided identity hash (hash kind = Identity).
    pub fn intern_value_with_identity(&mut self, value: Value, identity_hash: [u8; 16]) -> ValueId {
        self.assert_child_ids_exist(&value);
        let canonical = canonical_identity(&identity_hash);
        self.intern_value_unique(value, HashKind::Identity, identity_hash, canonical)
    }

    fn intern_value_unique(
        &mut self,
        value: Value,
        hash_kind: HashKind,
        hash: [u8; 16],
        canonical_repr: Vec<u8>,
    ) -> ValueId {
        let tag = value.tag();
        let key: ValueKey = (tag, hash_kind, hash);
        if let Some(&id) = self.value_lookup.get(&key) {
            return id;
        }
        let mut encoded_data = Vec::new();
        write_value_data(&mut encoded_data, &value).expect("write to Vec<u8> never fails");
        let id = self.values.len() as ValueId;
        self.value_lookup.insert(key, id);
        self.values.push(StoredValue {
            value,
            encoded_data,
            canonical_repr,
            hash_kind,
            hash,
        });
        id
    }

    fn assert_child_ids_exist(&self, value: &Value) {
        match value {
            Value::List(ids) | Value::Set(ids) => {
                for id in ids {
                    assert!(
                        (*id as usize) < self.values.len(),
                        "child ValueId {id} not yet interned",
                    );
                }
            }
            Value::Dict(pairs) => {
                for (k, v) in pairs {
                    assert!(
                        (*k as usize) < self.values.len(),
                        "key ValueId {k} not interned",
                    );
                    assert!(
                        (*v as usize) < self.values.len(),
                        "value ValueId {v} not interned",
                    );
                }
            }
            Value::TypeRef(id) => {
                assert!(
                    (*id as usize) < self.strings.len(),
                    "type ref StringId {id} not interned",
                );
            }
            _ => {}
        }
    }

    fn expect_string(&self, id: StringId) -> Result<()> {
        if (id as usize) < self.strings.len() {
            Ok(())
        } else {
            Err(FormatError::UnknownStringId(id))
        }
    }

    fn expect_value(&self, id: ValueId) -> Result<()> {
        if (id as usize) < self.values.len() {
            Ok(())
        } else {
            Err(FormatError::UnknownValueId(id))
        }
    }

    fn expect_file(&self, id: FileId) -> Result<()> {
        if (id as usize) < self.sources.len() {
            Ok(())
        } else {
            Err(FormatError::UnknownFileId(id))
        }
    }

    pub fn write_function_entry(&mut self, e: FunctionEntry) -> Result<()> {
        self.expect_string(e.function_id)?;
        self.expect_file(e.source_file_id)?;
        for arg in &e.args {
            self.expect_string(arg.name)?;
            self.expect_value(arg.value)?;
        }
        self.events.push(Event::FunctionEntry(e));
        Ok(())
    }

    pub fn write_function_exit(&mut self, e: FunctionExit) -> Result<()> {
        self.expect_value(e.return_value)?;
        self.events.push(Event::FunctionExit(e));
        Ok(())
    }

    pub fn write_frame_snapshot(&mut self, e: FrameSnapshot) -> Result<()> {
        for local in &e.locals {
            self.expect_string(local.name)?;
            self.expect_value(local.value)?;
        }
        self.events.push(Event::FrameSnapshot(e));
        Ok(())
    }

    pub fn write_line_delta(&mut self, e: LineDelta) -> Result<()> {
        for change in &e.changes {
            self.expect_string(change.name)?;
            self.expect_value(change.value)?;
        }
        self.events.push(Event::LineDelta(e));
        Ok(())
    }

    /// Emit the full `.hindsight` byte stream.
    pub fn finish<W: Write>(self, mut w: W) -> Result<()> {
        self.write_header(&mut w)?;
        self.write_metadata_block(&mut w)?;
        self.write_source_bundle(&mut w)?;
        self.write_initial_string_table(&mut w)?;
        self.write_initial_value_table(&mut w)?;
        self.write_event_block(&mut w)?;
        Ok(())
    }

    fn write_header<W: Write>(&self, w: &mut W) -> Result<()> {
        let mut header = [0u8; 64];
        header[0..8].copy_from_slice(&FILE_MAGIC);
        header[8] = FORMAT_VERSION_MAJOR;
        header[9] = FORMAT_VERSION_MINOR;
        // Flags (10..12), reserved zero in v0.2.
        header[12..16].copy_from_slice(&HEADER_LENGTH.to_le_bytes());
        header[16..32].copy_from_slice(&self.metadata.trace_uuid);
        header[32..40].copy_from_slice(&self.metadata.recording_start_ns.to_le_bytes());
        // recording_end (40..48): zero — file is unfinalized.
        // footer_offset (48..56): zero — no footer in this revision.
        // Reserved (56..64): zero.
        w.write_all(&header)?;
        Ok(())
    }

    fn write_metadata_block<W: Write>(&self, w: &mut W) -> Result<()> {
        let payload = self.metadata.to_toml();
        let payload_bytes = payload.as_bytes();
        let length: u32 = (1 + payload_bytes.len())
            .try_into()
            .map_err(|_| FormatError::MetadataTooLarge(1 + payload_bytes.len()))?;
        w.write_all(&length.to_le_bytes())?;
        w.write_all(&[METADATA_FORMAT_TAG_TOML])?;
        w.write_all(payload_bytes)?;
        Ok(())
    }

    fn write_source_bundle<W: Write>(&self, w: &mut W) -> Result<()> {
        let mut body = Vec::new();
        body.extend_from_slice(&(self.sources.len() as u32).to_le_bytes());
        for source in &self.sources {
            self.write_source_file_entry(&mut body, source)?;
        }
        let length: u32 = body
            .len()
            .try_into()
            .map_err(|_| FormatError::SectionTooLarge(body.len()))?;
        w.write_all(&length.to_le_bytes())?;
        w.write_all(&body)?;
        Ok(())
    }

    fn write_source_file_entry(&self, w: &mut Vec<u8>, source: &SourceFile) -> Result<()> {
        write_uvarint(w, source.file_id).expect("Vec write");
        w.extend_from_slice(&source.blake3_hash);
        let path_len: u16 = source
            .path
            .len()
            .try_into()
            .map_err(|_| FormatError::PathTooLong(source.path.len()))?;
        w.extend_from_slice(&path_len.to_le_bytes());
        w.extend_from_slice(source.path.as_bytes());
        let content_len: u32 = source
            .content
            .len()
            .try_into()
            .map_err(|_| FormatError::SourceTooLong(source.content.len()))?;
        w.extend_from_slice(&content_len.to_le_bytes());
        w.extend_from_slice(&source.content);
        Ok(())
    }

    fn write_initial_string_table<W: Write>(&self, w: &mut W) -> Result<()> {
        let mut body = Vec::new();
        body.extend_from_slice(&(self.strings.len() as u32).to_le_bytes());
        for s in &self.strings {
            write_uvarint(&mut body, s.len() as u64).expect("Vec write");
            body.extend_from_slice(s.as_bytes());
        }
        let length: u32 = body
            .len()
            .try_into()
            .map_err(|_| FormatError::SectionTooLarge(body.len()))?;
        w.write_all(&length.to_le_bytes())?;
        w.write_all(&body)?;
        Ok(())
    }

    fn write_initial_value_table<W: Write>(&self, w: &mut W) -> Result<()> {
        let mut body = Vec::new();
        body.extend_from_slice(&(self.values.len() as u32).to_le_bytes());
        for v in &self.values {
            write_value_table_entry(&mut body, v);
        }
        let length: u32 = body
            .len()
            .try_into()
            .map_err(|_| FormatError::SectionTooLarge(body.len()))?;
        w.write_all(&length.to_le_bytes())?;
        w.write_all(&body)?;
        Ok(())
    }

    fn write_event_block<W: Write>(&self, w: &mut W) -> Result<()> {
        let mut uncompressed = Vec::new();
        for event in &self.events {
            write_event(&mut uncompressed, event).expect("Vec write");
        }
        let uncompressed_len = uncompressed.len() as u64;

        let compressed = zstd::stream::encode_all(&uncompressed[..], 3)
            .map_err(|e| FormatError::Compression(e.to_string()))?;
        let compressed_len = compressed.len() as u64;
        let checksum = crc32c::crc32c(&compressed);

        let mut header = Vec::new();
        write_uvarint(&mut header, compressed_len).expect("Vec write");
        write_uvarint(&mut header, uncompressed_len).expect("Vec write");
        write_uvarint(&mut header, /* first_event_id */ 0).expect("Vec write");
        write_uvarint(&mut header, self.events.len() as u64).expect("Vec write");
        header.extend_from_slice(&(self.strings.len() as u32).to_le_bytes());
        header.extend_from_slice(&(self.values.len() as u32).to_le_bytes());
        header.extend_from_slice(&checksum.to_le_bytes());
        let header_length = header.len() as u64;

        let block_length = 1 + uvarint_len(header_length) + header.len() + compressed.len();
        let block_length: u32 = block_length
            .try_into()
            .map_err(|_| FormatError::SectionTooLarge(block_length))?;

        w.write_all(&block_length.to_le_bytes())?;
        w.write_all(&[BLOCK_TAG_EVENT])?;
        write_uvarint(w, header_length)?;
        w.write_all(&header)?;
        w.write_all(&compressed)?;
        Ok(())
    }
}

fn write_value_table_entry(w: &mut Vec<u8>, v: &StoredValue) {
    w.push(v.value.tag().as_u8());
    w.push(v.hash_kind.as_u8());
    w.extend_from_slice(&v.hash);
    write_uvarint(w, v.encoded_data.len() as u64).expect("Vec write");
    w.extend_from_slice(&v.encoded_data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{RecorderInfo, RecordingInfo, ScopeConfig};

    fn empty_metadata() -> Metadata {
        Metadata {
            recorder: RecorderInfo {
                language: "python".into(),
                language_version: "3.12.5".into(),
                recorder_version: "0.1.0".into(),
                platform: "linux-x86_64".into(),
            },
            recording: RecordingInfo {
                program: "python -c pass".into(),
                working_directory: None,
                scope_config: ScopeConfig::default(),
            },
            program: None,
            trace_uuid: [0; 16],
            recording_start_ns: 0,
        }
    }

    #[test]
    fn reserved_indices_are_zero_and_one() {
        let w = TraceWriter::new(empty_metadata());
        assert_eq!(w.values.len(), 2);
        assert!(matches!(w.values[0].value, Value::None));
        assert!(matches!(w.values[1].value, Value::ExceptionUnwindSentinel));
    }

    #[test]
    fn none_and_sentinel_do_not_dedup_despite_same_canonical() {
        // Both have empty canonical bytes; tag-aware dedup keeps them separate.
        let w = TraceWriter::new(empty_metadata());
        assert_ne!(w.values[0].value.tag(), w.values[1].value.tag());
    }

    #[test]
    fn bool_false_and_int_zero_do_not_dedup() {
        let mut w = TraceWriter::new(empty_metadata());
        let b = w.intern_value_inline(Value::Bool(false));
        let i = w.intern_value_inline(Value::Int(0));
        assert_ne!(b, i);
    }

    #[test]
    fn equal_values_dedup() {
        let mut w = TraceWriter::new(empty_metadata());
        let a = w.intern_value_inline(Value::Int(42));
        let b = w.intern_value_inline(Value::Int(42));
        assert_eq!(a, b);
    }

    #[test]
    fn strings_dedup_by_content() {
        let mut w = TraceWriter::new(empty_metadata());
        let a = w.intern_string("foo");
        let b = w.intern_string("foo");
        assert_eq!(a, b);
        let c = w.intern_string("bar");
        assert_ne!(a, c);
    }

    #[test]
    fn source_files_dedup_by_path() {
        let mut w = TraceWriter::new(empty_metadata());
        let a = w.add_source_file("foo.py", b"x = 1".to_vec());
        let b = w.add_source_file("foo.py", b"different content".to_vec());
        assert_eq!(a, b);
    }
}
