// SPDX-License-Identifier: Apache-2.0

//! Error types for the MCP server.
//!
//! Tool calls return structured errors as JSON rather than RPC failures so
//! the LLM can read the error and react. We do this by returning a
//! `ToolError` from each tool implementation; the rmcp wrapper converts
//! it into a `CallToolResult` with `is_error = true`.

use serde::{Deserialize, Serialize};

/// Structured error returned to the LLM as the body of a failed tool call.
///
/// The `error` field is a stable machine-readable code; `message` is the
/// human-readable explanation; `suggested_action` is a hint the LLM can
/// follow without further help.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolError {
    pub error: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
}

impl ToolError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: code.into(),
            message: message.into(),
            suggested_action: None,
        }
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggested_action = Some(suggestion.into());
        self
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error, self.message)
    }
}

/// Server-level errors that prevent the server from starting at all.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("database open error at {path:?}: {source}")]
    DbOpen {
        path: std::path::PathBuf,
        #[source]
        source: duckdb::Error,
    },
    #[error("rmcp service error: {0}")]
    Service(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
