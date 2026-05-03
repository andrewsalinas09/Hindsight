// SPDX-License-Identifier: Apache-2.0

//! Model Context Protocol server exposing Hindsight debugging primitives.

pub fn placeholder() -> &'static str {
    "hindsight-mcp"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_returns_crate_name() {
        assert_eq!(placeholder(), "hindsight-mcp");
    }
}
