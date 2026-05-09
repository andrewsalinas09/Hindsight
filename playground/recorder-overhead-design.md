# Recorder Overhead: Diagnosis, Failed Solutions, Proposed Design

## Background (so this brief stands alone)

Hindsight is an AI-native debugger. A Python recorder uses `sys.monitoring` to capture program execution into a binary trace; an indexer builds a DuckDB view over the trace; an MCP server exposes typed tools that an AI agent uses to query the trace. The full architecture is in `ARCHITECTURE.md` and the trace format is documented in `docs/trace-format.md`.

The recorder is described in CLAUDE.md as "the most performance-sensitive component because it sits in the hot path of the program being recorded." Everything else is run-once-after-the-fact and has slack.

## The problem

The recorder's per-capture cost grows with the size of captured containers. For mutable containers re-captured at every line event in a frame (e.g., a list being appended to inside a loop), the work compounds quadratically.

### Empirical demonstration

A trivial Python function was decorated and run once:

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

The user's actual code is O(1) per iteration (one append, one comparison, one occasional sleep). Total user work is O(N).

The trace's per-iteration timings showed a steady linear drift: ~10 μs at iter 1, ~28 μs at iter 99. The slope is ~0.2 μs added per element of `results` length.

### Root cause, verified from the trace

Querying the trace's `values` table:

```sql
SELECT type_tag, hash_kind, COUNT(*) FROM values GROUP BY type_tag, hash_kind;
```

| type_tag | hash_kind | count |
|---|---|---|
| list | content | **101** |
| int | content | 150 |

The recorder content-hashes (xxhash3-128) the entire `results` list at every per-iteration capture. 101 distinct list snapshots, every one fully content-hashed.

Hashing a list of length N is O(N). Doing it 100 times where N grows 0 → 99 sums to:

```
0 + 1 + 2 + ... + 99 = 4950 element-hashes
```

That is the source of the linear drift. **The drift is recorder overhead, not user code.**

### Scaling

| N (captures of a growing list of length up to N-1) | recorder overhead |
|---|---|
| 100 | 1 ms |
| 1,000 | 0.1 sec |
| 10,000 | 10 sec |
| 100,000 | 17 minutes |
| 1,000,000 | **~28 hours** |

The slope estimate could be off by 2× in either direction; the order of magnitude is solid. At realistic workload sizes (any function that builds a response list, processes a batch, accumulates results), the recorder is non-viable.

This contradicts the recorder's mandate to be the cheap component.

## Solutions considered, and why they don't work

### Option 1: Per-frame identity cache. Skip rehashing for re-captured objects.

Maintain `HashMap<id(obj), value_id>` per frame. On capture, if the object's Python `id()` was seen previously in this frame, reuse the value_id rather than recompute the hash.

**Why it doesn't work as the full answer:**

- Loses the captured *state* of the object across line events. Two captures of the same `id()` would share a value_id but the snapshots would be of different content (the list has grown). The trace would show the variable as "unchanged" between line events when in fact it grew.
- Unless you couple identity caching with a versioning scheme that emits new value_ids on detected change, you've turned a perf bug into a correctness bug.

This is a useful primitive but not a full solution.

### Option 2: Streaming / incremental hashing.

Use xxhash3's streaming API. Maintain hash state per object id; on re-capture, feed in only the elements appended since the last capture. O(Δ) per capture instead of O(N).

**Why it doesn't work cleanly:**

- Streaming hashes only support *suffix-only* mutations (append, extend). For arbitrary mutations (in-place index assignment, sort, reverse, delete), you have to rehash from scratch.
- Detecting "append-only" requires either trusting the user or verifying. Verification (comparing previous prefix to current prefix) costs O(prefix-size) — the very work we're trying to avoid.
- A lightweight verification scheme (store first/last K element hashes from previous capture, compare on re-capture) costs O(K) per re-capture and catches most non-append mutations, but is a partial solution with a documented edge case.

**Why this is interesting but not the answer:** it preserves cross-frame content dedup (which Option 1 loses), and it's the cleanest fix that keeps content hashing semantically intact. But it's complex to implement correctly, has subtle failure modes, and doesn't actually answer the deeper question of *why we're hashing at all in the recorder*.

### Option 3: User-supplied "append-only" hints.

API like `@hindsight.record(append_only=["results"])`. The recorder trusts the hint and uses the streaming-update fast path.

**Why this is actively dangerous:**

- The user is right ~95% of the time. The 5% they're wrong is exactly the case worth catching — a mutation bug. If the recorder trusts the hint, it produces a stable value_id for what is actually a mutating object. The trace looks consistent. The mutation bug hides. The user spends a day chasing the wrong layer.
- This inverts Hindsight's value proposition. The whole point is "we capture what really happened, not what you think happened." A trust-the-user knob undoes that on the exact code path most likely to have a bug worth catching.
- Hints accumulate. Once you ship `append_only`, someone will ask for `immutable`, then `pure`, then `no_side_effects`. Each is a gun pointed at the user.

**A safer variant:** a *checked* hint, where the recorder still verifies on each capture and emits a `note` event in the trace when the hint is violated. This turns a wrong hint into a *signal* (helping find the very bug it would otherwise hide). Still adds API surface for marginal benefit. Probably skip for v0.

### Option 4: Two-mode design (`=debug` for accuracy, `=perf` for speed).

Accept that there's a tension between fidelity and speed. Offer two profiles. `debug` content-hashes everything; `perf` uses identity-only summaries.

**Why this is the wrong shape:**

- Doesn't address the root issue, which is that the recorder is paying CPU for a feature (cross-value dedup) that mostly serves storage and post-hoc query convenience.
- Fragments the trace ecosystem. A trace recorded in `perf` mode behaves differently for some MCP queries; tools have to know the mode and adapt. Two test matrices.
- "Default = debug" means most users hit the slow path. "Default = perf" means most users get degraded fidelity. There's no good default.
- Adds a knob users have to know about. CLAUDE.md is explicit: don't add features without explicit discussion, and resist the temptation to add knobs.

If we did this, we'd be papering over a deeper architectural mis-allocation rather than fixing it.

## The actual question: why are we hashing in the recorder at all?

Stepping back to first principles. Content hashing in the recorder serves two purposes:

1. **Cross-event dedup** — if the same value is captured at multiple events, share storage by reusing the `value_id`.
2. **Cross-frame content identity** — "where else did this exact value appear?" queries can join on `value_id`.

Both are optimizations. Neither is fundamental to the debugging value of a trace.

- Dedup is a *storage* optimization. The recorder is paying CPU during the program's hot path to save bytes on disk. That's the wrong trade. CPU at record time is precious; disk is cheap; gzip exists.
- Cross-frame content queries are a *query-time* feature. They don't need to be answered at record time. The indexer (post-hoc, has slack) can compute content hashes once and build a secondary index for any cross-value query that needs it. Or the query layer can compare contents directly on demand.

The recorder is being asked to do work that belongs to the indexer. Moving the work where the slack is dissolves the entire problem.

## Proposed design

### Principle

**The recorder does not content-hash.** Period. It writes structural events with monotonically-assigned value_ids. The indexer (cold path) is responsible for any dedup or content-based identity that downstream tools need.

### Recorder per-capture behavior

```
fn capture(&mut self, obj: PyObject) -> ValueId {
    let value_id = self.next_value_id();           // monotonic counter, O(1)
    match type_of(obj) {
        Scalar(t) => {
            // int, float, bool, str (capped), bytes (capped), none
            // Just record the bit pattern or capped repr.
            // O(1) for fixed-size scalars, O(cap) for variable-size.
            self.write_scalar(value_id, t, obj);
        }
        Container(t) => {
            // list, dict, set, tuple
            let n = len(obj);
            self.write_container_header(value_id, t, n);
            
            if let Some(prev) = self.frame_cache.get(&id(obj)) {
                // Re-capture in the same frame.
                if n == prev.last_length {
                    // Length unchanged: emit a reference to the previous snapshot.
                    // O(1). Documented limitation: same-length in-place mutations 
                    // not re-reflected here.
                    self.write_value_alias(value_id, prev.value_id);
                } else if n > prev.last_length {
                    // Length grew: copy element references for the unchanged prefix
                    // by reading from prev's value_elements; capture only the new tail.
                    // O(n - prev.last_length) typical = O(1) for one-at-a-time append.
                    self.copy_prefix_elements(value_id, prev.value_id, prev.last_length);
                    self.capture_tail(value_id, obj, prev.last_length, n);
                } else {
                    // Length shrunk or same-length-different-content: full re-walk.
                    // O(n). Rare in practice.
                    self.capture_full_container(value_id, obj);
                }
            } else {
                // First capture of this object identity in this frame.
                // O(n) walk over elements, but each element is registered as its 
                // own value_id with no hashing — just a map insert and a row write.
                self.capture_full_container(value_id, obj);
            }
            
            self.frame_cache.insert(id(obj), CacheEntry { value_id, last_length: n });
        }
    }
    value_id
}
```

### Per-capture cost

| case | cost |
|---|---|
| Scalar | O(1) |
| Container, first-seen, length n | O(n) — element registrations only, no hashing |
| Container, re-capture, length unchanged | O(1) |
| Container, re-capture, length grew by k | O(k) |
| Container, re-capture, length shrunk or same-length mutation | O(n) |

For the perf.py demonstration (list growing 0 → 99 over 100 captures), per-capture cost is O(0) + O(1) × 99 = O(N) total. At N=10^6, the recorder runs in milliseconds, not 28 hours.

### Trace format implications

The current trace format spec treats `value_id` as content-derived. That assumption needs to relax to "value_id is unique per capture; content equality across value_ids is determined post-hoc by the indexer."

Concrete spec changes:
- `value_id` becomes a monotonic counter, scoped to the trace.
- The `values` table may contain duplicate content rows; this is expected and not an error.
- A new "value_alias" relation lets re-captures of unchanged-length containers reference a previous snapshot's value_elements rather than duplicating them.
- Per CLAUDE.md, format changes require a version bump and updating `docs/trace-format.md` *first*, then code.

### What the indexer does

After recording (or lazily on first query that needs it), the indexer can compute a content hash for any value and populate a secondary index. This is a single pass over the values table, runs once per trace, and is cold-path work. Tools that join on content identity (e.g., "find all events with the same exception value as this one") use the secondary index.

For traces where no query ever asks for content identity, the indexer never bothers. Pay-as-you-go.

### Limitations and where they apply

The "length-unchanged → emit alias" branch will not re-reflect in-place mutations of fixed-length containers (e.g., `lst[3] = x`, `lst.sort()`, `dict.update` that overwrites existing keys without growing).

Mitigations:
1. The mutation *line itself* still fires a line event. The variable's `value_id` at that line event is captured; if the recorder takes a fresh snapshot at any line where a mutation likely occurred (assignment to a container element), the change is captured at the mutation site even if subsequent line events reuse the alias.
2. Future work: opcode-level mutation detection via `sys.monitoring`'s `STORE_SUBSCR`, `LIST_APPEND`, etc. Captures mutations as they happen, not by polling. Probably v1, not v0.
3. Documented: a user reading a trace should know that "this variable looks unchanged at line N+1" means "the recorder did not detect a change between line N and line N+1," which is true under length-based detection. The convention is correct, just not omniscient.

### Storage cost

Trace files will grow because the recorder no longer dedups. Estimated impact:
- For traces dominated by primitive scalars, growth is small (most scalars don't dedup anyway in CPython).
- For traces with many repeated structural values (config dicts, exception singletons), growth is meaningful.
- Mitigation: post-hoc compression. The trace file can be gzipped. The indexer's content-hash secondary index recovers logical dedup at query time.

This is the right trade. CPU at record time is the constrained resource; storage and indexer time are not.

## What this unblocks

- Recorder is O(1) per capture in the common case.
- No mode flags. No `=debug` vs `=perf` API. The recorder is fast in all cases.
- No user-supplied hints. No correctness footguns.
- Code is simpler — one capture path, no per-element content hashing, no per-frame identity-cache-with-versioning gymnastics.
- Demos work on real workloads. The 28-hours-at-10^6 problem doesn't exist.

## What's lost

- Cross-event dedup happens lazily (in the indexer or query layer) instead of eagerly (in the recorder). For most queries, the user never notices.
- Cross-run content identity (same value across two different trace files getting the same value_id) is gone — but that wasn't a v0 feature anyway.
- Trace files grow. Mitigated by compression and lazy indexing.

## Implementation order

1. **Update `docs/trace-format.md`** to reflect monotonic value_ids and value_aliases. Bump format version. Per CLAUDE.md, spec change comes first.
2. **Recorder: drop content hashing.** Replace with monotonic `value_id` + per-frame `id(obj) → CacheEntry` map. Add length-based change detection on re-capture. This is roughly the function `capture()` shown above.
3. **Indexer: lazy content-hash index.** Build secondary index on first query that joins on content. Until then, the index doesn't have it.
4. **Tools: update queries that assumed content-derived value_ids.** Most don't depend on this; the ones that do should use the lazy content-hash index.
5. **Tests:** existing recorder tests assume content-derived value_ids and will need adjusting. New tests should specifically cover the length-based detection branches and the value_alias case.

Each step is mostly self-contained. Step 1 unblocks 2; 2 unblocks 3 and 4; tests evolve alongside.

## What to push back on

If a reviewer says "but cross-frame content dedup is essential for [specific debugging workflow]" — ask for the specific workflow and trace through whether lazy content-hash indexing handles it. In most cases it does. If a workflow genuinely requires eager dedup at record time, that's load-bearing evidence to reconsider; absent that evidence, the perf cost of recorder-time hashing isn't justified.

If a reviewer wants the two-mode design ("just give users a knob"), the response is: knobs add user-visible complexity for a problem the user shouldn't have to think about. The recorder should be cheap and dumb. If the recorder is cheap enough that no one needs a knob, no knob.

## Summary

We were optimizing the wrong thing. Content hashing in the recorder is solving for storage and query-time convenience, paid in CPU on the program's hot path. Moving the work to the indexer (where there's slack) fixes the asymptotic blow-up, dissolves the mode-flag question, eliminates the user-hint footgun, and makes the recorder genuinely cheap. The trade-off is bigger trace files and lazy-built content indexes, both of which are tractable in the cold path.

The fix is a trace format version bump plus a recorder rewrite of the capture function. Probably 1-2 days of focused work. Probably should happen before any feature work that builds on the current recorder, because every feature shipped on the current capture path is one more thing to retest after the eventual fix.
