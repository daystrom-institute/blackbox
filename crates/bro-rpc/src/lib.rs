//! Same-host RPC framing and negotiated worker connection mechanics.
//!
//! This crate deliberately sits above `bro-protocol`: it performs bounded I/O
//! and request correlation but owns no fleet, operations, corpus, or harness
//! behavior.

mod auth;
mod error;
mod framing;
mod handshake;
mod negotiated;
mod peer;

pub use auth::{ServiceToken, ServiceTokenError};
pub use error::{DisconnectReason, RpcError, RpcPhase};
pub use framing::{DEFAULT_MAX_FRAME_BYTES, FramedIo};
pub use handshake::{
    DEFAULT_HANDSHAKE_TIMEOUT, FleetHandshakeConfig, FleetHandshakeGrant, HandshakeAuthorityReject,
    HandshakeOptions, accept_worker, accept_worker_with_authority, connect_worker,
    connect_worker_with_options,
};
pub use negotiated::{ConnectionBinding, MAX_MESSAGE_ID_BYTES, NegotiatedIo, validate_envelope};
pub use peer::{MessagePriority, PeerConfig, PeerHandle, RpcPeer};
