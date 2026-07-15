//! Same-host RPC framing and worker handshake mechanics.
//!
//! This crate deliberately sits above `bro-protocol`: it performs I/O but owns
//! no fleet, operations, corpus, or harness behavior.

use std::io;

use bro_protocol::{
    BuildIdentity, FleetWelcome, HandshakeMessage, HandshakeReject, LeaseGrant, SessionPolicy,
    WorkerHello,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const DEFAULT_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

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
    #[error("no common worker protocol version")]
    VersionMismatch,
    #[error("fleet selected worker protocol version {selected} that was not offered")]
    SelectedProtocolNotOffered { selected: u16 },
    #[error("worker authentication failed: {0}")]
    Authentication(String),
}

#[derive(Debug)]
pub struct FramedIo<T> {
    io: T,
    max_frame_bytes: usize,
}

impl<T> FramedIo<T> {
    pub fn new(io: T) -> Self {
        Self::with_max_frame_bytes(io, DEFAULT_MAX_FRAME_BYTES)
    }

    pub fn with_max_frame_bytes(io: T, max_frame_bytes: usize) -> Self {
        Self {
            io,
            max_frame_bytes,
        }
    }

    pub fn into_inner(self) -> T {
        self.io
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> FramedIo<T> {
    pub async fn write_json<V: Serialize>(&mut self, value: &V) -> Result<(), RpcError> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > self.max_frame_bytes || bytes.len() > u32::MAX as usize {
            return Err(RpcError::FrameTooLarge {
                actual: bytes.len(),
                limit: self.max_frame_bytes.min(u32::MAX as usize),
            });
        }
        self.io.write_u32(bytes.len() as u32).await?;
        self.io.write_all(&bytes).await?;
        self.io.flush().await?;
        Ok(())
    }

    pub async fn read_json<V: DeserializeOwned>(&mut self) -> Result<V, RpcError> {
        let len = self.io.read_u32().await? as usize;
        if len > self.max_frame_bytes {
            return Err(RpcError::FrameTooLarge {
                actual: len,
                limit: self.max_frame_bytes,
            });
        }
        let mut bytes = vec![0_u8; len];
        self.io.read_exact(&mut bytes).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[derive(Debug, Clone)]
pub struct FleetHandshakeConfig {
    pub supported_protocol_versions: Vec<u16>,
    pub connection_generation: u64,
    pub event_ack: u64,
    pub next_command_seq: u64,
    pub lease: LeaseGrant,
    pub reconnect_proof: bro_protocol::AuthenticationProof,
    pub session_policy: SessionPolicy,
    pub fleet_build: BuildIdentity,
}

/// Complete the worker side of the handshake and retain the framed channel.
pub async fn connect_worker<T>(
    io: T,
    hello: WorkerHello,
) -> Result<(FramedIo<T>, FleetWelcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut framed = FramedIo::new(io);
    let offered_versions = hello.protocol_versions.clone();
    framed
        .write_json(&HandshakeMessage::WorkerHello(hello))
        .await?;
    match framed.read_json::<HandshakeMessage>().await? {
        HandshakeMessage::FleetWelcome(welcome) => {
            if !offered_versions.contains(&welcome.selected_protocol) {
                return Err(RpcError::SelectedProtocolNotOffered {
                    selected: welcome.selected_protocol,
                });
            }
            Ok((framed, welcome))
        }
        HandshakeMessage::Reject(reject) => Err(RpcError::HandshakeRejected {
            code: reject.code,
            message: reject.message,
            supported_protocol_versions: reject.supported_protocol_versions,
        }),
        HandshakeMessage::WorkerHello(_) => Err(RpcError::UnexpectedHandshake("worker_hello")),
    }
}

/// Complete the fleet side of the handshake.
///
/// Authentication runs before a welcome is emitted. The callback receives the
/// full hello so callers can bind task, session, worker, and proof atomically.
pub async fn accept_worker<T, F>(
    io: T,
    config: FleetHandshakeConfig,
    authenticate: F,
) -> Result<(FramedIo<T>, WorkerHello, FleetWelcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce(&WorkerHello) -> Result<(), String>,
{
    let mut framed = FramedIo::new(io);
    let hello = match framed.read_json::<HandshakeMessage>().await? {
        HandshakeMessage::WorkerHello(hello) => hello,
        HandshakeMessage::FleetWelcome(_) => {
            return Err(RpcError::UnexpectedHandshake("fleet_welcome"));
        }
        HandshakeMessage::Reject(_) => return Err(RpcError::UnexpectedHandshake("reject")),
    };

    let selected_protocol = config
        .supported_protocol_versions
        .iter()
        .copied()
        .filter(|version| hello.protocol_versions.contains(version))
        .max();
    let Some(selected_protocol) = selected_protocol else {
        let reject = HandshakeReject {
            code: "protocol.version_mismatch".to_string(),
            message: "worker and fleet have no common protocol version".to_string(),
            supported_protocol_versions: config.supported_protocol_versions,
        };
        framed.write_json(&HandshakeMessage::Reject(reject)).await?;
        return Err(RpcError::VersionMismatch);
    };

    if let Err(message) = authenticate(&hello) {
        let reject = HandshakeReject {
            code: "protocol.authentication_failed".to_string(),
            message: message.clone(),
            supported_protocol_versions: config.supported_protocol_versions,
        };
        framed.write_json(&HandshakeMessage::Reject(reject)).await?;
        return Err(RpcError::Authentication(message));
    }

    let welcome = FleetWelcome {
        selected_protocol,
        connection_generation: config.connection_generation,
        event_ack: config.event_ack,
        next_command_seq: config.next_command_seq,
        lease: config.lease,
        reconnect_proof: config.reconnect_proof,
        session_policy: config.session_policy,
        fleet_build: config.fleet_build,
    };
    framed
        .write_json(&HandshakeMessage::FleetWelcome(welcome.clone()))
        .await?;
    Ok((framed, hello, welcome))
}

#[cfg(test)]
mod tests {
    use bro_core::{SessionId, TaskId, WorkerId};
    use bro_protocol::{AuthenticationProof, WORKER_PROTOCOL_V1};
    use tokio::io::AsyncWriteExt;
    use tokio::io::duplex;

    use super::*;

    fn hello(versions: Vec<u16>) -> WorkerHello {
        WorkerHello {
            protocol_versions: versions,
            worker_build: BuildIdentity {
                version: "0.0.1".to_string(),
                build_id: "worker-build".to_string(),
            },
            worker_id: WorkerId::new("worker-1"),
            task_id: TaskId::new("task-1"),
            session_id: SessionId::new("session-1"),
            bootstrap_or_resume_proof: AuthenticationProof::new("bootstrap"),
            last_local_event_seq: 0,
            last_fleet_command_seq: 0,
            worker_capabilities: vec!["probe".to_string()],
        }
    }

    fn config() -> FleetHandshakeConfig {
        FleetHandshakeConfig {
            supported_protocol_versions: vec![WORKER_PROTOCOL_V1],
            connection_generation: 4,
            event_ack: 0,
            next_command_seq: 1,
            lease: LeaseGrant {
                lease_id: "lease-1".to_string(),
                expires_at_unix_ms: 10_000,
                heartbeat_interval_ms: 1_000,
                reattach_grace_ms: 5_000,
            },
            reconnect_proof: AuthenticationProof::new("reconnect"),
            session_policy: SessionPolicy {
                allowed_capabilities: vec!["probe".to_string()],
                attributes: Default::default(),
            },
            fleet_build: BuildIdentity {
                version: "0.0.1".to_string(),
                build_id: "fleet-build".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn handshake_negotiates_and_authenticates() {
        let (client, server) = duplex(16 * 1024);
        let fleet = tokio::spawn(async move {
            accept_worker(server, config(), |hello| {
                (hello.bootstrap_or_resume_proof.expose_secret() == "bootstrap")
                    .then_some(())
                    .ok_or_else(|| "bad proof".to_string())
            })
            .await
        });
        let (_, welcome) = connect_worker(client, hello(vec![1])).await.unwrap();
        assert_eq!(welcome.selected_protocol, WORKER_PROTOCOL_V1);
        let (_, accepted, _) = fleet.await.unwrap().unwrap();
        assert_eq!(accepted.session_id.as_str(), "session-1");
    }

    #[tokio::test]
    async fn version_mismatch_is_precise_on_both_sides() {
        let (client, server) = duplex(16 * 1024);
        let fleet = tokio::spawn(async move { accept_worker(server, config(), |_| Ok(())).await });
        let err = connect_worker(client, hello(vec![99])).await.unwrap_err();
        assert!(matches!(
            err,
            RpcError::HandshakeRejected {
                ref code,
                ref supported_protocol_versions,
                ..
            } if code == "protocol.version_mismatch"
                && supported_protocol_versions == &vec![WORKER_PROTOCOL_V1]
        ));
        assert!(matches!(
            fleet.await.unwrap(),
            Err(RpcError::VersionMismatch)
        ));
    }

    #[tokio::test]
    async fn handshake_selects_highest_common_version() {
        let (client, server) = duplex(16 * 1024);
        let mut fleet_config = config();
        fleet_config.supported_protocol_versions = vec![1, 2, 4];
        let fleet =
            tokio::spawn(async move { accept_worker(server, fleet_config, |_| Ok(())).await });
        let (_, welcome) = connect_worker(client, hello(vec![1, 2, 3])).await.unwrap();
        assert_eq!(welcome.selected_protocol, 2);
        fleet.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn authentication_rejection_is_typed_for_both_peers() {
        let (client, server) = duplex(16 * 1024);
        let fleet = tokio::spawn(async move {
            accept_worker(server, config(), |_| Err("bad proof".to_string())).await
        });
        let client_error = connect_worker(client, hello(vec![1])).await.unwrap_err();
        assert!(matches!(
            client_error,
            RpcError::HandshakeRejected { ref code, .. }
                if code == "protocol.authentication_failed"
        ));
        assert!(matches!(
            fleet.await.unwrap(),
            Err(RpcError::Authentication(ref message)) if message == "bad proof"
        ));
    }

    #[tokio::test]
    async fn oversized_outbound_frame_is_rejected_before_write() {
        let (left, _right) = duplex(64);
        let mut framed = FramedIo::with_max_frame_bytes(left, 8);
        let err = framed.write_json(&"0123456789").await.unwrap_err();
        assert!(matches!(err, RpcError::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn oversized_inbound_frame_is_rejected_before_allocation() {
        let (mut writer, reader) = duplex(64);
        writer.write_u32(9).await.unwrap();
        writer.write_all(b"123456789").await.unwrap();
        let mut framed = FramedIo::with_max_frame_bytes(reader, 8);
        let err = framed.read_json::<serde_json::Value>().await.unwrap_err();
        assert!(matches!(
            err,
            RpcError::FrameTooLarge {
                actual: 9,
                limit: 8
            }
        ));
    }

    #[tokio::test]
    async fn malformed_json_is_rejected_precisely() {
        let (mut writer, reader) = duplex(64);
        writer.write_u32(4).await.unwrap();
        writer.write_all(b"nope").await.unwrap();
        let mut framed = FramedIo::new(reader);
        let err = framed.read_json::<serde_json::Value>().await.unwrap_err();
        assert!(matches!(err, RpcError::InvalidJson(_)));
    }

    #[tokio::test]
    async fn fleet_rejects_unexpected_first_handshake_message() {
        let (client, server) = duplex(16 * 1024);
        let peer = tokio::spawn(async move {
            let mut framed = FramedIo::new(client);
            let fleet = config();
            framed
                .write_json(&HandshakeMessage::FleetWelcome(FleetWelcome {
                    selected_protocol: WORKER_PROTOCOL_V1,
                    connection_generation: fleet.connection_generation,
                    event_ack: fleet.event_ack,
                    next_command_seq: fleet.next_command_seq,
                    lease: fleet.lease,
                    reconnect_proof: fleet.reconnect_proof,
                    session_policy: fleet.session_policy,
                    fleet_build: fleet.fleet_build,
                }))
                .await
                .unwrap();
        });
        let error = accept_worker(server, config(), |_| Ok(()))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            RpcError::UnexpectedHandshake("fleet_welcome")
        ));
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn client_rejects_unoffered_selected_version() {
        let (client, server) = duplex(16 * 1024);
        let peer = tokio::spawn(async move {
            let mut framed = FramedIo::new(server);
            let _: HandshakeMessage = framed.read_json().await.unwrap();
            let welcome = config();
            let selected = FleetWelcome {
                selected_protocol: 99,
                connection_generation: welcome.connection_generation,
                event_ack: welcome.event_ack,
                next_command_seq: welcome.next_command_seq,
                lease: welcome.lease,
                reconnect_proof: welcome.reconnect_proof,
                session_policy: welcome.session_policy,
                fleet_build: welcome.fleet_build,
            };
            framed
                .write_json(&HandshakeMessage::FleetWelcome(selected))
                .await
                .unwrap();
        });
        let err = connect_worker(client, hello(vec![1])).await.unwrap_err();
        assert!(matches!(
            err,
            RpcError::SelectedProtocolNotOffered { selected: 99 }
        ));
        peer.await.unwrap();
    }
}
