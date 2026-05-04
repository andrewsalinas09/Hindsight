// SPDX-License-Identifier: Apache-2.0

//! `get_source` — return source code from the trace's source bundle.

use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;
use crate::source::{fetch_source_content, list_source_paths, slice_lines};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetSourceInput {
    /// Trace to read source from.
    pub trace_id: String,
    /// File path as stored in the trace (matches `events.source_file`).
    pub file_path: String,
    /// Optional 1-indexed inclusive `[start, end]` line range. Omit to
    /// return the whole file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_range: Option<[i32; 2]>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetSourceOutput {
    pub file_path: String,
    pub content: String,
    pub start_line: i32,
    pub end_line: i32,
    pub total_lines: i32,
}

pub fn run(db: &DbConnection, input: GetSourceInput) -> Result<GetSourceOutput, ToolError> {
    let content = match fetch_source_content(db, &input.file_path)? {
        Some(c) => c,
        None => {
            let suggestions = list_source_paths(db).unwrap_or_default();
            let mut msg = format!(
                "No source file at path {:?} in this trace.",
                input.file_path
            );
            if !suggestions.is_empty() {
                msg.push_str("\nAvailable file paths: ");
                msg.push_str(&suggestions.join(", "));
            }
            return Err(ToolError::new("source_not_found", msg).with_suggestion(
                "Use one of the listed file paths, or query `SELECT path FROM source_files` for \
                 the full list.",
            ));
        }
    };
    let range = input.line_range.map(|r| (r[0], r[1]));
    let sliced = slice_lines(&content, range);
    Ok(GetSourceOutput {
        file_path: input.file_path,
        content: sliced.content,
        start_line: sliced.start_line,
        end_line: sliced.end_line,
        total_lines: sliced.total_lines,
    })
}
