// SPDX-License-Identifier: Apache-2.0

//! `find_call` — find specific function activations by structured criteria.

use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindCallWhere {
    /// Exactly the Nth call (0-indexed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_index: Option<i32>,
    /// Substring match against the frame's argument_summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_contains: Option<String>,
    /// True for frames with exit_kind='raised'.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raised_exception: Option<bool>,
    /// Only frames with duration_ns > this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_duration_ns: Option<i64>,
    /// Only frames called from this qualified parent function.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_qualified_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindCallInput {
    pub qualified_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#where: Option<FindCallWhere>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallMatch {
    pub frame_id: i64,
    pub call_index: i32,
    pub depth: i32,
    pub argument_summary: Option<String>,
    pub duration_ns: Option<i64>,
    pub exit_kind: Option<String>,
    pub entry_event_id: i64,
    pub exit_event_id: Option<i64>,
    pub parent_frame_id: Option<i64>,
    pub source_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindCallOutput {
    pub qualified_name: String,
    pub matches: Vec<CallMatch>,
    pub total_matches: u64,
    /// True if more matches existed than `limit` returned.
    pub truncated: bool,
}

const DEFAULT_LIMIT: i32 = 10;

pub fn run(db: &DbConnection, input: FindCallInput) -> Result<FindCallOutput, ToolError> {
    let limit = input.limit.unwrap_or(DEFAULT_LIMIT).max(1);
    let where_ = input.r#where.unwrap_or_default();

    // Build dynamic SQL.
    let mut sql = String::from(
        "SELECT f.frame_id, f.call_index, f.depth, f.argument_summary, f.duration_ns, \
         f.exit_kind, f.entry_event_id, f.exit_event_id, f.parent_frame_id, f.source_file \
         FROM frames f",
    );
    if where_.parent_qualified_name.is_some() {
        sql.push_str(" LEFT JOIN frames p ON f.parent_frame_id = p.frame_id");
    }
    sql.push_str(" WHERE f.qualified_name = ?");

    let mut bind: Vec<Box<dyn duckdb::ToSql>> = Vec::new();
    bind.push(Box::new(input.qualified_name.clone()));

    if let Some(idx) = where_.call_index {
        sql.push_str(" AND f.call_index = ?");
        bind.push(Box::new(idx));
    }
    if let Some(needle) = &where_.argument_contains {
        sql.push_str(" AND f.argument_summary LIKE ?");
        bind.push(Box::new(format!("%{needle}%")));
    }
    if let Some(true) = where_.raised_exception {
        sql.push_str(" AND f.exit_kind = 'raised'");
    } else if let Some(false) = where_.raised_exception {
        sql.push_str(" AND (f.exit_kind != 'raised' OR f.exit_kind IS NULL)");
    }
    if let Some(min) = where_.min_duration_ns {
        sql.push_str(" AND f.duration_ns > ?");
        bind.push(Box::new(min));
    }
    if let Some(parent) = &where_.parent_qualified_name {
        sql.push_str(" AND p.qualified_name = ?");
        bind.push(Box::new(parent.clone()));
    }

    sql.push_str(" ORDER BY f.entry_event_id LIMIT ?");
    bind.push(Box::new(limit + 1));

    let conn = db.lock();
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| ToolError::new("database_error", e.to_string()))?;
    let bind_refs: Vec<&dyn duckdb::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(bind_refs.as_slice(), |row| {
            Ok(CallMatch {
                frame_id: row.get(0)?,
                call_index: row.get(1)?,
                depth: row.get(2)?,
                argument_summary: row.get(3)?,
                duration_ns: row.get(4)?,
                exit_kind: row.get(5)?,
                entry_event_id: row.get(6)?,
                exit_event_id: row.get(7)?,
                parent_frame_id: row.get(8)?,
                source_file: row.get(9)?,
            })
        })
        .map_err(|e| ToolError::new("database_error", e.to_string()))?;
    let mut matches: Vec<CallMatch> = Vec::new();
    for r in rows {
        matches.push(r.map_err(|e| ToolError::new("database_error", e.to_string()))?);
    }
    let truncated = matches.len() as i32 > limit;
    if truncated {
        matches.truncate(limit as usize);
    }
    let total_matches = matches.len() as u64;
    Ok(FindCallOutput {
        qualified_name: input.qualified_name,
        matches,
        total_matches,
        truncated,
    })
}
