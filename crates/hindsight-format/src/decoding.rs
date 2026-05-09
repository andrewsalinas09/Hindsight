// SPDX-License-Identifier: Apache-2.0

//! On-disk byte decoding for values. Mirror of `encoding.rs`.
//!
//! Each value table entry carries a length-prefixed `data` slice that this
//! module turns back into a [`Value`]. The slice is exactly the bytes the
//! writer produced via `encoding::write_value_data`; reads should consume it
//! exactly. Leftover bytes for any tag indicate a malformed entry and surface
//! as [`FormatError::ValueDataLeftover`].

use crate::byte_reader::ByteReader;
use crate::error::{FormatError, Result};
use crate::value::{AliasKind, Confidence, Value, ValueTag};

pub(crate) fn decode_value(tag: ValueTag, data: &[u8]) -> Result<Value> {
    let mut sub = ByteReader::new(data);
    let value = match tag {
        // Tags whose payload must be empty.
        ValueTag::None => {
            require_empty(data, tag)?;
            Value::None
        }
        ValueTag::ExceptionUnwindSentinel => {
            require_empty(data, tag)?;
            Value::ExceptionUnwindSentinel
        }

        // Tags whose payload is the entire `data` slice (no inner length
        // prefix; the value-table entry's length already bounds it).
        ValueTag::IntBig => Value::BigInt(data.to_vec()),
        ValueTag::String => {
            let s = std::str::from_utf8(data).map_err(|_| FormatError::InvalidUtf8 {
                field: "string value",
            })?;
            Value::String(s.to_owned())
        }
        ValueTag::Bytes => Value::Bytes(data.to_vec()),

        // Tags whose payload is read structurally; we then verify the slice
        // was consumed exactly.
        ValueTag::Bool => {
            let b = sub.read_u8()?;
            require_consumed(&sub, tag)?;
            match b {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                other => return Err(FormatError::InvalidBoolByte(other)),
            }
        }
        ValueTag::IntSmall => {
            let i = sub.read_svarint()?;
            require_consumed(&sub, tag)?;
            Value::Int(i)
        }
        ValueTag::Float => {
            let bytes = sub.take(8)?;
            let mut a = [0u8; 8];
            a.copy_from_slice(bytes);
            require_consumed(&sub, tag)?;
            Value::Float(f64::from_le_bytes(a))
        }
        ValueTag::ListOrTuple => {
            let ids = read_id_list(&mut sub)?;
            require_consumed(&sub, tag)?;
            Value::List(ids)
        }
        ValueTag::Set => {
            let ids = read_id_list(&mut sub)?;
            require_consumed(&sub, tag)?;
            Value::Set(ids)
        }
        ValueTag::Dict => {
            let count = sub.read_uvarint()? as usize;
            let mut pairs = Vec::with_capacity(count);
            for _ in 0..count {
                let k = sub.read_uvarint()?;
                let v = sub.read_uvarint()?;
                pairs.push((k, v));
            }
            require_consumed(&sub, tag)?;
            Value::Dict(pairs)
        }
        ValueTag::CycleRef => {
            let depth = sub.read_uvarint()?;
            require_consumed(&sub, tag)?;
            let depth: u32 = depth.try_into().map_err(|_| FormatError::FieldOverflow {
                field: "cycle ref depth",
            })?;
            Value::CycleRef(depth)
        }
        ValueTag::Summary => {
            let type_name = sub.read_uvarint()?;
            let length = sub.read_uvarint()?;
            let repr = sub.read_uvarint()?;
            require_consumed(&sub, tag)?;
            Value::Summary {
                type_name,
                length,
                repr,
            }
        }
        ValueTag::TypeRef => {
            let id = sub.read_uvarint()?;
            require_consumed(&sub, tag)?;
            Value::TypeRef(id)
        }
        ValueTag::AliasRef => {
            let kind_byte = sub.read_u8()?;
            let confidence_byte = sub.read_u8()?;
            let aliased_value_id = sub.read_uvarint()?;
            let confidence = Confidence::from_u8(confidence_byte)
                .ok_or(FormatError::InvalidConfidence(confidence_byte))?;
            let kind = match kind_byte {
                0x01 => {
                    require_consumed(&sub, tag)?;
                    AliasKind::Equivalent
                }
                0x02 => {
                    let count = sub.read_uvarint()? as usize;
                    let mut new_elements = Vec::with_capacity(count);
                    for _ in 0..count {
                        new_elements.push(sub.read_uvarint()?);
                    }
                    require_consumed(&sub, tag)?;
                    AliasKind::Grown { new_elements }
                }
                other => return Err(FormatError::InvalidAliasKind(other)),
            };
            Value::Alias {
                kind,
                aliased_value_id,
                confidence,
            }
        }
    };
    Ok(value)
}

fn read_id_list(sub: &mut ByteReader) -> Result<Vec<u64>> {
    let count = sub.read_uvarint()? as usize;
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        ids.push(sub.read_uvarint()?);
    }
    Ok(ids)
}

// Strict-mode policy for value table entries.
//
// Both helpers below reject any case where the on-disk `data` slice for a
// value table entry is *longer* than the bytes the type tag's encoding
// describes. The v0.2 spec doesn't say whether a reader should accept or
// reject this, so this is a deliberate choice on top of an underspecified
// behavior.
//
// Why strict in v0:
// - Padding/trailing bytes in a value entry have no semantic meaning today;
//   if we see them, something is wrong (writer bug, corruption, or someone
//   smuggling extra data we can't interpret).
// - Catching this at decode time gives a precise error pointing at the
//   offending value, which is much easier to diagnose than a downstream
//   crash from a misaligned subsequent read.
//
// When we'd flip to permissive: if a future format revision deliberately
// adds optional trailing fields to value entries (analogous to v1.0's
// possible "optional trailing fields with their own length prefixes" idea
// in the spec's "Forward compatibility" section). At that point this would
// move behind a strict-mode toggle on `TraceReader`.

fn require_empty(data: &[u8], tag: ValueTag) -> Result<()> {
    if data.is_empty() {
        Ok(())
    } else {
        Err(FormatError::ValueDataLeftover {
            tag: tag.as_u8(),
            leftover: data.len(),
        })
    }
}

fn require_consumed(sub: &ByteReader, tag: ValueTag) -> Result<()> {
    if sub.remaining() == 0 {
        Ok(())
    } else {
        Err(FormatError::ValueDataLeftover {
            tag: tag.as_u8(),
            leftover: sub.remaining(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_none_and_sentinel_require_empty_data() {
        assert!(matches!(
            decode_value(ValueTag::None, &[]).unwrap(),
            Value::None
        ));
        assert!(matches!(
            decode_value(ValueTag::ExceptionUnwindSentinel, &[]).unwrap(),
            Value::ExceptionUnwindSentinel
        ));
        assert!(matches!(
            decode_value(ValueTag::None, &[0x00]),
            Err(FormatError::ValueDataLeftover { .. })
        ));
    }

    #[test]
    fn decode_bool_validates_byte() {
        assert!(matches!(
            decode_value(ValueTag::Bool, &[0]).unwrap(),
            Value::Bool(false)
        ));
        assert!(matches!(
            decode_value(ValueTag::Bool, &[1]).unwrap(),
            Value::Bool(true)
        ));
        assert!(matches!(
            decode_value(ValueTag::Bool, &[2]),
            Err(FormatError::InvalidBoolByte(2))
        ));
        assert!(matches!(
            decode_value(ValueTag::Bool, &[1, 0]),
            Err(FormatError::ValueDataLeftover { .. })
        ));
    }

    #[test]
    fn decode_int_small_inverts_zigzag() {
        // Int(-1) zigzag = 1
        assert!(matches!(
            decode_value(ValueTag::IntSmall, &[0x01]).unwrap(),
            Value::Int(-1)
        ));
        // Int(5) zigzag = 10 = 0x0A
        assert!(matches!(
            decode_value(ValueTag::IntSmall, &[0x0A]).unwrap(),
            Value::Int(5)
        ));
    }

    #[test]
    fn decode_float_requires_eight_bytes() {
        let bytes = 1.0_f64.to_le_bytes();
        let v = decode_value(ValueTag::Float, &bytes).unwrap();
        match v {
            Value::Float(f) => assert_eq!(f, 1.0),
            other => panic!("expected Float, got {other:?}"),
        }
        // Wrong size:
        assert!(matches!(
            decode_value(ValueTag::Float, &[0u8; 7]),
            Err(FormatError::Truncated)
        ));
        assert!(matches!(
            decode_value(ValueTag::Float, &[0u8; 9]),
            Err(FormatError::ValueDataLeftover { .. })
        ));
    }

    #[test]
    fn decode_string_validates_utf8() {
        let v = decode_value(ValueTag::String, b"hello").unwrap();
        match v {
            Value::String(s) => assert_eq!(s, "hello"),
            other => panic!("expected String, got {other:?}"),
        }
        assert!(matches!(
            decode_value(ValueTag::String, &[0xFF, 0xFE]),
            Err(FormatError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn decode_list_round_trip_with_multibyte_ids() {
        // count=2, ids=[300, 16384]
        let bytes = [0x02, 0xAC, 0x02, 0x80, 0x80, 0x01];
        let v = decode_value(ValueTag::ListOrTuple, &bytes).unwrap();
        match v {
            Value::List(ids) => assert_eq!(ids, vec![300, 16384]),
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn decode_dict_round_trip() {
        // count=1, pair=(300, 16384)
        let bytes = [0x01, 0xAC, 0x02, 0x80, 0x80, 0x01];
        let v = decode_value(ValueTag::Dict, &bytes).unwrap();
        match v {
            Value::Dict(p) => assert_eq!(p, vec![(300, 16384)]),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn decode_summary_reads_three_varints() {
        // type_name=1, length=42, repr=3
        let bytes = [0x01, 0x2A, 0x03];
        let v = decode_value(ValueTag::Summary, &bytes).unwrap();
        match v {
            Value::Summary {
                type_name,
                length,
                repr,
            } => {
                assert_eq!(type_name, 1);
                assert_eq!(length, 42);
                assert_eq!(repr, 3);
            }
            other => panic!("expected Summary, got {other:?}"),
        }
    }

    #[test]
    fn decode_cycle_ref_overflow_when_depth_exceeds_u32() {
        // u64::MAX as varint -> 10 bytes of 0xFF.. 0x01
        let mut bytes = vec![0xFFu8; 9];
        bytes.push(0x01);
        assert!(matches!(
            decode_value(ValueTag::CycleRef, &bytes),
            Err(FormatError::FieldOverflow { .. })
        ));
    }
}
