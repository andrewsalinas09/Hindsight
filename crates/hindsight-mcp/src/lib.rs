// SPDX-License-Identifier: Apache-2.0

//! Model Context Protocol server exposing Hindsight debugging primitives.
//!
//! The server connects to a *registry* of indexed Hindsight traces and
//! exposes 13 tools to MCP clients (Claude Desktop, Claude Code, etc.).
//! The tool surface is documented in `docs/mcp-server-design.md` and the
//! multi-trace architecture in `docs/multi-trace-design.md`.
//!
//! Public entry points:
//!
//! - [`HindsightServer`] — the rmcp `ServerHandler` with all tools wired in.
//! - [`run_stdio_dir`] — point at a directory of `.hindsight` traces and
//!   serve over stdio until the client disconnects.
//! - [`run_stdio_file`] — legacy single-file mode for a `.hindsight` or
//!   `.duckdb` path.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::{ServiceExt, transport::stdio};

pub mod conn;
pub mod error;
pub mod identifiers;
pub mod registry;
pub mod schema_descriptions;
pub mod server;
pub mod source;
pub mod tools;
pub mod value;

pub use conn::DbConnection;
pub use error::{ServerError, ToolError};
pub use registry::{TraceMetadata, TraceRegistry};
pub use server::HindsightServer;

/// Serve a directory of `.hindsight` traces over stdio.
pub async fn run_stdio_dir(dir: PathBuf) -> Result<(), ServerError> {
    let registry = Arc::new(TraceRegistry::from_directory(dir)?);
    serve(registry).await
}

/// Serve a single trace file (`.hindsight` or `.duckdb`) over stdio.
/// Equivalent to a one-entry registry; the LLM can still address the
/// trace by its `trace_id` (the filename stem).
pub async fn run_stdio_file(path: PathBuf) -> Result<(), ServerError> {
    let registry = Arc::new(TraceRegistry::from_files(vec![path])?);
    serve(registry).await
}

async fn serve(registry: Arc<TraceRegistry>) -> Result<(), ServerError> {
    let server = HindsightServer::new(registry);
    let service = server
        .serve(stdio())
        .await
        .map_err(|e| ServerError::Service(e.to_string()))?;
    service
        .waiting()
        .await
        .map_err(|e| ServerError::Service(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_module_links() {
        let _ = std::any::type_name::<HindsightServer>();
    }
}
