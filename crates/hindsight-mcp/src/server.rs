// SPDX-License-Identifier: Apache-2.0

//! `HindsightServer` — the rmcp `ServerHandler` that wires the 13
//! Hindsight tools to a registry of indexed traces.
//!
//! The server holds an `Arc<TraceRegistry>` rather than a single
//! connection. Investigation tools take a `trace_id` parameter; the
//! server resolves it (lazily indexing if needed) and hands the
//! resulting `DbConnection` to the tool. The tool functions themselves
//! still take `&DbConnection`, which keeps them pure SQL plumbing and
//! lets unit tests exercise them without spinning up a registry.
//!
//! Errors flow back as `Result<Json<T>, McpError>`. We turn `ToolError`
//! into an rmcp `McpError` carrying the structured payload as `data`,
//! which surfaces to the LLM as a structured error response.

use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, Json, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use serde_json::to_value;

use crate::conn::DbConnection;
use crate::error::ToolError;
use crate::registry::TraceRegistry;
use crate::tools::*;

#[derive(Clone)]
pub struct HindsightServer {
    registry: Arc<TraceRegistry>,
    tool_router: ToolRouter<HindsightServer>,
}

impl HindsightServer {
    pub fn new(registry: Arc<TraceRegistry>) -> Self {
        Self {
            registry,
            tool_router: Self::tool_router(),
        }
    }

    pub fn registry(&self) -> &TraceRegistry {
        &self.registry
    }

    pub fn list_tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    /// Resolve a `trace_id` to an open connection. Lazy-indexes if needed.
    fn resolve(&self, trace_id: &str) -> Result<DbConnection, ToolError> {
        self.registry.get_or_open(trace_id)
    }
}

#[tool_router(router = tool_router)]
impl HindsightServer {
    #[tool(
        description = "List every trace the server can see, with metadata. Call this first when the user references 'the latest trace' or 'the trace from earlier'."
    )]
    fn list_traces(
        &self,
        Parameters(input): Parameters<list_traces::ListTracesInput>,
    ) -> Result<Json<list_traces::ListTracesOutput>, McpError> {
        run(list_traces::run(&self.registry, input))
    }

    #[tool(
        description = "Detailed metadata for one trace by trace_id — recorder version, platform, recorded function list, event counts. Call this when the user wants to know what's in a trace before diving in."
    )]
    fn trace_info(
        &self,
        Parameters(input): Parameters<trace_info::TraceInfoInput>,
    ) -> Result<Json<trace_info::TraceInfoOutput>, McpError> {
        run(trace_info::run(&self.registry, input))
    }

    #[tool(
        description = "Return the indexed schema with prose descriptions per table/column plus common query patterns. Call this when you're uncertain how to query the trace. trace_id is optional — the schema is the same across all traces."
    )]
    fn describe_schema(
        &self,
        Parameters(input): Parameters<describe_schema::DescribeSchemaInput>,
    ) -> Result<Json<describe_schema::DescribeSchemaOutput>, McpError> {
        // describe_schema can run without a DB — fall back to static
        // descriptions if no trace is provided AND the registry has none.
        let db = match input.trace_id.as_deref() {
            Some(id) => Some(self.resolve(id).map_err(to_mcp)?),
            None => match self.registry.sole_trace_id() {
                Some(id) => Some(self.resolve(&id).map_err(to_mcp)?),
                None => None,
            },
        };
        run(describe_schema::run(db.as_ref()))
    }

    #[tool(
        description = "Execute a read-only SQL query against the indexed database for the given trace. The escape hatch when no typed tool fits."
    )]
    fn run_sql(
        &self,
        Parameters(input): Parameters<run_sql::RunSqlInput>,
    ) -> Result<Json<run_sql::RunSqlOutput>, McpError> {
        let db = self.resolve(&input.trace_id).map_err(to_mcp)?;
        run(run_sql::run(&db, input))
    }

    #[tool(
        description = "Read source code from the trace's source bundle. Use this whenever you need to understand what code did what."
    )]
    fn get_source(
        &self,
        Parameters(input): Parameters<get_source::GetSourceInput>,
    ) -> Result<Json<get_source::GetSourceOutput>, McpError> {
        let db = self.resolve(&input.trace_id).map_err(to_mcp)?;
        run(get_source::run(&db, input))
    }

    #[tool(
        description = "Return the full history of a variable in a frame — answers 'what values did X take during this call?'"
    )]
    fn trace_variable(
        &self,
        Parameters(input): Parameters<trace_variable::TraceVariableInput>,
    ) -> Result<Json<trace_variable::TraceVariableOutput>, McpError> {
        let db = self.resolve(&input.trace_id).map_err(to_mcp)?;
        run(trace_variable::run(&db, input))
    }

    #[tool(
        description = "Find specific function activations by qualified_name and structured criteria — answers 'which call to X are we talking about?'"
    )]
    fn find_call(
        &self,
        Parameters(input): Parameters<find_call::FindCallInput>,
    ) -> Result<Json<find_call::FindCallOutput>, McpError> {
        let db = self.resolve(&input.trace_id).map_err(to_mcp)?;
        run(find_call::run(&db, input))
    }

    #[tool(
        description = "Given a branch event, return source context and the locals at the branch point — answers 'why did this if-statement go this way?'"
    )]
    fn explain_branch(
        &self,
        Parameters(input): Parameters<explain_branch::ExplainBranchInput>,
    ) -> Result<Json<explain_branch::ExplainBranchOutput>, McpError> {
        let db = self.resolve(&input.trace_id).map_err(to_mcp)?;
        run(explain_branch::run(&db, input))
    }

    #[tool(
        description = "Explain what caused a variable to change at a specific point — answers 'why did X end up being this value?'"
    )]
    fn why_did_value_change(
        &self,
        Parameters(input): Parameters<why_did_value_change::WhyDidValueChangeInput>,
    ) -> Result<Json<why_did_value_change::WhyDidValueChangeOutput>, McpError> {
        let db = self.resolve(&input.trace_id).map_err(to_mcp)?;
        run(why_did_value_change::run(&db, input))
    }

    #[tool(
        description = "Per-iteration breakdown of a loop in a frame — answers 'what did this loop actually do?'"
    )]
    fn find_iterations(
        &self,
        Parameters(input): Parameters<find_iterations::FindIterationsInput>,
    ) -> Result<Json<find_iterations::FindIterationsOutput>, McpError> {
        let db = self.resolve(&input.trace_id).map_err(to_mcp)?;
        run(find_iterations::run(&db, input))
    }

    #[tool(
        description = "Given an exception_raised event, return the propagation chain showing where it was caught — answers 'where did this exception come from?'"
    )]
    fn exception_chain(
        &self,
        Parameters(input): Parameters<exception_chain::ExceptionChainInput>,
    ) -> Result<Json<exception_chain::ExceptionChainOutput>, McpError> {
        let db = self.resolve(&input.trace_id).map_err(to_mcp)?;
        run(exception_chain::run(&db, input))
    }

    #[tool(
        description = "Return the call tree starting from a frame as nested data — answers 'what's the call structure here?'"
    )]
    fn get_call_tree(
        &self,
        Parameters(input): Parameters<get_call_tree::GetCallTreeInput>,
    ) -> Result<Json<get_call_tree::GetCallTreeOutput>, McpError> {
        let db = self.resolve(&input.trace_id).map_err(to_mcp)?;
        run(get_call_tree::run(&db, input))
    }

    #[tool(
        description = "Walk backward from a value to find what produced it — answers 'what produced this value, working backward?' Reserve for deep 'why' questions; cheaper than blind run_sql for this shape of question."
    )]
    fn causal_slice(
        &self,
        Parameters(input): Parameters<causal_slice::CausalSliceInput>,
    ) -> Result<Json<causal_slice::CausalSliceOutput>, McpError> {
        let db = self.resolve(&input.trace_id).map_err(to_mcp)?;
        run(causal_slice::run(&db, input))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HindsightServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(SERVER_INSTRUCTIONS.to_string())
    }
}

const SERVER_INSTRUCTIONS: &str = "Hindsight MCP server (multi-trace mode). The server holds a registry of indexed traces \
     (a directory of .hindsight/.duckdb files, or one explicit file in legacy single-file mode).\n\
     Discovery tools: list_traces (enumerate available traces), trace_info (deep metadata on one trace).\n\
     Investigation tools (each takes a trace_id): trace_variable, find_call, explain_branch, \
     why_did_value_change, find_iterations, exception_chain, get_call_tree.\n\
     Composite source-aware tool: causal_slice (use sparingly).\n\
     Foundational tools: describe_schema (orient yourself; trace_id optional), get_source \
     (read source code), run_sql (escape hatch for queries that don't fit a typed tool).\n\
     Workflow: start with list_traces to learn what's available, pick a trace_id, then thread \
     it through subsequent tool calls. Indexing happens lazily on first investigation against an \
     unindexed trace. Narrate findings to the user in prose; the structured tool outputs are for \
     your consumption.";

/// Convert `Result<T, ToolError>` into the rmcp tool return type.
fn run<T: serde::Serialize>(res: Result<T, ToolError>) -> Result<Json<T>, McpError> {
    match res {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(to_mcp(e)),
    }
}

fn to_mcp(e: ToolError) -> McpError {
    let data = to_value(&e).ok();
    McpError::invalid_params(e.to_string(), data)
}
