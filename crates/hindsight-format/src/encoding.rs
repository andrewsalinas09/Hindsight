// SPDX-License-Identifier: Apache-2.0

//! On-disk byte encoding for values. Distinct from canonical hashing reprs,
//! which live in `canonical.rs`.

use std::io::{self, Write};

use crate::value::Value;
use crate::varint::{write_svarint, write_uvarint};

/// Write the on-disk data bytes for a value (the bytes following the
/// type tag, hash kind, hash, and length-prefix in a value table entry).
pub fn write_value_data<W: Write>(w: &mut W, value: &Value) -> io::Result<()> {
    match value {
        Value::None | Value::ExceptionUnwindSentinel => {}
        Value::Bool(b) => {
            w.write_all(&[u8::from(*b)])?;
        }
        Value::Int(i) => {
            write_svarint(w, *i)?;
        }
        Value::BigInt(bytes) => {
            w.write_all(bytes)?;
        }
        Value::Float(f) => {
            w.write_all(&f.to_le_bytes())?;
        }
        Value::String(s) => {
            w.write_all(s.as_bytes())?;
        }
        Value::Bytes(b) => {
            w.write_all(b)?;
        }
        Value::List(ids) | Value::Set(ids) => {
            write_uvarint(w, ids.len() as u64)?;
            for id in ids {
                write_uvarint(w, *id)?;
            }
        }
        Value::Dict(pairs) => {
            write_uvarint(w, pairs.len() as u64)?;
            for (k, v) in pairs {
                write_uvarint(w, *k)?;
                write_uvarint(w, *v)?;
            }
        }
        Value::CycleRef(depth) => {
            write_uvarint(w, u64::from(*depth))?;
        }
        Value::Summary {
            type_name,
            length,
            repr,
        } => {
            write_uvarint(w, *type_name)?;
            write_uvarint(w, *length)?;
            write_uvarint(w, *repr)?;
        }
        Value::TypeRef(string_id) => {
            write_uvarint(w, *string_id)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(value: &Value) -> Vec<u8> {
        let mut buf = Vec::new();
        write_value_data(&mut buf, value).unwrap();
        buf
    }

    #[test]
    fn none_and_sentinel_are_empty() {
        assert!(encode(&Value::None).is_empty());
        assert!(encode(&Value::ExceptionUnwindSentinel).is_empty());
    }

    #[test]
    fn bool_is_one_byte() {
        assert_eq!(encode(&Value::Bool(false)), vec![0]);
        assert_eq!(encode(&Value::Bool(true)), vec![1]);
    }

    #[test]
    fn int_uses_zigzag_varint() {
        assert_eq!(encode(&Value::Int(0)), vec![0x00]);
        assert_eq!(encode(&Value::Int(-1)), vec![0x01]);
        assert_eq!(encode(&Value::Int(1)), vec![0x02]);
    }

    #[test]
    fn float_is_eight_le_bytes() {
        let bytes = encode(&Value::Float(1.0));
        assert_eq!(bytes.len(), 8);
        assert_eq!(bytes, 1.0_f64.to_le_bytes());
    }

    #[test]
    fn string_is_raw_utf8() {
        assert_eq!(encode(&Value::String("hi".to_string())), b"hi");
    }

    #[test]
    fn list_is_count_then_ids() {
        // List of value IDs [3, 5]: count varint + two id varints.
        assert_eq!(encode(&Value::List(vec![3, 5])), vec![0x02, 0x03, 0x05]);
    }

    #[test]
    fn list_handles_multi_byte_varint_ids() {
        // 300 -> two-byte varint (0xAC, 0x02); 16384 -> three-byte (0x80, 0x80, 0x01).
        // count = 2 -> single byte 0x02. Catches per-element loop bugs that a
        // single-byte-id test would miss.
        let bytes = encode(&Value::List(vec![300, 16384]));
        assert_eq!(bytes, vec![0x02, 0xAC, 0x02, 0x80, 0x80, 0x01]);
    }

    #[test]
    fn dict_handles_multi_byte_varint_ids() {
        // Single (key=300, value=16384) entry. count=1 + key varint + value varint.
        let bytes = encode(&Value::Dict(vec![(300, 16384)]));
        assert_eq!(bytes, vec![0x01, 0xAC, 0x02, 0x80, 0x80, 0x01]);
    }
}
