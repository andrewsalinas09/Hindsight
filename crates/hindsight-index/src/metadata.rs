// SPDX-License-Identifier: Apache-2.0

//! Parsing of the trace's TOML metadata blocks (initial metadata + final
//! summary) into typed structs the indexer can populate `trace_metadata` from.
//!
//! The wire-format spec for these blocks is in `docs/trace-format.md` §"Initial
//! metadata block" and §"Final summary".

use serde::Deserialize;

use crate::error::{IndexError, Result};

#[derive(Debug, Default, Deserialize)]
pub struct InitialMetadata {
    pub recorder: RecorderSection,
    pub recording: RecordingSection,
}

#[derive(Debug, Default, Deserialize)]
pub struct RecorderSection {
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub language_version: String,
    #[serde(default)]
    pub recorder_version: String,
    #[serde(default)]
    pub platform: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct RecordingSection {
    #[serde(default)]
    pub program: String,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub scope_config: ScopeConfigSection,
}

#[derive(Debug, Default, Deserialize)]
pub struct ScopeConfigSection {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub depth_limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct FinalSummary {
    #[serde(default)]
    pub r#final: FinalBody,
}

#[derive(Debug, Default, Deserialize)]
pub struct FinalBody {
    #[serde(default)]
    #[allow(dead_code)]
    pub clean_shutdown: bool,
    #[serde(default)]
    pub total_events: i64,
    #[serde(default)]
    pub total_blocks: i64,
    #[serde(default)]
    pub trace_duration_ns: i64,
    #[serde(default)]
    pub scope_resolved: ScopeResolved,
    #[serde(default)]
    pub statistics: Statistics,
}

#[derive(Debug, Default, Deserialize)]
pub struct ScopeResolved {
    #[serde(default)]
    pub recorded_functions: Vec<String>,
    #[serde(default)]
    pub excluded_functions: Vec<ExcludedFunction>,
    #[serde(default)]
    pub skip_blocks_observed: i64,
    #[serde(default)]
    pub depth_clips_observed: i64,
}

#[derive(Debug, Deserialize)]
pub struct ExcludedFunction {
    pub name: String,
    pub matched_pattern: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct Statistics {
    #[serde(default)]
    pub function_entry_events: i64,
    #[serde(default)]
    pub line_events: i64,
    #[serde(default)]
    pub branch_events: i64,
    #[serde(default)]
    pub exception_events: i64,
    #[serde(default)]
    pub note_events: i64,
}

pub fn parse_initial(payload: &str) -> Result<InitialMetadata> {
    toml::from_str(payload).map_err(|e| IndexError::Metadata(format!("initial metadata: {e}")))
}

pub fn parse_final(payload: &str) -> Result<FinalSummary> {
    toml::from_str(payload).map_err(|e| IndexError::Metadata(format!("final summary: {e}")))
}
