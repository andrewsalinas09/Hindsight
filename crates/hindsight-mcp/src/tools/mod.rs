// SPDX-License-Identifier: Apache-2.0

//! Tool implementations. Each submodule defines a tool's request/response
//! types and its `run` function. The server in `server.rs` wires these
//! into rmcp's `tool_router` and provides the connection.
//!
//! Splitting the logic out of the macro-decorated impl block keeps the
//! macro surface small and lets unit tests exercise tool functions
//! directly without spinning up an MCP service.

pub mod causal_slice;
pub mod describe_schema;
pub mod exception_chain;
pub mod explain_branch;
pub mod find_call;
pub mod find_iterations;
pub mod get_call_tree;
pub mod get_source;
pub mod list_traces;
pub mod run_sql;
pub mod trace_info;
pub mod trace_variable;
pub mod why_did_value_change;
