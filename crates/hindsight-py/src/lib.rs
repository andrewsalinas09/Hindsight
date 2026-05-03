// SPDX-License-Identifier: Apache-2.0

//! PyO3 bindings exposing the Hindsight Rust core to the Python recorder.

pub fn placeholder() -> &'static str {
    "hindsight-py"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_returns_crate_name() {
        assert_eq!(placeholder(), "hindsight-py");
    }
}
