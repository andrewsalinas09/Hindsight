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
}

impl HashKind {
    pub fn as_u8(self) -> u8 {
        self as u8
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
}

impl ValueTag {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// A program value. Container variants reference other values by ID, which must
/// already exist in the value table at intern time.
#[derive(Debug, Clone)]
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
        }
    }
}
