// SPDX-License-Identifier: Apache-2.0

//! First-pass frame computation. Walk events in order, tracking the call
//! stack and building one `FrameRow` per FUNCTION_ENTRY. After processing
//! every event we have everything the `frames` table needs (depth,
//! call_index, parent, exit info, duration, argument_summary).
//!
//! The pass also produces a per-event side table:
//!
//! - `event_frame_id[i]`: the frame_id the i-th event implicitly belongs to
//!   (the most recent FUNCTION_ENTRY frame, or 0 if no recorded frame is
//!   active — should not happen for well-formed traces from the in-tree
//!   recorder, which always wraps a `@hindsight.record`).
//! - `event_timestamp_ns[i]`: cumulative ns since recording start.
//!
//! These are consumed by the second pass to denormalize `function_name` /
//! `source_file` and to populate `events.timestamp_ns` directly.

use std::collections::HashMap;

use hindsight_format::{EXCEPTION_UNWIND_VALUE_ID, Event, ValueEntry};

use crate::error::{IndexError, Result};

#[derive(Debug, Clone)]
pub struct FrameRow {
    pub frame_id: u64,
    pub function_name: String,
    pub qualified_name: String,
    pub source_file: String,
    pub parent_frame_id: Option<u64>,
    pub entry_event_id: u64,
    pub exit_event_id: Option<u64>,
    pub exit_kind: String,
    pub depth: i32,
    pub call_index: i32,
    pub duration_ns: Option<i64>,
    pub argument_summary: Option<String>,
}

/// Output of pass one: the populated `frames` table plus per-event
/// implicit frame_id and absolute timestamp.
pub struct FramePass {
    pub frames: Vec<FrameRow>,
    pub event_frame_id: Vec<u64>,
    pub event_timestamp_ns: Vec<i64>,
}

/// Stack entry for an active recorded frame.
struct StackEntry {
    frame_index: usize,
    entry_timestamp_ns: i64,
}

pub fn compute_frames(
    events: &[Event],
    sources: &[hindsight_format::SourceFile],
    strings: &[String],
    values: &[ValueEntry],
) -> Result<FramePass> {
    let mut frames: Vec<FrameRow> = Vec::new();
    let mut event_frame_id: Vec<u64> = Vec::with_capacity(events.len());
    let mut event_timestamp_ns: Vec<i64> = Vec::with_capacity(events.len());

    let mut stack: Vec<StackEntry> = Vec::new();
    let mut call_index_by_qualname: HashMap<String, i32> = HashMap::new();
    let mut frame_index_by_id: HashMap<u64, usize> = HashMap::new();
    let mut current_ts_ns: i64 = 0;

    for (i, event) in events.iter().enumerate() {
        current_ts_ns = current_ts_ns.saturating_add(event.timestamp_delta_ns() as i64);
        let event_id = i as u64;

        match event {
            Event::FunctionEntry(e) => {
                let qualified_name = lookup_string(strings, e.function_id)?.to_string();
                let function_name = bare_name(&qualified_name).to_string();
                let source_file = lookup_source(sources, e.source_file_id)?.to_string();

                let parent_frame_id = stack.last().map(|s| frames[s.frame_index].frame_id);
                let depth = match stack.last() {
                    Some(top) => frames[top.frame_index].depth + 1,
                    None => 0,
                };
                let call_index = {
                    let entry = call_index_by_qualname
                        .entry(qualified_name.clone())
                        .or_insert(0);
                    let v = *entry;
                    *entry += 1;
                    v
                };

                let argument_summary = build_argument_summary(&e.args, strings, values)?;

                let frame_index = frames.len();
                frames.push(FrameRow {
                    frame_id: e.frame_id,
                    function_name,
                    qualified_name,
                    source_file,
                    parent_frame_id,
                    entry_event_id: event_id,
                    exit_event_id: None,
                    exit_kind: "still_running".to_string(),
                    depth,
                    call_index,
                    duration_ns: None,
                    argument_summary: Some(argument_summary),
                });
                frame_index_by_id.insert(e.frame_id, frame_index);
                stack.push(StackEntry {
                    frame_index,
                    entry_timestamp_ns: current_ts_ns,
                });

                event_frame_id.push(e.frame_id);
            }
            Event::FunctionExit(e) => {
                event_frame_id.push(e.frame_id);
                let stack_top_idx = stack
                    .iter()
                    .rposition(|s| frames[s.frame_index].frame_id == e.frame_id);
                if let Some(pos) = stack_top_idx {
                    let popped = stack.remove(pos);
                    let frame = &mut frames[popped.frame_index];
                    frame.exit_event_id = Some(event_id);
                    frame.duration_ns =
                        Some(current_ts_ns.saturating_sub(popped.entry_timestamp_ns));
                    frame.exit_kind = if e.return_value == EXCEPTION_UNWIND_VALUE_ID {
                        "raised".to_string()
                    } else {
                        "returned".to_string()
                    };
                }
                // If frame_id wasn't on the stack, the trace is malformed but
                // we don't fail — the frames table still reflects what we did
                // see. The events table records the FUNCTION_EXIT regardless.
            }
            Event::FrameSnapshot(e) => {
                event_frame_id.push(e.frame_id);
            }
            Event::FrameSwitch(e) => {
                event_frame_id.push(e.new_frame_id);
                // FRAME_SWITCH adjusts the implicit frame for subsequent
                // implicit-frame events. Push the new top onto the stack
                // representation by emulating what the recorder would have
                // tracked — but for v0 the recorder doesn't actually emit
                // FRAME_SWITCH events (Python is single-threaded recording),
                // so this branch is for future-proofing.
                if let Some(&new_idx) = frame_index_by_id.get(&e.new_frame_id)
                    && !stack.iter().any(|s| s.frame_index == new_idx)
                {
                    stack.push(StackEntry {
                        frame_index: new_idx,
                        entry_timestamp_ns: current_ts_ns,
                    });
                }
            }
            // All other events have an implicit frame: the most recent
            // recorded frame on the stack.
            Event::LineDelta(_)
            | Event::BranchResult(_)
            | Event::ExceptionRaised(_)
            | Event::Note(_)
            | Event::ScopeBoundary(_) => {
                let frame_id = stack
                    .last()
                    .map(|s| frames[s.frame_index].frame_id)
                    .unwrap_or(0);
                event_frame_id.push(frame_id);
            }
        }

        event_timestamp_ns.push(current_ts_ns);
    }

    Ok(FramePass {
        frames,
        event_frame_id,
        event_timestamp_ns,
    })
}

/// "module.path.func" → "func"; "module.Cls.method" → "method".
fn bare_name(qualified: &str) -> &str {
    qualified.rsplit_once('.').map_or(qualified, |(_, t)| t)
}

fn lookup_string(strings: &[String], id: u64) -> Result<&str> {
    strings
        .get(id as usize)
        .map(|s| s.as_str())
        .ok_or_else(|| IndexError::Internal(format!("string id {id} out of range")))
}

fn lookup_source(sources: &[hindsight_format::SourceFile], file_id: u64) -> Result<&str> {
    sources
        .iter()
        .find(|f| f.file_id == file_id)
        .map(|f| f.path.as_str())
        .ok_or_else(|| IndexError::Internal(format!("source file id {file_id} not found")))
}

/// Build the `name=value, name=value, ...` argument summary, with each value
/// rendered via `short_value_repr` and the whole string truncated to 200
/// chars (with a trailing `...` ellipsis if truncation occurred).
fn build_argument_summary(
    args: &[hindsight_format::Argument],
    strings: &[String],
    values: &[ValueEntry],
) -> Result<String> {
    let mut parts: Vec<String> = Vec::with_capacity(args.len());
    for arg in args {
        let name = lookup_string(strings, arg.name)?;
        let val_repr = short_value_repr(values, arg.value, strings);
        parts.push(format!("{name}={val_repr}"));
    }
    let joined = parts.join(", ");
    Ok(truncate_with_ellipsis(&joined, 200))
}

fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

/// Short repr of a value for argument summaries. Uses the trace's value
/// table; container reprs are placeholders since reading the full content
/// for every arg would be expensive. Summaries get their stored repr_text
/// (which is exactly what Python's repr() would produce, truncated at
/// recording time).
fn short_value_repr(values: &[ValueEntry], id: u64, strings: &[String]) -> String {
    let Some(entry) = values.get(id as usize) else {
        return "?".to_string();
    };
    match &entry.value {
        hindsight_format::Value::None => "None".to_string(),
        hindsight_format::Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        hindsight_format::Value::Int(n) => n.to_string(),
        hindsight_format::Value::BigInt(_) => "<bigint>".to_string(),
        hindsight_format::Value::Float(f) => format!("{f}"),
        hindsight_format::Value::String(s) => format!("'{}'", escape_for_repr(s, 40)),
        hindsight_format::Value::Bytes(b) => format!("b'<{} bytes>'", b.len()),
        hindsight_format::Value::List(ids) => format!("[<{} items>]", ids.len()),
        hindsight_format::Value::Dict(pairs) => format!("{{<{} items>}}", pairs.len()),
        hindsight_format::Value::Set(ids) => format!("{{<{} items>}}", ids.len()),
        hindsight_format::Value::CycleRef(_) => "<cycle>".to_string(),
        hindsight_format::Value::Summary { repr, .. } => strings
            .get(*repr as usize)
            .cloned()
            .unwrap_or_else(|| "<summary>".to_string()),
        hindsight_format::Value::TypeRef(id) => strings
            .get(*id as usize)
            .map(|s| format!("<type {s}>"))
            .unwrap_or_else(|| "<type ?>".to_string()),
        hindsight_format::Value::ExceptionUnwindSentinel => "<unwound>".to_string(),
    }
}

fn escape_for_repr(s: &str, max: usize) -> String {
    let truncated: String = s.chars().take(max).collect();
    truncated.replace('\\', "\\\\").replace('\'', "\\'")
}
