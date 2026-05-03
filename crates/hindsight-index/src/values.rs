// SPDX-License-Identifier: Apache-2.0

//! Value materialization: convert each `ValueEntry` from the trace into rows
//! in the `values` table (with type-specific columns populated) and rows in
//! `value_elements` for container values.

use duckdb::{Appender, ToSql, params};
use hindsight_format::{HashKind, Value, ValueEntry, ValueTag};

use crate::error::{IndexError, Result};

/// Lowercase hex-encode a byte slice.
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = vec![0u8; bytes.len() * 2];
    for (i, &b) in bytes.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0x0F) as usize];
    }
    // SAFETY-equivalent: every byte is from the HEX table which is ASCII.
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
    }
}

pub fn hash_kind_str(kind: HashKind) -> &'static str {
    match kind {
        HashKind::Content => "content",
        HashKind::Summary => "summary",
        HashKind::Identity => "identity",
    }
}

/// Insert all value rows. Container element rows are inserted by
/// `insert_value_elements`. Both use Appender for bulk speed.
pub fn insert_values(
    appender: &mut Appender<'_>,
    values: &[ValueEntry],
    strings: &[String],
) -> Result<()> {
    for (id, entry) in values.iter().enumerate() {
        let value_id = id as i64;
        let tag = entry.value.tag();
        let type_tag = type_tag_str(tag);
        let hash_kind = hash_kind_str(entry.hash_kind);
        let hash_hex = hex_encode(&entry.hash);

        let mut bool_value: Option<bool> = None;
        let mut int_value: Option<i64> = None;
        let mut big_int_hex: Option<String> = None;
        let mut float_value: Option<f64> = None;
        let mut string_value: Option<String> = None;
        let mut bytes_value: Option<Vec<u8>> = None;
        let mut container_length: Option<i64> = None;
        let mut cycle_ref_depth: Option<i32> = None;
        let mut type_name: Option<String> = None;
        let mut repr_text: Option<String> = None;
        let mut summary_length: Option<i64> = None;
        let mut type_ref_name: Option<String> = None;

        match &entry.value {
            Value::None => {}
            Value::Bool(b) => bool_value = Some(*b),
            Value::Int(n) => int_value = Some(*n),
            Value::BigInt(bytes) => big_int_hex = Some(hex_encode(bytes)),
            Value::Float(f) => float_value = Some(*f),
            Value::String(s) => string_value = Some(s.clone()),
            Value::Bytes(b) => bytes_value = Some(b.clone()),
            Value::List(ids) => container_length = Some(ids.len() as i64),
            Value::Dict(pairs) => container_length = Some(pairs.len() as i64),
            Value::Set(ids) => container_length = Some(ids.len() as i64),
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
        }

        let params: [&dyn ToSql; 16] = [
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
        ];
        appender.append_row(params.as_slice())?;
    }
    Ok(())
}

/// Insert rows into `value_elements` for every container value.
pub fn insert_value_elements(appender: &mut Appender<'_>, values: &[ValueEntry]) -> Result<()> {
    for (id, entry) in values.iter().enumerate() {
        let container_id = id as i64;
        match &entry.value {
            Value::List(ids) | Value::Set(ids) => {
                for (pos, child) in ids.iter().enumerate() {
                    let position = pos as i32;
                    let element = *child as i64;
                    appender.append_row(params![container_id, position, None::<i64>, element])?;
                }
            }
            Value::Dict(pairs) => {
                for (pos, (k, v)) in pairs.iter().enumerate() {
                    let position = pos as i32;
                    let key = *k as i64;
                    let element = *v as i64;
                    appender.append_row(params![container_id, position, key, element])?;
                }
            }
            _ => {}
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
