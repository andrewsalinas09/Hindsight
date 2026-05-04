# Hindsight playground

This is a sandbox for trying out Hindsight end-to-end: record a small
Python program, index the trace into DuckDB, then ask SQL questions
about what happened. Each example program is small enough to read in a
minute, and several of them have real bugs that the trace makes obvious
once you know how to query for them.

Treat this as a place to poke around. Nothing here is part of the
shipping product — it's a scratchpad for getting the feel of the tool.

## Prerequisites

- **Python 3.12 or newer.** Hindsight uses `sys.monitoring` (PEP 669),
  which doesn't exist in earlier versions. There is intentionally no
  fallback to `sys.settrace`.
- **Rust toolchain.** [rustup.rs](https://rustup.rs/) installs `cargo`,
  which builds both the Hindsight CLI and the Python extension.
- **Bash.** Git Bash on Windows works; macOS and Linux ship with bash.
- **DuckDB CLI** (optional, recommended). Grab it from
  [duckdb.org/docs/installation](https://duckdb.org/docs/installation/)
  if you want a SQL prompt against indexed traces. The Python `duckdb`
  package gets installed automatically by `setup.sh`, so you can also
  query from a Python REPL if you skip the CLI.

## Setup

From this directory:

```bash
bash setup.sh
```

What that does:

1. Creates a Python 3.12 virtual environment at `.venv/`.
2. Installs `maturin` and `duckdb` into the venv.
3. Builds the Hindsight Rust extension and installs it editable into
   the venv (so `import hindsight` works).
4. Builds the `hindsight` CLI binary (`cargo build --release -p hindsight-cli`).
5. Prints how to activate the venv.

The script is idempotent — re-running it just rebuilds in place. Now
activate the venv:

```bash
# macOS / Linux:
source .venv/bin/activate

# Windows (Git Bash):
source .venv/Scripts/activate
```

You should be able to run `python -c "import hindsight; print(hindsight.__all__)"`
and see `['record', 'skip', 'note', 'TraceWriter', 'read_trace']`.

## The first trace

The simplest thing to try:

```bash
python examples/basic.py
```

You'll see two lines of program output, plus the recorder's
finalization messages:

```
hindsight: trace written to basic.hindsight (NN events)
hindsight: trace written to basic.hindsight (NN events)
clean run: 9
buggy run: 10
```

A note on the trace path: by default the recorder writes to a
timestamped filename like `trace_20260503_201145_123456789.hindsight`
so two recordings can't overwrite each other. The example programs in
this playground each set `HINDSIGHT_OUTPUT_PATH` to a stable per-example
name so the README can reference them directly — `basic.py` →
`basic.hindsight`, `recursion.py` → `recursion.hindsight`, and so on.
The second `@record` call inside `basic.py` *does* overwrite the first
because both calls share the same env var; that's intentional here so
the walkthrough has a known target file. To keep both traces from a
single script, change `setdefault` to a fresh path between calls.

Now index the trace. The CLI binary lives in the workspace's
`target/release/`; either set up an alias or call it by full path:

```bash
../target/release/hindsight index basic.hindsight
```

You'll see:

```
Indexed basic.hindsight → basic.duckdb
```

Now query it. With the DuckDB CLI installed:

```bash
duckdb basic.duckdb
```

That drops you at a SQL prompt. Try one of the queries from
`queries/common.sql` (paste the body, no `--` comments needed):

```sql
SELECT type, COUNT(*) AS n FROM events GROUP BY type ORDER BY n DESC;
```

Expected (your numbers may differ):

```
┌──────────────────┬───────┐
│       type       │   n   │
├──────────────────┼───────┤
│ line_delta       │   23  │
│ branch_result    │   16  │
│ frame_snapshot   │    2  │
│ function_entry   │    2  │
│ function_exit    │    2  │
│ note             │    2  │
└──────────────────┴───────┘
```

If you don't have the DuckDB CLI, do the same query from Python:

```bash
python -c "import duckdb; print(duckdb.connect('basic.duckdb').execute(
  'SELECT type, COUNT(*) FROM events GROUP BY type ORDER BY 2 DESC'
).fetchall())"
```

## A real debugging walkthrough

`examples/basic.py` has a real bug. The function is supposed to return
"the largest value strictly less than the threshold," but a `<=`
escaped into the comparison and the function silently returns
`threshold` itself when it appears in the input. Let's catch it.

Run the program (writes `basic.hindsight`):

```bash
python examples/basic.py
```

Output:

```
clean run: 9
buggy run: 10
```

The "buggy run" returned 10. The threshold was 10, the largest below it
is 9. Something went wrong.

Index and open:

```bash
../target/release/hindsight index basic.hindsight
duckdb basic.duckdb
```

The script calls `find_largest_below` twice with the same
`HINDSIGHT_OUTPUT_PATH`, so the second call's trace overwrites the
first — `basic.hindsight` is the buggy call. `call_index = 0` because
the recorder's frame counter resets per recording session:

```sql
SELECT frame_id, depth, exit_kind, duration_ns, argument_summary
FROM frames
WHERE qualified_name = '__main__.find_largest_below'
  AND call_index = 0;
```

You should see something like `argument_summary = 'values=[<6 items>], threshold=10'`
— that's the call you're looking for.

Note the frame_id (it'll be `0` for this trace). Now look at every
value `largest` ever took in that frame:

```sql
SELECT el.event_id, e.line, v.type_tag, v.int_value
FROM event_locals el
JOIN events e ON el.event_id = e.event_id
JOIN values v ON el.value_id = v.value_id
WHERE el.frame_id = 0
  AND el.name = 'largest'
ORDER BY el.event_id;
```

You'll see `largest` walk up: `None → 3 → 7 → 9 → 10`. The last
update — to 10 — is the bug. The trace also tells you what line that
update happened on. Open `examples/basic.py` and look at that line —
it's the `if item <= threshold:` check that's letting `item == threshold`
slip through.

That's the workflow. Record, index, query, find.

## The other examples

- **`examples/recursion.py`** — naive Fibonacci. No bug, just a clean
  recursive call tree to query. Try the recursive CTE in
  `queries/common.sql` query #7.
- **`examples/data_processing.py`** — sums revenue from a list of
  orders. Has a misspelled key (`"totals"` vs `"total"`) that silently
  returns 0 for each shipped order. The trace's
  `event_locals` rows for `revenue` show it never growing past 0
  even though `counted` does.
- **`examples/exception_demo.py`** — a 3-deep call chain where
  the deepest function raises and the top one catches. Useful for
  seeing how `frames.exit_kind` distinguishes `raised` (frames that
  unwound) from `returned` (the frame that caught it).

For each: same workflow. Record, index, query. The trace filename
matches the example name (`recursion.py` → `recursion.hindsight` →
`recursion.duckdb`).

```bash
python examples/recursion.py
../target/release/hindsight index recursion.hindsight
duckdb recursion.duckdb
```

## The query collection

`queries/common.sql` is a commented catalog of useful queries. Read it
through once — it's the fastest way to get a feel for what the schema
makes easy. The full schema spec lives in `docs/indexer-schema.md` at
the repo root.

## Function names in queries

Functions show up in the trace under their *Python-runtime* qualified
name, which depends on how the program was invoked:

- `python examples/basic.py` → `__name__` is `"__main__"`, so
  `find_largest_below` is recorded as `__main__.find_largest_below`.
- `python -m examples.basic` (with an `__init__.py` in `examples/`) →
  same: the executed module is still `__main__`. The qualname doesn't
  change.
- A function imported from another module — e.g. `from utils import
  helper; helper(...)` from inside the recorded scope — keeps its real
  module path, so it shows up as `utils.helper`.

This isn't the recorder making something up; it's exactly what
`frame.f_globals["__name__"]` returns. The takeaway when you're
filtering: the entry-point script's functions live under `__main__.*`,
imported modules' functions live under their module path. If you're
not sure which to use, query by source path instead — every event and
frame row has `source_file` denormalized in:

```sql
SELECT * FROM frames WHERE source_file LIKE '%basic.py';
SELECT * FROM events WHERE source_file LIKE '%basic.py' AND line = 28;
```

## Tips for writing your own recorded code

- Decorate the entry point you care about with `@hindsight.record`.
  Anything it calls transitively is also recorded, subject to scope.
- Use `hindsight.note("message", **kwargs)` to drop structured
  observations into the trace at runtime. They become rows in the
  `notes` table with their kwargs available via `note_kwargs`.
- Use `with hindsight.skip():` to suppress recording for a block (e.g.
  a tight inner loop you don't care about). That emits a SCOPE_BOUNDARY
  pair into the trace so you know where the gap is.
- Edit `hindsight.toml` to control scope. The `defaults` token in
  `exclude` blocks recording for noisy stdlib / third-party libs;
  it's almost always what you want. See `docs/scope-control.md` for
  the full vocabulary.
- The recorder writes to a unique-per-recording file by default —
  something like `trace_20260503_201145_123456789.hindsight` in the
  current directory. The timestamp tail prevents back-to-back
  recordings from silently overwriting each other. Override with
  `HINDSIGHT_OUTPUT_PATH=foo.hindsight python ...` to pin a stable
  name.

## When something goes wrong

- *"hindsight not found"* after `setup.sh`: the venv isn't active.
  Re-run `source .venv/bin/activate` (or `.venv/Scripts/activate` on
  Windows).
- *Trace file missing after running an example*: check the program
  actually called the `@hindsight.record`-decorated function. The
  recorder only fires inside the decorated scope.
- *Trace too big*: tighten `hindsight.toml` — add specific exclude
  patterns or set `depth_limit` to cap how far recording recurses.
- *DDL error on indexing*: re-run `cargo build --release -p hindsight-cli`
  in case the schema changed since the binary was built.

Have fun.
