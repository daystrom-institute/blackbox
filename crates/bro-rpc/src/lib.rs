//! Framing/auth/handshake transport substrate for same-host RPC channels.
//!
//! Built for design/daemon-runtime/locality-first-decomposition.md slice 5:
//! the daemon<->fleetd Unix domain socket channel (the daemon dials, fleetd
//! listens). This crate owns bounded frame I/O, same-host authentication
//! (shared-secret token plus peer-uid verification), and version/build
//! negotiation. It does not know what daemon and fleetd say to each other;
//! concrete message enums and the spawn-spec contract land with fleetd
//! itself, riding the payload-generic [`Envelope<T>`] this crate defines.
//!
//! See `AGENTS.md` in this crate for the invariants a change here must
//! preserve.

mod auth;
mod envelope;
mod error;
mod framing;
mod handshake;

#[cfg(unix)]
pub use auth::verify_peer_uid;
pub use auth::{ServiceToken, ServiceTokenError, ServiceTokenSet, ServiceTokenSetError};
pub use envelope::{
    ConnectionBinding, Envelope, MAX_MESSAGE_ID_BYTES, NegotiatedIo, validate_envelope,
};
pub use error::{RpcError, RpcPhase};
pub use framing::{DEFAULT_MAX_FRAME_BYTES, FramedIo};
pub use handshake::{
    BuildIdentity, DEFAULT_HANDSHAKE_TIMEOUT, HandshakeMessage, HandshakeOptions, Hello,
    MAX_BUILD_ID_BYTES, Reject, Welcome, accept, builds_compatible, connect,
};
