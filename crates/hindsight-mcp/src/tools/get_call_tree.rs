// SPDX-License-Identifier: Apache-2.0

//! `get_call_tree` — return the call tree starting from a frame as nested
//! structured data.

use std::collections::HashMap;

use duckdb::params;
use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetCallTreeInput {
    pub trace_id: String,
    pub frame_id: i64,
    /// Maximum tree depth from the root frame. Default unlimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<i32>,
    /// Whether to include argument summaries. Defaults to true.
    #[serde(default = "default_true")]
    pub include_args: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallTreeNode {
    pub frame_id: i64,
    pub qualified_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ns: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_kind: Option<String>,
    pub depth: i32,
    pub call_index: i32,
    pub children: Vec<CallTreeNode>,
}

pub type GetCallTreeOutput = CallTreeNode;

struct FlatRow {
    frame_id: i64,
    parent_frame_id: Option<i64>,
    qualified_name: String,
    argument_summary: Option<String>,
    duration_ns: Option<i64>,
    exit_kind: Option<String>,
    depth: i32,
    call_index: i32,
}

pub fn run(db: &DbConnection, input: GetCallTreeInput) -> Result<GetCallTreeOutput, ToolError> {
    let max_depth = input.max_depth.unwrap_or(i32::MAX);
    let conn = db.lock();
    // Recursive CTE: walk children of the root frame.
    let mut stmt = conn
        .prepare(
            "WITH RECURSIVE tree AS ( \
               SELECT frame_id, parent_frame_id, qualified_name, argument_summary, duration_ns, \
                      exit_kind, depth, call_index, 0 AS tree_depth \
               FROM frames WHERE frame_id = ? \
               UNION ALL \
               SELECT f.frame_id, f.parent_frame_id, f.qualified_name, f.argument_summary, \
                      f.duration_ns, f.exit_kind, f.depth, f.call_index, t.tree_depth + 1 \
               FROM frames f JOIN tree t ON f.parent_frame_id = t.frame_id \
               WHERE t.tree_depth < ? \
             ) SELECT frame_id, parent_frame_id, qualified_name, argument_summary, duration_ns, \
                      exit_kind, depth, call_index, tree_depth FROM tree ORDER BY frame_id",
        )
        .map_err(map_db)?;
    let rows = stmt
        .query_map(params![input.frame_id, max_depth], |r| {
            Ok(FlatRow {
                frame_id: r.get(0)?,
                parent_frame_id: r.get(1)?,
                qualified_name: r.get(2)?,
                argument_summary: r.get(3)?,
                duration_ns: r.get(4)?,
                exit_kind: r.get(5)?,
                depth: r.get(6)?,
                call_index: r.get(7)?,
            })
        })
        .map_err(map_db)?;
    let flat: Vec<FlatRow> = rows.collect::<duckdb::Result<Vec<_>>>().map_err(map_db)?;
    drop(stmt);
    drop(conn);

    if flat.is_empty() {
        return Err(ToolError::new(
            "frame_not_found",
            format!("No frame with frame_id={}", input.frame_id),
        )
        .with_suggestion("Use find_call to locate frames by qualified_name."));
    }

    // Build a map: parent_frame_id -> children rows in entry order.
    let mut children_of: HashMap<i64, Vec<&FlatRow>> = HashMap::new();
    for row in &flat {
        if let Some(p) = row.parent_frame_id {
            children_of.entry(p).or_default().push(row);
        }
    }
    for v in children_of.values_mut() {
        v.sort_by_key(|r| r.frame_id);
    }

    let root = flat.iter().find(|r| r.frame_id == input.frame_id).unwrap();
    Ok(build_node(root, &children_of, input.include_args))
}

fn build_node(
    row: &FlatRow,
    children_of: &HashMap<i64, Vec<&FlatRow>>,
    include_args: bool,
) -> CallTreeNode {
    let kids = children_of.get(&row.frame_id).cloned().unwrap_or_default();
    let children = kids
        .into_iter()
        .map(|c| build_node(c, children_of, include_args))
        .collect();
    CallTreeNode {
        frame_id: row.frame_id,
        qualified_name: row.qualified_name.clone(),
        argument_summary: if include_args {
            row.argument_summary.clone()
        } else {
            None
        },
        duration_ns: row.duration_ns,
        exit_kind: row.exit_kind.clone(),
        depth: row.depth,
        call_index: row.call_index,
        children,
    }
}

fn map_db(e: duckdb::Error) -> ToolError {
    ToolError::new("database_error", e.to_string())
}
