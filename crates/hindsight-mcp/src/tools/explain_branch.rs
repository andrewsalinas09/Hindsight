// SPDX-License-Identifier: Apache-2.0

//! `explain_branch` — given a BRANCH_RESULT event, return everything needed
//! to understand why it went the way it did.

use std::collections::BTreeMap;

use duckdb::params;
use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;
use crate::identifiers::extract_identifiers;
use crate::source::{SourceLine, fetch_window};
use crate::value::{ValueSummary, fetch_value_summaries};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExplainBranchInput {
    pub event_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SourceContext {
    pub lines: Vec<SourceLine>,
    pub branch_line: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NextEventSummary {
    pub event_id: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub line: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExplainBranchOutput {
    pub event_id: i64,
    pub frame_id: i64,
    pub function_name: String,
    pub line: i32,
    pub taken: bool,
    pub source_context: SourceContext,
    pub locals_at_branch: BTreeMap<String, ValueSummary>,
    pub next_events: Vec<NextEventSummary>,
}

const SOURCE_RADIUS: i32 = 2;
const NEXT_EVENTS: i64 = 4;

pub fn run(db: &DbConnection, input: ExplainBranchInput) -> Result<ExplainBranchOutput, ToolError> {
    let row = fetch_branch_row(db, input.event_id)?;
    let Some(branch) = row else {
        return Err(ToolError::new(
            "branch_not_found",
            format!("event_id={} is not a branch_result event", input.event_id),
        )
        .with_suggestion(
            "Use run_sql with `SELECT * FROM branches LIMIT 10` to find a branch event_id.",
        ));
    };

    let lines = fetch_window(db, &branch.source_file, branch.line, SOURCE_RADIUS)?;
    let source_context = SourceContext {
        lines,
        branch_line: branch.line,
    };

    // Identifiers in the branch source line, walked back through event_locals.
    let branch_line_text = source_context
        .lines
        .iter()
        .find(|l| l.line == branch.line)
        .map(|l| l.content.clone())
        .unwrap_or_default();
    let mut idents: Vec<String> = extract_identifiers(&branch_line_text);
    idents.sort();
    idents.dedup();
    let locals_at_branch = fetch_locals_at(db, branch.frame_id, branch.event_id, &idents)?;

    let next_events = fetch_next_events(db, branch.event_id, branch.frame_id)?;

    Ok(ExplainBranchOutput {
        event_id: branch.event_id,
        frame_id: branch.frame_id,
        function_name: branch.function_name,
        line: branch.line,
        taken: branch.taken,
        source_context,
        locals_at_branch,
        next_events,
    })
}

struct BranchRow {
    event_id: i64,
    frame_id: i64,
    function_name: String,
    source_file: String,
    line: i32,
    taken: bool,
}

fn fetch_branch_row(db: &DbConnection, event_id: i64) -> Result<Option<BranchRow>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT event_id, frame_id, function_name, source_file, line, taken FROM branches \
             WHERE event_id = ?",
        )
        .map_err(map_db)?;
    let mut rows = stmt.query(params![event_id]).map_err(map_db)?;
    if let Some(r) = rows.next().map_err(map_db)? {
        Ok(Some(BranchRow {
            event_id: r.get(0).map_err(map_db)?,
            frame_id: r.get(1).map_err(map_db)?,
            function_name: r.get(2).map_err(map_db)?,
            source_file: r.get(3).map_err(map_db)?,
            line: r.get(4).map_err(map_db)?,
            taken: r.get(5).map_err(map_db)?,
        }))
    } else {
        Ok(None)
    }
}

fn fetch_locals_at(
    db: &DbConnection,
    frame_id: i64,
    event_id: i64,
    names: &[String],
) -> Result<BTreeMap<String, ValueSummary>, ToolError> {
    if names.is_empty() {
        return Ok(Default::default());
    }
    let conn = db.lock();
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
    let pairs: Vec<(String, i64)> = stmt
        .query_map(bind.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(map_db)?
        .collect::<duckdb::Result<Vec<_>>>()
        .map_err(map_db)?;
    drop(stmt);
    drop(conn);

    let value_ids: Vec<i64> = pairs.iter().map(|(_, v)| *v).collect();
    let mut summaries = fetch_value_summaries(db, &value_ids)?;
    let mut out = BTreeMap::new();
    for (name, vid) in pairs {
        if let Some(s) = summaries.remove(&vid) {
            out.insert(name, s);
        }
    }
    Ok(out)
}

fn fetch_next_events(
    db: &DbConnection,
    event_id: i64,
    frame_id: i64,
) -> Result<Vec<NextEventSummary>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT event_id, type, line, branch_taken FROM events \
             WHERE event_id > ? AND frame_id = ? ORDER BY event_id LIMIT ?",
        )
        .map_err(map_db)?;
    let rows = stmt
        .query_map(params![event_id, frame_id, NEXT_EVENTS], |r| {
            Ok(NextEventSummary {
                event_id: r.get(0)?,
                event_type: r.get(1)?,
                line: r.get(2)?,
                taken: r.get(3)?,
            })
        })
        .map_err(map_db)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(map_db)?);
    }
    Ok(out)
}

fn map_db(e: duckdb::Error) -> ToolError {
    ToolError::new("database_error", e.to_string())
}
