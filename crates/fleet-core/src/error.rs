use std::io;

use thiserror::Error;

pub type FleetResult<T> = Result<T, FleetError>;

#[derive(Debug, Error)]
pub enum FleetError {
    #[error("fleet state I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("fleet state serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("unsupported fleet snapshot format {found}; expected {expected}")]
    UnsupportedSnapshotFormat { found: u32, expected: u32 },
    #[error("unsupported legacy worker authority version {0}")]
    UnsupportedLegacyWorkerVersion(u32),
    #[error("invalid durable fleet state: {0}")]
    InvalidState(String),
    #[error("snapshot generation conflict: expected {expected}, found {actual}")]
    SnapshotGenerationConflict { expected: u64, actual: u64 },
    #[error("entity not found: {kind} {id}")]
    NotFound { kind: &'static str, id: String },
    #[error("attempt idempotency key was reused with different execution intent")]
    IdempotencyConflict,
    #[error("worker handshake rejected: {0}")]
    HandshakeRejected(String),
    #[error("stale worker connection generation: expected {expected}, found {actual}")]
    StaleConnectionGeneration { expected: u64, actual: u64 },
    #[error("stale worktree ownership generation: expected {expected}, found {actual}")]
    StaleOwnershipGeneration { expected: u64, actual: u64 },
    #[error("sequence gap on {stream}: expected {expected}, found {actual}")]
    SequenceGap {
        stream: &'static str,
        expected: u64,
        actual: u64,
    },
    #[error("command outcome conflicts with the durable command or prior outcome")]
    CommandOutcomeConflict,
    #[error("no live worker is available for task {0}")]
    NoLiveWorker(String),
    #[error("worktree {path} is owned by task {owner}")]
    WorktreeConflict { path: String, owner: String },
    #[error("worktree {path} is not owned by task {owner}")]
    WorktreeOwnerMismatch { path: String, owner: String },
    #[error("capability route is not authorized: {0}")]
    CapabilityUnauthorized(String),
    #[error("provider allocation unavailable: {0}")]
    ProviderUnavailable(String),
    #[error("repository lock was poisoned")]
    LockPoisoned,
    #[error("monotonic counter exhausted: {0}")]
    CounterExhausted(&'static str),
}

impl FleetError {
    pub(crate) fn not_found(kind: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound {
            kind,
            id: id.into(),
        }
    }
}
