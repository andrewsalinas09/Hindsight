// SPDX-License-Identifier: Apache-2.0

//! Identifier extraction from source lines.
//!
//! Used by `explain_branch` (locals around a branch condition) and
//! `causal_slice` (RHS dependency walk). The extractor is a deliberately
//! simple Python-identifier scanner — it walks bytes, skips string
//! literals and comments, and emits everything that matches
//! `[A-Za-z_][A-Za-z0-9_]*`. Python keywords are filtered out so the LLM
//! doesn't see `if`, `else`, `not`, etc. as locals to look up.
//!
//! Limitations (documented in tool output):
//! - Does not parse f-strings (interpolated identifiers are missed).
//! - Treats attribute access as the leading identifier only (e.g.
//!   `order.total` extracts `order`).
//! - Does not distinguish definition from use.
//!
//! For v0 this is good enough. A future improvement is to swap in a
//! Rust-based Python parser like `rustpython-parser`.

const PY_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield", "match", "case",
];

/// Extract Python-style identifiers from `source`. Skips string literals
/// and `#` comments. Returns identifiers in order of appearance, with
/// duplicates preserved (callers can dedupe if needed).
pub fn extract_identifiers(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // Skip whitespace.
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Skip comment to end-of-line.
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip string literals (single or double, possibly triple-quoted).
        if b == b'"' || b == b'\'' {
            i = skip_string(bytes, i);
            continue;
        }
        // Identifier.
        if is_ident_start(b) {
            let start = i;
            while i < bytes.len() && is_ident_cont(bytes[i]) {
                i += 1;
            }
            // Treat attribute access: `obj.attr` — emit only the leading
            // identifier so the dependency walk sees the local that holds
            // the object, not a phantom "attr" name.
            let ident = &source[start..i];
            if !PY_KEYWORDS.contains(&ident) {
                out.push(ident.to_string());
            }
            // Skip subsequent .attr chains.
            while i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                while i < bytes.len() && is_ident_cont(bytes[i]) {
                    i += 1;
                }
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Identifiers on the RHS of an assignment, if `source` looks like one.
/// Heuristic: split on the first top-level `=` not preceded by an
/// operator character (`==`, `!=`, `>=`, `<=`, `+=`, etc.). For
/// augmented-assignments like `x += 1` we treat the LHS as both an input
/// and an output (which matches how a dependency walk should see it).
pub fn rhs_identifiers(source: &str) -> Vec<String> {
    let trimmed = source.trim_start();
    let leading = source.len() - trimmed.len();
    let bytes = trimmed.as_bytes();
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut depth_brace = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'(' => depth_paren += 1,
            b')' => depth_paren -= 1,
            b'[' => depth_brack += 1,
            b']' => depth_brack -= 1,
            b'{' => depth_brace += 1,
            b'}' => depth_brace -= 1,
            b'#' => break,
            b'"' | b'\'' => {
                i = skip_string(bytes, i);
                continue;
            }
            b'=' if depth_paren == 0 && depth_brack == 0 && depth_brace == 0 => {
                // Reject `==`, `>=`, `<=`, `!=`.
                let next = bytes.get(i + 1).copied();
                let prev = if i == 0 { None } else { Some(bytes[i - 1]) };
                let is_compare =
                    next == Some(b'=') || matches!(prev, Some(b'=' | b'<' | b'>' | b'!'));
                let is_augmented = matches!(
                    prev,
                    Some(b'+' | b'-' | b'*' | b'/' | b'%' | b'|' | b'&' | b'^')
                );
                if !is_compare {
                    let abs = leading + i + 1;
                    if is_augmented {
                        // Treat both sides as identifiers.
                        return extract_identifiers(source);
                    }
                    let rhs = &source[abs..];
                    return extract_identifiers(rhs);
                }
            }
            _ => {}
        }
        i += 1;
    }
    // No assignment — treat entire source as the expression.
    extract_identifiers(source)
}

fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic()
}

fn is_ident_cont(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Advance `i` past a Python string literal starting at `bytes[i]`. Handles
/// triple quotes and backslash escapes. Returns the index just after the
/// closing quote(s), or `bytes.len()` if the literal doesn't close in the
/// input.
fn skip_string(bytes: &[u8], i: usize) -> usize {
    let quote = bytes[i];
    // Triple-quoted?
    if i + 2 < bytes.len() && bytes[i + 1] == quote && bytes[i + 2] == quote {
        let mut j = i + 3;
        while j + 2 < bytes.len() {
            if bytes[j] == b'\\' {
                j += 2;
                continue;
            }
            if bytes[j] == quote && bytes[j + 1] == quote && bytes[j + 2] == quote {
                return j + 3;
            }
            j += 1;
        }
        return bytes.len();
    }
    // Single-quoted — to closing quote on same line.
    let mut j = i + 1;
    while j < bytes.len() {
        let b = bytes[j];
        if b == b'\\' {
            j += 2;
            continue;
        }
        if b == b'\n' {
            return j;
        }
        if b == quote {
            return j + 1;
        }
        j += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_basic_identifiers() {
        let ids = extract_identifiers("largest = item");
        assert_eq!(ids, vec!["largest", "item"]);
    }

    #[test]
    fn skips_keywords() {
        let ids = extract_identifiers("if largest is None or item > largest:");
        assert_eq!(ids, vec!["largest", "item", "largest"]);
    }

    #[test]
    fn skips_string_contents() {
        let ids = extract_identifiers("x = \"hello item world\"");
        assert_eq!(ids, vec!["x"]);
    }

    #[test]
    fn skips_comments() {
        let ids = extract_identifiers("y = 1  # foo bar baz");
        assert_eq!(ids, vec!["y"]);
    }

    #[test]
    fn rhs_only() {
        let ids = rhs_identifiers("largest = item");
        assert_eq!(ids, vec!["item"]);
    }

    #[test]
    fn rhs_augmented_includes_lhs() {
        let mut ids = rhs_identifiers("revenue += amount");
        ids.sort();
        assert_eq!(ids, vec!["amount", "revenue"]);
    }

    #[test]
    fn rhs_complex_expression() {
        let ids = rhs_identifiers("result = a + b * c");
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn attribute_only_leading() {
        let ids = extract_identifiers("amount = order.total");
        assert_eq!(ids, vec!["amount", "order"]);
    }

    #[test]
    fn no_assignment_treats_as_expression() {
        let ids = rhs_identifiers("if item <= threshold:");
        assert_eq!(ids, vec!["item", "threshold"]);
    }
}
