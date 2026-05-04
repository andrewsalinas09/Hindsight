// SPDX-License-Identifier: Apache-2.0

//! `find_iterations` — per-iteration breakdown of a loop in a frame.

use std::collections::BTreeMap;

use duckdb::params;
use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;
use crate::value::{ValueSummary, fetch_value_summaries};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindIterationsInput {
    pub trace_id: String,
    pub frame_id: i64,
    /// Source line of the loop header (the `for ...` or `while ...` line).
    pub loop_line: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LoopVariable {
    pub name: String,
    pub value: ValueSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LocalChange {
    pub value: ValueSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BranchTaken {
    pub event_id: i64,
    pub line: i32,
    pub taken: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Iteration {
    pub iteration_index: i32,
    pub first_event_id: i64,
    pub last_event_id: i64,
    /// Locals captured on the loop header line (typically the loop
    /// variable). May be empty for `while` loops with no rebind.
    pub loop_variables: Vec<LoopVariable>,
    pub locals_changed: BTreeMap<String, LocalChange>,
    pub branches_taken: Vec<BranchTaken>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindIterationsOutput {
    pub frame_id: i64,
    pub loop_line: i32,
    pub iteration_count: i32,
    pub iterations: Vec<Iteration>,
}

pub fn run(
    db: &DbConnection,
    input: FindIterationsInput,
) -> Result<FindIterationsOutput, ToolError> {
    let starts = fetch_iteration_starts(db, input.frame_id, input.loop_line)?;
    if starts.is_empty() {
        return Ok(FindIterationsOutput {
            frame_id: input.frame_id,
            loop_line: input.loop_line,
            iteration_count: 0,
            iterations: Vec::new(),
        });
    }

    // Determine the upper bound: the frame's exit_event_id (or the last
    // event_id observed if the frame didn't exit).
    let frame_end = fetch_frame_end(db, input.frame_id)?;

    // Bounds for each iteration: [start_i, start_{i+1}).
    let mut iter_bounds: Vec<(i64, i64)> = Vec::with_capacity(starts.len());
    for (i, s) in starts.iter().enumerate() {
        let end = if i + 1 < starts.len() {
            starts[i + 1] - 1
        } else {
            frame_end
        };
        iter_bounds.push((*s, end));
    }

    let mut iterations = Vec::with_capacity(iter_bounds.len());
    for (idx, (start, end)) in iter_bounds.iter().enumerate() {
        let loop_vars = fetch_locals_on_line(db, input.frame_id, *start, input.loop_line)?;
        let locals_changed =
            fetch_iteration_changes(db, input.frame_id, *start, *end, input.loop_line)?;
        let branches_taken = fetch_branches_in_range(db, input.frame_id, *start, *end)?;
        iterations.push(Iteration {
            iteration_index: idx as i32,
            first_event_id: *start,
            last_event_id: *end,
            loop_variables: loop_vars,
            locals_changed,
            branches_taken,
        });
    }

    let count = iterations.len() as i32;
    Ok(FindIterationsOutput {
        frame_id: input.frame_id,
        loop_line: input.loop_line,
        iteration_count: count,
        iterations,
    })
}

fn fetch_iteration_starts(
    db: &DbConnection,
    frame_id: i64,
    loop_line: i32,
) -> Result<Vec<i64>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT event_id FROM events WHERE frame_id = ? AND type = 'line_delta' AND line = ? \
             ORDER BY event_id",
        )
        .map_err(map_db)?;
    let rows = stmt
        .query_map(params![frame_id, loop_line], |r| r.get::<_, i64>(0))
        .map_err(map_db)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(map_db)?);
    }
    Ok(out)
}

fn fetch_frame_end(db: &DbConnection, frame_id: i64) -> Result<i64, ToolError> {
    let conn = db.lock();
    let exit: Option<Option<i64>> = conn
        .query_row(
            "SELECT exit_event_id FROM frames WHERE frame_id = ?",
            params![frame_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .ok();
    if let Some(Some(eid)) = exit {
        return Ok(eid);
    }
    let last: Option<i64> = conn
        .query_row(
            "SELECT MAX(event_id) FROM events WHERE frame_id = ?",
            params![frame_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .map_err(map_db)?;
    Ok(last.unwrap_or(i64::MAX))
}

fn fetch_locals_on_line(
    db: &DbConnection,
    frame_id: i64,
    event_id: i64,
    line: i32,
) -> Result<Vec<LoopVariable>, ToolError> {
    // Locals captured in this exact event (the LINE_DELTA on the loop header).
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT el.name, el.value_id FROM event_locals el JOIN events e ON el.event_id = e.event_id \
             WHERE el.event_id = ? AND el.frame_id = ? AND e.line = ?",
        )
        .map_err(map_db)?;
    let rows = stmt
        .query_map(params![event_id, frame_id, line], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(map_db)?;
    let pairs: Vec<(String, i64)> = rows.collect::<duckdb::Result<Vec<_>>>().map_err(map_db)?;
    drop(stmt);
    drop(conn);

    let value_ids: Vec<i64> = pairs.iter().map(|(_, v)| *v).collect();
    let mut summaries = fetch_value_summaries(db, &value_ids)?;
    Ok(pairs
        .into_iter()
        .filter_map(|(name, vid)| {
            summaries
                .remove(&vid)
                .map(|v| LoopVariable { name, value: v })
        })
        .collect())
}

fn fetch_iteration_changes(
    db: &DbConnection,
    frame_id: i64,
    start: i64,
    end: i64,
    loop_line: i32,
) -> Result<BTreeMap<String, LocalChange>, ToolError> {
    // For each name captured in this iteration's body (excluding the loop
    // header itself, which we already report as loop_variables), find the
    // last value before this iteration started so we can show "previous".
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT el.name, MAX(el.event_id) AS last_event \
             FROM event_locals el JOIN events e ON el.event_id = e.event_id \
             WHERE el.frame_id = ? AND el.event_id > ? AND el.event_id <= ? \
             AND NOT (el.event_id = ? AND e.line = ?) \
             GROUP BY el.name",
        )
        .map_err(map_db)?;
    let rows = stmt
        .query_map(params![frame_id, start, end, start, loop_line], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(map_db)?;
    let pairs: Vec<(String, i64)> = rows.collect::<duckdb::Result<Vec<_>>>().map_err(map_db)?;
    drop(stmt);
    drop(conn);

    let mut out = BTreeMap::new();
    for (name, last_event) in pairs {
        let conn = db.lock();
        let cur_value: i64 = conn
            .query_row(
                "SELECT value_id FROM event_locals WHERE event_id = ? AND name = ?",
                params![last_event, &name],
                |r| r.get(0),
            )
            .map_err(map_db)?;
        let prev: Option<(i64, i64)> = conn
            .query_row(
                "SELECT event_id, value_id FROM event_locals WHERE frame_id = ? AND name = ? \
                 AND event_id < ? ORDER BY event_id DESC LIMIT 1",
                params![frame_id, &name, start + 1],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        drop(conn);

        let cur_summary = fetch_value_summaries(db, &[cur_value])?
            .remove(&cur_value)
            .ok_or_else(|| {
                ToolError::new(
                    "internal_error",
                    format!("value_id {cur_value} not in values"),
                )
            })?;
        let prev_display = match prev {
            Some((_, prev_vid)) => fetch_value_summaries(db, &[prev_vid])?
                .remove(&prev_vid)
                .map(|s| s.display),
            None => None,
        };
        out.insert(
            name,
            LocalChange {
                value: cur_summary,
                previous: prev_display,
            },
        );
    }
    Ok(out)
}

fn fetch_branches_in_range(
    db: &DbConnection,
    frame_id: i64,
    start: i64,
    end: i64,
) -> Result<Vec<BranchTaken>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT event_id, line, taken FROM branches \
             WHERE frame_id = ? AND event_id >= ? AND event_id <= ? ORDER BY event_id",
        )
        .map_err(map_db)?;
    let rows = stmt
        .query_map(params![frame_id, start, end], |r| {
            Ok(BranchTaken {
                event_id: r.get(0)?,
                line: r.get(1)?,
                taken: r.get(2)?,
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
