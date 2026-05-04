// SPDX-License-Identifier: Apache-2.0

//! `trace_variable` — full history of a variable in a frame.

use std::collections::{BTreeMap, HashMap};

use duckdb::params;
use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;
use crate::identifiers::extract_identifiers;
use crate::source::fetch_line;
use crate::value::{ValueSummary, fetch_value_summaries};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceVariableInput {
    pub trace_id: String,
    pub name: String,
    pub frame_id: i64,
    /// If specified, only returns captures at or before this event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_event_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VariableCapture {
    pub event_id: i64,
    pub line: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<String>,
    pub value: ValueSummary,
    /// Other locals captured at the same event_id whose names appear in
    /// the source line — included so the LLM can narrate "x became 10
    /// because item was 10."
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, ValueSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FrameSummaryInfo {
    pub qualified_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceVariableOutput {
    pub name: String,
    pub frame_id: i64,
    pub captures: Vec<VariableCapture>,
    pub total_captures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_summary: Option<FrameSummaryInfo>,
}

pub fn run(db: &DbConnection, input: TraceVariableInput) -> Result<TraceVariableOutput, ToolError> {
    let frame_summary = fetch_frame_summary(db, input.frame_id)?;
    if frame_summary.is_none() {
        return Err(ToolError::new(
            "frame_not_found",
            format!("No frame with frame_id={}", input.frame_id),
        )
        .with_suggestion("Use find_call to locate frames by qualified_name."));
    }

    let raw_captures = fetch_captures(db, &input)?;
    if raw_captures.is_empty() {
        return Ok(TraceVariableOutput {
            name: input.name,
            frame_id: input.frame_id,
            captures: Vec::new(),
            total_captures: 0,
            frame_summary,
        });
    }

    // Pull the value summaries we need in one shot.
    let value_ids: Vec<i64> = raw_captures.iter().map(|c| c.value_id).collect();
    let mut summaries = fetch_value_summaries(db, &value_ids)?;

    // Pull source lines on demand. Cache per (file, line).
    let mut src_cache: HashMap<(String, i32), Option<String>> = HashMap::new();

    let mut captures: Vec<VariableCapture> = Vec::with_capacity(raw_captures.len());
    for raw in &raw_captures {
        let value = summaries.remove(&raw.value_id).unwrap_or(ValueSummary {
            value_id: raw.value_id,
            type_tag: "unknown".into(),
            display: "<missing>".into(),
            type_name: None,
            container_length: None,
        });
        let source_line = match (raw.source_file.as_ref(), raw.line) {
            (Some(file), Some(ln)) => {
                let key = (file.clone(), ln);
                src_cache
                    .entry(key)
                    .or_insert_with_key(|(f, l)| fetch_line(db, f, *l).ok().flatten())
                    .clone()
            }
            _ => None,
        };
        let context = match &source_line {
            Some(line_text) => {
                let mut idents: Vec<String> = extract_identifiers(line_text)
                    .into_iter()
                    .filter(|n| n != &input.name)
                    .collect();
                idents.sort();
                idents.dedup();
                fetch_context_locals(db, input.frame_id, raw.event_id, &idents)?
            }
            None => Default::default(),
        };
        captures.push(VariableCapture {
            event_id: raw.event_id,
            line: raw.line,
            source_line,
            value,
            context,
        });
    }

    let total_captures = captures.len() as u64;
    Ok(TraceVariableOutput {
        name: input.name,
        frame_id: input.frame_id,
        captures,
        total_captures,
        frame_summary,
    })
}

struct RawCapture {
    event_id: i64,
    value_id: i64,
    line: Option<i32>,
    source_file: Option<String>,
}

fn fetch_captures(
    db: &DbConnection,
    input: &TraceVariableInput,
) -> Result<Vec<RawCapture>, ToolError> {
    let conn = db.lock();
    let sql = "SELECT el.event_id, el.value_id, e.line, e.source_file \
               FROM event_locals el JOIN events e ON el.event_id = e.event_id \
               WHERE el.frame_id = ? AND el.name = ? \
               AND (CAST(? AS BIGINT) IS NULL OR el.event_id <= CAST(? AS BIGINT)) \
               ORDER BY el.event_id";
    let before: Option<i64> = input.before_event_id;
    let mut stmt = conn.prepare(sql).map_err(map_db)?;
    let rows = stmt
        .query_map(params![input.frame_id, input.name, before, before], |row| {
            Ok(RawCapture {
                event_id: row.get(0)?,
                value_id: row.get(1)?,
                line: row.get(2)?,
                source_file: row.get(3)?,
            })
        })
        .map_err(map_db)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(map_db)?);
    }
    Ok(out)
}

fn fetch_frame_summary(
    db: &DbConnection,
    frame_id: i64,
) -> Result<Option<FrameSummaryInfo>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare("SELECT qualified_name, argument_summary FROM frames WHERE frame_id = ?")
        .map_err(map_db)?;
    let mut rows = stmt.query(params![frame_id]).map_err(map_db)?;
    let row = rows.next().map_err(map_db)?;
    Ok(row.map(|r| FrameSummaryInfo {
        qualified_name: r.get(0).unwrap_or_default(),
        argument_summary: r.get(1).ok(),
    }))
}

fn fetch_context_locals(
    db: &DbConnection,
    frame_id: i64,
    event_id: i64,
    names: &[String],
) -> Result<BTreeMap<String, ValueSummary>, ToolError> {
    if names.is_empty() {
        return Ok(Default::default());
    }
    let conn = db.lock();
    // For each requested name, walk back to its most recent value at or
    // before event_id. DuckDB's window functions make this clean.
    let placeholders: Vec<&str> = names.iter().map(|_| "?").collect();
    let sql = format!(
        "SELECT name, value_id FROM ( \
           SELECT el.name, el.value_id, ROW_NUMBER() OVER (PARTITION BY el.name ORDER BY el.event_id DESC) AS rn \
           FROM event_locals el \
           WHERE el.frame_id = ? AND el.event_id <= ? AND el.name IN ({}) \
         ) WHERE rn = 1",
        placeholders.join(", ")
    );
    let mut bind: Vec<&dyn duckdb::ToSql> = Vec::with_capacity(2 + names.len());
    bind.push(&frame_id as &dyn duckdb::ToSql);
    bind.push(&event_id as &dyn duckdb::ToSql);
    for n in names {
        bind.push(n as &dyn duckdb::ToSql);
    }
    let mut stmt = conn.prepare(&sql).map_err(map_db)?;
    let rows = stmt
        .query_map(bind.as_slice(), |row| {
            let name: String = row.get(0)?;
            let value_id: i64 = row.get(1)?;
            Ok((name, value_id))
        })
        .map_err(map_db)?;
    let pairs: Vec<(String, i64)> = rows.collect::<duckdb::Result<Vec<_>>>().map_err(map_db)?;
    drop(stmt);
    drop(conn);

    let value_ids: Vec<i64> = pairs.iter().map(|(_, vid)| *vid).collect();
    let mut summaries = fetch_value_summaries(db, &value_ids)?;
    let mut out = BTreeMap::new();
    for (name, value_id) in pairs {
        if let Some(s) = summaries.remove(&value_id) {
            out.insert(name, s);
        }
    }
    Ok(out)
}

fn map_db(e: duckdb::Error) -> ToolError {
    ToolError::new("database_error", e.to_string())
}
