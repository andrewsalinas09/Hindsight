// SPDX-License-Identifier: Apache-2.0

//! Multi-trace registry that backs the MCP server's `trace_id`-keyed tools.
//!
//! The registry holds a directory of `.hindsight` traces (or, for legacy
//! single-file mode, an explicit list of files). For each trace it lazily:
//!
//! - reads the wire-format header + initial metadata + final summary to
//!   produce a [`TraceMetadata`] without indexing.
//! - runs the indexer (if no sibling `.duckdb` exists, or it's older than
//!   the `.hindsight`) on the first investigation-tool call.
//! - opens and caches a DuckDB connection.
//!
//! All access is serialized through a single `Mutex<RegistryInner>` —
//! adequate for interactive debugging where only one tool call is in flight
//! at a time. If contention becomes a problem the obvious next step is
//! per-entry locks plus an upper-level reader for the discovery map.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use hindsight_format::TraceReader;
use hindsight_index::Indexer;
use serde::Deserialize;

use crate::conn::DbConnection;
use crate::error::{ServerError, ToolError};

const HINDSIGHT_EXT: &str = "hindsight";
const DUCKDB_EXT: &str = "duckdb";

/// Where the registry's traces come from.
#[derive(Debug, Clone)]
pub enum RegistrySource {
    /// Watch a directory; `scan` enumerates `*.hindsight` inside it.
    Directory(PathBuf),
    /// A fixed list of `.hindsight` files (or a single `.duckdb` for
    /// legacy single-file mode).
    Files(Vec<PathBuf>),
}

pub struct TraceRegistry {
    source: RegistrySource,
    inner: Mutex<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    entries: HashMap<String, TraceEntry>,
}

struct TraceEntry {
    trace_id: String,
    /// Source `.hindsight` file. `None` for entries pointed at a pre-built
    /// `.duckdb` (legacy single-file `hindsight serve foo.duckdb`).
    hindsight_path: Option<PathBuf>,
    /// Indexed `.duckdb` path. Always set; the file may not exist yet if
    /// the entry hasn't been touched.
    duckdb_path: PathBuf,
    /// Lazily populated by [`TraceRegistry::ensure_metadata`].
    metadata: Option<TraceMetadata>,
    /// Lazily populated on first investigation-tool call.
    connection: Option<DbConnection>,
}

/// Metadata read from a `.hindsight` file's header + initial metadata
/// block + final summary (or, for entries pointed at a pre-indexed
/// `.duckdb`, from the `trace_metadata` table).
#[derive(Debug, Clone)]
pub struct TraceMetadata {
    pub trace_id: String,
    pub program: Option<String>,
    pub recorded_at_ns: Option<u64>,
    pub trace_uuid: Option<String>,
    pub recorder_language: Option<String>,
    pub recorder_version: Option<String>,
    pub language_version: Option<String>,
    pub platform: Option<String>,
    pub working_directory: Option<String>,
    pub event_count: Option<i64>,
    pub duration_ns: Option<i64>,
    pub function_entry_count: Option<i64>,
    pub line_event_count: Option<i64>,
    pub branch_event_count: Option<i64>,
    pub exception_event_count: Option<i64>,
    pub note_event_count: Option<i64>,
    pub recorded_functions: Vec<String>,
    pub excluded_functions: Vec<String>,
    pub indexed: bool,
    pub size_bytes: u64,
}

impl TraceRegistry {
    pub fn from_directory(dir: PathBuf) -> Result<Self, ServerError> {
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
        }
        Ok(Self {
            source: RegistrySource::Directory(dir),
            inner: Mutex::new(RegistryInner::default()),
        })
    }

    pub fn from_files(files: Vec<PathBuf>) -> Result<Self, ServerError> {
        Ok(Self {
            source: RegistrySource::Files(files),
            inner: Mutex::new(RegistryInner::default()),
        })
    }

    pub fn source(&self) -> &RegistrySource {
        &self.source
    }

    /// Re-scan source for trace files, refreshing the entries map. Returns
    /// the trace_ids in sorted order.
    pub fn scan(&self) -> Result<Vec<String>, ToolError> {
        let discovered = self.discover().map_err(map_io)?;
        let mut inner = self.inner.lock().expect("registry mutex poisoned");

        let active_ids: HashSet<String> = discovered.iter().map(|p| trace_id_for_path(p)).collect();
        // Drop entries whose source file has disappeared.
        inner.entries.retain(|id, _| active_ids.contains(id));

        for path in &discovered {
            let trace_id = trace_id_for_path(path);
            inner.entries.entry(trace_id.clone()).or_insert_with(|| {
                let (hindsight_path, duckdb_path) = paths_for(path);
                TraceEntry {
                    trace_id: trace_id.clone(),
                    hindsight_path,
                    duckdb_path,
                    metadata: None,
                    connection: None,
                }
            });
        }
        let mut ids: Vec<String> = inner.entries.keys().cloned().collect();
        ids.sort();
        Ok(ids)
    }

    /// List metadata for every trace. Reads metadata for any entry that
    /// hasn't been touched yet.
    pub fn list(&self) -> Result<Vec<TraceMetadata>, ToolError> {
        let ids = self.scan()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let meta = self.metadata(&id)?;
            out.push(meta);
        }
        Ok(out)
    }

    /// Metadata for one trace. Cached after first read.
    pub fn metadata(&self, trace_id: &str) -> Result<TraceMetadata, ToolError> {
        // If entry is missing, scan once and try again.
        {
            let inner = self.inner.lock().expect("registry mutex poisoned");
            if let Some(entry) = inner.entries.get(trace_id)
                && let Some(m) = &entry.metadata
            {
                return Ok(m.clone());
            }
        }
        // Either entry exists but metadata uncached, or entry doesn't
        // exist yet. In either case, scan + populate.
        self.scan()?;

        let mut inner = self.inner.lock().expect("registry mutex poisoned");
        let entry = inner.entries.get_mut(trace_id).ok_or_else(|| {
            ToolError::new(
                "trace_not_found",
                format!("No trace with trace_id {trace_id:?}"),
            )
            .with_suggestion("Use list_traces to see all available trace_ids.")
        })?;
        let meta = read_metadata(entry).map_err(|e| {
            ToolError::new("metadata_read_failed", e.to_string())
                .with_suggestion("The trace file may be corrupt or truncated.")
        })?;
        entry.metadata = Some(meta.clone());
        Ok(meta)
    }

    /// Ensure the trace is indexed (running the indexer if needed) and
    /// return a connection to its DuckDB. Cheap on subsequent calls — the
    /// connection is cached.
    pub fn get_or_open(&self, trace_id: &str) -> Result<DbConnection, ToolError> {
        // Fast path: connection already cached.
        {
            let inner = self.inner.lock().expect("registry mutex poisoned");
            if let Some(entry) = inner.entries.get(trace_id)
                && let Some(conn) = &entry.connection
            {
                return Ok(conn.clone());
            }
        }
        // Slow path: scan, ensure indexed, open connection.
        self.scan()?;

        let mut inner = self.inner.lock().expect("registry mutex poisoned");
        let entry = inner.entries.get_mut(trace_id).ok_or_else(|| {
            ToolError::new(
                "trace_not_found",
                format!("No trace with trace_id {trace_id:?}"),
            )
            .with_suggestion("Use list_traces to see all available trace_ids.")
        })?;
        if let Some(conn) = &entry.connection {
            return Ok(conn.clone());
        }
        ensure_indexed(entry).map_err(|e| {
            ToolError::new("index_failed", e.to_string()).with_suggestion(
                "Check that the .hindsight file is well-formed and that the directory is writable.",
            )
        })?;
        let conn = DbConnection::open(entry.duckdb_path.clone()).map_err(|e| {
            ToolError::new("open_failed", e.to_string())
                .with_suggestion("Re-run hindsight index <trace> to rebuild the indexed database.")
        })?;
        entry.connection = Some(conn.clone());
        Ok(conn)
    }

    /// Convenience: when there's exactly one trace registered, return its
    /// trace_id. Used by tools whose `trace_id` parameter is optional in
    /// single-trace mode.
    pub fn sole_trace_id(&self) -> Option<String> {
        // Don't scan — we want the cached count, not a fresh enumeration.
        let inner = self.inner.lock().expect("registry mutex poisoned");
        if inner.entries.len() == 1 {
            inner.entries.keys().next().cloned()
        } else {
            None
        }
    }

    /// Walk the source for trace files.
    fn discover(&self) -> std::io::Result<Vec<PathBuf>> {
        match &self.source {
            RegistrySource::Directory(dir) => {
                let mut out = Vec::new();
                if !dir.exists() {
                    return Ok(out);
                }
                for entry in std::fs::read_dir(dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_file()
                        && path.extension().and_then(|s| s.to_str()) == Some(HINDSIGHT_EXT)
                    {
                        out.push(path);
                    }
                }
                out.sort();
                Ok(out)
            }
            RegistrySource::Files(files) => Ok(files.clone()),
        }
    }
}

/// trace_id is the filename without its `.hindsight` or `.duckdb`
/// extension. For paths like `trace_<timestamp>.hindsight` this gives
/// `trace_<timestamp>`; for legacy `basic.hindsight`, just `basic`.
pub fn trace_id_for_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Given a path that may be either a `.hindsight` or `.duckdb`, return the
/// `(hindsight_path, duckdb_path)` pair the registry tracks for that
/// trace. The `.duckdb` is always the sibling with the same stem;
/// `.hindsight` may be `None` when the registry was given a pre-built
/// `.duckdb` directly.
fn paths_for(path: &Path) -> (Option<PathBuf>, PathBuf) {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    if ext == DUCKDB_EXT {
        let hindsight = path.with_extension(HINDSIGHT_EXT);
        let hp = if hindsight.exists() {
            Some(hindsight)
        } else {
            None
        };
        (hp, path.to_path_buf())
    } else {
        // Treat as .hindsight (or unknown extension; we still track it).
        let duckdb = path.with_extension(DUCKDB_EXT);
        (Some(path.to_path_buf()), duckdb)
    }
}

fn ensure_indexed(entry: &mut TraceEntry) -> Result<(), ServerError> {
    let needs_index = match &entry.hindsight_path {
        Some(hp) => !is_indexed_current(hp, &entry.duckdb_path),
        // No source .hindsight; the .duckdb is what we have.
        None => !entry.duckdb_path.exists(),
    };
    if !needs_index {
        return Ok(());
    }
    let hp = entry.hindsight_path.as_ref().ok_or_else(|| {
        ServerError::Service(format!(
            "trace {} has no .hindsight source and no .duckdb on disk",
            entry.trace_id
        ))
    })?;
    Indexer::index(hp, &entry.duckdb_path).map_err(|e| ServerError::Service(e.to_string()))?;
    Ok(())
}

fn is_indexed_current(hindsight: &Path, duckdb: &Path) -> bool {
    if !duckdb.exists() {
        return false;
    }
    let h_mtime = mtime(hindsight).unwrap_or(SystemTime::UNIX_EPOCH);
    let d_mtime = mtime(duckdb).unwrap_or(SystemTime::UNIX_EPOCH);
    d_mtime >= h_mtime
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn read_metadata(entry: &TraceEntry) -> Result<TraceMetadata, anyhow::Error> {
    let size_bytes = entry
        .hindsight_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .or_else(|| std::fs::metadata(&entry.duckdb_path).ok().map(|m| m.len()))
        .unwrap_or(0);
    let indexed_current = match &entry.hindsight_path {
        Some(hp) => is_indexed_current(hp, &entry.duckdb_path),
        None => entry.duckdb_path.exists(),
    };

    if let Some(hp) = &entry.hindsight_path {
        let bytes = std::fs::read(hp)?;
        let reader = TraceReader::from_bytes(&bytes)?;
        return Ok(metadata_from_reader(
            &entry.trace_id,
            &reader,
            indexed_current,
            size_bytes,
        ));
    }

    // Pre-indexed .duckdb path only — read trace_metadata table.
    let conn = DbConnection::open(entry.duckdb_path.clone())?;
    metadata_from_db(&entry.trace_id, &conn, indexed_current, size_bytes)
}

fn metadata_from_reader(
    trace_id: &str,
    reader: &TraceReader,
    indexed: bool,
    size_bytes: u64,
) -> TraceMetadata {
    let header = reader.header();
    let initial = parse_initial_metadata(&reader.metadata().payload);
    let final_summary = reader
        .final_summary()
        .map(|fs| parse_final_summary(&fs.payload));

    let trace_uuid = Some(hex_lower(&header.trace_uuid));
    let (
        program,
        recorder_language,
        recorder_version,
        language_version,
        platform,
        working_directory,
    ) = match initial {
        Ok(m) => (
            Some(m.recording.program),
            Some(m.recorder.language),
            Some(m.recorder.recorder_version),
            Some(m.recorder.language_version),
            Some(m.recorder.platform),
            m.recording.working_directory,
        ),
        Err(_) => (None, None, None, None, None, None),
    };

    let (
        event_count,
        duration_ns,
        function_entry_count,
        line_event_count,
        branch_event_count,
        exception_event_count,
        note_event_count,
        recorded_functions,
        excluded_functions,
    ) = match final_summary {
        Some(Ok(fs)) => (
            Some(fs.r#final.total_events),
            Some(fs.r#final.trace_duration_ns),
            Some(fs.r#final.statistics.function_entry_events),
            Some(fs.r#final.statistics.line_events),
            Some(fs.r#final.statistics.branch_events),
            Some(fs.r#final.statistics.exception_events),
            Some(fs.r#final.statistics.note_events),
            fs.r#final.scope_resolved.recorded_functions,
            fs.r#final
                .scope_resolved
                .excluded_functions
                .into_iter()
                .map(|e| e.name)
                .collect(),
        ),
        _ => (
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
        ),
    };

    TraceMetadata {
        trace_id: trace_id.to_string(),
        program,
        recorded_at_ns: Some(header.recording_start_ns),
        trace_uuid,
        recorder_language,
        recorder_version,
        language_version,
        platform,
        working_directory,
        event_count,
        duration_ns,
        function_entry_count,
        line_event_count,
        branch_event_count,
        exception_event_count,
        note_event_count,
        recorded_functions,
        excluded_functions,
        indexed,
        size_bytes,
    }
}

/// Subset of `trace_metadata` we read out of an indexed `.duckdb`. Named
/// struct rather than a 14-tuple so it doesn't trip clippy::type_complexity.
#[derive(Default)]
struct DbMetadataRow {
    program: Option<String>,
    recorder_language: Option<String>,
    recorder_version: Option<String>,
    language_version: Option<String>,
    platform: Option<String>,
    working_directory: Option<String>,
    recorded_at_ns: Option<i64>,
    duration_ns: Option<i64>,
    trace_uuid: Option<String>,
    event_count: Option<i64>,
    function_entry_count: Option<i64>,
    line_event_count: Option<i64>,
    branch_event_count: Option<i64>,
    exception_event_count: Option<i64>,
}

fn metadata_from_db(
    trace_id: &str,
    db: &DbConnection,
    indexed: bool,
    size_bytes: u64,
) -> Result<TraceMetadata, anyhow::Error> {
    let conn = db.lock();
    let row: DbMetadataRow = conn
        .query_row(
            "SELECT program, recorder_language, recorder_version, language_version, platform, \
             working_directory, recording_start_ns, trace_duration_ns, trace_uuid, total_events, \
             function_entry_count, line_event_count, branch_event_count, exception_event_count \
             FROM trace_metadata LIMIT 1",
            [],
            |r| {
                Ok(DbMetadataRow {
                    program: r.get(0)?,
                    recorder_language: r.get(1)?,
                    recorder_version: r.get(2)?,
                    language_version: r.get(3)?,
                    platform: r.get(4)?,
                    working_directory: r.get(5)?,
                    recorded_at_ns: r.get(6)?,
                    duration_ns: r.get(7)?,
                    trace_uuid: r.get(8)?,
                    event_count: r.get(9)?,
                    function_entry_count: r.get(10)?,
                    line_event_count: r.get(11)?,
                    branch_event_count: r.get(12)?,
                    exception_event_count: r.get(13)?,
                })
            },
        )
        .unwrap_or_default();
    let recorded_functions: Vec<String> = {
        let mut stmt = conn.prepare("SELECT qualified_name FROM recorded_functions ORDER BY 1")?;
        let mut out = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            out.push(row.get(0)?);
        }
        out
    };
    let excluded_functions: Vec<String> = {
        let mut stmt = conn.prepare("SELECT qualified_name FROM excluded_functions ORDER BY 1")?;
        let mut out = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            out.push(row.get(0)?);
        }
        out
    };
    let note_event_count: Option<i64> = conn
        .query_row(
            "SELECT note_event_count FROM trace_metadata LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();

    Ok(TraceMetadata {
        trace_id: trace_id.to_string(),
        program: row.program,
        recorded_at_ns: row.recorded_at_ns.map(|x| x as u64),
        trace_uuid: row.trace_uuid,
        recorder_language: row.recorder_language,
        recorder_version: row.recorder_version,
        language_version: row.language_version,
        platform: row.platform,
        working_directory: row.working_directory,
        event_count: row.event_count,
        duration_ns: row.duration_ns,
        function_entry_count: row.function_entry_count,
        line_event_count: row.line_event_count,
        branch_event_count: row.branch_event_count,
        exception_event_count: row.exception_event_count,
        note_event_count,
        recorded_functions,
        excluded_functions,
        indexed,
        size_bytes,
    })
}

// ---------------------------------------------------------------------------
// Local copies of the indexer's TOML structs. We deliberately re-declare them
// here so the registry doesn't depend on hindsight-index's internals.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct InitialMetadata {
    #[serde(default)]
    recorder: Recorder,
    #[serde(default)]
    recording: Recording,
}

#[derive(Debug, Default, Deserialize)]
struct Recorder {
    #[serde(default)]
    language: String,
    #[serde(default)]
    language_version: String,
    #[serde(default)]
    recorder_version: String,
    #[serde(default)]
    platform: String,
}

#[derive(Debug, Default, Deserialize)]
struct Recording {
    #[serde(default)]
    program: String,
    #[serde(default)]
    working_directory: Option<String>,
}

fn parse_initial_metadata(payload: &str) -> Result<InitialMetadata, toml::de::Error> {
    toml::from_str(payload)
}

#[derive(Debug, Default, Deserialize)]
struct FinalSummary {
    #[serde(default)]
    r#final: FinalBody,
}

#[derive(Debug, Default, Deserialize)]
struct FinalBody {
    #[serde(default)]
    total_events: i64,
    #[serde(default)]
    trace_duration_ns: i64,
    #[serde(default)]
    statistics: Statistics,
    #[serde(default)]
    scope_resolved: ScopeResolved,
}

#[derive(Debug, Default, Deserialize)]
struct Statistics {
    #[serde(default)]
    function_entry_events: i64,
    #[serde(default)]
    line_events: i64,
    #[serde(default)]
    branch_events: i64,
    #[serde(default)]
    exception_events: i64,
    #[serde(default)]
    note_events: i64,
}

#[derive(Debug, Default, Deserialize)]
struct ScopeResolved {
    #[serde(default)]
    recorded_functions: Vec<String>,
    #[serde(default)]
    excluded_functions: Vec<ExcludedFunc>,
}

#[derive(Debug, Deserialize)]
struct ExcludedFunc {
    name: String,
    #[allow(dead_code)]
    matched_pattern: String,
}

fn parse_final_summary(payload: &str) -> Result<FinalSummary, toml::de::Error> {
    toml::from_str(payload)
}

fn hex_lower(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len() * 2);
    for byte in b {
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{byte:02x}"));
    }
    out
}

fn map_io(e: std::io::Error) -> ToolError {
    ToolError::new("io_error", e.to_string())
}
