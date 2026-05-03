// SPDX-License-Identifier: Apache-2.0

//! Binary trace format reader and writer for Hindsight.
//!
//! Implementation will follow the spec in `docs/trace-format.md`.

pub fn hello_world() -> &'static str {
    "hello from hindsight-format"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_world_returns_greeting() {
        assert_eq!(hello_world(), "hello from hindsight-format");
    }
}
