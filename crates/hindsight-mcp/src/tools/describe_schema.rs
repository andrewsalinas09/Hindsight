// SPDX-License-Identifier: Apache-2.0

//! `describe_schema` — returns the indexed database's schema with prose
//! descriptions of each table and column, plus a small library of common
//! query patterns.

use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;
use crate::schema_descriptions::{QUERY_PATTERNS, TABLES};

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DescribeSchemaInput {
    /// Optional. If provided, the column types come from that trace's
    /// indexed database; otherwise the server picks any available trace.
    /// The schema is the same across all Hindsight traces, so this is
    /// primarily useful as a smoke test that the trace is accessible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ColumnInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub col_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TableInfo {
    pub name: String,
    pub description: String,
    pub columns: Vec<ColumnInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct QueryPatternInfo {
    pub name: String,
    pub description: String,
    pub sql: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DescribeSchemaOutput {
    pub tables: Vec<TableInfo>,
    pub common_query_patterns: Vec<QueryPatternInfo>,
}

/// `db` is optional — the prose schema descriptions are static, so a
/// trace need not be available. When `db` is `Some`, column types are
/// pulled from the live information_schema; when `None`, the columns
/// list is built from the static descriptions only.
pub fn run(db: Option<&DbConnection>) -> Result<DescribeSchemaOutput, ToolError> {
    let mut by_table: std::collections::BTreeMap<String, Vec<(String, String)>> =
        Default::default();
    if let Some(db) = db {
        let conn = db.lock();
        let mut stmt = conn
            .prepare(
                "SELECT table_name, column_name, data_type FROM information_schema.columns \
                 WHERE table_schema = 'main' ORDER BY table_name, ordinal_position",
            )
            .map_err(|e| ToolError::new("database_error", e.to_string()))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| ToolError::new("database_error", e.to_string()))?;
        while let Some(row) = rows
            .next()
            .map_err(|e| ToolError::new("database_error", e.to_string()))?
        {
            let table: String = row
                .get(0)
                .map_err(|e| ToolError::new("database_error", e.to_string()))?;
            let col: String = row
                .get(1)
                .map_err(|e| ToolError::new("database_error", e.to_string()))?;
            let ty: String = row
                .get(2)
                .map_err(|e| ToolError::new("database_error", e.to_string()))?;
            by_table.entry(table).or_default().push((col, ty));
        }
    }

    let mut tables: Vec<TableInfo> = Vec::new();
    // Iterate in our documented order so the LLM sees important tables first.
    for desc in TABLES {
        let cols_in_db = by_table.remove(desc.name);
        let columns = match cols_in_db {
            // Live DB: type comes from information_schema, description
            // from our static map.
            Some(cols) => cols
                .into_iter()
                .map(|(name, ty)| ColumnInfo {
                    description: desc
                        .columns
                        .iter()
                        .find(|(n, _)| *n == name)
                        .map(|(_, d)| d.to_string()),
                    name,
                    col_type: ty,
                })
                .collect(),
            // No DB available: fall back to the static descriptions.
            // Type is left empty since we don't have it without querying.
            None => desc
                .columns
                .iter()
                .map(|(name, description)| ColumnInfo {
                    name: (*name).to_string(),
                    col_type: String::new(),
                    description: if description.is_empty() {
                        None
                    } else {
                        Some((*description).to_string())
                    },
                })
                .collect(),
        };
        tables.push(TableInfo {
            name: desc.name.to_string(),
            description: desc.description.to_string(),
            columns,
        });
    }
    // Append any tables that exist in the DB but aren't in our static list
    // (forward-compat for schema additions).
    for (name, cols) in by_table {
        tables.push(TableInfo {
            name,
            description: String::new(),
            columns: cols
                .into_iter()
                .map(|(n, t)| ColumnInfo {
                    name: n,
                    col_type: t,
                    description: None,
                })
                .collect(),
        });
    }

    let common_query_patterns = QUERY_PATTERNS
        .iter()
        .map(|p| QueryPatternInfo {
            name: p.name.to_string(),
            description: p.description.to_string(),
            sql: p.sql.to_string(),
        })
        .collect();

    Ok(DescribeSchemaOutput {
        tables,
        common_query_patterns,
    })
}
