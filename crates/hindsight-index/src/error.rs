// SPDX-License-Identifier: Apache-2.0

//! Error type for the indexer. The indexer is a thin transformer over the
//! format reader and DuckDB; almost every failure originates in one of those
//! two layers, so we wrap them with thiserror.

use std::path::PathBuf;

use hindsight_format::FormatError;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("trace I/O error reading {path:?}: {source}")]
    TraceIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("database I/O error at {path:?}: {source}")]
    DbIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("trace format error: {0}")]
    Format(#[from] FormatError),
    #[error("duckdb error: {0}")]
    DuckDb(#[from] duckdb::Error),
    #[error("trace metadata: {0}")]
    Metadata(String),
    #[error("internal indexer error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, IndexError>;
