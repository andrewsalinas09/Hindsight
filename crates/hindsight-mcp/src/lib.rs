// SPDX-License-Identifier: Apache-2.0

//! Model Context Protocol server exposing Hindsight debugging primitives.

pub fn hello_world() -> &'static str {
    "hello from hindsight-mcp"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_world_returns_greeting() {
        assert_eq!(hello_world(), "hello from hindsight-mcp");
    }
}
