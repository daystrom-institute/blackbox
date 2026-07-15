use thiserror::Error;

pub type BlackopsResult<T> = Result<T, BlackopsError>;

#[derive(Debug, Error)]
pub enum BlackopsError {
    #[error("operational state conflict: {0}")]
    Conflict(String),
    #[error("invalid operational state: {0}")]
    InvalidState(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("repository generation changed: expected {expected}, found {found}")]
    GenerationConflict { expected: u64, found: u64 },
    #[error("unsupported snapshot format {found}; expected {expected}")]
    UnsupportedSnapshotFormat { found: u32, expected: u32 },
    #[error("operational repository I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("operational repository serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}
