#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

use std::os::unix::fs::PermissionsExt;

use bro_core::{CommandId, SessionId, TaskId};
use bro_protocol::{
    BuildIdentity, Envelope, EventAck, LeaseGrant, SessionPolicy, WORKER_PROTOCOL_V1,
    WorkerCommand, WorkerCommandKind, WorkerLifecycleState, WorkerMessage,
};
use bro_rpc::{FleetHandshakeConfig, accept_worker};
use tempfile::tempdir;
use tokio::net::UnixListener;
use tokio::process::Command;

fn fleet_config() -> FleetHandshakeConfig {
    FleetHandshakeConfig {
        supported_protocol_versions: vec![WORKER_PROTOCOL_V1],
        connection_generation: 3,
        event_ack: 0,
        next_command_seq: 1,
        lease: LeaseGrant {
            lease_id: "probe-lease".to_string(),
            expires_at_unix_ms: u64::MAX,
            heartbeat_interval_ms: 1_000,
            reattach_grace_ms: 5_000,
        },
        reconnect_proof: bro_protocol::AuthenticationProof::new("probe-reconnect"),
        session_policy: SessionPolicy {
            allowed_capabilities: vec!["protocol_probe".to_string()],
            attributes: Default::default(),
        },
        fleet_build: BuildIdentity {
            version: "test".to_string(),
            build_id: "fake-fleet".to_string(),
        },
    }
}

#[tokio::test]
async fn real_harness_binary_completes_probe_exchange() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("fleet.sock");
    let secret = dir.path().join("bootstrap.secret");
    std::fs::write(&secret, "bootstrap-probe\n").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_bro-harness"))
        .args([
            "--worker-probe",
            "--fleet-socket",
            socket.to_str().unwrap(),
            "--task-id",
            "task-probe",
            "--session-id",
            "session-probe",
            "--worker-id",
            "worker-probe",
            "--bootstrap-secret-file",
            secret.to_str().unwrap(),
        ])
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("worker did not connect within 10 seconds")
        .unwrap();
    let (mut framed, hello, welcome) = accept_worker(stream, fleet_config(), |hello| {
        (hello.bootstrap_or_resume_proof.expose_secret() == "bootstrap-probe")
            .then_some(())
            .ok_or_else(|| "wrong bootstrap proof".to_string())
    })
    .await
    .unwrap();
    assert_eq!(hello.task_id, TaskId::new("task-probe"));
    assert_eq!(hello.session_id, SessionId::new("session-probe"));
    assert_eq!(welcome.reconnect_proof.expose_secret(), "probe-reconnect");

    let event: Envelope = framed.read_json().await.unwrap();
    assert_eq!(event.protocol_version, WORKER_PROTOCOL_V1);
    assert_eq!(event.connection_generation, 3);
    assert_eq!(event.message_id, "probe-event-1");
    let WorkerMessage::Event(worker_event) = event.body else {
        panic!("expected worker event");
    };
    assert_eq!(worker_event.event_seq, 1);
    assert_eq!(worker_event.event["type"], "worker_probe_ready");
    framed
        .write_json(&Envelope {
            protocol_version: welcome.selected_protocol,
            connection_generation: welcome.connection_generation,
            message_id: "ack-1".to_string(),
            reply_to: Some(event.message_id.clone()),
            body: WorkerMessage::EventAck(EventAck {
                through_event_seq: 1,
            }),
        })
        .await
        .unwrap();
    framed
        .write_json(&Envelope {
            protocol_version: welcome.selected_protocol,
            connection_generation: welcome.connection_generation,
            message_id: "command-1".to_string(),
            reply_to: None,
            body: WorkerMessage::Command(WorkerCommand {
                command_seq: 1,
                command_id: CommandId::new("command-1"),
                command: WorkerCommandKind::RequestStatus,
            }),
        })
        .await
        .unwrap();
    let outcome: Envelope = framed.read_json().await.unwrap();
    assert_eq!(outcome.reply_to.as_deref(), Some("command-1"));
    let WorkerMessage::CommandOutcome(command_outcome) = outcome.body else {
        panic!("expected command outcome");
    };
    assert_eq!(command_outcome.command_seq, 1);
    assert_eq!(command_outcome.command_id, CommandId::new("command-1"));
    assert!(command_outcome.accepted);
    assert!(command_outcome.terminal);

    let status: Envelope = framed.read_json().await.unwrap();
    assert_eq!(status.reply_to.as_deref(), Some("command-1"));
    let WorkerMessage::Status(status) = status.body else {
        panic!("expected typed worker status");
    };
    assert_eq!(status.worker_id.as_str(), "worker-probe");
    assert_eq!(status.task_id, TaskId::new("task-probe"));
    assert_eq!(status.session_id, SessionId::new("session-probe"));
    assert_eq!(status.protocol_version, WORKER_PROTOCOL_V1);
    assert_eq!(status.connection_generation, 3);
    assert_eq!(status.last_local_event_seq, 1);
    assert_eq!(status.last_fleet_command_seq, 1);
    assert_eq!(status.state, WorkerLifecycleState::Terminal);
    assert!(!status.worker_build.build_id.is_empty());

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("worker probe did not exit within 10 seconds")
        .unwrap();
    assert!(
        output.status.success(),
        "worker probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn real_harness_binary_reports_version_mismatch() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("fleet.sock");
    let secret = dir.path().join("bootstrap.secret");
    std::fs::write(&secret, "bootstrap-probe\n").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_bro-harness"))
        .args([
            "--worker-probe",
            "--fleet-socket",
            socket.to_str().unwrap(),
            "--task-id",
            "task-probe",
            "--session-id",
            "session-probe",
            "--bootstrap-secret-file",
            secret.to_str().unwrap(),
            "--worker-protocol-versions",
            "99",
        ])
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("worker did not connect within 10 seconds")
        .unwrap();
    let result = accept_worker(stream, fleet_config(), |_| Ok(())).await;
    assert!(result.is_err());
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("worker mismatch probe did not exit within 10 seconds")
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("protocol.version_mismatch"));
}

#[tokio::test]
async fn real_harness_binary_reports_authentication_rejection() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("fleet.sock");
    let secret = dir.path().join("bootstrap.secret");
    std::fs::write(&secret, "wrong-proof\n").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_bro-harness"))
        .args([
            "--worker-probe",
            "--fleet-socket",
            socket.to_str().unwrap(),
            "--task-id",
            "task-probe",
            "--session-id",
            "session-probe",
            "--bootstrap-secret-file",
            secret.to_str().unwrap(),
        ])
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("worker did not connect within 10 seconds")
        .unwrap();
    let result = accept_worker(stream, fleet_config(), |_| {
        Err("bootstrap proof mismatch".to_string())
    })
    .await;
    assert!(result.is_err());
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("worker authentication probe did not exit within 10 seconds")
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("protocol.authentication_failed"));
}
