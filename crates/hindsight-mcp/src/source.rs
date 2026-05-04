// SPDX-License-Identifier: Apache-2.0

//! Helpers for reading source files out of the indexed database and slicing
//! them by line range.
//!
//! Source files are stored whole in `source_files.content`. We read the
//! whole file once per query and slice in Rust; for the size of files
//! Hindsight typically traces this is fast and avoids any DuckDB
//! string-manipulation gymnastics.

use duckdb::params;
use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SourceLine {
    pub line: i32,
    pub content: String,
}

/// Fetch the full source content of `path` from `source_files`. Returns
/// Ok(None) if the file isn't recorded.
pub fn fetch_source_content(db: &DbConnection, path: &str) -> Result<Option<String>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare("SELECT content FROM source_files WHERE path = ?")
        .map_err(map_db)?;
    let mut rows = stmt.query(params![path]).map_err(map_db)?;
    let row = rows.next().map_err(map_db)?;
    match row {
        Some(r) => {
            let content: String = r.get(0).map_err(map_db)?;
            Ok(Some(content))
        }
        None => Ok(None),
    }
}

/// Suggest available file paths when a lookup misses. Returns up to 8.
pub fn list_source_paths(db: &DbConnection) -> Result<Vec<String>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare("SELECT path FROM source_files ORDER BY path LIMIT 16")
        .map_err(map_db)?;
    let mut out = Vec::new();
    let mut rows = stmt.query([]).map_err(map_db)?;
    while let Some(row) = rows.next().map_err(map_db)? {
        let path: String = row.get(0).map_err(map_db)?;
        out.push(path);
    }
    Ok(out)
}

/// Slice `content` by 1-indexed inclusive line range. If `range` is None,
/// returns the entire file with its first line at 1.
pub fn slice_lines(content: &str, range: Option<(i32, i32)>) -> SlicedSource {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len() as i32;
    let (start, end) = match range {
        Some((s, e)) => {
            let s = s.max(1);
            let e = e.min(total_lines);
            (s, e)
        }
        None => (1, total_lines),
    };
    let body = if start > end || total_lines == 0 {
        String::new()
    } else {
        let from = (start - 1) as usize;
        let to = end as usize;
        lines[from..to].join("\n")
    };
    SlicedSource {
        content: body,
        start_line: start,
        end_line: end,
        total_lines,
    }
}

#[derive(Debug, Clone)]
pub struct SlicedSource {
    pub content: String,
    pub start_line: i32,
    pub end_line: i32,
    pub total_lines: i32,
}

/// Fetch a window of source lines around `center_line` from `path`. Returns
/// up to `radius` lines on either side (clamped to file bounds). Empty if
/// the file isn't recorded.
pub fn fetch_window(
    db: &DbConnection,
    path: &str,
    center_line: i32,
    radius: i32,
) -> Result<Vec<SourceLine>, ToolError> {
    let Some(content) = fetch_source_content(db, path)? else {
        return Ok(Vec::new());
    };
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len() as i32;
    if total == 0 {
        return Ok(Vec::new());
    }
    let start = (center_line - radius).max(1);
    let end = (center_line + radius).min(total);
    if start > end {
        return Ok(Vec::new());
    }
    Ok((start..=end)
        .map(|n| SourceLine {
            line: n,
            content: lines[(n - 1) as usize].to_string(),
        })
        .collect())
}

/// Fetch a single line.
pub fn fetch_line(db: &DbConnection, path: &str, line: i32) -> Result<Option<String>, ToolError> {
    let Some(content) = fetch_source_content(db, path)? else {
        return Ok(None);
    };
    let lines: Vec<&str> = content.lines().collect();
    let idx = (line - 1) as usize;
    Ok(lines.get(idx).map(|s| s.to_string()))
}

fn map_db(e: duckdb::Error) -> ToolError {
    ToolError::new("database_error", e.to_string())
}
