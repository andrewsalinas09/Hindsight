// SPDX-License-Identifier: Apache-2.0

//! `causal_slice` — walk backward from a value to identify what produced
//! it, recursively up to `max_depth` levels.
//!
//! This is the canonical "operations beyond SQL" tool. It composes:
//!   1. SQL: find the most recent capture of `value_id` that has a name.
//!   2. SQL: fetch the source line at that capture.
//!   3. Source parsing: extract identifiers on the RHS of the assignment.
//!   4. SQL: walk back to find each dependency's value at the capture.
//!   5. Recurse.

use std::collections::HashSet;

use duckdb::params;
use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;
use crate::identifiers::rhs_identifiers;
use crate::source::fetch_line;
use crate::value::{ValueSummary, fetch_value_summary};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CausalSliceInput {
    pub value_id: i64,
    /// How far back to walk. Default 5.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CapturedAs {
    pub name: String,
    pub frame_id: i64,
    pub event_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DependencyNode {
    pub name: String,
    pub value: ValueSummary,
    pub captured_at_event: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<String>,
    pub depends_on: Vec<DependencyNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CausalSliceOutput {
    pub root_value: ValueSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_as: Option<CapturedAs>,
    pub depends_on: Vec<DependencyNode>,
    pub depth_reached: i32,
    pub truncated: bool,
    /// Documents the source-parsing strategy and its limits so the LLM can
    /// caveat its conclusions.
    pub parser_notes: Vec<String>,
}

const DEFAULT_MAX_DEPTH: i32 = 5;

pub fn run(db: &DbConnection, input: CausalSliceInput) -> Result<CausalSliceOutput, ToolError> {
    let max_depth = input.max_depth.unwrap_or(DEFAULT_MAX_DEPTH).max(0);

    let root_value = fetch_value_summary(db, input.value_id)?.ok_or_else(|| {
        ToolError::new(
            "value_not_found",
            format!("No value with value_id={}", input.value_id),
        )
        .with_suggestion("Use run_sql against the values table to find a valid value_id.")
    })?;

    let captured = find_capture(db, input.value_id)?;
    let captured_info = captured.as_ref().map(|c| CapturedAs {
        name: c.name.clone(),
        frame_id: c.frame_id,
        event_id: c.event_id,
        line: c.line,
        source: c.source.clone(),
    });

    // Cycle key: (frame_id, name, event_id) — true value-flow cycles
    // would re-enter the same capture; value-id reuse (e.g. interned
    // small ints) is not a cycle.
    let mut visited: HashSet<(i64, String, i64)> = HashSet::new();
    if let Some(c) = &captured {
        visited.insert((c.frame_id, c.name.clone(), c.event_id));
    }

    let mut truncated = false;
    let mut depth_reached = 0i32;
    let depends_on = match captured {
        Some(c) => walk_dependencies(
            db,
            &c,
            max_depth,
            1,
            &mut visited,
            &mut truncated,
            &mut depth_reached,
        )?,
        None => Vec::new(),
    };

    Ok(CausalSliceOutput {
        root_value,
        captured_as: captured_info,
        depends_on,
        depth_reached,
        truncated,
        parser_notes: vec![
            "RHS dependencies extracted via byte-level Python identifier scanner. \
             Skips comments and string literals; treats `obj.attr` as `obj`. May miss \
             identifiers in f-strings."
                .into(),
            "Walks backward through event_locals; stops at function arguments, constants, \
             unparseable lines, max_depth, or values not in the trace."
                .into(),
        ],
    })
}

#[derive(Debug, Clone)]
struct Capture {
    event_id: i64,
    frame_id: i64,
    name: String,
    line: Option<i32>,
    source: Option<String>,
}

fn find_capture(db: &DbConnection, value_id: i64) -> Result<Option<Capture>, ToolError> {
    // Most recent capture in event_locals with a name.
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT el.event_id, el.frame_id, el.name, e.line, e.source_file \
             FROM event_locals el JOIN events e ON el.event_id = e.event_id \
             WHERE el.value_id = ? ORDER BY el.event_id DESC LIMIT 1",
        )
        .map_err(map_db)?;
    let mut rows = stmt.query(params![value_id]).map_err(map_db)?;
    if let Some(r) = rows.next().map_err(map_db)? {
        let event_id: i64 = r.get(0).map_err(map_db)?;
        let frame_id: i64 = r.get(1).map_err(map_db)?;
        let name: String = r.get(2).map_err(map_db)?;
        let line: Option<i32> = r.get(3).map_err(map_db)?;
        let source_file: Option<String> = r.get(4).map_err(map_db)?;
        drop(rows);
        drop(stmt);
        drop(conn);
        let source = match (source_file.as_ref(), line) {
            (Some(p), Some(l)) => fetch_line(db, p, l)?,
            _ => None,
        };
        Ok(Some(Capture {
            event_id,
            frame_id,
            name,
            line,
            source,
        }))
    } else {
        Ok(None)
    }
}

fn walk_dependencies(
    db: &DbConnection,
    capture: &Capture,
    max_depth: i32,
    cur_depth: i32,
    visited: &mut HashSet<(i64, String, i64)>,
    truncated: &mut bool,
    depth_reached: &mut i32,
) -> Result<Vec<DependencyNode>, ToolError> {
    if cur_depth > max_depth {
        *truncated = true;
        return Ok(Vec::new());
    }
    *depth_reached = (*depth_reached).max(cur_depth);

    // Detect: is this capture a function argument? If so, mark and stop.
    let is_arg = is_function_argument(db, capture.event_id, &capture.name)?;

    let source_text = match &capture.source {
        Some(t) => t.clone(),
        None => return Ok(Vec::new()),
    };

    // For function-entry captures, the "source" is the def line, which
    // doesn't have an RHS. Bail with a note.
    if is_arg {
        return Ok(Vec::new());
    }

    let mut idents = rhs_identifiers(&source_text);
    idents.retain(|n| n != &capture.name);
    let mut seen = HashSet::new();
    idents.retain(|n| seen.insert(n.clone()));

    if idents.is_empty() {
        return Ok(Vec::new());
    }

    let mut out: Vec<DependencyNode> = Vec::new();
    for name in idents {
        let dep = lookup_dependency(db, capture, &name)?;
        match dep {
            Some(dep_capture) => {
                let value =
                    fetch_value_summary(db, dep_capture.value_id)?.unwrap_or(ValueSummary {
                        value_id: dep_capture.value_id,
                        type_tag: "unknown".into(),
                        display: "<missing>".into(),
                        type_name: None,
                        container_length: None,
                    });
                let cycle_key = (capture.frame_id, name.clone(), dep_capture.event_id);
                let already_visited = !visited.insert(cycle_key);
                let mut note: Option<String> = None;
                let dep_capture_inner = Capture {
                    event_id: dep_capture.event_id,
                    frame_id: capture.frame_id,
                    name: name.clone(),
                    line: dep_capture.line,
                    source: dep_capture.source.clone(),
                };
                if already_visited {
                    note = Some("cycle: value already in walk".into());
                }
                let inner = if already_visited {
                    Vec::new()
                } else {
                    walk_dependencies(
                        db,
                        &dep_capture_inner,
                        max_depth,
                        cur_depth + 1,
                        visited,
                        truncated,
                        depth_reached,
                    )?
                };
                if note.is_none() && is_function_argument(db, dep_capture.event_id, &name)? {
                    note = Some("function argument".into());
                }
                out.push(DependencyNode {
                    name,
                    value,
                    captured_at_event: dep_capture.event_id,
                    source_line: dep_capture.source.clone(),
                    depends_on: inner,
                    note,
                });
            }
            None => {
                out.push(DependencyNode {
                    name: name.clone(),
                    value: ValueSummary {
                        value_id: 0,
                        type_tag: "unresolved".into(),
                        display: format!("<no capture for `{name}`>"),
                        type_name: None,
                        container_length: None,
                    },
                    captured_at_event: 0,
                    source_line: None,
                    depends_on: Vec::new(),
                    note: Some("no capture in this frame at or before the assignment".into()),
                });
            }
        }
    }
    Ok(out)
}

#[derive(Debug)]
struct DepCapture {
    event_id: i64,
    value_id: i64,
    line: Option<i32>,
    source: Option<String>,
}

fn lookup_dependency(
    db: &DbConnection,
    capture: &Capture,
    name: &str,
) -> Result<Option<DepCapture>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT el.event_id, el.value_id, e.line, e.source_file \
             FROM event_locals el JOIN events e ON el.event_id = e.event_id \
             WHERE el.frame_id = ? AND el.name = ? AND el.event_id <= ? \
             ORDER BY el.event_id DESC LIMIT 1",
        )
        .map_err(map_db)?;
    let mut rows = stmt
        .query(params![capture.frame_id, name, capture.event_id])
        .map_err(map_db)?;
    if let Some(r) = rows.next().map_err(map_db)? {
        let event_id: i64 = r.get(0).map_err(map_db)?;
        let value_id: i64 = r.get(1).map_err(map_db)?;
        let line: Option<i32> = r.get(2).map_err(map_db)?;
        let source_file: Option<String> = r.get(3).map_err(map_db)?;
        drop(rows);
        drop(stmt);
        drop(conn);
        let source = match (source_file.as_ref(), line) {
            (Some(p), Some(l)) => fetch_line(db, p, l)?,
            _ => None,
        };
        Ok(Some(DepCapture {
            event_id,
            value_id,
            line,
            source,
        }))
    } else {
        Ok(None)
    }
}

fn is_function_argument(db: &DbConnection, event_id: i64, name: &str) -> Result<bool, ToolError> {
    let conn = db.lock();
    // Either the lookup landed exactly on the FUNCTION_ENTRY event, or
    // the name is an argument of the frame's entry event (i.e. it's a
    // re-capture of an argument by a FRAME_SNAPSHOT or LINE_DELTA in the
    // same frame).
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM event_args ea \
             JOIN events e ON ea.event_id = e.event_id \
             WHERE ea.name = ? AND ( \
               ea.event_id = ? OR \
               ea.event_id = (SELECT entry_event_id FROM frames WHERE frame_id = e.frame_id) \
             )",
            params![name, event_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if count > 0 {
        return Ok(true);
    }
    // Fallback: walk to the frame holding this event, then check if its
    // entry_event_id has this argument name.
    let arg_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM event_args ea \
             WHERE ea.name = ? AND ea.event_id IN ( \
               SELECT entry_event_id FROM frames \
               WHERE frame_id = (SELECT frame_id FROM events WHERE event_id = ?) \
             )",
            params![name, event_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(arg_count > 0)
}

fn map_db(e: duckdb::Error) -> ToolError {
    ToolError::new("database_error", e.to_string())
}
