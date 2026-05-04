// SPDX-License-Identifier: Apache-2.0

//! Shared DuckDB connection used by every tool handler.
//!
//! `duckdb::Connection` is `Send` but not `Sync`, so we hold it inside a
//! synchronous `std::sync::Mutex`. Tools acquire the lock briefly to run
//! their query. For interactive debugging the serialization is fine; if
//! latency becomes a problem we can switch to a connection pool later.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use duckdb::Connection;

use crate::error::ServerError;

#[derive(Clone)]
pub struct DbConnection {
    inner: Arc<Mutex<Connection>>,
    path: Arc<PathBuf>,
}

impl DbConnection {
    /// Open the database at `path` for the life of the server. The DuckDB
    /// crate doesn't expose a read-only flag on this version, but the
    /// server only ever runs SELECTs so writes never happen.
    pub fn open(path: PathBuf) -> Result<Self, ServerError> {
        let conn = Connection::open(&path).map_err(|e| ServerError::DbOpen {
            path: path.clone(),
            source: e,
        })?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
            path: Arc::new(path),
        })
    }

    /// Acquire the connection lock. Panics only if a previous holder
    /// panicked while holding the lock — in which case the database is in
    /// an unknown state and the server is dead anyway.
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.inner.lock().expect("db mutex poisoned")
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}
