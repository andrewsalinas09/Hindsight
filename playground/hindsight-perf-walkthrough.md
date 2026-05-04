# Per-Iteration Performance Analysis from a Single Trace

## What was recorded

A trivial Python function, decorated and run once:

```python
@hindsight.record
def slow_iteration_demo():
    results = []
    for i in range(100):
        if i == 47:
            time.sleep(0.5)
        results.append(i * 2)
    return results
```

The trace captured 100 loop iterations, each with branch outcomes and per-line variable changes. Wall time: 502.6 ms. The agent then performed a per-iteration statistical analysis from the trace.

This document covers the analysis — and a meta-finding: a real performance bug in the Hindsight recorder itself, surfaced by Hindsight recording its own instrumentation overhead.

---

## Headline finding: 99.6% of wall-clock is in one iteration

| metric | value |
|---|---|
| Total function time | 502.6 ms |
| Time spent in iteration 47 alone | 500.4 ms |
| Time across other 99 iterations | 2.2 ms |
| Iter 47 share of total | **99.56%** |

A traditional profiler would tell you "the function took 502 ms and most of it was in the loop." That's true but useless. The trace lets us see that one specific iteration carried essentially the entire cost.

---

## Distribution of per-iteration durations

```
            iters    median     mean      min      max     p95
all 100        —    20.4 μs   5.03 ms   9.2 μs   500.4 ms  29.2 μs
excl. iter 47  99   20.4 μs    22.0 μs   9.2 μs    90.6 μs  29.2 μs
```

The mean/median ratio with iter 47 included is ~250×. That's the textbook signature of an outlier-dominated distribution: any "average iteration cost" computed traditionally (total / count) would be wildly misleading.

---

## The four patterns visible in the timing series

### Pattern 1 — warm-up (iter 0)

Iter 0: **183.4 μs**. Iter 1 onward: ~10 μs. ~18× the steady-state.

This is classic cold-cache / first-execution cost: function loading, attribute caches, first allocations. Not a bug. If `slow_iteration_demo` is called once per request, this is a per-request tax worth knowing about.

### Pattern 2 — the outlier (iter 47)

Iter 47: **500.4 ms**. Every other iteration is 9–91 μs.

This iteration is correlated with a *structural* branch difference, not just a timing anomaly:

- Iters 0–46 and 48–99: the `if i == 47:` check evaluates `taken=false` (the branch on line 11)
- Iter 47 alone: the check evaluates `taken=true`, entering the `time.sleep(0.5)` block on line 12

The trace lets us prove that iteration 47 took a different code path, not just that its work happened to take longer. **Cause, not symptom.**

This is qualitatively different from what a sampling profiler can tell you. A profiler can show "iter 47 was slow"; only the trace can show *why*: a one-shot conditional sent execution into a different branch.

### Pattern 3 — linear drift (the rest of the loop)

Per-iteration cost across the 99 normal iterations climbs roughly linearly:

| iter range | typical duration |
|---|---|
| 1–10 | ~10 μs |
| 11–20 | ~12 μs |
| 21–30 | ~14 μs |
| 31–40 | ~16 μs |
| 41–50 | ~18 μs |
| 51–60 | ~20 μs |
| 61–70 | ~22 μs |
| 71–80 | ~24 μs |
| 81–90 | ~26 μs |
| 91–99 | ~28 μs |

Per-iteration cost is **2.8× higher at the end than at the start**. Slope: ~0.2 μs added per iteration, or ~+2 μs per 10 iterations.

**Initial hypothesis (wrong):** the user's code is doing O(n²) work — each iteration does work proportional to the current size of `results`.

**Verified hypothesis (correct):** the drift is *recorder overhead*, not Python work. See the next section.

### Pattern 4 — allocation spikes

A handful of iterations break the linear trend with isolated spikes:

| iter | duration | likely cause |
|---|---|---|
| 32 | 33.6 μs (vs ~16 μs trend) | GC pause or list resize |
| 50 | 30.7 μs (vs ~20 μs) | similar |
| 64 | 29.6 μs (vs ~22 μs) | similar |
| 88 | 34.5 μs (vs ~26 μs) | likely list-grow resize threshold |
| 95 | **90.6 μs** (vs ~28 μs) | largest non-iter-47 spike — possibly GC sweep |

These follow the pattern of CPython's amortized-O(1) list growth: most appends are cheap, but at certain length thresholds the underlying array is reallocated and copied. A traditional profiler cannot distinguish these from the surrounding loop body.

---

## The meta-finding: a performance bug in the recorder

Looking at the user's code:

```python
for i in range(100):
    if i == 47:
        time.sleep(0.5)
    results.append(i * 2)
```

Each iteration does:
- One integer comparison (`i == 47`)
- One conditional sleep (only at i=47)
- One `list.append` (amortized O(1))
- One integer multiply

That is **O(1) per iteration**. Total work: O(n).

So where is the linear drift coming from?

### Evidence

A query against the indexed trace:

```sql
SELECT type_tag, hash_kind, COUNT(*) FROM values GROUP BY type_tag, hash_kind;
```

| type_tag | hash_kind | count |
|---|---|---|
| list | content | **101** |
| int | content | 150 |

The recorder captured **101 distinct list values**, all with `hash_kind='content'`. These correspond to the per-iteration `loop_variables` captures of `results` — Hindsight is content-hashing (xxhash3-128) the entire `results` list on every iteration's loop-header line event.

Hashing a list of length N is O(N). Doing it 100 times where N grows from 0 to 99 sums to:

```
0 + 1 + 2 + ... + 99 = 4950 element-hashes
```

That is **the source of the O(n²) drift**. It is not in the user's Python code. It is in the recorder's value-capture path.

### Quantification

The slope of the drift is ~0.2 μs per element of list length. At length 99, the per-iteration cost is ~20 μs above the empty-list baseline. That number — ~20 μs to xxhash a 99-element Python list of small integers across the PyO3 boundary — is consistent with the dominant-cost hypothesis.

### Why this fix matters

For a 100-element list this is annoying but tolerable (~2 ms total overhead). For a real workload — a function that builds up a list of length 10,000 — content-hashing on every line event would impose ~50 million element-hashes of overhead, dwarfing the user's actual computation.

The recorder is supposed to be the most performance-sensitive component of Hindsight. Per-CLAUDE.md: *"The recorder is the most performance-sensitive component because it sits in the hot path of the program being recorded."* This bug directly contradicts that goal.

### Fix candidates

1. **Skip content hashing for re-captured mutable containers in the same frame.** If the same Python `id()` was captured previously in this frame, record only the new length and a "mutated" marker. Skip the hash. (Smallest change, probably correct default.)

2. **Identity-based hashing for `loop_variables` captures specifically.** These are by definition repeated captures of the same object across line events. Content hash adds no information — identity + length suffices.

3. **Incremental hashing.** Track the previous hash and fold in only the appended tail. Correct but requires tracking deltas.

4. **Size-thresholded summary.** Switch to `hash_kind='summary'` above some size threshold, losing cross-frame dedup but preserving correctness.

The argument for option 1: content hashing exists for cross-value dedup. A variable that's the same Python object captured at successive line events does not need cross-value dedup — it is *definitionally* a sequence of mutations of one object. The lengths already disambiguate them. The hash is pure overhead.

---

## How the recorder bug was found

The user wrote a trivial-looking function and ran it once with `@hindsight.record`. They asked the AI agent for per-iteration statistics.

The agent reported the four patterns above, including the linear drift, and initially attributed the drift to O(n²) work in the user's code. The user's reaction:

> *"i'm scared it's actually my hindsight code and not the python"*

The agent then read the source file (10 lines), confirmed the user's code was provably O(1) per iteration, and queried the trace's `values` table to identify the actual cause: 101 content-hashed list captures, each O(N) in list length, for total recorder overhead of O(N²).

The whole investigation — including identifying the bug, quantifying the overhead, and proposing fixes — took one conversation against a single trace. The trace was the recording of the recorder recording user code, so the recorder's overhead was visible in its own output.

---

## Why this is a different class of debugging

A traditional profile of `slow_iteration_demo` would report: "loop body, line 13, 502 ms." That's all the granularity sampling can give you, because the loop ran in 502 ms and most samples landed inside it.

To distinguish the four patterns above with conventional tooling, you would need to:

- Add per-iteration timing instrumentation (and re-run)
- Add branch-correlation logging (and re-run)
- Add data-size logging (and re-run)
- Cross-reference across multiple runs because each instrumentation pass is independent

What actually happened: ran the program once, asked an AI agent for the analysis. The trace already had every per-iteration timestamp, every branch outcome, every container-length capture, every value's hash kind. The agent composed structured queries against a frozen artifact and produced the breakdown.

The closest analogue in conventional tooling is `perf` / `eBPF` with custom probes — but those are sampling-based, point-in-time, language-runtime-aware, and would not give you the structural branch correlation or the recorder-overhead diagnosis. None of them speak natural language.

This is not "a faster profiler." It is a different shape of activity. Performance pathology — warm-up, drift, outliers, branch correlation, **and instrumentation-induced artifacts** — becomes a *query* against a recorded artifact, not an investigation that requires re-instrumenting.

For embedded systems where re-running with extra instrumentation may not be possible (real-time constraints, field-deployed, intermittent failures), this shape of analysis is essentially the only feasible way to do per-iteration performance debugging.

And the punchline: a sufficiently introspective tracer can observe its own instrumentation overhead, in the same trace, without a second tool. That's how the recorder bug surfaced. The artifact records what happened; *what happened* includes the recorder being slow.
