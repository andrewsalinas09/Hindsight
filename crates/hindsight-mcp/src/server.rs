// SPDX-License-Identifier: Apache-2.0

//! `HindsightServer` — the rmcp `ServerHandler` that wires the 11 Hindsight
//! tools to the indexed DuckDB database.
//!
//! The tool methods are thin: each delegates to a function in `tools::*`
//! that contains the real logic. This keeps the `#[tool_router]` macro
//! surface narrow and makes the tool implementations testable without
//! spinning up an MCP service.
//!
//! Errors flow back as `Result<Json<T>, McpError>`. We turn `ToolError`
//! into an rmcp `McpError` carrying the structured payload as `data`,
//! which surfaces to the LLM as a structured error response.

use rmcp::{
    ErrorData as McpError, Json, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use serde_json::to_value;

use crate::conn::DbConnection;
use crate::error::ToolError;
use crate::tools::*;

#[derive(Clone)]
pub struct HindsightServer {
    db: DbConnection,
    tool_router: ToolRouter<HindsightServer>,
}

impl HindsightServer {
    pub fn new(db: DbConnection) -> Self {
        Self {
            db,
            tool_router: Self::tool_router(),
        }
    }

    pub fn db(&self) -> &DbConnection {
        &self.db
    }

    pub fn list_tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }
}

#[tool_router(router = tool_router)]
impl HindsightServer {
    #[tool(
        description = "Return the indexed schema with prose descriptions per table/column plus common query patterns. Call this when you're uncertain how to query the trace."
    )]
    fn describe_schema(
        &self,
        _params: Parameters<describe_schema::DescribeSchemaInput>,
    ) -> Result<Json<describe_schema::DescribeSchemaOutput>, McpError> {
        run(describe_schema::run(&self.db))
    }

    #[tool(
        description = "Execute a read-only SQL query against the indexed database. The escape hatch when no typed tool fits."
    )]
    fn run_sql(
        &self,
        Parameters(input): Parameters<run_sql::RunSqlInput>,
    ) -> Result<Json<run_sql::RunSqlOutput>, McpError> {
        run(run_sql::run(&self.db, input))
    }

    #[tool(
        description = "Read source code from the trace's source bundle. Use this whenever you need to understand what code did what."
    )]
    fn get_source(
        &self,
        Parameters(input): Parameters<get_source::GetSourceInput>,
    ) -> Result<Json<get_source::GetSourceOutput>, McpError> {
        run(get_source::run(&self.db, input))
    }

    #[tool(
        description = "Return the full history of a variable in a frame — answers 'what values did X take during this call?'"
    )]
    fn trace_variable(
        &self,
        Parameters(input): Parameters<trace_variable::TraceVariableInput>,
    ) -> Result<Json<trace_variable::TraceVariableOutput>, McpError> {
        run(trace_variable::run(&self.db, input))
    }

    #[tool(
        description = "Find specific function activations by qualified_name and structured criteria — answers 'which call to X are we talking about?'"
    )]
    fn find_call(
        &self,
        Parameters(input): Parameters<find_call::FindCallInput>,
    ) -> Result<Json<find_call::FindCallOutput>, McpError> {
        run(find_call::run(&self.db, input))
    }

    #[tool(
        description = "Given a branch event, return source context and the locals at the branch point — answers 'why did this if-statement go this way?'"
    )]
    fn explain_branch(
        &self,
        Parameters(input): Parameters<explain_branch::ExplainBranchInput>,
    ) -> Result<Json<explain_branch::ExplainBranchOutput>, McpError> {
        run(explain_branch::run(&self.db, input))
    }

    #[tool(
        description = "Explain what caused a variable to change at a specific point — answers 'why did X end up being this value?'"
    )]
    fn why_did_value_change(
        &self,
        Parameters(input): Parameters<why_did_value_change::WhyDidValueChangeInput>,
    ) -> Result<Json<why_did_value_change::WhyDidValueChangeOutput>, McpError> {
        run(why_did_value_change::run(&self.db, input))
    }

    #[tool(
        description = "Per-iteration breakdown of a loop in a frame — answers 'what did this loop actually do?'"
    )]
    fn find_iterations(
        &self,
        Parameters(input): Parameters<find_iterations::FindIterationsInput>,
    ) -> Result<Json<find_iterations::FindIterationsOutput>, McpError> {
        run(find_iterations::run(&self.db, input))
    }

    #[tool(
        description = "Given an exception_raised event, return the propagation chain showing where it was caught — answers 'where did this exception come from?'"
    )]
    fn exception_chain(
        &self,
        Parameters(input): Parameters<exception_chain::ExceptionChainInput>,
    ) -> Result<Json<exception_chain::ExceptionChainOutput>, McpError> {
        run(exception_chain::run(&self.db, input))
    }

    #[tool(
        description = "Return the call tree starting from a frame as nested data — answers 'what's the call structure here?'"
    )]
    fn get_call_tree(
        &self,
        Parameters(input): Parameters<get_call_tree::GetCallTreeInput>,
    ) -> Result<Json<get_call_tree::GetCallTreeOutput>, McpError> {
        run(get_call_tree::run(&self.db, input))
    }

    #[tool(
        description = "Walk backward from a value to find what produced it — answers 'what produced this value, working backward?' Reserve for deep 'why' questions; cheaper than blind run_sql for this shape of question."
    )]
    fn causal_slice(
        &self,
        Parameters(input): Parameters<causal_slice::CausalSliceInput>,
    ) -> Result<Json<causal_slice::CausalSliceOutput>, McpError> {
        run(causal_slice::run(&self.db, input))
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

const SERVER_INSTRUCTIONS: &str = "Hindsight MCP server. The connected DuckDB database is an indexed Hindsight trace.\n\
     Investigation tools: trace_variable, find_call, explain_branch, why_did_value_change, \
     find_iterations, exception_chain, get_call_tree.\n\
     Composite source-aware tool: causal_slice (use sparingly).\n\
     Foundational tools: describe_schema (orient yourself), get_source (read source code), \
     run_sql (escape hatch for queries that don't fit a typed tool).\n\
     Narrate findings to the user in prose; the structured tool outputs are for your consumption.";

/// Convert `Result<T, ToolError>` into the rmcp tool return type.
fn run<T: serde::Serialize>(res: Result<T, ToolError>) -> Result<Json<T>, McpError> {
    match res {
        Ok(v) => Ok(Json(v)),
        Err(e) => {
            let data = to_value(&e).ok();
            Err(McpError::invalid_params(e.to_string(), data))
        }
    }
}
