// SPDX-License-Identifier: Apache-2.0

//! `run_sql` — execute a read-only SQL query against the indexed database.
//! The escape hatch for any question that doesn't fit a typed tool.

use duckdb::types::Value as DuckValue;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::conn::DbConnection;
use crate::error::ToolError;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunSqlInput {
    /// The SQL query to execute. Must be SELECT-only — INSERT, UPDATE,
    /// DELETE, DROP, CREATE, ALTER, ATTACH, and PRAGMA are rejected.
    pub query: String,
    /// Maximum number of rows to return. Defaults to 1000.
    #[serde(default)]
    pub max_rows: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunSqlOutput {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<JsonValue>>,
    pub row_count: u64,
    pub truncated: bool,
}

const DEFAULT_MAX_ROWS: u32 = 1000;
/// Keywords that mutate the database. Matched as whole words, case-insensitive.
const FORBIDDEN: &[&str] = &[
    "INSERT",
    "UPDATE",
    "DELETE",
    "DROP",
    "CREATE",
    "ALTER",
    "ATTACH",
    "DETACH",
    "PRAGMA",
    "TRUNCATE",
    "REPLACE",
    "VACUUM",
    "LOAD",
    "INSTALL",
    "EXPORT",
    "IMPORT",
    "COPY",
    "CHECKPOINT",
    "USE",
];

/// Lightweight read-only-ness check: strip line comments, then look for
/// a forbidden keyword as a whole word. `--` line comments and /* */
/// block comments are stripped first so attackers can't smuggle keywords
/// through comments. This is intentionally conservative; if it rejects a
/// legitimate query the caller can use a typed tool.
pub fn is_read_only(query: &str) -> bool {
    let stripped = strip_comments(query);
    let upper = stripped.to_ascii_uppercase();
    for kw in FORBIDDEN {
        if contains_word(&upper, kw) {
            return false;
        }
    }
    true
}

fn strip_comments(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    let bytes = query.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // Line comment: `--` to end of line.
        if c == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment: `/* ... */`.
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(idx) = haystack[start..].find(needle) {
        let abs = start + idx;
        let before_ok = abs == 0 || !is_word_char(haystack.as_bytes()[abs - 1]);
        let end = abs + needle.len();
        let after_ok = end == haystack.len() || !is_word_char(haystack.as_bytes()[end]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

pub fn run(db: &DbConnection, input: RunSqlInput) -> Result<RunSqlOutput, ToolError> {
    if !is_read_only(&input.query) {
        return Err(ToolError::new(
            "write_query_rejected",
            "run_sql only accepts read-only queries; INSERT, UPDATE, DELETE, DROP, \
             CREATE, ALTER, ATTACH, and PRAGMA are rejected.",
        )
        .with_suggestion(
            "Rewrite the query as a SELECT, or use a typed tool such as trace_variable, \
             find_call, or get_call_tree.",
        ));
    }
    let max_rows = input.max_rows.unwrap_or(DEFAULT_MAX_ROWS);

    let conn = db.lock();
    let mut stmt = conn.prepare(&input.query).map_err(map_sql)?;
    // duckdb's column metadata isn't populated until execution. Run the
    // query first, then read the column names off the active statement.
    let mut rows = stmt.query([]).map_err(map_sql)?;
    let column_names: Vec<String> = match rows.as_ref() {
        Some(stmt_ref) => stmt_ref.column_names(),
        None => Vec::new(),
    };
    let mut out_rows: Vec<Vec<JsonValue>> = Vec::new();
    let mut truncated = false;
    let mut row_count: u64 = 0;
    while let Some(row) = rows.next().map_err(map_sql)? {
        if out_rows.len() as u32 >= max_rows {
            truncated = true;
            break;
        }
        let mut record: Vec<JsonValue> = Vec::with_capacity(column_names.len());
        for i in 0..column_names.len() {
            let v: DuckValue = row.get(i).map_err(map_sql)?;
            record.push(duck_value_to_json(&v));
        }
        out_rows.push(record);
        row_count += 1;
    }

    Ok(RunSqlOutput {
        columns: column_names,
        rows: out_rows,
        row_count,
        truncated,
    })
}

fn map_sql(e: duckdb::Error) -> ToolError {
    ToolError::new("sql_error", e.to_string()).with_suggestion(
        "Check syntax against the schema (use describe_schema for table/column names).",
    )
}

fn duck_value_to_json(v: &DuckValue) -> JsonValue {
    use duckdb::types::Value as V;
    match v {
        V::Null => JsonValue::Null,
        V::Boolean(b) => JsonValue::Bool(*b),
        V::TinyInt(n) => JsonValue::from(*n),
        V::SmallInt(n) => JsonValue::from(*n),
        V::Int(n) => JsonValue::from(*n),
        V::BigInt(n) => JsonValue::from(*n),
        V::HugeInt(n) => JsonValue::String(n.to_string()),
        V::UTinyInt(n) => JsonValue::from(*n),
        V::USmallInt(n) => JsonValue::from(*n),
        V::UInt(n) => JsonValue::from(*n),
        V::UBigInt(n) => JsonValue::from(*n),
        V::Float(f) => serde_json::Number::from_f64(*f as f64)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        V::Double(f) => serde_json::Number::from_f64(*f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        V::Text(s) => JsonValue::String(s.clone()),
        V::Blob(b) => JsonValue::String(format!("<blob {} bytes>", b.len())),
        V::Decimal(d) => JsonValue::String(d.to_string()),
        V::Timestamp(_, n) => JsonValue::from(*n),
        V::Date32(d) => JsonValue::from(*d),
        V::Time64(_, n) => JsonValue::from(*n),
        V::Interval {
            months,
            days,
            nanos,
        } => serde_json::json!({
            "months": months, "days": days, "nanos": nanos
        }),
        V::List(values) | V::Array(values) => {
            JsonValue::Array(values.iter().map(duck_value_to_json).collect())
        }
        V::Enum(s) => JsonValue::String(s.clone()),
        V::Struct(fields) => {
            let mut m = serde_json::Map::new();
            for (k, v) in fields.iter() {
                m.insert(k.clone(), duck_value_to_json(v));
            }
            JsonValue::Object(m)
        }
        V::Map(entries) => {
            let mut arr = Vec::new();
            for (k, v) in entries.iter() {
                arr.push(serde_json::json!({
                    "key": duck_value_to_json(k),
                    "value": duck_value_to_json(v),
                }));
            }
            JsonValue::Array(arr)
        }
        V::Union(v) => duck_value_to_json(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_passes() {
        assert!(is_read_only("SELECT * FROM events"));
        assert!(is_read_only("with x as (select 1) select * from x"));
    }

    #[test]
    fn rejects_writes() {
        assert!(!is_read_only("INSERT INTO events VALUES (1)"));
        assert!(!is_read_only("DROP TABLE events"));
        assert!(!is_read_only("PRAGMA enable_progress_bar"));
        assert!(!is_read_only("CREATE TABLE foo(x INT)"));
    }

    #[test]
    fn block_comments_dont_smuggle_writes() {
        assert!(!is_read_only(
            "SELECT 1; /* DROP TABLE */ INSERT INTO x VALUES (1)"
        ));
        assert!(is_read_only("/* DROP */ SELECT 1"));
    }

    #[test]
    fn line_comments_dont_smuggle_writes() {
        assert!(is_read_only("-- DROP TABLE events\nSELECT 1"));
    }

    #[test]
    fn word_boundary_avoids_false_positives() {
        assert!(is_read_only("SELECT created_at FROM events"));
        assert!(is_read_only("SELECT * FROM event_args"));
    }
}
