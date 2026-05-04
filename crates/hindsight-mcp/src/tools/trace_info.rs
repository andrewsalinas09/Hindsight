// SPDX-License-Identifier: Apache-2.0

//! `trace_info` — detailed metadata for one trace.
//!
//! Used when the user asks "what's in this trace" or before diving into
//! deep investigation; pairs well with `list_traces` (which returns just
//! enough to pick a trace).

use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::registry::TraceRegistry;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceInfoInput {
    pub trace_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceInfoOutput {
    pub trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_at_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorder_language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorder_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ns: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_entry_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_event_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_event_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exception_event_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_event_count: Option<i64>,
    pub recorded_functions: Vec<String>,
    pub excluded_functions: Vec<String>,
    pub indexed: bool,
    pub size_bytes: u64,
}

pub fn run(registry: &TraceRegistry, input: TraceInfoInput) -> Result<TraceInfoOutput, ToolError> {
    let m = registry.metadata(&input.trace_id)?;
    Ok(TraceInfoOutput {
        trace_id: m.trace_id,
        recorded_at_ns: m.recorded_at_ns,
        trace_uuid: m.trace_uuid,
        program: m.program,
        recorder_language: m.recorder_language,
        recorder_version: m.recorder_version,
        language_version: m.language_version,
        platform: m.platform,
        working_directory: m.working_directory,
        event_count: m.event_count,
        duration_ns: m.duration_ns,
        function_entry_count: m.function_entry_count,
        line_event_count: m.line_event_count,
        branch_event_count: m.branch_event_count,
        exception_event_count: m.exception_event_count,
        note_event_count: m.note_event_count,
        recorded_functions: m.recorded_functions,
        excluded_functions: m.excluded_functions,
        indexed: m.indexed,
        size_bytes: m.size_bytes,
    })
}
