// SPDX-License-Identifier: Apache-2.0

//! PyO3 bindings exposing the Hindsight Rust core to the Python recorder.
//!
//! This module makes the `hindsight-format` writer (and a small mirror of the
//! reader, just enough to back integration tests) available to Python as the
//! private submodule `hindsight._core`. The user-facing `hindsight` package
//! lives in `python/hindsight/` and re-exports a curated surface from here.
//!
//! Session A scope (no `sys.monitoring` integration yet):
//! - `TraceWriter` pyclass exposing source/string/value interning, the four
//!   v0 event types the recorder will emit first (FUNCTION_ENTRY,
//!   FUNCTION_EXIT, FRAME_SNAPSHOT, LINE_DELTA), and finalization to file or
//!   bytes.
//! - `read_trace(path)` function returning a `dict` of the trace's contents,
//!   used by the Python integration test to verify round-trip.
//!
//! `convert_value` is the heart of the boundary: it walks any Python value,
//! converts each node, and auto-interns children before parents so the
//! writer's "child IDs must already exist" contract holds.

use std::path::PathBuf;

use pyo3::exceptions::{PyOverflowError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyBool, PyBytes, PyDict, PyFloat, PyFrozenSet, PyInt, PyList, PySet, PyString, PyTuple,
};

use hindsight_format::{
    AliasKind, Argument, BoundaryType, BranchResult, Change, Confidence, EXCEPTION_UNWIND_VALUE_ID,
    Event, EventTag, ExceptionRaised, ExcludedFunction, Finalization, FrameSnapshot, FunctionEntry,
    FunctionExit, HashKind, Kwarg, LineDelta, Local, Metadata, NONE_VALUE_ID, Note, ProgramInfo,
    RecorderInfo, RecordingInfo, ScopeBoundary, ScopeConfig, ScopeResolution, SourceFile,
    TraceReader, Value, ValueEntry, ValueId, ValueTag,
};

/// Maximum char count we keep from `repr(obj)` when summarizing arbitrary
/// objects. Char-based, not byte-based, so we never split a UTF-8 codepoint.
const SUMMARY_REPR_MAX_CHARS: usize = 256;

// --- Metadata dict parsing --------------------------------------------------

/// Convert a Python `dict` into a Rust `Metadata`. Expected shape:
///
/// ```python
/// {
///     "recorder": {"language": str, "language_version": str,
///                  "recorder_version": str, "platform": str},
///     "recording": {"program": str,
///                   "working_directory": str | None,
///                   "scope_config": {"include": [str], "exclude": [str],
///                                    "depth_limit": int | None}},
///     "program": {"<key>": "<value>", ...} | None,
///     "trace_uuid": bytes (length 16),
///     "recording_start_ns": int,
/// }
/// ```
fn metadata_from_dict(dict: &Bound<'_, PyDict>) -> PyResult<Metadata> {
    let recorder = get_required_dict(dict, "recorder")?;
    let recording = get_required_dict(dict, "recording")?;
    let scope_config = get_required_dict(&recording, "scope_config")?;

    let trace_uuid_bytes: Vec<u8> = get_item(dict, "trace_uuid")?
        .ok_or_else(|| PyValueError::new_err("metadata: missing 'trace_uuid'"))?
        .extract()?;
    if trace_uuid_bytes.len() != 16 {
        return Err(PyValueError::new_err(format!(
            "metadata: 'trace_uuid' must be 16 bytes, got {}",
            trace_uuid_bytes.len()
        )));
    }
    let mut trace_uuid = [0u8; 16];
    trace_uuid.copy_from_slice(&trace_uuid_bytes);

    let recording_start_ns: u64 = get_item(dict, "recording_start_ns")?
        .ok_or_else(|| PyValueError::new_err("metadata: missing 'recording_start_ns'"))?
        .extract()?;

    let recorder = RecorderInfo {
        language: extract_string(&recorder, "language")?,
        language_version: extract_string(&recorder, "language_version")?,
        recorder_version: extract_string(&recorder, "recorder_version")?,
        platform: extract_string(&recorder, "platform")?,
    };

    let recording = RecordingInfo {
        program: extract_string(&recording, "program")?,
        working_directory: extract_optional_string(&recording, "working_directory")?,
        scope_config: ScopeConfig {
            include: extract_optional_string_list(&scope_config, "include")?.unwrap_or_default(),
            exclude: extract_optional_string_list(&scope_config, "exclude")?.unwrap_or_default(),
            depth_limit: get_item(&scope_config, "depth_limit")?
                .filter(|v| !v.is_none())
                .map(|v| v.extract::<u32>())
                .transpose()?,
        },
    };

    let program = match get_item(dict, "program")? {
        Some(v) if !v.is_none() => {
            let d: Bound<'_, PyDict> = v
                .downcast_into()
                .map_err(|_| PyValueError::new_err("metadata: 'program' must be a dict or None"))?;
            let mut fields = Vec::with_capacity(d.len());
            for (k, val) in d.iter() {
                fields.push((k.extract::<String>()?, val.extract::<String>()?));
            }
            Some(ProgramInfo { fields })
        }
        _ => None,
    };

    Ok(Metadata {
        recorder,
        recording,
        program,
        trace_uuid,
        recording_start_ns,
    })
}

fn get_item<'py>(d: &Bound<'py, PyDict>, key: &str) -> PyResult<Option<Bound<'py, PyAny>>> {
    d.get_item(key)
}

fn get_required_dict<'py>(d: &Bound<'py, PyDict>, key: &str) -> PyResult<Bound<'py, PyDict>> {
    let v = get_item(d, key)?
        .ok_or_else(|| PyValueError::new_err(format!("metadata: missing '{key}'")))?;
    v.downcast_into::<PyDict>()
        .map_err(|_| PyValueError::new_err(format!("metadata: '{key}' must be a dict")))
}

fn extract_string(d: &Bound<'_, PyDict>, key: &str) -> PyResult<String> {
    get_item(d, key)?
        .ok_or_else(|| PyValueError::new_err(format!("metadata: missing '{key}'")))?
        .extract::<String>()
}

fn extract_optional_string(d: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<String>> {
    match get_item(d, key)? {
        Some(v) if !v.is_none() => Ok(Some(v.extract()?)),
        _ => Ok(None),
    }
}

fn extract_optional_string_list(d: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<Vec<String>>> {
    match get_item(d, key)? {
        Some(v) if !v.is_none() => Ok(Some(v.extract()?)),
        _ => Ok(None),
    }
}

// --- Value conversion -------------------------------------------------------

/// Walk a Python value, recursively interning children before parents, and
/// return the parent's `ValueId`.
///
/// **Type-dispatch order matters** for two reasons:
/// - `bool` is a subclass of `int` in Python, so the bool check has to win
///   over the int check.
/// - The summary fallback must come last so it doesn't swallow real types we
///   know how to encode inline.
fn convert_value(
    writer: &mut hindsight_format::TraceWriter,
    obj: &Bound<'_, PyAny>,
) -> PyResult<ValueId> {
    if obj.is_none() {
        return Ok(NONE_VALUE_ID);
    }

    if obj.is_instance_of::<PyBool>() {
        let b: bool = obj.extract()?;
        return Ok(writer.intern_value_inline(Value::Bool(b)));
    }

    if obj.is_instance_of::<PyInt>() {
        return convert_int(writer, obj);
    }

    if obj.is_instance_of::<PyFloat>() {
        let f: f64 = obj.extract()?;
        return Ok(writer.intern_value_inline(Value::Float(f)));
    }

    if obj.is_instance_of::<PyString>() {
        let s: String = obj.extract()?;
        return Ok(writer.intern_value_inline(Value::String(s)));
    }

    if obj.is_instance_of::<PyBytes>() {
        let b: Vec<u8> = obj.extract()?;
        return Ok(writer.intern_value_inline(Value::Bytes(b)));
    }

    if let Ok(list) = obj.downcast::<PyList>() {
        let mut child_ids = Vec::with_capacity(list.len());
        for item in list.iter() {
            child_ids.push(convert_value(writer, &item)?);
        }
        return Ok(writer.intern_value_inline(Value::List(child_ids)));
    }

    if let Ok(tuple) = obj.downcast::<PyTuple>() {
        let mut child_ids = Vec::with_capacity(tuple.len());
        for item in tuple.iter() {
            child_ids.push(convert_value(writer, &item)?);
        }
        return Ok(writer.intern_value_inline(Value::List(child_ids)));
    }

    if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut pairs = Vec::with_capacity(dict.len());
        for (k, v) in dict.iter() {
            let kid = convert_value(writer, &k)?;
            let vid = convert_value(writer, &v)?;
            pairs.push((kid, vid));
        }
        return Ok(writer.intern_value_inline(Value::Dict(pairs)));
    }

    if let Ok(set) = obj.downcast::<PySet>() {
        let mut ids = Vec::with_capacity(set.len());
        for item in set.iter() {
            ids.push(convert_value(writer, &item)?);
        }
        return Ok(writer.intern_value_inline(Value::Set(ids)));
    }

    if let Ok(frozen) = obj.downcast::<PyFrozenSet>() {
        let mut ids = Vec::with_capacity(frozen.len());
        for item in frozen.iter() {
            ids.push(convert_value(writer, &item)?);
        }
        return Ok(writer.intern_value_inline(Value::Set(ids)));
    }

    convert_summary(writer, obj)
}

/// Try `extract::<i64>()` first; fall back to canonical-bytes BigInt when the
/// Python int is outside i64. We intentionally trust Python's `int.to_bytes`
/// to produce the canonical (two's-complement, big-endian, minimum-length)
/// bytes the writer requires — see `Value::BigInt`'s caller-contract docs.
fn convert_int(
    writer: &mut hindsight_format::TraceWriter,
    obj: &Bound<'_, PyAny>,
) -> PyResult<ValueId> {
    match obj.extract::<i64>() {
        Ok(i) => Ok(writer.intern_value_inline(Value::Int(i))),
        Err(_) => {
            let bytes = python_int_to_canonical_bytes(obj)?;
            Ok(writer.intern_value_inline(Value::BigInt(bytes)))
        }
    }
}

/// Compute minimum-byte-length two's-complement big-endian bytes for an
/// arbitrary-precision Python int by calling `int.bit_length()` and
/// `int.to_bytes(length, "big", signed=True)`.
///
/// `length = (bit_length + 8) // 8` — the +8 reserves one bit for sign on
/// positive values and one byte for sign-extension on negatives, while
/// staying minimal.
fn python_int_to_canonical_bytes(obj: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    let py = obj.py();
    let bit_length: u32 = obj.call_method0("bit_length")?.extract()?;
    let length: u64 = (u64::from(bit_length) + 8) / 8;

    let kwargs = PyDict::new(py);
    kwargs.set_item("signed", true)?;
    let bytes_obj = obj
        .call_method("to_bytes", (length, "big"), Some(&kwargs))
        .map_err(|e| PyOverflowError::new_err(format!("int.to_bytes failed: {e}")))?;
    bytes_obj.extract::<Vec<u8>>()
}

/// Summary fallback for any Python type we don't know how to inline.
/// type_name = `type(obj).__qualname__`. length is set to 0 (placeholder —
/// see spec note: the length field is type-defined and not meaningful for
/// arbitrary objects). repr is `repr(obj)` truncated at 256 chars.
fn convert_summary(
    writer: &mut hindsight_format::TraceWriter,
    obj: &Bound<'_, PyAny>,
) -> PyResult<ValueId> {
    let type_name: String = obj.get_type().qualname()?.extract()?;
    let repr_full: String = obj.repr()?.extract()?;
    let repr_truncated: String = repr_full.chars().take(SUMMARY_REPR_MAX_CHARS).collect();

    let type_name_id = writer.intern_string(type_name);
    let repr_id = writer.intern_string(repr_truncated);
    writer
        .intern_value_summary(type_name_id, 0, repr_id)
        .map_err(format_err)
}

// --- TraceWriter pyclass ----------------------------------------------------

/// Python-facing trace writer. Wraps `hindsight_format::TraceWriter`.
///
/// `inner` is `Option` so `finish` / `finish_to_bytes` can take ownership of
/// the underlying writer (the Rust `finish` consumes `self`). Every other
/// method goes through `inner_mut()` which raises a Python `RuntimeError` if
/// the writer was already finished.
#[pyclass(name = "TraceWriter", module = "hindsight._core")]
struct PyTraceWriter {
    inner: Option<hindsight_format::TraceWriter>,
}

#[pymethods]
impl PyTraceWriter {
    /// Construct a new writer. `metadata` is a dict; see `metadata_from_dict`
    /// for the expected shape.
    #[new]
    fn new(metadata: &Bound<'_, PyDict>) -> PyResult<Self> {
        let metadata = metadata_from_dict(metadata)?;
        Ok(Self {
            inner: Some(hindsight_format::TraceWriter::new(metadata)),
        })
    }

    /// Add a source file. Returns the assigned file ID. Duplicate paths
    /// return the existing ID without overwriting content.
    fn add_source_file(&mut self, path: &str, content: &str) -> PyResult<u64> {
        let w = self.inner_mut()?;
        Ok(w.add_source_file(path.to_string(), content.as_bytes().to_vec()))
    }

    /// Intern a string. Returns its string ID; same content returns same ID.
    fn intern_string(&mut self, s: &str) -> PyResult<u64> {
        let w = self.inner_mut()?;
        Ok(w.intern_string(s.to_string()))
    }

    /// Intern a Python value, recursively interning children. Returns the
    /// value ID. See `convert_value` for type dispatch and the summary
    /// fallback for unknown types.
    fn intern_value(&mut self, value: &Bound<'_, PyAny>) -> PyResult<u64> {
        let w = self.inner_mut()?;
        convert_value(w, value)
    }

    /// Intern a summary value directly: pre-interned `type_name` string ID,
    /// a length (type-defined), and a pre-interned `repr` string ID.
    fn intern_value_summary(&mut self, type_name: u64, length: u64, repr: u64) -> PyResult<u64> {
        let w = self.inner_mut()?;
        w.intern_value_summary(type_name, length, repr)
            .map_err(format_err)
    }

    /// Intern a value with a caller-provided 16-byte identity hash. The
    /// Python value's structure is converted as for `intern_value` (children
    /// content-hashed); the parent itself goes in with hash kind = Identity.
    fn intern_value_with_identity(
        &mut self,
        value: &Bound<'_, PyAny>,
        identity_hash: &[u8],
    ) -> PyResult<u64> {
        if identity_hash.len() != 16 {
            return Err(PyValueError::new_err(format!(
                "identity_hash must be exactly 16 bytes, got {}",
                identity_hash.len()
            )));
        }
        let mut hash = [0u8; 16];
        hash.copy_from_slice(identity_hash);

        let w = self.inner_mut()?;
        let value = python_value_to_rust(w, value)?;
        Ok(w.intern_value_with_identity(value, hash))
    }

    /// Emit an "equivalent" alias: a fresh value_id whose content is declared
    /// to match `aliased_value_id`. `confidence_str` is one of
    /// "content_exact", "mutation_tracked", "dirty_reconciled",
    /// "summary_observed", "uncertain_external".
    ///
    /// O(1). The recorder uses this when a container's summary fingerprint
    /// matches the previous capture (length + endpoint identity unchanged).
    fn intern_value_alias_equivalent(
        &mut self,
        aliased_value_id: u64,
        confidence_str: &str,
    ) -> PyResult<u64> {
        let confidence = parse_confidence(confidence_str)?;
        let w = self.inner_mut()?;
        w.intern_value_alias(AliasKind::Equivalent, aliased_value_id, confidence)
            .map_err(format_err)
    }

    /// Emit a "grown" alias: a fresh value_id representing the aliased
    /// container with `new_elements` appended. For dicts, `new_elements` is a
    /// flat list of alternating key, value, key, value... ids.
    ///
    /// O(k) in the number of new elements. The recorder uses this for
    /// append-in-loop patterns: capture the new tail elements, alias the
    /// rest.
    fn intern_value_alias_grown(
        &mut self,
        aliased_value_id: u64,
        new_elements: Vec<u64>,
        confidence_str: &str,
    ) -> PyResult<u64> {
        let confidence = parse_confidence(confidence_str)?;
        let w = self.inner_mut()?;
        w.intern_value_alias(
            AliasKind::Grown { new_elements },
            aliased_value_id,
            confidence,
        )
        .map_err(format_err)
    }

    /// Emit a "patch" alias: a fresh value_id representing the aliased
    /// container with one element replaced at the given position.
    ///
    /// For lists/tuples/sets, ``position`` is the integer index. For
    /// dicts, ``position`` is the pair index — the value half is
    /// replaced by ``new_element_value_id``, the key half stays.
    ///
    /// O(1). The recorder uses this for in-place index assignment
    /// (``lst[i] = x``, ``dict[k] = v``) when the static analyzer
    /// pinned the (container, key) at a STORE_SUBSCR offset.
    fn intern_value_alias_patch(
        &mut self,
        aliased_value_id: u64,
        position: u64,
        new_element_value_id: u64,
        confidence_str: &str,
    ) -> PyResult<u64> {
        let confidence = parse_confidence(confidence_str)?;
        let w = self.inner_mut()?;
        w.intern_value_alias(
            AliasKind::Patch {
                position,
                new_element_value_id,
            },
            aliased_value_id,
            confidence,
        )
        .map_err(format_err)
    }

    /// Intern a Python container without re-walking it: takes a list of
    /// pre-interned child value_ids and emits an inline list/set/dict.
    ///
    /// This is the bypass for the recorder's "I already have child ids
    /// from per-element intern calls" path. Unlike `intern_value`, this
    /// doesn't walk the Python object — the recorder is responsible for
    /// passing the right child ids in order.
    ///
    /// `kind_str` selects the container shape:
    /// - "list" or "tuple" → Value::List (tag 0x07)
    /// - "set" or "frozenset" → Value::Set (tag 0x09)
    /// - "dict" → Value::Dict (interpret children as alternating k,v pairs)
    fn intern_value_container_from_children(
        &mut self,
        kind_str: &str,
        children: Vec<u64>,
    ) -> PyResult<u64> {
        let w = self.inner_mut()?;
        let value = match kind_str {
            "list" | "tuple" => Value::List(children),
            "set" | "frozenset" => Value::Set(children),
            "dict" => {
                if !children.len().is_multiple_of(2) {
                    return Err(PyValueError::new_err(
                        "dict requires an even-length children list (alternating k,v)",
                    ));
                }
                let pairs: Vec<(u64, u64)> =
                    children.chunks_exact(2).map(|c| (c[0], c[1])).collect();
                Value::Dict(pairs)
            }
            other => {
                return Err(PyValueError::new_err(format!(
                    "unknown container kind {other:?}; expected list/tuple/set/frozenset/dict"
                )));
            }
        };
        Ok(w.intern_value_inline(value))
    }

    /// Write a FUNCTION_ENTRY event. `args` is an iterable of
    /// `(string_id, value_id)` tuples, one per argument.
    fn write_function_entry(
        &mut self,
        timestamp_delta_ns: u64,
        frame_id: u64,
        function_id: u64,
        source_file_id: u64,
        line: u32,
        args: Vec<(u64, u64)>,
    ) -> PyResult<()> {
        let w = self.inner_mut()?;
        w.write_function_entry(FunctionEntry {
            timestamp_delta_ns,
            frame_id,
            function_id,
            source_file_id,
            line,
            args: args
                .into_iter()
                .map(|(name, value)| Argument { name, value })
                .collect(),
        })
        .map_err(format_err)
    }

    /// Write a FUNCTION_EXIT event.
    fn write_function_exit(
        &mut self,
        timestamp_delta_ns: u64,
        frame_id: u64,
        return_value: u64,
    ) -> PyResult<()> {
        let w = self.inner_mut()?;
        w.write_function_exit(FunctionExit {
            timestamp_delta_ns,
            frame_id,
            return_value,
        })
        .map_err(format_err)
    }

    /// Write a FRAME_SNAPSHOT event. `locals` is `(string_id, value_id)` pairs.
    fn write_frame_snapshot(
        &mut self,
        timestamp_delta_ns: u64,
        frame_id: u64,
        line: u32,
        locals: Vec<(u64, u64)>,
    ) -> PyResult<()> {
        let w = self.inner_mut()?;
        w.write_frame_snapshot(FrameSnapshot {
            timestamp_delta_ns,
            frame_id,
            line,
            locals: locals
                .into_iter()
                .map(|(name, value)| Local { name, value })
                .collect(),
        })
        .map_err(format_err)
    }

    /// Write a LINE_DELTA event. `changes` is `(string_id, value_id)` pairs.
    fn write_line_delta(
        &mut self,
        timestamp_delta_ns: u64,
        line: u32,
        changes: Vec<(u64, u64)>,
    ) -> PyResult<()> {
        let w = self.inner_mut()?;
        w.write_line_delta(LineDelta {
            timestamp_delta_ns,
            line,
            changes: changes
                .into_iter()
                .map(|(name, value)| Change { name, value })
                .collect(),
        })
        .map_err(format_err)
    }

    /// Write a BRANCH_RESULT event. ``taken`` is the boolean truth value
    /// of the branch's condition: ``True`` if the condition evaluated
    /// truthy, ``False`` if falsy. The Python recorder computes this
    /// from the BRANCH callback by disassembling the branch opcode and
    /// comparing the destination offset to the next sequential offset.
    fn write_branch_result(
        &mut self,
        timestamp_delta_ns: u64,
        line: u32,
        taken: bool,
    ) -> PyResult<()> {
        let w = self.inner_mut()?;
        w.write_branch_result(BranchResult {
            timestamp_delta_ns,
            line,
            taken,
        })
        .map_err(format_err)
    }

    /// Write an EXCEPTION_RAISED event. ``exception_type`` is a string
    /// ID for the qualified exception class name (e.g.
    /// ``"builtins.ValueError"``). ``exception_value`` is a value ID
    /// for the exception instance summary.
    fn write_exception_raised(
        &mut self,
        timestamp_delta_ns: u64,
        line: u32,
        exception_type: u64,
        exception_value: u64,
    ) -> PyResult<()> {
        let w = self.inner_mut()?;
        w.write_exception_raised(ExceptionRaised {
            timestamp_delta_ns,
            line,
            exception_type,
            exception_value,
        })
        .map_err(format_err)
    }

    /// Write a NOTE event (``hindsight.note(message, **kwargs)``).
    /// ``message`` is a pre-interned string ID for the note's main
    /// text. ``kwargs`` is a list of ``(name_string_id, value_id)``
    /// pairs, mirroring the FUNCTION_ENTRY ``args`` format.
    fn write_note(
        &mut self,
        timestamp_delta_ns: u64,
        line: u32,
        message: u64,
        kwargs: Vec<(u64, u64)>,
    ) -> PyResult<()> {
        let w = self.inner_mut()?;
        w.write_note(Note {
            timestamp_delta_ns,
            line,
            message,
            kwargs: kwargs
                .into_iter()
                .map(|(name, value)| Kwarg { name, value })
                .collect(),
        })
        .map_err(format_err)
    }

    /// The reserved value ID for the "exception unwind sentinel" — the
    /// return value to use in ``write_function_exit`` when a frame
    /// exits via exception unwind rather than a normal return. Exposed
    /// as a class attribute so the Python recorder doesn't have to
    /// hardcode the constant.
    #[classattr]
    #[allow(non_snake_case)]
    fn EXCEPTION_UNWIND_VALUE_ID() -> u64 {
        EXCEPTION_UNWIND_VALUE_ID
    }

    /// Write a SCOPE_BOUNDARY event. `boundary_type` is one of:
    ///
    /// - `0x01` entered_skip / `0x02` exited_skip — `hindsight.skip()` block boundary
    /// - `0x03` entered_excluded / `0x04` exited_excluded — call to a function
    ///   matched by an exclude pattern
    /// - `0x05` entered_depth_clipped / `0x06` exited_depth_clipped — call past
    ///   the configured `depth_limit`
    ///
    /// `reason` is a string ID for a free-form description (e.g.,
    /// `"matched pattern: numpy.*"`). The Python recorder interns the
    /// reason string itself before calling this.
    fn write_scope_boundary(
        &mut self,
        timestamp_delta_ns: u64,
        boundary_type: u8,
        reason: u64,
    ) -> PyResult<()> {
        let bt = BoundaryType::from_u8(boundary_type).map_err(format_err)?;
        let w = self.inner_mut()?;
        w.write_scope_boundary(ScopeBoundary {
            timestamp_delta_ns,
            boundary_type: bt,
            reason,
        })
        .map_err(format_err)
    }

    /// Finalize the trace and write it to a file at `path`. After this call
    /// the writer is consumed; further methods raise `RuntimeError`.
    ///
    /// `scope_resolution` is the optional resolved scope info that lands in
    /// the trace's final summary block (see `scope_resolution_from_dict`
    /// for the dict shape). Pass `None` for an empty resolution; tests and
    /// the v0 recorder always pass a populated dict.
    #[pyo3(signature = (path, recording_end_ns, scope_resolution=None))]
    fn finish(
        &mut self,
        path: PathBuf,
        recording_end_ns: u64,
        scope_resolution: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<()> {
        let resolution = match scope_resolution {
            Some(d) => scope_resolution_from_dict(d)?,
            None => ScopeResolution::default(),
        };
        let writer = self.take_writer()?;
        let bytes = writer
            .finish_to_bytes(Finalization {
                recording_end_ns,
                scope_resolution: resolution,
            })
            .map_err(format_err)?;
        std::fs::write(path, bytes).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }

    /// Finalize the trace and return its bytes. Convenience for in-memory
    /// callers (tests). See `finish` for `scope_resolution` semantics.
    #[pyo3(signature = (recording_end_ns, scope_resolution=None))]
    fn finish_to_bytes<'py>(
        &mut self,
        py: Python<'py>,
        recording_end_ns: u64,
        scope_resolution: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let resolution = match scope_resolution {
            Some(d) => scope_resolution_from_dict(d)?,
            None => ScopeResolution::default(),
        };
        let writer = self.take_writer()?;
        let bytes = writer
            .finish_to_bytes(Finalization {
                recording_end_ns,
                scope_resolution: resolution,
            })
            .map_err(format_err)?;
        Ok(PyBytes::new(py, &bytes))
    }
}

/// Convert the Python-side scope-resolution dict into the Rust struct.
///
/// Expected dict shape (any field may be omitted; missing fields default
/// to empty/zero):
/// ```python
/// {
///     "recorded_functions": [str, ...],
///     "excluded_functions": [(name: str, matched_pattern: str), ...],
///     "skip_blocks_observed": int,
///     "depth_clips_observed": int,
/// }
/// ```
fn scope_resolution_from_dict(d: &Bound<'_, PyDict>) -> PyResult<ScopeResolution> {
    let recorded_functions: Vec<String> = match d.get_item("recorded_functions")? {
        Some(v) if !v.is_none() => v.extract()?,
        _ => Vec::new(),
    };
    let excluded_functions: Vec<ExcludedFunction> = match d.get_item("excluded_functions")? {
        Some(v) if !v.is_none() => {
            let pairs: Vec<(String, String)> = v.extract()?;
            pairs
                .into_iter()
                .map(|(name, matched_pattern)| ExcludedFunction {
                    name,
                    matched_pattern,
                })
                .collect()
        }
        _ => Vec::new(),
    };
    let skip_blocks_observed: u32 = match d.get_item("skip_blocks_observed")? {
        Some(v) if !v.is_none() => v.extract()?,
        _ => 0,
    };
    let depth_clips_observed: u32 = match d.get_item("depth_clips_observed")? {
        Some(v) if !v.is_none() => v.extract()?,
        _ => 0,
    };
    Ok(ScopeResolution {
        recorded_functions,
        excluded_functions,
        skip_blocks_observed,
        depth_clips_observed,
    })
}

impl PyTraceWriter {
    fn inner_mut(&mut self) -> PyResult<&mut hindsight_format::TraceWriter> {
        self.inner
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("TraceWriter has already been finished"))
    }

    fn take_writer(&mut self) -> PyResult<hindsight_format::TraceWriter> {
        self.inner
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("TraceWriter has already been finished"))
    }
}

/// Convert a Python value to a Rust `Value` *and* intern any container
/// children, returning the parent value uninterned. Used by
/// `intern_value_with_identity`, where the caller wants Identity hashing on
/// the parent but content hashing on the children.
fn python_value_to_rust(
    writer: &mut hindsight_format::TraceWriter,
    obj: &Bound<'_, PyAny>,
) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::None);
    }
    if obj.is_instance_of::<PyBool>() {
        return Ok(Value::Bool(obj.extract()?));
    }
    if obj.is_instance_of::<PyInt>() {
        return match obj.extract::<i64>() {
            Ok(i) => Ok(Value::Int(i)),
            Err(_) => Ok(Value::BigInt(python_int_to_canonical_bytes(obj)?)),
        };
    }
    if obj.is_instance_of::<PyFloat>() {
        return Ok(Value::Float(obj.extract()?));
    }
    if obj.is_instance_of::<PyString>() {
        return Ok(Value::String(obj.extract()?));
    }
    if obj.is_instance_of::<PyBytes>() {
        return Ok(Value::Bytes(obj.extract()?));
    }
    if let Ok(list) = obj.downcast::<PyList>() {
        let mut ids = Vec::with_capacity(list.len());
        for item in list.iter() {
            ids.push(convert_value(writer, &item)?);
        }
        return Ok(Value::List(ids));
    }
    if let Ok(tuple) = obj.downcast::<PyTuple>() {
        let mut ids = Vec::with_capacity(tuple.len());
        for item in tuple.iter() {
            ids.push(convert_value(writer, &item)?);
        }
        return Ok(Value::List(ids));
    }
    if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut pairs = Vec::with_capacity(dict.len());
        for (k, v) in dict.iter() {
            pairs.push((convert_value(writer, &k)?, convert_value(writer, &v)?));
        }
        return Ok(Value::Dict(pairs));
    }
    if let Ok(set) = obj.downcast::<PySet>() {
        let mut ids = Vec::with_capacity(set.len());
        for item in set.iter() {
            ids.push(convert_value(writer, &item)?);
        }
        return Ok(Value::Set(ids));
    }
    if let Ok(frozen) = obj.downcast::<PyFrozenSet>() {
        let mut ids = Vec::with_capacity(frozen.len());
        for item in frozen.iter() {
            ids.push(convert_value(writer, &item)?);
        }
        return Ok(Value::Set(ids));
    }
    Err(PyValueError::new_err(
        "intern_value_with_identity: value type cannot be Identity-hashed (only inlinable types are supported)",
    ))
}

fn format_err<E: std::fmt::Display>(e: E) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// Parse a Python-side confidence string (the snake_case form used by the
/// indexer's `confidence` column) into a Rust `Confidence` enum.
fn parse_confidence(s: &str) -> PyResult<Confidence> {
    match s {
        "content_exact" => Ok(Confidence::ContentExact),
        "mutation_tracked" => Ok(Confidence::MutationTracked),
        "dirty_reconciled" => Ok(Confidence::DirtyReconciled),
        "summary_observed" => Ok(Confidence::SummaryObserved),
        "uncertain_external" => Ok(Confidence::UncertainExternal),
        other => Err(PyValueError::new_err(format!(
            "unknown confidence {other:?}; expected one of \
             content_exact, mutation_tracked, dirty_reconciled, \
             summary_observed, uncertain_external"
        ))),
    }
}

// --- Reader -----------------------------------------------------------------

/// Read a `.hindsight` trace from disk and return its contents as a Python
/// dict suitable for inspection in tests. See `trace_to_dict` for the shape.
#[pyfunction]
fn read_trace(py: Python<'_>, path: PathBuf) -> PyResult<Py<PyDict>> {
    let bytes = std::fs::read(path).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    let reader = TraceReader::from_bytes(&bytes).map_err(format_err)?;
    Ok(trace_to_dict(py, &reader)?.unbind())
}

/// Build the dict shape returned by `read_trace`. Values are decoded eagerly,
/// recursively building Python equivalents — so a list value comes back as a
/// real Python list of resolved children, not a list of value IDs.
fn trace_to_dict<'py>(py: Python<'py>, reader: &TraceReader) -> PyResult<Bound<'py, PyDict>> {
    let out = PyDict::new(py);

    out.set_item("is_finalized", reader.is_finalized())?;

    let header = reader.header();
    let header_dict = PyDict::new(py);
    header_dict.set_item("trace_uuid", PyBytes::new(py, &header.trace_uuid))?;
    header_dict.set_item("recording_start_ns", header.recording_start_ns)?;
    header_dict.set_item("recording_end_ns", header.recording_end_ns)?;
    out.set_item("header", header_dict)?;

    out.set_item("metadata_toml", &reader.metadata().payload)?;

    let source_files = PyList::empty(py);
    for sf in reader.source_files() {
        source_files.append(source_file_to_dict(py, sf)?)?;
    }
    out.set_item("source_files", source_files)?;

    let strings = PyList::empty(py);
    for s in reader.strings() {
        strings.append(s)?;
    }
    out.set_item("strings", strings)?;

    let decoded_cache: Vec<Py<PyAny>> = decode_all_values(py, reader)?;

    let values = PyList::empty(py);
    for (i, ve) in reader.values().iter().enumerate() {
        values.append(value_entry_to_dict(py, ve, &decoded_cache, i)?)?;
    }
    out.set_item("values", values)?;

    let events = PyList::empty(py);
    for e in reader.events() {
        events.append(event_to_dict(py, e)?)?;
    }
    out.set_item("events", events)?;

    if let Some(s) = reader.final_summary() {
        out.set_item("final_summary_toml", &s.payload)?;
    } else {
        out.set_item("final_summary_toml", py.None())?;
    }

    Ok(out)
}

fn source_file_to_dict<'py>(py: Python<'py>, sf: &SourceFile) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("file_id", sf.file_id)?;
    d.set_item("path", &sf.path)?;
    d.set_item("content", PyBytes::new(py, &sf.content))?;
    d.set_item("blake3_hash", PyBytes::new(py, &sf.blake3_hash))?;
    Ok(d)
}

/// Decode every value in the value table to its Python equivalent, in
/// table order. Containers reference earlier IDs (the writer enforces no
/// forward refs), so a single forward pass is enough.
fn decode_all_values(py: Python<'_>, reader: &TraceReader) -> PyResult<Vec<Py<PyAny>>> {
    let entries = reader.values();
    let mut decoded: Vec<Py<PyAny>> = Vec::with_capacity(entries.len());
    for (i, ve) in entries.iter().enumerate() {
        let v = decode_value_entry(py, ve, &decoded, i, reader)?;
        decoded.push(v);
    }
    Ok(decoded)
}

fn decode_value_entry(
    py: Python<'_>,
    ve: &ValueEntry,
    decoded_so_far: &[Py<PyAny>],
    current_index: usize,
    reader: &TraceReader,
) -> PyResult<Py<PyAny>> {
    let resolve = |id: ValueId| -> PyResult<Py<PyAny>> {
        let id = id as usize;
        if id >= current_index {
            return Err(PyRuntimeError::new_err(format!(
                "value table forward ref: value[{current_index}] points to value[{id}]"
            )));
        }
        Ok(decoded_so_far[id].clone_ref(py))
    };

    let obj: Py<PyAny> = match &ve.value {
        Value::None => py.None(),
        Value::ExceptionUnwindSentinel => {
            // Distinguish from None for tests that care to check.
            let d = PyDict::new(py);
            d.set_item("kind", "exception_unwind_sentinel")?;
            d.into_any().unbind()
        }
        Value::Bool(b) => PyBool::new(py, *b).to_owned().into_any().unbind(),
        Value::Int(i) => i.into_pyobject(py)?.into_any().unbind(),
        Value::BigInt(bytes) => {
            let int_type = py.import("builtins")?.getattr("int")?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("signed", true)?;
            int_type
                .call_method(
                    "from_bytes",
                    (PyBytes::new(py, bytes), "big"),
                    Some(&kwargs),
                )?
                .unbind()
        }
        Value::Float(f) => f.into_pyobject(py)?.into_any().unbind(),
        Value::String(s) => s.into_pyobject(py)?.into_any().unbind(),
        Value::Bytes(b) => PyBytes::new(py, b).into_any().unbind(),
        Value::List(ids) => {
            let list = PyList::empty(py);
            for id in ids {
                list.append(resolve(*id)?)?;
            }
            list.into_any().unbind()
        }
        Value::Dict(pairs) => {
            let dict = PyDict::new(py);
            for (k, v) in pairs {
                dict.set_item(resolve(*k)?, resolve(*v)?)?;
            }
            dict.into_any().unbind()
        }
        Value::Set(ids) => {
            let set = PySet::empty(py)?;
            for id in ids {
                set.add(resolve(*id)?)?;
            }
            set.into_any().unbind()
        }
        Value::CycleRef(depth) => {
            let d = PyDict::new(py);
            d.set_item("kind", "cycle_ref")?;
            d.set_item("depth", *depth)?;
            d.into_any().unbind()
        }
        Value::Summary {
            type_name,
            length,
            repr,
        } => {
            let d = PyDict::new(py);
            d.set_item("kind", "summary")?;
            d.set_item("type_name", &reader.strings()[*type_name as usize])?;
            d.set_item("length", *length)?;
            d.set_item("repr", &reader.strings()[*repr as usize])?;
            d.into_any().unbind()
        }
        Value::TypeRef(string_id) => {
            let d = PyDict::new(py);
            d.set_item("kind", "type_ref")?;
            d.set_item("type_name", &reader.strings()[*string_id as usize])?;
            d.into_any().unbind()
        }
        Value::Alias {
            kind,
            aliased_value_id,
            confidence,
        } => {
            let d = PyDict::new(py);
            d.set_item("kind", "alias")?;
            d.set_item("aliased_value_id", *aliased_value_id)?;
            d.set_item("confidence", confidence.as_str())?;
            match kind {
                hindsight_format::AliasKind::Equivalent => {
                    d.set_item("alias_kind", "equivalent")?;
                }
                hindsight_format::AliasKind::Grown { new_elements } => {
                    d.set_item("alias_kind", "grown")?;
                    d.set_item("new_elements", new_elements.clone())?;
                }
                hindsight_format::AliasKind::Patch {
                    position,
                    new_element_value_id,
                } => {
                    d.set_item("alias_kind", "patch")?;
                    d.set_item("position", *position)?;
                    d.set_item("new_element_value_id", *new_element_value_id)?;
                }
            }
            d.into_any().unbind()
        }
    };
    Ok(obj)
}

fn value_entry_to_dict<'py>(
    py: Python<'py>,
    ve: &ValueEntry,
    decoded_cache: &[Py<PyAny>],
    index: usize,
) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("value_id", index as u64)?;
    d.set_item("tag", value_tag_byte(ve.value.tag()))?;
    d.set_item("hash_kind", hash_kind_byte(ve.hash_kind))?;
    d.set_item("hash", PyBytes::new(py, &ve.hash))?;
    d.set_item("decoded", decoded_cache[index].clone_ref(py))?;
    Ok(d)
}

fn value_tag_byte(tag: ValueTag) -> u8 {
    tag.as_u8()
}

fn hash_kind_byte(kind: HashKind) -> u8 {
    kind.as_u8()
}

fn event_to_dict<'py>(py: Python<'py>, event: &Event) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("type_tag", event_tag_byte(event.tag()))?;
    d.set_item("timestamp_delta_ns", event.timestamp_delta_ns())?;
    match event {
        Event::FunctionEntry(e) => {
            d.set_item("type", "function_entry")?;
            d.set_item("frame_id", e.frame_id)?;
            d.set_item("function_id", e.function_id)?;
            d.set_item("source_file_id", e.source_file_id)?;
            d.set_item("line", e.line)?;
            d.set_item(
                "args",
                e.args
                    .iter()
                    .map(|a| (a.name, a.value))
                    .collect::<Vec<(u64, u64)>>(),
            )?;
        }
        Event::FunctionExit(e) => {
            d.set_item("type", "function_exit")?;
            d.set_item("frame_id", e.frame_id)?;
            d.set_item("return_value", e.return_value)?;
        }
        Event::FrameSnapshot(e) => {
            d.set_item("type", "frame_snapshot")?;
            d.set_item("frame_id", e.frame_id)?;
            d.set_item("line", e.line)?;
            d.set_item(
                "locals",
                e.locals
                    .iter()
                    .map(|l| (l.name, l.value))
                    .collect::<Vec<(u64, u64)>>(),
            )?;
        }
        Event::LineDelta(e) => {
            d.set_item("type", "line_delta")?;
            d.set_item("line", e.line)?;
            d.set_item(
                "changes",
                e.changes
                    .iter()
                    .map(|c| (c.name, c.value))
                    .collect::<Vec<(u64, u64)>>(),
            )?;
        }
        Event::BranchResult(e) => {
            d.set_item("type", "branch_result")?;
            d.set_item("line", e.line)?;
            d.set_item("taken", e.taken)?;
        }
        Event::ExceptionRaised(e) => {
            d.set_item("type", "exception_raised")?;
            d.set_item("line", e.line)?;
            d.set_item("exception_type", e.exception_type)?;
            d.set_item("exception_value", e.exception_value)?;
        }
        Event::Note(e) => {
            d.set_item("type", "note")?;
            d.set_item("line", e.line)?;
            d.set_item("message", e.message)?;
            d.set_item(
                "kwargs",
                e.kwargs
                    .iter()
                    .map(|k| (k.name, k.value))
                    .collect::<Vec<(u64, u64)>>(),
            )?;
        }
        Event::ScopeBoundary(e) => {
            d.set_item("type", "scope_boundary")?;
            d.set_item("boundary_type", e.boundary_type.as_u8())?;
            d.set_item("reason", e.reason)?;
        }
        Event::FrameSwitch(e) => {
            d.set_item("type", "frame_switch")?;
            d.set_item("old_frame_id", e.old_frame_id)?;
            d.set_item("new_frame_id", e.new_frame_id)?;
            d.set_item("reason", e.reason.as_u8())?;
        }
    }
    Ok(d)
}

fn event_tag_byte(tag: EventTag) -> u8 {
    tag.as_u8()
}

// --- Module registration ----------------------------------------------------

/// Function name must match `[lib].name` in `Cargo.toml` and the leaf of
/// `[tool.maturin].module-name` in `pyproject.toml` — `_core` for all three.
#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyTraceWriter>()?;
    m.add_function(wrap_pyfunction!(read_trace, m)?)?;
    Ok(())
}

// --- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn fresh_writer() -> hindsight_format::TraceWriter {
        hindsight_format::TraceWriter::new(Metadata {
            recorder: RecorderInfo {
                language: "python".into(),
                language_version: "3.12.5".into(),
                recorder_version: "0.1.0".into(),
                platform: "test".into(),
            },
            recording: RecordingInfo {
                program: "pytest".into(),
                working_directory: None,
                scope_config: ScopeConfig::default(),
            },
            program: None,
            trace_uuid: [0; 16],
            recording_start_ns: 0,
        })
    }

    /// Convenience: evaluate `src` as a Python expression, then convert it.
    fn convert(py: Python<'_>, w: &mut hindsight_format::TraceWriter, src: &str) -> ValueId {
        let code = CString::new(src).expect("source contains no null bytes");
        let obj = py.eval(&code, None, None).expect("eval succeeds");
        convert_value(w, &obj).expect("convert_value succeeds")
    }

    #[test]
    fn none_returns_reserved_zero() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let id = convert(py, &mut w, "None");
            assert_eq!(id, NONE_VALUE_ID);
        });
    }

    #[test]
    fn bool_distinct_from_int_zero_and_one() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let true_id = convert(py, &mut w, "True");
            let false_id = convert(py, &mut w, "False");
            let one_id = convert(py, &mut w, "1");
            let zero_id = convert(py, &mut w, "0");
            assert_ne!(true_id, one_id);
            assert_ne!(false_id, zero_id);
            // Self-dedup within a kind:
            let true_again = convert(py, &mut w, "True");
            assert_eq!(true_id, true_again);
        });
    }

    #[test]
    fn small_int_dedups() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let a = convert(py, &mut w, "42");
            let b = convert(py, &mut w, "42");
            assert_eq!(a, b);
            let c = convert(py, &mut w, "43");
            assert_ne!(a, c);
        });
    }

    #[test]
    fn big_int_path_takes_when_outside_i64() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let id_small = convert(py, &mut w, "9223372036854775807"); // i64::MAX
            let id_too_big = convert(py, &mut w, "9223372036854775808"); // i64::MAX + 1
            let id_two_pow_100 = convert(py, &mut w, "2 ** 100");
            assert_ne!(id_small, id_too_big);
            assert_ne!(id_too_big, id_two_pow_100);
            // Round-trip via the reader requires a finalized trace; that's
            // covered by the Python integration test. Here we just verify
            // distinct IDs were produced.
        });
    }

    #[test]
    fn negative_big_int_takes_big_int_path() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            // -2**100 is comfortably outside i64.
            let id_neg = convert(py, &mut w, "-(2 ** 100)");
            let id_pos = convert(py, &mut w, "2 ** 100");
            assert_ne!(id_neg, id_pos);
        });
    }

    #[test]
    fn float_path() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let a = convert(py, &mut w, "1.5");
            let b = convert(py, &mut w, "1.5");
            assert_eq!(a, b);
            let c = convert(py, &mut w, "2.5");
            assert_ne!(a, c);
        });
    }

    #[test]
    fn string_path() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let a = convert(py, &mut w, "'hello'");
            let b = convert(py, &mut w, "'hello'");
            assert_eq!(a, b);
            let c = convert(py, &mut w, "'world'");
            assert_ne!(a, c);
        });
    }

    #[test]
    fn bytes_path() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let a = convert(py, &mut w, "b'abc'");
            let b = convert(py, &mut w, "b'abc'");
            assert_eq!(a, b);
        });
    }

    #[test]
    fn empty_containers() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let l = convert(py, &mut w, "[]");
            let t = convert(py, &mut w, "()");
            // Empty list and empty tuple share Value::List([]).
            assert_eq!(l, t);
            let _d = convert(py, &mut w, "{}");
            let _s = convert(py, &mut w, "set()");
        });
    }

    #[test]
    fn tuple_and_list_share_tag_and_dedup_when_equal() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let lst = convert(py, &mut w, "[1, 2, 3]");
            let tup = convert(py, &mut w, "(1, 2, 3)");
            // Both encode to Value::List with identical children.
            assert_eq!(lst, tup);
            let d = convert(py, &mut w, "{1: 2}");
            assert_ne!(lst, d);
        });
    }

    #[test]
    fn nested_list_of_lists() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let outer = convert(py, &mut w, "[[1, 2], [3, 4]]");
            let outer_again = convert(py, &mut w, "[[1, 2], [3, 4]]");
            assert_eq!(outer, outer_again);
            let inner = convert(py, &mut w, "[1, 2]");
            assert_ne!(outer, inner);
        });
    }

    #[test]
    fn nested_dict_of_dicts() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let outer = convert(py, &mut w, "{'a': {'b': 1}, 'c': {'d': 2}}");
            let outer_again = convert(py, &mut w, "{'a': {'b': 1}, 'c': {'d': 2}}");
            assert_eq!(outer, outer_again);
        });
    }

    #[test]
    fn set_and_frozenset_both_encode_as_value_set() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let s = convert(py, &mut w, "{1, 2, 3}");
            let fs = convert(py, &mut w, "frozenset({1, 2, 3})");
            assert_eq!(s, fs);
        });
    }

    #[test]
    fn summary_fallback_for_arbitrary_class_instance_dedups_same_object() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            let code =
                CString::new("type('Foo', (), {'__repr__': lambda self: 'foo()'})()").unwrap();
            let obj = py.eval(&code, None, None).unwrap();
            let id = convert_value(&mut w, &obj).unwrap();
            let id2 = convert_value(&mut w, &obj).unwrap();
            assert_eq!(id, id2);
        });
    }

    #[test]
    fn repr_truncation_dedups_distinct_instances_with_same_truncated_repr() {
        Python::with_gil(|py| {
            let mut w = fresh_writer();
            // Two distinct instances of the same class, both whose reprs are
            // 1000 'x' chars. After truncation to 256 chars the summaries
            // are identical (same type_name, same truncated repr), so they
            // dedup to a single value table entry.
            let make = "type('Big', (), {'__repr__': lambda self: 'x' * 1000})";
            let code_a = CString::new(format!("{make}()")).unwrap();
            let code_b = CString::new(format!("{make}()")).unwrap();
            let a = py.eval(&code_a, None, None).unwrap();
            let b = py.eval(&code_b, None, None).unwrap();
            let id_a = convert_value(&mut w, &a).unwrap();
            let id_b = convert_value(&mut w, &b).unwrap();
            assert_eq!(id_a, id_b);
        });
    }
}
