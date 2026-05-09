// SPDX-License-Identifier: Apache-2.0

//! Command-line entry point that orchestrates the Hindsight components.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hindsight_index::Indexer;

#[derive(Parser, Debug)]
#[command(
    name = "hindsight",
    version,
    about = "Hindsight: an AI-native debugger."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print the hindsight CLI version.
    Version,
    /// Index a `.hindsight` trace file into a DuckDB database.
    Index {
        /// Path to the `.hindsight` trace file to index.
        trace: PathBuf,
        /// Output database path. Defaults to `<input>.duckdb`.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Serve the MCP server. With no path, watches `~/.hindsight/traces/`.
    /// With a directory, watches that directory. With a file, serves
    /// that single trace (legacy single-file mode).
    Serve {
        /// Optional path. Directory or single `.hindsight`/`.duckdb`
        /// file. Defaults to `~/.hindsight/traces/` (created if missing).
        path: Option<PathBuf>,
        /// Force directory mode even if `path` is omitted; equivalent
        /// to passing the default directory explicitly.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Index a `.hindsight` trace if needed, then serve the MCP server
    /// against just that one file. One-command path for users who just
    /// want to query a single recording.
    Debug {
        /// Path to the `.hindsight` trace file.
        trace: PathBuf,
        /// Force re-indexing even if a `.duckdb` already exists alongside the trace.
        #[arg(long)]
        reindex: bool,
    },
    /// Walk the indexed database and content-verify every
    /// `summary_observed` alias. Upgrades matched aliases to
    /// `dirty_reconciled` and flags mismatches as
    /// `uncertain_external`. Use this after a debugging session if you
    /// want to be sure the recorder's summary-fingerprint optimization
    /// didn't hide any same-fingerprint mutations.
    Verify {
        /// Path to the indexed `.duckdb` file.
        db: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Index { trace, output } => {
            let db_path = output.unwrap_or_else(|| default_db_path(&trace));
            match Indexer::index(&trace, &db_path) {
                Ok(()) => {
                    println!("Indexed {} → {}", trace.display(), db_path.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("hindsight: index failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Serve { path, dir } => match run_serve(path, dir) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("hindsight: serve failed: {e}");
                ExitCode::FAILURE
            }
        },
        Command::Verify { db } => match hindsight_index::verify_to_string(&db) {
            Ok(report) => {
                println!("{report}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("hindsight: verify failed: {e}");
                ExitCode::FAILURE
            }
        },
        Command::Debug { trace, reindex } => {
            let db_path = default_db_path(&trace);
            if reindex || !db_path.exists() {
                if let Err(e) = Indexer::index(&trace, &db_path) {
                    eprintln!("hindsight: index failed: {e}");
                    return ExitCode::FAILURE;
                }
                eprintln!(
                    "hindsight: indexed {} → {}",
                    trace.display(),
                    db_path.display()
                );
            } else {
                eprintln!(
                    "hindsight: reusing existing index at {} (pass --reindex to rebuild)",
                    db_path.display()
                );
            }
            match run_serve_file(trace) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("hindsight: serve failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn run_serve(path: Option<PathBuf>, dir_flag: Option<PathBuf>) -> anyhow::Result<()> {
    if path.is_some() && dir_flag.is_some() {
        anyhow::bail!("pass either a positional path or --dir, not both");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    if let Some(d) = dir_flag {
        eprintln!("hindsight: serving directory {}", d.display());
        return runtime
            .block_on(async move { hindsight_mcp::run_stdio_dir(d).await.map_err(Into::into) });
    }

    match path {
        None => {
            let default = default_traces_dir()?;
            eprintln!("hindsight: serving directory {}", default.display());
            runtime.block_on(async move {
                hindsight_mcp::run_stdio_dir(default)
                    .await
                    .map_err(Into::into)
            })
        }
        Some(p) => {
            if p.is_dir() {
                eprintln!("hindsight: serving directory {}", p.display());
                runtime.block_on(async move {
                    hindsight_mcp::run_stdio_dir(p).await.map_err(Into::into)
                })
            } else {
                // .hindsight or .duckdb. Pre-flight (auto-index for
                // .hindsight, freshness-check for already-indexed).
                let db = prepare_single_file_target(&p)?;
                eprintln!("hindsight: serving single file {}", db.display());
                runtime.block_on(async move {
                    hindsight_mcp::run_stdio_file(db).await.map_err(Into::into)
                })
            }
        }
    }
}

fn run_serve_file(path: PathBuf) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        hindsight_mcp::run_stdio_file(path)
            .await
            .map_err(Into::into)
    })
}

/// Resolve a `hindsight serve <file>` argument to the `.duckdb` to actually
/// open. Three cases by extension:
///
/// - `.duckdb` — return the path unchanged. Caller serves it directly.
///   This is the legacy single-file mode and stays backward-compatible.
/// - `.hindsight` — auto-index. If a sibling `.duckdb` exists and is at
///   least as new as the source, reuse it; otherwise run the indexer.
///   The indexer is invoked synchronously at startup so any failure
///   (corrupt trace, malformed format, etc.) surfaces *before* the MCP
///   server starts listening — we never hand the client a broken server.
/// - anything else — error out with a helpful message.
///
/// If the source directory isn't writable, the indexer falls back to a
/// deterministic temp path (`<temp>/hindsight-<stem>-<hash>.duckdb`).
fn prepare_single_file_target(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "duckdb" => Ok(path.to_path_buf()),
        "hindsight" => ensure_indexed_for_serve(path),
        other => anyhow::bail!(
            "unsupported file extension {:?} for {} — pass a .hindsight, a .duckdb, or a directory",
            other,
            path.display()
        ),
    }
}

fn ensure_indexed_for_serve(hindsight: &std::path::Path) -> anyhow::Result<PathBuf> {
    if !hindsight.exists() {
        anyhow::bail!("trace file not found: {}", hindsight.display());
    }

    let sibling_db = hindsight.with_extension("duckdb");
    if sibling_db.exists() && is_db_current(hindsight, &sibling_db) {
        eprintln!(
            "hindsight: existing index is current at {} (skipping reindex)",
            sibling_db.display()
        );
        return Ok(sibling_db);
    }

    // Pick where to write. Prefer the sibling location; if the parent
    // dir isn't writable, fall back to a deterministic temp path so the
    // user doesn't have to think about it.
    let target = if can_write_in(hindsight.parent()) {
        sibling_db
    } else {
        let fallback = fallback_db_path(hindsight);
        eprintln!(
            "hindsight: source directory not writable, indexing to {} instead",
            fallback.display()
        );
        fallback
    };

    eprintln!("hindsight: indexing {}...", hindsight.display());
    let start = std::time::Instant::now();
    Indexer::index(hindsight, &target).map_err(|e| {
        anyhow::anyhow!(
            "indexing {} failed: {} — server not started",
            hindsight.display(),
            e
        )
    })?;
    let elapsed = start.elapsed();
    eprintln!(
        "hindsight: indexed in {}ms → {}",
        elapsed.as_millis(),
        target.display()
    );
    Ok(target)
}

/// `mtime(duckdb) >= mtime(hindsight)` — the index is up to date. Errors
/// reading either mtime are treated as "out of date" so we re-index.
fn is_db_current(hindsight: &std::path::Path, duckdb: &std::path::Path) -> bool {
    let h = std::fs::metadata(hindsight).and_then(|m| m.modified());
    let d = std::fs::metadata(duckdb).and_then(|m| m.modified());
    matches!((h, d), (Ok(h), Ok(d)) if d >= h)
}

/// Probe `dir` for write access by creating then deleting a tiny
/// dot-prefixed file. Conservative — returns false on any error.
fn can_write_in(dir: Option<&std::path::Path>) -> bool {
    let Some(dir) = dir else {
        return false;
    };
    if !dir.exists() {
        return false;
    }
    let probe = dir.join(format!(
        ".hindsight-write-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    match std::fs::File::create(&probe) {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn fallback_db_path(hindsight: &std::path::Path) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    hindsight
        .canonicalize()
        .unwrap_or_else(|_| hindsight.to_path_buf())
        .hash(&mut hasher);
    let h = hasher.finish();
    let stem = hindsight
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("trace");
    std::env::temp_dir().join(format!("hindsight-{stem}-{h:x}.duckdb"))
}

/// Default traces directory: `~/.hindsight/traces/`. Created if missing.
fn default_traces_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| anyhow::anyhow!("no HOME or USERPROFILE env var set"))?;
    let dir = PathBuf::from(home).join(".hindsight").join("traces");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

/// Replace the trace's extension with `.duckdb`. If the trace has no
/// extension, append `.duckdb`.
fn default_db_path(trace: &std::path::Path) -> PathBuf {
    let mut p = trace.to_path_buf();
    p.set_extension("duckdb");
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_subcommand_parses() {
        let cli = Cli::try_parse_from(["hindsight", "version"]).unwrap();
        assert!(matches!(cli.command, Command::Version));
    }

    #[test]
    fn missing_subcommand_is_an_error() {
        assert!(Cli::try_parse_from(["hindsight"]).is_err());
    }

    #[test]
    fn index_subcommand_parses_with_trace_only() {
        let cli = Cli::try_parse_from(["hindsight", "index", "trace.hindsight"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Index { ref trace, output: None } if trace.as_os_str() == "trace.hindsight"
        ));
    }

    #[test]
    fn index_subcommand_parses_with_output_flag() {
        let cli = Cli::try_parse_from([
            "hindsight",
            "index",
            "trace.hindsight",
            "--output",
            "out.duckdb",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Index { ref trace, output: Some(ref out) }
                if trace.as_os_str() == "trace.hindsight" && out.as_os_str() == "out.duckdb"
        ));
    }

    #[test]
    fn serve_with_no_args_parses() {
        let cli = Cli::try_parse_from(["hindsight", "serve"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Serve {
                path: None,
                dir: None
            }
        ));
    }

    #[test]
    fn serve_with_positional_path_parses() {
        let cli = Cli::try_parse_from(["hindsight", "serve", "out.duckdb"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Serve { path: Some(ref p), dir: None } if p.as_os_str() == "out.duckdb"
        ));
    }

    #[test]
    fn serve_with_dir_flag_parses() {
        let cli = Cli::try_parse_from(["hindsight", "serve", "--dir", "/tmp/traces"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Serve { path: None, dir: Some(ref d) } if d.as_os_str() == "/tmp/traces"
        ));
    }

    #[test]
    fn debug_subcommand_parses() {
        let cli = Cli::try_parse_from(["hindsight", "debug", "trace.hindsight"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Debug { ref trace, reindex: false } if trace.as_os_str() == "trace.hindsight"
        ));
    }

    #[test]
    fn debug_subcommand_parses_with_reindex() {
        let cli =
            Cli::try_parse_from(["hindsight", "debug", "trace.hindsight", "--reindex"]).unwrap();
        assert!(matches!(cli.command, Command::Debug { reindex: true, .. }));
    }

    #[test]
    fn default_db_path_swaps_extension() {
        assert_eq!(
            default_db_path(std::path::Path::new("foo.hindsight")),
            PathBuf::from("foo.duckdb"),
        );
        assert_eq!(
            default_db_path(std::path::Path::new("dir/foo.hindsight")),
            PathBuf::from("dir/foo.duckdb"),
        );
        assert_eq!(
            default_db_path(std::path::Path::new("foo")),
            PathBuf::from("foo.duckdb"),
        );
    }

    // ----- prepare_single_file_target / pre-flight indexing -----------------
    //
    // These tests exercise the startup-time auto-indexing for `.hindsight`
    // arguments. They drive the helper directly (no MCP serve loop) — the
    // helper is the only new behavior the change introduces.

    use hindsight_format::{
        Finalization, FunctionEntry, FunctionExit, Metadata, RecorderInfo, RecordingInfo,
        ScopeConfig, ScopeResolution, TraceWriter, Value,
    };

    fn fixture_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hindsight-cli-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a tiny valid `.hindsight` trace at `path`.
    fn write_minimal_trace(path: &std::path::Path) {
        let metadata = Metadata {
            recorder: RecorderInfo {
                language: "python".into(),
                language_version: "3.12.5".into(),
                recorder_version: "0.1.0".into(),
                platform: "test".into(),
            },
            recording: RecordingInfo {
                program: "python preflight.py".into(),
                working_directory: Some("/tmp".into()),
                scope_config: ScopeConfig {
                    include: vec![],
                    exclude: vec!["defaults".into()],
                    depth_limit: None,
                },
            },
            program: None,
            trace_uuid: [0xAB; 16],
            recording_start_ns: 0,
        };
        let mut w = TraceWriter::new(metadata);
        let fid = w.add_source_file("demo.py", b"def demo(): return 1\n".to_vec());
        let func_id = w.intern_string("demo.demo");
        let one = w.intern_value_inline(Value::Int(1));
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns: 0,
            frame_id: 0,
            function_id: func_id,
            source_file_id: fid,
            line: 1,
            args: vec![],
        })
        .unwrap();
        w.write_function_exit(FunctionExit {
            timestamp_delta_ns: 1,
            frame_id: 0,
            return_value: one,
        })
        .unwrap();
        let bytes = w
            .finish_to_bytes(Finalization {
                recording_end_ns: 100,
                scope_resolution: ScopeResolution {
                    recorded_functions: vec![],
                    excluded_functions: vec![],
                    skip_blocks_observed: 0,
                    depth_clips_observed: 0,
                },
            })
            .unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn serve_with_hindsight_file_auto_indexes() {
        let dir = fixture_dir();
        let trace = dir.join("auto.hindsight");
        write_minimal_trace(&trace);
        let expected_db = dir.join("auto.duckdb");
        assert!(!expected_db.exists(), "precondition: no .duckdb yet");

        let target = prepare_single_file_target(&trace).unwrap();
        assert_eq!(target, expected_db);
        assert!(
            expected_db.exists(),
            "indexer should have produced the .duckdb"
        );

        // Sanity-check the produced DB is queryable.
        let db = hindsight_mcp::DbConnection::open(expected_db).unwrap();
        let n: i64 = db
            .lock()
            .query_row("SELECT COUNT(*) FROM frames", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn serve_with_hindsight_file_skips_reindex_if_current() {
        let dir = fixture_dir();
        let trace = dir.join("skip.hindsight");
        write_minimal_trace(&trace);
        let db = dir.join("skip.duckdb");

        // First call indexes from scratch.
        prepare_single_file_target(&trace).unwrap();
        let mtime_after_first = std::fs::metadata(&db).unwrap().modified().unwrap();

        // The .duckdb wrote *after* the .hindsight, so it's current.
        // A second call must skip reindexing — verify by mtime equality.
        prepare_single_file_target(&trace).unwrap();
        let mtime_after_second = std::fs::metadata(&db).unwrap().modified().unwrap();
        assert_eq!(
            mtime_after_first, mtime_after_second,
            "second call should NOT have re-indexed (mtime should be unchanged)"
        );
    }

    #[test]
    fn serve_with_hindsight_file_reindexes_if_stale() {
        let dir = fixture_dir();
        let trace = dir.join("stale.hindsight");
        write_minimal_trace(&trace);
        let db = dir.join("stale.duckdb");

        // First index.
        prepare_single_file_target(&trace).unwrap();
        let db_mtime_first = std::fs::metadata(&db).unwrap().modified().unwrap();

        // Force the .hindsight to look strictly newer than the .duckdb
        // — bumps the source mtime by a comfortable margin so even
        // coarse-resolution filesystems agree it's newer.
        let bumped = db_mtime_first + std::time::Duration::from_secs(60);
        let f = std::fs::File::options().write(true).open(&trace).unwrap();
        f.set_modified(bumped).unwrap();

        // Second call should re-index.
        prepare_single_file_target(&trace).unwrap();
        let db_mtime_second = std::fs::metadata(&db).unwrap().modified().unwrap();
        assert!(
            db_mtime_second > db_mtime_first,
            "stale .duckdb should have been re-indexed (mtime should have advanced)"
        );
    }

    #[test]
    fn serve_with_corrupt_hindsight_file_exits_cleanly() {
        let dir = fixture_dir();
        let trace = dir.join("corrupt.hindsight");
        // Garbage bytes — not a valid Hindsight trace.
        std::fs::write(&trace, b"this is not a hindsight trace at all").unwrap();
        let db = dir.join("corrupt.duckdb");

        let result = prepare_single_file_target(&trace);
        assert!(result.is_err(), "corrupt trace must produce an error");
        assert!(
            !db.exists(),
            "no .duckdb should remain after a failed index"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("indexing") && msg.contains("corrupt.hindsight"),
            "error message should name the file and the operation, got: {msg}"
        );
    }

    #[test]
    fn prepare_single_file_target_passes_duckdb_through() {
        let dir = fixture_dir();
        let trace = dir.join("legacy.hindsight");
        write_minimal_trace(&trace);
        // Pre-build the .duckdb the way `hindsight index` would.
        let db = dir.join("legacy.duckdb");
        Indexer::index(&trace, &db).unwrap();
        let mtime_before = std::fs::metadata(&db).unwrap().modified().unwrap();

        // Pointing serve at the .duckdb directly should NOT touch it.
        let target = prepare_single_file_target(&db).unwrap();
        assert_eq!(target, db);
        let mtime_after = std::fs::metadata(&db).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after);
    }

    #[test]
    fn prepare_single_file_target_rejects_unknown_extension() {
        let dir = fixture_dir();
        let weird = dir.join("data.bin");
        std::fs::write(&weird, b"hello").unwrap();
        let err = prepare_single_file_target(&weird).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unsupported file extension"), "got: {msg}");
    }
}
