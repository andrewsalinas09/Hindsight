// SPDX-License-Identifier: Apache-2.0

//! Canonical byte representation of values used for content/summary hashing.
//! See `docs/trace-format.md` §"Canonical representation".
//!
//! These bytes are *only* used as input to xxhash3-128. The on-disk encoding
//! in `encoding.rs` is independent.

use crate::value::{StringId, Value, ValueId};

/// Canonical-repr lookup over the writer's interned values and strings.
///
/// Implemented by the writer; threaded as a trait so canonical-repr code can be
/// unit-tested with stub stores.
pub(crate) trait CanonicalLookup {
    /// Canonical bytes for an already-interned value.
    fn value_canonical(&self, id: ValueId) -> &[u8];
    /// String content for an already-interned string.
    fn string(&self, id: StringId) -> &str;
}

/// Compute the canonical bytes that the value's hash will be taken over,
/// for an inline value (hash kind = Content).
///
/// Container variants look up their children's already-cached canonical bytes
/// from `lookup`.
pub(crate) fn canonical_inline(value: &Value, lookup: &dyn CanonicalLookup) -> Vec<u8> {
    match value {
        Value::None | Value::ExceptionUnwindSentinel => Vec::new(),
        Value::Bool(b) => vec![u8::from(*b)],
        Value::Int(i) => signed_int_canonical(*i),
        Value::BigInt(bytes) => bytes.clone(),
        Value::Float(f) => f.to_be_bytes().to_vec(),
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Bytes(b) => b.clone(),
        Value::List(ids) => {
            let mut out = Vec::new();
            out.extend_from_slice(&(ids.len() as u64).to_be_bytes());
            for id in ids {
                out.extend_from_slice(lookup.value_canonical(*id));
            }
            out
        }
        Value::Dict(pairs) => {
            let mut sorted: Vec<&(ValueId, ValueId)> = pairs.iter().collect();
            sorted.sort_by(|a, b| lookup.value_canonical(a.0).cmp(lookup.value_canonical(b.0)));
            let mut out = Vec::new();
            out.extend_from_slice(&(sorted.len() as u64).to_be_bytes());
            for (k, v) in sorted {
                out.extend_from_slice(lookup.value_canonical(*k));
                out.extend_from_slice(lookup.value_canonical(*v));
            }
            out
        }
        Value::Set(ids) => {
            let mut sorted: Vec<&ValueId> = ids.iter().collect();
            sorted.sort_by(|a, b| lookup.value_canonical(**a).cmp(lookup.value_canonical(**b)));
            let mut out = Vec::new();
            for id in sorted {
                out.extend_from_slice(lookup.value_canonical(*id));
            }
            out
        }
        Value::CycleRef(depth) => {
            let mut out = vec![0xFF];
            out.extend_from_slice(&depth.to_be_bytes());
            out
        }
        Value::TypeRef(string_id) => lookup.string(*string_id).as_bytes().to_vec(),
        Value::Summary { .. } => {
            // For inline (content_hash) on a summary, the spec says the recorder
            // must compute the full canonical representation. We don't support
            // that path: callers asking for a content hash on a summary should
            // intern the underlying value directly. The summary-hash path is
            // `canonical_summary` below.
            unreachable!("Summary values must be interned via intern_value_summary, not _inline");
        }
    }
}

/// Canonical bytes for a summary value (hash kind = Summary).
/// Per spec: `type_ref_string + length_be8 + repr_string`.
pub(crate) fn canonical_summary(
    type_name: StringId,
    length: u64,
    repr: StringId,
    lookup: &dyn CanonicalLookup,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(lookup.string(type_name).as_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(lookup.string(repr).as_bytes());
    out
}

/// Canonical bytes used as a placeholder when a value's hash kind is Identity.
///
/// The spec defines canonical representations for content_hash and summary_hash
/// but is silent on identity_hash. We use the 16-byte identity hash itself as
/// the canonical bytes, so that an identity-hashed value used as a container
/// child contributes deterministic bytes to the parent's content hash.
pub(crate) fn canonical_identity(identity_hash: &[u8; 16]) -> Vec<u8> {
    identity_hash.to_vec()
}

/// Two's-complement big-endian, minimum byte length, per spec.
fn signed_int_canonical(value: i64) -> Vec<u8> {
    if value == 0 {
        return vec![0];
    }
    let bytes = value.to_be_bytes();
    let mut start = 0;
    if value > 0 {
        // Strip leading 0x00 while the next byte's sign bit is 0, so the
        // result is still unambiguously non-negative.
        while start < 7 && bytes[start] == 0x00 && (bytes[start + 1] & 0x80) == 0 {
            start += 1;
        }
    } else {
        // Strip leading 0xFF while the next byte's sign bit is 1.
        while start < 7 && bytes[start] == 0xFF && (bytes[start + 1] & 0x80) != 0 {
            start += 1;
        }
    }
    bytes[start..].to_vec()
}

pub(crate) fn hash_bytes(canonical: &[u8]) -> [u8; 16] {
    // xxhash3-128. Big-endian byte order matches XXH3_128bits_canonicalFromHash.
    // Hashes are byte sequences, not numeric scalars, so the format spec's LE
    // rule for multi-byte numerics does not apply.
    xxhash_rust::xxh3::xxh3_128(canonical).to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct StubLookup {
        values: HashMap<ValueId, Vec<u8>>,
        strings: HashMap<StringId, String>,
    }

    impl CanonicalLookup for StubLookup {
        fn value_canonical(&self, id: ValueId) -> &[u8] {
            self.values.get(&id).expect("stub: value missing")
        }
        fn string(&self, id: StringId) -> &str {
            self.strings.get(&id).expect("stub: string missing")
        }
    }

    fn empty_lookup() -> StubLookup {
        StubLookup {
            values: HashMap::new(),
            strings: HashMap::new(),
        }
    }

    #[test]
    fn signed_int_minimum_byte_length() {
        assert_eq!(signed_int_canonical(0), vec![0x00]);
        assert_eq!(signed_int_canonical(1), vec![0x01]);
        assert_eq!(signed_int_canonical(127), vec![0x7F]);
        // 128 needs a leading zero so the high bit doesn't flip it negative.
        assert_eq!(signed_int_canonical(128), vec![0x00, 0x80]);
        assert_eq!(signed_int_canonical(-1), vec![0xFF]);
        assert_eq!(signed_int_canonical(-128), vec![0x80]);
        assert_eq!(signed_int_canonical(-129), vec![0xFF, 0x7F]);
    }

    #[test]
    fn float_canonical_is_be8() {
        let lookup = empty_lookup();
        assert_eq!(
            canonical_inline(&Value::Float(1.0), &lookup),
            1.0_f64.to_be_bytes().to_vec()
        );
    }

    #[test]
    fn list_canonical_includes_count_prefix() {
        let mut lookup = empty_lookup();
        lookup.values.insert(0, b"a".to_vec());
        lookup.values.insert(1, b"bc".to_vec());
        let bytes = canonical_inline(&Value::List(vec![0, 1]), &lookup);
        let mut expected = Vec::new();
        expected.extend_from_slice(&2u64.to_be_bytes());
        expected.extend_from_slice(b"a");
        expected.extend_from_slice(b"bc");
        assert_eq!(bytes, expected);
    }

    #[test]
    fn dict_canonical_sorts_by_key_canonical() {
        let mut lookup = empty_lookup();
        lookup.values.insert(0, b"b".to_vec()); // key "b"
        lookup.values.insert(1, b"a".to_vec()); // key "a"
        lookup.values.insert(2, b"x".to_vec()); // value
        lookup.values.insert(3, b"y".to_vec()); // value
        // Insertion order: ("b", "x"), ("a", "y"). Should sort to ("a","y"), ("b","x").
        let bytes = canonical_inline(&Value::Dict(vec![(0, 2), (1, 3)]), &lookup);
        let mut expected = Vec::new();
        expected.extend_from_slice(&2u64.to_be_bytes());
        expected.extend_from_slice(b"a");
        expected.extend_from_slice(b"y");
        expected.extend_from_slice(b"b");
        expected.extend_from_slice(b"x");
        assert_eq!(bytes, expected);
    }

    #[test]
    fn cycle_ref_canonical_is_marker_plus_depth() {
        let lookup = empty_lookup();
        assert_eq!(
            canonical_inline(&Value::CycleRef(2), &lookup),
            vec![0xFF, 0x00, 0x00, 0x00, 0x02]
        );
    }

    #[test]
    fn summary_canonical_concats_type_length_repr() {
        let mut lookup = empty_lookup();
        lookup.strings.insert(0, "numpy.ndarray".to_string());
        lookup.strings.insert(1, "shape=(3,)".to_string());
        let bytes = canonical_summary(0, 3, 1, &lookup);
        let mut expected = Vec::new();
        expected.extend_from_slice(b"numpy.ndarray");
        expected.extend_from_slice(&3u64.to_be_bytes());
        expected.extend_from_slice(b"shape=(3,)");
        assert_eq!(bytes, expected);
    }

    #[test]
    fn hash_is_xxhash3_128_be() {
        let bytes = b"hello world";
        let hash = hash_bytes(bytes);
        let expected = xxhash_rust::xxh3::xxh3_128(bytes).to_be_bytes();
        assert_eq!(hash, expected);
    }
}
