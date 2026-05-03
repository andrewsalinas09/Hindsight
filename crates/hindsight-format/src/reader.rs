// SPDX-License-Identifier: Apache-2.0

//! Buffered reader that parses a `.hindsight` trace file produced by
//! [`crate::TraceWriter`].
//!
//! Scope mirrors the writer's current scope: file header, initial metadata
//! block, source bundle, initial string and value tables, and a single event
//! block holding FUNCTION_ENTRY, FUNCTION_EXIT, FRAME_SNAPSHOT, and
//! LINE_DELTA events. Footer, final summary, checkpoints, table updates,
//! table snapshots, and other event types are out of scope and surface as
//! [`FormatError::UnsupportedBlockTag`] / [`FormatError::UnsupportedEventTag`]
//! / [`FormatError::TrailingBytesAfterEventBlock`] depending on what the
//! reader hits.
//!
//! ## Section length conventions (resolved against the writer)
//!
//! The v0.2 spec has several diagrams whose "length" labels are ambiguous.
//! The writer's interpretations, which this reader matches:
//!
//! - **Metadata block.** `length` u32 LE *includes* the format-tag byte; the
//!   payload is `length - 1` bytes.
//! - **Source bundle / string table / value table.** `length` u32 LE is the
//!   size of everything *after* the length field — i.e., file_count + entries
//!   for the bundle, count + strings for the string table, count + entries
//!   for the value table. Length does not include itself.
//! - **Event block.** `block_length` u32 LE is the size of everything after
//!   itself: `block_tag` (1) + `header_length` varint + header content +
//!   compressed payload. The inner `header_length` varint is the size of the
//!   header content only — i.e., compressed_len/uncompressed_len/first_event_id/
//!   event_count varints + the three trailing u32s (table sizes + checksum) —
//!   not counting the header_length varint itself.
//! - **Value table entries.** The `length` varint is just the encoded `data`
//!   bytes; it does *not* include the type tag, hash kind, hash, or the
//!   length varint itself.
//!
//! ## Spec ambiguities surfaced by this implementation
//!
//! - The v0.2 spec lists "int (big)", "string", and "bytes" as
//!   "length-prefixed". The writer treats this prefix as the *outer* value
//!   table entry length only — there is no second inner length prefix on the
//!   bytes themselves. The reader follows the same convention.
//! - The v0.2 spec doesn't say whether a strict reader should reject extra
//!   bytes inside a value table entry (e.g., 9 bytes for a `Float`). This
//!   reader rejects them via [`FormatError::ValueDataLeftover`].
//! - The v0.2 spec doesn't say whether a reader should validate that a
//!   container value's child IDs refer to entries already defined earlier in
//!   the table. The writer enforces that on write; the reader enforces it on
//!   read via [`FormatError::ForwardValueRef`].
//! - The v0.2 spec says unknown event tags are skipped. This reader instead
//!   errors on event tags 0x05+ because the current writer doesn't produce
//!   them; silently skipping them in v0 would mask writer bugs. Forward
//!   compatibility (skip on unknown) can be reintroduced once additional
//!   event types are actually emitted.

use std::io::Read;

use crate::byte_reader::ByteReader;
use crate::decoding::decode_value;
use crate::error::{FormatError, Result};
use crate::event::{
    Argument, Change, Event, FrameSnapshot, FunctionEntry, FunctionExit, LineDelta, Local,
};
use crate::value::{HashKind, StringId, Value, ValueId, ValueTag};
use crate::writer::{
    BLOCK_TAG_EVENT, FILE_MAGIC, FORMAT_VERSION_MAJOR, FORMAT_VERSION_MINOR, FileId, HEADER_LENGTH,
    METADATA_FORMAT_TAG_TOML,
};

/// Parsed file header. See `docs/trace-format.md` §"File header".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub format_version_major: u8,
    pub format_version_minor: u8,
    pub flags: u16,
    pub header_length: u32,
    pub trace_uuid: [u8; 16],
    pub recording_start_ns: u64,
    /// Zero if the file was not finalized cleanly (current writer always
    /// produces zero — finalization isn't implemented yet).
    pub recording_end_ns: u64,
    /// Zero if the file was not finalized cleanly.
    pub footer_offset: u64,
}

/// Parsed initial metadata block. The TOML payload is returned verbatim;
/// callers decode it on their own (e.g., via the `toml` crate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataBlock {
    /// Currently always [`crate::METADATA_FORMAT_TAG_TOML`] (`0x01`).
    pub format_tag: u8,
    pub payload: String,
}

/// One source file from the bundle, with its blake3-256 content hash
/// already verified against `content`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub file_id: FileId,
    pub blake3_hash: [u8; 32],
    pub path: String,
    pub content: Vec<u8>,
}

/// One value table entry: hash kind + hash + decoded value. The on-disk type
/// tag is recoverable as `value.tag()`.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueEntry {
    pub hash_kind: HashKind,
    pub hash: [u8; 16],
    pub value: Value,
}

/// Header information for the (single) event block this reader supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBlockInfo {
    pub first_event_id: u64,
    pub event_count: u64,
    pub string_table_size_after: u32,
    pub value_table_size_after: u32,
}

/// Parsed `.hindsight` trace.
///
/// Construct via [`TraceReader::new`] (any `Read` impl) or
/// [`TraceReader::from_bytes`] (already-loaded slice). Both eagerly parse the
/// whole file; this matches the writer, which buffers everything before
/// emitting, and keeps the API simple given the v0.2 single-block shape.
#[derive(Debug, Clone)]
pub struct TraceReader {
    header: Header,
    metadata: MetadataBlock,
    source_files: Vec<SourceFile>,
    strings: Vec<String>,
    values: Vec<ValueEntry>,
    events: Vec<Event>,
    event_block: EventBlockInfo,
}

impl TraceReader {
    pub fn new<R: Read>(mut reader: R) -> Result<Self> {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        Self::from_bytes(&buf)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut br = ByteReader::new(bytes);
        let header = parse_header(&mut br)?;
        let metadata = parse_metadata_block(&mut br)?;
        let source_files = parse_source_bundle(&mut br)?;
        let strings = parse_string_table(&mut br)?;
        let values = parse_value_table(&mut br, strings.len())?;
        let (event_block, events) = parse_event_block(&mut br, &source_files, &strings, &values)?;

        if br.remaining() != 0 {
            return Err(FormatError::TrailingBytesAfterEventBlock(br.remaining()));
        }

        Ok(Self {
            header,
            metadata,
            source_files,
            strings,
            values,
            events,
            event_block,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }
    pub fn metadata(&self) -> &MetadataBlock {
        &self.metadata
    }
    pub fn source_files(&self) -> &[SourceFile] {
        &self.source_files
    }
    pub fn strings(&self) -> &[String] {
        &self.strings
    }
    pub fn values(&self) -> &[ValueEntry] {
        &self.values
    }
    pub fn events(&self) -> &[Event] {
        &self.events
    }
    pub fn event_block_info(&self) -> &EventBlockInfo {
        &self.event_block
    }
}

// --- Section parsers ---

fn parse_header(br: &mut ByteReader) -> Result<Header> {
    let magic_bytes = br.take(8)?;
    let mut magic = [0u8; 8];
    magic.copy_from_slice(magic_bytes);
    if magic != FILE_MAGIC {
        return Err(FormatError::BadMagic {
            expected: FILE_MAGIC,
            got: magic,
        });
    }
    let major = br.read_u8()?;
    let minor = br.read_u8()?;
    if major != FORMAT_VERSION_MAJOR || minor != FORMAT_VERSION_MINOR {
        return Err(FormatError::UnsupportedVersion { major, minor });
    }
    let flags = br.read_u16_le()?;
    if flags != 0 {
        return Err(FormatError::ReservedFieldNonzero("header flags"));
    }
    let header_length = br.read_u32_le()?;
    if header_length != HEADER_LENGTH {
        return Err(FormatError::BadHeaderLength {
            expected: HEADER_LENGTH,
            got: header_length,
        });
    }
    let mut trace_uuid = [0u8; 16];
    trace_uuid.copy_from_slice(br.take(16)?);
    let recording_start_ns = br.read_u64_le()?;
    let recording_end_ns = br.read_u64_le()?;
    let footer_offset = br.read_u64_le()?;
    let reserved = br.read_u64_le()?;
    if reserved != 0 {
        return Err(FormatError::ReservedFieldNonzero("header reserved tail"));
    }
    Ok(Header {
        format_version_major: major,
        format_version_minor: minor,
        flags,
        header_length,
        trace_uuid,
        recording_start_ns,
        recording_end_ns,
        footer_offset,
    })
}

fn parse_metadata_block(br: &mut ByteReader) -> Result<MetadataBlock> {
    let length = br.read_u32_le()?;
    if length < 1 {
        return Err(FormatError::SectionLengthMismatch {
            section: "metadata",
            expected: 1,
            consumed: 0,
        });
    }
    let format_tag = br.read_u8()?;
    if format_tag != METADATA_FORMAT_TAG_TOML {
        return Err(FormatError::UnsupportedMetadataFormatTag(format_tag));
    }
    let payload_len = (length - 1) as usize;
    let payload_bytes = br.take(payload_len)?;
    let payload = std::str::from_utf8(payload_bytes)
        .map_err(|_| FormatError::InvalidUtf8 {
            field: "metadata payload",
        })?
        .to_owned();
    Ok(MetadataBlock {
        format_tag,
        payload,
    })
}

fn parse_source_bundle(br: &mut ByteReader) -> Result<Vec<SourceFile>> {
    let length = br.read_u32_le()? as usize;
    let start = br.pos();
    let file_count = br.read_u32_le()? as usize;
    let mut files = Vec::with_capacity(file_count);
    for _ in 0..file_count {
        let file_id = br.read_uvarint()?;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(br.take(32)?);
        let path_len = br.read_u16_le()? as usize;
        let path_bytes = br.take(path_len)?;
        let path = std::str::from_utf8(path_bytes)
            .map_err(|_| FormatError::InvalidUtf8 {
                field: "source path",
            })?
            .to_owned();
        let content_len = br.read_u32_le()? as usize;
        let content = br.take(content_len)?.to_vec();
        let computed = *blake3::hash(&content).as_bytes();
        if computed != hash {
            return Err(FormatError::SourceHashMismatch { file_id });
        }
        files.push(SourceFile {
            file_id,
            blake3_hash: hash,
            path,
            content,
        });
    }
    let consumed = br.pos() - start;
    if consumed != length {
        return Err(FormatError::SectionLengthMismatch {
            section: "source bundle",
            expected: length as u64,
            consumed: consumed as u64,
        });
    }
    Ok(files)
}

fn parse_string_table(br: &mut ByteReader) -> Result<Vec<String>> {
    let length = br.read_u32_le()? as usize;
    let start = br.pos();
    let count = br.read_u32_le()? as usize;
    let mut strings = Vec::with_capacity(count);
    for _ in 0..count {
        let len = br.read_uvarint()? as usize;
        let bytes = br.take(len)?;
        let s = std::str::from_utf8(bytes)
            .map_err(|_| FormatError::InvalidUtf8 {
                field: "string table entry",
            })?
            .to_owned();
        strings.push(s);
    }
    let consumed = br.pos() - start;
    if consumed != length {
        return Err(FormatError::SectionLengthMismatch {
            section: "string table",
            expected: length as u64,
            consumed: consumed as u64,
        });
    }
    Ok(strings)
}

fn parse_value_table(br: &mut ByteReader, strings_len: usize) -> Result<Vec<ValueEntry>> {
    let length = br.read_u32_le()? as usize;
    let start = br.pos();
    let count = br.read_u32_le()? as usize;
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let tag_byte = br.read_u8()?;
        let tag = ValueTag::from_u8(tag_byte).ok_or(FormatError::InvalidValueTag(tag_byte))?;
        let hash_kind_byte = br.read_u8()?;
        let hash_kind = HashKind::from_u8(hash_kind_byte)
            .ok_or(FormatError::InvalidHashKind(hash_kind_byte))?;
        let mut hash = [0u8; 16];
        hash.copy_from_slice(br.take(16)?);
        let data_len = br.read_uvarint()? as usize;
        let data = br.take(data_len)?;
        let value = decode_value(tag, data)?;
        validate_value_refs(&value, i as ValueId, strings_len)?;
        entries.push(ValueEntry {
            hash_kind,
            hash,
            value,
        });
    }
    let consumed = br.pos() - start;
    if consumed != length {
        return Err(FormatError::SectionLengthMismatch {
            section: "value table",
            expected: length as u64,
            consumed: consumed as u64,
        });
    }
    Ok(entries)
}

/// Reject container values whose child IDs point at entries that haven't
/// been defined yet at this position in the value table.
///
/// **Strict-mode policy.** The v0.2 spec doesn't say a reader has to do this.
/// The writer enforces it on emit (see `assert_child_ids_exist`), so any
/// trace produced by the in-tree writer will pass; rejecting forward refs on
/// read costs nothing and catches:
///   - third-party recorders that don't enforce ordering,
///   - corruption that swaps or shifts entries inside the value table,
///   - hand-crafted traces in tests that accidentally encode a cycle.
///
/// We'd flip this to permissive (or downgrade to a warning) if a future
/// format revision allows forward references — for example, to break value
/// cycles without using `CycleRef`. No such revision is on the roadmap.
fn validate_value_refs(value: &Value, current_index: ValueId, strings_len: usize) -> Result<()> {
    match value {
        Value::List(ids) | Value::Set(ids) => {
            for id in ids {
                if *id >= current_index {
                    return Err(FormatError::ForwardValueRef {
                        id: *id,
                        at_index: current_index,
                    });
                }
            }
        }
        Value::Dict(pairs) => {
            for (k, v) in pairs {
                if *k >= current_index {
                    return Err(FormatError::ForwardValueRef {
                        id: *k,
                        at_index: current_index,
                    });
                }
                if *v >= current_index {
                    return Err(FormatError::ForwardValueRef {
                        id: *v,
                        at_index: current_index,
                    });
                }
            }
        }
        Value::TypeRef(id) if (*id as usize) >= strings_len => {
            return Err(FormatError::UnknownStringId(*id));
        }
        Value::Summary {
            type_name, repr, ..
        } => {
            if (*type_name as usize) >= strings_len {
                return Err(FormatError::UnknownStringId(*type_name));
            }
            if (*repr as usize) >= strings_len {
                return Err(FormatError::UnknownStringId(*repr));
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_event_block(
    br: &mut ByteReader,
    sources: &[SourceFile],
    strings: &[String],
    values: &[ValueEntry],
) -> Result<(EventBlockInfo, Vec<Event>)> {
    let block_length = br.read_u32_le()? as usize;
    let block_start = br.pos();
    let block_tag = br.read_u8()?;
    if block_tag != BLOCK_TAG_EVENT {
        return Err(FormatError::UnsupportedBlockTag(block_tag));
    }
    let header_length = br.read_uvarint()? as usize;
    let header_start = br.pos();
    let compressed_len = br.read_uvarint()? as usize;
    let uncompressed_len = br.read_uvarint()?;
    let first_event_id = br.read_uvarint()?;
    let event_count = br.read_uvarint()?;
    let string_table_size_after = br.read_u32_le()?;
    let value_table_size_after = br.read_u32_le()?;
    let checksum = br.read_u32_le()?;
    let header_consumed = br.pos() - header_start;
    if header_consumed != header_length {
        return Err(FormatError::SectionLengthMismatch {
            section: "event block header",
            expected: header_length as u64,
            consumed: header_consumed as u64,
        });
    }
    let compressed = br.take(compressed_len)?;
    let block_consumed = br.pos() - block_start;
    if block_consumed != block_length {
        return Err(FormatError::SectionLengthMismatch {
            section: "event block",
            expected: block_length as u64,
            consumed: block_consumed as u64,
        });
    }

    if string_table_size_after as usize != strings.len() {
        return Err(FormatError::TableSizeMismatch {
            section: "string",
            expected: string_table_size_after,
            actual: strings.len() as u32,
        });
    }
    if value_table_size_after as usize != values.len() {
        return Err(FormatError::TableSizeMismatch {
            section: "value",
            expected: value_table_size_after,
            actual: values.len() as u32,
        });
    }

    let computed_checksum = crc32c::crc32c(compressed);
    if computed_checksum != checksum {
        return Err(FormatError::ChecksumMismatch {
            expected: checksum,
            got: computed_checksum,
        });
    }

    let decompressed = zstd::stream::decode_all(compressed)
        .map_err(|e| FormatError::Decompression(e.to_string()))?;
    if decompressed.len() as u64 != uncompressed_len {
        return Err(FormatError::DecompressedLengthMismatch {
            expected: uncompressed_len,
            got: decompressed.len() as u64,
        });
    }

    let mut event_reader = ByteReader::new(&decompressed);
    let mut events = Vec::with_capacity(event_count as usize);
    for _ in 0..event_count {
        events.push(parse_event(&mut event_reader, sources, strings, values)?);
    }
    if event_reader.remaining() != 0 {
        return Err(FormatError::TrailingBytesInEventBlock(
            event_reader.remaining(),
        ));
    }

    Ok((
        EventBlockInfo {
            first_event_id,
            event_count,
            string_table_size_after,
            value_table_size_after,
        },
        events,
    ))
}

fn parse_event(
    br: &mut ByteReader,
    sources: &[SourceFile],
    strings: &[String],
    values: &[ValueEntry],
) -> Result<Event> {
    let event_length = br.read_uvarint()? as usize;
    if event_length == 0 {
        return Err(FormatError::EmptyEvent);
    }
    let event_start = br.pos();
    let tag_byte = br.read_u8()?;

    let event = match tag_byte {
        0x01 => {
            let timestamp_delta_ns = br.read_uvarint()?;
            let frame_id = br.read_uvarint()?;
            let function_id = br.read_uvarint()?;
            let source_file_id = br.read_uvarint()?;
            let line = read_u32_varint(br, "FUNCTION_ENTRY line")?;
            let arg_count = br.read_uvarint()? as usize;
            let mut args = Vec::with_capacity(arg_count);
            for _ in 0..arg_count {
                let name = br.read_uvarint()?;
                let value = br.read_uvarint()?;
                args.push(Argument { name, value });
            }
            check_string(strings, function_id)?;
            check_file(sources, source_file_id)?;
            for arg in &args {
                check_string(strings, arg.name)?;
                check_value(values, arg.value)?;
            }
            Event::FunctionEntry(FunctionEntry {
                timestamp_delta_ns,
                frame_id,
                function_id,
                source_file_id,
                line,
                args,
            })
        }
        0x02 => {
            let timestamp_delta_ns = br.read_uvarint()?;
            let frame_id = br.read_uvarint()?;
            let return_value = br.read_uvarint()?;
            check_value(values, return_value)?;
            Event::FunctionExit(FunctionExit {
                timestamp_delta_ns,
                frame_id,
                return_value,
            })
        }
        0x03 => {
            let timestamp_delta_ns = br.read_uvarint()?;
            let frame_id = br.read_uvarint()?;
            let line = read_u32_varint(br, "FRAME_SNAPSHOT line")?;
            let locals_count = br.read_uvarint()? as usize;
            let mut locals = Vec::with_capacity(locals_count);
            for _ in 0..locals_count {
                let name = br.read_uvarint()?;
                let value = br.read_uvarint()?;
                check_string(strings, name)?;
                check_value(values, value)?;
                locals.push(Local { name, value });
            }
            Event::FrameSnapshot(FrameSnapshot {
                timestamp_delta_ns,
                frame_id,
                line,
                locals,
            })
        }
        0x04 => {
            let timestamp_delta_ns = br.read_uvarint()?;
            let line = read_u32_varint(br, "LINE_DELTA line")?;
            let changes_count = br.read_uvarint()? as usize;
            let mut changes = Vec::with_capacity(changes_count);
            for _ in 0..changes_count {
                let name = br.read_uvarint()?;
                let value = br.read_uvarint()?;
                check_string(strings, name)?;
                check_value(values, value)?;
                changes.push(Change { name, value });
            }
            Event::LineDelta(LineDelta {
                timestamp_delta_ns,
                line,
                changes,
            })
        }
        // Strict-mode policy: error on unknown event tags rather than skip
        // them via the length prefix as the spec describes.
        //
        // The spec's §"Reserved event types" says readers must skip unknown
        // tags using the length prefix — that's the right long-term behavior
        // for forward compatibility once multiple event types exist. But
        // today the writer only ever emits 0x01..=0x04, so any other tag is
        // either:
        //   - a writer bug (a new event type half-implemented),
        //   - corruption that flipped a tag byte, or
        //   - a third-party recorder running ahead of this reader.
        // All three are worth surfacing loudly during v0; skip-with-warning
        // would mask them.
        //
        // Flip to spec-compliant skip behavior once the writer supports the
        // full event-type set (BRANCH_RESULT, EXCEPTION_RAISED, NOTE,
        // SCOPE_BOUNDARY, FRAME_SWITCH); at that point this turns into a
        // forward-compatibility shim and the strictness is no longer paying
        // for itself.
        _ => return Err(FormatError::UnsupportedEventTag(tag_byte)),
    };

    let consumed = br.pos() - event_start;
    if consumed != event_length {
        return Err(FormatError::SectionLengthMismatch {
            section: "event",
            expected: event_length as u64,
            consumed: consumed as u64,
        });
    }
    Ok(event)
}

fn read_u32_varint(br: &mut ByteReader, field: &'static str) -> Result<u32> {
    let v = br.read_uvarint()?;
    v.try_into()
        .map_err(|_| FormatError::FieldOverflow { field })
}

fn check_string(strings: &[String], id: StringId) -> Result<()> {
    if (id as usize) < strings.len() {
        Ok(())
    } else {
        Err(FormatError::UnknownStringId(id))
    }
}

fn check_value(values: &[ValueEntry], id: ValueId) -> Result<()> {
    if (id as usize) < values.len() {
        Ok(())
    } else {
        Err(FormatError::UnknownValueId(id))
    }
}

fn check_file(sources: &[SourceFile], id: FileId) -> Result<()> {
    if (id as usize) < sources.len() {
        Ok(())
    } else {
        Err(FormatError::UnknownFileId(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{Metadata, RecorderInfo, RecordingInfo, ScopeConfig};
    use crate::writer::TraceWriter;

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
            trace_uuid: [0xAB; 16],
            recording_start_ns: 1_700_000_000,
        }
    }

    fn write_minimal_trace() -> Vec<u8> {
        let w = TraceWriter::new(empty_metadata());
        let mut out = Vec::new();
        w.finish(&mut out).unwrap();
        out
    }

    // --- Header parser ---

    #[test]
    fn parse_header_extracts_fields() {
        let bytes = write_minimal_trace();
        let mut br = ByteReader::new(&bytes);
        let h = parse_header(&mut br).unwrap();
        assert_eq!(h.format_version_major, FORMAT_VERSION_MAJOR);
        assert_eq!(h.format_version_minor, FORMAT_VERSION_MINOR);
        assert_eq!(h.flags, 0);
        assert_eq!(h.header_length, HEADER_LENGTH);
        assert_eq!(h.trace_uuid, [0xAB; 16]);
        assert_eq!(h.recording_start_ns, 1_700_000_000);
        assert_eq!(h.recording_end_ns, 0);
        assert_eq!(h.footer_offset, 0);
        assert_eq!(br.pos(), HEADER_LENGTH as usize);
    }

    #[test]
    fn parse_header_rejects_bad_magic() {
        let mut bytes = write_minimal_trace();
        bytes[0] = b'X';
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_header(&mut br),
            Err(FormatError::BadMagic { .. })
        ));
    }

    #[test]
    fn parse_header_rejects_unsupported_version() {
        let mut bytes = write_minimal_trace();
        bytes[8] = 1; // major
        bytes[9] = 0; // minor
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_header(&mut br),
            Err(FormatError::UnsupportedVersion { major: 1, minor: 0 })
        ));
    }

    #[test]
    fn parse_header_rejects_nonzero_flags() {
        let mut bytes = write_minimal_trace();
        bytes[10] = 0x01;
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_header(&mut br),
            Err(FormatError::ReservedFieldNonzero("header flags"))
        ));
    }

    #[test]
    fn parse_header_rejects_bad_header_length() {
        let mut bytes = write_minimal_trace();
        // header_length lives at offset 12..16
        bytes[12..16].copy_from_slice(&128u32.to_le_bytes());
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_header(&mut br),
            Err(FormatError::BadHeaderLength {
                expected: 64,
                got: 128
            })
        ));
    }

    #[test]
    fn parse_header_rejects_nonzero_reserved_tail() {
        let mut bytes = write_minimal_trace();
        // Reserved tail lives at offset 56..64.
        bytes[56] = 1;
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_header(&mut br),
            Err(FormatError::ReservedFieldNonzero("header reserved tail"))
        ));
    }

    #[test]
    fn parse_header_truncated_input_errors() {
        let bytes = vec![0u8; 10];
        let mut br = ByteReader::new(&bytes);
        // 10 bytes is enough for the magic but not the rest.
        assert!(parse_header(&mut br).is_err());
    }

    // --- Metadata parser ---

    #[test]
    fn parse_metadata_returns_raw_toml() {
        let bytes = write_minimal_trace();
        let mut br = ByteReader::new(&bytes);
        parse_header(&mut br).unwrap();
        let m = parse_metadata_block(&mut br).unwrap();
        assert_eq!(m.format_tag, METADATA_FORMAT_TAG_TOML);
        assert!(m.payload.contains("[recorder]"));
        assert!(m.payload.contains("language = \"python\""));
    }

    #[test]
    fn parse_metadata_rejects_unknown_format_tag() {
        // Build a bogus metadata block: length=1, format_tag=0x99, no payload.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(0x99);
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_metadata_block(&mut br),
            Err(FormatError::UnsupportedMetadataFormatTag(0x99))
        ));
    }

    #[test]
    fn parse_metadata_rejects_zero_length() {
        let bytes = 0u32.to_le_bytes();
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_metadata_block(&mut br),
            Err(FormatError::SectionLengthMismatch { .. })
        ));
    }

    // --- Source bundle parser ---

    #[test]
    fn parse_source_bundle_round_trips_one_file() {
        let mut w = TraceWriter::new(empty_metadata());
        w.add_source_file("hi.py", b"print('hi')\n".to_vec());
        let mut out = Vec::new();
        w.finish(&mut out).unwrap();

        let mut br = ByteReader::new(&out);
        parse_header(&mut br).unwrap();
        parse_metadata_block(&mut br).unwrap();
        let files = parse_source_bundle(&mut br).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "hi.py");
        assert_eq!(files[0].content, b"print('hi')\n");
        assert_eq!(
            files[0].blake3_hash,
            *blake3::hash(b"print('hi')\n").as_bytes()
        );
    }

    #[test]
    fn parse_source_bundle_detects_hash_mismatch() {
        // Build a bogus source bundle by hand: file with right hash for content
        // "abc", but body actually contains "abd".
        let real_content = b"abd";
        let lying_hash = *blake3::hash(b"abc").as_bytes();
        let path = b"x.py";

        let mut entries = Vec::new();
        entries.push(0u8); // file_id varint = 0
        entries.extend_from_slice(&lying_hash);
        entries.extend_from_slice(&(path.len() as u16).to_le_bytes());
        entries.extend_from_slice(path);
        entries.extend_from_slice(&(real_content.len() as u32).to_le_bytes());
        entries.extend_from_slice(real_content);

        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes()); // file_count
        body.extend_from_slice(&entries);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);

        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_source_bundle(&mut br),
            Err(FormatError::SourceHashMismatch { file_id: 0 })
        ));
    }

    // --- String table parser ---

    #[test]
    fn parse_string_table_round_trip() {
        let mut w = TraceWriter::new(empty_metadata());
        w.intern_string("foo");
        w.intern_string("bar");
        let mut out = Vec::new();
        w.finish(&mut out).unwrap();

        let mut br = ByteReader::new(&out);
        parse_header(&mut br).unwrap();
        parse_metadata_block(&mut br).unwrap();
        parse_source_bundle(&mut br).unwrap();
        let strings = parse_string_table(&mut br).unwrap();
        assert_eq!(strings, vec!["foo".to_string(), "bar".to_string()]);
    }

    #[test]
    fn parse_string_table_rejects_invalid_utf8() {
        // length = 4 (count u32) + 1 (varint len) + 1 (one byte) = 6
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&6u32.to_le_bytes()); // section length
        bytes.extend_from_slice(&1u32.to_le_bytes()); // count
        bytes.push(1); // length varint
        bytes.push(0xFF); // not valid utf-8 alone
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_string_table(&mut br),
            Err(FormatError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn parse_string_table_section_length_mismatch_is_caught() {
        // declared length 100 but body is just 4 bytes (count=0).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let mut br = ByteReader::new(&bytes);
        // Will fail because we only declared 4 bytes of body but section length
        // says 100; the reader hits Truncated trying to fill the gap.
        assert!(parse_string_table(&mut br).is_err());
    }

    // --- Value table parser ---

    #[test]
    fn parse_value_table_includes_reserved_indices() {
        let bytes = write_minimal_trace();
        let mut br = ByteReader::new(&bytes);
        parse_header(&mut br).unwrap();
        parse_metadata_block(&mut br).unwrap();
        parse_source_bundle(&mut br).unwrap();
        let strings = parse_string_table(&mut br).unwrap();
        let values = parse_value_table(&mut br, strings.len()).unwrap();
        assert!(matches!(values[0].value, Value::None));
        assert!(matches!(values[1].value, Value::ExceptionUnwindSentinel));
        assert_eq!(values[0].hash_kind, HashKind::Content);
        assert_eq!(values[1].hash_kind, HashKind::Content);
    }

    #[test]
    fn parse_value_table_rejects_invalid_tag() {
        // length = 4 (count) + 1 (tag) = 5
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&5u32.to_le_bytes()); // section length
        bytes.extend_from_slice(&1u32.to_le_bytes()); // count
        bytes.push(0x55); // invalid tag
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_value_table(&mut br, 0),
            Err(FormatError::InvalidValueTag(0x55))
        ));
    }

    #[test]
    fn parse_value_table_rejects_invalid_hash_kind() {
        // section: count=1, tag=None(0x00), hash_kind=0x99, hash=16 zero bytes,
        // length varint=0
        let mut entry = Vec::new();
        entry.push(0x00); // None tag
        entry.push(0x99); // bogus hash kind
        entry.extend_from_slice(&[0u8; 16]); // hash
        entry.push(0); // length=0
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((4 + entry.len()) as u32).to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&entry);
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_value_table(&mut br, 0),
            Err(FormatError::InvalidHashKind(0x99))
        ));
    }

    #[test]
    fn parse_value_table_rejects_forward_value_ref() {
        // count=1, single List entry referencing value id 5 (out of range).
        let mut entry = Vec::new();
        entry.push(ValueTag::ListOrTuple as u8);
        entry.push(HashKind::Content as u8);
        entry.extend_from_slice(&[0u8; 16]);
        // data: count=1 varint, then id=5 varint
        let data = [0x01u8, 0x05];
        entry.push(data.len() as u8);
        entry.extend_from_slice(&data);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((4 + entry.len()) as u32).to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&entry);
        let mut br = ByteReader::new(&bytes);
        assert!(matches!(
            parse_value_table(&mut br, 0),
            Err(FormatError::ForwardValueRef { id: 5, at_index: 0 })
        ));
    }

    // --- Event block parser ---

    #[test]
    fn parse_event_block_returns_zero_events_for_minimal_trace() {
        let bytes = write_minimal_trace();
        let mut br = ByteReader::new(&bytes);
        parse_header(&mut br).unwrap();
        parse_metadata_block(&mut br).unwrap();
        let sources = parse_source_bundle(&mut br).unwrap();
        let strings = parse_string_table(&mut br).unwrap();
        let values = parse_value_table(&mut br, strings.len()).unwrap();
        let (info, events) = parse_event_block(&mut br, &sources, &strings, &values).unwrap();
        assert_eq!(events.len(), 0);
        assert_eq!(info.event_count, 0);
        assert_eq!(info.first_event_id, 0);
        assert_eq!(info.string_table_size_after as usize, strings.len());
        assert_eq!(info.value_table_size_after as usize, values.len());
    }

    #[test]
    fn parse_event_block_detects_checksum_mismatch() {
        let mut bytes = write_minimal_trace();
        // Corrupt the last byte of the file (within the compressed payload).
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let mut br = ByteReader::new(&bytes);
        parse_header(&mut br).unwrap();
        parse_metadata_block(&mut br).unwrap();
        let sources = parse_source_bundle(&mut br).unwrap();
        let strings = parse_string_table(&mut br).unwrap();
        let values = parse_value_table(&mut br, strings.len()).unwrap();
        let result = parse_event_block(&mut br, &sources, &strings, &values);
        assert!(matches!(result, Err(FormatError::ChecksumMismatch { .. })));
    }

    #[test]
    fn parse_event_block_rejects_non_event_block_tag() {
        // Build a bogus event block with tag 0x02 (checkpoint) — out of scope.
        let body = [0x02u8, 0x01, 0x00]; // tag, header_length=1, header_byte (won't actually parse)
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        let mut br = ByteReader::new(&bytes);
        let result = parse_event_block(&mut br, &[], &[], &[]);
        assert!(matches!(
            result,
            Err(FormatError::UnsupportedBlockTag(0x02))
        ));
    }

    #[test]
    fn from_bytes_full_pipeline_for_minimal_trace_works() {
        let bytes = write_minimal_trace();
        let r = TraceReader::from_bytes(&bytes).unwrap();
        assert_eq!(r.header().trace_uuid, [0xAB; 16]);
        assert_eq!(r.events().len(), 0);
        assert!(r.source_files().is_empty());
        assert_eq!(r.values().len(), 2);
    }

    #[test]
    fn from_bytes_rejects_trailing_garbage() {
        let mut bytes = write_minimal_trace();
        bytes.push(0xAB);
        assert!(matches!(
            TraceReader::from_bytes(&bytes),
            Err(FormatError::TrailingBytesAfterEventBlock(1))
        ));
    }
}
