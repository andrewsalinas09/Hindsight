// SPDX-License-Identifier: Apache-2.0

//! Command-line entry point that orchestrates the Hindsight components.

fn main() {
    println!("{}", placeholder());
}

fn placeholder() -> &'static str {
    "hindsight-cli"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_returns_crate_name() {
        assert_eq!(placeholder(), "hindsight-cli");
    }
}
