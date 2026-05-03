// SPDX-License-Identifier: Apache-2.0

//! Buffered reader that parses a `.hindsight` trace file produced by
//! [`crate::TraceWriter`].
//!
//! ## Block stream model
//!
//! After the prelude (header through initial value table) the file is a
//! sequence of blocks of any of the four v0.2 tags:
//!
//! - `0x01` — event block. Append events to the flat events list. The
//!   reader's view of "events at index i" is stable across blocks; block
//!   boundaries are exposed via [`TraceReader::event_blocks`].
//! - `0x02` — checkpoint record. Recorded for cross-checking against the
//!   checkpoint index; the index (parsed from the populated section just
//!   before the footer) is what callers see via [`TraceReader::checkpoints`].
//! - `0x03` — table update. Validate `base_*_count` against the reader's
//!   current table sizes, then append the new strings/values.
//! - `0x04` — table snapshot. Replace the reader's string and value tables
//!   wholesale with the snapshot's content. Per spec, after applying a
//!   snapshot the reader's tables are in a fully reconstructed state.
//!
//! Trailing sections (final summary, checkpoint index, footer) are only
//! parsed when the file header's `footer_offset` is non-zero — see
//! `parse_finalized_tail`.
//!
//! Readers should not assume there's a 1:1 correspondence between event
//! blocks and table updates: the writer skips empty updates, so an event
//! block that doesn't reference any newly-introduced strings or values has
//! no preceding update.
//!
//! ## Section length conventions (resolved against the writer)
//!
//! The v0.2 spec has several diagrams whose "length" labels are ambiguous.
//! The writer's interpretations, which this reader matches:
//!
//! - **Metadata block.** `length` u32 LE *includes* the format-tag byte; the
//!   payload is `length - 1` bytes.
//! - **Source bundle / string table / value table / checkpoint index.**
//!   `length` u32 LE is the size of everything *after* the length field,
//!   not counting the length field itself.
//! - **Block.** `block_length` u32 LE is the size of everything after
//!   itself: `block_tag` (1) + `header_length` varint + header content +
//!   compressed payload. The inner `header_length` varint is the size of the
//!   header content only.
//! - **Value table entries.** The `length` varint is just the encoded `data`
//!   bytes; it does *not* include the type tag, hash kind, hash, or the
//!   length varint itself.
//! - **Final summary.** `length` u32 LE is the byte length of the TOML
//!   payload that follows; there is no inner format-tag byte.
//!
//! ## Spec ambiguities surfaced by this implementation
//!
//! See `docs/trace-format.md` for the TODO(v0.3) notes that codify each.
//! The reader-side strict-mode policies — value-data leftover, forward
//! value refs, unknown event tags, footer offset cross-check, snapshot
//! offset sentinel `0` — are all documented at the relevant call sites.

use std::io::Read;

use crate::byte_reader::ByteReader;
use crate::decoding::decode_value;
use crate::error::{FormatError, Result};
use crate::event::{
    Argument, BoundaryType, BranchResult, Change, Event, ExceptionRaised, FrameSnapshot,
    FrameSwitch, FrameSwitchReason, FunctionEntry, FunctionExit, Kwarg, LineDelta, Local, Note,
    ScopeBoundary,
};
use crate::value::{HashKind, StringId, Value, ValueId, ValueTag};
use crate::writer::{
    BLOCK_TAG_CHECKPOINT, BLOCK_TAG_EVENT, BLOCK_TAG_TABLE_SNAPSHOT, BLOCK_TAG_TABLE_UPDATE,
    FILE_MAGIC, FOOTER_LENGTH, FOOTER_MAGIC, FORMAT_VERSION_MAJOR, FORMAT_VERSION_MINOR, FileId,
    HEADER_LENGTH, METADATA_FORMAT_TAG_TOML,
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
    /// Zero if the file was not finalized cleanly.
    pub recording_end_ns: u64,
    /// Zero if the file was not finalized cleanly. When non-zero, the reader
    /// parses through to the footer and exposes the final summary, the
    /// checkpoint index, and the footer.
    pub footer_offset: u64,
}

/// Parsed initial metadata block. The TOML payload is returned verbatim.
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

/// Header information for one event block. With the multi-block writer the
/// reader exposes a `Vec<EventBlockInfo>` via [`TraceReader::event_blocks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBlockInfo {
    pub first_event_id: u64,
    pub event_count: u64,
    pub string_table_size_after: u32,
    pub value_table_size_after: u32,
    /// Byte offset of this event block's leading u32 length prefix in the
    /// file. Useful for callers that want to map an event back to its block.
    pub file_offset: u64,
}

/// Parsed final summary block. The TOML payload is returned verbatim, mirror
/// of [`MetadataBlock`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalSummary {
    pub payload: String,
}

/// One entry in the checkpoint index (from the populated section just before
/// the footer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointEntry {
    pub wall_clock_ns: u64,
    /// Global event ID at the moment this checkpoint was emitted (i.e. the
    /// number of events recorded *before* the checkpoint).
    pub event_id: u64,
    /// Byte offset of the checkpoint record in the file.
    pub file_offset: u64,
    /// Byte offset of the most recent table snapshot block, or zero meaning
    /// "use the initial string and value tables that follow the file
    /// header" (no snapshot block emitted before this checkpoint). See the
    /// TODO(v0.3) on snapshot offsets in `docs/trace-format.md`.
    pub snapshot_offset: u64,
}

/// Parsed footer. See `docs/trace-format.md` §"Footer".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    pub footer_length: u32,
    pub checkpoint_index_offset: u64,
    pub final_summary_offset: u64,
}

/// Parsed `.hindsight` trace.
///
/// Construct via [`TraceReader::new`] (any `Read` impl) or
/// [`TraceReader::from_bytes`] (already-loaded slice). Both eagerly parse the
/// whole file; this matches the writer, which buffers everything before
/// emitting. Trailing sections (final summary, checkpoint index, footer) are
/// only parsed when the file header's `footer_offset` is non-zero. For
/// unfinalized files (header.footer_offset == 0), the reader stops after the
/// last successfully-parsed block and any bytes after that point are
/// silently ignored — this matches the spec's crash-recovery semantics.
#[derive(Debug, Clone)]
pub struct TraceReader {
    header: Header,
    metadata: MetadataBlock,
    source_files: Vec<SourceFile>,
    strings: Vec<String>,
    values: Vec<ValueEntry>,
    events: Vec<Event>,
    event_blocks: Vec<EventBlockInfo>,
    /// Per-event mapping back to the global event ID. `event_global_ids[i]`
    /// is the global event ID of `events[i]`; built up as event blocks are
    /// parsed using each block's `first_event_id`. Drives `seek_to_event_id`.
    event_global_ids: Vec<u64>,
    final_summary: Option<FinalSummary>,
    checkpoints: Vec<CheckpointEntry>,
    footer: Option<Footer>,
}

/// A view into a [`TraceReader`] starting at a seek target.
///
/// Returned by [`TraceReader::seek_to_event_id`] and
/// [`TraceReader::seek_to_wall_clock`]. The `events()` slice begins at the
/// seek target (or the closest event past it). The `seek_anchor` reports
/// which checkpoint the seek used as its starting point — `None` if the
/// reader scanned from the beginning (no usable checkpoint), or `Some(idx)`
/// pointing into the checkpoint index. This lets tests verify the seek
/// actually consulted the index instead of returning a sliced view from the
/// start (which a buggy implementation might).
#[derive(Debug, Clone, Copy)]
pub struct TraceCursor<'a> {
    reader: &'a TraceReader,
    /// Index into `reader.events()` where the cursor is positioned.
    start_index: usize,
    /// Index into `reader.checkpoints()` of the checkpoint that anchored
    /// the seek. `None` means no usable checkpoint (e.g. the seek target is
    /// before any checkpoint).
    seek_anchor: Option<usize>,
}

impl<'a> TraceCursor<'a> {
    /// Events from the seek point onward, in order.
    pub fn events(&self) -> &'a [Event] {
        &self.reader.events[self.start_index..]
    }

    /// Index into [`TraceReader::events`] of the cursor's first event.
    pub fn start_index(&self) -> usize {
        self.start_index
    }

    /// Global event ID of the cursor's first event, or `None` if the cursor
    /// is past the end of the trace.
    pub fn first_event_id(&self) -> Option<u64> {
        self.reader.event_global_ids.get(self.start_index).copied()
    }

    /// Which checkpoint (by index into [`TraceReader::checkpoints`]) the
    /// seek used as its starting point. `None` means the seek either
    /// scanned from the start of the events list (no preceding checkpoint
    /// exists for the target) or the trace had no checkpoints at all.
    pub fn seek_anchor(&self) -> Option<&'a CheckpointEntry> {
        self.seek_anchor.map(|i| &self.reader.checkpoints[i])
    }

    /// Index of the seek anchor in [`TraceReader::checkpoints`], or `None`
    /// — useful for tests that want the index without dereferencing.
    pub fn seek_anchor_index(&self) -> Option<usize> {
        self.seek_anchor
    }
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
        let mut strings = parse_string_table(&mut br)?;
        let mut values = parse_value_table(&mut br, strings.len())?;

        // Block stream: parse blocks until either the file ends (unfinalized)
        // or we hit the start of the trailing sections (finalized).
        let stream_end = if header.footer_offset == 0 {
            bytes.len()
        } else {
            // Read the footer's final_summary_offset to know where the block
            // stream ends. We have to peek without consuming.
            //
            // Approach: parse blocks one at a time; after each, check whether
            // we've reached header.footer_offset (the trailing sections).
            // But we don't know final_summary_offset yet. We'll use a
            // simpler approach: parse blocks while pos < header.footer_offset
            // - 32 (footer length); the trailing sections fit there. To find
            // the exact final_summary_offset we'd have to either pre-parse
            // the footer or guess. Pre-parse is cleaner.
            //
            // The footer is at header.footer_offset. We can read it directly
            // without disturbing `br`, then use its final_summary_offset as
            // the block-stream-end.
            let footer = peek_footer(bytes, header.footer_offset)?;
            footer.final_summary_offset as usize
        };

        let mut events = Vec::new();
        let mut event_blocks = Vec::new();
        let mut event_global_ids = Vec::new();

        while br.pos() < stream_end {
            parse_one_block(
                &mut br,
                &source_files,
                &mut strings,
                &mut values,
                &mut events,
                &mut event_blocks,
                &mut event_global_ids,
            )?;
        }

        let (final_summary, checkpoints, footer) = if header.footer_offset == 0 {
            (None, Vec::new(), None)
        } else {
            // Pos should now be at final_summary_offset.
            let final_summary_offset = br.pos() as u64;
            let final_summary = parse_final_summary(&mut br)?;
            let checkpoint_index_offset = br.pos() as u64;
            let checkpoints = parse_checkpoint_index(&mut br)?;
            let observed_footer_offset = br.pos() as u64;
            if observed_footer_offset != header.footer_offset {
                return Err(FormatError::HeaderFooterOffsetMismatch {
                    expected: header.footer_offset,
                    observed: observed_footer_offset,
                });
            }
            let footer = parse_footer(&mut br)?;
            if footer.final_summary_offset != final_summary_offset {
                return Err(FormatError::FooterOffsetMismatch {
                    field: "final_summary_offset",
                    expected: footer.final_summary_offset,
                    observed: final_summary_offset,
                });
            }
            if footer.checkpoint_index_offset != checkpoint_index_offset {
                return Err(FormatError::FooterOffsetMismatch {
                    field: "checkpoint_index_offset",
                    expected: footer.checkpoint_index_offset,
                    observed: checkpoint_index_offset,
                });
            }
            if br.remaining() != 0 {
                return Err(FormatError::TrailingBytesAfterFinalSection(br.remaining()));
            }
            (Some(final_summary), checkpoints, Some(footer))
        };

        Ok(Self {
            header,
            metadata,
            source_files,
            strings,
            values,
            events,
            event_blocks,
            event_global_ids,
            final_summary,
            checkpoints,
            footer,
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
    /// Per-event-block headers, in file order.
    pub fn event_blocks(&self) -> &[EventBlockInfo] {
        &self.event_blocks
    }
    /// Returns `Some` if the file was finalized cleanly (header.footer_offset
    /// non-zero), otherwise `None`.
    pub fn final_summary(&self) -> Option<&FinalSummary> {
        self.final_summary.as_ref()
    }
    /// The checkpoint index from the trailing section. Empty for unfinalized
    /// files and for finalized files with no checkpoints emitted.
    pub fn checkpoints(&self) -> &[CheckpointEntry] {
        &self.checkpoints
    }
    /// Returns `Some` if the file was finalized cleanly.
    pub fn footer(&self) -> Option<&Footer> {
        self.footer.as_ref()
    }
    /// Convenience for callers that just want a yes/no on finalization.
    pub fn is_finalized(&self) -> bool {
        self.footer.is_some()
    }

    /// Seek to the first event whose global event ID is `>= target`. Uses
    /// the checkpoint index to find the nearest preceding checkpoint, then
    /// scans the already-parsed events from there.
    ///
    /// For v0 the "skip parts of the file" optimization the spec mentions
    /// is intentionally not implemented — the file is already fully parsed
    /// and the seek is a slice operation. The cursor's `seek_anchor` reports
    /// which checkpoint anchored the seek so tests can verify the index was
    /// actually consulted.
    pub fn seek_to_event_id(&self, target: u64) -> Result<TraceCursor<'_>> {
        if !self.is_finalized() {
            return Err(FormatError::SeekUnfinalized);
        }
        let last = self.event_global_ids.last().copied().unwrap_or(0);
        if !self.events.is_empty() && target > last {
            return Err(FormatError::SeekPastEnd { target, last });
        }

        // Find the latest checkpoint with event_id <= target.
        let anchor_idx = self.find_checkpoint_anchor_by_event_id(target);
        let scan_start = match anchor_idx {
            Some(i) => self
                .event_global_ids
                .partition_point(|&gid| gid < self.checkpoints[i].event_id),
            None => 0,
        };

        // Linear scan forward from the anchor to find target.
        let start_index = self.event_global_ids[scan_start..]
            .iter()
            .position(|&gid| gid >= target)
            .map(|p| scan_start + p)
            .unwrap_or(self.events.len());

        Ok(TraceCursor {
            reader: self,
            start_index,
            seek_anchor: anchor_idx,
        })
    }

    /// Seek to the first event whose timestamp is `>= target_wall_clock_ns`.
    /// Wall-clock time is reconstructed by summing per-event timestamp
    /// deltas starting from `header.recording_start_ns`. The checkpoint
    /// index provides the anchor so the scan doesn't start from event 0.
    pub fn seek_to_wall_clock(&self, target_wall_clock_ns: u64) -> Result<TraceCursor<'_>> {
        if !self.is_finalized() {
            return Err(FormatError::SeekUnfinalized);
        }
        let recording_start = self.header.recording_start_ns;

        // Find the latest checkpoint with wall_clock_ns <= target.
        let anchor_idx = self.find_checkpoint_anchor_by_wall_clock(target_wall_clock_ns);

        // Sum deltas to reconstruct wall-clock per event.
        let (scan_start, mut current_wall_ns) = match anchor_idx {
            Some(i) => {
                let cp = &self.checkpoints[i];
                // The checkpoint records wall_clock_ns AS OF its emission,
                // i.e. *after* event_id events. Scan from event_id.
                let idx = self
                    .event_global_ids
                    .partition_point(|&gid| gid < cp.event_id);
                (idx, cp.wall_clock_ns)
            }
            None => (0, recording_start),
        };

        let mut start_index = self.events.len();
        for (i, event) in self.events.iter().enumerate().skip(scan_start) {
            current_wall_ns = current_wall_ns.saturating_add(event.timestamp_delta_ns());
            if current_wall_ns >= target_wall_clock_ns {
                start_index = i;
                break;
            }
        }

        Ok(TraceCursor {
            reader: self,
            start_index,
            seek_anchor: anchor_idx,
        })
    }

    fn find_checkpoint_anchor_by_event_id(&self, target: u64) -> Option<usize> {
        // Latest checkpoint with event_id <= target.
        let pos = self.checkpoints.partition_point(|cp| cp.event_id <= target);
        if pos == 0 { None } else { Some(pos - 1) }
    }

    fn find_checkpoint_anchor_by_wall_clock(&self, target_ns: u64) -> Option<usize> {
        let pos = self
            .checkpoints
            .partition_point(|cp| cp.wall_clock_ns <= target_ns);
        if pos == 0 { None } else { Some(pos - 1) }
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
        let entry = parse_value_table_entry(br, i as ValueId, strings_len)?;
        entries.push(entry);
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

/// Parse one value table entry from the *outer* stream (initial table or
/// table-update body). `current_index` is what the new entry's ID will be —
/// used to reject forward refs.
fn parse_value_table_entry(
    br: &mut ByteReader,
    current_index: ValueId,
    strings_len: usize,
) -> Result<ValueEntry> {
    let tag_byte = br.read_u8()?;
    let tag = ValueTag::from_u8(tag_byte).ok_or(FormatError::InvalidValueTag(tag_byte))?;
    let hash_kind_byte = br.read_u8()?;
    let hash_kind =
        HashKind::from_u8(hash_kind_byte).ok_or(FormatError::InvalidHashKind(hash_kind_byte))?;
    let mut hash = [0u8; 16];
    hash.copy_from_slice(br.take(16)?);
    let data_len = br.read_uvarint()? as usize;
    let data = br.take(data_len)?;
    let value = decode_value(tag, data)?;
    validate_value_refs(&value, current_index, strings_len)?;
    Ok(ValueEntry {
        hash_kind,
        hash,
        value,
    })
}

/// Reject container values whose child IDs point at entries that haven't
/// been defined yet at this position in the value table.
///
/// **Strict-mode policy.** The v0.2 spec doesn't say a reader has to do this.
/// The writer enforces it on emit, so any trace produced by the in-tree
/// writer will pass; rejecting forward refs on read costs nothing and
/// catches third-party recorders that don't enforce ordering, corruption
/// that swaps entries, and hand-crafted traces that accidentally encode a
/// cycle.
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

/// Parse one block of any of the four v0.2 tags, applying its effect to the
/// reader's accumulating state (events list, table content, block index).
#[allow(clippy::too_many_arguments)]
fn parse_one_block(
    br: &mut ByteReader,
    sources: &[SourceFile],
    strings: &mut Vec<String>,
    values: &mut Vec<ValueEntry>,
    events: &mut Vec<Event>,
    event_blocks: &mut Vec<EventBlockInfo>,
    event_global_ids: &mut Vec<u64>,
) -> Result<()> {
    let block_file_offset = br.pos() as u64;
    let block_length = br.read_u32_le()? as usize;
    let block_start = br.pos();
    let block_tag = br.read_u8()?;

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
            section: "block header",
            expected: header_length as u64,
            consumed: header_consumed as u64,
        });
    }
    let compressed = br.take(compressed_len)?;
    let block_consumed = br.pos() - block_start;
    if block_consumed != block_length {
        return Err(FormatError::SectionLengthMismatch {
            section: "block",
            expected: block_length as u64,
            consumed: block_consumed as u64,
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

    match block_tag {
        BLOCK_TAG_EVENT => {
            apply_event_block(
                &decompressed,
                sources,
                strings,
                values,
                events,
                event_global_ids,
                first_event_id,
                event_count,
            )?;
            // Validate the table-size-after fields against current state
            // *after* applying the block. Event blocks don't change tables
            // so the after-sizes must equal current.
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
            event_blocks.push(EventBlockInfo {
                first_event_id,
                event_count,
                string_table_size_after,
                value_table_size_after,
                file_offset: block_file_offset,
            });
        }
        BLOCK_TAG_CHECKPOINT => {
            // In-stream checkpoint: parsed for completeness but the data
            // we surface to callers comes from the populated checkpoint
            // index just before the footer. The spec is silent on whether
            // these must agree; in v0 the writer makes them agree by
            // construction. We don't cross-check here because doing so
            // would force the reader to know what's coming up in the
            // (unread) checkpoint index.
            let _ = parse_checkpoint_record_payload(&decompressed)?;
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
        }
        BLOCK_TAG_TABLE_UPDATE => {
            apply_table_update(&decompressed, strings, values)?;
            // After applying, the table-size-after must match the block's
            // claim.
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
        }
        BLOCK_TAG_TABLE_SNAPSHOT => {
            apply_table_snapshot(
                &decompressed,
                strings,
                values,
                string_table_size_after,
                value_table_size_after,
            )?;
            // Snapshot block file_offset is what subsequent checkpoints
            // reference via their snapshot_offset; we don't need to track
            // it here since the checkpoint index already carries the
            // offsets.
            let _ = block_file_offset;
        }
        // Unknown block tags are intentionally rejected today (same
        // strict-mode policy as unknown event tags). Will flip to
        // skip-with-warning once the writer can stay behind a reader.
        _ => return Err(FormatError::UnsupportedBlockTag(block_tag)),
    }

    // No further validation here — sources isn't mutated, just borrowed
    // for event reference checks inside apply_event_block.
    let _ = sources;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_event_block(
    decompressed: &[u8],
    sources: &[SourceFile],
    strings: &[String],
    values: &[ValueEntry],
    events: &mut Vec<Event>,
    event_global_ids: &mut Vec<u64>,
    first_event_id: u64,
    event_count: u64,
) -> Result<()> {
    let mut event_reader = ByteReader::new(decompressed);
    for i in 0..event_count {
        let event = parse_event(&mut event_reader, sources, strings, values)?;
        events.push(event);
        event_global_ids.push(first_event_id + i);
    }
    if event_reader.remaining() != 0 {
        return Err(FormatError::TrailingBytesInEventBlock(
            event_reader.remaining(),
        ));
    }
    Ok(())
}

fn parse_checkpoint_record_payload(decompressed: &[u8]) -> Result<(u64, u64)> {
    let mut br = ByteReader::new(decompressed);
    let wall_clock_ns = br.read_u64_le()?;
    let snapshot_offset = br.read_u64_le()?;
    if br.remaining() != 0 {
        return Err(FormatError::SectionLengthMismatch {
            section: "checkpoint record payload",
            expected: 16,
            consumed: (16 + br.remaining()) as u64,
        });
    }
    Ok((wall_clock_ns, snapshot_offset))
}

fn apply_table_update(
    decompressed: &[u8],
    strings: &mut Vec<String>,
    values: &mut Vec<ValueEntry>,
) -> Result<()> {
    let mut br = ByteReader::new(decompressed);
    let strings_before = strings.len();
    let values_before = values.len();

    let base_string_count = br.read_u32_le()?;
    let new_string_count = br.read_u32_le()?;
    if base_string_count as usize != strings_before {
        return Err(FormatError::TableUpdateBaseMismatch {
            field: "base_string_count",
            expected: base_string_count,
            observed: strings_before as u32,
        });
    }
    for _ in 0..new_string_count {
        let len = br.read_uvarint()? as usize;
        let bytes = br.take(len)?;
        let s = std::str::from_utf8(bytes)
            .map_err(|_| FormatError::InvalidUtf8 {
                field: "table update string entry",
            })?
            .to_owned();
        strings.push(s);
    }
    let actual_new_strings = (strings.len() - strings_before) as u32;
    if actual_new_strings != new_string_count {
        return Err(FormatError::TableUpdateNewCountMismatch {
            field: "string",
            expected: new_string_count,
            observed: actual_new_strings,
        });
    }

    let base_value_count = br.read_u32_le()?;
    let new_value_count = br.read_u32_le()?;
    if base_value_count as usize != values_before {
        return Err(FormatError::TableUpdateBaseMismatch {
            field: "base_value_count",
            expected: base_value_count,
            observed: values_before as u32,
        });
    }
    for _ in 0..new_value_count {
        let current_index = values.len() as ValueId;
        let entry = parse_value_table_entry(&mut br, current_index, strings.len())?;
        values.push(entry);
    }
    let actual_new_values = (values.len() - values_before) as u32;
    if actual_new_values != new_value_count {
        return Err(FormatError::TableUpdateNewCountMismatch {
            field: "value",
            expected: new_value_count,
            observed: actual_new_values,
        });
    }
    if br.remaining() != 0 {
        return Err(FormatError::SectionLengthMismatch {
            section: "table update payload",
            expected: 0,
            consumed: br.remaining() as u64,
        });
    }
    Ok(())
}

fn apply_table_snapshot(
    decompressed: &[u8],
    strings: &mut Vec<String>,
    values: &mut Vec<ValueEntry>,
    string_table_size_after: u32,
    value_table_size_after: u32,
) -> Result<()> {
    let mut br = ByteReader::new(decompressed);

    // String section: length u32 + count u32 + entries.
    let string_section_length = br.read_u32_le()? as usize;
    let string_section_start = br.pos();
    let string_count = br.read_u32_le()?;
    let mut new_strings: Vec<String> = Vec::with_capacity(string_count as usize);
    for _ in 0..string_count {
        let len = br.read_uvarint()? as usize;
        let bytes = br.take(len)?;
        let s = std::str::from_utf8(bytes)
            .map_err(|_| FormatError::InvalidUtf8 {
                field: "snapshot string entry",
            })?
            .to_owned();
        new_strings.push(s);
    }
    let string_section_consumed = br.pos() - string_section_start;
    if string_section_consumed != string_section_length {
        return Err(FormatError::SectionLengthMismatch {
            section: "snapshot string table",
            expected: string_section_length as u64,
            consumed: string_section_consumed as u64,
        });
    }
    if string_count != string_table_size_after {
        return Err(FormatError::SnapshotCountMismatch {
            field: "string",
            expected: string_table_size_after,
            observed: string_count,
        });
    }

    // Value section: length u32 + count u32 + entries. We need to validate
    // value refs against the *new* string table, so install strings first.
    *strings = new_strings;

    let value_section_length = br.read_u32_le()? as usize;
    let value_section_start = br.pos();
    let value_count = br.read_u32_le()?;
    let mut new_values: Vec<ValueEntry> = Vec::with_capacity(value_count as usize);
    for i in 0..value_count {
        let entry = parse_value_table_entry(&mut br, i as ValueId, strings.len())?;
        new_values.push(entry);
    }
    let value_section_consumed = br.pos() - value_section_start;
    if value_section_consumed != value_section_length {
        return Err(FormatError::SectionLengthMismatch {
            section: "snapshot value table",
            expected: value_section_length as u64,
            consumed: value_section_consumed as u64,
        });
    }
    if value_count != value_table_size_after {
        return Err(FormatError::SnapshotCountMismatch {
            field: "value",
            expected: value_table_size_after,
            observed: value_count,
        });
    }
    *values = new_values;

    if br.remaining() != 0 {
        return Err(FormatError::SectionLengthMismatch {
            section: "snapshot payload",
            expected: 0,
            consumed: br.remaining() as u64,
        });
    }
    Ok(())
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
        0x05 => {
            let timestamp_delta_ns = br.read_uvarint()?;
            let line = read_u32_varint(br, "BRANCH_RESULT line")?;
            let taken_byte = br.read_u8()?;
            let taken = match taken_byte {
                0 => false,
                1 => true,
                other => return Err(FormatError::InvalidBoolByte(other)),
            };
            Event::BranchResult(BranchResult {
                timestamp_delta_ns,
                line,
                taken,
            })
        }
        0x06 => {
            let timestamp_delta_ns = br.read_uvarint()?;
            let line = read_u32_varint(br, "EXCEPTION_RAISED line")?;
            let exception_type = br.read_uvarint()?;
            let exception_value = br.read_uvarint()?;
            check_string(strings, exception_type)?;
            check_value(values, exception_value)?;
            Event::ExceptionRaised(ExceptionRaised {
                timestamp_delta_ns,
                line,
                exception_type,
                exception_value,
            })
        }
        0x07 => {
            let timestamp_delta_ns = br.read_uvarint()?;
            let line = read_u32_varint(br, "NOTE line")?;
            let message = br.read_uvarint()?;
            let kwarg_count = br.read_uvarint()? as usize;
            let mut kwargs = Vec::with_capacity(kwarg_count);
            for _ in 0..kwarg_count {
                let name = br.read_uvarint()?;
                let value = br.read_uvarint()?;
                check_string(strings, name)?;
                check_value(values, value)?;
                kwargs.push(Kwarg { name, value });
            }
            check_string(strings, message)?;
            Event::Note(Note {
                timestamp_delta_ns,
                line,
                message,
                kwargs,
            })
        }
        0x08 => {
            let timestamp_delta_ns = br.read_uvarint()?;
            let boundary_byte = br.read_u8()?;
            let boundary_type = BoundaryType::from_u8(boundary_byte)?;
            let reason = br.read_uvarint()?;
            check_string(strings, reason)?;
            Event::ScopeBoundary(ScopeBoundary {
                timestamp_delta_ns,
                boundary_type,
                reason,
            })
        }
        0x09 => {
            let timestamp_delta_ns = br.read_uvarint()?;
            let old_frame_id = br.read_uvarint()?;
            let new_frame_id = br.read_uvarint()?;
            let reason_byte = br.read_u8()?;
            let reason = FrameSwitchReason::from_u8(reason_byte)?;
            Event::FrameSwitch(FrameSwitch {
                timestamp_delta_ns,
                old_frame_id,
                new_frame_id,
                reason,
            })
        }
        // Strict-mode policy: error on event tags 0x0A+ rather than skip
        // them via the length prefix as the spec describes. See the strict-
        // mode comment block in `parse_event` of the previous revision.
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

fn parse_final_summary(br: &mut ByteReader) -> Result<FinalSummary> {
    let length = br.read_u32_le()? as usize;
    let payload_bytes = br.take(length)?;
    let payload = std::str::from_utf8(payload_bytes)
        .map_err(|_| FormatError::InvalidUtf8 {
            field: "final summary payload",
        })?
        .to_owned();
    Ok(FinalSummary { payload })
}

fn parse_checkpoint_index(br: &mut ByteReader) -> Result<Vec<CheckpointEntry>> {
    let length = br.read_u32_le()? as usize;
    let start = br.pos();
    let count = br.read_u32_le()? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let wall_clock_ns = br.read_u64_le()?;
        let event_id = br.read_u64_le()?;
        let file_offset = br.read_u64_le()?;
        let snapshot_offset = br.read_u64_le()?;
        entries.push(CheckpointEntry {
            wall_clock_ns,
            event_id,
            file_offset,
            snapshot_offset,
        });
    }
    let consumed = br.pos() - start;
    if consumed != length {
        return Err(FormatError::SectionLengthMismatch {
            section: "checkpoint index",
            expected: length as u64,
            consumed: consumed as u64,
        });
    }
    Ok(entries)
}

fn parse_footer(br: &mut ByteReader) -> Result<Footer> {
    let magic_bytes = br.take(8)?;
    let mut magic = [0u8; 8];
    magic.copy_from_slice(magic_bytes);
    if magic != FOOTER_MAGIC {
        return Err(FormatError::BadFooterMagic {
            expected: FOOTER_MAGIC,
            got: magic,
        });
    }
    let footer_length = br.read_u32_le()?;
    if footer_length != FOOTER_LENGTH {
        return Err(FormatError::BadFooterLength {
            expected: FOOTER_LENGTH,
            got: footer_length,
        });
    }
    let checkpoint_index_offset = br.read_u64_le()?;
    let final_summary_offset = br.read_u64_le()?;
    let reserved = br.read_u32_le()?;
    if reserved != 0 {
        return Err(FormatError::ReservedFieldNonzero("footer reserved tail"));
    }
    Ok(Footer {
        footer_length,
        checkpoint_index_offset,
        final_summary_offset,
    })
}

/// Read just the footer at `footer_offset` without disturbing the main parse
/// cursor. Used by `from_bytes` to learn the block-stream-end (which equals
/// the footer's `final_summary_offset`).
fn peek_footer(file_bytes: &[u8], footer_offset: u64) -> Result<Footer> {
    let start = footer_offset as usize;
    if start.saturating_add(FOOTER_LENGTH as usize) > file_bytes.len() {
        return Err(FormatError::Truncated);
    }
    let mut br = ByteReader::new(&file_bytes[start..start + FOOTER_LENGTH as usize]);
    parse_footer(&mut br)
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
    use crate::writer::{Finalization, ScopeResolution, TraceWriter};

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

    fn finalize() -> Finalization {
        Finalization {
            recording_end_ns: 1_700_001_000,
            scope_resolution: ScopeResolution::default(),
        }
    }

    fn write_minimal_finalized() -> Vec<u8> {
        TraceWriter::new(empty_metadata())
            .finish_to_bytes(finalize())
            .unwrap()
    }

    fn write_minimal_unfinalized() -> Vec<u8> {
        let w = TraceWriter::new(empty_metadata());
        let mut out = Vec::new();
        w.write_unfinalized(&mut out).unwrap();
        out
    }

    #[test]
    fn parse_header_extracts_fields_for_unfinalized_trace() {
        let bytes = write_minimal_unfinalized();
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
    fn parse_header_extracts_patched_fields_for_finalized_trace() {
        let bytes = write_minimal_finalized();
        let mut br = ByteReader::new(&bytes);
        let h = parse_header(&mut br).unwrap();
        assert_eq!(h.recording_end_ns, 1_700_001_000);
        assert!(h.footer_offset > 0);
    }

    #[test]
    fn from_bytes_full_pipeline_for_unfinalized_minimal_trace_works() {
        let bytes = write_minimal_unfinalized();
        let r = TraceReader::from_bytes(&bytes).unwrap();
        assert_eq!(r.header().trace_uuid, [0xAB; 16]);
        assert_eq!(r.events().len(), 0);
        assert!(r.source_files().is_empty());
        assert_eq!(r.values().len(), 2);
        assert!(r.final_summary().is_none());
        assert!(r.footer().is_none());
        assert!(r.checkpoints().is_empty());
        assert!(!r.is_finalized());
        // Even an empty trace has one event block.
        assert_eq!(r.event_blocks().len(), 1);
    }

    #[test]
    fn from_bytes_full_pipeline_for_finalized_minimal_trace_works() {
        let bytes = write_minimal_finalized();
        let r = TraceReader::from_bytes(&bytes).unwrap();
        assert!(r.final_summary().is_some());
        assert!(r.footer().is_some());
        assert!(r.checkpoints().is_empty());
        assert!(r.is_finalized());
        let summary = r.final_summary().unwrap();
        assert!(summary.payload.contains("[final]"));
        assert!(summary.payload.contains("clean_shutdown = true"));
    }

    #[test]
    fn finalized_trace_with_trailing_garbage_errors() {
        let mut bytes = write_minimal_finalized();
        bytes.push(0xAB);
        assert!(matches!(
            TraceReader::from_bytes(&bytes),
            Err(FormatError::TrailingBytesAfterFinalSection(1))
        ));
    }

    #[test]
    fn footer_round_trips_offsets() {
        let bytes = write_minimal_finalized();
        let r = TraceReader::from_bytes(&bytes).unwrap();
        let footer = r.footer().unwrap();
        assert_eq!(footer.footer_length, FOOTER_LENGTH);
        assert!(footer.final_summary_offset < footer.checkpoint_index_offset);
        assert!(footer.checkpoint_index_offset < r.header().footer_offset);
    }

    #[test]
    fn parse_checkpoint_index_empty() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let mut br = ByteReader::new(&bytes);
        let entries = parse_checkpoint_index(&mut br).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_checkpoint_index_one_entry() {
        let mut bytes = Vec::new();
        let body_len = 4 + 32;
        bytes.extend_from_slice(&(body_len as u32).to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&100u64.to_le_bytes());
        bytes.extend_from_slice(&7u64.to_le_bytes());
        bytes.extend_from_slice(&200u64.to_le_bytes());
        bytes.extend_from_slice(&300u64.to_le_bytes());
        let mut br = ByteReader::new(&bytes);
        let entries = parse_checkpoint_index(&mut br).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].wall_clock_ns, 100);
        assert_eq!(entries[0].event_id, 7);
        assert_eq!(entries[0].file_offset, 200);
        assert_eq!(entries[0].snapshot_offset, 300);
    }

    #[test]
    fn seek_unfinalized_errors() {
        let bytes = write_minimal_unfinalized();
        let r = TraceReader::from_bytes(&bytes).unwrap();
        assert!(matches!(
            r.seek_to_event_id(0),
            Err(FormatError::SeekUnfinalized)
        ));
        assert!(matches!(
            r.seek_to_wall_clock(0),
            Err(FormatError::SeekUnfinalized)
        ));
    }
}
