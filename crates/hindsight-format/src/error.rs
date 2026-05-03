// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FormatError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown string ID: {0}")]
    UnknownStringId(u64),
    #[error("unknown value ID: {0}")]
    UnknownValueId(u64),
    #[error("unknown source file ID: {0}")]
    UnknownFileId(u64),
    #[error("source path too long: {0} bytes (max 65535)")]
    PathTooLong(usize),
    #[error("source content too long: {0} bytes (max u32::MAX)")]
    SourceTooLong(usize),
    #[error("metadata block too large: {0} bytes (max u32::MAX)")]
    MetadataTooLarge(usize),
    #[error("section too large: {0} bytes (max u32::MAX)")]
    SectionTooLarge(usize),
    #[error("zstd compression failed: {0}")]
    Compression(String),

    // --- Reader-side variants ---
    /// The buffer ran out before a structured read could complete.
    #[error("trace truncated: not enough bytes to satisfy a read")]
    Truncated,
    #[error("bad file magic: expected {expected:?}, got {got:?}")]
    BadMagic { expected: [u8; 8], got: [u8; 8] },
    #[error("unsupported format version {major}.{minor}")]
    UnsupportedVersion { major: u8, minor: u8 },
    #[error("bad header length: expected {expected}, got {got}")]
    BadHeaderLength { expected: u32, got: u32 },
    #[error("reserved field nonzero: {0}")]
    ReservedFieldNonzero(&'static str),
    #[error("unsupported metadata format tag {0:#x}")]
    UnsupportedMetadataFormatTag(u8),
    #[error("invalid utf-8 in {field}")]
    InvalidUtf8 { field: &'static str },
    #[error("source file {file_id}: blake3 hash does not match content")]
    SourceHashMismatch { file_id: u64 },
    #[error("invalid value type tag {0:#x}")]
    InvalidValueTag(u8),
    #[error("invalid hash kind {0:#x}")]
    InvalidHashKind(u8),
    #[error("varint overflows u64")]
    VarintOverflow,
    #[error("invalid bool byte {0:#x} (expected 0 or 1)")]
    InvalidBoolByte(u8),
    #[error(
        "{section} section length mismatch: declared {expected} bytes, parsed {consumed} bytes"
    )]
    SectionLengthMismatch {
        section: &'static str,
        expected: u64,
        consumed: u64,
    },
    /// A value table entry's encoded payload had bytes left over after
    /// decoding finished — i.e., the on-disk length does not match the value
    /// the type tag describes.
    #[error("trailing bytes in value table entry of tag {tag:#x}: {leftover} byte(s) left over")]
    ValueDataLeftover { tag: u8, leftover: usize },
    /// A container value (list, set, dict) referenced a value ID that hasn't
    /// been defined yet at this point in the value table.
    #[error("forward value reference: id {id} referenced from value at index {at_index}")]
    ForwardValueRef { id: u64, at_index: u64 },
    #[error("{field} value out of range")]
    FieldOverflow { field: &'static str },
    /// This reader only supports event blocks (tag 0x01). Checkpoint, table
    /// update, and table snapshot blocks (0x02–0x04) are intentionally out of
    /// scope for the current revision.
    #[error("unsupported block tag {0:#x}")]
    UnsupportedBlockTag(u8),
    /// Only FUNCTION_ENTRY/EXIT, FRAME_SNAPSHOT and LINE_DELTA events are
    /// supported; other tags (BRANCH_RESULT, EXCEPTION_RAISED, NOTE,
    /// SCOPE_BOUNDARY, FRAME_SWITCH, reserved) produce this error rather than
    /// being silently skipped.
    #[error("unsupported event tag {0:#x}")]
    UnsupportedEventTag(u8),
    #[error("CRC32C mismatch: expected {expected:#010x}, got {got:#010x}")]
    ChecksumMismatch { expected: u32, got: u32 },
    #[error("decompressed length mismatch: expected {expected}, got {got}")]
    DecompressedLengthMismatch { expected: u64, got: u64 },
    #[error("zstd decompression failed: {0}")]
    Decompression(String),
    /// Bytes left in the file after the last expected section. For
    /// finalized files (header.footer_offset != 0) the last expected section
    /// is the footer; for unfinalized files the reader does not enforce an
    /// end-of-file marker (per the spec's crash-recovery semantics) so this
    /// error is only emitted on the finalized path.
    #[error("trailing bytes after final section: {0} byte(s) left in file")]
    TrailingBytesAfterFinalSection(usize),
    #[error("trailing bytes inside event block payload: {0} byte(s) after the last event")]
    TrailingBytesInEventBlock(usize),
    /// SCOPE_BOUNDARY event payload's `boundary_type` byte was not one of the
    /// values defined by the spec.
    #[error("invalid SCOPE_BOUNDARY boundary type {0:#x}")]
    InvalidBoundaryType(u8),
    /// FRAME_SWITCH event payload's `reason` byte was not one of the values
    /// defined by the spec.
    #[error("invalid FRAME_SWITCH reason {0:#x}")]
    InvalidFrameSwitchReason(u8),
    #[error("bad footer magic: expected {expected:?}, got {got:?}")]
    BadFooterMagic { expected: [u8; 8], got: [u8; 8] },
    #[error("bad footer length: expected {expected}, got {got}")]
    BadFooterLength { expected: u32, got: u32 },
    /// The footer's `final_summary_offset` or `checkpoint_index_offset` did
    /// not match where the reader actually parsed those sections.
    #[error("{field} mismatch: footer says {expected}, reader observed {observed}")]
    FooterOffsetMismatch {
        field: &'static str,
        expected: u64,
        observed: u64,
    },
    /// The header's `footer_offset` did not match where the reader actually
    /// parsed the footer.
    #[error("header footer_offset mismatch: header says {expected}, reader observed {observed}")]
    HeaderFooterOffsetMismatch { expected: u64, observed: u64 },
    /// Event length prefix was zero — at minimum the type tag byte must fit.
    #[error("event with zero length prefix")]
    EmptyEvent,
    #[error("{section} table size mismatch: block header says {expected} entries, parsed {actual}")]
    TableSizeMismatch {
        section: &'static str,
        expected: u32,
        actual: u32,
    },
}

pub type Result<T> = std::result::Result<T, FormatError>;
