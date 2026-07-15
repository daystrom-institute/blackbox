use std::time::Duration;

use bro_core::{SessionId, TaskId, WorkerId};
use bro_protocol::{
    AuthenticationProof, BuildIdentity, CapabilityRequest, CapabilityResponse, CommandOutcomeAck,
    Envelope, EventAck, LeaseGrant, ProtocolError, ProtocolErrorCode, ReplayRequest, SessionPolicy,
    WORKER_PROTOCOL_V1, WorkerHello, WorkerMessage,
};
use bro_rpc::{
    DisconnectReason, FleetHandshakeGrant, HandshakeOptions, MessagePriority, NegotiatedIo,
    PeerConfig, RpcError, RpcPeer, RpcPhase, accept_worker_with_authority,
    connect_worker_with_options,
};
use serde_json::json;
use tokio::io::{DuplexStream, duplex};

fn hello() -> WorkerHello {
    WorkerHello {
        protocol_versions: vec![WORKER_PROTOCOL_V1],
        worker_build: BuildIdentity {
            version: "0.0.1".to_string(),
            build_id: "worker-build".to_string(),
        },
        worker_id: WorkerId::new("worker-1"),
        task_id: TaskId::new("task-1"),
        session_id: SessionId::new("session-1"),
        bootstrap_or_resume_proof: AuthenticationProof::new("proof"),
        last_local_event_seq: 0,
        last_fleet_command_seq: 0,
        worker_capabilities: vec![],
    }
}

fn grant() -> FleetHandshakeGrant {
    FleetHandshakeGrant {
        connection_generation: 9,
        event_ack: 0,
        next_command_seq: 1,
        lease: LeaseGrant {
            lease_id: "lease-1".to_string(),
            expires_at_unix_ms: 10_000,
            heartbeat_interval_ms: 1_000,
            reattach_grace_ms: 5_000,
        },
        reconnect_proof: AuthenticationProof::new("resume"),
        session_policy: SessionPolicy {
            allowed_capabilities: vec![],
            attributes: Default::default(),
        },
        fleet_build: BuildIdentity {
            version: "0.0.1".to_string(),
            build_id: "fleet-build".to_string(),
        },
    }
}

async fn negotiated_pair(
    capacity: usize,
) -> (NegotiatedIo<DuplexStream>, NegotiatedIo<DuplexStream>) {
    let (worker_stream, fleet_stream) = duplex(capacity);
    let worker = connect_worker_with_options(
        worker_stream,
        hello(),
        HandshakeOptions {
            timeout: Duration::from_secs(2),
            ..HandshakeOptions::default()
        },
    );
    let fleet = accept_worker_with_authority(
        fleet_stream,
        vec![WORKER_PROTOCOL_V1],
        HandshakeOptions {
            timeout: Duration::from_secs(2),
            ..HandshakeOptions::default()
        },
        |_, _| async { Ok(grant()) },
    );
    let (worker, fleet) = tokio::join!(worker, fleet);
    (worker.unwrap().0, fleet.unwrap().0)
}

async fn negotiated_pair_with_frame_limit(
    capacity: usize,
    max_frame_bytes: usize,
) -> (NegotiatedIo<DuplexStream>, NegotiatedIo<DuplexStream>) {
    let (worker_stream, fleet_stream) = duplex(capacity);
    let options = HandshakeOptions {
        max_frame_bytes,
        timeout: Duration::from_secs(2),
    };
    let worker = connect_worker_with_options(worker_stream, hello(), options.clone());
    let fleet = accept_worker_with_authority(
        fleet_stream,
        vec![WORKER_PROTOCOL_V1],
        options,
        |_, _| async { Ok(grant()) },
    );
    let (worker, fleet) = tokio::join!(worker, fleet);
    (worker.unwrap().0, fleet.unwrap().0)
}

fn peer_config() -> PeerConfig {
    PeerConfig {
        read_idle_timeout: None,
        request_timeout: Duration::from_secs(1),
        write_timeout: Duration::from_secs(1),
        ..PeerConfig::default()
    }
}

#[tokio::test]
async fn generation_scoped_request_response_round_trips() {
    let (worker_io, fleet_io) = negotiated_pair(16 * 1024).await;
    let worker = RpcPeer::spawn(worker_io, peer_config()).unwrap();
    let mut fleet = RpcPeer::spawn(fleet_io, peer_config()).unwrap();
    let fleet_handle = fleet.handle();
    let (release_fleet, hold_fleet) = tokio::sync::oneshot::channel();
    let fleet_task = tokio::spawn(async move {
        let request = fleet.recv().await.unwrap();
        assert!(matches!(
            request.body,
            WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq: 7 })
        ));
        fleet_handle
            .respond(
                &request,
                WorkerMessage::EventAck(EventAck {
                    through_event_seq: 12,
                }),
                MessagePriority::Control,
            )
            .unwrap();
        let _ = hold_fleet.await;
    });

    let response = worker
        .handle()
        .request(
            WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq: 7 }),
            MessagePriority::Normal,
        )
        .await
        .unwrap();
    assert_eq!(response.connection_generation, 9);
    assert!(matches!(
        response.body,
        WorkerMessage::EventAck(EventAck {
            through_event_seq: 12
        })
    ));
    let _ = release_fleet.send(());
    fleet_task.await.unwrap();
}

#[tokio::test]
async fn capability_call_uses_envelope_message_id_as_call_id() {
    let (worker_io, fleet_io) = negotiated_pair(16 * 1024).await;
    let worker = RpcPeer::spawn(worker_io, peer_config()).unwrap();
    let mut fleet = RpcPeer::spawn(fleet_io, peer_config()).unwrap();
    let fleet_handle = fleet.handle();
    let (release_fleet, hold_fleet) = tokio::sync::oneshot::channel();
    let fleet_task = tokio::spawn(async move {
        let request = fleet.recv().await.unwrap();
        assert_eq!(request.message_id, "call-1");
        fleet_handle
            .respond(
                &request,
                WorkerMessage::CapabilityResponse(CapabilityResponse::success(
                    "call-1",
                    json!({"hits": 3}),
                )),
                MessagePriority::Control,
            )
            .unwrap();
        let _ = hold_fleet.await;
    });
    let response = worker
        .handle()
        .request_with_id(
            "call-1",
            WorkerMessage::CapabilityRequest(CapabilityRequest {
                call_id: "call-1".to_string(),
                invocation_id: None,
                capability: "corpus".to_string(),
                operation: "search".to_string(),
                bounded_payload: json!({"query": "lease"}),
                deadline_unix_ms: None,
            }),
            MessagePriority::Normal,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert_eq!(response.reply_to.as_deref(), Some("call-1"));
    let _ = release_fleet.send(());
    fleet_task.await.unwrap();
}

#[tokio::test]
async fn stale_generation_disconnects_before_delivery() {
    let (worker_io, fleet_io) = negotiated_pair(16 * 1024).await;
    let fleet = RpcPeer::spawn(fleet_io, peer_config()).unwrap();
    let fleet_handle = fleet.handle();
    let mut raw_worker = worker_io.into_framed();
    raw_worker
        .write_json(&Envelope {
            protocol_version: WORKER_PROTOCOL_V1,
            connection_generation: 8,
            message_id: "stale-message".to_string(),
            reply_to: None,
            body: WorkerMessage::DrainAck,
        })
        .await
        .unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(1), fleet_handle.wait_disconnected())
        .await
        .unwrap();
    assert!(matches!(reason, DisconnectReason::ProtocolViolation(_)));
}

#[tokio::test]
async fn disconnect_fails_all_pending_requests_immediately() {
    let (worker_io, fleet_io) = negotiated_pair(16 * 1024).await;
    let worker = RpcPeer::spawn(worker_io, peer_config()).unwrap();
    let fleet = RpcPeer::spawn(fleet_io, peer_config()).unwrap();
    let handle = worker.handle();
    let request_handle = handle.clone();
    let request = tokio::spawn(async move {
        request_handle
            .request_with_id(
                "pending-1",
                WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq: 1 }),
                MessagePriority::Normal,
                Duration::from_secs(10),
            )
            .await
    });
    while handle.pending_request_count() == 0 {
        tokio::task::yield_now().await;
    }
    drop(fleet);
    let error = tokio::time::timeout(Duration::from_secs(1), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, RpcError::Disconnected { .. }));
    assert_eq!(handle.pending_request_count(), 0);
}

#[tokio::test]
async fn request_timeout_removes_pending_entry() {
    let (worker_io, fleet_io) = negotiated_pair(16 * 1024).await;
    let worker = RpcPeer::spawn(worker_io, peer_config()).unwrap();
    let _fleet = RpcPeer::spawn(fleet_io, peer_config()).unwrap();
    let handle = worker.handle();
    let error = handle
        .request_with_id(
            "timeout-1",
            WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq: 1 }),
            MessagePriority::Normal,
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        RpcError::Timeout {
            phase: RpcPhase::Request
        }
    ));
    assert_eq!(handle.pending_request_count(), 0);
}

#[tokio::test]
async fn in_flight_request_limit_fails_closed() {
    let (worker_io, fleet_io) = negotiated_pair(16 * 1024).await;
    let config = PeerConfig {
        max_in_flight_requests: 1,
        ..peer_config()
    };
    let worker = RpcPeer::spawn(worker_io, config).unwrap();
    let fleet = RpcPeer::spawn(fleet_io, peer_config()).unwrap();
    let handle = worker.handle();
    let first_handle = handle.clone();
    let first = tokio::spawn(async move {
        first_handle
            .request_with_id(
                "first",
                WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq: 1 }),
                MessagePriority::Normal,
                Duration::from_secs(10),
            )
            .await
    });
    while handle.pending_request_count() == 0 {
        tokio::task::yield_now().await;
    }
    let error = handle
        .request_with_id(
            "second",
            WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq: 2 }),
            MessagePriority::Normal,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        RpcError::TooManyInFlightRequests { limit: 1 }
    ));
    drop(fleet);
    let _ = first.await;
}

#[tokio::test]
async fn duplicate_pending_message_id_is_rejected_within_generation() {
    let (worker_io, fleet_io) = negotiated_pair(16 * 1024).await;
    let worker = RpcPeer::spawn(worker_io, peer_config()).unwrap();
    let fleet = RpcPeer::spawn(fleet_io, peer_config()).unwrap();
    let handle = worker.handle();
    let first_handle = handle.clone();
    let first = tokio::spawn(async move {
        first_handle
            .request_with_id(
                "duplicate",
                WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq: 1 }),
                MessagePriority::Normal,
                Duration::from_secs(10),
            )
            .await
    });
    while handle.pending_request_count() == 0 {
        tokio::task::yield_now().await;
    }
    let error = handle
        .request_with_id(
            "duplicate",
            WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq: 2 }),
            MessagePriority::Normal,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        RpcError::DuplicateMessageId {
            generation: 9,
            ref message_id
        } if message_id == "duplicate"
    ));
    drop(fleet);
    let _ = first.await;
}

#[tokio::test]
async fn control_frames_overtake_queued_replay_frames() {
    let (worker_io, mut raw_fleet) = negotiated_pair(64).await;
    let config = PeerConfig {
        write_timeout: Duration::from_secs(2),
        read_idle_timeout: None,
        ..PeerConfig::default()
    };
    let worker = RpcPeer::spawn(worker_io, config).unwrap();
    let handle = worker.handle();
    handle
        .send(
            WorkerMessage::ProtocolError(ProtocolError {
                code: ProtocolErrorCode::Internal,
                message: "f".repeat(32 * 1024),
                fatal: false,
                related_message_id: None,
                expected_protocol_version: None,
                expected_connection_generation: None,
            }),
            MessagePriority::Normal,
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    handle
        .send(
            WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq: 22 }),
            MessagePriority::Replay,
        )
        .unwrap();
    handle
        .send(
            WorkerMessage::CommandOutcomeAck(CommandOutcomeAck {
                through_command_seq: 99,
            }),
            MessagePriority::Control,
        )
        .unwrap();

    let first = raw_fleet.read_envelope().await.unwrap();
    assert!(matches!(first.body, WorkerMessage::ProtocolError(_)));
    let second = raw_fleet.read_envelope().await.unwrap();
    assert!(matches!(
        second.body,
        WorkerMessage::CommandOutcomeAck(CommandOutcomeAck {
            through_command_seq: 99
        })
    ));
    let third = raw_fleet.read_envelope().await.unwrap();
    assert!(matches!(
        third.body,
        WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq: 22 })
    ));
}

#[tokio::test]
async fn byte_budget_bounds_bulk_queue_without_consuming_control_reserve() {
    let (worker_io, _raw_fleet) = negotiated_pair(64).await;
    let config = PeerConfig {
        bulk_queue_bytes: 1_024,
        control_queue_bytes: 1_024,
        write_timeout: Duration::from_secs(2),
        read_idle_timeout: None,
        ..PeerConfig::default()
    };
    let worker = RpcPeer::spawn(worker_io, config).unwrap();
    let handle = worker.handle();
    handle
        .send(
            WorkerMessage::ProtocolError(ProtocolError {
                code: ProtocolErrorCode::Internal,
                message: "b".repeat(500),
                fatal: false,
                related_message_id: None,
                expected_protocol_version: None,
                expected_connection_generation: None,
            }),
            MessagePriority::Normal,
        )
        .unwrap();
    let bulk_error = handle
        .send(
            WorkerMessage::ProtocolError(ProtocolError {
                code: ProtocolErrorCode::Internal,
                message: "c".repeat(500),
                fatal: false,
                related_message_id: None,
                expected_protocol_version: None,
                expected_connection_generation: None,
            }),
            MessagePriority::Replay,
        )
        .unwrap_err();
    assert!(matches!(
        bulk_error,
        RpcError::QueueFull {
            priority: "replay",
            byte_limit: 1_024
        }
    ));
    handle
        .send(
            WorkerMessage::CommandOutcomeAck(CommandOutcomeAck {
                through_command_seq: 1,
            }),
            MessagePriority::Control,
        )
        .unwrap();
}

#[tokio::test]
async fn stalled_writer_hits_deadline_and_disconnects() {
    let (worker_io, _raw_fleet) = negotiated_pair(64).await;
    let config = PeerConfig {
        write_timeout: Duration::from_millis(20),
        read_idle_timeout: None,
        ..PeerConfig::default()
    };
    let worker = RpcPeer::spawn(worker_io, config).unwrap();
    let handle = worker.handle();
    handle
        .send(
            WorkerMessage::ProtocolError(ProtocolError {
                code: ProtocolErrorCode::Internal,
                message: "w".repeat(32 * 1024),
                fatal: false,
                related_message_id: None,
                expected_protocol_version: None,
                expected_connection_generation: None,
            }),
            MessagePriority::Normal,
        )
        .unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(1), handle.wait_disconnected())
        .await
        .unwrap();
    assert!(matches!(reason, DisconnectReason::WriteFailed(_)));
}

#[tokio::test]
async fn idle_reader_hits_configured_deadline() {
    let (worker_io, _raw_fleet) = negotiated_pair(16 * 1024).await;
    let config = PeerConfig {
        read_idle_timeout: Some(Duration::from_millis(20)),
        ..peer_config()
    };
    let worker = RpcPeer::spawn(worker_io, config).unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(1), worker.handle().wait_disconnected())
        .await
        .unwrap();
    assert_eq!(reason, DisconnectReason::IdleTimeout);
}

#[tokio::test]
async fn inbound_queue_holds_a_bounded_number_of_large_frames() {
    let frame_limit = 2 * 1024;
    let inbound_limit = frame_limit + 4;
    let (worker_io, fleet_io) = negotiated_pair_with_frame_limit(16 * 1024, frame_limit).await;
    let worker = RpcPeer::spawn(worker_io, peer_config()).unwrap();
    let config = PeerConfig {
        inbound_queue_bytes: inbound_limit,
        ..peer_config()
    };
    let mut fleet = RpcPeer::spawn(fleet_io, config).unwrap();
    let worker_handle = worker.handle();
    let message = || {
        WorkerMessage::ProtocolError(ProtocolError {
            code: ProtocolErrorCode::Internal,
            message: "x".repeat(1_200),
            fatal: false,
            related_message_id: None,
            expected_protocol_version: None,
            expected_connection_generation: None,
        })
    };
    worker_handle
        .send(message(), MessagePriority::Normal)
        .unwrap();
    worker_handle
        .send(message(), MessagePriority::Normal)
        .unwrap();

    tokio::time::timeout(Duration::from_secs(1), async {
        while fleet.handle().inbound_available_bytes() > inbound_limit / 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(fleet.handle().inbound_available_bytes() < inbound_limit / 2);

    let _ = fleet.recv().await.unwrap();
    let _ = fleet.recv().await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(fleet.handle().inbound_available_bytes(), inbound_limit);
}
