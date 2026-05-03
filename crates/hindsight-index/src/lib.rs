// SPDX-License-Identifier: Apache-2.0

//! Indexes Hindsight trace files into an embedded DuckDB database.

pub fn hello_world() -> &'static str {
    "hello from hindsight-index"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_world_returns_greeting() {
        assert_eq!(hello_world(), "hello from hindsight-index");
    }
}
