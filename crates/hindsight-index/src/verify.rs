// SPDX-License-Identifier: Apache-2.0

//! Post-hoc verification of `summary_observed` aliases.
//!
//! The recorder emits aliases with confidence ``summary_observed`` when
//! the capture path could not actively re-verify the contents (the
//! mutation-tracking layer didn't see anything mutate, but the
//! summary fingerprint matched — which is *probably* fine, but might
//! hide a same-fingerprint mutation).
//!
//! ``hindsight verify`` is the user's escape hatch for "I want to know
//! for sure." It walks the indexed values, follows each
//! ``summary_observed`` alias to the materialized container, computes
//! the expected content hash of the alias's effective contents, and
//! compares against the source value's hash. If they match, the
//! alias's confidence is upgraded to ``dirty_reconciled``. If they
//! differ, the entry is flagged ``uncertain_external`` and the
//! mismatch is recorded for the user to inspect.
//!
//! This is purely additive over an already-indexed database — the
//! `.hindsight` source isn't needed.

use duckdb::{Connection, params};

use crate::error::{IndexError, Result};

/// Outcome counts from one verify pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct VerifyReport {
    /// Total `summary_observed` rows examined.
    pub examined: u64,
    /// Aliases whose recomputed content matched the source — confidence
    /// upgraded to `dirty_reconciled`.
    pub upgraded: u64,
    /// Aliases whose recomputed content did NOT match — flagged
    /// `uncertain_external`. These are the cases the verify pass found
    /// the recorder's summary_observed inference would have hidden.
    pub mismatched: u64,
    /// Rows skipped because the alias source couldn't be hashed (e.g.,
    /// a chain pointing at a non-container scalar — those don't need
    /// element-level verification).
    pub skipped: u64,
}

/// Run a verify pass on the indexed database at `db_path`. Returns the
/// outcome counts. Idempotent: rows already upgraded by a previous run
/// stay at their current confidence and aren't re-counted.
///
/// The pass adds two new columns to `values` (idempotent if they
/// already exist): `verify_status` for the per-row outcome, and
/// `verify_run_at` for telemetry (the SQL timestamp at which this
/// alias was last verified). These are queryable by tools that want to
/// surface "this alias was content-checked at <time>" to the user.
pub fn verify(db_path: &std::path::Path) -> Result<VerifyReport> {
    let conn = Connection::open(db_path)?;
    ensure_verify_columns(&conn)?;

    // Pull every summary_observed alias's (id, source_id) pair, plus a
    // computable expected-content fingerprint we can compare against the
    // source's elements. Verification at this level is a structural
    // comparison: if alias A claims to have the same content as source S,
    // then A's effective element_list (already materialized in
    // value_elements at index time) should equal S's effective
    // element_list. Inequality means a mutation slipped past the
    // recorder's summary fingerprint between captures.
    let mut stmt = conn.prepare(
        "SELECT v.value_id, v.aliased_value_id \
         FROM values v \
         WHERE v.confidence = 'summary_observed' \
           AND v.aliased_value_id IS NOT NULL \
           AND (v.verify_status IS NULL OR v.verify_status = 'unverified')",
    )?;

    let pairs: Vec<(i64, i64)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<duckdb::Result<Vec<_>>>()?;
    drop(stmt);

    let mut report = VerifyReport::default();
    for (alias_id, source_id) in pairs {
        report.examined += 1;
        let outcome = compare_alias_to_source(&conn, alias_id, source_id)?;
        match outcome {
            CompareOutcome::Match => {
                conn.execute(
                    "UPDATE values SET confidence = 'dirty_reconciled', \
                     verify_status = 'verified', verify_run_at = CURRENT_TIMESTAMP \
                     WHERE value_id = ?",
                    params![alias_id],
                )?;
                report.upgraded += 1;
            }
            CompareOutcome::Mismatch => {
                conn.execute(
                    "UPDATE values SET confidence = 'uncertain_external', \
                     verify_status = 'mismatch', verify_run_at = CURRENT_TIMESTAMP \
                     WHERE value_id = ?",
                    params![alias_id],
                )?;
                report.mismatched += 1;
            }
            CompareOutcome::Skip => {
                conn.execute(
                    "UPDATE values SET verify_status = 'skipped', \
                     verify_run_at = CURRENT_TIMESTAMP WHERE value_id = ?",
                    params![alias_id],
                )?;
                report.skipped += 1;
            }
        }
    }
    Ok(report)
}

enum CompareOutcome {
    Match,
    Mismatch,
    Skip,
}

fn compare_alias_to_source(
    conn: &Connection,
    alias_id: i64,
    source_id: i64,
) -> Result<CompareOutcome> {
    // Fast path: if the source isn't a container, there's nothing
    // element-level to compare. Trust the alias.
    let source_is_container: bool = conn
        .query_row(
            "SELECT type_tag IN ('list', 'set', 'dict') FROM values WHERE value_id = ?",
            params![source_id],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if !source_is_container {
        return Ok(CompareOutcome::Skip);
    }

    // Pull both element lists, in position order. For dicts we include
    // the key column so a (key,value) reordering is detected.
    let alias_elems = fetch_element_signature(conn, alias_id)?;
    let source_elems = fetch_element_signature(conn, source_id)?;

    if alias_elems == source_elems {
        Ok(CompareOutcome::Match)
    } else {
        Ok(CompareOutcome::Mismatch)
    }
}

fn fetch_element_signature(conn: &Connection, value_id: i64) -> Result<Vec<(i64, i64, i64)>> {
    // (position, key_id_or_null_marker, element_id). Returned in position
    // order. Non-dict containers have key=-1.
    let mut stmt = conn.prepare(
        "SELECT position, COALESCE(key_value_id, -1), element_value_id \
         FROM value_elements WHERE container_value_id = ? ORDER BY position",
    )?;
    Ok(stmt
        .query_map(params![value_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<duckdb::Result<Vec<_>>>()?)
}

/// Idempotently add the verify-status columns to `values`. If they
/// already exist, do nothing.
fn ensure_verify_columns(conn: &Connection) -> Result<()> {
    let cols: Vec<String> = conn
        .prepare("SELECT column_name FROM information_schema.columns WHERE table_name = 'values'")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<duckdb::Result<Vec<_>>>()?;
    if !cols.iter().any(|c| c == "verify_status") {
        conn.execute_batch(
            "ALTER TABLE values ADD COLUMN verify_status VARCHAR DEFAULT 'unverified';
             ALTER TABLE values ADD COLUMN verify_run_at TIMESTAMP;",
        )?;
    } else {
        // Older tables may have verify_status but no verify_run_at; add
        // it if missing.
        if !cols.iter().any(|c| c == "verify_run_at") {
            conn.execute_batch("ALTER TABLE values ADD COLUMN verify_run_at TIMESTAMP;")?;
        }
    }
    Ok(())
}

/// Convenience wrapper used by the CLI — opens the DB, runs verify,
/// returns the report formatted as text. Pure plumbing, no logic.
pub fn verify_to_string(db_path: &std::path::Path) -> Result<String> {
    let report = verify(db_path)?;
    Ok(format!(
        "verified: examined={} upgraded={} mismatched={} skipped={}",
        report.examined, report.upgraded, report.mismatched, report.skipped
    ))
}

// Suppress unused-import warning for IndexError in some build configs.
#[allow(dead_code)]
fn _force_use_index_error(e: IndexError) -> IndexError {
    e
}
