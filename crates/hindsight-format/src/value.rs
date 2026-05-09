// SPDX-License-Identifier: Apache-2.0

//! Value types stored in the trace's value table.
//!
//! See `docs/trace-format.md` §"Value encoding" for the full spec. The on-disk
//! encoding lives in `encoding.rs`; canonical-representation rules used for
//! hashing live in `canonical.rs`.

pub type StringId = u64;
pub type ValueId = u64;

/// Hash kind for a value table entry. See spec §"Value table".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashKind {
    /// xxhash3-128 of the value's full canonical representation.
    Content = 0x01,
    /// xxhash3-128 of the captured summary representation only.
    Summary = 0x02,
    /// Language-level object identity (Python `id()`, etc.).
    Identity = 0x03,
    /// Marker on alias entries — the hash field is all zeros and meaningless.
    Alias = 0x04,
}

impl HashKind {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x01 => Self::Content,
            0x02 => Self::Summary,
            0x03 => Self::Identity,
            0x04 => Self::Alias,
            _ => return None,
        })
    }
}

/// On-disk type tag for a value. See spec §"Type tags".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ValueTag {
    None = 0x00,
    Bool = 0x01,
    IntSmall = 0x02,
    IntBig = 0x03,
    Float = 0x04,
    String = 0x05,
    Bytes = 0x06,
    ListOrTuple = 0x07,
    Dict = 0x08,
    Set = 0x09,
    CycleRef = 0x0A,
    Summary = 0x10,
    TypeRef = 0x11,
    ExceptionUnwindSentinel = 0x12,
    /// Alias to a previously-emitted value. See spec §"Alias values".
    AliasRef = 0x15,
}

impl ValueTag {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x00 => Self::None,
            0x01 => Self::Bool,
            0x02 => Self::IntSmall,
            0x03 => Self::IntBig,
            0x04 => Self::Float,
            0x05 => Self::String,
            0x06 => Self::Bytes,
            0x07 => Self::ListOrTuple,
            0x08 => Self::Dict,
            0x09 => Self::Set,
            0x0A => Self::CycleRef,
            0x10 => Self::Summary,
            0x11 => Self::TypeRef,
            0x12 => Self::ExceptionUnwindSentinel,
            0x15 => Self::AliasRef,
            _ => return None,
        })
    }
}

/// Confidence level for a value entry. Captures *how the recorder knows* the
/// entry reflects program state at capture time. See spec §"Confidence
/// semantics" for the full table.
///
/// Wire encoding: 1 byte. Stored on disk only inside [`Value::Alias`]
/// payloads; for non-alias entries it is *derived* from `(hash_kind, tag)`
/// via [`derive_confidence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Confidence {
    /// Inline value with full content hash. Recorder walked + hashed it.
    ContentExact = 0x01,
    /// Alias whose freshness is guaranteed by opcode-level mutation tracking.
    MutationTracked = 0x02,
    /// Alias re-validated by walking the live container after a mutation.
    DirtyReconciled = 0x03,
    /// Alias inferred from a summary fingerprint match. May hide same-
    /// fingerprint mutations. Also used for inline `Summary` values, where
    /// the recorder hashed only the summary bytes.
    SummaryObserved = 0x04,
    /// Identity-hashed, or alias known to be possibly stale (C extension
    /// mutation, ctypes, etc.).
    UncertainExternal = 0x05,
}

impl Confidence {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x01 => Self::ContentExact,
            0x02 => Self::MutationTracked,
            0x03 => Self::DirtyReconciled,
            0x04 => Self::SummaryObserved,
            0x05 => Self::UncertainExternal,
            _ => return None,
        })
    }

    /// Stable lower-snake-case name used by the indexer's `confidence` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContentExact => "content_exact",
            Self::MutationTracked => "mutation_tracked",
            Self::DirtyReconciled => "dirty_reconciled",
            Self::SummaryObserved => "summary_observed",
            Self::UncertainExternal => "uncertain_external",
        }
    }
}

/// Derive a confidence label for a non-alias value entry from its hash kind
/// and tag. Aliases carry their own confidence in the payload — this
/// function is only meaningful for non-alias entries.
pub fn derive_confidence(hash_kind: HashKind, tag: ValueTag) -> Confidence {
    match (hash_kind, tag) {
        (HashKind::Alias, _) | (_, ValueTag::AliasRef) => {
            // Aliases carry confidence in the payload. Callers should not
            // ask this function about them; we return UncertainExternal as
            // a safe fallback rather than panic.
            Confidence::UncertainExternal
        }
        (HashKind::Content, _) => Confidence::ContentExact,
        (HashKind::Summary, _) => Confidence::SummaryObserved,
        (HashKind::Identity, _) => Confidence::UncertainExternal,
    }
}

/// Kind of an alias entry. Distinguishes "fully-equivalent re-snapshot" from
/// "previous container plus new tail elements" — the latter is the
/// append-in-a-loop fast path.
#[derive(Debug, Clone, PartialEq)]
pub enum AliasKind {
    /// Same content as the aliased value.
    Equivalent,
    /// Aliased container plus appended tail elements. For lists/tuples/sets
    /// the tail is a flat list of value IDs. For dicts, the tail is a flat
    /// list of `(key_id, value_id)` pairs encoded as alternating value IDs.
    Grown {
        /// New element value IDs appended after the aliased container's
        /// existing elements. For dicts this is interpreted as `(k, v)` pairs.
        new_elements: Vec<ValueId>,
    },
}

impl AliasKind {
    pub fn as_u8(&self) -> u8 {
        match self {
            Self::Equivalent => 0x01,
            Self::Grown { .. } => 0x02,
        }
    }
}

/// A program value. Container variants reference other values by ID, which must
/// already exist in the value table at intern time.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    None,
    Bool(bool),
    /// A signed integer fitting in i64. Prefer this for any integer that fits
    /// in i64 — `BigInt` is only for values outside that range.
    Int(i64),
    /// A big integer in two's complement, big-endian, minimum byte length.
    ///
    /// **Caller contract:** the bytes must already be canonicalized (two's
    /// complement, big-endian, with leading 0x00 / 0xFF stripped down to the
    /// minimum needed to preserve sign). The writer trusts these bytes; it
    /// does not re-canonicalize them. They are used both as the on-disk
    /// encoding and (via `canonical::canonical_inline`) as the bytes hashed
    /// for `content_hash`. If the caller passes non-canonical bytes, two
    /// equal big-int values may end up with distinct hashes.
    BigInt(Vec<u8>),
    Float(f64),
    String(String),
    Bytes(Vec<u8>),
    /// A list or tuple. Type ref distinguishes list vs. tuple; both share tag 0x07.
    List(Vec<ValueId>),
    /// A dict mapping key value IDs to value IDs.
    Dict(Vec<(ValueId, ValueId)>),
    /// A set or frozenset.
    Set(Vec<ValueId>),
    /// A cycle reference: depth frames back to the referenced container.
    CycleRef(u32),
    /// A summary of a value too large to inline.
    Summary {
        type_name: StringId,
        length: u64,
        repr: StringId,
    },
    /// A reference to a type/class name.
    TypeRef(StringId),
    /// Sentinel for unwound exception frames; reserved at value table index 1.
    ExceptionUnwindSentinel,
    /// Alias to a previously-emitted value. See spec §"Alias values".
    Alias {
        kind: AliasKind,
        aliased_value_id: ValueId,
        confidence: Confidence,
    },
}

impl Value {
    pub fn tag(&self) -> ValueTag {
        match self {
            Value::None => ValueTag::None,
            Value::Bool(_) => ValueTag::Bool,
            Value::Int(_) => ValueTag::IntSmall,
            Value::BigInt(_) => ValueTag::IntBig,
            Value::Float(_) => ValueTag::Float,
            Value::String(_) => ValueTag::String,
            Value::Bytes(_) => ValueTag::Bytes,
            Value::List(_) => ValueTag::ListOrTuple,
            Value::Dict(_) => ValueTag::Dict,
            Value::Set(_) => ValueTag::Set,
            Value::CycleRef(_) => ValueTag::CycleRef,
            Value::Summary { .. } => ValueTag::Summary,
            Value::TypeRef(_) => ValueTag::TypeRef,
            Value::ExceptionUnwindSentinel => ValueTag::ExceptionUnwindSentinel,
            Value::Alias { .. } => ValueTag::AliasRef,
        }
    }
}
