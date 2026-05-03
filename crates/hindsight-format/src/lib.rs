// SPDX-License-Identifier: Apache-2.0

//! Binary trace format reader and writer for Hindsight.
//!
//! The format is specified in `docs/trace-format.md` (v0.2). This crate
//! implements a v0.2 writer that produces a fully-finalized file (header,
//! initial metadata, source bundle, initial string and value tables, one
//! event block carrying any of the nine v0.2 event types, final summary,
//! checkpoint index, and footer) and a matching reader that handles both
//! finalized and unfinalized (interrupted) files. Multi-block streams,
//! checkpoints, table updates, and table snapshots are not yet implemented.

mod byte_reader;
mod canonical;
mod decoding;
mod encoding;
mod error;
mod event;
mod metadata;
mod reader;
mod value;
mod varint;
mod writer;

pub use error::{FormatError, Result};
pub use event::{
    Argument, BoundaryType, BranchResult, Change, Event, EventTag, ExceptionRaised, FrameId,
    FrameSnapshot, FrameSwitch, FrameSwitchReason, FunctionEntry, FunctionExit, Kwarg, LineDelta,
    Local, Note, ScopeBoundary,
};
pub use metadata::{Metadata, ProgramInfo, RecorderInfo, RecordingInfo, ScopeConfig};
pub use reader::{
    CheckpointEntry, EventBlockInfo, FinalSummary, Footer, Header, MetadataBlock, SourceFile,
    TraceCursor, TraceReader, ValueEntry,
};
pub use value::{HashKind, StringId, Value, ValueId, ValueTag};
pub use writer::{
    BLOCK_TAG_CHECKPOINT, BLOCK_TAG_EVENT, BLOCK_TAG_TABLE_SNAPSHOT, BLOCK_TAG_TABLE_UPDATE,
    EXCEPTION_UNWIND_VALUE_ID, ExcludedFunction, FILE_MAGIC, FOOTER_LENGTH, FOOTER_MAGIC,
    FORMAT_VERSION_MAJOR, FORMAT_VERSION_MINOR, FileId, Finalization, HEADER_LENGTH,
    METADATA_FORMAT_TAG_TOML, NONE_VALUE_ID, ScopeResolution, TraceWriter, WriterConfig,
};
