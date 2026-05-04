// SPDX-License-Identifier: Apache-2.0

//! `exception_chain` — given an EXCEPTION_RAISED event, return the
//! propagation chain showing where (if anywhere) it was caught.

use duckdb::params;
use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;
use crate::source::fetch_line;
use crate::value::fetch_value_summary;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExceptionChainInput {
    pub event_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CaughtAt {
    pub line: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PropagationStep {
    pub frame_id: i64,
    pub qualified_name: String,
    pub raise_event_id: i64,
    pub raise_line: i32,
    pub exit_kind: Option<String>,
    pub exit_event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caught_at: Option<CaughtAt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExceptionChainOutput {
    pub exception_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exception_repr: Option<String>,
    pub raise_event_id: i64,
    pub propagation: Vec<PropagationStep>,
    pub ultimately_caught: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catching_frame: Option<i64>,
}

pub fn run(
    db: &DbConnection,
    input: ExceptionChainInput,
) -> Result<ExceptionChainOutput, ToolError> {
    let raise = fetch_raise(db, input.event_id)?.ok_or_else(|| {
        ToolError::new(
            "exception_event_not_found",
            format!(
                "event_id={} is not an exception_raised event",
                input.event_id
            ),
        )
        .with_suggestion(
            "Use run_sql with `SELECT event_id, exception_type FROM exceptions LIMIT 10`.",
        )
    })?;

    let exception_repr = fetch_value_summary(db, raise.exception_value_id)?.map(|s| s.display);

    // All raise events sharing this exception_value_id (this exception's
    // walk through the stack).
    let raises = fetch_raises_for_value(db, raise.exception_value_id)?;

    // Find the catcher: the first frame above the deepest raise that
    // exited 'returned'.
    let mut propagation = Vec::with_capacity(raises.len());
    let mut catching_frame_id: Option<i64> = None;
    for r in &raises {
        let frame_info = fetch_frame_info(db, r.frame_id)?;
        let (exit_kind, exit_event_id, qualified_name) = match frame_info {
            Some(f) => (f.exit_kind, f.exit_event_id, f.qualified_name),
            None => (None, None, String::new()),
        };
        let caught_at = if exit_kind.as_deref() == Some("returned") {
            // Find the next non-exception event in this frame after the
            // last raise event, and treat its line as the recovery line.
            catching_frame_id = Some(r.frame_id);
            fetch_recovery_line(db, r.frame_id, r.event_id)?
        } else {
            None
        };
        propagation.push(PropagationStep {
            frame_id: r.frame_id,
            qualified_name,
            raise_event_id: r.event_id,
            raise_line: r.line,
            exit_kind,
            exit_event_id,
            caught_at,
        });
    }

    Ok(ExceptionChainOutput {
        exception_type: raise.exception_type,
        exception_repr,
        raise_event_id: input.event_id,
        propagation,
        ultimately_caught: catching_frame_id.is_some(),
        catching_frame: catching_frame_id,
    })
}

struct RaiseRow {
    event_id: i64,
    frame_id: i64,
    line: i32,
    exception_type: String,
    exception_value_id: i64,
}

fn fetch_raise(db: &DbConnection, event_id: i64) -> Result<Option<RaiseRow>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT event_id, frame_id, line, exception_type, exception_value_id \
             FROM exceptions WHERE event_id = ?",
        )
        .map_err(map_db)?;
    let mut rows = stmt.query(params![event_id]).map_err(map_db)?;
    if let Some(r) = rows.next().map_err(map_db)? {
        Ok(Some(RaiseRow {
            event_id: r.get(0).map_err(map_db)?,
            frame_id: r.get(1).map_err(map_db)?,
            line: r.get(2).map_err(map_db)?,
            exception_type: r.get(3).map_err(map_db)?,
            exception_value_id: r.get(4).map_err(map_db)?,
        }))
    } else {
        Ok(None)
    }
}

fn fetch_raises_for_value(
    db: &DbConnection,
    exception_value_id: i64,
) -> Result<Vec<RaiseRow>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT event_id, frame_id, line, exception_type, exception_value_id FROM exceptions \
             WHERE exception_value_id = ? ORDER BY event_id",
        )
        .map_err(map_db)?;
    let rows = stmt
        .query_map(params![exception_value_id], |r| {
            Ok(RaiseRow {
                event_id: r.get(0)?,
                frame_id: r.get(1)?,
                line: r.get(2)?,
                exception_type: r.get(3)?,
                exception_value_id: r.get(4)?,
            })
        })
        .map_err(map_db)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(map_db)?);
    }
    Ok(out)
}

struct FrameInfo {
    qualified_name: String,
    exit_kind: Option<String>,
    exit_event_id: Option<i64>,
}

fn fetch_frame_info(db: &DbConnection, frame_id: i64) -> Result<Option<FrameInfo>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare("SELECT qualified_name, exit_kind, exit_event_id FROM frames WHERE frame_id = ?")
        .map_err(map_db)?;
    let mut rows = stmt.query(params![frame_id]).map_err(map_db)?;
    if let Some(r) = rows.next().map_err(map_db)? {
        Ok(Some(FrameInfo {
            qualified_name: r.get(0).map_err(map_db)?,
            exit_kind: r.get(1).map_err(map_db)?,
            exit_event_id: r.get(2).map_err(map_db)?,
        }))
    } else {
        Ok(None)
    }
}

fn fetch_recovery_line(
    db: &DbConnection,
    frame_id: i64,
    after_raise_event: i64,
) -> Result<Option<CaughtAt>, ToolError> {
    let conn = db.lock();
    // The catching frame's first non-exception event after the raise is
    // typically the `except`/`finally` block.
    let mut stmt = conn
        .prepare(
            "SELECT line, source_file FROM events WHERE frame_id = ? AND event_id > ? \
             AND type != 'exception_raised' AND line IS NOT NULL ORDER BY event_id LIMIT 1",
        )
        .map_err(map_db)?;
    let mut rows = stmt
        .query(params![frame_id, after_raise_event])
        .map_err(map_db)?;
    if let Some(r) = rows.next().map_err(map_db)? {
        let line: i32 = r.get(0).map_err(map_db)?;
        let source_file: Option<String> = r.get(1).map_err(map_db)?;
        drop(rows);
        drop(stmt);
        drop(conn);
        let source = match source_file {
            Some(p) => fetch_line(db, &p, line)?,
            None => None,
        };
        Ok(Some(CaughtAt { line, source }))
    } else {
        Ok(None)
    }
}

fn map_db(e: duckdb::Error) -> ToolError {
    ToolError::new("database_error", e.to_string())
}
