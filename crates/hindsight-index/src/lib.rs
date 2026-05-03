// SPDX-License-Identifier: Apache-2.0

//! Indexes Hindsight trace files into an embedded DuckDB database.

pub fn placeholder() -> &'static str {
    "hindsight-index"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_returns_crate_name() {
        assert_eq!(placeholder(), "hindsight-index");
    }
}
