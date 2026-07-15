use std::io;

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcPhase {
    Handshake,
    Read,
    Write,
    Request,
}

impl std::fmt::Display for RpcPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Handshake => "handshake",
            Self::Read => "read",
            Self::Write => "write",
            Self::Request => "request",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    LocalShutdown,
    PeerClosed,
    InboundConsumerClosed,
    IdleTimeout,
    ReadFailed(String),
    WriteFailed(String),
    ProtocolViolation(String),
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalShutdown => formatter.write_str("local shutdown"),
            Self::PeerClosed => formatter.write_str("peer closed the connection"),
            Self::InboundConsumerClosed => formatter.write_str("inbound consumer closed"),
            Self::IdleTimeout => formatter.write_str("peer read idle timeout"),
            Self::ReadFailed(message) => write!(formatter, "read failed: {message}"),
            Self::WriteFailed(message) => write!(formatter, "write failed: {message}"),
            Self::ProtocolViolation(message) => {
                write!(formatter, "protocol violation: {message}")
            }
        }
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
    #[error("unexpected handshake message: {0}")]
    UnexpectedHandshake(&'static str),
    #[error("peer rejected handshake ({code}): {message}")]
    HandshakeRejected {
        code: String,
        message: String,
        supported_protocol_versions: Vec<u16>,
    },
    #[error("handshake authority rejected worker ({code}): {local_message}")]
    HandshakeAuthorityRejected { code: String, local_message: String },
    #[error("no common worker protocol version")]
    VersionMismatch,
    #[error("fleet selected worker protocol version {selected} that was not offered")]
    SelectedProtocolNotOffered { selected: u16 },
    #[error("worker authentication failed: {0}")]
    Authentication(String),
    #[error("RPC {phase} timed out")]
    Timeout { phase: RpcPhase },
    #[error("invalid RPC configuration: {0}")]
    InvalidConfiguration(String),
    #[error("envelope protocol version {actual} does not match negotiated version {expected}")]
    ProtocolVersionMismatch { expected: u16, actual: u16 },
    #[error("envelope generation {actual} does not match current generation {expected}")]
    StaleGeneration { expected: u64, actual: u64 },
    #[error("envelope message id is empty or exceeds {limit} bytes")]
    InvalidMessageId { limit: usize },
    #[error("envelope correlation is invalid: {0}")]
    CorrelationMismatch(String),
    #[error("request message id `{message_id}` is already pending in generation {generation}")]
    DuplicateMessageId { generation: u64, message_id: String },
    #[error("outbound {priority} queue is full or exceeds its {byte_limit}-byte budget")]
    QueueFull {
        priority: &'static str,
        byte_limit: usize,
    },
    #[error("maximum in-flight request count {limit} reached")]
    TooManyInFlightRequests { limit: usize },
    #[error("RPC peer disconnected: {reason}")]
    Disconnected { reason: DisconnectReason },
    #[error("RPC response channel closed without a result")]
    ResponseChannelClosed,
}

impl RpcError {
    pub(crate) fn disconnected(reason: DisconnectReason) -> Self {
        Self::Disconnected { reason }
    }
}
