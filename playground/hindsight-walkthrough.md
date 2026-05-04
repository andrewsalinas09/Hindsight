# Hindsight: A Real Debugging Session

## TL;DR

Two Python programs were each recorded once. An AI agent connected to the recorded traces over the Model Context Protocol (MCP). Through natural-language conversation, the following were diagnosed:

- **A typo'd elif** that silently zeroed a customer discount tier
- **A silent fallback** in a dict accessor that absorbed a missing-field bug as $0
- **A dead branch** that was evaluated repeatedly but never taken
- **A reporting metric** that systematically lied about whether algorithmic needs were met
- **A units mismatch** in a forecasting function that was understating projected demand by a factor of 7 across every center, causing every rebalancing decision to be confidently wrong

Each finding came from one or two questions asked in English. The agent answered by querying the trace.

What did **not** happen during the investigation:

- No re-runs. Each program executed exactly once.
- No `print` statements, no `logging.debug` calls, no added asserts, no instrumentation of any kind.
- No `pdb`, no IDE breakpoints, no step-through debugging.
- For the second program, no source file was opened directly; every fact came from Hindsight's tools.

This document walks through what was found and how.

---

## Setup

### Programs analyzed

**`examples/test.py`** — A small order-processing pipeline (~170 lines). Seven input orders, five supporting functions. Intentionally seeded with bugs to demonstrate the tool.

**`examples/test_harder.py`** — A multi-region inventory rebalancing system. Six fulfillment centers across three regions, eight supporting functions, EWMA forecasting, safety stock computation, donor selection, and need/donor matching. The bugs in this program were not all intentionally seeded.

### What was recorded

Each program was decorated with `@hindsight.record` on its top-level function and run once. The recorder captured:

- Function entries and exits (with structured argument and return values, not strings)
- Branch outcomes — which way each `if`/`elif` went, with both directions tracked separately
- Per-line variable changes, capturing the exact value at the moment of mutation
- Loop iterations with per-iteration deltas
- Structured `hindsight.note(...)` checkpoints with their kwargs preserved as queryable values

Output: a binary trace (~11 KB and ~18 KB respectively) plus a DuckDB index built lazily on first query.

The agent then answered investigative questions by calling Hindsight's MCP tool surface.

---

## Trace 1: `process_orders`

The user's first question was: *"Can you tell me what the process_orders function did?"*

The agent listed traces, located the root frame, and reconstructed the pipeline behavior from the call tree and variable history. It identified that 7 orders were processed, 5 went through the full pipeline, and 2 were skipped after `is_valid_order` returned `False`. None of this required reading source.

### Finding 1: Two orders skipped, two distinct reasons

**Question:** *"Can you figure out why they got skipped?"*

The agent queried the `branches` table for each rejected `is_valid_order` frame and called `explain_branch` to see the local state at each branching point.

- **Order id=4** was rejected at `if "customer" not in order:`. The order had only two keys (`id` and `items`); no `customer` field.
- **Order id=5** was rejected at `if "items" not in order or not order["items"]:`. All three keys were present, but `items` was an empty list. The condition short-circuited on the second clause.

Two different invalidation paths through the same validator, distinguished without re-running and without reading source.

### Finding 2: The "platnium" typo

**Question:** *"Bob's order was tier=platinum but got no discount. Why?"*

The agent traced the relevant `calculate_discount` call, found that all elif branches evaluated false, and called `explain_branch` on the first failed comparison. The 5-line source window that Hindsight returns alongside each branch event included:

```python
elif customer_tier == "platnium":  # typo — never matches real "platinum"
    return price * 0.20
```

The function fell through every branch and returned `0.0`. Verified by querying the captured return value (type=float, value=0.0). Bob's $500 order was billed at full price.

This is direct evidence of *what the bug did* — the actual return value, captured — not just inference about *what it might do*.

### Finding 3: Dead branch detection

The agent then asked the trace a question that's structurally hard to ask other tools:

```sql
SELECT line, taken, COUNT(*) AS hits
FROM branches WHERE function_name = '__main__.calculate_discount'
GROUP BY line, taken;
```

| line | taken | hits |
|---|---|---|
| 21 | true | 1 |
| 22 | false | **4** |
| 24 | false | 4 |
| 25 | true | 2 |
| 27 | false | 2 |

Line 22 (the `"platnium"` typo) was evaluated four times and **taken=true zero times**. Every other branch had at least one true case.

Coverage tools track line-level execution — they cannot answer "this branch was evaluated but its true side was never taken." The trace can.

### Finding 4: Silent zero-price absorption

The agent identified Carol's order as a candidate for a different bug pattern and called `find_iterations` on the items loop inside `compute_order_total`:

| iteration | item | price captured | quantity |
|---|---|---|---|
| 0 | `{name, price, quantity}` (3 entries) | 200 | 1 |
| 1 | `{name, quantity}` (**2 entries — no `price` key**) | **0** | 3 |

The line `item.get("price", 0.0)` silently absorbed a missing field. Carol's "mystery" item, quantity 3, was billed at zero.

This bug raises no exception, writes no log, and produces no externally visible error. You cannot grep for it. The trace captured the exact moment the silent substitution happened.

### Bonus: Structured note checkpoints

The program embedded `hindsight.note(...)` calls. Their kwargs are stored as typed values, not parsed strings:

| message | kwarg | type | value |
|---|---|---|---|
| processing batch | count | int | 7 |
| batch complete | processed | int | 5 |
| batch complete | revenue | float | 1823.5 |
| batch complete | skipped | int | 2 |

The `revenue=1823.5` matched the per-iteration sum the agent had reconstructed independently — a built-in cross-check between annotated state and reconstructed state.

---

## Trace 2: `rebalance_inventory`

**Question:** *"What did rebalance_inventory do?"*

The agent reconstructed the per-center pipeline from the call tree without ever reading the source file. Six centers were processed, each running through `project_demand` → `determine_target_stock` → `compute_transfer`. Centers were classified as donors or recipients. The output was a single 308-unit transfer (fc-west-02 → fc-west-01).

### Finding 5: The `unmet_needs` reporting bug

**Question:** *"I expected more activity from 6 centers across 3 regions. Is this output reasonable?"*

The output reported `transfer_count=1, units_moved=308, unmet_needs=1`.

The agent's investigation:

1. Queried `match_transfers`'s inputs: 1 need (fc-west-01 needs 308) and 4 donors with 238 / 1127 / 631 / 950 available.
2. Queried `event_locals` inside the `match_transfers` frame. Found `remaining` started at 308 and decremented to **0**. Found the chosen donor's `available` went from 631 to 323 (decremented by exactly 308).
3. Conclusion at this point: the algorithm correctly satisfied the only need. So why does the output say `unmet_needs=1`?
4. Located the generator-expression frame that computed `unmet_needs`. Called `explain_branch` on its branch event. The returned source window:

   ```python
   unmet_needs=sum(1 for n in needs if n["amount"] > 0),
   ```

5. **The bug:** `match_transfers` decrements its donors' `available` field correctly, but it never writes back to the need's `amount` field. The need dict still shows `amount=308` after matching completes. The summary comprehension always counts the original need as unmet, regardless of whether matching succeeded.

This is a *reporting* bug — the algorithm worked correctly, but the metric reporting on it was structurally wrong. Without trace evidence proving the algorithm was correct, the obvious first hypothesis would be that matching itself was broken. The trace let the agent prove the algorithm was right and isolate the bug to the reporting layer.

### Finding 6: The `project_demand` units bug — operationally severe

**Question:** *"Are those projections sensible given the inputs?"*

The agent queried `event_locals` for the `project_demand` frame and reconstructed the full computation for center 0:

| step | value |
|---|---|
| historical input | [120, 135, 128, 140, 132, 138, 145, 150] |
| alpha (smoothing constant) | 0.3 |
| smoothed walk (EWMA) | 120 → 124.5 → 125.55 → 129.885 → 130.52 → 132.76 → 136.43 → **140.504** |
| daily_rate | **20.072** |
| projected (returned) | 281.008 |

The math:
- `smoothed` = EWMA(historical) = 140.504
- `daily_rate` = smoothed / 7 = 20.072
- `projected` = daily_rate × days_ahead = 20.072 × 14 = 281.008

The `/7` step says the function is treating historical inputs as **weekly aggregates**. But the inputs are **daily demand values**.

**Effect: every center's projected demand is understated by a factor of 7.**

| center | current | target (set) | target if daily-correct (~×7) | implied state |
|---|---|---|---|---|
| fc-east-01 | 850 | 612 | ~4280 | severely understocked, **not a donor** |
| fc-east-02 | 1400 | 273 | ~1900 | understocked, not a donor |
| fc-west-01 | 600 | 1008 | ~7050 | catastrophic deficit |
| fc-west-02 | 1100 | 469 | ~3300 | understocked, not a donor |
| fc-central-01 | 300 | 290 | ~2030 | severely understocked |

Under the corrected reading, the "successful" 308-unit transfer didn't move surplus to deficit — it moved stock from one understocked center to another. Every rebalancing decision the system made was confidently wrong.

The agent verified the pattern was systematic across all 5 non-empty centers: every call returned `(EWMA / 7) × days_ahead`. This is not a one-off arithmetic slip — it is a unit-mismatch baked into the function's structure, applied to every center on every run.

**Why this kind of bug is hard to catch otherwise:**

- Tests don't catch it unless they assert on absolute numerical correctness of projections, which most tests don't.
- Code review doesn't catch it unless the reviewer happens to think about units at the right line.
- Users don't catch it until inventory runs out — and even then, the system's reports look internally consistent ("we moved stock to where we needed it"), so the diagnosis points at supply chain rather than the model.

The agent found it in a single conversation, by asking *"are these numbers sensible?"* — possible because the trace captured every intermediate value of the smoothing computation as a structured queryable value.

---

## What was *not* done

This list matters as much as the list of findings.

- **Did not re-run either program.** Both ran exactly once.
- **Did not add any instrumentation.** No prints, no logging, no asserts.
- **Did not start a debugger.** No pdb, no breakpoints, no step-through.
- **Did not open `examples/test_harder.py` directly.** Every fact about that program — its function structure, behavior, intermediate values, bugs — came from Hindsight tools, including the small (5-line) source windows that `explain_branch` returns alongside branch events.
- **Did not replay or run "what-if" scenarios.** Hindsight does not support replay. Every diagnosis was post-hoc against a frozen record.

For `test.py`, the agent did open the source file once, after the bug-finding portion was complete, when the user invited new hypothesis generation. The diagnostic work — the typo, the skip reasons, the silent-zero bug — happened entirely from trace queries before that point.

---

## Tool surface used

The investigation used the following Hindsight MCP tools:

- `list_traces`, `trace_info` — discovery
- `find_call`, `get_call_tree` — frame location and call structure
- `trace_variable` — variable history within a frame
- `find_iterations` — per-iteration loop breakdown
- `explain_branch` — local state and source context at a branch event
- `run_sql` — direct queries against the indexed trace database

The `run_sql` calls covered things the typed tools don't yet expose: resolving raw value IDs to content, expanding container values, scanning all branches in a function, and similar. There is real friction here that more typed tools could remove. Even with this friction, the agent found four distinct bugs across two programs in one conversation.

---

## Why this is a different shape of activity

Conventional debugging is interactive:

> bug → reproduce → instrument → re-run → step through → fix

Each arrow has cost. Reproducing a bug from production is sometimes impossible. Instrumenting requires guessing what to log before knowing what's wrong; the wrong guesses mean another iteration. Stepping through a debugger requires holding the running process in a state where stepping is meaningful — not always available (real-time systems, distributed systems, third-party black boxes).

Hindsight changes the workflow to:

> bug → record once → query → fix

The artifact ships forward. A trace from a customer environment can be analyzed by an AI agent without the customer's environment, source tree, or running stack. A trace from a CI failure can be queried after the runner is gone. A trace from a field-deployed system can be shipped home and diagnosed without physical access.

The agent gains a level of access to past program state that is, in normal debugging, unavailable. Every variable at every line, every branch outcome, every container's contents, every iteration of every loop — all queryable structurally, all without re-running anything.

For the bug classes demonstrated above — silent fallbacks, dead branches, unit mismatches, reporting drift between algorithm and metric — this changes the cost from "expensive instrumented investigation" to "one question." Some of these bugs would never have been found at all by ordinary means; others would have been found only after multiple incidents in production.

The fact that an AI agent can do the querying — rather than a human clicking through a debugger UI — is the multiplier. The agent composes structured queries, follows leads, cross-checks findings against itself, and writes up the result. The user asks a question and gets an answer.

What used to be hours of instrumented re-runs is one sentence and a structured response.

---

## Closing note on maturity

Hindsight is a young project — the recording, indexing, and MCP server were built before this investigation. The Python recorder is the first language frontend; a C++ recorder is planned, with embedded systems as an explicit target.

The session documented above is on toy programs, but the bugs found are realistic — a typo, a silent-default fallback, a units mismatch, a reporting drift. These are the kinds of bugs that survive code review and tests because they don't *crash* — they produce confidently wrong output. The trace caught them in a way that other tools structurally cannot.

A trace-based, AI-queryable record of a program's execution is a different debugging primitive from anything currently in common use. The session above is a small demonstration of what that primitive enables.
