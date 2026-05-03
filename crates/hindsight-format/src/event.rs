// SPDX-License-Identifier: Apache-2.0

//! Event types and event payload encoding.
//!
//! All nine v0.2 event types are implemented here:
//! FUNCTION_ENTRY, FUNCTION_EXIT, FRAME_SNAPSHOT, LINE_DELTA, BRANCH_RESULT,
//! EXCEPTION_RAISED, NOTE, SCOPE_BOUNDARY, FRAME_SWITCH.

use std::io::{self, Write};

use crate::error::{FormatError, Result};
use crate::value::{StringId, ValueId};
use crate::varint::write_uvarint;

pub type FrameId = u64;

/// Event type tag bytes. Matches `docs/trace-format.md` §"Event types".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EventTag {
    FunctionEntry = 0x01,
    FunctionExit = 0x02,
    FrameSnapshot = 0x03,
    LineDelta = 0x04,
    BranchResult = 0x05,
    ExceptionRaised = 0x06,
    Note = 0x07,
    ScopeBoundary = 0x08,
    FrameSwitch = 0x09,
}

impl EventTag {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Reason a SCOPE_BOUNDARY event was emitted. See spec §"SCOPE_BOUNDARY".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BoundaryType {
    EnteredSkip = 0x01,
    ExitedSkip = 0x02,
    EnteredExcluded = 0x03,
    ExitedExcluded = 0x04,
    EnteredDepthClipped = 0x05,
    ExitedDepthClipped = 0x06,
}

impl BoundaryType {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(b: u8) -> Result<Self> {
        Ok(match b {
            0x01 => Self::EnteredSkip,
            0x02 => Self::ExitedSkip,
            0x03 => Self::EnteredExcluded,
            0x04 => Self::ExitedExcluded,
            0x05 => Self::EnteredDepthClipped,
            0x06 => Self::ExitedDepthClipped,
            _ => return Err(FormatError::InvalidBoundaryType(b)),
        })
    }
}

/// Reason a FRAME_SWITCH event was emitted. See spec §"FRAME_SWITCH".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameSwitchReason {
    GeneratorYield = 0x01,
    GeneratorResume = 0x02,
    AsyncTaskSwitch = 0x03,
    ExceptionPartialUnwind = 0x04,
}

impl FrameSwitchReason {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(b: u8) -> Result<Self> {
        Ok(match b {
            0x01 => Self::GeneratorYield,
            0x02 => Self::GeneratorResume,
            0x03 => Self::AsyncTaskSwitch,
            0x04 => Self::ExceptionPartialUnwind,
            _ => return Err(FormatError::InvalidFrameSwitchReason(b)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argument {
    pub name: StringId,
    pub value: ValueId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub name: StringId,
    pub value: ValueId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub name: StringId,
    pub value: ValueId,
}

/// One keyword argument attached to a NOTE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kwarg {
    pub name: StringId,
    pub value: ValueId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionEntry {
    pub timestamp_delta_ns: u64,
    pub frame_id: FrameId,
    pub function_id: StringId,
    pub source_file_id: u64,
    pub line: u32,
    pub args: Vec<Argument>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionExit {
    pub timestamp_delta_ns: u64,
    pub frame_id: FrameId,
    pub return_value: ValueId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSnapshot {
    pub timestamp_delta_ns: u64,
    pub frame_id: FrameId,
    pub line: u32,
    pub locals: Vec<Local>,
}

/// TODO(v0.3-spec): in v0.2 the frame is implicit (the most recent
/// FUNCTION_ENTRY / FRAME_SWITCH). The v0.3 spec is expected to make
/// `frame_id` explicit on all frame-scoped events including LINE_DELTA;
/// when that lands, add the field here, in the payload writer below, and
/// in the worked-example test that locks the on-disk byte layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineDelta {
    pub timestamp_delta_ns: u64,
    pub line: u32,
    pub changes: Vec<Change>,
}

/// A conditional branch was evaluated. See spec §"BRANCH_RESULT".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchResult {
    pub timestamp_delta_ns: u64,
    pub line: u32,
    /// `true` if the branch was taken (the condition evaluated truthy).
    pub taken: bool,
}

/// An exception was raised. See spec §"EXCEPTION_RAISED".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionRaised {
    pub timestamp_delta_ns: u64,
    pub line: u32,
    /// String ID of the qualified exception class name (e.g.,
    /// `"ValueError"`, `"myapp.errors.NotFound"`).
    pub exception_type: StringId,
    /// Value ID of the exception instance summary.
    pub exception_value: ValueId,
}

/// A user-emitted `hindsight.note(...)` call. See spec §"NOTE".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub timestamp_delta_ns: u64,
    pub line: u32,
    /// String ID of the note message (the positional argument).
    pub message: StringId,
    /// Optional structured key/value pairs from `**kwargs`.
    pub kwargs: Vec<Kwarg>,
}

/// Recording crossed a scope boundary. See spec §"SCOPE_BOUNDARY".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeBoundary {
    pub timestamp_delta_ns: u64,
    pub boundary_type: BoundaryType,
    /// String ID of a free-form reason (e.g., `"matched pattern: numpy.*"`).
    pub reason: StringId,
}

/// Execution switched to a different frame. See spec §"FRAME_SWITCH".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSwitch {
    pub timestamp_delta_ns: u64,
    pub old_frame_id: FrameId,
    pub new_frame_id: FrameId,
    pub reason: FrameSwitchReason,
}

/// One of the event types this writer can emit and the reader can produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    FunctionEntry(FunctionEntry),
    FunctionExit(FunctionExit),
    FrameSnapshot(FrameSnapshot),
    LineDelta(LineDelta),
    BranchResult(BranchResult),
    ExceptionRaised(ExceptionRaised),
    Note(Note),
    ScopeBoundary(ScopeBoundary),
    FrameSwitch(FrameSwitch),
}

impl Event {
    pub fn tag(&self) -> EventTag {
        match self {
            Event::FunctionEntry(_) => EventTag::FunctionEntry,
            Event::FunctionExit(_) => EventTag::FunctionExit,
            Event::FrameSnapshot(_) => EventTag::FrameSnapshot,
            Event::LineDelta(_) => EventTag::LineDelta,
            Event::BranchResult(_) => EventTag::BranchResult,
            Event::ExceptionRaised(_) => EventTag::ExceptionRaised,
            Event::Note(_) => EventTag::Note,
            Event::ScopeBoundary(_) => EventTag::ScopeBoundary,
            Event::FrameSwitch(_) => EventTag::FrameSwitch,
        }
    }

    /// Nanoseconds elapsed since the previous event in the trace. Used by
    /// the writer to track wall-clock progress for checkpoint timing.
    pub fn timestamp_delta_ns(&self) -> u64 {
        match self {
            Event::FunctionEntry(e) => e.timestamp_delta_ns,
            Event::FunctionExit(e) => e.timestamp_delta_ns,
            Event::FrameSnapshot(e) => e.timestamp_delta_ns,
            Event::LineDelta(e) => e.timestamp_delta_ns,
            Event::BranchResult(e) => e.timestamp_delta_ns,
            Event::ExceptionRaised(e) => e.timestamp_delta_ns,
            Event::Note(e) => e.timestamp_delta_ns,
            Event::ScopeBoundary(e) => e.timestamp_delta_ns,
            Event::FrameSwitch(e) => e.timestamp_delta_ns,
        }
    }
}

/// Exact serialized size of an event in bytes, as the writer would emit it
/// (length varint + tag byte + payload). Used by the writer to decide when
/// the pending event buffer has reached its block-size threshold; computed
/// by serializing into a scratch buffer (cheap — events are tens of bytes).
pub(crate) fn event_serialized_size(event: &Event) -> usize {
    let mut buf = Vec::new();
    write_event(&mut buf, event).expect("Vec write");
    buf.len()
}

/// Write an event's wire form: varint(event_length) + tag byte + payload.
/// `event_length` includes the tag byte. See spec §"Event block payload".
pub(crate) fn write_event<W: Write>(w: &mut W, event: &Event) -> io::Result<()> {
    // Encode payload to a scratch buffer so we can length-prefix.
    let mut payload = Vec::new();
    write_event_payload(&mut payload, event)?;
    let event_length = 1 + payload.len() as u64;
    write_uvarint(w, event_length)?;
    w.write_all(&[event.tag().as_u8()])?;
    w.write_all(&payload)?;
    Ok(())
}

fn write_event_payload<W: Write>(w: &mut W, event: &Event) -> io::Result<()> {
    match event {
        Event::FunctionEntry(e) => {
            write_uvarint(w, e.timestamp_delta_ns)?;
            write_uvarint(w, e.frame_id)?;
            write_uvarint(w, e.function_id)?;
            write_uvarint(w, e.source_file_id)?;
            write_uvarint(w, u64::from(e.line))?;
            write_uvarint(w, e.args.len() as u64)?;
            for arg in &e.args {
                write_uvarint(w, arg.name)?;
                write_uvarint(w, arg.value)?;
            }
        }
        Event::FunctionExit(e) => {
            write_uvarint(w, e.timestamp_delta_ns)?;
            write_uvarint(w, e.frame_id)?;
            write_uvarint(w, e.return_value)?;
        }
        Event::FrameSnapshot(e) => {
            write_uvarint(w, e.timestamp_delta_ns)?;
            write_uvarint(w, e.frame_id)?;
            write_uvarint(w, u64::from(e.line))?;
            write_uvarint(w, e.locals.len() as u64)?;
            for local in &e.locals {
                write_uvarint(w, local.name)?;
                write_uvarint(w, local.value)?;
            }
        }
        Event::LineDelta(e) => {
            // No frame_id: implicit, per v0.2 spec §LINE_DELTA.
            // TODO(v0.3-spec): emit frame_id varint here when v0.3 makes it
            // explicit. See the doc comment on `LineDelta`.
            write_uvarint(w, e.timestamp_delta_ns)?;
            write_uvarint(w, u64::from(e.line))?;
            write_uvarint(w, e.changes.len() as u64)?;
            for change in &e.changes {
                write_uvarint(w, change.name)?;
                write_uvarint(w, change.value)?;
            }
        }
        Event::BranchResult(e) => {
            write_uvarint(w, e.timestamp_delta_ns)?;
            write_uvarint(w, u64::from(e.line))?;
            w.write_all(&[u8::from(e.taken)])?;
        }
        Event::ExceptionRaised(e) => {
            write_uvarint(w, e.timestamp_delta_ns)?;
            write_uvarint(w, u64::from(e.line))?;
            write_uvarint(w, e.exception_type)?;
            write_uvarint(w, e.exception_value)?;
        }
        Event::Note(e) => {
            write_uvarint(w, e.timestamp_delta_ns)?;
            write_uvarint(w, u64::from(e.line))?;
            write_uvarint(w, e.message)?;
            write_uvarint(w, e.kwargs.len() as u64)?;
            for kw in &e.kwargs {
                write_uvarint(w, kw.name)?;
                write_uvarint(w, kw.value)?;
            }
        }
        Event::ScopeBoundary(e) => {
            write_uvarint(w, e.timestamp_delta_ns)?;
            w.write_all(&[e.boundary_type.as_u8()])?;
            write_uvarint(w, e.reason)?;
        }
        Event::FrameSwitch(e) => {
            write_uvarint(w, e.timestamp_delta_ns)?;
            write_uvarint(w, e.old_frame_id)?;
            write_uvarint(w, e.new_frame_id)?;
            w.write_all(&[e.reason.as_u8()])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(event: &Event) -> Vec<u8> {
        let mut buf = Vec::new();
        write_event(&mut buf, event).unwrap();
        buf
    }

    #[test]
    fn empty_line_delta_is_compact() {
        // No changes: just timestamp_delta varint, line varint, zero count varint.
        let bytes = encode(&Event::LineDelta(LineDelta {
            timestamp_delta_ns: 50,
            line: 3,
            changes: vec![],
        }));
        // event_length = 1 (tag) + 3 (three single-byte varints) = 4
        assert_eq!(bytes[0], 0x04, "event_length");
        assert_eq!(bytes[1], EventTag::LineDelta.as_u8());
        assert_eq!(&bytes[2..], &[50u8, 3, 0]);
    }

    #[test]
    fn function_exit_layout() {
        let bytes = encode(&Event::FunctionExit(FunctionExit {
            timestamp_delta_ns: 100,
            frame_id: 0,
            return_value: 5,
        }));
        // event_length = 1 (tag) + 3 (three single-byte varints) = 4
        assert_eq!(bytes[0], 0x04);
        assert_eq!(bytes[1], EventTag::FunctionExit.as_u8());
        assert_eq!(&bytes[2..], &[100u8, 0, 5]);
    }

    #[test]
    fn branch_result_layout() {
        // event_length = 1 (tag) + 2 varints + 1 byte = 4.
        let bytes = encode(&Event::BranchResult(BranchResult {
            timestamp_delta_ns: 7,
            line: 12,
            taken: true,
        }));
        assert_eq!(bytes[0], 4);
        assert_eq!(bytes[1], EventTag::BranchResult.as_u8());
        assert_eq!(&bytes[2..], &[7, 12, 1]);
    }

    #[test]
    fn scope_boundary_layout() {
        // event_length = 1 (tag) + ts varint + 1 byte type + reason varint = 4.
        let bytes = encode(&Event::ScopeBoundary(ScopeBoundary {
            timestamp_delta_ns: 0,
            boundary_type: BoundaryType::EnteredSkip,
            reason: 9,
        }));
        assert_eq!(bytes[0], 4);
        assert_eq!(bytes[1], EventTag::ScopeBoundary.as_u8());
        assert_eq!(&bytes[2..], &[0, 0x01, 9]);
    }

    #[test]
    fn frame_switch_layout() {
        // event_length = 1 (tag) + 3 varints + 1 byte = 5.
        let bytes = encode(&Event::FrameSwitch(FrameSwitch {
            timestamp_delta_ns: 1,
            old_frame_id: 2,
            new_frame_id: 3,
            reason: FrameSwitchReason::AsyncTaskSwitch,
        }));
        assert_eq!(bytes[0], 5);
        assert_eq!(bytes[1], EventTag::FrameSwitch.as_u8());
        assert_eq!(&bytes[2..], &[1, 2, 3, 0x03]);
    }

    #[test]
    fn boundary_type_round_trip() {
        for bt in [
            BoundaryType::EnteredSkip,
            BoundaryType::ExitedSkip,
            BoundaryType::EnteredExcluded,
            BoundaryType::ExitedExcluded,
            BoundaryType::EnteredDepthClipped,
            BoundaryType::ExitedDepthClipped,
        ] {
            assert_eq!(BoundaryType::from_u8(bt.as_u8()).unwrap(), bt);
        }
        assert!(matches!(
            BoundaryType::from_u8(0x00),
            Err(FormatError::InvalidBoundaryType(0x00))
        ));
        assert!(matches!(
            BoundaryType::from_u8(0x07),
            Err(FormatError::InvalidBoundaryType(0x07))
        ));
    }

    #[test]
    fn frame_switch_reason_round_trip() {
        for r in [
            FrameSwitchReason::GeneratorYield,
            FrameSwitchReason::GeneratorResume,
            FrameSwitchReason::AsyncTaskSwitch,
            FrameSwitchReason::ExceptionPartialUnwind,
        ] {
            assert_eq!(FrameSwitchReason::from_u8(r.as_u8()).unwrap(), r);
        }
        assert!(matches!(
            FrameSwitchReason::from_u8(0x00),
            Err(FormatError::InvalidFrameSwitchReason(0x00))
        ));
        assert!(matches!(
            FrameSwitchReason::from_u8(0x05),
            Err(FormatError::InvalidFrameSwitchReason(0x05))
        ));
    }
}
