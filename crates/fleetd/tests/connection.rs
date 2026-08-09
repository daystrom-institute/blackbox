//! End-to-end tests over a real Unix socket with a fake daemon on the other
//! end and stub shell children standing in for `bro-harness`.
//!
//! Deliberately NOT using the real harness: this slice is about the socket
//! contract, the spawn semantics, and the supervision lifecycle. A stub script
//! that prints known envelope lines and exits with a known code makes every
//! assertion here exact instead of dependent on a provider round-trip.

// Fixture setup (writing stub scripts and event logs before the system under
// test runs) is deliberately blocking std::fs. The crate's `disallowed_methods`
// deny targets blocking I/O on tokio worker threads in the SERVING path; a
// test fixture written before the first await is not that. Same carve-out
// bro-rpc's auth tests take.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bro_protocol::{
    BearerToken, DaemonToFleetd, FLEETD_PROTOCOL_VERSION, FleetdToDaemon, SessionState,
    WorkerSpawnSpec,
};
use bro_rpc::{BuildIdentity, Envelope, HandshakeOptions, NegotiatedIo, ServiceToken};
use fleetd::server::{Fleetd, bind_listener, build_identity, serve};
use tokio::net::UnixStream;

const TEST_TOKEN: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
/// Every wait in this file is bounded, so a broken assumption fails as a test
/// failure rather than hanging the suite.
const DEADLINE: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------- fake daemon

/// The dialing side: what the daemon's future `FleetdExecutor` client will do.
struct FakeDaemon {
    io: NegotiatedIo<UnixStream>,
    next_id: u64,
}

impl FakeDaemon {
    async fn connect(socket: &Path) -> Result<Self, bro_rpc::RpcError> {
        Self::connect_with_build(socket, build_identity()).await
    }

    async fn connect_with_build(
        socket: &Path,
        build: BuildIdentity,
    ) -> Result<Self, bro_rpc::RpcError> {
        let stream = UnixStream::connect(socket).await.expect("dial fleetd");
        let (io, _welcome) = bro_rpc::connect(
            stream,
            build,
            vec![FLEETD_PROTOCOL_VERSION],
            HandshakeOptions::default(),
        )
        .await?;
        Ok(Self { io, next_id: 0 })
    }

    async fn send(&mut self, body: DaemonToFleetd) {
        self.next_id += 1;
        let binding = self.io.binding();
        let envelope = Envelope {
            protocol_version: binding.protocol_version,
            connection_generation: binding.connection_generation,
            message_id: format!("daemon-{}", self.next_id),
            reply_to: None,
            body,
        };
        self.io.write_envelope(&envelope).await.expect("send");
    }

    async fn recv(&mut self) -> Result<FleetdToDaemon, bro_rpc::RpcError> {
        let envelope = tokio::time::timeout(DEADLINE, self.io.read_envelope::<FleetdToDaemon>())
            .await
            .expect("fleetd answered within the deadline")?;
        Ok(envelope.body)
    }

    async fn expect(&mut self) -> FleetdToDaemon {
        self.recv().await.expect("fleetd message")
    }

    /// Authenticate and consume the `Ready` acknowledgement.
    async fn authenticate(&mut self) -> u64 {
        self.send(DaemonToFleetd::Authenticate {
            token: BearerToken::new(TEST_TOKEN),
        })
        .await;
        match self.expect().await {
            FleetdToDaemon::Ready {
                connection_generation,
            } => connection_generation,
            other => panic!("expected ready, got {other:?}"),
        }
    }

    /// Drain until a message satisfying `matcher` arrives.
    async fn recv_until(&mut self, matcher: impl Fn(&FleetdToDaemon) -> bool) -> FleetdToDaemon {
        for _ in 0..200 {
            let message = self.expect().await;
            if matcher(&message) {
                return message;
            }
        }
        panic!("expected message never arrived");
    }
}

// ------------------------------------------------------------------- fixtures

struct Harness {
    _directory: tempfile::TempDir,
    root: PathBuf,
    socket: PathBuf,
    state: Arc<Fleetd>,
}

async fn start_fleetd() -> Harness {
    let directory = tempfile::tempdir().expect("tempdir");
    // Canonicalize: on macOS the tempdir is /var/... but resolves to
    // /private/var/..., and path assertions against uncanonicalized roots
    // silently miss.
    let root = directory.path().canonicalize().expect("canonicalize");
    let socket = root.join("fleetd.sock");
    let listener = bind_listener(&socket).await.expect("bind");
    let state = Fleetd::new(
        ServiceToken::parse(TEST_TOKEN).expect("token"),
        build_identity(),
    );
    tokio::spawn(serve(state.clone(), listener));
    Harness {
        _directory: directory,
        root,
        socket,
        state,
    }
}

/// Write an executable stub child. It stands in for `bro-harness`: prints
/// envelope lines on stdout, optionally waits for a control line on stdin,
/// writes to stderr, and exits with a chosen code.
fn write_stub(root: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = root.join(name);
    std::fs::write(&path, body).expect("write stub");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn spec_for(stub: &Path, session_id: &str, event_log_path: PathBuf) -> WorkerSpawnSpec {
    WorkerSpawnSpec {
        task_id: format!("task-{session_id}"),
        session_id: session_id.to_string(),
        workspace_id: None,
        provider: bro_core::Provider::Glm,
        bin_override: Some(stub.to_string_lossy().into_owned()),
        argv: vec![],
        cwd: None,
        env: Default::default(),
        env_unset: vec![],
        initial_messages: vec![serde_json::json!({"type": "user"})],
        bro_home: event_log_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
        event_log_path,
    }
}

fn write_event_log(path: &Path, seqs: &[u64]) {
    let mut body = String::new();
    for seq in seqs {
        body.push_str(&format!(
            r#"{{"ts":"2026-07-19T00:00:00Z","event":{{"type":"assistant","seq":{seq}}}}}"#
        ));
        body.push('\n');
    }
    std::fs::write(path, body).expect("write event log");
}

// --------------------------------------------------------------- handshake

#[tokio::test]
async fn authenticated_connection_becomes_the_owner() {
    let harness = start_fleetd().await;
    let mut daemon = FakeDaemon::connect(&harness.socket).await.expect("connect");
    let generation = daemon.authenticate().await;
    assert_eq!(generation, 1, "first connection gets generation 1");
    assert!(harness.state.has_owner());
}

/// The token gate is independent of the handshake: a peer that negotiates
/// protocol and build fine but presents the wrong secret gets nothing.
#[tokio::test]
async fn a_wrong_bearer_token_is_refused() {
    let harness = start_fleetd().await;
    let mut daemon = FakeDaemon::connect(&harness.socket).await.expect("connect");
    daemon
        .send(DaemonToFleetd::Authenticate {
            token: BearerToken::new("f".repeat(64)),
        })
        .await;
    match daemon.expect().await {
        FleetdToDaemon::Error { code, .. } => assert_eq!(code, "auth.invalid"),
        other => panic!("expected auth.invalid, got {other:?}"),
    }
    assert!(
        !harness.state.has_owner(),
        "a rejected peer never owns fleetd"
    );
}

/// Authentication is a gate, not a suggestion: a well-formed command sent
/// before it is refused rather than executed.
#[tokio::test]
async fn commands_before_authentication_are_refused() {
    let harness = start_fleetd().await;
    let mut daemon = FakeDaemon::connect(&harness.socket).await.expect("connect");
    daemon.send(DaemonToFleetd::ListSessions).await;
    match daemon.expect().await {
        FleetdToDaemon::Error { code, .. } => assert_eq!(code, "auth.required"),
        other => panic!("expected auth.required, got {other:?}"),
    }
    assert!(!harness.state.has_owner());
}

/// An incompatible build is refused BEFORE any application message, with a
/// wire-visible reason. Pre-1.0 versions require an exact major.minor match.
#[tokio::test]
async fn an_incompatible_build_is_rejected_at_handshake() {
    let harness = start_fleetd().await;
    let error = FakeDaemon::connect_with_build(
        &harness.socket,
        BuildIdentity {
            version: "9.9.9".to_string(),
            build_id: "from-the-future".to_string(),
        },
    )
    .await
    .err()
    .expect("incompatible build must be rejected");
    assert!(
        matches!(error, bro_rpc::RpcError::HandshakeRejected { ref code, .. } if code == "protocol.build_incompatible"),
        "expected a build-incompatibility rejection, got {error:?}"
    );
}

// -------------------------------------------------------- generation fencing

/// A reconnecting daemon fences the previous connection without any separate
/// liveness protocol: authenticate, and the old one is gone.
#[tokio::test]
async fn a_new_connection_fences_the_previous_owner() {
    let harness = start_fleetd().await;
    let mut first = FakeDaemon::connect(&harness.socket).await.expect("connect");
    assert_eq!(first.authenticate().await, 1);

    let mut second = FakeDaemon::connect(&harness.socket).await.expect("connect");
    assert_eq!(
        second.authenticate().await,
        2,
        "each connection gets a fresh, never-reused generation"
    );

    // The superseded connection is dropped, so its next read fails rather
    // than silently continuing to look healthy.
    let fenced = first.recv().await;
    assert!(
        fenced.is_err(),
        "superseded connection must be fenced, got {fenced:?}"
    );
    assert!(harness.state.has_owner(), "the new connection owns fleetd");
}

// --------------------------------------------------------- spawn and relay

#[tokio::test]
async fn spawn_relays_events_then_the_terminal_outcome() {
    let harness = start_fleetd().await;
    let stub = write_stub(
        &harness.root,
        "stub.sh",
        // Emits an event, waits for the initial control line, emits a second
        // event, writes stderr, exits non-zero.
        "#!/bin/sh\n\
         echo '{\"type\":\"system\",\"seq\":1}'\n\
         read -r _line\n\
         echo '{\"type\":\"assistant\",\"seq\":2}'\n\
         echo 'stub stderr line' >&2\n\
         exit 7\n",
    );

    let mut daemon = FakeDaemon::connect(&harness.socket).await.expect("connect");
    daemon.authenticate().await;
    daemon
        .send(DaemonToFleetd::Spawn {
            spec: Box::new(spec_for(
                &stub,
                "sess-1",
                harness.root.join("sess-1.events.jsonl"),
            )),
        })
        .await;

    match daemon.expect().await {
        FleetdToDaemon::SessionStarted {
            session_id,
            task_id,
            workspace_id,
            pid,
        } => {
            assert_eq!(session_id, "sess-1");
            assert_eq!(task_id, "task-sess-1");
            assert_eq!(workspace_id, None);
            assert!(pid.is_some(), "a spawned child must report a pid");
        }
        other => panic!("expected session_started, got {other:?}"),
    }

    // Event 1 arrives before the child has read anything.
    match daemon.expect().await {
        FleetdToDaemon::Event { seq, line, .. } => {
            assert_eq!(seq, Some(1), "top-level seq is extracted from the line");
            assert!(line.contains("\"type\":\"system\""));
        }
        other => panic!("expected event, got {other:?}"),
    }

    // Event 2 only happens if the spec's initial_messages actually reached
    // the child's stdin as an NDJSON line: the stub blocks on `read`.
    match daemon.expect().await {
        FleetdToDaemon::Event { seq, .. } => assert_eq!(seq, Some(2)),
        other => panic!("expected second event, got {other:?}"),
    }

    // SessionExited comes last, after both pumps drained: the stderr snapshot
    // must not race empty on a fast exit.
    match daemon.expect().await {
        FleetdToDaemon::SessionExited {
            session_id,
            exit_code,
            stderr_tail,
        } => {
            assert_eq!(session_id, "sess-1");
            assert_eq!(exit_code, Some(7));
            assert!(
                stderr_tail.contains("stub stderr line"),
                "stderr tail lost the child's output: {stderr_tail:?}"
            );
        }
        other => panic!("expected session_exited, got {other:?}"),
    }
}

/// An event line with no `seq` relays with `seq: None` rather than being
/// dropped or renumbered.
#[tokio::test]
async fn events_without_a_seq_still_relay() {
    let harness = start_fleetd().await;
    let stub = write_stub(
        &harness.root,
        "noseq.sh",
        "#!/bin/sh\necho '{\"type\":\"legacy\"}'\nexit 0\n",
    );
    let mut daemon = FakeDaemon::connect(&harness.socket).await.expect("connect");
    daemon.authenticate().await;
    daemon
        .send(DaemonToFleetd::Spawn {
            spec: Box::new(spec_for(
                &stub,
                "sess-legacy",
                harness.root.join("legacy.events.jsonl"),
            )),
        })
        .await;

    let event = daemon
        .recv_until(|m| matches!(m, FleetdToDaemon::Event { .. }))
        .await;
    match event {
        FleetdToDaemon::Event { seq, line, .. } => {
            assert_eq!(seq, None);
            assert!(line.contains("legacy"));
        }
        other => panic!("unreachable: {other:?}"),
    }
}

#[tokio::test]
async fn spawning_an_unresolvable_binary_reports_an_error() {
    let harness = start_fleetd().await;
    let mut daemon = FakeDaemon::connect(&harness.socket).await.expect("connect");
    daemon.authenticate().await;
    let missing = harness.root.join("definitely-not-here");
    daemon
        .send(DaemonToFleetd::Spawn {
            spec: Box::new(spec_for(
                &missing,
                "sess-missing",
                harness.root.join("missing.events.jsonl"),
            )),
        })
        .await;
    match daemon.expect().await {
        FleetdToDaemon::Error {
            session_id, code, ..
        } => {
            assert_eq!(session_id.as_deref(), Some("sess-missing"));
            assert_eq!(code, "spawn.failed");
        }
        other => panic!("expected a spawn error, got {other:?}"),
    }
}

// ------------------------------------------------------------------ control

#[tokio::test]
async fn kill_is_idempotent_and_unknown_sessions_are_no_ops() {
    let harness = start_fleetd().await;
    let stub = write_stub(
        &harness.root,
        "sleeper.sh",
        "#!/bin/sh\necho '{\"type\":\"system\",\"seq\":1}'\nexec sleep 300\n",
    );
    let mut daemon = FakeDaemon::connect(&harness.socket).await.expect("connect");
    daemon.authenticate().await;
    daemon
        .send(DaemonToFleetd::Spawn {
            spec: Box::new(spec_for(
                &stub,
                "sess-kill",
                harness.root.join("kill.events.jsonl"),
            )),
        })
        .await;
    daemon
        .recv_until(|m| matches!(m, FleetdToDaemon::SessionStarted { .. }))
        .await;

    // Killing an unknown session must not error or tear the connection down.
    daemon
        .send(DaemonToFleetd::Kill {
            session_id: "no-such-session".to_string(),
        })
        .await;
    // Two kills of the real session: the second is a no-op, not a second
    // signal.
    for _ in 0..2 {
        daemon
            .send(DaemonToFleetd::Kill {
                session_id: "sess-kill".to_string(),
            })
            .await;
    }

    let exited = daemon
        .recv_until(|m| matches!(m, FleetdToDaemon::SessionExited { .. }))
        .await;
    match exited {
        FleetdToDaemon::SessionExited { session_id, .. } => assert_eq!(session_id, "sess-kill"),
        other => panic!("unreachable: {other:?}"),
    }
}

/// A fully-acknowledged terminal session is GC'd; that is the only persistence
/// lifecycle fleetd has.
#[tokio::test]
async fn list_sessions_reflects_lifecycle_and_ack_gc() {
    let harness = start_fleetd().await;
    let stub = write_stub(
        &harness.root,
        "quick.sh",
        "#!/bin/sh\necho '{\"type\":\"system\",\"seq\":4}'\nexit 0\n",
    );
    let mut daemon = FakeDaemon::connect(&harness.socket).await.expect("connect");
    daemon.authenticate().await;
    daemon
        .send(DaemonToFleetd::Spawn {
            spec: Box::new(spec_for(
                &stub,
                "sess-gc",
                harness.root.join("gc.events.jsonl"),
            )),
        })
        .await;
    daemon
        .recv_until(|m| matches!(m, FleetdToDaemon::SessionExited { .. }))
        .await;

    daemon.send(DaemonToFleetd::ListSessions).await;
    let listed = daemon
        .recv_until(|m| matches!(m, FleetdToDaemon::Sessions { .. }))
        .await;
    match listed {
        FleetdToDaemon::Sessions { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, "sess-gc");
            assert_eq!(sessions[0].state, SessionState::Exited);
            assert_eq!(sessions[0].exit_code, Some(0));
            assert_eq!(sessions[0].last_seq, Some(4));
        }
        other => panic!("unreachable: {other:?}"),
    }

    daemon
        .send(DaemonToFleetd::EventAck {
            session_id: "sess-gc".to_string(),
            through_seq: 4,
        })
        .await;
    daemon.send(DaemonToFleetd::ListSessions).await;
    let listed = daemon
        .recv_until(|m| matches!(m, FleetdToDaemon::Sessions { .. }))
        .await;
    match listed {
        FleetdToDaemon::Sessions { sessions } => assert!(
            sessions.is_empty(),
            "a fully-acked terminal session must be GC'd, got {sessions:?}"
        ),
        other => panic!("unreachable: {other:?}"),
    }
}

// ------------------------------------------------------------------- replay

#[tokio::test]
async fn replay_streams_the_event_log_tail_and_terminates() {
    let harness = start_fleetd().await;
    let log = harness.root.join("replay.events.jsonl");
    write_event_log(&log, &[90, 91, 92]);
    let stub = write_stub(
        &harness.root,
        "replay-stub.sh",
        "#!/bin/sh\nexec sleep 300\n",
    );

    let mut daemon = FakeDaemon::connect(&harness.socket).await.expect("connect");
    daemon.authenticate().await;
    daemon
        .send(DaemonToFleetd::Spawn {
            spec: Box::new(spec_for(&stub, "sess-replay", log.clone())),
        })
        .await;
    daemon
        .recv_until(|m| matches!(m, FleetdToDaemon::SessionStarted { .. }))
        .await;

    daemon
        .send(DaemonToFleetd::ReplayFrom {
            session_id: "sess-replay".to_string(),
            from_seq: 90,
        })
        .await;

    let mut replayed = Vec::new();
    loop {
        match daemon.expect().await {
            FleetdToDaemon::Event { seq, line, .. } => {
                // What arrives is the inner event, indistinguishable from a
                // live stdout line: no `ts` wrapper.
                assert!(!line.contains("\"ts\""), "log wrapper leaked: {line}");
                replayed.push(seq.expect("replayed events are seq-positioned"));
            }
            FleetdToDaemon::ReplayComplete { through_seq, .. } => {
                assert_eq!(through_seq, 92);
                break;
            }
            other => panic!("unexpected message during replay: {other:?}"),
        }
    }
    assert_eq!(replayed, vec![91, 92], "from_seq is exclusive");

    daemon
        .send(DaemonToFleetd::Kill {
            session_id: "sess-replay".to_string(),
        })
        .await;
}

/// The gap case: the daemon's cursor predates everything the log retains. It
/// gets the exact window that DOES exist rather than a silently short replay.
#[tokio::test]
async fn replay_below_the_retained_window_is_reported_as_unavailable() {
    let harness = start_fleetd().await;
    let log = harness.root.join("gap.events.jsonl");
    write_event_log(&log, &[90, 91, 92]);
    let stub = write_stub(&harness.root, "gap-stub.sh", "#!/bin/sh\nexec sleep 300\n");

    let mut daemon = FakeDaemon::connect(&harness.socket).await.expect("connect");
    daemon.authenticate().await;
    daemon
        .send(DaemonToFleetd::Spawn {
            spec: Box::new(spec_for(&stub, "sess-gap", log.clone())),
        })
        .await;
    daemon
        .recv_until(|m| matches!(m, FleetdToDaemon::SessionStarted { .. }))
        .await;

    daemon
        .send(DaemonToFleetd::ReplayFrom {
            session_id: "sess-gap".to_string(),
            from_seq: 3,
        })
        .await;
    let message = daemon
        .recv_until(|m| matches!(m, FleetdToDaemon::ReplayUnavailable { .. }))
        .await;
    match message {
        FleetdToDaemon::ReplayUnavailable {
            session_id,
            requested_from,
            earliest_available,
            latest_available,
        } => {
            assert_eq!(session_id, "sess-gap");
            assert_eq!(requested_from, 3);
            assert_eq!(earliest_available, 90);
            assert_eq!(latest_available, 92);
        }
        other => panic!("unreachable: {other:?}"),
    }

    daemon
        .send(DaemonToFleetd::Kill {
            session_id: "sess-gap".to_string(),
        })
        .await;
}

#[tokio::test]
async fn replaying_an_unknown_session_is_an_error_not_a_hang() {
    let harness = start_fleetd().await;
    let mut daemon = FakeDaemon::connect(&harness.socket).await.expect("connect");
    daemon.authenticate().await;
    daemon
        .send(DaemonToFleetd::ReplayFrom {
            session_id: "ghost".to_string(),
            from_seq: 0,
        })
        .await;
    match daemon.expect().await {
        FleetdToDaemon::Error { code, .. } => assert_eq!(code, "session.unknown"),
        other => panic!("expected session.unknown, got {other:?}"),
    }
}

// ------------------------------------------------------------- re-adoption

/// The reason fleetd exists: a daemon restart must not drop live sessions.
/// The child keeps running across the disconnect, and the next connection can
/// enumerate it and resume ingesting from its own cursor.
#[tokio::test]
async fn sessions_survive_a_daemon_disconnect_and_are_re_adoptable() {
    let harness = start_fleetd().await;
    let log = harness.root.join("adopt.events.jsonl");
    let stub = write_stub(
        &harness.root,
        "adopt-stub.sh",
        "#!/bin/sh\necho '{\"type\":\"system\",\"seq\":1}'\nexec sleep 300\n",
    );

    let mut first = FakeDaemon::connect(&harness.socket).await.expect("connect");
    first.authenticate().await;
    first
        .send(DaemonToFleetd::Spawn {
            spec: Box::new(spec_for(&stub, "sess-adopt", log.clone())),
        })
        .await;
    first
        .recv_until(|m| matches!(m, FleetdToDaemon::Event { .. }))
        .await;

    // The daemon goes away mid-session, exactly as a rebuild-and-kickstart
    // does. The child is NOT killed.
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        harness.state.registry().len(),
        1,
        "a disconnect must not evict the session registry"
    );

    // A brand-new daemon re-adopts by listing, then replaying per session.
    let mut second = FakeDaemon::connect(&harness.socket)
        .await
        .expect("reconnect");
    let generation = second.authenticate().await;
    assert!(generation >= 2, "the reconnect gets a fresh generation");

    second.send(DaemonToFleetd::ListSessions).await;
    let listed = second
        .recv_until(|m| matches!(m, FleetdToDaemon::Sessions { .. }))
        .await;
    match listed {
        FleetdToDaemon::Sessions { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, "sess-adopt");
            assert_eq!(
                sessions[0].state,
                SessionState::Running,
                "the child kept running across the disconnect"
            );
            assert_eq!(sessions[0].last_seq, Some(1));
            assert_eq!(sessions[0].event_log_path, log);
        }
        other => panic!("unreachable: {other:?}"),
    }

    second
        .send(DaemonToFleetd::Kill {
            session_id: "sess-adopt".to_string(),
        })
        .await;
    second
        .recv_until(|m| matches!(m, FleetdToDaemon::SessionExited { .. }))
        .await;
}
