//! End-to-end handshake behavior over an in-memory duplex pair and, for the
//! peer-uid check, a real Unix domain socket pair.

use std::time::Duration;

use bro_rpc::{
    BuildIdentity, Envelope, FramedIo, HandshakeMessage, HandshakeOptions, Hello, Reject, RpcError,
    RpcPhase, Welcome, accept, builds_compatible, connect,
};
use tokio::io::duplex;

fn build(version: &str, build_id: &str) -> BuildIdentity {
    BuildIdentity {
        version: version.to_string(),
        build_id: build_id.to_string(),
    }
}

#[tokio::test]
async fn negotiation_picks_the_max_of_the_intersection() {
    let (client, server) = duplex(16 * 1024);
    let fleetd = tokio::spawn(async move {
        accept(
            server,
            build("1.3.0", "fleetd-build"),
            vec![1, 2, 4],
            77,
            HandshakeOptions::default(),
        )
        .await
    });
    let (negotiated, welcome) = connect(
        client,
        build("1.3.0", "daemon-build"),
        vec![1, 2, 3],
        HandshakeOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(welcome.selected_protocol, 2);
    assert_eq!(negotiated.binding().protocol_version, 2);
    assert_eq!(negotiated.binding().connection_generation, 77);

    let (fleetd_negotiated, hello, fleetd_welcome) = fleetd.await.unwrap().unwrap();
    assert_eq!(fleetd_negotiated.binding(), negotiated.binding());
    assert_eq!(hello.build.build_id, "daemon-build");
    assert_eq!(fleetd_welcome, welcome);
}

#[tokio::test]
async fn empty_intersection_rejects_on_the_wire_before_the_local_error() {
    let (client, server) = duplex(16 * 1024);
    let fleetd = tokio::spawn(async move {
        accept(
            server,
            build("1.0.0", "fleetd-build"),
            vec![5],
            1,
            HandshakeOptions::default(),
        )
        .await
    });
    let client_error = connect(
        client,
        build("1.0.0", "daemon-build"),
        vec![1, 2],
        HandshakeOptions::default(),
    )
    .await
    .unwrap_err();
    // The client sees only the public wire-visible rejection.
    assert!(matches!(
        client_error,
        RpcError::HandshakeRejected {
            ref code,
            ref supported_protocol_versions,
            ..
        } if code == "protocol.version_mismatch" && supported_protocol_versions == &vec![5]
    ));
    let rendered = client_error.to_string();
    assert!(rendered.contains("no common protocol version"));

    // The server's local error is a richer diagnostic, never sent on the wire.
    let server_error = fleetd.await.unwrap().unwrap_err();
    assert!(matches!(
        server_error,
        RpcError::HandshakeAuthorityRejected {
            ref code,
            ref local_message,
        } if code == "protocol.version_mismatch"
            && local_message.contains('1')
            && local_message.contains('2')
            && local_message.contains('5')
    ));
}

#[tokio::test]
async fn incompatible_build_rejects_before_welcome() {
    let (client, server) = duplex(16 * 1024);
    let fleetd = tokio::spawn(async move {
        accept(
            server,
            build("2.0.0", "fleetd-build"),
            vec![1],
            1,
            HandshakeOptions::default(),
        )
        .await
    });
    let client_error = connect(
        client,
        build("0.5.0", "daemon-build"),
        vec![1],
        HandshakeOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        client_error,
        RpcError::HandshakeRejected { ref code, .. } if code == "protocol.build_incompatible"
    ));
    assert!(matches!(
        fleetd.await.unwrap().unwrap_err(),
        RpcError::HandshakeAuthorityRejected { ref code, .. }
            if code == "protocol.build_incompatible"
    ));
}

#[test]
fn rolling_window_build_compatibility() {
    // Pre-1.0 requires an exact major.minor match.
    assert!(builds_compatible(
        &build("0.3.1", "a"),
        &build("0.3.9", "b")
    ));
    assert!(!builds_compatible(
        &build("0.3.0", "a"),
        &build("0.4.0", "b")
    ));
    // Post-1.0 tolerates a one-minor-version skew, same major.
    assert!(builds_compatible(
        &build("1.4.0", "a"),
        &build("1.5.0", "b")
    ));
    assert!(builds_compatible(
        &build("1.5.0", "a"),
        &build("1.4.0", "b")
    ));
    assert!(!builds_compatible(
        &build("1.4.0", "a"),
        &build("1.6.0", "b")
    ));
    assert!(!builds_compatible(
        &build("1.0.0", "a"),
        &build("2.0.0", "b")
    ));
    // Malformed version strings are never compatible.
    assert!(!builds_compatible(
        &build("not-a-version", "a"),
        &build("1.0.0", "b")
    ));
}

#[test]
fn build_identity_validation_rejects_empty_and_oversize_build_id() {
    assert!(build("1.0.0", "ok").validate().is_ok());
    assert!(build("1.0.0", "").validate().is_err());
    assert!(build("1.0.0", &"x".repeat(129)).validate().is_err());
    assert!(build("not-a-version", "ok").validate().is_err());
}

#[tokio::test]
async fn client_rejects_a_protocol_version_it_did_not_offer() {
    let (client, server) = duplex(16 * 1024);
    let malicious_fleetd = tokio::spawn(async move {
        let mut framed = FramedIo::new(server);
        let _: HandshakeMessage = framed.read_json().await.unwrap();
        framed
            .write_json(&HandshakeMessage::Welcome(Welcome {
                selected_protocol: 99,
                connection_generation: 1,
                build: build("1.0.0", "fleetd-build"),
            }))
            .await
            .unwrap();
    });
    let error = connect(
        client,
        build("1.0.0", "daemon-build"),
        vec![1],
        HandshakeOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        RpcError::SelectedProtocolNotOffered { selected: 99 }
    ));
    malicious_fleetd.await.unwrap();
}

#[tokio::test]
async fn handshake_has_one_end_to_end_deadline() {
    let (client, _silent_fleetd) = duplex(16 * 1024);
    let options = HandshakeOptions {
        timeout: Duration::from_millis(20),
        ..HandshakeOptions::default()
    };
    let error = connect(client, build("1.0.0", "daemon-build"), vec![1], options)
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
async fn unknown_handshake_variant_is_forward_compatible_via_serde_other() {
    // Internally tagged: a future variant's fields sit alongside `type`, not
    // nested under a separate content key. A genuinely new variant would
    // carry real fields here; `Unknown` must tolerate them, not just an
    // empty/absent payload.
    let raw = serde_json::json!({"type": "future_message", "anything": true, "more": [1, 2]});
    let message: HandshakeMessage = serde_json::from_value(raw).unwrap();
    assert!(matches!(message, HandshakeMessage::Unknown));
}

#[tokio::test]
async fn negotiated_io_round_trips_envelopes_and_fences_stale_generation() {
    let (client, server) = duplex(16 * 1024);
    let fleetd = tokio::spawn(async move {
        accept(
            server,
            build("1.0.0", "fleetd-build"),
            vec![1],
            42,
            HandshakeOptions::default(),
        )
        .await
        .unwrap()
        .0
    });
    let (mut daemon_io, _welcome) = connect(
        client,
        build("1.0.0", "daemon-build"),
        vec![1],
        HandshakeOptions::default(),
    )
    .await
    .unwrap();
    let mut fleetd_io = fleetd.await.unwrap();

    let outgoing: Envelope<serde_json::Value> = Envelope {
        protocol_version: 1,
        connection_generation: 42,
        message_id: "spawn-1".to_string(),
        reply_to: None,
        body: serde_json::json!({"kind": "spawn"}),
    };
    daemon_io.write_envelope(&outgoing).await.unwrap();
    let received: Envelope<serde_json::Value> = fleetd_io.read_envelope().await.unwrap();
    assert_eq!(received.message_id, "spawn-1");
    assert_eq!(received.body, serde_json::json!({"kind": "spawn"}));

    // A message from a superseded connection generation is rejected on write
    // before it ever reaches the wire.
    let mut stale = outgoing.clone();
    stale.connection_generation = 41;
    stale.message_id = "spawn-2".to_string();
    let error = daemon_io.write_envelope(&stale).await.unwrap_err();
    assert!(matches!(
        error,
        RpcError::StaleGeneration {
            expected: 42,
            actual: 41
        }
    ));
}

#[test]
fn hello_and_reject_round_trip_through_json() {
    let hello = Hello {
        protocol_versions: vec![1, 2],
        build: build("1.0.0", "daemon-build"),
    };
    let encoded = serde_json::to_value(&HandshakeMessage::Hello(hello.clone())).unwrap();
    let decoded: HandshakeMessage = serde_json::from_value(encoded).unwrap();
    assert!(
        matches!(decoded, HandshakeMessage::Hello(h) if h.protocol_versions == hello.protocol_versions)
    );

    let reject = Reject {
        code: "protocol.version_mismatch".to_string(),
        message: "no common protocol version".to_string(),
        supported_protocol_versions: vec![1, 2],
    };
    let encoded = serde_json::to_value(&HandshakeMessage::Reject(reject.clone())).unwrap();
    let decoded: HandshakeMessage = serde_json::from_value(encoded).unwrap();
    assert!(matches!(decoded, HandshakeMessage::Reject(r) if r.code == reject.code));
}
