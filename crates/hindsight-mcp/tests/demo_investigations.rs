// SPDX-License-Identifier: Apache-2.0

//! End-to-end demo investigations from `docs/mcp-server-design.md`.
//!
//! Each test sequences the tool calls a smart LLM would make and asserts
//! on the structured output at each step. These tests double as the
//! canonical examples of the server in use — running them under
//! `--nocapture` prints the JSON payloads each tool returns, which is
//! exactly what the design document promised to surface.
//!
//! Use:
//!     cargo test -p hindsight-mcp --test demo_investigations -- --nocapture

mod common;

use hindsight_mcp::tools::{
    causal_slice, exception_chain, find_call, find_iterations, get_call_tree, get_source, run_sql,
    trace_variable, why_did_value_change,
};

use common::{
    TID, build_basic_trace, build_data_processing_trace, build_exception_trace,
    build_recursion_trace, index_to_db,
};

fn dump<T: serde::Serialize>(label: &str, v: &T) {
    println!("\n--- {label} ---");
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

// ---------------------------------------------------------------------------
// Investigation 1: the off-by-one bug in find_largest_below.
//
// User asks: "Why did find_largest_below([3,7,1,9,4,10], 10) return 10
// instead of 9?"
// ---------------------------------------------------------------------------
#[test]
fn investigation_1_off_by_one_bug() {
    let (db, _) = index_to_db(&build_basic_trace());

    // Step 1: find the call by argument.
    let calls = find_call::run(
        &db,
        find_call::FindCallInput {
            trace_id: TID.into(),
            qualified_name: "__main__.find_largest_below".into(),
            r#where: Some(find_call::FindCallWhere {
                argument_contains: Some("threshold=10".into()),
                ..Default::default()
            }),
            limit: None,
        },
    )
    .unwrap();
    dump("step 1 — find_call", &calls);
    assert_eq!(calls.matches.len(), 1);
    let frame_id = calls.matches[0].frame_id;

    // Step 2: trace the variable.
    let history = trace_variable::run(
        &db,
        trace_variable::TraceVariableInput {
            trace_id: TID.into(),
            name: "largest".into(),
            frame_id,
            before_event_id: None,
        },
    )
    .unwrap();
    dump("step 2 — trace_variable(largest)", &history);
    let displays: Vec<&str> = history
        .captures
        .iter()
        .map(|c| c.value.display.as_str())
        .collect();
    assert_eq!(displays, vec!["None", "3", "7", "9", "10"]);

    // Step 3: explain the last change.
    let last_event = history.captures.last().unwrap().event_id;
    let why = why_did_value_change::run(
        &db,
        why_did_value_change::WhyDidValueChangeInput {
            trace_id: TID.into(),
            name: "largest".into(),
            frame_id,
            around_event_id: last_event,
        },
    )
    .unwrap();
    dump("step 3 — why_did_value_change(largest, last)", &why);
    assert_eq!(why.change_event.new_value.display, "10");
    assert_eq!(why.change_event.previous_value.unwrap().display, "9");
    assert!(
        why.preceding_branches
            .iter()
            .any(|b| b.line == 8 && b.taken)
    );

    // Step 4: read the source around the comparison.
    let source = get_source::run(
        &db,
        get_source::GetSourceInput {
            trace_id: TID.into(),
            file_path: "basic.py".into(),
            line_range: Some([5, 12]),
        },
    )
    .unwrap();
    dump("step 4 — get_source(basic.py, 5..=12)", &source);
    assert!(source.content.contains("if item <= threshold"));
}

// ---------------------------------------------------------------------------
// Investigation 2: the recursion redundancy.
//
// User asks: "Is naive Fibonacci as wasteful as it's supposed to be?"
//
// This investigation uses `run_sql` because the questions don't fit typed
// tools — that's the escape hatch's job.
// ---------------------------------------------------------------------------
#[test]
fn investigation_2_recursion_redundancy() {
    let (db, _) = index_to_db(&build_recursion_trace());

    // Step 1: distribution of n values across all fib calls.
    let distribution = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            trace_id: TID.into(),
            query: "SELECT v.int_value AS n, COUNT(*) AS calls FROM frames f \
                    JOIN event_args ea ON f.entry_event_id = ea.event_id \
                    JOIN values v ON ea.value_id = v.value_id \
                    WHERE f.qualified_name = '__main__.fib' AND ea.name = 'n' \
                    GROUP BY n ORDER BY n"
                .into(),
            max_rows: None,
        },
    )
    .unwrap();
    dump("step 1 — run_sql(distribution of n)", &distribution);
    assert!(!distribution.rows.is_empty());

    // Step 2: total fib calls.
    let total = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            trace_id: TID.into(),
            query: "SELECT COUNT(*) AS total FROM frames WHERE qualified_name = '__main__.fib'"
                .into(),
            max_rows: None,
        },
    )
    .unwrap();
    dump("step 2 — run_sql(total fib calls)", &total);
    let total_calls = total.rows[0][0].as_i64().unwrap();
    assert_eq!(total_calls, 5);

    // Step 3: get the call tree.
    let calls = find_call::run(
        &db,
        find_call::FindCallInput {
            trace_id: TID.into(),
            qualified_name: "__main__.main".into(),
            r#where: None,
            limit: Some(1),
        },
    )
    .unwrap();
    let main_frame = calls.matches[0].frame_id;
    let tree = get_call_tree::run(
        &db,
        get_call_tree::GetCallTreeInput {
            trace_id: TID.into(),
            frame_id: main_frame,
            max_depth: None,
            include_args: true,
        },
    )
    .unwrap();
    dump("step 3 — get_call_tree(main)", &tree);
    // main has one child: fib(3); fib(3) has two children fib(2) and fib(1).
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].children.len(), 2);
}

// ---------------------------------------------------------------------------
// Investigation 3: the exception chain.
//
// User asks: "What happened with the ValueError?"
// ---------------------------------------------------------------------------
#[test]
fn investigation_3_exception_chain() {
    let (db, _) = index_to_db(&build_exception_trace());

    // Step 1: find the raise event_id.
    let raises = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            trace_id: TID.into(),
            query: "SELECT event_id, exception_type FROM exceptions ORDER BY event_id LIMIT 5"
                .into(),
            max_rows: None,
        },
    )
    .unwrap();
    dump("step 1 — run_sql(exceptions)", &raises);
    let first_event_id = raises.rows[0][0].as_i64().unwrap();

    // Step 2: walk the propagation chain.
    let chain = exception_chain::run(
        &db,
        exception_chain::ExceptionChainInput {
            trace_id: TID.into(),
            event_id: first_event_id,
        },
    )
    .unwrap();
    dump("step 2 — exception_chain(first raise)", &chain);
    assert_eq!(chain.exception_type, "builtins.ValueError");
    assert_eq!(chain.propagation.len(), 3);
    assert!(chain.ultimately_caught);
    assert_eq!(chain.catching_frame, Some(0));
    // The third frame in the propagation should be the catcher.
    assert_eq!(chain.propagation[2].qualified_name, "__main__.run");
    assert!(chain.propagation[2].caught_at.is_some());
}

// ---------------------------------------------------------------------------
// Investigation 4: the data processing bug.
//
// User asks: "Why is revenue zero in the data_processing example?"
// ---------------------------------------------------------------------------
#[test]
fn investigation_4_data_processing_bug() {
    let (db, _) = index_to_db(&build_data_processing_trace());

    // Step 1: find the call.
    let calls = find_call::run(
        &db,
        find_call::FindCallInput {
            trace_id: TID.into(),
            qualified_name: "__main__.sum_shipped_revenue".into(),
            r#where: None,
            limit: None,
        },
    )
    .unwrap();
    dump("step 1 — find_call(sum_shipped_revenue)", &calls);
    assert_eq!(calls.matches.len(), 1);
    let frame_id = calls.matches[0].frame_id;

    // Step 2: trace `revenue` — should stay 0.0 the whole time.
    let revenue_history = trace_variable::run(
        &db,
        trace_variable::TraceVariableInput {
            trace_id: TID.into(),
            name: "revenue".into(),
            frame_id,
            before_event_id: None,
        },
    )
    .unwrap();
    dump("step 2 — trace_variable(revenue)", &revenue_history);
    for c in &revenue_history.captures {
        assert_eq!(c.value.display, "0");
    }

    // Step 3: per-iteration breakdown of the loop at line 8.
    let iters = find_iterations::run(
        &db,
        find_iterations::FindIterationsInput {
            trace_id: TID.into(),
            frame_id,
            loop_line: 8,
        },
    )
    .unwrap();
    dump("step 3 — find_iterations(loop@8)", &iters);
    assert_eq!(iters.iteration_count, 3);
    // Every iteration should rebind `order` and emit `amount=0` and `counted++`.
    for it in &iters.iterations {
        assert!(it.loop_variables.iter().any(|lv| lv.name == "order"));
    }

    // Step 4: read the buggy source.
    let source = get_source::run(
        &db,
        get_source::GetSourceInput {
            trace_id: TID.into(),
            file_path: "data_processing.py".into(),
            line_range: Some([5, 16]),
        },
    )
    .unwrap();
    dump("step 4 — get_source(data_processing.py)", &source);
    // The typo is the smoking gun.
    assert!(source.content.contains("\"totals\""));

    // Step 5: causal_slice on the (zero) revenue at the end of the loop.
    // Find the last revenue value_id.
    let row = run_sql::run(
        &db,
        run_sql::RunSqlInput {
            trace_id: TID.into(),
            query:
                "SELECT value_id FROM values WHERE type_tag = 'float' AND float_value = 0.0 LIMIT 1"
                    .into(),
            max_rows: None,
        },
    )
    .unwrap();
    let value_id = row.rows[0][0].as_i64().unwrap();
    let slice = causal_slice::run(
        &db,
        causal_slice::CausalSliceInput {
            trace_id: TID.into(),
            value_id,
            max_depth: Some(2),
        },
    )
    .unwrap();
    dump("step 5 — causal_slice(revenue=0.0)", &slice);
    assert_eq!(slice.root_value.display, "0");
}
