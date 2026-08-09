//! The post-handshake daemon<->fleetd message contract.
//!
//! Slice 5 of `design/daemon-runtime/locality-first-decomposition.md`: the
//! daemon composes a fully-resolved [`WorkerSpawnSpec`] and hands it over a
//! narrow typed local RPC; `fleetd` executes and supervises the child and
//! relays its stdio lanes back. Everything here is pure serde with no I/O:
//! these bodies ride the payload-generic `bro_rpc::Envelope<T>`, which owns
//! framing, auth, and generation fencing.
//!
//! Three shape rules this module encodes:
//!
//! 1. **Direction-typed, never one flat bidirectional enum.** [`DaemonToFleetd`]
//!    and [`FleetdToDaemon`] are separate types, so a side cannot accidentally
//!    send a message only the other side is allowed to originate, and neither
//!    side has to carry a match arm for messages it will never receive.
//! 2. **Internally tagged with an `#[serde(other)]` catch-all.** A newer peer
//!    may add a variant; an older peer must decode it as [`DaemonToFleetd::Unknown`]
//!    / [`FleetdToDaemon::Unknown`] and skip it rather than failing the whole
//!    connection. Adjacent tagging (`tag`+`content`) cannot do this: its
//!    fallback still tries to deserialize `content` as the fallback variant's
//!    own (unit) shape, so it breaks the moment a new variant carries data.
//!    Same lesson as `bro_rpc::HandshakeMessage`; do not "tidy" these into
//!    adjacent tagging.
//! 3. **The event log is the replay source, the daemon owns the cursor.**
//!    There is no in-memory replay buffer message. The daemon asks
//!    [`DaemonToFleetd::ReplayFrom`] with its own last-ingested seq and fleetd
//!    streams the session's durable event-log tail, or answers
//!    [`FleetdToDaemon::ReplayUnavailable`] with the window it actually holds.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use bro_core::WorkspaceId;

use crate::worker::{
    REDACTED, WorkerSpawnSpec, WorkerWorkspaceScope, WorkspaceBindingToken,
    WorkspaceInspectionOutcome, WorkspaceInspectionRequest,
};

/// The shared-secret bearer token, as it crosses the wire.
///
/// A newtype for exactly one reason: `DaemonToFleetd` derives `Debug`, and a
/// plain `String` field would print the secret into any log line or error that
/// formats a message. Serde is transparent (the wire form is the bare string,
/// because the executor must receive the real value); `Debug`/`Display` are
/// not. Same shape and same rationale as [`crate::SecretEnv`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BearerToken(String);

impl BearerToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The raw secret, for a constant-time comparison against the loaded
    /// `ServiceToken`. Never log or `Display` the result.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("BearerToken")
            .field(&REDACTED)
            .finish()
    }
}

impl std::fmt::Display for BearerToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// Protocol version for the daemon<->fleetd channel, offered/selected through
/// the `bro_rpc` handshake. Bump when a change is not additively decodable by
/// the `#[serde(other)]` fallbacks below.
pub const FLEETD_PROTOCOL_VERSION: u16 = 1;

/// Messages the daemon originates. fleetd never sends these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum DaemonToFleetd {
    /// Present the shared-secret bearer token. This MUST be the first
    /// post-handshake envelope on a connection; fleetd refuses everything
    /// else until it has verified the token.
    ///
    /// It rides here rather than in the `bro_rpc` handshake because that
    /// handshake is payload-generic by design: it negotiates protocol version
    /// and build compatibility for any same-host channel and deliberately
    /// knows nothing about what the daemon and fleetd say to each other. The
    /// token check is this contract's concern, so it lives in this contract.
    Authenticate { token: BearerToken },
    /// Spawn a supervised worker child from a fully-resolved spec. fleetd
    /// re-derives no policy from it beyond final binary path resolution.
    Spawn { spec: Box<WorkerSpawnSpec> },
    /// Ask the worker host to prove whether `cwd` belongs to an explicitly
    /// managed checkout and, if so, which daemon-supplied durable scope it
    /// contains. fleetd verifies local facts only; candidate policy remains
    /// daemon-owned.
    InspectWorkspace {
        request_id: String,
        request: WorkspaceInspectionRequest,
    },
    /// Deliver one control-lane message (user turn, `control_request`, ...) to
    /// a live session's stdin as an NDJSON line.
    Control { session_id: String, message: Value },
    /// Idempotent SIGTERM to a session's child. Unknown or already-exited
    /// sessions are a no-op, not an error.
    Kill { session_id: String },
    /// Ask for every session fleetd currently holds. The daemon issues this
    /// right after a reconnect handshake to re-adopt sessions that outlived
    /// its own restart.
    ListSessions,
    /// Stream the session's durable event-log tail with event seq strictly
    /// greater than `from_seq`. `from_seq = 0` means "everything".
    ReplayFrom { session_id: String, from_seq: u64 },
    /// The daemon has durably ingested every event through `through_seq`.
    /// Advisory: it lets fleetd GC terminal-session state, and never gates
    /// live relay.
    EventAck {
        session_id: String,
        through_seq: u64,
    },
    /// Forward compatibility: a variant this build does not know. Receivers
    /// skip it instead of dropping the connection.
    #[serde(other)]
    Unknown,
}

/// Messages fleetd originates. The daemon never sends these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum FleetdToDaemon {
    /// The connection is authenticated and is now the owner connection. The
    /// echoed `connection_generation` is the one fleetd allocated for this
    /// connection: any older connection is fenced out as of this message.
    Ready { connection_generation: u64 },
    /// A [`DaemonToFleetd::Spawn`] produced a live child.
    SessionStarted {
        session_id: String,
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<WorkspaceId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    /// Answer to [`DaemonToFleetd::InspectWorkspace`].
    WorkspaceInspected {
        request_id: String,
        outcome: WorkspaceInspectionOutcome,
    },
    /// One raw harness stdout envelope line, relayed verbatim. `seq` is the
    /// line's own top-level `seq` field when it carries one; fleetd does not
    /// invent or renumber sequence values, so a pre-seq harness build simply
    /// relays `None` and the daemon falls back to its own ordering.
    Event {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        line: String,
    },
    /// The child exited and both stdio pumps drained (same ordering the
    /// daemon's `LocalExecutor` enforces, so a fast fatal exit cannot race the
    /// stderr snapshot empty).
    SessionExited {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        stderr_tail: String,
    },
    /// Answer to [`DaemonToFleetd::ListSessions`].
    Sessions { sessions: Vec<SessionSummary> },
    /// The requested replay start is older than the durable log reaches. The
    /// three-field payload tells the daemon exactly what window exists so it
    /// can decide between resuming at `earliest_available` (accepting a
    /// documented gap) and giving up on the session's history.
    ReplayUnavailable {
        session_id: String,
        requested_from: u64,
        earliest_available: u64,
        latest_available: u64,
    },
    /// Terminator for a replay stream: every event through `through_seq` has
    /// been sent as an [`FleetdToDaemon::Event`]. Lets the daemon know when it
    /// may resume treating the session as live-tailing rather than catching up.
    ReplayComplete {
        session_id: String,
        through_seq: u64,
    },
    /// Something went wrong servicing a daemon request. `session_id` is set
    /// when the failure is scoped to one session.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        code: String,
        message: String,
    },
    /// Forward compatibility: a variant this build does not know.
    #[serde(other)]
    Unknown,
}

/// Lifecycle state of one supervised session, as fleetd sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Child is alive.
    Running,
    /// Child exited; fleetd retains the registry entry so a reconnecting
    /// daemon can still learn the terminal outcome and replay the tail.
    Exited,
    /// Forward compatibility: a state this build does not know.
    #[serde(other)]
    Unknown,
}

/// Per-session row in a [`FleetdToDaemon::Sessions`] answer. This is what a
/// restarted daemon re-adopts from: enough to repopulate the roster and issue
/// a per-session [`DaemonToFleetd::ReplayFrom`] against its own cursor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// Durable scope proved with the workspace id at spawn time. This lets a
    /// restarted daemon restore the binding without opening worker-local cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_scope: Option<WorkerWorkspaceScope>,
    /// Opaque session/workspace capability retained by fleetd solely so a
    /// restarted daemon can restore the exact binding the live harness still
    /// presents. Debug is redacted by the token newtype.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_binding_token: Option<WorkspaceBindingToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub state: SessionState,
    /// Highest event seq fleetd has observed on this session's stdout. `None`
    /// when no seq-carrying event has been seen yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
    /// Where the durable event log lives, taken verbatim from the spawn spec.
    pub event_log_path: PathBuf,
    /// Terminal exit code, once the child has exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip_daemon(message: &DaemonToFleetd) {
        let text = serde_json::to_string(message).expect("serialize");
        let back: DaemonToFleetd = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(message, &back);
    }

    #[test]
    fn legacy_session_started_without_workspace_id_decodes_unbound() {
        let decoded: FleetdToDaemon = serde_json::from_value(json!({
            "type": "session_started",
            "session_id": "sess-1",
            "task_id": "task-1",
            "pid": 42
        }))
        .unwrap();
        assert!(matches!(
            decoded,
            FleetdToDaemon::SessionStarted {
                workspace_id: None,
                ..
            }
        ));
    }

    #[test]
    fn legacy_session_summary_without_workspace_binding_decodes_unbound() {
        let decoded: FleetdToDaemon = serde_json::from_value(json!({
            "type": "sessions",
            "sessions": [{
                "session_id": "sess-1",
                "task_id": "task-1",
                "workspace_id": "0123456789abcdef0123456789abcdef",
                "state": "running",
                "event_log_path": "/state/bro/sess-1.events.jsonl"
            }]
        }))
        .unwrap();
        let FleetdToDaemon::Sessions { sessions } = decoded else {
            panic!("expected sessions message");
        };
        assert_eq!(sessions[0].workspace_binding_token, None);
        assert_eq!(sessions[0].workspace_scope, None);
    }

    fn round_trip_fleetd(message: &FleetdToDaemon) {
        let text = serde_json::to_string(message).expect("serialize");
        let back: FleetdToDaemon = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(message, &back);
    }

    fn sample_spec() -> WorkerSpawnSpec {
        WorkerSpawnSpec {
            task_id: "task-1".to_string(),
            session_id: "sess-1".to_string(),
            workspace_id: Some(WorkspaceId::parse("0123456789abcdef0123456789abcdef").unwrap()),
            workspace_scope: Some(WorkerWorkspaceScope::try_new("repo-1", ".").unwrap()),
            provider: bro_core::Provider::Glm,
            bin_override: Some("bro-harness".to_string()),
            argv: vec!["--daemon-worker".to_string()],
            cwd: Some("/repo/x".to_string()),
            env: Default::default(),
            env_unset: vec!["BRO_HOME".to_string()],
            initial_messages: vec![json!({"type": "user"})],
            bro_home: PathBuf::from("/state/bro"),
            event_log_path: PathBuf::from("/state/bro/sess-1.events.jsonl"),
        }
    }

    #[test]
    fn daemon_messages_round_trip() {
        for message in [
            DaemonToFleetd::Authenticate {
                token: BearerToken::new("a".repeat(64)),
            },
            DaemonToFleetd::Spawn {
                spec: Box::new(sample_spec()),
            },
            DaemonToFleetd::InspectWorkspace {
                request_id: "inspect-1".to_string(),
                request: WorkspaceInspectionRequest {
                    cwd: "/repo/x".to_string(),
                    candidate_scopes: vec![WorkerWorkspaceScope::try_new("repo-1", ".").unwrap()],
                },
            },
            DaemonToFleetd::Control {
                session_id: "sess-1".to_string(),
                message: json!({"type": "user", "message": {"role": "user"}}),
            },
            DaemonToFleetd::Kill {
                session_id: "sess-1".to_string(),
            },
            DaemonToFleetd::ListSessions,
            DaemonToFleetd::ReplayFrom {
                session_id: "sess-1".to_string(),
                from_seq: 42,
            },
            DaemonToFleetd::EventAck {
                session_id: "sess-1".to_string(),
                through_seq: 42,
            },
        ] {
            round_trip_daemon(&message);
        }
    }

    #[test]
    fn fleetd_messages_round_trip() {
        for message in [
            FleetdToDaemon::Ready {
                connection_generation: 3,
            },
            FleetdToDaemon::SessionStarted {
                session_id: "sess-1".to_string(),
                task_id: "task-1".to_string(),
                workspace_id: Some(WorkspaceId::parse("0123456789abcdef0123456789abcdef").unwrap()),
                pid: Some(4242),
            },
            FleetdToDaemon::WorkspaceInspected {
                request_id: "inspect-1".to_string(),
                outcome: WorkspaceInspectionOutcome::Managed {
                    identity: crate::WorkerWorkspaceIdentity {
                        workspace_id: WorkspaceId::parse("0123456789abcdef0123456789abcdef")
                            .unwrap(),
                        scope: WorkerWorkspaceScope::try_new("repo-1", ".").unwrap(),
                    },
                },
            },
            FleetdToDaemon::Event {
                session_id: "sess-1".to_string(),
                seq: Some(7),
                line: r#"{"type":"assistant","seq":7}"#.to_string(),
            },
            FleetdToDaemon::Event {
                session_id: "sess-1".to_string(),
                seq: None,
                line: r#"{"type":"assistant"}"#.to_string(),
            },
            FleetdToDaemon::SessionExited {
                session_id: "sess-1".to_string(),
                exit_code: Some(0),
                stderr_tail: "warning: something\n".to_string(),
            },
            FleetdToDaemon::Sessions {
                sessions: vec![SessionSummary {
                    session_id: "sess-1".to_string(),
                    task_id: "task-1".to_string(),
                    workspace_id: Some(
                        WorkspaceId::parse("0123456789abcdef0123456789abcdef").unwrap(),
                    ),
                    workspace_scope: Some(WorkerWorkspaceScope::try_new("repo-1", ".").unwrap()),
                    workspace_binding_token: Some(
                        WorkspaceBindingToken::parse("b".repeat(64)).unwrap(),
                    ),
                    pid: Some(4242),
                    state: SessionState::Running,
                    last_seq: Some(7),
                    event_log_path: PathBuf::from("/state/bro/sess-1.events.jsonl"),
                    exit_code: None,
                }],
            },
            FleetdToDaemon::ReplayUnavailable {
                session_id: "sess-1".to_string(),
                requested_from: 3,
                earliest_available: 90,
                latest_available: 120,
            },
            FleetdToDaemon::ReplayComplete {
                session_id: "sess-1".to_string(),
                through_seq: 120,
            },
            FleetdToDaemon::Error {
                session_id: Some("sess-1".to_string()),
                code: "spawn_failed".to_string(),
                message: "no such file".to_string(),
            },
        ] {
            round_trip_fleetd(&message);
        }
    }

    /// The tag is `type` and the payload is inlined (internal tagging), which
    /// is what makes the `#[serde(other)]` fallbacks below actually work.
    #[test]
    fn messages_are_internally_tagged() {
        let value = serde_json::to_value(DaemonToFleetd::Kill {
            session_id: "sess-1".to_string(),
        })
        .expect("serialize");
        assert_eq!(value, json!({"type": "kill", "session_id": "sess-1"}));
        assert!(
            value.get("content").is_none(),
            "adjacent tagging breaks unknown-variant tolerance; keep internal tagging"
        );
    }

    /// A newer peer's unknown variant must decode to `Unknown`, INCLUDING when
    /// it carries data. This is the exact case adjacent tagging fails.
    #[test]
    fn unknown_daemon_variant_with_data_decodes_as_unknown() {
        let future = json!({
            "type": "pause_session",
            "session_id": "sess-1",
            "reason": {"nested": ["data", 1, true]}
        });
        let decoded: DaemonToFleetd = serde_json::from_value(future).expect("unknown tolerated");
        assert_eq!(decoded, DaemonToFleetd::Unknown);
    }

    #[test]
    fn unknown_fleetd_variant_with_data_decodes_as_unknown() {
        let future = json!({
            "type": "session_paused",
            "session_id": "sess-1",
            "detail": {"nested": ["data", 1, true]}
        });
        let decoded: FleetdToDaemon = serde_json::from_value(future).expect("unknown tolerated");
        assert_eq!(decoded, FleetdToDaemon::Unknown);
    }

    #[test]
    fn unknown_session_state_decodes_as_unknown() {
        let decoded: SessionState = serde_json::from_value(json!("draining")).expect("tolerated");
        assert_eq!(decoded, SessionState::Unknown);
    }

    /// A spec crossing the wire inside a `Spawn` must arrive intact, secrets
    /// included: the executor cannot re-derive them.
    #[test]
    fn spawn_carries_the_spec_verbatim() {
        let message = DaemonToFleetd::Spawn {
            spec: Box::new(sample_spec()),
        };
        let text = serde_json::to_string(&message).expect("serialize");
        let back: DaemonToFleetd = serde_json::from_str(&text).expect("deserialize");
        let DaemonToFleetd::Spawn { spec } = back else {
            panic!("expected spawn");
        };
        assert_eq!(*spec, sample_spec());
    }

    /// The bearer token must reach fleetd verbatim on the wire and must never
    /// reach a log line through the enum's derived `Debug`.
    #[test]
    fn bearer_token_serializes_plainly_and_debugs_redacted() {
        let message = DaemonToFleetd::Authenticate {
            token: BearerToken::new("f".repeat(64)),
        };
        let value = serde_json::to_value(&message).expect("serialize");
        assert_eq!(
            value["token"],
            json!("f".repeat(64)),
            "the real secret must cross the wire"
        );

        let debug = format!("{message:?}");
        assert!(debug.contains("REDACTED"), "{debug}");
        assert!(
            !debug.contains(&"f".repeat(64)),
            "the secret must never appear in Debug: {debug}"
        );
        assert_eq!(
            format!("{}", BearerToken::new("f".repeat(64))),
            "REDACTED",
            "Display must not leak the secret either"
        );
    }

    /// Optional fields stay off the wire when absent, so an `Event` frame for
    /// a pre-seq harness build is a two-key object plus its tag.
    #[test]
    fn absent_optionals_are_omitted() {
        let value = serde_json::to_value(FleetdToDaemon::Event {
            session_id: "sess-1".to_string(),
            seq: None,
            line: "{}".to_string(),
        })
        .expect("serialize");
        let object = value.as_object().expect("object");
        assert!(!object.contains_key("seq"), "absent seq must be omitted");
        assert_eq!(object.len(), 3, "type + session_id + line only: {value}");
    }
}
