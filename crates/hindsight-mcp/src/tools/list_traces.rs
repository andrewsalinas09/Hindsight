// SPDX-License-Identifier: Apache-2.0

//! `list_traces` — return every trace the registry can see, with enough
//! metadata for the LLM to pick one.
//!
//! This is what Claude calls when the user says "the latest trace" or
//! "the trace from earlier" — Claude lists, sorts by recorded_at, picks
//! the one matching the user's reference, and threads its `trace_id`
//! through subsequent investigation tools.

use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::registry::{RegistrySource, TraceRegistry};

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListTracesInput {}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScopeSummary {
    pub recorded_function_count: usize,
    pub excluded_function_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceListing {
    pub trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_at_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ns: Option<i64>,
    pub indexed: bool,
    pub size_bytes: u64,
    pub scope_summary: ScopeSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListTracesOutput {
    /// Filesystem path the registry is watching, or `null` for explicit
    /// file lists (legacy single-file mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    pub traces: Vec<TraceListing>,
}

pub fn run(
    registry: &TraceRegistry,
    _input: ListTracesInput,
) -> Result<ListTracesOutput, ToolError> {
    let metadatas = registry.list()?;
    let mut traces: Vec<TraceListing> = metadatas
        .into_iter()
        .map(|m| TraceListing {
            scope_summary: ScopeSummary {
                recorded_function_count: m.recorded_functions.len(),
                excluded_function_count: m.excluded_functions.len(),
            },
            trace_id: m.trace_id,
            recorded_at_ns: m.recorded_at_ns,
            program: m.program,
            event_count: m.event_count,
            duration_ns: m.duration_ns,
            indexed: m.indexed,
            size_bytes: m.size_bytes,
        })
        .collect();
    // Newest first by recording start ns; ties broken by trace_id.
    traces.sort_by(|a, b| {
        b.recorded_at_ns
            .cmp(&a.recorded_at_ns)
            .then_with(|| a.trace_id.cmp(&b.trace_id))
    });
    let directory = match registry.source() {
        RegistrySource::Directory(d) => Some(d.display().to_string()),
        RegistrySource::Files(_) => None,
    };
    Ok(ListTracesOutput { directory, traces })
}
