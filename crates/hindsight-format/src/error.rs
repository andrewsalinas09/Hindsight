// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FormatError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown string ID: {0}")]
    UnknownStringId(u64),
    #[error("unknown value ID: {0}")]
    UnknownValueId(u64),
    #[error("unknown source file ID: {0}")]
    UnknownFileId(u64),
    #[error("source path too long: {0} bytes (max 65535)")]
    PathTooLong(usize),
    #[error("source content too long: {0} bytes (max u32::MAX)")]
    SourceTooLong(usize),
    #[error("metadata block too large: {0} bytes (max u32::MAX)")]
    MetadataTooLarge(usize),
    #[error("section too large: {0} bytes (max u32::MAX)")]
    SectionTooLarge(usize),
    #[error("zstd compression failed: {0}")]
    Compression(String),
}

pub type Result<T> = std::result::Result<T, FormatError>;
