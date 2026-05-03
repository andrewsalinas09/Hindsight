# Trace format specification (v0.2)

This document specifies the binary format of Hindsight trace files (`.hindsight`). It is the contract between recorders and the rest of the system. Once we have third-party recorders, breaking changes to this format become expensive — we version the format and add fields rather than reorganizing existing ones.

This is v0.2: incorporates feedback from initial review. Sections marked **[CONTESTED]** are decisions that need more thought before lock-in. Sections marked **[DEFERRED]** are recognized issues whose resolution is intentionally pushed beyond v0.

## Goals

The format optimizes for:

- **Fast sequential write.** The recorder is in the program's hot path. Every microsecond of encoding overhead is overhead the user's program pays. Writes should be O(1) per event with small constants.
- **Compact representation.** Traces of substantial executions should fit in tens of MB after compression. Bloat anywhere in the format compounds across millions of events.
- **Forward compatibility.** A v1 reader should handle v2 traces gracefully (skipping unknown event types). Adding new event types and fields should not require breaking changes.
- **Self-containment.** A trace file should carry enough context (source code, scope metadata, type information) that an indexer can process it without external dependencies.
- **Streamability.** A reader should be able to process the trace incrementally without loading the whole file into memory.
- **Seekability.** Readers should be able to jump to a specific point in the trace without scanning from the start.
- **Crash recovery.** A trace whose recording was interrupted should still be partially readable. Last valid block becomes the effective end of the trace.

The format does not optimize for:

- **Direct query.** Queries are run against the indexed (DuckDB) form, not the wire format. The wire format only needs to be readable; it doesn't need indexes.
- **Append-after-close.** Once a trace is finalized, it's read-only. We don't support appending new events to an existing trace.
- **Random update.** Events are written once. There's no mechanism to modify a previously-written event.
- **Human readability.** This is a binary format. Humans inspect traces through tools, not by opening the file.

## File structure

A `.hindsight` file is a sequence of sections, in this order:

```
+----------------------+
| File header          |  Fixed size, 64 bytes
+----------------------+
| Initial metadata     |  Variable size, length-prefixed
+----------------------+
| Source bundle        |  Variable size, length-prefixed
+----------------------+
| Initial string table |  Length-prefixed
+----------------------+
| Initial value table  |  Length-prefixed
+----------------------+
| Event blocks         |  Sequence of compressed event blocks,
| (with checkpoints,   |  interspersed with checkpoints, table
|  table updates, and  |  snapshots, and table updates
|  table snapshots)    |
+----------------------+
| Final summary        |  Variable size, length-prefixed
+----------------------+
| Checkpoint index     |  Variable size, length-prefixed
+----------------------+
| Footer               |  Fixed size, 32 bytes
+----------------------+
```

The split between initial metadata (known at recording start) and final summary (known only at finalization) lets crashed traces preserve as much information as possible — initial metadata is always readable; final summary may be missing if the recorder didn't finalize cleanly.

### File header

64 bytes, fixed layout:

| Offset | Size | Field            | Description                                |
|--------|------|------------------|--------------------------------------------|
| 0      | 8    | Magic bytes      | ASCII `"HNDSGHT\0"` (0x48 0x4E 0x44 ...)   |
| 8      | 2    | Format version   | Major.minor as two u8s. v0.2 = 0x00 0x02.  |
| 10     | 2    | Header flags     | Bitfield, reserved. Must be zero in v0.2.  |
| 12     | 4    | Header length    | u32, total header section size including this field. v0.2 = 64. |
| 16     | 16   | Trace UUID       | Random 128-bit identifier for this trace.  |
| 32     | 8    | Recording start  | Wall-clock time of recording start, ns since Unix epoch. |
| 40     | 8    | Recording end    | Wall-clock time at finalization, ns since Unix epoch. Zero if not finalized cleanly. |
| 48     | 8    | Footer offset    | u64 offset into file where footer begins. Zero if file was not finalized cleanly. |
| 56     | 8    | Reserved         | Must be zero in v0.2.                      |

Magic bytes serve dual purposes: file-type identification (for `file` command, etc.) and corruption detection (a truncated or corrupted file fails the magic check immediately).

The format version uses major.minor to distinguish breaking changes (major bump) from additive changes (minor bump). v0.2 is the second iteration of the initial format; v1.0 will be the first stable release with strict backward-compat guarantees.

The footer offset enables seeking to the index without scanning. If zero, the file was not closed cleanly (recorder crashed or process killed); readers should fall back to scanning from the start of the event blocks.

### Initial metadata block

Length-prefixed binary blob containing recorder version, language metadata, command line, and configured scope. Written immediately after the header, when recording starts.

```
+------------+------------+------------+
| Length     | Format tag | Payload    |
| u32 (LE)   | u8         | (length    |
|            |            |  bytes)    |
+------------+------------+------------+
```

**TODO(v0.3):** the diagram is ambiguous about whether `Length` covers the format-tag byte. The current writer/reader pair treats `Length` as **inclusive of the format tag** — payload is `Length - 1` bytes. Surfaced by the reader implementation; rewrite this diagram to make the convention explicit.

Format tag indicates payload encoding:
- `0x01`: TOML (UTF-8)
- `0x02`: JSON (UTF-8)
- `0x03`-`0xFF`: reserved

v0.2 uses TOML. JSON is reserved for future use if we want lighter parsing in the recorder.

Required fields in the metadata payload:

```toml
[recorder]
language = "python"           # Recorder language frontend
language_version = "3.12.5"   # Specific language version
recorder_version = "0.1.0"    # Hindsight recorder version
platform = "linux-x86_64"     # OS and architecture

[recording]
program = "python script.py"  # Command line that produced this trace
working_directory = "/path/to/project"

[recording.scope_config]
# The scope configuration as requested by the user (before any matching).
# Resolved scope decisions appear in the final summary block.
include = []
exclude = ["defaults", "myapp.helpers.*"]
depth_limit = null

[program]
# Optional: information about the program being recorded
# (project name from pyproject.toml, git commit hash, etc.)
```

Information known only after recording (which functions actually got recorded, which were excluded by which patterns, whether the recording shut down cleanly) lives in the final summary block, not here.

### Source bundle

A content-addressed collection of source files referenced by events in the trace. Each entry is a source file with its hash, length, and content.

```
+------------+------------+------------+------------+
| Bundle     | File count |  ... files ...          |
| length u32 | u32        |                         |
+------------+------------+------------+------------+
```

Each file entry:

```
+------------+------------+------------+
| File ID    | Hash (32B) | Path len   |
| varint     | blake3-256 | u16        |
+------------+------------+------------+
| Path UTF-8 | Content    | Content    |
|            | length u32 | bytes      |
+------------+------------+------------+
```

File ID is a small integer assigned at write time, used by events to reference this file. The 0-indexed position in the bundle could serve, but we use an explicit ID to allow for future extension (e.g., source files added incrementally during long recordings).

Hash is blake3-256 of the file content. Source bundle uses blake3 specifically because source files are larger and fewer than values, so the hash performance cost is negligible and the stronger collision resistance is worth having for an artifact users might verify or sign.

Events reference source files by file ID (compact). The hash is for content addressing — verifying the recorded source matches what's currently on disk, sharing traces with their source intact, and so on.

Source files are stored uncompressed in the bundle. Compression at the section level would save space but complicate seeking. Source bundles for typical projects are small enough (kilobytes to low megabytes) that this isn't a major concern.

**[DEFERRED]** Privacy modes for source bundles. v0.2 always embeds full source. Future versions may support `referenced_lines`, `hashes_only`, or `none` modes for users with privacy or size concerns. The format leaves room for this via a future header flag bit; v0.2 ignores all flags.

### String table

A table of strings used in events: function names, variable names, file paths, exception types, etc.

The initial string table appears after the source bundle. It contains all strings known at recording start (typically just the recorder's built-in identifiers — common type names, well-known function names if any). As recording progresses, additional strings are added incrementally via update records inside the event stream.

Layout:

```
+------------+------------+----------------+
| Table      | String     | ... strings ...|
| length u32 | count u32  |                |
+------------+------------+----------------+
```

Each string:

```
+------------+------------+
| Length     | UTF-8 bytes|
| varint     |            |
+------------+------------+
```

Strings are referenced from events by their integer ID, which is their 0-indexed position in the table. Adding new strings via incremental updates appends to the table; IDs are stable.

The recorder must deduplicate before adding (each entry must be unique). Readers may assume uniqueness.

### Value table

A table of values referenced by events.

Layout:

```
+------------+------------+----------------+
| Table      | Value      | ... values ... |
| length u32 | count u32  |                |
+------------+------------+----------------+
```

Each value:

```
+------------+------------+------------+------------+
| Type tag   | Hash kind  | Hash       | Length     |
| u8         | u8         | (16B)      | varint     |
+------------+------------+------------+------------+
| Encoded data                                        |
+----------------------------------------------------+
```

Type tag indicates how the encoded data is interpreted (see "Value encoding" below).

Hash kind indicates what the hash represents:
- `0x01`: `content_hash` — xxhash3-128 of the value's full canonical representation. Cheap to compute for inlined values; not present for summarized values that would require full traversal.
- `0x02`: `summary_hash` — xxhash3-128 of the captured summary representation only. Used for summarized values where computing the full content hash would defeat summarization.
- `0x03`: `identity_hash` — language-level object identity (Python's `id()`, C++ pointer cast to integer, etc.). Indicates "same object" not "same content."
- `0x04`-`0xFF`: reserved.

Length is the byte length of the encoded data, not including the type tag, hash kind, hash, or length prefix itself.

**TODO(v0.3):** the spec doesn't say what a reader should do if `Length` exceeds the bytes the type tag's encoding consumes (i.e., trailing bytes inside the entry). The current reader treats this as an error. Codify this — strict-mode behavior in v0.x, possibly relaxed if a future revision adds optional trailing fields with their own length prefixes. Surfaced by the reader implementation.

**TODO(v0.3):** the spec doesn't say whether container values (`list`, `tuple`, `set`, `dict`) may reference value IDs that haven't been defined yet at this point in the table (forward refs). The current writer never emits them and the current reader rejects them. Codify this as "all referenced IDs must be `< current_index`" unless we deliberately want to allow forward refs (e.g., to encode cycles without `cycle_ref`). Surfaced by the reader implementation.

Why three hash kinds: content hash gives the strongest "is this the same value" answer but requires materializing the full content. For summarized values (large NumPy arrays, big DataFrames, deep object graphs), computing content hash defeats the purpose of summarization. Summary hash gives a weaker "the captured summary is the same" answer for summarized values. Identity hash gives "same object reference" which is useful for tracking mutation across function calls.

Hashes are 16 bytes (xxhash3-128) regardless of kind. This is a deliberate choice: 64 bits would be enough for collision resistance at recording time but is uncomfortable as a user-visible "same value" assertion across long-lived traces. xxhash3-128 is the same speed as xxhash3-64 on modern hardware (the algorithm computes 128 bits natively) so there's no performance reason to prefer the smaller hash.

When the LLM is told two values have the same hash, it can use the hash kind to calibrate confidence: same content_hash means almost certainly equal values; same summary_hash means same captured summary (the underlying values might differ outside the summary); same identity_hash means same object (mutable objects might have changed contents between observations).

Values are referenced from events by their integer ID, which is their 0-indexed position in the table.

### Event blocks

The bulk of the trace. A sequence of zstd-compressed blocks, each containing many events plus optional table-update records, table snapshots, and checkpoints.

Block structure:

```
+------------+------------+------------+
| Block      | Block tag  | Header     |
| length u32 | u8         | length     |
|            |            | varint     |
+------------+------------+------------+
| Compressed | Uncompressed| First     |
| length     | length      | event ID  |
| varint     | varint      | varint    |
+------------+------------+------------+
| Event      | String     | Value      |
| count      | table size | table size |
| varint     | after u32  | after u32  |
+------------+------------+------------+
| Checksum   | Compressed payload      |
| u32        |                          |
+------------+----------------------------+
```

Block tag values:
- `0x01`: Event block (contains events)
- `0x02`: Checkpoint record (contains seekability metadata)
- `0x03`: Table update (contains incremental additions to string and/or value tables)
- `0x04`: Table snapshot (contains complete state of string and value tables)
- `0x05`-`0xFF`: reserved

Header fields:

- **Compressed length**: byte length of the compressed payload that follows
- **Uncompressed length**: byte length the payload decompresses to (for safe allocation)
- **First event ID**: the global event ID of the first event in this block. For non-event blocks (checkpoints, table updates, snapshots), this is the event ID at the time the block was written
- **Event count**: number of events in this block. Zero for non-event blocks
- **String table size after**: total entries in the string table after this block has been processed
- **Value table size after**: total entries in the value table after this block has been processed
- **Checksum**: CRC32C of the compressed payload, for corruption detection

The compressed payload is zstd-compressed. The uncompressed format depends on the block tag.

Why `first_event_id` and `event_count` in the block header: events have stable global IDs, computed as `first_event_id + position_in_block`. This means events don't carry their own ID (saving bytes per event) but every event has a stable global identifier that the indexer and MCP tools can reference.

Why string/value table sizes after: lets a reader seeking to a specific point know exactly how to reconstruct the tables, and validate after applying updates that the resulting size matches.

#### Event block payload (tag 0x01)

```
+----------------+
| ... events ... |
+----------------+
```

Each event:

```
+------------+------------+----------------+
| Event      | Event type | Event payload  |
| length     | tag u8     | (length-1      |
| varint     |            |  bytes)        |
+------------+------------+----------------+
```

The event length includes the type tag byte. So a 20-byte event has a 1-byte type tag and 19 bytes of payload.

Event type tags are enumerated in "Event types" below. Unknown event types are skipped using the length prefix.

#### Checkpoint record payload (tag 0x02)

A checkpoint is metadata that lets a reader seek into the trace efficiently. Checkpoints are written periodically in the event stream (default: every 10,000 events or every 100ms of wall-clock time, whichever comes first).

```
+------------+------------+
| Wall clock | Nearest    |
| time ns u64| snapshot   |
|            | offset u64 |
+------------+------------+
```

- Wall clock time: ns since Unix epoch at checkpoint creation
- Nearest snapshot offset: byte offset of the most recent table snapshot (tag `0x04`) in the file. This is what makes seeking efficient — to seek to this checkpoint, read the snapshot, then apply all table-update blocks between the snapshot and this checkpoint.

Other information (event ID, table sizes, file offset) is in the block header itself, not duplicated in the payload.

**TODO(v0.3):** the spec says "the first snapshot in the file is logically the initial string and value tables (after the header)," but the on-disk encoding of the initial tables and a snapshot block (tag `0x04`) are different (initial = two raw length-prefixed sections; snapshot = a compressed block-tagged payload), so a reader can't decode them uniformly. The current writer/reader pair uses **`snapshot_offset == 0` as a sentinel meaning "use the initial string and value tables that follow the file header"** — readers special-case zero rather than trying to parse a snapshot block at offset 0. This sentinel value applies both to the checkpoint record payload above and to the same-named field in checkpoint index entries. Codify this sentinel (or alternatively require the writer to emit an explicit snapshot block right after the initial value table so the offset always points at a real snapshot block). Surfaced by the writer/reader implementation.

#### Table update payload (tag 0x03)

Incremental additions to the string and value tables since the last update or snapshot.

```
+------------+------------+----------------+
| Base       | New string | New strings    |
| string     | count u32  | (same encoding |
| count u32  |            |  as initial    |
|            |            |  table)        |
+------------+------------+----------------+
| Base value | New value  | New values     |
| count u32  | count u32  | (same encoding |
|            |            |  as initial    |
|            |            |  table)        |
+------------+------------+----------------+
```

`base_string_count` is the size of the string table immediately before applying this update. Readers verify their reconstructed table size matches this value before applying; if not, the trace was misordered or corrupted.

`new_string_count` is the number of new entries this update adds. The total string table size after this update is `base_string_count + new_string_count`, which should match the block header's `string_table_size_after` field.

Same logic for values.

The recorder emits table updates immediately before any event block that references newly-introduced strings or values. This guarantees that an event never references an ID that doesn't yet exist in the table.

**TODO(v0.3):** the spec doesn't say whether the writer must emit an *empty* table update block (one whose `new_string_count` and `new_value_count` are both zero) before an event block when no new entries have been interned since the last update. The current writer **skips** empty updates as pure overhead; the current reader correspondingly does **not** assume a 1:1 correspondence between event blocks and table updates — an event block that doesn't reference any newly-introduced strings or values has no preceding update. Codify "writers MUST NOT emit empty table updates; readers MUST NOT assume one update per event block." Surfaced by the writer/reader implementation.

**TODO(v0.3):** the spec also doesn't say whether the snapshot interval ("default every 100 checkpoints") is counted since file start or since the last snapshot. The current writer counts **since the last snapshot** so snapshot decisions are locally determined rather than dependent on the full checkpoint history. Codify the choice. Surfaced by the writer implementation.

#### Table snapshot payload (tag 0x04)

A complete state of the string and value tables at this point. Snapshots enable efficient seeking — instead of replaying every table update from the start of the file, a reader can jump to the most recent snapshot and apply only the updates between the snapshot and the seek target.

```
+------------+------------+----------------+
| String     | String     | All strings    |
| table      | count u32  | (same encoding |
| length u32 |            |  as initial    |
|            |            |  table)        |
+------------+------------+----------------+
| Value      | Value      | All values     |
| table      | count u32  | (same encoding |
| length u32 |            |  as initial    |
|            |            |  table)        |
+------------+------------+----------------+
```

A snapshot is functionally a fresh "initial table" written at a specific point in the trace. After reading a snapshot, the reader's tables are in a fully reconstructed state and ready to process subsequent events.

Snapshots are written periodically — default every 100 checkpoints, or roughly every million events. They cost space (each is the full size of the tables at that point) but make seeking O(1) rather than O(n) in the number of preceding table updates.

The first snapshot in the file is logically the initial string and value tables (after the header). Subsequent snapshots are inline with the event blocks.

### Final summary

Length-prefixed TOML, written near the end of the file when recording finalizes cleanly. Contains information that's only known after recording completes.

**TODO(v0.3):** unlike the initial metadata block, this section's diagram doesn't show a format-tag byte distinguishing TOML from JSON. The current writer/reader pair treats the section as a u32 length followed by raw TOML bytes — no inner format tag. Either commit to that (final summary is always TOML) or add a parallel format-tag byte so JSON is reachable later. Surfaced by the writer/reader implementation.

**TODO(v0.3):** the `[final.statistics]` example in this section only enumerates a subset of event types (function_entry, line, branch, exception, note). The current writer emits a full per-event-type breakdown — `function_entry_events`, `function_exit_events`, `frame_snapshot_events`, `line_events`, `branch_events`, `exception_events`, `note_events`, `scope_boundary_events`, `frame_switch_events`. Update the example to enumerate all nine, or explicitly mark the spec list as illustrative-only. Surfaced by the writer implementation.

```toml
[final]
clean_shutdown = true
total_events = 1234567
total_blocks = 234
trace_duration_ns = 5432100000  # Total wall-clock duration

[final.scope_resolved]
recorded_functions = [
    "myapp.process_request",
    "myapp.parse_user",
    # ...
]
excluded_functions = [
    { name = "numpy.dot", matched_pattern = "numpy.*" },
    # ...
]
skip_blocks_observed = 5
depth_clips_observed = 0

[final.statistics]
function_entry_events = 12345
line_events = 987654
branch_events = 234567
exception_events = 12
note_events = 0
```

If `clean_shutdown = false` or the final summary is missing entirely, the trace was interrupted. The header's `recording_end` field will be zero in this case. Readers handle this gracefully — events read up to the last valid block are fully usable; the summary just isn't available.

**TODO(v0.3):** the spec doesn't define what `total_blocks` counts. The §"Event blocks" section describes `0x01..=0x04` block tags; the final summary, checkpoint index, and footer aren't block-tagged at all. The current writer counts every block emitted into the file (event 0x01 + checkpoint 0x02 + table update 0x03 + table snapshot 0x04) plus the final summary itself — so an empty trace finalizes with `total_blocks = 2` (one event block + one summary), and a trace with checkpoints/updates/snapshots adds one to that count for each. Either codify this all-blocks-plus-summary interpretation or rename the field (e.g., `total_event_blocks`) to scope it strictly to `0x01` blocks. Surfaced by the writer implementation.

### Checkpoint index

Length-prefixed flat array of checkpoint metadata, written just before the footer. Enables fast wall-clock-based seeking via binary search.

```
+------------+------------+----------------+
| Index      | Entry      | ... entries ...|
| length u32 | count u32  |                |
+------------+------------+----------------+
```

Each entry:

```
+------------+------------+------------+------------+
| Wall clock | Event ID   | File       | Snapshot   |
| time ns u64| u64        | offset u64 | offset u64 |
+------------+------------+------------+------------+
```

Entries are sorted by wall-clock time. To seek to a specific time:
1. Binary search the index for the latest entry with `wall_clock_time <= target_time`
2. Read the table snapshot at `snapshot_offset` (reconstructing the tables)
3. Apply any table updates between the snapshot and the checkpoint
4. Read events from the checkpoint forward

If no checkpoint index is present (unfinalized trace), readers fall back to scanning from the start.

### Footer

32 bytes, fixed layout. Written at the end of the file when recording finalizes cleanly.

| Offset | Size | Field                | Description                              |
|--------|------|----------------------|------------------------------------------|
| 0      | 8    | Magic bytes          | ASCII `"HNDFOOT\0"` for footer detection |
| 8      | 4    | Footer length        | u32, total footer size. v0.2 = 32        |
| 12     | 8    | Checkpoint index off | u64, file offset of checkpoint index     |
| 20     | 8    | Final summary offset | u64, file offset of final summary block  |
| 28     | 4    | Reserved             | Must be zero in v0.2                     |

If the recording was not finalized cleanly, the footer is absent and the header's `footer_offset` is zero. Readers detecting this case scan from the start of the event blocks; they're slower but the file is still readable.

**TODO(v0.3):** the spec doesn't say whether a sequential reader should validate that the footer's `final_summary_offset` and `checkpoint_index_offset` match where it actually parsed those sections, nor whether the header's `footer_offset` should be validated against where the footer actually begins. The current reader cross-checks both, surfacing `FooterOffsetMismatch` / `HeaderFooterOffsetMismatch` on disagreement, on the theory that mismatches indicate corruption and a sequential reader has those offsets for free. Codify the requirement (or explicitly mark it as optional). Surfaced by the reader implementation.

## Event types

Event type tags and their payload schemas. All multi-byte numeric fields are little-endian. Varints follow the LEB128 unsigned encoding.

### `0x01` FUNCTION_ENTRY

A function began executing.

```
+----------------+----------------+----------------+
| Timestamp      | Frame ID       | Function ID    |
| delta varint   | varint         | varint (string |
|                |                |  table)        |
+----------------+----------------+----------------+
| Source file ID | Line number    | Argument count |
| varint         | varint         | varint         |
+----------------+----------------+----------------+
| Argument list                                     |
+--------------------------------------------------+
```

Each argument:

```
+----------------+----------------+
| String ID      | Value ID       |
| (arg name)     |                |
| varint         | varint         |
+----------------+----------------+
```

**Frame ID** is a recorder-assigned identifier for this function activation. Frame IDs are unique within a single trace; the recorder maintains a counter and assigns the next available ID at each function entry. Subsequent events implicitly belong to the most-recently-entered frame until a frame transition occurs (FUNCTION_EXIT, FRAME_SWITCH for generators/async).

Timestamp delta is nanoseconds since the previous event in the trace.

Function ID references the string table; the referenced string is the qualified function name (e.g., `module.submodule.ClassName.method_name`).

Source file ID references the source bundle. Line number is where the function definition begins.

Arguments capture the function's parameters at entry. Argument names are interned strings; values are interned in the value table.

### `0x02` FUNCTION_EXIT

A function returned (normally; exceptions use a separate event).

```
+----------------+----------------+----------------+
| Timestamp      | Frame ID       | Return value   |
| delta varint   | varint         | ID varint      |
+----------------+----------------+----------------+
```

Frame ID identifies which function activation is exiting. This makes the format robust against generators, async, and other cases where exits don't strictly nest with entries.

Return value ID references the value table. For functions returning `None` (Python) or `void` (C++), this references a special interned None value (always at value table index 0 by convention).

### `0x03` FRAME_SNAPSHOT

A complete snapshot of all locals in a frame. Emitted at function entry (immediately after FUNCTION_ENTRY) and periodically thereafter (default: every 1000 LINE_DELTA events in the same frame, or every 10ms of frame time, whichever comes first).

```
+----------------+----------------+----------------+
| Timestamp      | Frame ID       | Line number    |
| delta varint   | varint         | varint         |
+----------------+----------------+----------------+
| Locals count   | Locals list                     |
| varint         |                                  |
+----------------+---------------------------------+
```

Each local:

```
+----------------+----------------+
| String ID      | Value ID       |
| (var name)     |                |
| varint         | varint         |
+----------------+----------------+
```

FRAME_SNAPSHOT is what makes "what was variable x at point Y" queries efficient: a reader can find the most recent FRAME_SNAPSHOT in the relevant frame and then apply LINE_DELTA events forward to reconstruct any variable's state at any point.

### `0x04` LINE_DELTA

A source line was executed; captures only locals whose values changed since the previous event in this frame.

```
+----------------+----------------+----------------+
| Timestamp      | Line number    | Changes count  |
| delta varint   | varint         | varint         |
+----------------+----------------+----------------+
| Changes list                                      |
+--------------------------------------------------+
```

Each change:

```
+----------------+----------------+
| String ID      | Value ID       |
| (var name)     | (new value)    |
| varint         | varint         |
+----------------+----------------+
```

The frame is implicit (the most recent FUNCTION_ENTRY or FRAME_SWITCH).

If no locals changed (e.g., the line just calls a function), the changes count is zero and the event is just the timestamp delta and line number — typically 4-6 bytes. Most LINE_DELTA events are this small.

When a local appears in a LINE_DELTA, the value ID may differ from the previous capture for two reasons: the variable was reassigned (`x = new_value`), or the variable's content changed (e.g., a list was mutated). The trace doesn't distinguish these — the LLM consumer infers which by reading the source between the events. This is intentional; capturing reassignment-vs-mutation explicitly would significantly complicate the recorder.

A reader reconstructing "what was x at this LINE_DELTA event" walks backward through events in this frame, looking for the most recent assignment to `x` (LINE_DELTA capturing it, or the initial FRAME_SNAPSHOT). The indexer makes this efficient via the `event_locals` join table (see "Indexer schema" below).

### `0x05` BRANCH_RESULT

A conditional branch was evaluated.

```
+----------------+----------------+----------------+
| Timestamp      | Line number    | Branch result  |
| delta varint   | varint         | u8 (0=False,   |
|                |                |  1=True)       |
+----------------+----------------+----------------+
```

The condition's text is recoverable from the source file at the given line. The LLM consumer reads the source to understand what the branch was testing.

The frame is implicit.

**[DEFERRED]** Per-operand capture for compound conditions (`if a and b and c:`). v0.2 captures only the final result. The LLM can usually infer operand values from surrounding LINE_DELTA events plus its understanding of short-circuit evaluation. A future event type (BRANCH_DETAIL, tag reserved) will add per-operand capture if benchmarks show this is insufficient.

### `0x06` EXCEPTION_RAISED

An exception was raised (regardless of whether it's caught later).

```
+----------------+----------------+----------------+
| Timestamp      | Line number    | Exception type |
| delta varint   | varint         | string ID      |
|                |                | varint         |
+----------------+----------------+----------------+
| Exception      |
| value ID       |
| varint         |
+----------------+
```

The frame is implicit.

Exception type ID references the string table; the referenced string is the qualified exception class name. Exception value ID references the value table; the referenced value is the exception instance summary.

If the exception propagates out of recorded functions, FUNCTION_EXIT events are emitted for each unwound frame, with the return value ID set to a special "exception unwind" sentinel value (always at value table index 1 by convention).

### `0x07` NOTE

A user-emitted note via `hindsight.note(...)`.

```
+----------------+----------------+----------------+
| Timestamp      | Line number    | Message        |
| delta varint   | varint         | string ID      |
|                |                | varint         |
+----------------+----------------+----------------+
| Keyword arg    | Keyword args list                |
| count varint   |                                  |
+----------------+----------------------------------+
```

Each keyword arg uses the same `(string_id, value_id)` encoding as function arguments.

The message is a free-form string. Keyword args let users attach structured data to notes (`hindsight.note("processed", count=42, status="ok")`).

The frame is implicit.

### `0x08` SCOPE_BOUNDARY

Recording entered or exited a scope boundary (e.g., a `hindsight.skip()` block, or a function transition that crossed an exclusion or depth limit).

```
+----------------+----------------+----------------+
| Timestamp      | Boundary type  | Reason         |
| delta varint   | u8             | string ID      |
|                |                | varint         |
+----------------+----------------+----------------+
```

Boundary type values:
- `0x01`: Entered skip block
- `0x02`: Exited skip block
- `0x03`: Entered excluded function (call site recorded; interior is not)
- `0x04`: Exited excluded function
- `0x05`: Entered depth-clipped function (call site recorded; interior is not)
- `0x06`: Exited depth-clipped function

Reason references a string explaining the boundary (e.g., `"matched pattern: numpy.*"` for an exclusion, or `"depth limit 1 exceeded"` for a depth clip).

These events let the LLM understand "why don't I have data for this function" without confusion. The trace explicitly records that the recorder chose not to enter, with the reason.

### `0x09` FRAME_SWITCH

Execution switched to a different frame without a normal call/return (generator yield/resume, async task switch, exception unwind partial).

```
+----------------+----------------+----------------+
| Timestamp      | Old frame ID   | New frame ID   |
| delta varint   | varint         | varint         |
+----------------+----------------+----------------+
| Reason         |
| u8             |
+----------------+
```

Reason values:
- `0x01`: Generator yield (old frame is suspended, control returns to caller)
- `0x02`: Generator resume (old frame is restored, was suspended)
- `0x03`: Async task switch
- `0x04`: Exception partial unwind
- `0x05`-`0xFF`: reserved

Subsequent events implicitly belong to the new frame. FRAME_SWITCH does not imply a function entry or exit; the frames involved already existed (either via FUNCTION_ENTRY earlier in the trace, or via implicit creation on yield/await).

### Reserved event types

`0x0A`-`0xFF` are reserved for future use. Readers must skip unknown event types using the length prefix.

**TODO(v0.3):** v0 readers (see `hindsight-format`) intentionally **reject** unknown event tags rather than skipping them, because the writer doesn't yet emit anything outside `0x01..=0x04` and a stray tag during v0 indicates a writer bug or corruption rather than a forward-compat scenario. This will flip to skip-with-warning once the writer supports the full event-type set; codify the transition condition here. Surfaced by the reader implementation.

Specifically reserved:
- `0x0A`: BRANCH_DETAIL — per-operand capture for compound conditions (planned for v0.3+)
- `0x0B`: THREAD_SWITCH — multi-threaded recording context switch (planned for v1+)

## Value encoding

The value table stores program values referenced by events. Each entry has a type tag, a hash kind, a hash, a length, and encoded data.

### Type tags

| Tag    | Type          | Encoding                                                |
|--------|---------------|---------------------------------------------------------|
| `0x00` | None/null     | Empty (length is 0). Always at value table index 0.     |
| `0x01` | bool          | 1 byte (0 or 1)                                         |
| `0x02` | int (small)   | varint signed (zigzag-encoded LEB128)                   |
| `0x03` | int (big)     | length-prefixed two's complement bytes                  |
| `0x04` | float (f64)   | 8 bytes IEEE 754 little-endian                          |
| `0x05` | string        | length-prefixed UTF-8                                   |
| `0x06` | bytes         | length-prefixed raw bytes                               |
| `0x07` | list/tuple    | varint count, then list of value IDs                    |
| `0x08` | dict          | varint count, then list of (key value ID, value ID)    |
| `0x09` | set           | varint count, then list of value IDs                    |
| `0x0A` | cycle ref     | varint depth (frames back to the referenced container) |
| `0x10` | summary       | structured summary of large/complex values (see below)  |
| `0x11` | type ref      | string ID referencing a type/class name                 |
| `0x12` | exception unwind sentinel | Empty. Always at value table index 1.       |
| `0x13`-`0x7F` | reserved primitive |                                                  |
| `0x80`-`0xFF` | language-specific |                                                  |

Tags `0x80`-`0xFF` are available for language-specific encodings. Each language frontend documents its tag assignments in a companion document.

**TODO(v0.3):** the rows for `0x03` (int big), `0x05` (string), and `0x06` (bytes) describe the encoding as "length-prefixed", which is ambiguous about whether they carry a *second*, inner length prefix on top of the value-table-entry length. The current writer/reader pair treats them as **the value-table-entry length only** — no inner length, the encoded data fills the entry. Reword these rows to say "raw bytes (entry length bounds the run)" or similar. Surfaced by the reader implementation.

**TODO(v0.3):** distinguish list / tuple / set / frozenset at the type-tag level. v0.2 collapses list+tuple onto `0x07` and set+frozenset onto `0x09`, expecting consumers to read a separate `TypeRef` to disambiguate — but inline containers don't carry one, so the indexer can't tell them apart and `frames.argument_summary` and `values.type_name` both lose the distinction. Plan: introduce dedicated tags from the reserved primitive range (e.g. `0x13` tuple, `0x14` frozenset) so each Python collection type round-trips losslessly. This is a deliberate spec revision, not an indexer-side workaround; defer until `hindsight-format` work resumes. Surfaced by the indexer implementation.

### Inlining vs. summarization

Values below a size threshold are encoded fully (inlined). Values above the threshold are summarized — only their type, identity hash, length, and a truncated representation are stored.

Default thresholds (configurable per recording):
- Strings: inline up to 1024 bytes; summarize beyond
- Lists/tuples/sets: inline up to 64 elements; summarize beyond
- Dicts: inline up to 32 entries; summarize beyond
- Bytes: inline up to 1024 bytes; summarize beyond

For container types, "inline" means the container is encoded with references to its elements' value IDs, but each element is itself subject to inlining/summarization. This is recursive.

### Summary type (`0x10`)

A summary captures a value too large to inline.

```
+----------------+----------------+----------------+
| Type ref       | Length         | Repr           |
| string ID      | varint         | string ID      |
| varint         |                | varint         |
+----------------+----------------+----------------+
```

Fields:
- Type ref: the value's type/class name (e.g., `"numpy.ndarray"`, `"list"`, `"MyClass"`)
- Length: a type-appropriate length measure (number of elements for containers, byte length for strings/bytes, dimensions for arrays — type-defined)
- Repr: a string ID for a truncated text representation of the value (e.g., first 256 characters of `repr(value)`)

When a value is summarized, the value table entry's hash kind is `summary_hash` (xxhash3-128 of the summary's contents), not `content_hash`. This is the explicit acknowledgment that we don't have a full content hash for summarized values; we only know the summary matches.

If the recorder is willing to pay the cost of computing a full content hash for a summarized value (e.g., for known-cheap types like NumPy arrays where hashing the underlying buffer is fast), it can use hash kind `content_hash`. The choice is recorder-configurable.

### Cycle reference (`0x0A`)

A cycle reference is used inside a container value's encoding to mark a position where a cycle would otherwise cause infinite recursion.

```
+----------------+
| Depth          |
| varint         |
+----------------+
```

Depth is the number of containers up from the current encoding position to find the referenced container. Depth 0 means "self-reference" (the immediately enclosing container). Depth 1 means "the parent container," and so on.

For example, encoding `x = []; x.append(x)`:
- The list has 1 element
- The element is a cycle_ref with depth 0 (self-reference)

For nested cycles, encoding `a = []; b = [a]; a.append(b)`:
- `a` is a list with 1 element (referencing `b`)
- `b` is a list with 1 element (referencing `a` via cycle_ref with depth 1)

Cycle detection happens in the recorder during canonicalization; the recorder maintains a stack of currently-being-encoded containers and emits cycle_refs when it would recurse into one. Readers and the indexer interpret cycle_refs but don't have to do detection themselves.

### Canonical representation

For hashing to be deterministic, values must have a canonical byte representation that's used for hash computation (independent of the encoding used in the trace).

- None: empty
- bool: single byte 0 or 1
- int: two's complement, minimum byte length, big-endian
- float: IEEE 754 8-byte big-endian
- string: UTF-8 bytes
- bytes: raw bytes
- list/tuple: concatenation of element canonical representations, prefixed by element count as 8-byte big-endian
- dict: entries sorted by key canonical representation, each entry as concatenation of key and value canonical representations
- set: elements sorted by canonical representation, then concatenated
- cycle_ref: 1 byte 0xFF followed by depth as 4-byte big-endian
- summary: when content_hash kind is used, the canonical representation of the full value (recorder must compute this); when summary_hash kind is used, the concatenation of type ref string + length as 8-byte big-endian + repr string

Canonical representation is for hashing only; the on-disk encoding can be different (e.g., little-endian for performance).

### Language-specific value handling

The Python recorder uses the following mapping from Python types to value tags:

| Python type     | Tag  | Notes                                            |
|-----------------|------|--------------------------------------------------|
| `None`          | 0x00 |                                                  |
| `bool`          | 0x01 |                                                  |
| `int` (fits i64)| 0x02 | Small integer encoding                           |
| `int` (large)   | 0x03 | Big integer encoding                             |
| `float`         | 0x04 |                                                  |
| `str`           | 0x05 | UTF-8                                            |
| `bytes`         | 0x06 |                                                  |
| `list`          | 0x07 |                                                  |
| `tuple`         | 0x07 | Same tag as list; type ref distinguishes        |
| `dict`          | 0x08 |                                                  |
| `set`/`frozenset`| 0x09 |                                                 |
| Other objects   | 0x10 (summary) | Type ref is class qualified name; repr is truncated `repr()` |

NumPy arrays are summarized with type ref `"numpy.ndarray"`, length encoding shape, and repr containing dtype plus first few elements. Tag `0x80` is reserved for a future native NumPy encoding if benchmarks show summarization is too lossy for common ML debugging cases.

## Indexer schema

The trace format itself is wire-only; the indexer transforms it into a queryable form (currently DuckDB). The canonical indexer schema is part of the contract — the format is designed so that this schema can answer the queries the MCP tools need.

Tables:

- `events(event_id PRIMARY KEY, type, frame_id, timestamp_ns, source_file_id, line, function_id_if_entry, return_value_id_if_exit, ...)`
- `event_locals(event_id, name_string_id, value_id)` — captures (name, value) pairs from FRAME_SNAPSHOT and LINE_DELTA events
- `frames(frame_id PRIMARY KEY, function_id, source_file_id, parent_frame_id, entry_event_id, exit_event_id)`
- `strings(string_id PRIMARY KEY, content)`
- `values(value_id PRIMARY KEY, type_tag, hash_kind, hash, encoded_length, encoded_data)`
- `source_files(source_file_id PRIMARY KEY, path, content_hash, content)`
- `branches(event_id, line, result)` — denormalized for fast branch queries
- `notes(event_id, message_string_id)` — denormalized for fast note queries
- `scope_boundaries(event_id, boundary_type, reason_string_id)` — denormalized for fast scope queries

Critical queries the schema supports:

**"What was variable X at event Y in frame F?"**

```sql
SELECT v.* FROM event_locals el
JOIN values v ON el.value_id = v.value_id
JOIN events e ON el.event_id = e.event_id
WHERE e.frame_id = F
  AND el.name_string_id = (SELECT string_id FROM strings WHERE content = 'X')
  AND el.event_id <= Y
ORDER BY el.event_id DESC LIMIT 1;
```

This walks backward through `event_locals` to find the most recent capture of X in the relevant frame.

**"All values of variable X over time in frame F."**

```sql
SELECT el.event_id, e.timestamp_ns, v.*
FROM event_locals el
JOIN values v ON el.value_id = v.value_id
JOIN events e ON el.event_id = e.event_id
WHERE e.frame_id = F
  AND el.name_string_id = (SELECT string_id FROM strings WHERE content = 'X')
ORDER BY el.event_id;
```

**"All calls to function F."**

```sql
SELECT * FROM events
WHERE type = 'FUNCTION_ENTRY'
  AND function_id = (SELECT string_id FROM strings WHERE content = 'F')
ORDER BY event_id;
```

**"Causal slice for value V at event Y."** This is a recursive query: find all events that read or wrote variables that V depends on, walking backward. Implemented as an iterative procedure in the MCP server, calling primitive queries against the indexer schema.

The indexer schema is normalized for storage efficiency and denormalized strategically (branches, notes) for query speed. Implementations may add additional indexes for specific query patterns; the schema above is the minimum required.

## Worked example

A small Python function and the trace it produces.

Source (`example.py`):

```python
def double(x):
    result = x * 2
    return result

double(5)
```

Recording invocation: `hindsight record python example.py`

The resulting `.hindsight` file (conceptual):

**File header**: magic, version 0x00 0x02, length 64, UUID, recording start, recording end (filled at finalization), footer offset.

**Initial metadata block**:
```toml
[recorder]
language = "python"
language_version = "3.12.5"
recorder_version = "0.1.0"

[recording]
program = "python example.py"

[recording.scope_config]
include = []
exclude = ["defaults"]
depth_limit = null
```

**Source bundle**:
- 1 file: file_id 0, path `example.py`, blake3 hash of content, 60 bytes of source

**Initial string table**: pre-populated with common Python strings (e.g., `"None"`, `"int"`, `"str"`, etc.), plus runtime additions during recording.

**Initial value table**:
- `[0]`: type tag 0x00 (None), hash kind content_hash, hash of empty
- `[1]`: type tag 0x12 (exception unwind sentinel)

**Event blocks (compressed; uncompressed contents)**:

Block 1 (table update for new strings `"__main__.double"`, `"x"`, `"result"`):
- base_string_count: (whatever the initial table had)
- new_string_count: 3
- ...

Block 2 (table update for new values: int 5, int 10):
- base_value_count: 2
- new_value_count: 2
- ...

Block 3 (event block, 5 events, first_event_id 0):
- Event 0: FUNCTION_ENTRY, frame_id 0, function_id (`__main__.double`), source_file_id 0, line 1, 1 arg: (`x`, value_id pointing to int 5)
- Event 1: FRAME_SNAPSHOT, frame_id 0, line 1, 1 local: (`x`, int 5)
- Event 2: LINE_DELTA, line 2, 1 change: (`result`, int 10)
- Event 3: LINE_DELTA, line 3, 0 changes (just the return statement, nothing changed)
- Event 4: FUNCTION_EXIT, frame_id 0, return value_id pointing to int 10

**Final summary**:
```toml
[final]
clean_shutdown = true
total_events = 5
trace_duration_ns = 1234567

[final.scope_resolved]
recorded_functions = ["__main__.double"]
excluded_functions = []
```

**Checkpoint index**: empty (trace too short for any checkpoints).

**Footer**: magic, length 32, checkpoint_index_offset, final_summary_offset.

Total file size estimate (with zstd compression on event block): ~600-800 bytes for this trivial example. Most of that is the metadata blocks (TOML is verbose) and the source bundle.

## Size targets

For typical traces, the format should achieve:

- Per-event overhead: 4-10 bytes for LINE_DELTA events with 0-1 changes (the common case in straight-line code); 10-20 bytes for FRAME_SNAPSHOT events with several locals; 15-30 bytes for FUNCTION_ENTRY events
- Compression ratio: 3-5x via zstd on event blocks
- String table overhead: amortizes to near-zero for traces with many events
- Value table overhead: similar amortization; deduplication is significant for traces with repeated values

A million-event trace should be 5-15 MB on disk (with the LINE_DELTA optimization). A debugging session for a typical bug should be tens of KB to a few MB. A heavy production-style recording session is the upper end at hundreds of MB.

The LINE_DELTA approach substantially reduces trace size compared to capturing all locals on every line: tight loops with stable variables produce near-empty events (just timestamp delta and line number), and only the variables that actually change pay encoding cost.

## Forward compatibility

The format is designed for graceful evolution.

**Adding event types** is safe: readers skip unknown event tags using the length prefix. Recorders writing new event types should bump the format version's minor number.

**Adding fields to existing event types** is *not* safe in v0.2, because event payloads don't have internal length prefixes for individual fields. To add fields to an event type without breaking compatibility, recorders should emit a new event type with the additional fields. Old readers see the new event type as unknown and skip it; new readers see it and process it.

This is a deliberate v0.2 trade-off: simpler events at the cost of less in-event extensibility. v1.0 may revisit by adding optional trailing fields with their own length prefixes, but for v0.2 the simpler approach wins.

**Adding value type tags** is safe: readers skip unknown value type tags using the length prefix. Old code that doesn't recognize a tag will treat the value as opaque; new code can decode it.

**Major version bumps** indicate breaking changes. v1.0 → v2.0 might restructure event encoding or change the table format. Readers must check the version field and refuse to process incompatible major versions.

## Implementation notes

This section is informative, not normative — concrete advice for implementers.

**Recorder side:** maintain in-memory copies of the string table, value table, and a content-hash index. Before adding a string or value, check if it's already in the table; if so, reuse the existing ID. The hash check is what makes this O(1) per addition.

For LINE_DELTA encoding, maintain per-frame state of "last captured value ID for each local." On each line event, compare current locals against last captured; emit only differences.

**Reader side:** stream events one at a time using the length prefixes. Maintain in-memory state for the current frame stack (driven by FUNCTION_ENTRY/FUNCTION_EXIT/FRAME_SWITCH) and current table contents (driven by table updates and snapshots).

**Indexer side:** translate the wire format into the canonical schema. Maintain a "running locals" data structure per frame to expand LINE_DELTA events into the full `event_locals` table — every LINE_DELTA event populates `event_locals` rows for the changed locals, and additionally for any unchanged locals if the indexer wants to support fast point-in-time queries without walking backward (this is a space/time tradeoff; the schema above assumes minimal `event_locals` and walk-backward queries).

**Compression block sizing:** zstd works best on blocks of ~16-64 KB of input. Recorders should aim for blocks in this range — flushing less often wastes memory; flushing more often hurts compression ratio.

**Checkpoint and snapshot frequency:** balance between seek granularity and overhead. Default 10,000 events / 100ms for checkpoints. Default every 100 checkpoints for table snapshots.

**Crash recovery:** readers detecting an unfinalized trace (footer offset = 0 in header, or no footer magic) should scan from the start of event blocks. Each block is self-contained (compressed length, checksum, event count), so corruption typically affects only the last partial block. Readers should stop at the first block whose checksum fails or whose decompression fails.

## Open questions

These are decisions explicitly deferred for iteration, beyond the **[CONTESTED]** and **[DEFERRED]** items in-line above.

- **Multi-process traces.** v0.2 is single-process. Multi-process is out of scope for v0; would require a manifest format for distributed traces.

- **Multi-threading.** v0.2 is single-thread; the format reserves THREAD_SWITCH (event tag 0x0B) for future use.

- **Source bundle compression.** Currently uncompressed for simplicity. Source files are usually small relative to event volume, but for traces of large codebases compression might matter.

- **Multi-language traces.** A program calling out to a C++ extension and back to Python. v0.2 says single-language; a future version would need cross-language event linkage.

These are not blockers for v0 but should be tracked.

## Status of this document

v0.2, incorporating revisions from initial review. Major decisions to revisit before v1.0:

1. The LINE_DELTA + FRAME_SNAPSHOT split — is the snapshot frequency tuning right?
2. The hash kind enumeration — are content/summary/identity the right three?
3. The scope of FRAME_SWITCH — does it cover all the async/generator cases cleanly?
4. The branch operand capture question — is the deferred BRANCH_DETAIL design right?

Implementers building against v0.2 should expect minor format changes before v1.0. Once v1.0 is published, the format is stable and breaking changes require a major version bump.
