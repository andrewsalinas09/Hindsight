// SPDX-License-Identifier: Apache-2.0

//! Per-tool tests against fixture traces. Each test exercises one tool and
//! asserts on the structured output. Tests are kept in this single file
//! per the workspace convention; they share fixtures from `common/mod.rs`.

mod common;

use hindsight_mcp::HindsightServer;
use hindsight_mcp::tools::{
    causal_slice, describe_schema, exception_chain, explain_branch, find_call, find_iterations,
    get_call_tree, get_source, run_sql, trace_variable, why_did_value_change,
};

use common::{
    build_basic_trace, build_data_processing_trace, build_exception_trace, build_minimal_trace,
    build_recursion_trace, index_to_db,
};

// ---------------------------------------------------------------------------
// Tool registration / smoke
// ---------------------------------------------------------------------------

#[test]
fn server_registers_all_eleven_tools() {
    let (db, _path) = index_to_db(&build_minimal_trace());
    let server = HindsightServer::new(db);
    let mut names = server.list_tool_names();
    names.sort();
    let mut expected: Vec<String> = vec![
        "causal_slice",
        "describe_schema",
        "exception_chain",
        "explain_branch",
        "find_call",
        "find_iterations",
        "get_call_tree",
        "get_source",
        "run_sql",
        "trace_variable",
        "why_did_value_change",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    expected.sort();
    assert_eq!(names, expected);
}

// ---------------------------------------------------------------------------
// describe_schema
// ---------------------------------------------------------------------------

#[test]
fn describe_schema_returns_documented_tables_and_patterns() {
    let (db, _) = index_to_db(&build_minimal_trace());
    let out = describe_schema::run(&db).unwrap();
    let names: Vec<&str> = out.tables.iter().map(|t| t.name.as_str()).collect();
    for must in [
        "events",
        "frames",
        "event_locals",
        "values",
        "source_files",
        "branches",
        "exceptions",
        "notes",
    ] {
        assert!(names.contains(&must), "missing table {must}");
    }
    assert!(out.common_query_patterns.len() >= 5);
    // Frames table should have its description.
    let frames = out.tables.iter().find(|t| t.name == "frames").unwrap();
    assert!(frames.description.contains("function activation"));
    assert!(frames.columns.iter().any(|c| c.name == "qualified_name"));
}

// ---------------------------------------------------------------------------
// run_sql
// ---------------------------------------------------------------------------

#[test]
fn run_sql_returns_rows() {
    let (db, _) = index_to_db(&build_basic_trace());
    let out = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            query: "SELECT COUNT(*) AS n FROM frames".into(),
            max_rows: None,
        },
    )
    .unwrap();
    assert_eq!(out.columns, vec!["n".to_string()]);
    assert_eq!(out.row_count, 1);
    assert!(!out.truncated);
}

#[test]
fn run_sql_rejects_writes() {
    let (db, _) = index_to_db(&build_minimal_trace());
    let err = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            query: "INSERT INTO events VALUES (999, 'x', 0, 0, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)"
                .into(),
            max_rows: None,
        },
    )
    .unwrap_err();
    assert_eq!(err.error, "write_query_rejected");
}

#[test]
fn run_sql_returns_structured_sql_error() {
    let (db, _) = index_to_db(&build_minimal_trace());
    let err = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            query: "SELECT not a real column FROM events".into(),
            max_rows: None,
        },
    )
    .unwrap_err();
    assert_eq!(err.error, "sql_error");
}

#[test]
fn run_sql_truncates() {
    let (db, _) = index_to_db(&build_basic_trace());
    let out = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            query: "SELECT event_id FROM events ORDER BY event_id".into(),
            max_rows: Some(2),
        },
    )
    .unwrap();
    assert_eq!(out.row_count, 2);
    assert!(out.truncated);
}

// ---------------------------------------------------------------------------
// get_source
// ---------------------------------------------------------------------------

#[test]
fn get_source_returns_full_file() {
    let (db, _) = index_to_db(&build_basic_trace());
    let out = get_source::run(
        &db,
        get_source::GetSourceInput {
            file_path: "basic.py".into(),
            line_range: None,
        },
    )
    .unwrap();
    assert!(out.content.contains("find_largest_below"));
    assert_eq!(out.start_line, 1);
    assert!(out.total_lines >= 12);
}

#[test]
fn get_source_returns_window() {
    let (db, _) = index_to_db(&build_basic_trace());
    let out = get_source::run(
        &db,
        get_source::GetSourceInput {
            file_path: "basic.py".into(),
            line_range: Some([5, 10]),
        },
    )
    .unwrap();
    assert_eq!(out.start_line, 5);
    assert_eq!(out.end_line, 10);
    let lines: Vec<&str> = out.content.lines().collect();
    assert_eq!(lines.len(), 6);
}

#[test]
fn get_source_missing_file_returns_structured_error() {
    let (db, _) = index_to_db(&build_basic_trace());
    let err = get_source::run(
        &db,
        get_source::GetSourceInput {
            file_path: "nonexistent.py".into(),
            line_range: None,
        },
    )
    .unwrap_err();
    assert_eq!(err.error, "source_not_found");
    assert!(err.message.contains("basic.py"));
}

// ---------------------------------------------------------------------------
// find_call
// ---------------------------------------------------------------------------

#[test]
fn find_call_locates_the_buggy_run() {
    let (db, _) = index_to_db(&build_basic_trace());
    let out = find_call::run(
        &db,
        find_call::FindCallInput {
            qualified_name: "__main__.find_largest_below".into(),
            r#where: None,
            limit: None,
        },
    )
    .unwrap();
    assert_eq!(out.matches.len(), 1);
    assert_eq!(out.matches[0].frame_id, 0);
    assert_eq!(out.matches[0].exit_kind.as_deref(), Some("returned"));
}

#[test]
fn find_call_missing_returns_unknown() {
    let (db, _) = index_to_db(&build_basic_trace());
    let out = find_call::run(
        &db,
        find_call::FindCallInput {
            qualified_name: "__main__.does_not_exist".into(),
            r#where: None,
            limit: None,
        },
    )
    .unwrap();
    assert_eq!(out.matches.len(), 0);
}

#[test]
fn find_call_filters_by_argument_contains() {
    let (db, _) = index_to_db(&build_recursion_trace());
    let out = find_call::run(
        &db,
        find_call::FindCallInput {
            qualified_name: "__main__.fib".into(),
            r#where: Some(find_call::FindCallWhere {
                argument_contains: Some("n=2".into()),
                ..Default::default()
            }),
            limit: None,
        },
    )
    .unwrap();
    assert_eq!(out.matches.len(), 1);
    assert!(
        out.matches[0]
            .argument_summary
            .as_ref()
            .unwrap()
            .contains("n=2")
    );
}

// ---------------------------------------------------------------------------
// trace_variable
// ---------------------------------------------------------------------------

#[test]
fn trace_variable_returns_full_history() {
    let (db, _) = index_to_db(&build_basic_trace());
    let out = trace_variable::run(
        &db,
        trace_variable::TraceVariableInput {
            name: "largest".into(),
            frame_id: 0,
            before_event_id: None,
        },
    )
    .unwrap();
    assert_eq!(out.name, "largest");
    // Snapshot (None) + four assignments (3, 7, 9, 10) = 5 captures
    let displays: Vec<&str> = out
        .captures
        .iter()
        .map(|c| c.value.display.as_str())
        .collect();
    assert_eq!(displays, vec!["None", "3", "7", "9", "10"]);
    // The last capture should also include `item=10` in context.
    let last_ctx = &out.captures.last().unwrap().context;
    assert_eq!(last_ctx.get("item").unwrap().display, "10");
}

#[test]
fn trace_variable_unknown_frame_returns_structured_error() {
    let (db, _) = index_to_db(&build_basic_trace());
    let err = trace_variable::run(
        &db,
        trace_variable::TraceVariableInput {
            name: "largest".into(),
            frame_id: 9999,
            before_event_id: None,
        },
    )
    .unwrap_err();
    assert_eq!(err.error, "frame_not_found");
}

// ---------------------------------------------------------------------------
// explain_branch
// ---------------------------------------------------------------------------

#[test]
fn explain_branch_returns_locals_and_source() {
    let (db, _) = index_to_db(&build_basic_trace());
    // Find a branch event_id at line 9 with taken=true.
    let row = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            query: "SELECT event_id FROM branches WHERE line = 9 AND taken = true ORDER BY event_id DESC LIMIT 1"
                .into(),
            max_rows: None,
        },
    )
    .unwrap();
    let event_id = row.rows[0][0].as_i64().unwrap();

    let out = explain_branch::run(&db, explain_branch::ExplainBranchInput { event_id }).unwrap();
    assert!(out.taken);
    assert_eq!(out.line, 9);
    // Locals should include `largest` and `item`.
    assert!(out.locals_at_branch.contains_key("item"));
    assert!(out.locals_at_branch.contains_key("largest"));
    // Source context window should include the branch line itself.
    assert!(out.source_context.lines.iter().any(|l| l.line == 9));
}

#[test]
fn explain_branch_unknown_event_returns_error() {
    let (db, _) = index_to_db(&build_basic_trace());
    let err = explain_branch::run(
        &db,
        explain_branch::ExplainBranchInput { event_id: 999_999 },
    )
    .unwrap_err();
    assert_eq!(err.error, "branch_not_found");
}

// ---------------------------------------------------------------------------
// why_did_value_change
// ---------------------------------------------------------------------------

#[test]
fn why_did_value_change_explains_the_buggy_assignment() {
    let (db, _) = index_to_db(&build_basic_trace());
    let row = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            query: "SELECT MAX(event_id) FROM events".into(),
            max_rows: None,
        },
    )
    .unwrap();
    let around = row.rows[0][0].as_i64().unwrap();

    let out = why_did_value_change::run(
        &db,
        why_did_value_change::WhyDidValueChangeInput {
            name: "largest".into(),
            frame_id: 0,
            around_event_id: around,
        },
    )
    .unwrap();
    assert_eq!(out.name, "largest");
    assert_eq!(out.change_event.new_value.display, "10");
    assert_eq!(out.change_event.previous_value.unwrap().display, "9");
    // Context should include `item=10`.
    assert_eq!(out.context_at_change.get("item").unwrap().display, "10");
    // Two preceding branches at line 8 and 9 should be captured.
    assert!(out.preceding_branches.len() >= 2);
    assert!(out.narrative_hint.contains("`largest`"));
}

// ---------------------------------------------------------------------------
// find_iterations
// ---------------------------------------------------------------------------

#[test]
fn find_iterations_returns_six_iterations() {
    let (db, _) = index_to_db(&build_basic_trace());
    let out = find_iterations::run(
        &db,
        find_iterations::FindIterationsInput {
            frame_id: 0,
            loop_line: 7,
        },
    )
    .unwrap();
    assert_eq!(out.iteration_count, 6);
    // Every iteration's loop variable should be `item`.
    for it in &out.iterations {
        assert!(it.loop_variables.iter().any(|lv| lv.name == "item"));
    }
    // The last iteration is the buggy one (item=10).
    let last = out.iterations.last().unwrap();
    let item = last
        .loop_variables
        .iter()
        .find(|lv| lv.name == "item")
        .unwrap();
    assert_eq!(item.value.display, "10");
}

// ---------------------------------------------------------------------------
// exception_chain
// ---------------------------------------------------------------------------

#[test]
fn exception_chain_walks_three_frames_and_finds_catcher() {
    let (db, _) = index_to_db(&build_exception_trace());
    let row = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            query: "SELECT event_id FROM exceptions ORDER BY event_id LIMIT 1".into(),
            max_rows: None,
        },
    )
    .unwrap();
    let event_id = row.rows[0][0].as_i64().unwrap();

    let out = exception_chain::run(&db, exception_chain::ExceptionChainInput { event_id }).unwrap();
    assert_eq!(out.exception_type, "builtins.ValueError");
    assert_eq!(out.propagation.len(), 3);
    assert!(out.ultimately_caught);
    assert_eq!(out.catching_frame, Some(0));
    // The deepest frame raised; the top frame returned.
    assert_eq!(out.propagation[0].exit_kind.as_deref(), Some("raised"));
    assert_eq!(out.propagation[2].exit_kind.as_deref(), Some("returned"));
    assert!(out.propagation[2].caught_at.is_some());
}

// ---------------------------------------------------------------------------
// get_call_tree
// ---------------------------------------------------------------------------

#[test]
fn get_call_tree_returns_recursive_structure() {
    let (db, _) = index_to_db(&build_recursion_trace());
    let out = get_call_tree::run(
        &db,
        get_call_tree::GetCallTreeInput {
            frame_id: 0,
            max_depth: None,
            include_args: true,
        },
    )
    .unwrap();
    assert_eq!(out.qualified_name, "__main__.main");
    assert_eq!(out.children.len(), 1);
    let fib3 = &out.children[0];
    assert_eq!(fib3.qualified_name, "__main__.fib");
    // fib(3) calls fib(2) and fib(1).
    assert_eq!(fib3.children.len(), 2);
}

#[test]
fn get_call_tree_respects_max_depth() {
    let (db, _) = index_to_db(&build_recursion_trace());
    let out = get_call_tree::run(
        &db,
        get_call_tree::GetCallTreeInput {
            frame_id: 0,
            max_depth: Some(1),
            include_args: true,
        },
    )
    .unwrap();
    assert_eq!(out.children.len(), 1);
    // Only one level deep — fib(3)'s children should be pruned.
    assert!(out.children[0].children.is_empty());
}

#[test]
fn get_call_tree_unknown_frame_errors() {
    let (db, _) = index_to_db(&build_recursion_trace());
    let err = get_call_tree::run(
        &db,
        get_call_tree::GetCallTreeInput {
            frame_id: 9999,
            max_depth: None,
            include_args: true,
        },
    )
    .unwrap_err();
    assert_eq!(err.error, "frame_not_found");
}

// ---------------------------------------------------------------------------
// causal_slice
// ---------------------------------------------------------------------------

#[test]
fn causal_slice_walks_from_largest_back_to_args() {
    let (db, _) = index_to_db(&build_basic_trace());
    // The final value of `largest` is the integer 10. Find its value_id.
    let row = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            query: "SELECT value_id FROM values WHERE type_tag = 'int' AND int_value = 10 LIMIT 1"
                .into(),
            max_rows: None,
        },
    )
    .unwrap();
    let value_id = row.rows[0][0].as_i64().unwrap();
    let out = causal_slice::run(
        &db,
        causal_slice::CausalSliceInput {
            value_id,
            max_depth: Some(3),
        },
    )
    .unwrap();
    assert_eq!(out.root_value.display, "10");
    let captured = out.captured_as.unwrap();
    assert_eq!(captured.name, "largest");
    // Should depend on `item`.
    assert!(out.depends_on.iter().any(|d| d.name == "item"));
    // And `item` should ultimately depend on `values` (a function arg).
    let item = out.depends_on.iter().find(|d| d.name == "item").unwrap();
    assert!(
        item.depends_on
            .iter()
            .any(|d| d.name == "values" && d.note.as_deref() == Some("function argument"))
    );
}

#[test]
fn causal_slice_invalid_value_returns_error() {
    let (db, _) = index_to_db(&build_basic_trace());
    let err = causal_slice::run(
        &db,
        causal_slice::CausalSliceInput {
            value_id: i64::MAX,
            max_depth: None,
        },
    )
    .unwrap_err();
    assert_eq!(err.error, "value_not_found");
}

// ---------------------------------------------------------------------------
// data processing fixture: silence dead-code warning
// ---------------------------------------------------------------------------

#[test]
fn data_processing_fixture_indexes() {
    let (db, _) = index_to_db(&build_data_processing_trace());
    let out = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            query: "SELECT COUNT(*) FROM frames".into(),
            max_rows: None,
        },
    )
    .unwrap();
    assert_eq!(out.row_count, 1);
}
