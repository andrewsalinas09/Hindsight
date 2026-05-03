// SPDX-License-Identifier: Apache-2.0

//! Buffered writer that produces a `.hindsight` trace file.
//!
//! Scope: file header, initial metadata, source bundle, initial string and
//! value tables, one event block carrying any of the nine v0.2 event types,
//! and (when finalized) a final summary block, an empty checkpoint index,
//! and a footer. Checkpoints (block tag 0x02), table updates (0x03), and
//! table snapshots (0x04) are out of scope for this revision; the single-
//! event-block model is unchanged from the previous revision.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;

use crate::canonical::{
    CanonicalLookup, canonical_identity, canonical_inline, canonical_summary, hash_bytes,
};
use crate::encoding::write_value_data;
use crate::error::{FormatError, Result};
use crate::event::{
    BranchResult, Event, EventTag, ExceptionRaised, FrameSnapshot, FrameSwitch, FunctionEntry,
    FunctionExit, LineDelta, Note, ScopeBoundary, write_event,
};
use crate::metadata::Metadata;
use crate::value::{HashKind, StringId, Value, ValueId, ValueTag};
use crate::varint::{uvarint_len, write_uvarint};

pub type FileId = u64;

/// File magic for `.hindsight` files. ASCII `"HNDSGHT\0"`.
pub const FILE_MAGIC: [u8; 8] = *b"HNDSGHT\0";

/// Footer magic. ASCII `"HNDFOOT\0"`.
pub const FOOTER_MAGIC: [u8; 8] = *b"HNDFOOT\0";

pub const FORMAT_VERSION_MAJOR: u8 = 0;
pub const FORMAT_VERSION_MINOR: u8 = 2;
pub const HEADER_LENGTH: u32 = 64;
pub const FOOTER_LENGTH: u32 = 32;

/// Format tag for the metadata block payload (TOML in v0.2).
pub const METADATA_FORMAT_TAG_TOML: u8 = 0x01;

/// Block tag for an event block.
pub const BLOCK_TAG_EVENT: u8 = 0x01;

/// Reserved value table indices, per spec.
pub const NONE_VALUE_ID: ValueId = 0;
pub const EXCEPTION_UNWIND_VALUE_ID: ValueId = 1;

// Header field byte offsets used by the back-patching step at finalization.
const HEADER_OFFSET_RECORDING_END: usize = 40;
const HEADER_OFFSET_FOOTER_OFFSET: usize = 48;

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

/// Per-event-type counters, populated as events are recorded and serialized
/// into `[final.statistics]` at finalization time.
#[derive(Debug, Clone, Default)]
struct Statistics {
    function_entry_events: u64,
    function_exit_events: u64,
    frame_snapshot_events: u64,
    line_events: u64,
    branch_events: u64,
    exception_events: u64,
    note_events: u64,
    scope_boundary_events: u64,
    frame_switch_events: u64,
}

impl Statistics {
    fn bump(&mut self, tag: EventTag) {
        match tag {
            EventTag::FunctionEntry => self.function_entry_events += 1,
            EventTag::FunctionExit => self.function_exit_events += 1,
            EventTag::FrameSnapshot => self.frame_snapshot_events += 1,
            EventTag::LineDelta => self.line_events += 1,
            EventTag::BranchResult => self.branch_events += 1,
            EventTag::ExceptionRaised => self.exception_events += 1,
            EventTag::Note => self.note_events += 1,
            EventTag::ScopeBoundary => self.scope_boundary_events += 1,
            EventTag::FrameSwitch => self.frame_switch_events += 1,
        }
    }

    fn total(&self) -> u64 {
        self.function_entry_events
            + self.function_exit_events
            + self.frame_snapshot_events
            + self.line_events
            + self.branch_events
            + self.exception_events
            + self.note_events
            + self.scope_boundary_events
            + self.frame_switch_events
    }
}

/// One excluded function reported in the final summary's `[final.scope_resolved]`
/// table. Mirrors the spec's `{ name = "...", matched_pattern = "..." }` shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedFunction {
    pub name: String,
    pub matched_pattern: String,
}

/// Scope-resolution information that's only known after recording completes.
/// Caller-supplied at [`TraceWriter::finish`]; for v0 callers without a
/// scope-resolution layer, the `Default` impl produces an empty value that
/// still yields a valid final summary.
#[derive(Debug, Clone, Default)]
pub struct ScopeResolution {
    pub recorded_functions: Vec<String>,
    pub excluded_functions: Vec<ExcludedFunction>,
    pub skip_blocks_observed: u32,
    pub depth_clips_observed: u32,
}

/// Information collected at finalization time and folded into the file
/// header (recording_end_ns, footer_offset) and final summary block.
#[derive(Debug, Clone)]
pub struct Finalization {
    /// Wall-clock time at finalization, ns since Unix epoch. Patched into
    /// the file header at offset 40.
    pub recording_end_ns: u64,
    pub scope_resolution: ScopeResolution,
}

/// Buffered trace writer. Build up sources, strings, values, and events;
/// call [`finish`](Self::finish) (or [`finish_to_bytes`](Self::finish_to_bytes))
/// to emit a fully-finalized file, or [`write_unfinalized`](Self::write_unfinalized)
/// to emit an interrupted-recording file (no final summary, no footer,
/// header `recording_end` and `footer_offset` left at zero).
pub struct TraceWriter {
    metadata: Metadata,

    sources: Vec<SourceFile>,
    source_lookup: HashMap<String, FileId>,

    strings: Vec<String>,
    string_lookup: HashMap<String, StringId>,

    values: Vec<StoredValue>,
    value_lookup: HashMap<ValueKey, ValueId>,

    events: Vec<Event>,
    statistics: Statistics,
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
            statistics: Statistics::default(),
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

    /// Single mutation point for the event stream and per-event-type counts.
    /// Every public `write_*` method must end here so statistics never drift
    /// from the on-disk event sequence.
    fn record_event(&mut self, event: Event) {
        self.statistics.bump(event.tag());
        self.events.push(event);
    }

    pub fn write_function_entry(&mut self, e: FunctionEntry) -> Result<()> {
        self.expect_string(e.function_id)?;
        self.expect_file(e.source_file_id)?;
        for arg in &e.args {
            self.expect_string(arg.name)?;
            self.expect_value(arg.value)?;
        }
        self.record_event(Event::FunctionEntry(e));
        Ok(())
    }

    pub fn write_function_exit(&mut self, e: FunctionExit) -> Result<()> {
        self.expect_value(e.return_value)?;
        self.record_event(Event::FunctionExit(e));
        Ok(())
    }

    pub fn write_frame_snapshot(&mut self, e: FrameSnapshot) -> Result<()> {
        for local in &e.locals {
            self.expect_string(local.name)?;
            self.expect_value(local.value)?;
        }
        self.record_event(Event::FrameSnapshot(e));
        Ok(())
    }

    pub fn write_line_delta(&mut self, e: LineDelta) -> Result<()> {
        for change in &e.changes {
            self.expect_string(change.name)?;
            self.expect_value(change.value)?;
        }
        self.record_event(Event::LineDelta(e));
        Ok(())
    }

    pub fn write_branch_result(&mut self, e: BranchResult) -> Result<()> {
        self.record_event(Event::BranchResult(e));
        Ok(())
    }

    pub fn write_exception_raised(&mut self, e: ExceptionRaised) -> Result<()> {
        self.expect_string(e.exception_type)?;
        self.expect_value(e.exception_value)?;
        self.record_event(Event::ExceptionRaised(e));
        Ok(())
    }

    pub fn write_note(&mut self, e: Note) -> Result<()> {
        self.expect_string(e.message)?;
        for kw in &e.kwargs {
            self.expect_string(kw.name)?;
            self.expect_value(kw.value)?;
        }
        self.record_event(Event::Note(e));
        Ok(())
    }

    pub fn write_scope_boundary(&mut self, e: ScopeBoundary) -> Result<()> {
        self.expect_string(e.reason)?;
        self.record_event(Event::ScopeBoundary(e));
        Ok(())
    }

    pub fn write_frame_switch(&mut self, e: FrameSwitch) -> Result<()> {
        self.record_event(Event::FrameSwitch(e));
        Ok(())
    }

    /// Emit the full `.hindsight` byte stream, fully finalized.
    pub fn finish<W: Write>(self, mut w: W, finalize: Finalization) -> Result<()> {
        let buf = self.finish_to_bytes(finalize)?;
        w.write_all(&buf)?;
        Ok(())
    }

    /// Emit the full byte stream as a `Vec<u8>`. Convenience for callers that
    /// want the bytes directly (tests, in-memory consumers).
    pub fn finish_to_bytes(self, finalize: Finalization) -> Result<Vec<u8>> {
        let recording_start_ns = self.metadata.recording_start_ns;
        let recording_end_ns = finalize.recording_end_ns;

        let mut buf = Vec::new();
        write_header(&mut buf, &self.metadata)?;
        write_metadata_block(&mut buf, &self.metadata)?;
        write_source_bundle(&mut buf, &self.sources)?;
        write_initial_string_table(&mut buf, &self.strings)?;
        write_initial_value_table(&mut buf, &self.values)?;
        write_event_block(
            &mut buf,
            &self.events,
            self.strings.len(),
            self.values.len(),
        )?;

        let final_summary_offset = buf.len() as u64;
        // total_blocks counts both the single event block and the (also single)
        // final-summary block. v0 always emits exactly these two; once
        // checkpoints / table updates / table snapshots ship, this becomes
        // a per-block counter on the writer.
        let total_blocks: u64 = 2;
        let trace_duration_ns = recording_end_ns.saturating_sub(recording_start_ns);
        write_final_summary(
            &mut buf,
            &self.statistics,
            total_blocks,
            trace_duration_ns,
            &finalize.scope_resolution,
        )?;

        let checkpoint_index_offset = buf.len() as u64;
        write_checkpoint_index_empty(&mut buf)?;

        let footer_offset = buf.len() as u64;
        write_footer(&mut buf, checkpoint_index_offset, final_summary_offset)?;

        // Back-patch the header's recording_end and footer_offset.
        buf[HEADER_OFFSET_RECORDING_END..HEADER_OFFSET_RECORDING_END + 8]
            .copy_from_slice(&recording_end_ns.to_le_bytes());
        buf[HEADER_OFFSET_FOOTER_OFFSET..HEADER_OFFSET_FOOTER_OFFSET + 8]
            .copy_from_slice(&footer_offset.to_le_bytes());

        Ok(buf)
    }

    /// Emit a deliberately unfinalized byte stream: header through event
    /// block only. The header's `recording_end` and `footer_offset` are left
    /// at zero, signaling to readers that the trace is partial. Used by
    /// tests; production recorders should call [`finish`](Self::finish).
    pub fn write_unfinalized<W: Write>(self, mut w: W) -> Result<()> {
        let mut buf = Vec::new();
        write_header(&mut buf, &self.metadata)?;
        write_metadata_block(&mut buf, &self.metadata)?;
        write_source_bundle(&mut buf, &self.sources)?;
        write_initial_string_table(&mut buf, &self.strings)?;
        write_initial_value_table(&mut buf, &self.values)?;
        write_event_block(
            &mut buf,
            &self.events,
            self.strings.len(),
            self.values.len(),
        )?;
        w.write_all(&buf)?;
        Ok(())
    }
}

// --- Section writers (free functions so finish_to_bytes can stitch them) ---

fn write_header(w: &mut Vec<u8>, metadata: &Metadata) -> Result<()> {
    let mut header = [0u8; 64];
    header[0..8].copy_from_slice(&FILE_MAGIC);
    header[8] = FORMAT_VERSION_MAJOR;
    header[9] = FORMAT_VERSION_MINOR;
    // Flags (10..12), reserved zero in v0.2.
    header[12..16].copy_from_slice(&HEADER_LENGTH.to_le_bytes());
    header[16..32].copy_from_slice(&metadata.trace_uuid);
    header[32..40].copy_from_slice(&metadata.recording_start_ns.to_le_bytes());
    // recording_end (40..48) and footer_offset (48..56) start zero. They
    // are back-patched at the end of finish_to_bytes when finalizing; for
    // write_unfinalized they stay zero.
    // Reserved (56..64): zero.
    w.extend_from_slice(&header);
    Ok(())
}

fn write_metadata_block(w: &mut Vec<u8>, metadata: &Metadata) -> Result<()> {
    let payload = metadata.to_toml();
    let payload_bytes = payload.as_bytes();
    let length: u32 = (1 + payload_bytes.len())
        .try_into()
        .map_err(|_| FormatError::MetadataTooLarge(1 + payload_bytes.len()))?;
    w.extend_from_slice(&length.to_le_bytes());
    w.push(METADATA_FORMAT_TAG_TOML);
    w.extend_from_slice(payload_bytes);
    Ok(())
}

fn write_source_bundle(w: &mut Vec<u8>, sources: &[SourceFile]) -> Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(&(sources.len() as u32).to_le_bytes());
    for source in sources {
        write_source_file_entry(&mut body, source)?;
    }
    let length: u32 = body
        .len()
        .try_into()
        .map_err(|_| FormatError::SectionTooLarge(body.len()))?;
    w.extend_from_slice(&length.to_le_bytes());
    w.extend_from_slice(&body);
    Ok(())
}

fn write_source_file_entry(w: &mut Vec<u8>, source: &SourceFile) -> Result<()> {
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

fn write_initial_string_table(w: &mut Vec<u8>, strings: &[String]) -> Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    for s in strings {
        write_uvarint(&mut body, s.len() as u64).expect("Vec write");
        body.extend_from_slice(s.as_bytes());
    }
    let length: u32 = body
        .len()
        .try_into()
        .map_err(|_| FormatError::SectionTooLarge(body.len()))?;
    w.extend_from_slice(&length.to_le_bytes());
    w.extend_from_slice(&body);
    Ok(())
}

fn write_initial_value_table(w: &mut Vec<u8>, values: &[StoredValue]) -> Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for v in values {
        write_value_table_entry(&mut body, v);
    }
    let length: u32 = body
        .len()
        .try_into()
        .map_err(|_| FormatError::SectionTooLarge(body.len()))?;
    w.extend_from_slice(&length.to_le_bytes());
    w.extend_from_slice(&body);
    Ok(())
}

fn write_event_block(
    w: &mut Vec<u8>,
    events: &[Event],
    strings_len: usize,
    values_len: usize,
) -> Result<()> {
    let mut uncompressed = Vec::new();
    for event in events {
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
    write_uvarint(&mut header, events.len() as u64).expect("Vec write");
    header.extend_from_slice(&(strings_len as u32).to_le_bytes());
    header.extend_from_slice(&(values_len as u32).to_le_bytes());
    header.extend_from_slice(&checksum.to_le_bytes());
    let header_length = header.len() as u64;

    let block_length = 1 + uvarint_len(header_length) + header.len() + compressed.len();
    let block_length: u32 = block_length
        .try_into()
        .map_err(|_| FormatError::SectionTooLarge(block_length))?;

    w.extend_from_slice(&block_length.to_le_bytes());
    w.push(BLOCK_TAG_EVENT);
    write_uvarint(w, header_length).expect("Vec write");
    w.extend_from_slice(&header);
    w.extend_from_slice(&compressed);
    Ok(())
}

fn write_final_summary(
    w: &mut Vec<u8>,
    statistics: &Statistics,
    total_blocks: u64,
    trace_duration_ns: u64,
    scope: &ScopeResolution,
) -> Result<()> {
    let toml = render_final_summary_toml(statistics, total_blocks, trace_duration_ns, scope);
    let payload = toml.as_bytes();
    // Practical limit: u32 max (4 GiB). The summary holds at most a handful
    // of integers plus a list of recorded function names; even a recording
    // covering hundreds of millions of distinct functions stays well under.
    // Errors only fire if a caller jams a pathological scope_resolution in.
    let length: u32 = payload
        .len()
        .try_into()
        .map_err(|_| FormatError::SectionTooLarge(payload.len()))?;
    w.extend_from_slice(&length.to_le_bytes());
    w.extend_from_slice(payload);
    Ok(())
}

fn write_checkpoint_index_empty(w: &mut Vec<u8>) -> Result<()> {
    // Body is just a u32 entry-count = 0. Section length covers the count
    // field but not itself, matching the convention used elsewhere.
    let body = 0u32.to_le_bytes();
    let length: u32 = body.len() as u32;
    w.extend_from_slice(&length.to_le_bytes());
    w.extend_from_slice(&body);
    Ok(())
}

fn write_footer(
    w: &mut Vec<u8>,
    checkpoint_index_offset: u64,
    final_summary_offset: u64,
) -> Result<()> {
    let mut footer = [0u8; 32];
    footer[0..8].copy_from_slice(&FOOTER_MAGIC);
    footer[8..12].copy_from_slice(&FOOTER_LENGTH.to_le_bytes());
    footer[12..20].copy_from_slice(&checkpoint_index_offset.to_le_bytes());
    footer[20..28].copy_from_slice(&final_summary_offset.to_le_bytes());
    // Reserved (28..32): zero.
    w.extend_from_slice(&footer);
    Ok(())
}

fn write_value_table_entry(w: &mut Vec<u8>, v: &StoredValue) {
    w.push(v.value.tag().as_u8());
    w.push(v.hash_kind.as_u8());
    w.extend_from_slice(&v.hash);
    write_uvarint(w, v.encoded_data.len() as u64).expect("Vec write");
    w.extend_from_slice(&v.encoded_data);
}

// --- Final summary TOML renderer ---

fn render_final_summary_toml(
    stats: &Statistics,
    total_blocks: u64,
    trace_duration_ns: u64,
    scope: &ScopeResolution,
) -> String {
    let mut s = String::new();

    s.push_str("[final]\n");
    s.push_str("clean_shutdown = true\n");
    writeln!(s, "total_events = {}", stats.total()).expect("write to String never fails");
    writeln!(s, "total_blocks = {total_blocks}").expect("write to String never fails");
    writeln!(s, "trace_duration_ns = {trace_duration_ns}").expect("write to String never fails");
    s.push('\n');

    s.push_str("[final.scope_resolved]\n");
    write_string_array_kv(&mut s, "recorded_functions", &scope.recorded_functions);
    s.push_str("excluded_functions = [");
    for (i, ef) in scope.excluded_functions.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str("{ name = ");
        write_basic_string(&mut s, &ef.name);
        s.push_str(", matched_pattern = ");
        write_basic_string(&mut s, &ef.matched_pattern);
        s.push_str(" }");
    }
    s.push_str("]\n");
    writeln!(s, "skip_blocks_observed = {}", scope.skip_blocks_observed)
        .expect("write to String never fails");
    writeln!(s, "depth_clips_observed = {}", scope.depth_clips_observed)
        .expect("write to String never fails");
    s.push('\n');

    s.push_str("[final.statistics]\n");
    writeln!(s, "function_entry_events = {}", stats.function_entry_events)
        .expect("write to String never fails");
    writeln!(s, "function_exit_events = {}", stats.function_exit_events)
        .expect("write to String never fails");
    writeln!(s, "frame_snapshot_events = {}", stats.frame_snapshot_events)
        .expect("write to String never fails");
    writeln!(s, "line_events = {}", stats.line_events).expect("write to String never fails");
    writeln!(s, "branch_events = {}", stats.branch_events).expect("write to String never fails");
    writeln!(s, "exception_events = {}", stats.exception_events)
        .expect("write to String never fails");
    writeln!(s, "note_events = {}", stats.note_events).expect("write to String never fails");
    writeln!(s, "scope_boundary_events = {}", stats.scope_boundary_events)
        .expect("write to String never fails");
    writeln!(s, "frame_switch_events = {}", stats.frame_switch_events)
        .expect("write to String never fails");

    s
}

fn write_string_array_kv(out: &mut String, key: &str, values: &[String]) {
    out.push_str(key);
    out.push_str(" = [");
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_basic_string(out, v);
    }
    out.push_str("]\n");
}

/// TOML basic string escapes; duplicated from `metadata.rs` to keep the
/// final-summary renderer self-contained.
fn write_basic_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                debug_assert!(
                    c as u32 <= 0xFFFF,
                    "TOML \\uXXXX escape only fits BMP (U+0000..U+FFFF); \
                     supplementary-plane code points need \\UXXXXXXXX"
                );
                write!(out, "\\u{:04X}", c as u32).expect("write to String never fails");
            }
            c => out.push(c),
        }
    }
    out.push('"');
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

    fn default_finalize() -> Finalization {
        Finalization {
            recording_end_ns: 1_000,
            scope_resolution: ScopeResolution::default(),
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
        let w = TraceWriter::new(empty_metadata());
        assert!(matches!(
            w.values[NONE_VALUE_ID as usize].value,
            Value::None
        ));
        assert!(matches!(
            w.values[EXCEPTION_UNWIND_VALUE_ID as usize].value,
            Value::ExceptionUnwindSentinel
        ));
        assert_ne!(NONE_VALUE_ID, EXCEPTION_UNWIND_VALUE_ID);
        assert_ne!(
            w.values[NONE_VALUE_ID as usize].value.tag(),
            w.values[EXCEPTION_UNWIND_VALUE_ID as usize].value.tag()
        );
    }

    #[test]
    fn re_interning_none_and_sentinel_returns_reserved_ids() {
        let mut w = TraceWriter::new(empty_metadata());
        let starting_len = w.values.len();
        let none_again = w.intern_value_inline(Value::None);
        let sentinel_again = w.intern_value_inline(Value::ExceptionUnwindSentinel);
        assert_eq!(none_again, NONE_VALUE_ID);
        assert_eq!(sentinel_again, EXCEPTION_UNWIND_VALUE_ID);
        assert_eq!(
            w.values.len(),
            starting_len,
            "no new entries should be allocated"
        );
    }

    #[test]
    fn bool_false_and_int_zero_do_not_dedup() {
        let mut w = TraceWriter::new(empty_metadata());
        let b = w.intern_value_inline(Value::Bool(false));
        let i = w.intern_value_inline(Value::Int(0));
        assert_ne!(b, i);
        assert_eq!(w.values[b as usize].value.tag(), ValueTag::Bool);
        assert_eq!(w.values[i as usize].value.tag(), ValueTag::IntSmall);
        assert_eq!(w.values[b as usize].hash, w.values[i as usize].hash);
        assert_eq!(
            w.values[b as usize].hash_kind,
            w.values[i as usize].hash_kind
        );
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

    #[test]
    fn statistics_bump_is_per_event_type() {
        let mut s = Statistics::default();
        s.bump(EventTag::FunctionEntry);
        s.bump(EventTag::FunctionEntry);
        s.bump(EventTag::BranchResult);
        assert_eq!(s.function_entry_events, 2);
        assert_eq!(s.branch_events, 1);
        assert_eq!(s.total(), 3);
    }

    #[test]
    fn finalize_back_patches_header_recording_end_and_footer_offset() {
        let metadata = Metadata {
            recording_start_ns: 100,
            ..empty_metadata()
        };
        let w = TraceWriter::new(metadata);
        let buf = w
            .finish_to_bytes(Finalization {
                recording_end_ns: 999,
                scope_resolution: ScopeResolution::default(),
            })
            .unwrap();

        let recording_end = u64::from_le_bytes(
            buf[HEADER_OFFSET_RECORDING_END..HEADER_OFFSET_RECORDING_END + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(recording_end, 999);

        let footer_offset = u64::from_le_bytes(
            buf[HEADER_OFFSET_FOOTER_OFFSET..HEADER_OFFSET_FOOTER_OFFSET + 8]
                .try_into()
                .unwrap(),
        );
        assert!(footer_offset > 0, "footer_offset must be patched non-zero");
        assert_eq!(
            footer_offset as usize,
            buf.len() - FOOTER_LENGTH as usize,
            "footer should be the last 32 bytes"
        );
        // Footer magic should be at footer_offset.
        assert_eq!(
            &buf[footer_offset as usize..footer_offset as usize + 8],
            &FOOTER_MAGIC,
        );
    }

    #[test]
    fn write_unfinalized_leaves_recording_end_and_footer_offset_zero() {
        let metadata = Metadata {
            recording_start_ns: 100,
            ..empty_metadata()
        };
        let w = TraceWriter::new(metadata);
        let mut buf = Vec::new();
        w.write_unfinalized(&mut buf).unwrap();
        let recording_end = u64::from_le_bytes(
            buf[HEADER_OFFSET_RECORDING_END..HEADER_OFFSET_RECORDING_END + 8]
                .try_into()
                .unwrap(),
        );
        let footer_offset = u64::from_le_bytes(
            buf[HEADER_OFFSET_FOOTER_OFFSET..HEADER_OFFSET_FOOTER_OFFSET + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(recording_end, 0);
        assert_eq!(footer_offset, 0);
    }

    #[test]
    fn final_summary_toml_includes_required_keys() {
        let mut stats = Statistics::default();
        stats.bump(EventTag::BranchResult);
        stats.bump(EventTag::BranchResult);
        stats.bump(EventTag::Note);
        let scope = ScopeResolution {
            recorded_functions: vec!["a.b".into(), "c".into()],
            excluded_functions: vec![ExcludedFunction {
                name: "numpy.dot".into(),
                matched_pattern: "numpy.*".into(),
            }],
            skip_blocks_observed: 5,
            depth_clips_observed: 0,
        };
        let toml = render_final_summary_toml(&stats, 7, 1_234, &scope);
        assert!(toml.contains("[final]"));
        assert!(toml.contains("clean_shutdown = true"));
        assert!(toml.contains("total_events = 3"));
        assert!(toml.contains("total_blocks = 7"));
        assert!(toml.contains("trace_duration_ns = 1234"));
        assert!(toml.contains("[final.scope_resolved]"));
        assert!(toml.contains("recorded_functions = [\"a.b\", \"c\"]"));
        assert!(toml.contains(
            "excluded_functions = [{ name = \"numpy.dot\", matched_pattern = \"numpy.*\" }]"
        ));
        assert!(toml.contains("skip_blocks_observed = 5"));
        assert!(toml.contains("depth_clips_observed = 0"));
        assert!(toml.contains("[final.statistics]"));
        assert!(toml.contains("branch_events = 2"));
        assert!(toml.contains("note_events = 1"));
        assert!(toml.contains("function_entry_events = 0"));
    }

    #[test]
    fn finish_to_bytes_writes_footer_with_correct_offsets() {
        let w = TraceWriter::new(empty_metadata());
        let buf = w.finish_to_bytes(default_finalize()).unwrap();
        // The footer is at the end; checkpoint_index_offset is the second u64
        // in the footer, final_summary_offset is the third.
        let footer_start = buf.len() - FOOTER_LENGTH as usize;
        assert_eq!(&buf[footer_start..footer_start + 8], &FOOTER_MAGIC);
        let footer_length =
            u32::from_le_bytes(buf[footer_start + 8..footer_start + 12].try_into().unwrap());
        assert_eq!(footer_length, FOOTER_LENGTH);
        let checkpoint_index_offset = u64::from_le_bytes(
            buf[footer_start + 12..footer_start + 20]
                .try_into()
                .unwrap(),
        );
        let final_summary_offset = u64::from_le_bytes(
            buf[footer_start + 20..footer_start + 28]
                .try_into()
                .unwrap(),
        );
        assert!(final_summary_offset < checkpoint_index_offset);
        assert!(checkpoint_index_offset < footer_start as u64);
    }
}
