//! Payload-generic protocol/build negotiation over a [`FramedIo`].
//!
//! Concrete daemon<->fleetd identity claims (worker/task/session ids, lease
//! grants, reconnect proofs, ...) are out of scope here by design: this crate
//! is the transport substrate, not the fleetd contract. What is in scope is
//! the mechanics every same-host handshake needs regardless of what rides on
//! top of it: negotiate a protocol version, check build compatibility, and
//! never leave the caller waiting forever if the peer goes silent.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;

use crate::envelope::ConnectionBinding;
use crate::{FramedIo, NegotiatedIo, RpcError, RpcPhase};

pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_BUILD_ID_BYTES: usize = 128;

/// A build/version identity exchanged during the handshake so either side
/// can refuse to talk to an incompatible peer instead of failing later, more
/// confusingly, on the first envelope it cannot make sense of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildIdentity {
    pub version: String,
    pub build_id: String,
}

impl BuildIdentity {
    pub fn validate(&self) -> Result<(), RpcError> {
        if self.build_id.is_empty() || self.build_id.len() > MAX_BUILD_ID_BYTES {
            return Err(RpcError::InvalidConfiguration(format!(
                "build_id must be non-empty and at most {MAX_BUILD_ID_BYTES} bytes, got {} bytes",
                self.build_id.len()
            )));
        }
        if parse_major_minor(&self.version).is_none() {
            return Err(RpcError::InvalidConfiguration(format!(
                "build version `{}` is not a `major.minor[.patch]` string",
                self.version
            )));
        }
        Ok(())
    }
}

fn parse_major_minor(version: &str) -> Option<(u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    Some((major, minor))
}

/// Rolling-window build compatibility. Pre-1.0 (`major == 0`) on either side
/// carries no cross-minor stability guarantee, so that case requires an
/// exact `major.minor` match. Once both sides are at `major >= 1`, a rolling
/// daemon/fleetd upgrade should tolerate one side being a single minor
/// version ahead or behind, so the window is `same major, minor within 1`.
/// A malformed version string on either side is never compatible.
pub fn builds_compatible(local: &BuildIdentity, remote: &BuildIdentity) -> bool {
    let (Some((local_major, local_minor)), Some((remote_major, remote_minor))) = (
        parse_major_minor(&local.version),
        parse_major_minor(&remote.version),
    ) else {
        return false;
    };
    if local_major == 0 || remote_major == 0 {
        return local_major == remote_major && local_minor == remote_minor;
    }
    local_major == remote_major && local_minor.abs_diff(remote_minor) <= 1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_versions: Vec<u16>,
    pub build: BuildIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub selected_protocol: u16,
    pub connection_generation: u64,
    pub build: BuildIdentity,
}

/// Sent on the wire before the local side ever returns an error, so the
/// peer always learns *why* the connection did not go through. `message` is
/// the public-facing string; the richer diagnostic stays local (see
/// [`RpcError::HandshakeAuthorityRejected`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub supported_protocol_versions: Vec<u16>,
}

// Internally tagged (`tag = "type"`, no separate `content` field), not
// adjacently tagged: the `#[serde(other)]` catch-all below only reliably
// ignores an unrecognized variant's data under internal tagging. Adjacent
// tagging's "other" fallback still expects the `content` field to
// deserialize as the fallback variant's own shape (unit, i.e. null), so it
// breaks the instant a genuinely new variant carries non-null data, exactly
// the case forward-compatibility needs to tolerate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum HandshakeMessage {
    Hello(Hello),
    Welcome(Welcome),
    Reject(Reject),
    /// Forward-compatibility catch-all: an older side deserializing a
    /// message shape a newer peer added should not blow up on an unknown
    /// tag before it even gets to negotiate.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone)]
pub struct HandshakeOptions {
    pub max_frame_bytes: usize,
    pub timeout: Duration,
}

impl Default for HandshakeOptions {
    fn default() -> Self {
        Self {
            max_frame_bytes: crate::DEFAULT_MAX_FRAME_BYTES,
            timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }
}

fn validate_options(options: &HandshakeOptions) -> Result<(), RpcError> {
    if options.max_frame_bytes == 0 {
        return Err(RpcError::InvalidConfiguration(
            "max_frame_bytes must be greater than zero".to_string(),
        ));
    }
    if options.timeout.is_zero() {
        return Err(RpcError::InvalidConfiguration(
            "handshake timeout must be greater than zero".to_string(),
        ));
    }
    Ok(())
}

/// The dialing side of the handshake (the daemon, per
/// design/daemon-runtime/locality-first-decomposition.md slice 5: the
/// harness never dials anyone, fleetd listens, the daemon dials fleetd).
pub async fn connect<T>(
    io: T,
    local_build: BuildIdentity,
    protocol_versions: Vec<u16>,
    options: HandshakeOptions,
) -> Result<(NegotiatedIo<T>, Welcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    validate_options(&options)?;
    timeout(
        options.timeout,
        connect_inner(io, local_build, protocol_versions, options),
    )
    .await
    .map_err(|_| RpcError::Timeout {
        phase: RpcPhase::Handshake,
    })?
}

async fn connect_inner<T>(
    io: T,
    local_build: BuildIdentity,
    protocol_versions: Vec<u16>,
    options: HandshakeOptions,
) -> Result<(NegotiatedIo<T>, Welcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    local_build.validate()?;
    let mut framed = FramedIo::with_max_frame_bytes(io, options.max_frame_bytes);
    let offered_versions = protocol_versions.clone();
    framed
        .write_json(&HandshakeMessage::Hello(Hello {
            protocol_versions,
            build: local_build,
        }))
        .await?;
    match framed.read_json::<HandshakeMessage>().await? {
        HandshakeMessage::Welcome(welcome) => {
            if !offered_versions.contains(&welcome.selected_protocol) {
                return Err(RpcError::SelectedProtocolNotOffered {
                    selected: welcome.selected_protocol,
                });
            }
            let binding = ConnectionBinding {
                protocol_version: welcome.selected_protocol,
                connection_generation: welcome.connection_generation,
            };
            Ok((NegotiatedIo::new(framed, binding), welcome))
        }
        HandshakeMessage::Reject(reject) => Err(RpcError::HandshakeRejected {
            code: reject.code,
            message: reject.message,
            supported_protocol_versions: reject.supported_protocol_versions,
        }),
        HandshakeMessage::Hello(_) => Err(RpcError::UnexpectedHandshake("hello")),
        HandshakeMessage::Unknown => Err(RpcError::UnexpectedHandshake("unknown")),
    }
}

/// The listening side of the handshake (fleetd). `connection_generation` is
/// allocated by the caller (e.g. an atomic counter fleetd owns) before
/// calling in; this crate has no opinion on generation-allocation policy,
/// only on fencing envelopes against whatever generation was negotiated.
pub async fn accept<T>(
    io: T,
    local_build: BuildIdentity,
    supported_protocol_versions: Vec<u16>,
    connection_generation: u64,
    options: HandshakeOptions,
) -> Result<(NegotiatedIo<T>, Hello, Welcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    validate_options(&options)?;
    timeout(
        options.timeout,
        accept_inner(
            io,
            local_build,
            supported_protocol_versions,
            connection_generation,
            options,
        ),
    )
    .await
    .map_err(|_| RpcError::Timeout {
        phase: RpcPhase::Handshake,
    })?
}

async fn accept_inner<T>(
    io: T,
    local_build: BuildIdentity,
    supported_protocol_versions: Vec<u16>,
    connection_generation: u64,
    options: HandshakeOptions,
) -> Result<(NegotiatedIo<T>, Hello, Welcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    local_build.validate()?;
    let mut framed = FramedIo::with_max_frame_bytes(io, options.max_frame_bytes);
    let hello = match framed.read_json::<HandshakeMessage>().await? {
        HandshakeMessage::Hello(hello) => hello,
        HandshakeMessage::Welcome(_) => return Err(RpcError::UnexpectedHandshake("welcome")),
        HandshakeMessage::Reject(_) => return Err(RpcError::UnexpectedHandshake("reject")),
        HandshakeMessage::Unknown => return Err(RpcError::UnexpectedHandshake("unknown")),
    };

    if let Err(error) = hello.build.validate() {
        return Err(reject(
            &mut framed,
            "protocol.invalid_build",
            "invalid build identity",
            error.to_string(),
            supported_protocol_versions,
        )
        .await?);
    }

    let selected_protocol = supported_protocol_versions
        .iter()
        .copied()
        .filter(|version| hello.protocol_versions.contains(version))
        .max();
    let Some(selected_protocol) = selected_protocol else {
        let local_message = format!(
            "no common protocol version between peer-offered {:?} and locally-supported {:?}",
            hello.protocol_versions, supported_protocol_versions
        );
        return Err(reject(
            &mut framed,
            "protocol.version_mismatch",
            "no common protocol version",
            local_message,
            supported_protocol_versions,
        )
        .await?);
    };

    if !builds_compatible(&local_build, &hello.build) {
        let local_message = format!(
            "build {:?} is not compatible with peer build {:?}",
            local_build, hello.build
        );
        return Err(reject(
            &mut framed,
            "protocol.build_incompatible",
            "incompatible build",
            local_message,
            supported_protocol_versions,
        )
        .await?);
    }

    let welcome = Welcome {
        selected_protocol,
        connection_generation,
        build: local_build,
    };
    framed
        .write_json(&HandshakeMessage::Welcome(welcome.clone()))
        .await?;
    let binding = ConnectionBinding {
        protocol_version: selected_protocol,
        connection_generation,
    };
    Ok((NegotiatedIo::new(framed, binding), hello, welcome))
}

/// Write a `Reject` frame on the wire (public-facing `public_message` plus
/// the code and supported-versions list the peer needs to retry sanely),
/// then hand back a local error carrying the richer `local_message` for
/// logging. If the write itself fails, that I/O error is returned instead:
/// a peer that cannot even receive the rejection is a more broken
/// connection than the rejection reason itself.
async fn reject<T: AsyncRead + AsyncWrite + Unpin>(
    framed: &mut FramedIo<T>,
    code: &str,
    public_message: &str,
    local_message: String,
    supported_protocol_versions: Vec<u16>,
) -> Result<RpcError, RpcError> {
    framed
        .write_json(&HandshakeMessage::Reject(Reject {
            code: code.to_string(),
            message: public_message.to_string(),
            supported_protocol_versions,
        }))
        .await?;
    Ok(RpcError::HandshakeAuthorityRejected {
        code: code.to_string(),
        local_message,
    })
}
