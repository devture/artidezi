// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! Durable orchestrator state.
//!
//! - [`ArtifactStore`]: immutable, content-addressed files (task inputs, prompts, results, bundles,
//!   diffs, logs). SQLite stores their hashes, never the bytes.
//! - [`Db`]: SQLite in WAL mode holding tasks, attempts, leases, published merge requests and the
//!   audit event log. The orchestrator and local account-management CLI open it.

pub mod artifacts;
pub mod db;
pub mod model;
pub mod retention;

pub use artifacts::ArtifactStore;
pub use db::Db;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("i/o error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("artifact {sha256} is corrupt: {reason}")]
    CorruptArtifact { sha256: String, reason: String },
    #[error("artifact {0} does not exist")]
    MissingArtifact(String),
    #[error("{entity} {id} does not exist")]
    NotFound { entity: &'static str, id: String },
    #[error("invalid state transition for {entity} {id}: {from} -> {to}")]
    InvalidTransition {
        entity: &'static str,
        id: String,
        from: String,
        to: String,
    },
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid UI account operation: {0}")]
    InvalidUiAccount(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}_{}",
        ulid::Ulid::from_datetime(std::time::SystemTime::now())
            .to_string()
            .to_lowercase()
    )
}
