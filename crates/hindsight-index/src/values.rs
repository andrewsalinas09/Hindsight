// SPDX-License-Identifier: Apache-2.0

//! Value materialization: convert each `ValueEntry` from the trace into rows
//! in the `values` table (with type-specific columns populated) and rows in
//! `value_elements` for container values.
//!
//! Alias entries (v0.3) are materialized as their effective container: the
//! indexer follows the alias pointer to the source value, copies its type tag
//! / container length / element list into the new row, and (for `Grown`
//! aliases) appends the new tail elements. This means downstream tools never
//! have to follow alias pointers themselves — they query `value_elements` and
//! it just works. The alias pointer is preserved as `values.aliased_value_id`
//! for the rare query that genuinely cares.

use duckdb::{Appender, ToSql, params};
use hindsight_format::{
    AliasKind, Confidence, HashKind, Value, ValueEntry, ValueId, ValueTag, derive_confidence,
};

use crate::error::{IndexError, Result};

/// Lowercase hex-encode a byte slice.
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = vec![0u8; bytes.len() * 2];
    for (i, &b) in bytes.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0x0F) as usize];
    }
    String::from_utf8(out).expect("hex chars are ascii")
}

pub fn type_tag_str(tag: ValueTag) -> &'static str {
    match tag {
        ValueTag::None => "none",
        ValueTag::Bool => "bool",
        ValueTag::IntSmall => "int",
        ValueTag::IntBig => "big_int",
        ValueTag::Float => "float",
        ValueTag::String => "string",
        ValueTag::Bytes => "bytes",
        ValueTag::ListOrTuple => "list",
        ValueTag::Dict => "dict",
        ValueTag::Set => "set",
        ValueTag::CycleRef => "cycle_ref",
        ValueTag::Summary => "summary",
        ValueTag::TypeRef => "type_ref",
        ValueTag::ExceptionUnwindSentinel => "exception_unwind_sentinel",
        ValueTag::AliasRef => "alias",
    }
}

pub fn hash_kind_str(kind: HashKind) -> &'static str {
    match kind {
        HashKind::Content => "content",
        HashKind::Summary => "summary",
        HashKind::Identity => "identity",
        HashKind::Alias => "alias",
    }
}

/// Per-value materialization output. Lets the values+elements passes share
/// the alias-resolution work without redoing it.
struct MaterializedValue {
    /// What the value's effective container/scalar shape is, for the values
    /// table row and value_elements rows. For aliases, this is the resolved
    /// type tag (matching the source's tag).
    effective_tag: ValueTag,
    container_length: Option<i64>,
    /// For container values (lists/sets/dicts), the resolved element list:
    /// either the value's own elements (non-alias) or the source elements
    /// followed by the alias's grown tail.
    element_list: Option<ElementList>,
    /// Confidence label for the values row.
    confidence: Confidence,
    /// For alias entries, the source value_id; None otherwise.
    aliased_value_id: Option<ValueId>,
}

#[derive(Debug, Clone)]
enum ElementList {
    Sequence(Vec<ValueId>),
    Pairs(Vec<(ValueId, ValueId)>),
}

/// Resolve every value in the trace to its materialized form (one pass).
/// Aliases follow chains transitively: an alias-to-alias-to-list resolves to
/// the underlying list's contents plus any grown tails along the chain.
fn materialize_all(values: &[ValueEntry]) -> Result<Vec<MaterializedValue>> {
    let mut out: Vec<MaterializedValue> = Vec::with_capacity(values.len());
    for (id, entry) in values.iter().enumerate() {
        let mat = materialize_one(id as ValueId, entry, &out)?;
        out.push(mat);
    }
    Ok(out)
}

fn materialize_one(
    self_id: ValueId,
    entry: &ValueEntry,
    prior: &[MaterializedValue],
) -> Result<MaterializedValue> {
    let tag = entry.value.tag();
    match &entry.value {
        Value::Alias {
            kind,
            aliased_value_id,
            confidence,
        } => resolve_alias(self_id, kind, *aliased_value_id, *confidence, prior),
        Value::List(ids) | Value::Set(ids) => Ok(MaterializedValue {
            effective_tag: tag,
            container_length: Some(ids.len() as i64),
            element_list: Some(ElementList::Sequence(ids.clone())),
            confidence: derive_confidence(entry.hash_kind, tag),
            aliased_value_id: None,
        }),
        Value::Dict(pairs) => Ok(MaterializedValue {
            effective_tag: tag,
            container_length: Some(pairs.len() as i64),
            element_list: Some(ElementList::Pairs(pairs.clone())),
            confidence: derive_confidence(entry.hash_kind, tag),
            aliased_value_id: None,
        }),
        _ => Ok(MaterializedValue {
            effective_tag: tag,
            container_length: None,
            element_list: None,
            confidence: derive_confidence(entry.hash_kind, tag),
            aliased_value_id: None,
        }),
    }
}

fn resolve_alias(
    self_id: ValueId,
    kind: &AliasKind,
    aliased_value_id: ValueId,
    confidence: Confidence,
    prior: &[MaterializedValue],
) -> Result<MaterializedValue> {
    let source = prior.get(aliased_value_id as usize).ok_or_else(|| {
        IndexError::Internal(format!(
            "alias at value_id {self_id} points at unknown value_id {aliased_value_id}"
        ))
    })?;
    // Inherit the source's effective tag (so an alias to a list shows up as
    // a list in the values table, not as 'alias').
    let effective_tag = source.effective_tag;
    let inherited = source.element_list.clone();

    let element_list = match (inherited, kind) {
        (Some(ElementList::Sequence(prior_ids)), AliasKind::Equivalent) => {
            Some(ElementList::Sequence(prior_ids))
        }
        (Some(ElementList::Pairs(prior_pairs)), AliasKind::Equivalent) => {
            Some(ElementList::Pairs(prior_pairs))
        }
        (Some(ElementList::Sequence(prior_ids)), AliasKind::Grown { new_elements }) => {
            let mut combined = prior_ids;
            combined.extend_from_slice(new_elements);
            Some(ElementList::Sequence(combined))
        }
        (Some(ElementList::Pairs(prior_pairs)), AliasKind::Grown { new_elements }) => {
            // For dict growth, new_elements is a flat list of alternating
            // key, value, key, value... (per spec). Pair them up.
            if new_elements.len() % 2 != 0 {
                return Err(IndexError::Internal(format!(
                    "dict-alias at value_id {self_id} has odd number of grown elements ({})",
                    new_elements.len()
                )));
            }
            let mut combined = prior_pairs;
            for chunk in new_elements.chunks_exact(2) {
                combined.push((chunk[0], chunk[1]));
            }
            Some(ElementList::Pairs(combined))
        }
        // Source isn't a container — alias to a scalar or summary or another
        // alias-resolved scalar. The alias's "effective" container_length is
        // None.
        (None, AliasKind::Equivalent) => None,
        (None, AliasKind::Grown { .. }) => {
            return Err(IndexError::Internal(format!(
                "Grown alias at value_id {self_id} aliases non-container value_id {aliased_value_id}"
            )));
        }
    };

    let container_length = match &element_list {
        Some(ElementList::Sequence(ids)) => Some(ids.len() as i64),
        Some(ElementList::Pairs(pairs)) => Some(pairs.len() as i64),
        None => None,
    };

    Ok(MaterializedValue {
        effective_tag,
        container_length,
        element_list,
        confidence,
        aliased_value_id: Some(aliased_value_id),
    })
}

/// Insert all value rows. Container element rows are inserted by
/// `insert_value_elements`. Both use Appender for bulk speed.
pub fn insert_values(
    appender: &mut Appender<'_>,
    values: &[ValueEntry],
    strings: &[String],
) -> Result<()> {
    let materialized = materialize_all(values)?;
    for (id, entry) in values.iter().enumerate() {
        let value_id = id as i64;
        let mat = &materialized[id];
        let type_tag = type_tag_str(mat.effective_tag);
        let hash_kind = hash_kind_str(entry.hash_kind);
        let hash_hex = hex_encode(&entry.hash);

        let mut bool_value: Option<bool> = None;
        let mut int_value: Option<i64> = None;
        let mut big_int_hex: Option<String> = None;
        let mut float_value: Option<f64> = None;
        let mut string_value: Option<String> = None;
        let mut bytes_value: Option<Vec<u8>> = None;
        let mut cycle_ref_depth: Option<i32> = None;
        let mut type_name: Option<String> = None;
        let mut repr_text: Option<String> = None;
        let mut summary_length: Option<i64> = None;
        let mut type_ref_name: Option<String> = None;

        // Scalar columns come from the entry itself, *not* from the alias
        // chain — an alias to a scalar still resolves to the source's value.
        // The simple approach: peek at whatever the resolved entry would
        // hold. For aliases pointing at scalars, this falls through the
        // material's effective_tag but we don't have direct access to the
        // scalar bits, so we walk the alias chain.
        let mut target_entry = entry;
        while let Value::Alias {
            aliased_value_id, ..
        } = &target_entry.value
        {
            match values.get(*aliased_value_id as usize) {
                Some(t) => target_entry = t,
                None => break,
            }
        }
        match &target_entry.value {
            Value::None => {}
            Value::Bool(b) => bool_value = Some(*b),
            Value::Int(n) => int_value = Some(*n),
            Value::BigInt(bytes) => big_int_hex = Some(hex_encode(bytes)),
            Value::Float(f) => float_value = Some(*f),
            Value::String(s) => string_value = Some(s.clone()),
            Value::Bytes(b) => bytes_value = Some(b.clone()),
            Value::List(_) | Value::Dict(_) | Value::Set(_) => {}
            Value::CycleRef(d) => cycle_ref_depth = Some(*d as i32),
            Value::Summary {
                type_name: tn,
                length,
                repr,
            } => {
                type_name = Some(lookup_string(strings, *tn)?.to_string());
                repr_text = Some(lookup_string(strings, *repr)?.to_string());
                summary_length = Some(*length as i64);
            }
            Value::TypeRef(id) => {
                type_ref_name = Some(lookup_string(strings, *id)?.to_string());
            }
            Value::ExceptionUnwindSentinel => {}
            // Cycle through alias chains was already resolved above; if we
            // somehow still see one here it's a bug.
            Value::Alias { .. } => {}
        }

        let confidence_str = mat.confidence.as_str();
        let aliased_value_id_i: Option<i64> = mat.aliased_value_id.map(|id| id as i64);
        let container_length = mat.container_length;

        let params: [&dyn ToSql; 18] = [
            &value_id,
            &type_tag,
            &hash_kind,
            &hash_hex,
            &bool_value,
            &int_value,
            &big_int_hex,
            &float_value,
            &string_value,
            &bytes_value,
            &container_length,
            &cycle_ref_depth,
            &type_name,
            &repr_text,
            &summary_length,
            &type_ref_name,
            &confidence_str,
            &aliased_value_id_i,
        ];
        appender.append_row(params.as_slice())?;
    }
    Ok(())
}

/// Insert rows into `value_elements` for every container value. Aliases get
/// their resolved element list (inherited from the source plus any grown tail).
pub fn insert_value_elements(appender: &mut Appender<'_>, values: &[ValueEntry]) -> Result<()> {
    let materialized = materialize_all(values)?;
    for (id, mat) in materialized.iter().enumerate() {
        let container_id = id as i64;
        match &mat.element_list {
            None => {}
            Some(ElementList::Sequence(ids)) => {
                for (pos, child) in ids.iter().enumerate() {
                    let position = pos as i32;
                    let element = *child as i64;
                    appender.append_row(params![container_id, position, None::<i64>, element])?;
                }
            }
            Some(ElementList::Pairs(pairs)) => {
                for (pos, (k, v)) in pairs.iter().enumerate() {
                    let position = pos as i32;
                    let key = *k as i64;
                    let element = *v as i64;
                    appender.append_row(params![container_id, position, key, element])?;
                }
            }
        }
    }
    Ok(())
}

fn lookup_string(strings: &[String], id: u64) -> Result<&str> {
    strings
        .get(id as usize)
        .map(|s| s.as_str())
        .ok_or_else(|| IndexError::Internal(format!("string id {id} out of range")))
}
