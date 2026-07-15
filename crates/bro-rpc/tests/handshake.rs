use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bro_core::{SessionId, TaskId, WorkerId};
use bro_protocol::{
    AuthenticationProof, BuildIdentity, FleetWelcome, HandshakeMessage, LeaseGrant, SessionPolicy,
    WORKER_PROTOCOL_V1, WorkerHello,
};
use bro_rpc::{
    FleetHandshakeConfig, FleetHandshakeGrant, FramedIo, HandshakeAuthorityReject,
    HandshakeOptions, RpcError, RpcPhase, accept_worker, accept_worker_with_authority,
    connect_worker, connect_worker_with_options,
};
use tokio::io::duplex;

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
        last_local_event_seq: 7,
        last_fleet_command_seq: 3,
        worker_capabilities: vec!["protocol.ordered_replay".to_string()],
    }
}

fn grant() -> FleetHandshakeGrant {
    FleetHandshakeGrant {
        connection_generation: 4,
        event_ack: 6,
        next_command_seq: 4,
        lease: LeaseGrant {
            lease_id: "lease-1".to_string(),
            expires_at_unix_ms: 10_000,
            heartbeat_interval_ms: 1_000,
            reattach_grace_ms: 5_000,
        },
        reconnect_proof: AuthenticationProof::new("reconnect"),
        session_policy: SessionPolicy {
            allowed_capabilities: vec!["corpus".to_string()],
            attributes: Default::default(),
        },
        fleet_build: BuildIdentity {
            version: "0.0.1".to_string(),
            build_id: "fleet-build".to_string(),
        },
    }
}

fn compatibility_config() -> FleetHandshakeConfig {
    let grant = grant();
    FleetHandshakeConfig {
        supported_protocol_versions: vec![WORKER_PROTOCOL_V1],
        connection_generation: grant.connection_generation,
        event_ack: grant.event_ack,
        next_command_seq: grant.next_command_seq,
        lease: grant.lease,
        reconnect_proof: grant.reconnect_proof,
        session_policy: grant.session_policy,
        fleet_build: grant.fleet_build,
    }
}

#[tokio::test]
async fn atomic_authority_allocates_complete_welcome_once() {
    let (client, server) = duplex(16 * 1024);
    let calls = Arc::new(AtomicUsize::new(0));
    let authority_calls = calls.clone();
    let fleet = tokio::spawn(async move {
        accept_worker_with_authority(
            server,
            vec![1, 2, 4],
            HandshakeOptions::default(),
            move |hello, selected| async move {
                authority_calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(selected, 2);
                assert_eq!(hello.session_id.as_str(), "session-1");
                assert_eq!(hello.last_local_event_seq, 7);
                Ok(grant())
            },
        )
        .await
    });
    let (worker_io, welcome) =
        connect_worker_with_options(client, hello(vec![1, 2, 3]), HandshakeOptions::default())
            .await
            .unwrap();
    assert_eq!(welcome.selected_protocol, 2);
    assert_eq!(worker_io.binding().protocol_version, 2);
    assert_eq!(worker_io.binding().connection_generation, 4);
    let (fleet_io, accepted, fleet_welcome) = fleet.await.unwrap().unwrap();
    assert_eq!(fleet_io.binding(), worker_io.binding());
    assert_eq!(accepted.worker_id.as_str(), "worker-1");
    assert_eq!(fleet_welcome, welcome);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn version_mismatch_never_calls_authority() {
    let (client, server) = duplex(16 * 1024);
    let called = Arc::new(AtomicBool::new(false));
    let authority_called = called.clone();
    let fleet = tokio::spawn(async move {
        accept_worker_with_authority(
            server,
            vec![WORKER_PROTOCOL_V1],
            HandshakeOptions::default(),
            move |_, _| async move {
                authority_called.store(true, Ordering::SeqCst);
                Ok(grant())
            },
        )
        .await
    });
    let worker_error =
        connect_worker_with_options(client, hello(vec![99]), HandshakeOptions::default())
            .await
            .unwrap_err();
    assert!(matches!(
        worker_error,
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
    assert!(!called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn authority_rejection_separates_public_and_private_diagnostics() {
    let (client, server) = duplex(16 * 1024);
    let fleet = tokio::spawn(async move {
        accept_worker_with_authority(
            server,
            vec![WORKER_PROTOCOL_V1],
            HandshakeOptions::default(),
            |_, _| async {
                Err(HandshakeAuthorityReject::new(
                    "protocol.authentication_failed",
                    "worker authentication failed",
                    "secret verifier mismatch for database row 42",
                ))
            },
        )
        .await
    });
    let worker_error = connect_worker_with_options(
        client,
        hello(vec![WORKER_PROTOCOL_V1]),
        HandshakeOptions::default(),
    )
    .await
    .unwrap_err();
    let rendered = worker_error.to_string();
    assert!(rendered.contains("worker authentication failed"));
    assert!(!rendered.contains("database row 42"));
    assert!(matches!(
        fleet.await.unwrap(),
        Err(RpcError::HandshakeAuthorityRejected {
            ref local_message,
            ..
        }) if local_message.contains("database row 42")
    ));
}

#[tokio::test]
async fn handshake_has_one_end_to_end_deadline() {
    let (client, _silent_server) = duplex(16 * 1024);
    let options = HandshakeOptions {
        timeout: Duration::from_millis(20),
        ..HandshakeOptions::default()
    };
    let error = connect_worker_with_options(client, hello(vec![1]), options)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        RpcError::Timeout {
            phase: RpcPhase::Handshake
        }
    ));
}

#[tokio::test]
async fn compatibility_probe_api_still_negotiates_and_authenticates() {
    let (client, server) = duplex(16 * 1024);
    let fleet = tokio::spawn(async move {
        accept_worker(server, compatibility_config(), |hello| {
            (hello.bootstrap_or_resume_proof.expose_secret() == "bootstrap")
                .then_some(())
                .ok_or_else(|| "private verifier detail".to_string())
        })
        .await
    });
    let (_, welcome) = connect_worker(client, hello(vec![1])).await.unwrap();
    assert_eq!(welcome.selected_protocol, WORKER_PROTOCOL_V1);
    let (_, accepted, _) = fleet.await.unwrap().unwrap();
    assert_eq!(accepted.task_id.as_str(), "task-1");
}

#[tokio::test]
async fn client_rejects_a_protocol_version_it_did_not_offer() {
    let (client, server) = duplex(16 * 1024);
    let malicious_fleet = tokio::spawn(async move {
        let mut framed = FramedIo::new(server);
        let _: HandshakeMessage = framed.read_json().await.unwrap();
        let grant = grant();
        framed
            .write_json(&HandshakeMessage::FleetWelcome(FleetWelcome {
                selected_protocol: 99,
                connection_generation: grant.connection_generation,
                event_ack: grant.event_ack,
                next_command_seq: grant.next_command_seq,
                lease: grant.lease,
                reconnect_proof: grant.reconnect_proof,
                session_policy: grant.session_policy,
                fleet_build: grant.fleet_build,
            }))
            .await
            .unwrap();
    });
    let error = connect_worker_with_options(
        client,
        hello(vec![WORKER_PROTOCOL_V1]),
        HandshakeOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        RpcError::SelectedProtocolNotOffered { selected: 99 }
    ));
    malicious_fleet.await.unwrap();
}
