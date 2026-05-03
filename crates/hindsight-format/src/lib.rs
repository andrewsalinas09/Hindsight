// SPDX-License-Identifier: Apache-2.0

//! Binary trace format reader and writer for Hindsight.
//!
//! Implementation will follow the spec in `docs/trace-format.md`.

pub fn placeholder() -> &'static str {
    "hindsight-format"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_returns_crate_name() {
        assert_eq!(placeholder(), "hindsight-format");
    }
}
