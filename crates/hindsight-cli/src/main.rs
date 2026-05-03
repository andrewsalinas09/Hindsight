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
    }
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
}
