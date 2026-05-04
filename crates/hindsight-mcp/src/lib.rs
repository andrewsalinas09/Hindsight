// SPDX-License-Identifier: Apache-2.0

//! Model Context Protocol server exposing Hindsight debugging primitives.
//!
//! The server connects to an indexed Hindsight trace (a DuckDB file
//! produced by `hindsight-index`) and exposes 11 tools to MCP clients
//! (Claude Desktop, Claude Code, etc.). The tool surface is documented in
//! `docs/mcp-server-design.md`.
//!
//! Public entry points:
//!
//! - [`HindsightServer`] — the rmcp `ServerHandler` with all tools wired in.
//! - [`run_stdio`] — open the database at `db_path`, build the server, and
//!   serve over stdio until the client disconnects.

use std::path::PathBuf;

use rmcp::{ServiceExt, transport::stdio};

pub mod conn;
pub mod error;
pub mod identifiers;
pub mod schema_descriptions;
pub mod server;
pub mod source;
pub mod tools;
pub mod value;

pub use conn::DbConnection;
pub use error::{ServerError, ToolError};
pub use server::HindsightServer;

/// Open the indexed database at `db_path` and serve the MCP protocol over
/// stdio. Returns once the client disconnects.
pub async fn run_stdio(db_path: PathBuf) -> Result<(), ServerError> {
    let db = DbConnection::open(db_path)?;
    let server = HindsightServer::new(db);
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
    fn server_constructs_without_db_calls() {
        // Smoke test — DbConnection::open requires a real file, so we
        // can't construct HindsightServer here without a fixture. This
        // test just verifies the module compiles with all wiring in place.
        let _ = std::any::type_name::<HindsightServer>();
    }
}
