// SPDX-License-Identifier: Apache-2.0

//! `why_did_value_change` — explain what caused a variable to change at a
//! specific point.

use std::collections::BTreeMap;

use duckdb::params;
use serde::{Deserialize, Serialize};

use crate::conn::DbConnection;
use crate::error::ToolError;
use crate::identifiers::extract_identifiers;
use crate::source::fetch_line;
use crate::value::{ValueSummary, fetch_value_summaries, fetch_value_summary};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WhyDidValueChangeInput {
    pub name: String,
    pub frame_id: i64,
    pub around_event_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviousValue {
    pub type_tag: String,
    pub display: String,
    pub captured_at_event: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChangeEvent {
    pub event_id: i64,
    pub line: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_value: Option<PreviousValue>,
    pub new_value: ValueSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PrecedingBranch {
    pub event_id: i64,
    pub line: i32,
    pub taken: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WhyDidValueChangeOutput {
    pub name: String,
    pub frame_id: i64,
    pub change_event: ChangeEvent,
    pub context_at_change: BTreeMap<String, ValueSummary>,
    pub preceding_branches: Vec<PrecedingBranch>,
    pub narrative_hint: String,
}

pub fn run(
    db: &DbConnection,
    input: WhyDidValueChangeInput,
) -> Result<WhyDidValueChangeOutput, ToolError> {
    let change = find_change(db, &input)?.ok_or_else(|| {
        ToolError::new(
            "no_change_found",
            format!(
                "No LINE_DELTA event found that captured `{}` in frame {} at or before event {}.",
                input.name, input.frame_id, input.around_event_id
            ),
        )
        .with_suggestion("Use trace_variable to confirm the variable is captured in this frame.")
    })?;

    let new_value = fetch_value_summary(db, change.value_id)?.unwrap_or(ValueSummary {
        value_id: change.value_id,
        type_tag: "unknown".into(),
        display: "<missing>".into(),
        type_name: None,
        container_length: None,
    });

    let previous = find_previous(db, &input, change.event_id)?;
    let previous_value = match &previous {
        Some(p) => fetch_value_summary(db, p.value_id)?.map(|s| PreviousValue {
            type_tag: s.type_tag,
            display: s.display,
            captured_at_event: p.event_id,
        }),
        None => None,
    };

    let source_line = match (change.source_file.as_ref(), change.line) {
        (Some(f), Some(l)) => fetch_line(db, f, l)?,
        _ => None,
    };

    let context_idents = match &source_line {
        Some(t) => {
            let mut ids = extract_identifiers(t);
            ids.retain(|s| s != &input.name);
            ids.sort();
            ids.dedup();
            ids
        }
        None => Vec::new(),
    };
    let context = fetch_locals_at(db, input.frame_id, change.event_id, &context_idents)?;

    let prev_event = previous.as_ref().map(|p| p.event_id).unwrap_or(0);
    let preceding_branches =
        fetch_preceding_branches(db, input.frame_id, prev_event, change.event_id)?;

    let narrative_hint = compose_narrative(
        &input.name,
        &change,
        &new_value,
        previous_value.as_ref(),
        &context,
        &preceding_branches,
    );

    Ok(WhyDidValueChangeOutput {
        name: input.name.clone(),
        frame_id: input.frame_id,
        change_event: ChangeEvent {
            event_id: change.event_id,
            line: change.line,
            source_line,
            previous_value,
            new_value,
        },
        context_at_change: context,
        preceding_branches,
        narrative_hint,
    })
}

struct ChangeRow {
    event_id: i64,
    value_id: i64,
    line: Option<i32>,
    source_file: Option<String>,
}

fn find_change(
    db: &DbConnection,
    input: &WhyDidValueChangeInput,
) -> Result<Option<ChangeRow>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT el.event_id, el.value_id, e.line, e.source_file \
             FROM event_locals el JOIN events e ON el.event_id = e.event_id \
             WHERE el.frame_id = ? AND el.name = ? AND el.event_id <= ? \
             AND e.type IN ('line_delta','frame_snapshot','function_entry') \
             ORDER BY el.event_id DESC LIMIT 1",
        )
        .map_err(map_db)?;
    let mut rows = stmt
        .query(params![input.frame_id, input.name, input.around_event_id])
        .map_err(map_db)?;
    if let Some(r) = rows.next().map_err(map_db)? {
        Ok(Some(ChangeRow {
            event_id: r.get(0).map_err(map_db)?,
            value_id: r.get(1).map_err(map_db)?,
            line: r.get(2).map_err(map_db)?,
            source_file: r.get(3).map_err(map_db)?,
        }))
    } else {
        Ok(None)
    }
}

struct PreviousRow {
    event_id: i64,
    value_id: i64,
}

fn find_previous(
    db: &DbConnection,
    input: &WhyDidValueChangeInput,
    change_event_id: i64,
) -> Result<Option<PreviousRow>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT event_id, value_id FROM event_locals \
             WHERE frame_id = ? AND name = ? AND event_id < ? \
             ORDER BY event_id DESC LIMIT 1",
        )
        .map_err(map_db)?;
    let mut rows = stmt
        .query(params![input.frame_id, input.name, change_event_id])
        .map_err(map_db)?;
    if let Some(r) = rows.next().map_err(map_db)? {
        Ok(Some(PreviousRow {
            event_id: r.get(0).map_err(map_db)?,
            value_id: r.get(1).map_err(map_db)?,
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

fn fetch_preceding_branches(
    db: &DbConnection,
    frame_id: i64,
    after_event: i64,
    before_event: i64,
) -> Result<Vec<PrecedingBranch>, ToolError> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT b.event_id, b.line, b.taken, b.source_file FROM branches b \
             WHERE b.frame_id = ? AND b.event_id > ? AND b.event_id < ? \
             ORDER BY b.event_id",
        )
        .map_err(map_db)?;
    let rows = stmt
        .query_map(params![frame_id, after_event, before_event], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i32>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(map_db)?;
    let raw: Vec<(i64, i32, bool, String)> =
        rows.collect::<duckdb::Result<Vec<_>>>().map_err(map_db)?;
    drop(stmt);
    drop(conn);

    let mut out = Vec::with_capacity(raw.len());
    for (event_id, line, taken, source_file) in raw {
        let source_line = fetch_line(db, &source_file, line)?;
        out.push(PrecedingBranch {
            event_id,
            line,
            taken,
            source_line,
        });
    }
    Ok(out)
}

fn compose_narrative(
    name: &str,
    change: &ChangeRow,
    new_value: &ValueSummary,
    previous: Option<&PreviousValue>,
    context: &BTreeMap<String, ValueSummary>,
    branches: &[PrecedingBranch],
) -> String {
    let line_clause = match change.line {
        Some(l) => format!(" at line {l}"),
        None => String::new(),
    };
    let prev_clause = match previous {
        Some(p) => format!("from {} ", p.display),
        None => "from <unset> ".to_string(),
    };
    let mut s = format!(
        "`{name}` changed {prev_clause}to {} (event {}){line_clause}.",
        new_value.display, change.event_id
    );
    if !branches.is_empty() {
        let parts: Vec<String> = branches
            .iter()
            .map(|b| format!("event {} (line {}, taken={})", b.event_id, b.line, b.taken))
            .collect();
        s.push_str(&format!(
            " Preceding branches in this frame: {}.",
            parts.join(", ")
        ));
    }
    if !context.is_empty() {
        let parts: Vec<String> = context
            .iter()
            .map(|(n, v)| format!("{n}={}", v.display))
            .collect();
        s.push_str(&format!(" Locals at the change: {}.", parts.join(", ")));
    }
    s
}

fn map_db(e: duckdb::Error) -> ToolError {
    ToolError::new("database_error", e.to_string())
}
