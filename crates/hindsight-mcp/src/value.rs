// SPDX-License-Identifier: Apache-2.0

//! Helpers for fetching `values` rows from the indexed database and
//! turning them into the `ValueSummary` shape used by tool outputs.
//!
//! The `values` table has type-specific columns; tool outputs converge on
//! a single shape `{type_tag, display, value_id, ...}` so the LLM can
//! consume any value uniformly.

use duckdb::{Row, params};
use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;

/// The shape every value takes in tool output.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ValueSummary {
    pub value_id: i64,
    pub type_tag: String,
    pub display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_length: Option<i64>,
}

#[derive(Debug, Clone)]
struct RawValueRow {
    type_tag: String,
    bool_value: Option<bool>,
    int_value: Option<i64>,
    big_int_hex: Option<String>,
    float_value: Option<f64>,
    string_value: Option<String>,
    container_length: Option<i64>,
    type_name: Option<String>,
    repr_text: Option<String>,
    summary_length: Option<i64>,
    type_ref_name: Option<String>,
    cycle_ref_depth: Option<i32>,
}

fn read_row(row: &Row<'_>) -> duckdb::Result<RawValueRow> {
    Ok(RawValueRow {
        type_tag: row.get(0)?,
        bool_value: row.get(1)?,
        int_value: row.get(2)?,
        big_int_hex: row.get(3)?,
        float_value: row.get(4)?,
        string_value: row.get(5)?,
        container_length: row.get(6)?,
        type_name: row.get(7)?,
        repr_text: row.get(8)?,
        summary_length: row.get(9)?,
        type_ref_name: row.get(10)?,
        cycle_ref_depth: row.get(11)?,
    })
}

const VALUE_COLUMNS: &str = "type_tag, bool_value, int_value, big_int_hex, float_value, \
     string_value, container_length, type_name, repr_text, summary_length, \
     type_ref_name, cycle_ref_depth";

fn make_display(raw: &RawValueRow) -> String {
    match raw.type_tag.as_str() {
        "none" => "None".to_string(),
        "bool" => match raw.bool_value {
            Some(true) => "True".to_string(),
            Some(false) => "False".to_string(),
            None => "?".to_string(),
        },
        "int" => raw
            .int_value
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string()),
        "big_int" => raw
            .big_int_hex
            .clone()
            .map(|h| format!("0x{h}"))
            .unwrap_or_else(|| "?".to_string()),
        "float" => raw
            .float_value
            .map(|v| format!("{v}"))
            .unwrap_or_else(|| "?".to_string()),
        "string" => match &raw.string_value {
            Some(s) => format_string(s),
            None => "?".to_string(),
        },
        "bytes" => "<bytes>".to_string(),
        "list" | "set" => format!(
            "<{} items>",
            raw.container_length
                .map(|x| x.to_string())
                .unwrap_or_default()
        ),
        "dict" => format!(
            "<{} entries>",
            raw.container_length
                .map(|x| x.to_string())
                .unwrap_or_default()
        ),
        "summary" => raw
            .repr_text
            .clone()
            .or_else(|| {
                raw.type_name
                    .clone()
                    .map(|t| format!("<summary {t} len={:?}>", raw.summary_length))
            })
            .unwrap_or_else(|| "<summary>".to_string()),
        "type_ref" => raw
            .type_ref_name
            .clone()
            .map(|n| format!("<class {n}>"))
            .unwrap_or_else(|| "<type>".to_string()),
        "cycle_ref" => format!("<cycle ref depth={:?}>", raw.cycle_ref_depth),
        "exception_unwind_sentinel" => "<exception_unwind>".to_string(),
        other => format!("<{other}>"),
    }
}

fn format_string(s: &str) -> String {
    const MAX: usize = 80;
    if s.len() <= MAX {
        format!("{s:?}")
    } else {
        let truncated: String = s.chars().take(MAX).collect();
        format!("{truncated:?}…")
    }
}

fn to_summary(value_id: i64, raw: &RawValueRow) -> ValueSummary {
    ValueSummary {
        value_id,
        type_tag: raw.type_tag.clone(),
        display: make_display(raw),
        type_name: raw.type_name.clone(),
        container_length: raw.container_length,
    }
}

/// Fetch a single value's summary by `value_id`. Returns Ok(None) if no row
/// exists.
pub fn fetch_value_summary(
    db: &DbConnection,
    value_id: i64,
) -> Result<Option<ValueSummary>, ToolError> {
    let conn = db.lock();
    let sql = format!("SELECT {VALUE_COLUMNS} FROM values WHERE value_id = ?");
    let mut stmt = conn.prepare(&sql).map_err(map_db)?;
    let rows: Vec<RawValueRow> = stmt
        .query_map(params![value_id], read_row)
        .map_err(map_db)?
        .collect::<duckdb::Result<Vec<_>>>()
        .map_err(map_db)?;
    Ok(rows.first().map(|r| to_summary(value_id, r)))
}

/// Fetch many value summaries at once. The returned map only contains
/// entries that exist in the database.
pub fn fetch_value_summaries(
    db: &DbConnection,
    value_ids: &[i64],
) -> Result<std::collections::HashMap<i64, ValueSummary>, ToolError> {
    if value_ids.is_empty() {
        return Ok(Default::default());
    }
    let conn = db.lock();
    let placeholders: Vec<&str> = value_ids.iter().map(|_| "?").collect();
    let sql = format!(
        "SELECT value_id, {} FROM values WHERE value_id IN ({})",
        VALUE_COLUMNS,
        placeholders.join(", ")
    );
    let mut stmt = conn.prepare(&sql).map_err(map_db)?;
    let bind: Vec<&dyn duckdb::ToSql> = value_ids.iter().map(|v| v as &dyn duckdb::ToSql).collect();
    let mut out = std::collections::HashMap::new();
    let rows = stmt
        .query_map(bind.as_slice(), |row| {
            let value_id: i64 = row.get(0)?;
            // Shift the columns by one because the first SELECT column is value_id.
            Ok((
                value_id,
                RawValueRow {
                    type_tag: row.get(1)?,
                    bool_value: row.get(2)?,
                    int_value: row.get(3)?,
                    big_int_hex: row.get(4)?,
                    float_value: row.get(5)?,
                    string_value: row.get(6)?,
                    container_length: row.get(7)?,
                    type_name: row.get(8)?,
                    repr_text: row.get(9)?,
                    summary_length: row.get(10)?,
                    type_ref_name: row.get(11)?,
                    cycle_ref_depth: row.get(12)?,
                },
            ))
        })
        .map_err(map_db)?;
    for row in rows {
        let (id, raw) = row.map_err(map_db)?;
        out.insert(id, to_summary(id, &raw));
    }
    Ok(out)
}

fn map_db(e: duckdb::Error) -> ToolError {
    ToolError::new("database_error", e.to_string())
}
