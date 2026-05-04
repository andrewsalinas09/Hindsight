// SPDX-License-Identifier: Apache-2.0

//! Test helpers shared across the integration tests. Each fixture builds a
//! synthetic `.hindsight` trace that mirrors one of the playground
//! examples, indexes it, and returns a `(DbConnection, db_path)` pair the
//! tests can drive.

use std::path::PathBuf;

use hindsight_format::{
    Argument, BoundaryType, BranchResult, Change, EXCEPTION_UNWIND_VALUE_ID, ExceptionRaised,
    Finalization, FrameSnapshot, FunctionEntry, FunctionExit, Kwarg, LineDelta, Local, Metadata,
    Note, RecorderInfo, RecordingInfo, ScopeBoundary, ScopeConfig, ScopeResolution, TraceWriter,
    Value,
};
use hindsight_index::Indexer;
use hindsight_mcp::DbConnection;

pub fn metadata(start_ns: u64) -> Metadata {
    Metadata {
        recorder: RecorderInfo {
            language: "python".into(),
            language_version: "3.12.5".into(),
            recorder_version: "0.1.0".into(),
            platform: "test".into(),
        },
        recording: RecordingInfo {
            program: "python demo.py".into(),
            working_directory: Some("/tmp/proj".into()),
            scope_config: ScopeConfig {
                include: vec![],
                exclude: vec!["defaults".into()],
                depth_limit: None,
            },
        },
        program: None,
        trace_uuid: [0xCD; 16],
        recording_start_ns: start_ns,
    }
}

pub fn finalize(end_ns: u64) -> Finalization {
    Finalization {
        recording_end_ns: end_ns,
        scope_resolution: ScopeResolution {
            recorded_functions: vec![],
            excluded_functions: vec![],
            skip_blocks_observed: 0,
            depth_clips_observed: 0,
        },
    }
}

static SUFFIX_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn write_tmp(bytes: &[u8], suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let counter = SUFFIX_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let unique = format!(
        "hindsight-mcp-test-{}-{}-{}-{}",
        pid,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        counter,
        suffix
    );
    let path = dir.join(unique);
    std::fs::write(&path, bytes).unwrap();
    path
}

pub fn index_to_db(trace_bytes: &[u8]) -> (DbConnection, PathBuf) {
    let trace = write_tmp(trace_bytes, ".hindsight");
    let db = write_tmp(b"", ".duckdb");
    Indexer::index(&trace, &db).unwrap();
    let conn = DbConnection::open(db.clone()).unwrap();
    (conn, db)
}

// ---------------------------------------------------------------------------
// Fixture: basic.py — find_largest_below with the off-by-one bug.
//
// Single recorded frame. The function takes (values, threshold) and runs
// the loop body. We seed enough events that:
//   - largest goes None → 3 → 7 → 9 → 10 (the bug)
//   - branches at line 36 (`if item <= threshold:`) and 37 (`if largest...`)
//     fire on each iteration with appropriate `taken` values.
// ---------------------------------------------------------------------------
pub fn build_basic_trace() -> Vec<u8> {
    const SRC: &str = "\
# basic.py
import os

@hindsight.record
def find_largest_below(values, threshold):
    largest = None
    for item in values:
        if item <= threshold:
            if largest is None or item > largest:
                largest = item
    hindsight.note(\"scan complete\", checked=len(values), winner=largest)
    return largest
";

    let mut w = TraceWriter::new(metadata(1_000));
    let fid = w.add_source_file("basic.py", SRC.as_bytes().to_vec());
    let func_id = w.intern_string("__main__.find_largest_below");

    let none_v = w.intern_value_inline(Value::None);
    let int_3 = w.intern_value_inline(Value::Int(3));
    let int_7 = w.intern_value_inline(Value::Int(7));
    let int_1 = w.intern_value_inline(Value::Int(1));
    let int_9 = w.intern_value_inline(Value::Int(9));
    let int_4 = w.intern_value_inline(Value::Int(4));
    let int_10 = w.intern_value_inline(Value::Int(10));
    let int_threshold = int_10;
    let int_6 = w.intern_value_inline(Value::Int(6));

    let values_list =
        w.intern_value_inline(Value::List(vec![int_3, int_7, int_1, int_9, int_4, int_10]));

    let s_values = w.intern_string("values");
    let s_threshold = w.intern_string("threshold");
    let s_largest = w.intern_string("largest");
    let s_item = w.intern_string("item");
    let s_msg = w.intern_string("scan complete");
    let s_checked = w.intern_string("checked");
    let s_winner = w.intern_string("winner");

    // FUNCTION_ENTRY at line 5.
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 5,
        args: vec![
            Argument {
                name: s_values,
                value: values_list,
            },
            Argument {
                name: s_threshold,
                value: int_threshold,
            },
        ],
    })
    .unwrap();

    // FRAME_SNAPSHOT establishing initial locals at line 6 (`largest = None`).
    w.write_frame_snapshot(FrameSnapshot {
        timestamp_delta_ns: 1,
        frame_id: 0,
        line: 6,
        locals: vec![
            Local {
                name: s_values,
                value: values_list,
            },
            Local {
                name: s_threshold,
                value: int_threshold,
            },
            Local {
                name: s_largest,
                value: none_v,
            },
        ],
    })
    .unwrap();

    // Helper to script one iteration.
    let emit_iter = |w: &mut TraceWriter,
                     item_v: u64,
                     branch_a_taken: bool,
                     branch_b_taken: bool,
                     new_largest: Option<u64>| {
        // LINE_DELTA on the loop header (line 7) — captures `item`.
        w.write_line_delta(LineDelta {
            timestamp_delta_ns: 1,
            line: 7,
            changes: vec![Change {
                name: s_item,
                value: item_v,
            }],
        })
        .unwrap();
        // BRANCH at line 8 — `if item <= threshold:`.
        w.write_branch_result(BranchResult {
            timestamp_delta_ns: 1,
            line: 8,
            taken: branch_a_taken,
        })
        .unwrap();
        if branch_a_taken {
            // BRANCH at line 9 — `if largest is None or item > largest:`.
            w.write_branch_result(BranchResult {
                timestamp_delta_ns: 1,
                line: 9,
                taken: branch_b_taken,
            })
            .unwrap();
            if branch_b_taken {
                // LINE_DELTA assigning largest = item at line 10.
                if let Some(v) = new_largest {
                    w.write_line_delta(LineDelta {
                        timestamp_delta_ns: 1,
                        line: 10,
                        changes: vec![Change {
                            name: s_largest,
                            value: v,
                        }],
                    })
                    .unwrap();
                }
            }
        }
    };

    emit_iter(&mut w, int_3, true, true, Some(int_3)); // 3
    emit_iter(&mut w, int_7, true, true, Some(int_7)); // 7
    emit_iter(&mut w, int_1, true, false, None); // 1 — not greater
    emit_iter(&mut w, int_9, true, true, Some(int_9)); // 9
    emit_iter(&mut w, int_4, true, false, None); // 4
    emit_iter(&mut w, int_10, true, true, Some(int_10)); // 10 (the bug!)

    // NOTE at line 11.
    w.write_note(Note {
        timestamp_delta_ns: 1,
        line: 11,
        message: s_msg,
        kwargs: vec![
            Kwarg {
                name: s_checked,
                value: int_6,
            },
            Kwarg {
                name: s_winner,
                value: int_10,
            },
        ],
    })
    .unwrap();

    // RETURN at line 12 returning largest=10.
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: int_10,
    })
    .unwrap();

    w.finish_to_bytes(finalize(50_000)).unwrap()
}

// ---------------------------------------------------------------------------
// Fixture: recursion.py — naive Fibonacci, fib(3).
//
// We hand-build the recursive call tree: main(target=3) -> fib(3) -> fib(2) -> fib(1)/fib(0)
//                                                                  -> fib(1)
// Five fib activations + one main activation.
// ---------------------------------------------------------------------------
pub fn build_recursion_trace() -> Vec<u8> {
    const SRC: &str = "\
# recursion.py
import hindsight

def fib(n):
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

@hindsight.record
def main(target):
    hindsight.note(\"starting fib\", n=target)
    result = fib(target)
    hindsight.note(\"fib finished\", n=target, value=result)
    return result
";

    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("recursion.py", SRC.as_bytes().to_vec());

    let s_main = w.intern_string("__main__.main");
    let s_fib = w.intern_string("__main__.fib");
    let s_n = w.intern_string("n");
    let s_target = w.intern_string("target");
    let s_result = w.intern_string("result");
    let s_value_kw = w.intern_string("value");
    let s_starting = w.intern_string("starting fib");
    let s_finished = w.intern_string("fib finished");

    let int_0 = w.intern_value_inline(Value::Int(0));
    let int_1 = w.intern_value_inline(Value::Int(1));
    let int_2 = w.intern_value_inline(Value::Int(2));
    let int_3 = w.intern_value_inline(Value::Int(3));

    // main(target=3) — frame 0
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: s_main,
        source_file_id: fid,
        line: 10,
        args: vec![Argument {
            name: s_target,
            value: int_3,
        }],
    })
    .unwrap();
    // hindsight.note(starting fib, n=3) at line 11
    w.write_note(Note {
        timestamp_delta_ns: 1,
        line: 11,
        message: s_starting,
        kwargs: vec![Kwarg {
            name: s_n,
            value: int_3,
        }],
    })
    .unwrap();

    // fib(3) — frame 1
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 1,
        frame_id: 1,
        function_id: s_fib,
        source_file_id: fid,
        line: 4,
        args: vec![Argument {
            name: s_n,
            value: int_3,
        }],
    })
    .unwrap();
    w.write_branch_result(BranchResult {
        timestamp_delta_ns: 1,
        line: 5,
        taken: false,
    })
    .unwrap();

    // fib(2) — frame 2
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 1,
        frame_id: 2,
        function_id: s_fib,
        source_file_id: fid,
        line: 4,
        args: vec![Argument {
            name: s_n,
            value: int_2,
        }],
    })
    .unwrap();
    w.write_branch_result(BranchResult {
        timestamp_delta_ns: 1,
        line: 5,
        taken: false,
    })
    .unwrap();

    // fib(1) — frame 3
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 1,
        frame_id: 3,
        function_id: s_fib,
        source_file_id: fid,
        line: 4,
        args: vec![Argument {
            name: s_n,
            value: int_1,
        }],
    })
    .unwrap();
    w.write_branch_result(BranchResult {
        timestamp_delta_ns: 1,
        line: 5,
        taken: true,
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 3,
        return_value: int_1,
    })
    .unwrap();

    // fib(0) — frame 4
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 1,
        frame_id: 4,
        function_id: s_fib,
        source_file_id: fid,
        line: 4,
        args: vec![Argument {
            name: s_n,
            value: int_0,
        }],
    })
    .unwrap();
    w.write_branch_result(BranchResult {
        timestamp_delta_ns: 1,
        line: 5,
        taken: true,
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 4,
        return_value: int_0,
    })
    .unwrap();

    // fib(2) returns
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 2,
        return_value: int_1,
    })
    .unwrap();

    // fib(1) — frame 5 (second arg)
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 1,
        frame_id: 5,
        function_id: s_fib,
        source_file_id: fid,
        line: 4,
        args: vec![Argument {
            name: s_n,
            value: int_1,
        }],
    })
    .unwrap();
    w.write_branch_result(BranchResult {
        timestamp_delta_ns: 1,
        line: 5,
        taken: true,
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 5,
        return_value: int_1,
    })
    .unwrap();

    // fib(3) returns 2
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 1,
        return_value: int_2,
    })
    .unwrap();

    // result = 2 (back in main)
    w.write_line_delta(LineDelta {
        timestamp_delta_ns: 1,
        line: 12,
        changes: vec![Change {
            name: s_result,
            value: int_2,
        }],
    })
    .unwrap();
    w.write_note(Note {
        timestamp_delta_ns: 1,
        line: 13,
        message: s_finished,
        kwargs: vec![
            Kwarg {
                name: s_n,
                value: int_3,
            },
            Kwarg {
                name: s_value_kw,
                value: int_2,
            },
        ],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: int_2,
    })
    .unwrap();

    w.finish_to_bytes(finalize(50_000)).unwrap()
}

// ---------------------------------------------------------------------------
// Fixture: exception_demo.py — ValueError propagating two frames.
//
// Frames:
//   frame 0: run(records=[{...}])   - catches the exception
//   frame 1: validate_and_parse(record={...}) - lets it through
//   frame 2: parse_age(raw="not-a-number") - raises ValueError
// ---------------------------------------------------------------------------
pub fn build_exception_trace() -> Vec<u8> {
    const SRC: &str = "\
# exception_demo.py
import hindsight

def parse_age(raw):
    n = int(raw)
    if n < 0 or n > 200:
        raise ValueError(f\"age out of plausible range: {n}\")
    return n

def validate_and_parse(record):
    name = record.get(\"name\", \"<unknown>\")
    age = parse_age(record[\"age\"])
    return {\"name\": name, \"age\": age}

@hindsight.record
def run(records):
    parsed = []
    for record in records:
        try:
            parsed.append(validate_and_parse(record))
        except ValueError as e:
            hindsight.note(\"skipped invalid record\")
            continue
    return parsed
";

    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("exception_demo.py", SRC.as_bytes().to_vec());

    let s_run = w.intern_string("__main__.run");
    let s_validate = w.intern_string("__main__.validate_and_parse");
    let s_parse_age = w.intern_string("__main__.parse_age");
    let s_records = w.intern_string("records");
    let s_record = w.intern_string("record");
    let s_raw = w.intern_string("raw");
    let s_parsed = w.intern_string("parsed");
    let s_value_error = w.intern_string("builtins.ValueError");
    let exc_type_name = w.intern_string("ValueError");
    let exc_repr = w.intern_string("ValueError('age out of plausible range: not-a-number')");
    let s_skipped = w.intern_string("skipped invalid record");

    let none_v = w.intern_value_inline(Value::None);
    let _ = none_v;
    let raw_str = w.intern_value_inline(Value::String("not-a-number".to_string()));
    let records_list = w.intern_value_inline(Value::List(vec![raw_str]));
    let record_dict = w.intern_value_inline(Value::Dict(vec![(raw_str, raw_str)]));
    let _ = record_dict;
    let parsed_empty = w.intern_value_inline(Value::List(vec![]));
    let exc_value = w.intern_value_summary(exc_type_name, 0, exc_repr).unwrap();

    // frame 0: run
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: s_run,
        source_file_id: fid,
        line: 16,
        args: vec![Argument {
            name: s_records,
            value: records_list,
        }],
    })
    .unwrap();
    w.write_line_delta(LineDelta {
        timestamp_delta_ns: 1,
        line: 17,
        changes: vec![Change {
            name: s_parsed,
            value: parsed_empty,
        }],
    })
    .unwrap();
    w.write_line_delta(LineDelta {
        timestamp_delta_ns: 1,
        line: 18,
        changes: vec![Change {
            name: s_record,
            value: raw_str,
        }],
    })
    .unwrap();

    // frame 1: validate_and_parse
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 1,
        frame_id: 1,
        function_id: s_validate,
        source_file_id: fid,
        line: 10,
        args: vec![Argument {
            name: s_record,
            value: raw_str,
        }],
    })
    .unwrap();

    // frame 2: parse_age
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 1,
        frame_id: 2,
        function_id: s_parse_age,
        source_file_id: fid,
        line: 4,
        args: vec![Argument {
            name: s_raw,
            value: raw_str,
        }],
    })
    .unwrap();
    w.write_exception_raised(ExceptionRaised {
        timestamp_delta_ns: 1,
        line: 5,
        exception_type: s_value_error,
        exception_value: exc_value,
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 2,
        return_value: EXCEPTION_UNWIND_VALUE_ID,
    })
    .unwrap();

    // Propagation: validate_and_parse re-raises (same value_id).
    w.write_exception_raised(ExceptionRaised {
        timestamp_delta_ns: 1,
        line: 12,
        exception_type: s_value_error,
        exception_value: exc_value,
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 1,
        return_value: EXCEPTION_UNWIND_VALUE_ID,
    })
    .unwrap();

    // Propagation observed in run, then caught.
    w.write_exception_raised(ExceptionRaised {
        timestamp_delta_ns: 1,
        line: 20,
        exception_type: s_value_error,
        exception_value: exc_value,
    })
    .unwrap();
    // Recovery line — `except ValueError as e:` at line 21
    w.write_line_delta(LineDelta {
        timestamp_delta_ns: 1,
        line: 21,
        changes: vec![],
    })
    .unwrap();
    w.write_note(Note {
        timestamp_delta_ns: 1,
        line: 22,
        message: s_skipped,
        kwargs: vec![],
    })
    .unwrap();
    // run returns parsed (still empty)
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: parsed_empty,
    })
    .unwrap();

    w.finish_to_bytes(finalize(20_000)).unwrap()
}

// ---------------------------------------------------------------------------
// Fixture: data_processing.py — sum_shipped_revenue with the typo bug.
//
// One frame iterating four orders. The bug: `order["totals"]` typo means
// the `.get(...)` returns None, so `revenue += None or 0.0` keeps revenue
// at 0.0 even though shipped orders are seen.
// ---------------------------------------------------------------------------
pub fn build_data_processing_trace() -> Vec<u8> {
    const SRC: &str = "\
# data_processing.py
import hindsight

@hindsight.record
def sum_shipped_revenue(orders):
    revenue = 0.0
    counted = 0
    for order in orders:
        if not is_shipped(order):
            continue
        amount = order.get(\"totals\") or 0.0
        revenue += amount
        counted += 1
    hindsight.note(\"done\", revenue=revenue)
    return revenue
";

    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("data_processing.py", SRC.as_bytes().to_vec());
    let s_func = w.intern_string("__main__.sum_shipped_revenue");

    let s_orders = w.intern_string("orders");
    let s_revenue = w.intern_string("revenue");
    let s_counted = w.intern_string("counted");
    let s_order = w.intern_string("order");
    let s_amount = w.intern_string("amount");
    let s_done = w.intern_string("done");
    let s_revenue_kw = w.intern_string("revenue");

    let zero_f = w.intern_value_inline(Value::Float(0.0));
    let int_0 = w.intern_value_inline(Value::Int(0));
    let int_1 = w.intern_value_inline(Value::Int(1));
    let int_2 = w.intern_value_inline(Value::Int(2));
    let int_3 = w.intern_value_inline(Value::Int(3));

    let order1 = w.intern_value_inline(Value::Dict(vec![]));
    let order2 = w.intern_value_inline(Value::Dict(vec![(int_2, int_2)]));
    let order3 = w.intern_value_inline(Value::Dict(vec![(int_3, int_3)]));
    let orders_list = w.intern_value_inline(Value::List(vec![order1, order2, order3]));

    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: s_func,
        source_file_id: fid,
        line: 5,
        args: vec![Argument {
            name: s_orders,
            value: orders_list,
        }],
    })
    .unwrap();
    w.write_frame_snapshot(FrameSnapshot {
        timestamp_delta_ns: 1,
        frame_id: 0,
        line: 6,
        locals: vec![
            Local {
                name: s_orders,
                value: orders_list,
            },
            Local {
                name: s_revenue,
                value: zero_f,
            },
            Local {
                name: s_counted,
                value: int_0,
            },
        ],
    })
    .unwrap();

    let iter = |w: &mut TraceWriter, order_v: u64, shipped: bool, counted_after: u64| {
        // Loop header (line 8).
        w.write_line_delta(LineDelta {
            timestamp_delta_ns: 1,
            line: 8,
            changes: vec![Change {
                name: s_order,
                value: order_v,
            }],
        })
        .unwrap();
        // Branch on `if not is_shipped(order):`
        w.write_branch_result(BranchResult {
            timestamp_delta_ns: 1,
            line: 9,
            taken: !shipped, // the `not` branch is taken when not shipped
        })
        .unwrap();
        if shipped {
            // amount = order.get(...) or 0.0  → 0.0 because of typo
            w.write_line_delta(LineDelta {
                timestamp_delta_ns: 1,
                line: 11,
                changes: vec![Change {
                    name: s_amount,
                    value: zero_f,
                }],
            })
            .unwrap();
            // revenue += amount  → still 0.0
            // (revenue doesn't actually change, but Python may emit a
            // LINE_DELTA noting the same value; for testing we omit it
            // so the data_processing test detects "no revenue change"
            // exactly.)
            // counted += 1
            w.write_line_delta(LineDelta {
                timestamp_delta_ns: 1,
                line: 13,
                changes: vec![Change {
                    name: s_counted,
                    value: counted_after,
                }],
            })
            .unwrap();
        }
    };

    iter(&mut w, order1, true, int_1);
    iter(&mut w, order2, true, int_2);
    iter(&mut w, order3, true, int_3);

    w.write_note(Note {
        timestamp_delta_ns: 1,
        line: 14,
        message: s_done,
        kwargs: vec![Kwarg {
            name: s_revenue_kw,
            value: zero_f,
        }],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: zero_f,
    })
    .unwrap();

    w.finish_to_bytes(finalize(20_000)).unwrap()
}

// ---------------------------------------------------------------------------
// Trace builder used for tool registration / smoke tests. A trivial trace
// with one frame and two events.
// ---------------------------------------------------------------------------
#[allow(dead_code)]
pub fn build_minimal_trace() -> Vec<u8> {
    const SRC: &str = "def demo(): return 1\n";
    let mut w = TraceWriter::new(metadata(0));
    let fid = w.add_source_file("demo.py", SRC.as_bytes().to_vec());
    let func_id = w.intern_string("demo.demo");
    let one = w.intern_value_inline(Value::Int(1));
    w.write_function_entry(FunctionEntry {
        timestamp_delta_ns: 0,
        frame_id: 0,
        function_id: func_id,
        source_file_id: fid,
        line: 1,
        args: vec![],
    })
    .unwrap();
    w.write_function_exit(FunctionExit {
        timestamp_delta_ns: 1,
        frame_id: 0,
        return_value: one,
    })
    .unwrap();
    w.finish_to_bytes(finalize(10)).unwrap()
}

// Avoid dead-code warnings for unused fixtures in any specific test file.
#[allow(dead_code)]
pub fn _unused() {
    let _ = ScopeBoundary {
        timestamp_delta_ns: 0,
        boundary_type: BoundaryType::EnteredSkip,
        reason: 0,
    };
}
