// SPDX-License-Identifier: Apache-2.0

//! PyO3 bindings exposing the Hindsight Rust core to the Python recorder.

pub fn hello_world() -> &'static str {
    "hello from hindsight-py"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_world_returns_greeting() {
        assert_eq!(hello_world(), "hello from hindsight-py");
    }
}
