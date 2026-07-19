use std::io;

use thiserror::Error;

/// Which leg of an RPC exchange a [`RpcError::Timeout`] fired in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcPhase {
    Handshake,
    Read,
    Write,
}

impl std::fmt::Display for RpcPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Handshake => "handshake",
            Self::Read => "read",
            Self::Write => "write",
        })
    }
}

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("RPC I/O failed: {0}")]
    Io(#[from] io::Error),

    #[error("RPC frame length {actual} exceeds limit {limit}")]
    FrameTooLarge { actual: usize, limit: usize },

    #[error("RPC frame contained invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),

    #[error("unexpected message during handshake: {0}")]
    UnexpectedHandshake(&'static str),

    #[error("peer rejected the handshake ({code}): {message}")]
    HandshakeRejected {
        code: String,
        message: String,
        supported_protocol_versions: Vec<u16>,
    },

    #[error("local handshake authority rejected the peer ({code}): {local_message}")]
    HandshakeAuthorityRejected { code: String, local_message: String },

    #[error("peer selected protocol version {selected} that was never offered")]
    SelectedProtocolNotOffered { selected: u16 },

    #[error("envelope protocol version {actual} does not match negotiated version {expected}")]
    VersionMismatch { expected: u16, actual: u16 },

    #[error("envelope generation {actual} does not match connection generation {expected}")]
    StaleGeneration { expected: u64, actual: u64 },

    #[error("invalid envelope: {0}")]
    InvalidEnvelope(String),

    #[error("authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("RPC {phase} timed out")]
    Timeout { phase: RpcPhase },

    #[error("invalid RPC configuration: {0}")]
    InvalidConfiguration(String),
}
