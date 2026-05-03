# CLAUDE.md

## Context for Claude Code working in this repository

This is Hindsight, an AI-native debugger built around the Model Context Protocol. The full design is in `ARCHITECTURE.md` and the trace format spec is in `docs/trace-format.md`. Read both before doing significant work in this codebase.

## Architectural decisions that aren't up for renegotiation

The core is in Rust. Don't propose rewriting any component in another language. Don't propose using C++ as an alternative even when something feels easier in C++. Rust was chosen deliberately and the reasons are in `ARCHITECTURE.md`.

The Python recorder is in Python and only Python. Its job is to hook the interpreter via `sys.monitoring` and pass events to the Rust core via PyO3. Don't add significant logic to the Python side; logic belongs in Rust where it's tested once and reused by every language frontend.

We target Python 3.12 and later only. `sys.monitoring` doesn't exist in earlier versions and we've explicitly chosen not to support `sys.settrace` fallback paths. If something requires older Python, we don't support that case.

The trace format is binary, not JSON or YAML or any text format. Don't propose text formats even for "debug" or "human-readable" variants. The format is documented in `docs/trace-format.md` and that document is the source of truth. Changes to the format require a version bump and require updating the spec first, not the code.

The indexer uses DuckDB. Don't propose alternative databases (SQLite, Postgres, custom B-trees, etc.). DuckDB was chosen for analytical query performance over append-only event streams; the alternatives are worse for our use case.

The MCP server uses the official Rust MCP SDK from Anthropic. Don't propose implementing the protocol from scratch; use the SDK.

The MCP tool surface is intentionally small. The list is in `ARCHITECTURE.md`. Don't add tools to the surface without explicit discussion. If a feature seems to need a new tool, first try to compose it from existing tools; if that genuinely doesn't work, raise it for discussion before implementing.

The recorder is opt-in via decorator and context manager. We don't record everything by default. Don't propose making capture broader without explicit discussion.

The license is Apache 2.0. Every source file should have the SPDX header `// SPDX-License-Identifier: Apache-2.0` (or the Python equivalent for Python files).

## Coding conventions

Use Rust 2024 edition. Prefer the standard library over external crates when the standard library is sufficient. When you do add a dependency, prefer well-maintained crates with reasonable transitive dependency counts.

Don't use `unsafe` Rust without explicit justification in a comment. The justification needs to explain what invariant the unsafe code is upholding and why it can't be expressed safely. Almost nothing in this codebase needs `unsafe`; if you find yourself reaching for it, reconsider the design first.

Errors propagate via `Result` types. Use `thiserror` for library errors and `anyhow` for binary error handling. Don't use `unwrap()` or `expect()` outside of test code. If you genuinely have a panic-on-bug situation, use `panic!` with a descriptive message rather than `unwrap`.

Functions should be small and named for what they do. Long functions are usually a sign of unclear thinking; if a function exceeds 50 lines, look for a structural break.

Tests live alongside the code they test. Each crate has a `tests/` directory for integration tests and inline `#[cfg(test)]` modules for unit tests. New code requires tests; refactors don't require new tests but shouldn't reduce coverage of existing code.

Code style follows `rustfmt` defaults. Run `cargo fmt` before committing. Run `cargo clippy` and address its warnings; if a warning is genuinely wrong, suppress it with a comment explaining why rather than ignoring it globally.

For the Python recorder, follow PEP 8. Use type hints throughout. The Python side is small enough that there's no excuse for unclear types.

## What to do when starting a task

Read `ARCHITECTURE.md` if you haven't recently. Read `docs/trace-format.md` if the task touches the format. Read the existing code in the relevant crate to understand current patterns; match those patterns rather than introducing new ones.

When implementing something, look at the existing tests for similar code to understand the testing patterns. Write tests as you go, not as an afterthought.

If you're about to make a decision that feels architecturally significant — a new dependency, a new module structure, a deviation from the patterns in this document — pause and surface the decision to the user before proceeding. Architectural drift is the main risk for this project; a brief check-in before drifting is much cheaper than a refactor afterward.

## What to do when you encounter ambiguity

Don't guess. If a task is underspecified, ask for clarification. The architectural docs cover the major decisions but leave room for interpretation in the details; check with the user when you hit those gaps rather than picking arbitrarily.

If you find a contradiction between what the user is asking for and what the architecture documents say, surface it. The architecture documents win unless the user explicitly says otherwise — which is fine, plans evolve, but the change should be deliberate.

## What to do when you finish a task

Run `cargo build --workspace` to ensure everything compiles. Run `cargo test --workspace` to ensure tests pass. Run `cargo clippy --workspace -- -D warnings` to ensure no lint issues. Run `cargo fmt --check` to ensure formatting is consistent.

For Python changes, run the test suite for the recorder package.

Commit when the task is complete and verified. Commit messages should describe what changed and why, not just what files were touched. Use the imperative mood ("Add X" not "Added X" or "Adds X").

## Things this codebase doesn't do

We don't have a web UI. The user interface is the AI client connected via MCP. Don't propose web UIs even for "admin" or "diagnostic" purposes.

We don't do live debugging. Hindsight is offline post-hoc analysis. Don't propose features that require pausing or stepping through a running program.

We don't do distributed tracing. Single-process recording only in v0. The format will eventually support distributed traces, but adding it now is out of scope.

We don't do replay. The trace is a record of what happened, not enough to re-execute the program. Replay is a different and harder problem and not what we're building.

We don't have authentication, authorization, or multi-tenancy. This is a local tool that operates on local files. If a deployment scenario requires those features, that's a future product, not v0.

## How to think about performance

Performance matters but correctness comes first. Don't sacrifice clarity for micro-optimizations. The recorder is the most performance-sensitive component because it sits in the hot path of the program being recorded; everything else is run-once-after-the-fact and has more slack.

Profile before optimizing. We use `criterion` for Rust benchmarks. If you propose an optimization, include a benchmark that demonstrates the improvement on representative data.

Algorithmic improvements beat micro-optimizations almost always. If something is slow, look for the asymptotic problem before reaching for `unsafe` or hand-rolled SIMD.

## How to think about features

The temptation in a project like this is to add features. Resist it. Hindsight ships if v0 is small and works; it dies if v0 grows beyond what can be finished.

When you find yourself thinking "it would be cool to also do X," write X down somewhere (issue, comment, doc) and don't implement it now. We'll get to it after v0 ships if it still seems worth doing then. Many things that seem essential before shipping turn out not to be once real users start trying the tool.

The minimum bar for a feature in v0 is that the demo doesn't work without it. If we can demo Hindsight catching a real bug in a real Python program without feature X, feature X is not in v0.
