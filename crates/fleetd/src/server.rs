//! The UDS server: accept, authenticate, fence, dispatch.
//!
//! ## Single owner, generation-fenced
//!
//! fleetd serves exactly ONE daemon connection at a time. Each accepted
//! connection is allocated a monotonically increasing `connection_generation`
//! and, once authenticated, becomes the owner: the previous owner is fenced
//! out. Two mechanisms enforce that, and both are load-bearing:
//!
//! 1. `bro_rpc`'s envelope validation rejects any frame whose generation is
//!    not the one its own handshake negotiated, so a stale connection cannot
//!    smuggle a command through even if it is still physically open.
//! 2. fleetd explicitly notifies the superseded connection's fence, so it
//!    stops reading and drops the socket instead of lingering.
//!
//! This is what lets a restarted daemon reclaim fleetd without a separate
//! liveness protocol: dial, authenticate, and the old connection is gone.
//!
//! ## Disconnect is not session death
//!
//! Losing the owner connection does NOT kill children. Relaying pauses, the
//! registry stays, and the durable per-session event log keeps accumulating.
//! The next daemon asks `ListSessions`, then issues a `ReplayFrom` per session
//! against its own cursor, and picks up exactly where it left off.
//!
//! ## Why the connection is split
//!
//! A connection needs to read commands and write relayed events concurrently.
//! `NegotiatedIo` owns the whole stream and its `read_envelope` is not
//! cancel-safe (cancelling mid-frame would desynchronize the length-prefixed
//! stream), so `select!`-ing over it is not an option. The handshake runs on
//! the whole `UnixStream`, then `NegotiatedIo::split` hands back a read half
//! and a write half that each still carry the negotiated `ConnectionBinding`,
//! so generation fencing applies to every frame on both halves exactly as it
//! did before the split.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bro_protocol::{
    DaemonToFleetd, FLEETD_PROTOCOL_VERSION, FleetdToDaemon, SessionState, WORKSPACE_BINDING_ENV,
    WorkerSpawnSpec, WorkspaceBindingToken,
};
use bro_rpc::{
    BuildIdentity, ConnectionBinding, Envelope, HandshakeOptions, NegotiatedIo, RpcError,
    ServiceToken, verify_peer_uid,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc};

use crate::registry::{Registry, SessionEntry};
use crate::replay::{ReplayDecision, ReplayStream, plan_replay};
use crate::spawn::{WorkerChild, event_seq, spawn_worker};

/// Build identity this fleetd advertises in the handshake.
pub fn build_identity() -> BuildIdentity {
    BuildIdentity {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_id: env!("FLEETD_BUILD_ID").to_string(),
    }
}

/// The owner connection's outbound lane and fence.
struct Owner {
    generation: u64,
    outbound: mpsc::UnboundedSender<FleetdToDaemon>,
    fence: Arc<Notify>,
}

/// Shared fleetd state: the session registry plus whichever daemon connection
/// currently owns it.
pub struct Fleetd {
    registry: Arc<Registry>,
    token: ServiceToken,
    build: BuildIdentity,
    generation: AtomicU64,
    owner: Mutex<Option<Owner>>,
}

impl Fleetd {
    pub fn new(token: ServiceToken, build: BuildIdentity) -> Arc<Self> {
        Arc::new(Self {
            registry: Arc::new(Registry::new()),
            token,
            build,
            generation: AtomicU64::new(0),
            owner: Mutex::new(None),
        })
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Allocate the next connection generation. Monotonic and never reused,
    /// which is what makes stale-generation rejection meaningful.
    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The highest generation allocated so far.
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Install a newly authenticated connection as the owner, fencing out
    /// whichever connection held that role before.
    fn install_owner(
        &self,
        generation: u64,
        outbound: mpsc::UnboundedSender<FleetdToDaemon>,
        fence: Arc<Notify>,
    ) {
        let previous = self
            .owner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replace(Owner {
                generation,
                outbound,
                fence,
            });
        if let Some(previous) = previous {
            tracing::info!(
                superseded_generation = previous.generation,
                new_generation = generation,
                "fencing superseded daemon connection"
            );
            previous.fence.notify_waiters();
        }
    }

    /// Release ownership, but only if `generation` still holds it. A late
    /// teardown from an already-superseded connection must not evict the new
    /// owner.
    fn release_owner(&self, generation: u64) {
        let mut owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
        if owner.as_ref().is_some_and(|o| o.generation == generation) {
            *owner = None;
        }
    }

    /// Whether an owner connection is currently attached.
    pub fn has_owner(&self) -> bool {
        self.owner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// Send a message to the current owner. With no owner attached the message
    /// is DROPPED, deliberately: relaying pauses across a daemon disconnect,
    /// and the durable event log is the backlog the next daemon replays from.
    pub fn emit(&self, message: FleetdToDaemon) {
        let owner = self.owner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(owner) = owner.as_ref() {
            let _ = owner.outbound.send(message);
        }
    }
}

/// Bind the fleetd socket, clearing a stale socket file but refusing to steal
/// a live one.
///
/// The distinction matters: a leftover socket file from a killed fleetd must
/// not block startup forever, but unlinking a socket another fleetd is
/// actively serving would silently split the machine's supervision.
/// A successful connect proves someone is listening; `ECONNREFUSED` proves
/// nobody is.
pub async fn bind_listener(socket_path: &Path) -> anyhow::Result<UnixListener> {
    if tokio::fs::symlink_metadata(socket_path).await.is_ok() {
        match UnixStream::connect(socket_path).await {
            Ok(_) => anyhow::bail!(
                "another fleetd is already listening on {}",
                socket_path.display()
            ),
            Err(_) => {
                tracing::warn!(socket = %socket_path.display(), "removing stale socket file");
                tokio::fs::remove_file(socket_path).await?;
            }
        }
    }
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let listener = UnixListener::bind(socket_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(listener)
}

/// Accept connections forever, serving each on its own task.
pub async fn serve(state: Arc<Fleetd>, listener: UnixListener) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _addr)) => stream,
            Err(error) => {
                tracing::warn!(%error, "accept failed");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_connection(state, stream).await {
                tracing::warn!(%error, "daemon connection ended with an error");
            }
        });
    }
}

/// Handshake, authenticate, then run one owner connection to completion.
pub async fn serve_connection(state: Arc<Fleetd>, stream: UnixStream) -> anyhow::Result<()> {
    // Peer-uid verification is independent of the token, not a substitute for
    // it: it proves the peer runs as our uid, never which service it is.
    verify_peer_uid(&stream)?;

    let generation = state.next_generation();
    let (negotiated, _hello, _welcome) = bro_rpc::accept(
        stream,
        state.build.clone(),
        vec![FLEETD_PROTOCOL_VERSION],
        generation,
        HandshakeOptions::default(),
    )
    .await?;
    let binding = negotiated.binding();

    // See the module note on why the connection is split.
    let (mut reader, mut writer) = negotiated.split();
    let counter = Arc::new(AtomicU64::new(0));

    // Auth gate: the FIRST envelope must be a valid Authenticate. Anything
    // else, including a well-formed Spawn, is refused and the connection is
    // dropped.
    let first = read_body(&mut reader).await?;
    let DaemonToFleetd::Authenticate { token } = first else {
        write_body(
            &mut writer,
            binding,
            &counter,
            generation,
            &FleetdToDaemon::Error {
                session_id: None,
                code: "auth.required".to_string(),
                message: "first message must be authenticate".to_string(),
            },
        )
        .await?;
        anyhow::bail!("daemon connection sent a non-authenticate first message");
    };
    if !state.token.verify(token.expose_secret()) {
        write_body(
            &mut writer,
            binding,
            &counter,
            generation,
            &FleetdToDaemon::Error {
                session_id: None,
                code: "auth.invalid".to_string(),
                message: "bearer token rejected".to_string(),
            },
        )
        .await?;
        anyhow::bail!("daemon connection presented an invalid bearer token");
    }

    // Authenticated: become the owner and fence out the previous connection.
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<FleetdToDaemon>();
    let fence = Arc::new(Notify::new());
    state.install_owner(generation, outbound_tx, fence.clone());
    tracing::info!(generation, "daemon connection authenticated and installed");

    let writer_counter = counter.clone();
    let writer_task = tokio::spawn(async move {
        // Ready first, so the daemon knows which generation it holds before
        // any relayed event arrives.
        let ready = FleetdToDaemon::Ready {
            connection_generation: generation,
        };
        if write_body(&mut writer, binding, &writer_counter, generation, &ready)
            .await
            .is_err()
        {
            return;
        }
        while let Some(message) = outbound_rx.recv().await {
            if write_body(&mut writer, binding, &writer_counter, generation, &message)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Reader loop: cancelled either by the fence (a newer connection took
    // over) or by the peer going away.
    loop {
        let body = tokio::select! {
            _ = fence.notified() => {
                tracing::info!(generation, "connection fenced by a newer daemon connection");
                break;
            }
            body = read_body(&mut reader) => body,
        };
        match body {
            Ok(body) => dispatch(state.clone(), body),
            Err(error) => {
                if !matches!(error, RpcError::Io(_)) {
                    tracing::warn!(generation, %error, "rejecting frame from daemon connection");
                }
                break;
            }
        }
    }

    state.release_owner(generation);
    writer_task.abort();
    Ok(())
}

/// Route one command. Long-running work (spawning a child, streaming a replay)
/// runs on its own task so the reader loop keeps servicing control traffic.
fn dispatch(state: Arc<Fleetd>, body: DaemonToFleetd) {
    match body {
        DaemonToFleetd::Authenticate { .. } => {
            // Re-authentication on an established connection is meaningless;
            // ignoring it is friendlier than tearing the connection down.
            tracing::debug!("ignoring redundant authenticate");
        }
        DaemonToFleetd::Spawn { spec } => {
            tokio::spawn(handle_spawn(state, *spec));
        }
        DaemonToFleetd::Control {
            session_id,
            message,
        } => {
            if let Some(control) = state.registry().control_sender(&session_id) {
                let _ = control.send(message);
            } else {
                state.emit(unknown_session(&session_id));
            }
        }
        DaemonToFleetd::Kill { session_id } => {
            // Idempotent by construction: WorkerKill fires SIGTERM at most
            // once, and an unknown session is a no-op rather than an error
            // (the daemon may be killing a session it already saw exit).
            if let Some(killer) = state.registry().killer(&session_id) {
                killer.kill();
            }
        }
        DaemonToFleetd::ListSessions => {
            state.emit(FleetdToDaemon::Sessions {
                sessions: state.registry().summaries(),
            });
        }
        DaemonToFleetd::ReplayFrom {
            session_id,
            from_seq,
        } => {
            tokio::spawn(handle_replay(state, session_id, from_seq));
        }
        DaemonToFleetd::EventAck {
            session_id,
            through_seq,
        } => {
            state.registry().note_ack(&session_id, through_seq);
        }
        DaemonToFleetd::Unknown => {
            // A newer daemon sent a variant this build does not know. Skip it;
            // dropping the connection would turn a rolling upgrade into an
            // outage.
            tracing::debug!("ignoring unknown daemon message variant");
        }
    }
}

async fn handle_spawn(state: Arc<Fleetd>, spec: WorkerSpawnSpec) {
    let session_id = spec.session_id.clone();
    let task_id = spec.task_id.clone();
    let workspace_id = spec.workspace_id.clone();
    let workspace_binding_token = match spec.env.as_map().get(WORKSPACE_BINDING_ENV) {
        Some(token) => match WorkspaceBindingToken::parse(token.clone()) {
            Ok(token) => Some(token),
            Err(error) => {
                state.emit(FleetdToDaemon::Error {
                    session_id: Some(session_id),
                    code: "workspace_binding.invalid".to_string(),
                    message: error.to_string(),
                });
                return;
            }
        },
        None => None,
    };
    if workspace_binding_token.is_some() && workspace_id.is_none() {
        state.emit(FleetdToDaemon::Error {
            session_id: Some(session_id),
            code: "workspace_binding.unbound".to_string(),
            message: "workspace binding token requires a workspace id".to_string(),
        });
        return;
    }
    let event_log_path = spec.event_log_path.clone();

    if state.registry().contains(&session_id) {
        state.emit(FleetdToDaemon::Error {
            session_id: Some(session_id),
            code: "session.duplicate".to_string(),
            message: "a session with this id is already registered".to_string(),
        });
        return;
    }

    let child = match spawn_worker(spec).await {
        Ok(child) => child,
        Err(error) => {
            state.emit(FleetdToDaemon::Error {
                session_id: Some(session_id),
                code: "spawn.failed".to_string(),
                message: error.to_string(),
            });
            return;
        }
    };

    let WorkerChild {
        control,
        mut events,
        pid,
        killer,
        outcome,
    } = child;

    state.registry().insert(SessionEntry {
        session_id: session_id.clone(),
        task_id: task_id.clone(),
        workspace_id: workspace_id.clone(),
        workspace_binding_token,
        pid,
        state: SessionState::Running,
        last_seq: None,
        acked_seq: None,
        event_log_path,
        exit_code: None,
        control,
        killer,
    });
    state.emit(FleetdToDaemon::SessionStarted {
        session_id: session_id.clone(),
        task_id,
        workspace_id,
        pid,
    });

    // Relay task: drain stdout fully, THEN await the outcome. The outcome
    // channel only resolves after the child exited and both pumps drained, so
    // this ordering guarantees every event precedes SessionExited.
    tokio::spawn(async move {
        while let Some(line) = events.recv().await {
            let seq = event_seq(&line);
            if let Some(seq) = seq {
                state.registry().note_seq(&session_id, seq);
            }
            state.emit(FleetdToDaemon::Event {
                session_id: session_id.clone(),
                seq,
                line,
            });
        }
        let (exit_code, stderr_tail) = match outcome.await {
            Ok(outcome) => (outcome.exit_code, outcome.stderr_tail),
            Err(_) => (None, String::new()),
        };
        state.registry().mark_exited(&session_id, exit_code);
        state.emit(FleetdToDaemon::SessionExited {
            session_id,
            exit_code,
            stderr_tail,
        });
    });
}

async fn handle_replay(state: Arc<Fleetd>, session_id: String, from_seq: u64) {
    let Some(path) = state.registry().event_log_path(&session_id) else {
        state.emit(unknown_session(&session_id));
        return;
    };

    let latest_available = match plan_replay(&path, from_seq).await {
        Ok(ReplayDecision::Stream { latest_available }) => latest_available,
        Ok(ReplayDecision::Unavailable {
            earliest_available,
            latest_available,
        }) => {
            state.emit(FleetdToDaemon::ReplayUnavailable {
                session_id,
                requested_from: from_seq,
                earliest_available,
                latest_available,
            });
            return;
        }
        Err(error) => {
            state.emit(FleetdToDaemon::Error {
                session_id: Some(session_id),
                code: "replay.failed".to_string(),
                message: error.to_string(),
            });
            return;
        }
    };

    match ReplayStream::open(&path, from_seq).await {
        Ok(Some(mut stream)) => loop {
            match stream.next_chunk().await {
                Ok(Some(chunk)) => {
                    for event in chunk {
                        state.emit(FleetdToDaemon::Event {
                            session_id: session_id.clone(),
                            seq: Some(event.seq),
                            line: event.line,
                        });
                    }
                    // Yield between chunks so a very long replay cannot
                    // starve live control traffic on this connection.
                    tokio::task::yield_now().await;
                }
                Ok(None) => break,
                Err(error) => {
                    state.emit(FleetdToDaemon::Error {
                        session_id: Some(session_id),
                        code: "replay.failed".to_string(),
                        message: error.to_string(),
                    });
                    return;
                }
            }
        },
        Ok(None) => {}
        Err(error) => {
            state.emit(FleetdToDaemon::Error {
                session_id: Some(session_id),
                code: "replay.failed".to_string(),
                message: error.to_string(),
            });
            return;
        }
    }

    state.emit(FleetdToDaemon::ReplayComplete {
        session_id,
        through_seq: latest_available,
    });
}

fn unknown_session(session_id: &str) -> FleetdToDaemon {
    FleetdToDaemon::Error {
        session_id: Some(session_id.to_string()),
        code: "session.unknown".to_string(),
        message: "no such session".to_string(),
    }
}

/// Read one generation-validated command. `NegotiatedIo` validates against its
/// own binding, so the fencing check is not repeated here.
async fn read_body<R>(reader: &mut NegotiatedIo<R>) -> Result<DaemonToFleetd, RpcError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    Ok(reader.read_envelope::<DaemonToFleetd>().await?.body)
}

/// Write one generation-stamped message.
async fn write_body<W>(
    writer: &mut NegotiatedIo<W>,
    binding: ConnectionBinding,
    counter: &AtomicU64,
    generation: u64,
    body: &FleetdToDaemon,
) -> Result<(), RpcError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let sequence = counter.fetch_add(1, Ordering::SeqCst);
    let envelope = Envelope {
        protocol_version: binding.protocol_version,
        connection_generation: binding.connection_generation,
        message_id: format!("fleetd-{generation}-{sequence}"),
        reply_to: None,
        body,
    };
    writer.write_envelope(&envelope).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> Arc<Fleetd> {
        Fleetd::new(
            ServiceToken::parse("a".repeat(64)).unwrap(),
            build_identity(),
        )
    }

    #[test]
    fn generations_are_monotonic_and_never_reused() {
        let state = test_state();
        let first = state.next_generation();
        let second = state.next_generation();
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(state.current_generation(), 2);
    }

    #[tokio::test]
    async fn installing_an_owner_fences_the_previous_one() {
        let state = test_state();
        let (first_tx, mut first_rx) = mpsc::unbounded_channel();
        let first_fence = Arc::new(Notify::new());
        state.install_owner(1, first_tx, first_fence.clone());

        let fenced = {
            let fence = first_fence.clone();
            tokio::spawn(async move { fence.notified().await })
        };
        // Give the waiter a chance to register before the fence fires.
        tokio::task::yield_now().await;

        let (second_tx, mut second_rx) = mpsc::unbounded_channel();
        state.install_owner(2, second_tx, Arc::new(Notify::new()));
        tokio::time::timeout(std::time::Duration::from_secs(2), fenced)
            .await
            .expect("superseded connection must be fenced")
            .unwrap();

        // Emissions now go to the new owner only.
        state.emit(FleetdToDaemon::Ready {
            connection_generation: 2,
        });
        assert!(second_rx.recv().await.is_some());
        assert!(first_rx.try_recv().is_err());
    }

    /// A superseded connection tearing down late must not evict the owner
    /// that replaced it.
    #[test]
    fn release_by_a_stale_generation_is_a_no_op() {
        let state = test_state();
        state.install_owner(1, mpsc::unbounded_channel().0, Arc::new(Notify::new()));
        state.install_owner(2, mpsc::unbounded_channel().0, Arc::new(Notify::new()));
        state.release_owner(1);
        assert!(
            state.has_owner(),
            "stale release must not evict generation 2"
        );
        state.release_owner(2);
        assert!(!state.has_owner());
    }

    /// With no daemon attached, relaying is a silent drop rather than an
    /// error or a queue that grows without bound. The event log is the
    /// backlog.
    #[test]
    fn emitting_without_an_owner_drops_the_message() {
        let state = test_state();
        assert!(!state.has_owner());
        state.emit(FleetdToDaemon::Ready {
            connection_generation: 1,
        });
    }

    #[tokio::test]
    async fn bind_refuses_to_steal_a_live_socket_and_clears_a_stale_one() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let socket = root.join("fleetd.sock");

        let listener = bind_listener(&socket).await.unwrap();
        assert!(
            bind_listener(&socket).await.is_err(),
            "a live socket must never be stolen"
        );

        drop(listener);
        // The file survives the listener drop; the next bind sees a socket
        // nobody is serving and clears it.
        assert!(tokio::fs::symlink_metadata(&socket).await.is_ok());
        let _relisten = bind_listener(&socket)
            .await
            .expect("stale socket is cleared");
    }
}
