// SPDX-License-Identifier: Apache-2.0

//! Binary trace format reader and writer for Hindsight.
//!
//! The format is specified in `docs/trace-format.md` (v0.2). This crate
//! currently implements a subset of the *writer* side: file header, initial
//! metadata block, source bundle, initial string and value tables, and one
//! event block holding FUNCTION_ENTRY, FUNCTION_EXIT, FRAME_SNAPSHOT, and
//! LINE_DELTA events. The reader, footer, final summary, checkpoints, table
//! updates, table snapshots, and the remaining event types are not yet
//! implemented.

mod canonical;
mod encoding;
mod error;
mod event;
mod metadata;
mod value;
mod varint;
mod writer;

pub use error::{FormatError, Result};
pub use event::{
    Argument, Change, EventTag, FrameId, FrameSnapshot, FunctionEntry, FunctionExit, LineDelta,
    Local,
};
pub use metadata::{Metadata, ProgramInfo, RecorderInfo, RecordingInfo, ScopeConfig};
pub use value::{HashKind, StringId, Value, ValueId, ValueTag};
pub use writer::{
    BLOCK_TAG_EVENT, EXCEPTION_UNWIND_VALUE_ID, FILE_MAGIC, FORMAT_VERSION_MAJOR,
    FORMAT_VERSION_MINOR, FileId, HEADER_LENGTH, METADATA_FORMAT_TAG_TOML, NONE_VALUE_ID,
    TraceWriter,
};
