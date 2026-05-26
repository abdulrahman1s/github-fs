use thiserror::Error;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON (de)serialization error: {0}")]
    Json(#[from] serde_json::Error),

    /// Wraps `tempfile::PersistError`. We flatten to a String because the
    /// original carries the failed `NamedTempFile` and is not `Sync`, which
    /// makes propagation across awaits inconvenient.
    #[error("failed to atomically persist blob: {0}")]
    Persist(String),

    #[error("libgit2 error: {0}")]
    Git(#[from] git2::Error),
}
