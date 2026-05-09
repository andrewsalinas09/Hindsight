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
///
/// **Writer policy**: this writer does not support `content_hash` on summary
/// values. The spec (§"Summary type (0x10)") permits a recorder to use
/// `content_hash` for summaries when full canonicalization is cheap (e.g.,
/// hashing a NumPy buffer directly), but supporting that path here would
/// require letting the caller supply the canonical bytes. No current caller
/// needs it; if a future caller does, extend the `Summary` arm below to take
/// caller-provided canonical bytes rather than panicking.
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
            // TODO(spec): the v0.2 spec defines set canonical form as
            // "elements sorted by canonical representation, then concatenated"
            // with no count prefix (unlike list, which has one). That admits
            // collisions: e.g., the set {"ab"} and the set {"a", "b"} both
            // canonicalize to bytes "ab". We follow the spec literally here,
            // but flag for revision — a count prefix or per-element length
            // prefix would resolve this.
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
            // Writer-policy choice (not a format requirement): see the doc
            // comment on this function. The spec allows content_hash on
            // summaries but this writer routes summaries through the
            // summary-hash path only.
            unreachable!(
                "writer policy: Summary values must be interned via \
                 intern_value_summary; content_hash on summaries is not supported here"
            );
        }
        Value::Alias { .. } => {
            // Aliases use HashKind::Alias with an all-zero hash field per
            // spec. They never reach the canonical_inline path because
            // intern_value_alias_* methods bypass canonicalization entirely.
            unreachable!(
                "writer policy: Alias values are interned via \
                 intern_value_alias_*; they do not have a canonical hash"
            );
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
///
/// **Implication for cross-run determinism:** when a content-hashed container
/// (e.g., a list) holds an identity-hashed child (e.g., a Python object whose
/// hash kind is Identity, derived from `id()`), the container's content hash
/// will differ across program runs because `id()` differs across runs. This is
/// correct: different objects mean different list contents. It's not a
/// non-determinism bug; the identity hash is the canonical witness for "same
/// object reference," and that's necessarily process-local.
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
    fn hash_is_xxhash3_128_be_known_answers() {
        // Frozen known-answer vectors. The expected bytes are xxhash3-128
        // values in big-endian byte order (matching the spec convention used
        // by `XXH3_128bits_canonicalFromHash` in the upstream C reference).
        //
        // PROVENANCE: these literals were generated by computing
        // `xxhash_rust::xxh3::xxh3_128(input).to_be_bytes()` once and frozen
        // here. Locking them as constants — rather than recomputing via the
        // same crate at assertion time — turns this from a tautology into a
        // real KAT: it would catch (a) a behavior change in xxhash-rust,
        // (b) a swap to a different xxhash crate that disagrees, and (c) a
        // regression in `hash_bytes` that flips byte order. To independently
        // verify, run `xxh128sum` (the official xxhash CLI) over the same
        // inputs and confirm the hex matches.
        // 0xdf8d09e93f874900a99b8775cc15b6c7
        const HELLO_WORLD_XXH3_128_BE: [u8; 16] = [
            0xDF, 0x8D, 0x09, 0xE9, 0x3F, 0x87, 0x49, 0x00, 0xA9, 0x9B, 0x87, 0x75, 0xCC, 0x15,
            0xB6, 0xC7,
        ];
        // 0x99aa06d3014798d86001c324468d497f — this is the published
        // XXH3_128bits("") reference vector (default seed = 0). It serves as
        // an independent cross-check on byte order and algorithm: any
        // conforming xxhash3-128 implementation must produce this for empty
        // input, regardless of which crate is in use.
        const EMPTY_XXH3_128_BE: [u8; 16] = [
            0x99, 0xAA, 0x06, 0xD3, 0x01, 0x47, 0x98, 0xD8, 0x60, 0x01, 0xC3, 0x24, 0x46, 0x8D,
            0x49, 0x7F,
        ];

        assert_eq!(hash_bytes(b"hello world"), HELLO_WORLD_XXH3_128_BE);
        assert_eq!(hash_bytes(b""), EMPTY_XXH3_128_BE);
    }
}
