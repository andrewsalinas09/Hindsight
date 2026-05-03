// SPDX-License-Identifier: Apache-2.0

//! Event types and event payload encoding.
//!
//! The four event types implemented here are FUNCTION_ENTRY, FUNCTION_EXIT,
//! FRAME_SNAPSHOT, and LINE_DELTA. Other event types in the spec
//! (BRANCH_RESULT, EXCEPTION_RAISED, NOTE, SCOPE_BOUNDARY, FRAME_SWITCH) are
//! out of scope for this revision.

use std::io::{self, Write};

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
}

impl EventTag {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone)]
pub struct Argument {
    pub name: StringId,
    pub value: ValueId,
}

#[derive(Debug, Clone)]
pub struct Local {
    pub name: StringId,
    pub value: ValueId,
}

#[derive(Debug, Clone)]
pub struct Change {
    pub name: StringId,
    pub value: ValueId,
}

#[derive(Debug, Clone)]
pub struct FunctionEntry {
    pub timestamp_delta_ns: u64,
    pub frame_id: FrameId,
    pub function_id: StringId,
    pub source_file_id: u64,
    pub line: u32,
    pub args: Vec<Argument>,
}

#[derive(Debug, Clone)]
pub struct FunctionExit {
    pub timestamp_delta_ns: u64,
    pub frame_id: FrameId,
    pub return_value: ValueId,
}

#[derive(Debug, Clone)]
pub struct FrameSnapshot {
    pub timestamp_delta_ns: u64,
    pub frame_id: FrameId,
    pub line: u32,
    pub locals: Vec<Local>,
}

#[derive(Debug, Clone)]
pub struct LineDelta {
    pub timestamp_delta_ns: u64,
    pub line: u32,
    pub changes: Vec<Change>,
}

/// One of the event types this writer can emit.
#[derive(Debug, Clone)]
pub(crate) enum Event {
    FunctionEntry(FunctionEntry),
    FunctionExit(FunctionExit),
    FrameSnapshot(FrameSnapshot),
    LineDelta(LineDelta),
}

impl Event {
    pub(crate) fn tag(&self) -> EventTag {
        match self {
            Event::FunctionEntry(_) => EventTag::FunctionEntry,
            Event::FunctionExit(_) => EventTag::FunctionExit,
            Event::FrameSnapshot(_) => EventTag::FrameSnapshot,
            Event::LineDelta(_) => EventTag::LineDelta,
        }
    }
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
            // No frame_id: implicit, per spec §LINE_DELTA.
            write_uvarint(w, e.timestamp_delta_ns)?;
            write_uvarint(w, u64::from(e.line))?;
            write_uvarint(w, e.changes.len() as u64)?;
            for change in &e.changes {
                write_uvarint(w, change.name)?;
                write_uvarint(w, change.value)?;
            }
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
}
