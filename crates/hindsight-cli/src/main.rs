// SPDX-License-Identifier: Apache-2.0

//! Command-line entry point that orchestrates the Hindsight components.

use clap::{Parser, Subcommand};

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
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Version => println!("{}", env!("CARGO_PKG_VERSION")),
    }
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
}
