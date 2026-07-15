use std::future::{Future, ready};
use std::time::Duration;

use bro_protocol::{
    AuthenticationProof, BuildIdentity, FleetWelcome, HandshakeMessage, HandshakeReject,
    LeaseGrant, SessionPolicy, WorkerHello,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;

use crate::{ConnectionBinding, FramedIo, NegotiatedIo, RpcError, RpcPhase};

pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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

#[derive(Debug, Clone)]
pub struct FleetHandshakeGrant {
    pub connection_generation: u64,
    pub event_ack: u64,
    pub next_command_seq: u64,
    pub lease: LeaseGrant,
    pub reconnect_proof: AuthenticationProof,
    pub session_policy: SessionPolicy,
    pub fleet_build: BuildIdentity,
}

/// Compatibility configuration for the original one-shot probe API.
#[derive(Debug, Clone)]
pub struct FleetHandshakeConfig {
    pub supported_protocol_versions: Vec<u16>,
    pub connection_generation: u64,
    pub event_ack: u64,
    pub next_command_seq: u64,
    pub lease: LeaseGrant,
    pub reconnect_proof: AuthenticationProof,
    pub session_policy: SessionPolicy,
    pub fleet_build: BuildIdentity,
}

impl FleetHandshakeConfig {
    fn grant(self) -> FleetHandshakeGrant {
        FleetHandshakeGrant {
            connection_generation: self.connection_generation,
            event_ack: self.event_ack,
            next_command_seq: self.next_command_seq,
            lease: self.lease,
            reconnect_proof: self.reconnect_proof,
            session_policy: self.session_policy,
            fleet_build: self.fleet_build,
        }
    }
}

/// Safe peer-visible rejection plus a private local diagnostic.
#[derive(Debug, Clone)]
pub struct HandshakeAuthorityReject {
    pub code: String,
    pub public_message: String,
    pub local_message: String,
}

impl HandshakeAuthorityReject {
    pub fn new(
        code: impl Into<String>,
        public_message: impl Into<String>,
        local_message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            public_message: public_message.into(),
            local_message: local_message.into(),
        }
    }

    fn authentication(local_message: String) -> Self {
        Self::new(
            "protocol.authentication_failed",
            "worker authentication failed",
            local_message,
        )
    }
}

/// Complete the worker side of the handshake and return generation-fenced I/O.
pub async fn connect_worker_with_options<T>(
    io: T,
    hello: WorkerHello,
    options: HandshakeOptions,
) -> Result<(NegotiatedIo<T>, FleetWelcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    validate_options(&options)?;
    timeout(options.timeout, connect_worker_inner(io, hello, options))
        .await
        .map_err(|_| RpcError::Timeout {
            phase: RpcPhase::Handshake,
        })?
}

async fn connect_worker_inner<T>(
    io: T,
    hello: WorkerHello,
    options: HandshakeOptions,
) -> Result<(NegotiatedIo<T>, FleetWelcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut framed = FramedIo::with_max_frame_bytes(io, options.max_frame_bytes);
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
        HandshakeMessage::WorkerHello(_) => Err(RpcError::UnexpectedHandshake("worker_hello")),
    }
}

/// Complete the fleet side of the handshake with one atomic authority call.
///
/// The authority receives the authenticated identity claim and selected
/// protocol together. It must validate the proof, bind task/session/worker,
/// fence any prior owner, allocate the generation and lease, rotate the proof,
/// and return the complete grant as one transaction.
pub async fn accept_worker_with_authority<T, F, Fut>(
    io: T,
    supported_protocol_versions: Vec<u16>,
    options: HandshakeOptions,
    authority: F,
) -> Result<(NegotiatedIo<T>, WorkerHello, FleetWelcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce(WorkerHello, u16) -> Fut,
    Fut: Future<Output = Result<FleetHandshakeGrant, HandshakeAuthorityReject>>,
{
    validate_options(&options)?;
    timeout(
        options.timeout,
        accept_worker_inner(io, supported_protocol_versions, options, authority),
    )
    .await
    .map_err(|_| RpcError::Timeout {
        phase: RpcPhase::Handshake,
    })?
}

async fn accept_worker_inner<T, F, Fut>(
    io: T,
    supported_protocol_versions: Vec<u16>,
    options: HandshakeOptions,
    authority: F,
) -> Result<(NegotiatedIo<T>, WorkerHello, FleetWelcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce(WorkerHello, u16) -> Fut,
    Fut: Future<Output = Result<FleetHandshakeGrant, HandshakeAuthorityReject>>,
{
    let mut framed = FramedIo::with_max_frame_bytes(io, options.max_frame_bytes);
    let hello = match framed.read_json::<HandshakeMessage>().await? {
        HandshakeMessage::WorkerHello(hello) => hello,
        HandshakeMessage::FleetWelcome(_) => {
            return Err(RpcError::UnexpectedHandshake("fleet_welcome"));
        }
        HandshakeMessage::Reject(_) => return Err(RpcError::UnexpectedHandshake("reject")),
    };

    let selected_protocol = supported_protocol_versions
        .iter()
        .copied()
        .filter(|version| hello.protocol_versions.contains(version))
        .max();
    let Some(selected_protocol) = selected_protocol else {
        let reject = HandshakeReject {
            code: "protocol.version_mismatch".to_string(),
            message: "worker and fleet have no common protocol version".to_string(),
            supported_protocol_versions,
        };
        framed.write_json(&HandshakeMessage::Reject(reject)).await?;
        return Err(RpcError::VersionMismatch);
    };

    let grant = match authority(hello.clone(), selected_protocol).await {
        Ok(grant) => grant,
        Err(rejection) => {
            let reject = HandshakeReject {
                code: rejection.code.clone(),
                message: rejection.public_message,
                supported_protocol_versions,
            };
            framed.write_json(&HandshakeMessage::Reject(reject)).await?;
            return Err(RpcError::HandshakeAuthorityRejected {
                code: rejection.code,
                local_message: rejection.local_message,
            });
        }
    };

    let welcome = FleetWelcome {
        selected_protocol,
        connection_generation: grant.connection_generation,
        event_ack: grant.event_ack,
        next_command_seq: grant.next_command_seq,
        lease: grant.lease,
        reconnect_proof: grant.reconnect_proof,
        session_policy: grant.session_policy,
        fleet_build: grant.fleet_build,
    };
    framed
        .write_json(&HandshakeMessage::FleetWelcome(welcome.clone()))
        .await?;
    let binding = ConnectionBinding {
        protocol_version: selected_protocol,
        connection_generation: welcome.connection_generation,
    };
    Ok((NegotiatedIo::new(framed, binding), hello, welcome))
}

/// Compatibility wrapper retaining the probe-facing API.
pub async fn connect_worker<T>(
    io: T,
    hello: WorkerHello,
) -> Result<(FramedIo<T>, FleetWelcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let (negotiated, welcome) =
        connect_worker_with_options(io, hello, HandshakeOptions::default()).await?;
    Ok((negotiated.into_framed(), welcome))
}

/// Compatibility wrapper retaining the original static-config probe API.
pub async fn accept_worker<T, F>(
    io: T,
    config: FleetHandshakeConfig,
    authenticate: F,
) -> Result<(FramedIo<T>, WorkerHello, FleetWelcome), RpcError>
where
    T: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce(&WorkerHello) -> Result<(), String>,
{
    let supported_protocol_versions = config.supported_protocol_versions.clone();
    let grant = config.grant();
    let result = accept_worker_with_authority(
        io,
        supported_protocol_versions,
        HandshakeOptions::default(),
        move |hello, _selected_protocol| {
            let result = authenticate(&hello)
                .map(|()| grant)
                .map_err(HandshakeAuthorityReject::authentication);
            ready(result)
        },
    )
    .await;

    match result {
        Ok((negotiated, hello, welcome)) => Ok((negotiated.into_framed(), hello, welcome)),
        Err(RpcError::HandshakeAuthorityRejected {
            code,
            local_message,
        }) if code == "protocol.authentication_failed" => {
            Err(RpcError::Authentication(local_message))
        }
        Err(error) => Err(error),
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
