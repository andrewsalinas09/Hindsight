// SPDX-License-Identifier: Apache-2.0

//! Indexes Hindsight trace files into an embedded DuckDB database.
//!
//! The schema is documented in `docs/indexer-schema.md` (v0.1). The wire
//! format the indexer reads is documented in `docs/trace-format.md`.
//!
//! Public entry points:
//!
//! - [`Indexer::index`] — read a `.hindsight` trace from disk, write a
//!   DuckDB database to disk, idempotently. The database file is deleted
//!   first if it already exists; on failure the partial database is
//!   removed so the caller is left with no half-indexed file.
//!
//! Internal layout:
//!
//! - `schema` — DDL constants and `create_tables` / `create_indexes`.
//! - `metadata` — TOML parsing of the trace's initial metadata + final
//!   summary blocks.
//! - `frames` — first-pass walk of the event stream that builds the
//!   `frames` table and computes per-event implicit frame_id + absolute
//!   timestamp.
//! - `values` — `values` and `value_elements` materialization.
//! - `error` — `IndexError` / `Result`.

mod error;
mod frames;
mod metadata;
mod schema;
mod values;
pub mod verify;

pub use error::{IndexError, Result};
pub use verify::{VerifyReport, verify, verify_to_string};

use std::path::Path;

use duckdb::{Connection, ToSql, params};
use hindsight_format::{BoundaryType, Event, EventTag, TraceReader};

use crate::frames::FramePass;
use crate::metadata::FinalSummary;

pub struct Indexer;

impl Indexer {
    /// Read `trace_path`, populate a fresh DuckDB at `db_path`. Idempotent:
    /// any existing file at `db_path` is removed first. On error, the
    /// partial database is removed so the caller is left with either the
    /// previous good state (none, since we deleted it) or a clean slate.
    pub fn index(trace_path: &Path, db_path: &Path) -> Result<()> {
        let bytes = std::fs::read(trace_path).map_err(|e| IndexError::TraceIo {
            path: trace_path.to_path_buf(),
            source: e,
        })?;
        let reader = TraceReader::from_bytes(&bytes)?;

        // Delete any pre-existing DB so re-runs produce identical files.
        if db_path.exists() {
            std::fs::remove_file(db_path).map_err(|e| IndexError::DbIo {
                path: db_path.to_path_buf(),
                source: e,
            })?;
        }

        let result = run_indexer(&reader, db_path);
        if result.is_err() {
            // Clean up the partial DB so the caller doesn't see a
            // half-indexed file masquerading as success.
            let _ = std::fs::remove_file(db_path);
        }
        result
    }
}

fn run_indexer(reader: &TraceReader, db_path: &Path) -> Result<()> {
    let conn = Connection::open(db_path)?;
    schema::drop_all(&conn)?;
    schema::create_tables(&conn)?;

    conn.execute_batch("BEGIN TRANSACTION")?;
    let res: Result<()> = (|| {
        // Frames first — second pass uses the per-event frame_id mapping
        // for denormalization.
        let frame_pass = frames::compute_frames(
            reader.events(),
            reader.source_files(),
            reader.strings(),
            reader.values(),
        )?;

        insert_source_files(&conn, reader)?;
        insert_values_with_appender(&conn, reader)?;
        insert_value_elements_with_appender(&conn, reader)?;
        insert_frames(&conn, &frame_pass)?;
        insert_events_and_subtables(&conn, reader, &frame_pass)?;
        insert_metadata(&conn, reader)?;
        Ok(())
    })();

    if res.is_err() {
        let _ = conn.execute_batch("ROLLBACK");
        return res;
    }
    conn.execute_batch("COMMIT")?;

    schema::create_indexes(&conn)?;
    Ok(())
}

fn insert_source_files(conn: &Connection, reader: &TraceReader) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO source_files (path, content_hash, content, line_count) VALUES (?, ?, ?, ?)",
    )?;
    for sf in reader.source_files() {
        let content = String::from_utf8_lossy(&sf.content).into_owned();
        let line_count = if content.is_empty() {
            0
        } else {
            content.matches('\n').count() as i32 + (if content.ends_with('\n') { 0 } else { 1 })
        };
        let hash_hex = values::hex_encode(&sf.blake3_hash);
        stmt.execute(params![sf.path, hash_hex, content, line_count])?;
    }
    Ok(())
}

fn insert_values_with_appender(conn: &Connection, reader: &TraceReader) -> Result<()> {
    let mut appender = conn.appender("values")?;
    values::insert_values(&mut appender, reader.values(), reader.strings())?;
    appender.flush()?;
    Ok(())
}

fn insert_value_elements_with_appender(conn: &Connection, reader: &TraceReader) -> Result<()> {
    let mut appender = conn.appender("value_elements")?;
    values::insert_value_elements(&mut appender, reader.values())?;
    appender.flush()?;
    Ok(())
}

fn insert_frames(conn: &Connection, pass: &FramePass) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO frames (frame_id, function_name, qualified_name, source_file, \
         parent_frame_id, entry_event_id, exit_event_id, exit_kind, depth, call_index, \
         duration_ns, argument_summary) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    for f in &pass.frames {
        let frame_id = f.frame_id as i64;
        let parent: Option<i64> = f.parent_frame_id.map(|x| x as i64);
        let entry_event_id = f.entry_event_id as i64;
        let exit_event_id: Option<i64> = f.exit_event_id.map(|x| x as i64);
        stmt.execute(params![
            frame_id,
            f.function_name,
            f.qualified_name,
            f.source_file,
            parent,
            entry_event_id,
            exit_event_id,
            f.exit_kind,
            f.depth,
            f.call_index,
            f.duration_ns,
            f.argument_summary,
        ])?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn insert_events_and_subtables(
    conn: &Connection,
    reader: &TraceReader,
    pass: &FramePass,
) -> Result<()> {
    // Quick map: frame_id → (qualified_name, source_file) for denormalization.
    let frame_lookup: std::collections::HashMap<u64, (&str, &str)> = pass
        .frames
        .iter()
        .map(|f| {
            (
                f.frame_id,
                (f.qualified_name.as_str(), f.source_file.as_str()),
            )
        })
        .collect();

    // Use Appender for the bulk events table.
    let mut events_appender = conn.appender("events")?;
    let mut event_locals_appender = conn.appender("event_locals")?;
    let mut event_args_stmt = conn.prepare(
        "INSERT INTO event_args (event_id, position, name, value_id) VALUES (?, ?, ?, ?)",
    )?;
    let mut note_kwargs_stmt =
        conn.prepare("INSERT INTO note_kwargs (event_id, name, value_id) VALUES (?, ?, ?)")?;
    let mut branch_stmt = conn.prepare(
        "INSERT INTO branches (event_id, frame_id, function_name, source_file, line, taken, \
         timestamp_ns) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;
    let mut exception_stmt = conn.prepare(
        "INSERT INTO exceptions (event_id, frame_id, function_name, source_file, line, \
         exception_type, exception_value_id, timestamp_ns) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    let mut note_stmt = conn.prepare(
        "INSERT INTO notes (event_id, frame_id, function_name, source_file, line, message, \
         timestamp_ns) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;
    let mut scope_boundary_stmt = conn.prepare(
        "INSERT INTO scope_boundaries (event_id, frame_id, boundary_type, reason, timestamp_ns) \
         VALUES (?, ?, ?, ?, ?)",
    )?;

    for (i, event) in reader.events().iter().enumerate() {
        let event_id = i as i64;
        let frame_id = pass.event_frame_id[i];
        let timestamp_ns = pass.event_timestamp_ns[i];
        let (function_name, frame_source) = match frame_lookup.get(&frame_id) {
            Some(&(qn, sf)) => (Some(qn), Some(sf)),
            None => (None, None),
        };
        let type_str = event_type_str(event);

        let mut source_file: Option<String> = None;
        let mut line: Option<i32> = None;
        let mut return_value_id: Option<i64> = None;
        let mut branch_taken: Option<bool> = None;
        let mut exception_type: Option<String> = None;
        let mut exception_value_id: Option<i64> = None;
        let mut note_message: Option<String> = None;
        let mut boundary_type: Option<String> = None;
        let mut boundary_reason: Option<String> = None;

        match event {
            Event::FunctionEntry(e) => {
                let path = source_path_for(reader, e.source_file_id)?;
                source_file = Some(path.to_string());
                line = Some(e.line as i32);
                // event_args + event_locals for the args.
                for (pos, arg) in e.args.iter().enumerate() {
                    let name = lookup_string(reader.strings(), arg.name)?;
                    event_args_stmt.execute(params![
                        event_id,
                        pos as i32,
                        name,
                        arg.value as i64,
                    ])?;
                    event_locals_appender.append_row(params![
                        event_id,
                        frame_id as i64,
                        name,
                        arg.value as i64,
                    ])?;
                }
            }
            Event::FunctionExit(e) => {
                return_value_id = Some(e.return_value as i64);
                source_file = frame_source.map(|s| s.to_string());
            }
            Event::FrameSnapshot(e) => {
                source_file = frame_source.map(|s| s.to_string());
                line = Some(e.line as i32);
                for local in &e.locals {
                    let name = lookup_string(reader.strings(), local.name)?;
                    event_locals_appender.append_row(params![
                        event_id,
                        frame_id as i64,
                        name,
                        local.value as i64,
                    ])?;
                }
            }
            Event::LineDelta(e) => {
                source_file = frame_source.map(|s| s.to_string());
                line = Some(e.line as i32);
                for change in &e.changes {
                    let name = lookup_string(reader.strings(), change.name)?;
                    event_locals_appender.append_row(params![
                        event_id,
                        frame_id as i64,
                        name,
                        change.value as i64,
                    ])?;
                }
            }
            Event::BranchResult(e) => {
                source_file = frame_source.map(|s| s.to_string());
                line = Some(e.line as i32);
                branch_taken = Some(e.taken);
                if let (Some(qn), Some(sf)) = (function_name, frame_source) {
                    branch_stmt.execute(params![
                        event_id,
                        frame_id as i64,
                        qn,
                        sf,
                        e.line as i32,
                        e.taken,
                        timestamp_ns,
                    ])?;
                }
            }
            Event::ExceptionRaised(e) => {
                source_file = frame_source.map(|s| s.to_string());
                line = Some(e.line as i32);
                let qual_type = lookup_string(reader.strings(), e.exception_type)?.to_string();
                exception_type = Some(qual_type.clone());
                exception_value_id = Some(e.exception_value as i64);
                if let (Some(qn), Some(sf)) = (function_name, frame_source) {
                    exception_stmt.execute(params![
                        event_id,
                        frame_id as i64,
                        qn,
                        sf,
                        e.line as i32,
                        qual_type,
                        e.exception_value as i64,
                        timestamp_ns,
                    ])?;
                }
            }
            Event::Note(e) => {
                source_file = frame_source.map(|s| s.to_string());
                line = Some(e.line as i32);
                let msg = lookup_string(reader.strings(), e.message)?.to_string();
                note_message = Some(msg.clone());
                if let (Some(qn), Some(sf)) = (function_name, frame_source) {
                    note_stmt.execute(params![
                        event_id,
                        frame_id as i64,
                        qn,
                        sf,
                        e.line as i32,
                        msg,
                        timestamp_ns,
                    ])?;
                }
                for kw in &e.kwargs {
                    let name = lookup_string(reader.strings(), kw.name)?;
                    note_kwargs_stmt.execute(params![event_id, name, kw.value as i64])?;
                }
            }
            Event::ScopeBoundary(e) => {
                let bt = boundary_type_str(e.boundary_type).to_string();
                let reason = lookup_string(reader.strings(), e.reason)?.to_string();
                boundary_type = Some(bt.clone());
                boundary_reason = Some(reason.clone());
                scope_boundary_stmt.execute(params![
                    event_id,
                    frame_id as i64,
                    bt,
                    reason,
                    timestamp_ns,
                ])?;
            }
            Event::FrameSwitch(_) => {
                // No standard subtype table; the events row carries enough.
                source_file = frame_source.map(|s| s.to_string());
            }
        }

        let row: [&dyn ToSql; 14] = [
            &event_id,
            &type_str,
            &(frame_id as i64),
            &timestamp_ns,
            &source_file,
            &line,
            &function_name,
            &return_value_id,
            &branch_taken,
            &exception_type,
            &exception_value_id,
            &note_message,
            &boundary_type,
            &boundary_reason,
        ];
        events_appender.append_row(row.as_slice())?;
    }

    events_appender.flush()?;
    event_locals_appender.flush()?;
    Ok(())
}

fn insert_metadata(conn: &Connection, reader: &TraceReader) -> Result<()> {
    let header = reader.header();
    let initial = metadata::parse_initial(&reader.metadata().payload)?;

    let recording_end_ns: Option<i64> = if header.recording_end_ns == 0 {
        None
    } else {
        Some(header.recording_end_ns as i64)
    };
    let trace_uuid = values::hex_encode(&header.trace_uuid);
    let include_patterns = if initial.recording.scope_config.include.is_empty() {
        None
    } else {
        Some(initial.recording.scope_config.include.join(","))
    };
    let exclude_patterns = Some(initial.recording.scope_config.exclude.join(","));
    let depth_limit: Option<i32> = initial.recording.scope_config.depth_limit.map(|x| x as i32);

    let final_summary: Option<FinalSummary> = match reader.final_summary() {
        Some(fs) => Some(metadata::parse_final(&fs.payload)?),
        None => None,
    };

    let (
        skip_blocks_observed,
        depth_clips_observed,
        total_events,
        total_blocks,
        trace_duration_ns,
        function_entry_count,
        line_event_count,
        branch_event_count,
        exception_event_count,
        note_event_count,
    ) = match &final_summary {
        Some(fs) => (
            Some(fs.r#final.scope_resolved.skip_blocks_observed as i32),
            Some(fs.r#final.scope_resolved.depth_clips_observed as i32),
            Some(fs.r#final.total_events),
            Some(fs.r#final.total_blocks as i32),
            Some(fs.r#final.trace_duration_ns),
            Some(fs.r#final.statistics.function_entry_events),
            Some(fs.r#final.statistics.line_events),
            Some(fs.r#final.statistics.branch_events),
            Some(fs.r#final.statistics.exception_events),
            Some(fs.r#final.statistics.note_events),
        ),
        None => (None, None, None, None, None, None, None, None, None, None),
    };

    conn.execute(
        "INSERT INTO trace_metadata (recorder_language, recorder_version, language_version, \
         platform, program, working_directory, trace_uuid, recording_start_ns, recording_end_ns, \
         include_patterns, exclude_patterns, depth_limit, skip_blocks_observed, \
         depth_clips_observed, total_events, total_blocks, trace_duration_ns, \
         function_entry_count, line_event_count, branch_event_count, exception_event_count, \
         note_event_count) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            initial.recorder.language,
            initial.recorder.recorder_version,
            initial.recorder.language_version,
            initial.recorder.platform,
            initial.recording.program,
            initial.recording.working_directory,
            trace_uuid,
            header.recording_start_ns as i64,
            recording_end_ns,
            include_patterns,
            exclude_patterns,
            depth_limit,
            skip_blocks_observed,
            depth_clips_observed,
            total_events,
            total_blocks,
            trace_duration_ns,
            function_entry_count,
            line_event_count,
            branch_event_count,
            exception_event_count,
            note_event_count,
        ],
    )?;

    if let Some(fs) = &final_summary {
        let mut rec_stmt =
            conn.prepare("INSERT INTO recorded_functions (qualified_name) VALUES (?)")?;
        for q in &fs.r#final.scope_resolved.recorded_functions {
            rec_stmt.execute(params![q])?;
        }
        let mut excl_stmt = conn.prepare(
            "INSERT INTO excluded_functions (qualified_name, matched_pattern) VALUES (?, ?)",
        )?;
        for ef in &fs.r#final.scope_resolved.excluded_functions {
            excl_stmt.execute(params![ef.name, ef.matched_pattern])?;
        }
    }
    Ok(())
}

fn event_type_str(event: &Event) -> &'static str {
    match event.tag() {
        EventTag::FunctionEntry => "function_entry",
        EventTag::FunctionExit => "function_exit",
        EventTag::FrameSnapshot => "frame_snapshot",
        EventTag::LineDelta => "line_delta",
        EventTag::BranchResult => "branch_result",
        EventTag::ExceptionRaised => "exception_raised",
        EventTag::Note => "note",
        EventTag::ScopeBoundary => "scope_boundary",
        EventTag::FrameSwitch => "frame_switch",
    }
}

fn boundary_type_str(b: BoundaryType) -> &'static str {
    match b {
        BoundaryType::EnteredSkip => "entered_skip",
        BoundaryType::ExitedSkip => "exited_skip",
        BoundaryType::EnteredExcluded => "entered_excluded",
        BoundaryType::ExitedExcluded => "exited_excluded",
        BoundaryType::EnteredDepthClipped => "entered_depth_clipped",
        BoundaryType::ExitedDepthClipped => "exited_depth_clipped",
    }
}

fn lookup_string(strings: &[String], id: u64) -> Result<&str> {
    strings
        .get(id as usize)
        .map(|s| s.as_str())
        .ok_or_else(|| IndexError::Internal(format!("string id {id} out of range")))
}

fn source_path_for(reader: &TraceReader, file_id: u64) -> Result<&str> {
    reader
        .source_files()
        .iter()
        .find(|f| f.file_id == file_id)
        .map(|f| f.path.as_str())
        .ok_or_else(|| IndexError::Internal(format!("source file id {file_id} not found")))
}
