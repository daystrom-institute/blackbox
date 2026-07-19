//! The daemon-side client for `fleetd`, the per-machine fleet supervisor.
//!
//! Slice 5 of `design/daemon-runtime/locality-first-decomposition.md`.
//! [`FleetdExecutor`] implements the same [`HarnessExecutor`] seam
//! [`super::executor::LocalExecutor`] does, so the daemon's dispatch
//! composition is byte-identical either way: compose a fully-resolved
//! [`WorkerSpawnSpec`], hand it to an executor, get back a [`WorkerHandle`].
//! The difference is only who is the child's parent, and therefore who
//! survives a `blackboxd` restart.
//!
//! ## One connection, many sessions
//!
//! fleetd serves exactly one owner connection at a time (its `AGENTS.md`,
//! "single owner connection, generation-fenced"), so the daemon keeps ONE
//! connection and multiplexes every session over it. A connection actor owns
//! the socket: a writer task drains a command queue onto the write half, a
//! reader task pulls `FleetdToDaemon` off the read half and fans each message
//! out to the right per-session handle. `bro_rpc::NegotiatedIo::split` gives us
//! the two halves with the generation fence intact on both, because
//! `read_envelope` is not cancel-safe and cannot be `select!`ed against a
//! concurrent write.
//!
//! ## Absence is visible, never masked
//!
//! There is no silent downgrade to `LocalExecutor`. If the socket is missing we
//! start fleetd (detached, so it outlives the daemon that started it) and wait
//! a bounded time for it to listen; if it still is not there, the dispatch
//! fails loudly. A daemon that quietly kept spawning its own children would
//! reintroduce exactly the restart-drops-sessions problem this slice exists to
//! remove, and would do it invisibly.
//!
//! ## Re-adoption
//!
//! Losing the connection is not session death on either side. After every
//! successful connect (first dial or reconnect) the client asks
//! `ListSessions` and hands each row fleetd still holds back to
//! [`super::readopt_harness_session`], which reattaches the live task to fresh
//! ingest/terminal plumbing and returns the task's durable ingest cursor. The
//! client then issues `ReplayFrom` against that cursor, so the daemon sees
//! every event it had not durably ingested and no event twice. Sessions fleetd
//! reports that the task store does not know are logged loudly and left
//! running: killing an orphan the daemon merely forgot would destroy work, and
//! fleetd GCs terminal sessions on its own once acked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use bro_protocol::{
    BearerToken, DaemonToFleetd, FLEETD_PROTOCOL_VERSION, FleetdToDaemon, SessionState,
    SessionSummary, WorkerSpawnSpec,
};
use bro_rpc::{BuildIdentity, Envelope, HandshakeOptions, NegotiatedIo, ServiceToken};
use tokio::net::UnixStream;

use super::executor::{HarnessExecutor, WorkerHandle, WorkerKill, WorkerOutcome};

/// Socket file name inside the state dir. Must match `fleetd::paths`, which
/// the daemon deliberately does not link (fleetd's dependency ceiling runs the
/// other way, but keeping the daemon off fleetd's internals keeps the seam a
/// wire contract rather than a shared-code contract).
const SOCKET_FILE: &str = "fleetd.sock";
/// Shared-secret token file name inside the state dir.
const TOKEN_FILE: &str = "fleetd.token";

/// How long to wait for a freshly started fleetd to bind its socket.
const AUTOSTART_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval while waiting for that socket.
const AUTOSTART_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long to wait for `SessionStarted` after sending a `Spawn`. Generous:
/// fleetd resolves the binary through a login shell, which on a cold macOS
/// host can take a noticeable fraction of a second.
const SPAWN_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Where fleetd's socket and token live, plus how to start it if it is not
/// running.
#[derive(Debug, Clone)]
pub struct FleetdConfig {
    pub socket: PathBuf,
    pub token: PathBuf,
    /// State dir handed to a fleetd we start ourselves, so it derives the same
    /// socket/token paths we are dialing.
    pub state_dir: PathBuf,
    /// Explicit fleetd binary. `None` means resolve at start time.
    pub binary: Option<PathBuf>,
}

impl FleetdConfig {
    pub fn in_state_dir(state_dir: impl AsRef<Path>) -> Self {
        let state_dir = state_dir.as_ref().to_path_buf();
        Self {
            socket: state_dir.join(SOCKET_FILE),
            token: state_dir.join(TOKEN_FILE),
            state_dir,
            binary: std::env::var("BLACKBOX_FLEETD_BIN")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from),
        }
    }
}

/// Build identity this daemon advertises to fleetd. `build_id` is the root
/// `build.rs` git stamp; `builds_compatible` only compares versions, but the id
/// makes "which two binaries actually talked" answerable from a log line.
pub fn build_identity() -> BuildIdentity {
    BuildIdentity {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_id: env!("BLACKBOX_BUILD_ID").to_string(),
    }
}

/// Per-session plumbing the connection actor fans messages into.
struct SessionSlot {
    /// Raw harness stdout lines, consumed by the daemon's ingest loop.
    events: mpsc::UnboundedSender<String>,
    /// Fired once on `SessionStarted` (spawn path only; a re-adopted session
    /// has already started).
    started: Option<oneshot::Sender<Result<Option<u32>, String>>>,
    /// Fired once on `SessionExited`, or on `ReplayComplete` for a session
    /// that had already exited before we re-adopted it.
    outcome: Option<oneshot::Sender<WorkerOutcome>>,
    /// Set when we re-adopt a session fleetd reports as already `Exited`:
    /// fleetd will not re-send `SessionExited` for it, so the terminal outcome
    /// is published once its replay stream terminates. `None` for a live
    /// session, whose terminal state arrives on the wire.
    exit_after_replay: Option<Option<i32>>,
    /// Highest seq seen on this session's relayed events, for `EventAck`.
    last_seq: u64,
    /// Command lane, so a terminal session can be acked without reaching back
    /// through the connection.
    commands: mpsc::UnboundedSender<DaemonToFleetd>,
}

impl SessionSlot {
    /// Close the event lane, publish the outcome, and ack fleetd through the
    /// last seq we relayed.
    ///
    /// Dropping the event sender FIRST matters: the daemon's ingest loop ends
    /// at EOF and the terminal waiter joins it before publishing, which is the
    /// same ordering `LocalExecutor` gets from its stdout pump closing ahead of
    /// its waiter. Publishing first would let a fast exit race the ingest of
    /// events already in the queue.
    ///
    /// The ack is sent here rather than after the daemon's terminal waiter
    /// finishes. It is advisory and gates only fleetd's GC of an
    /// already-terminal session, never live relay or replay, and the daemon's
    /// own durable cursor (not this ack) is what a later `ReplayFrom` uses.
    fn finish(mut self, session_id: &str, exit_code: Option<i32>, stderr: String) {
        if let Some(outcome) = self.outcome.take() {
            let events = self.events;
            drop(events);
            let _ = outcome.send(WorkerOutcome { exit_code, stderr });
        }
        let _ = self.commands.send(DaemonToFleetd::EventAck {
            session_id: session_id.to_string(),
            through_seq: self.last_seq,
        });
    }
}

/// The live connection: a command queue feeding the writer task, plus the
/// generation fleetd allocated for it.
struct Connection {
    generation: u64,
    commands: mpsc::UnboundedSender<DaemonToFleetd>,
}

impl Connection {
    fn is_alive(&self) -> bool {
        !self.commands.is_closed()
    }
}

/// Shared client state. Sessions outlive any single connection, which is what
/// makes reconnect-and-replay work rather than reconnect-and-lose.
struct Shared {
    config: FleetdConfig,
    sessions: Mutex<HashMap<String, SessionSlot>>,
    /// Waiters for the next `Sessions` answer, in FIFO order.
    list_waiters: Mutex<Vec<oneshot::Sender<Vec<SessionSummary>>>>,
    message_counter: AtomicU64,
}

impl Shared {
    fn next_message_id(&self) -> String {
        let seq = self.message_counter.fetch_add(1, Ordering::Relaxed);
        format!("bbox-{seq}")
    }
}

/// Executes workers as children of fleetd, over its Unix domain socket.
pub struct FleetdExecutor {
    shared: Arc<Shared>,
    /// Held across a dial so two concurrent dispatches cannot race two
    /// connections into existence (the second would fence the first out).
    connection: tokio::sync::Mutex<Option<Connection>>,
}

impl FleetdExecutor {
    pub fn new(config: FleetdConfig) -> Self {
        Self {
            shared: Arc::new(Shared {
                config,
                sessions: Mutex::new(HashMap::new()),
                list_waiters: Mutex::new(Vec::new()),
                message_counter: AtomicU64::new(0),
            }),
            connection: tokio::sync::Mutex::new(None),
        }
    }

    /// Return a live command sender, dialing (and if necessary starting)
    /// fleetd first. Every successful dial runs re-adoption before returning,
    /// so a caller that gets a sender is talking to a fleetd whose live
    /// sessions are already reattached.
    async fn commands(&self) -> anyhow::Result<mpsc::UnboundedSender<DaemonToFleetd>> {
        let mut guard = self.connection.lock().await;
        if let Some(connection) = guard.as_ref()
            && connection.is_alive()
        {
            return Ok(connection.commands.clone());
        }
        let connection = self.dial().await?;
        let commands = connection.commands.clone();
        let generation = connection.generation;
        *guard = Some(connection);
        drop(guard);

        // Re-adoption runs outside the connection lock: it awaits a
        // ListSessions round trip, and holding the lock across that would
        // serialize every concurrent dispatch behind it for no reason.
        if let Err(error) = readopt_live_sessions(&self.shared, &commands).await {
            tracing::warn!(
                %error,
                generation,
                "fleetd re-adoption failed; live sessions may not be reattached"
            );
        }
        Ok(commands)
    }

    /// Connect, authenticate, and start the connection actor.
    async fn dial(&self) -> anyhow::Result<Connection> {
        let socket = self.shared.config.socket.clone();
        if UnixStream::connect(&socket).await.is_err() {
            start_fleetd(&self.shared.config).await?;
        }
        let stream = UnixStream::connect(&socket).await.map_err(|error| {
            anyhow::anyhow!(
                "cannot reach fleetd at {}: {error}. Start it (launchd label \
                 com.daystrom.fleetd, or `fleetd --state-dir {}`), or set \
                 BLACKBOX_EXECUTOR=local to run workers as daemon children.",
                socket.display(),
                self.shared.config.state_dir.display()
            )
        })?;

        // The token file is created by whichever of daemon and fleetd starts
        // first; `load_or_create` keeps a daemon-first boot working.
        let token = ServiceToken::load_or_create(&self.shared.config.token).map_err(|error| {
            anyhow::anyhow!(
                "cannot load the fleetd token at {}: {error}",
                self.shared.config.token.display()
            )
        })?;

        let (io, welcome) = bro_rpc::connect(
            stream,
            build_identity(),
            vec![FLEETD_PROTOCOL_VERSION],
            HandshakeOptions::default(),
        )
        .await
        .map_err(|error| anyhow::anyhow!("fleetd handshake failed: {error}"))?;
        let generation = welcome.connection_generation;

        // Authenticate BEFORE splitting: the gate is strictly request/response
        // and fleetd refuses everything else until it passes, so there is no
        // concurrency to serve yet.
        let mut io = io;
        let authenticate = Envelope {
            protocol_version: io.binding().protocol_version,
            connection_generation: generation,
            message_id: self.shared.next_message_id(),
            reply_to: None,
            body: DaemonToFleetd::Authenticate {
                token: BearerToken::new(token.expose_secret().to_string()),
            },
        };
        io.write_envelope(&authenticate)
            .await
            .map_err(|error| anyhow::anyhow!("fleetd authenticate write failed: {error}"))?;
        match io.read_envelope::<FleetdToDaemon>().await {
            Ok(envelope) => match envelope.body {
                FleetdToDaemon::Ready {
                    connection_generation,
                } => {
                    if connection_generation != generation {
                        anyhow::bail!(
                            "fleetd readied generation {connection_generation} but the \
                             handshake negotiated {generation}"
                        );
                    }
                }
                FleetdToDaemon::Error { code, message, .. } => {
                    anyhow::bail!("fleetd refused authentication: {code}: {message}")
                }
                other => anyhow::bail!("unexpected first fleetd message: {other:?}"),
            },
            Err(error) => anyhow::bail!("fleetd authenticate read failed: {error}"),
        }

        let (reader, writer) = io.split();
        let (commands_tx, commands_rx) = mpsc::unbounded_channel::<DaemonToFleetd>();
        spawn_writer(self.shared.clone(), writer, commands_rx, generation);
        spawn_reader(self.shared.clone(), reader);

        tracing::info!(
            socket = %socket.display(),
            generation,
            fleetd_version = %welcome.build.version,
            fleetd_build = %welcome.build.build_id,
            "connected to fleetd"
        );
        Ok(Connection {
            generation,
            commands: commands_tx,
        })
    }
}

#[async_trait]
impl HarnessExecutor for FleetdExecutor {
    async fn spawn(&self, spec: WorkerSpawnSpec) -> anyhow::Result<WorkerHandle> {
        let commands = self.commands().await?;
        let session_id = spec.session_id.clone();

        let (events_tx, events_rx) = mpsc::unbounded_channel::<String>();
        let (started_tx, started_rx) = oneshot::channel();
        let (outcome_tx, outcome_rx) = oneshot::channel();
        {
            let mut sessions = self.shared.sessions.lock();
            if sessions.contains_key(&session_id) {
                anyhow::bail!(
                    "fleetd session id `{session_id}` is already live; refusing to \
                     multiplex two dispatches onto one supervision key"
                );
            }
            sessions.insert(
                session_id.clone(),
                SessionSlot {
                    events: events_tx,
                    started: Some(started_tx),
                    outcome: Some(outcome_tx),
                    exit_after_replay: None,
                    last_seq: 0,
                    commands: commands.clone(),
                },
            );
        }

        if commands
            .send(DaemonToFleetd::Spawn {
                spec: Box::new(spec),
            })
            .is_err()
        {
            self.shared.sessions.lock().remove(&session_id);
            anyhow::bail!("fleetd connection dropped before the spawn was sent");
        }

        let pid = match tokio::time::timeout(SPAWN_ACK_TIMEOUT, started_rx).await {
            Ok(Ok(Ok(pid))) => pid,
            Ok(Ok(Err(message))) => {
                self.shared.sessions.lock().remove(&session_id);
                anyhow::bail!("fleetd refused the spawn: {message}");
            }
            Ok(Err(_)) => {
                self.shared.sessions.lock().remove(&session_id);
                anyhow::bail!("fleetd connection dropped before the session started");
            }
            Err(_) => {
                self.shared.sessions.lock().remove(&session_id);
                anyhow::bail!(
                    "fleetd did not acknowledge the spawn within {}s",
                    SPAWN_ACK_TIMEOUT.as_secs()
                );
            }
        };

        Ok(WorkerHandle {
            control: control_lane(session_id.clone(), commands.clone()),
            events: events_rx,
            pid,
            killer: WorkerKill::via_fleetd(session_id, commands),
            outcome: outcome_rx,
        })
    }
}

/// Adapt the daemon's `Value`-shaped control lane onto fleetd `Control`
/// messages. The daemon-side registry stores an
/// `UnboundedSender<Value>` regardless of executor, so the translation is a
/// relay task rather than a change to every `bro_steer` caller.
fn control_lane(
    session_id: String,
    commands: mpsc::UnboundedSender<DaemonToFleetd>,
) -> mpsc::UnboundedSender<Value> {
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        while let Some(message) = control_rx.recv().await {
            if commands
                .send(DaemonToFleetd::Control {
                    session_id: session_id.clone(),
                    message,
                })
                .is_err()
            {
                tracing::debug!(%session_id, "fleetd control lane closed");
                break;
            }
        }
    });
    control_tx
}

/// Writer task: wrap each queued command in a fenced envelope and put it on the
/// wire. Exits when the queue closes or the socket errors, which drops the
/// command sender and makes `Connection::is_alive` false so the next dispatch
/// redials.
fn spawn_writer(
    shared: Arc<Shared>,
    mut writer: NegotiatedIo<tokio::io::WriteHalf<UnixStream>>,
    mut commands: mpsc::UnboundedReceiver<DaemonToFleetd>,
    generation: u64,
) {
    tokio::spawn(async move {
        while let Some(body) = commands.recv().await {
            let envelope = Envelope {
                protocol_version: FLEETD_PROTOCOL_VERSION,
                connection_generation: generation,
                message_id: shared.next_message_id(),
                reply_to: None,
                body,
            };
            if let Err(error) = writer.write_envelope(&envelope).await {
                tracing::warn!(%error, generation, "fleetd write failed; dropping connection");
                break;
            }
        }
    });
}

/// Reader task: fan every `FleetdToDaemon` out to the session it names.
fn spawn_reader(shared: Arc<Shared>, mut reader: NegotiatedIo<tokio::io::ReadHalf<UnixStream>>) {
    tokio::spawn(async move {
        loop {
            let envelope = match reader.read_envelope::<FleetdToDaemon>().await {
                Ok(envelope) => envelope,
                Err(error) => {
                    tracing::info!(%error, "fleetd connection closed");
                    break;
                }
            };
            handle_message(&shared, envelope.body);
        }
        // The socket is gone. Live sessions keep their slots: fleetd kept the
        // children running, and the next dial re-adopts and replays them.
        // Pending spawn acks cannot be satisfied, so their waiters are dropped
        // (the `Err` arm in `spawn` turns that into a loud dispatch failure).
        let mut sessions = shared.sessions.lock();
        for slot in sessions.values_mut() {
            slot.started.take();
        }
        shared.list_waiters.lock().clear();
    });
}

fn handle_message(shared: &Arc<Shared>, message: FleetdToDaemon) {
    match message {
        FleetdToDaemon::SessionStarted {
            session_id, pid, ..
        } => {
            let mut sessions = shared.sessions.lock();
            if let Some(slot) = sessions.get_mut(&session_id)
                && let Some(started) = slot.started.take()
            {
                let _ = started.send(Ok(pid));
            }
        }
        FleetdToDaemon::Event {
            session_id,
            seq,
            line,
        } => {
            // The daemon's ingest loop parses `seq` off the line itself and
            // advances the DURABLE cursor from there, so the same rule holds
            // for both executors and an event without a seq simply does not
            // advance it. The seq tracked here is only for `EventAck`.
            let mut sessions = shared.sessions.lock();
            if let Some(slot) = sessions.get_mut(&session_id) {
                if let Some(seq) = seq {
                    slot.last_seq = slot.last_seq.max(seq);
                }
                let _ = slot.events.send(line);
            } else {
                tracing::debug!(%session_id, "event for an unknown fleetd session; dropping");
            }
        }
        FleetdToDaemon::SessionExited {
            session_id,
            exit_code,
            stderr_tail,
        } => {
            let slot = shared.sessions.lock().remove(&session_id);
            match slot {
                Some(slot) => slot.finish(&session_id, exit_code, stderr_tail),
                None => {
                    tracing::debug!(%session_id, "exit for an unknown fleetd session; ignoring")
                }
            }
        }
        FleetdToDaemon::Sessions { sessions } => {
            let waiter = shared.list_waiters.lock().pop();
            if let Some(waiter) = waiter {
                let _ = waiter.send(sessions);
            }
        }
        FleetdToDaemon::ReplayComplete {
            session_id,
            through_seq,
        } => {
            // A session that had already exited when we re-adopted it will
            // never get a `SessionExited` (fleetd sent that to the previous
            // daemon), so its replay terminator is where terminal state gets
            // published. A live session just becomes live-tailing.
            let pending = {
                let mut sessions = shared.sessions.lock();
                match sessions.get_mut(&session_id) {
                    Some(slot) if slot.exit_after_replay.is_some() => {
                        slot.last_seq = slot.last_seq.max(through_seq);
                        sessions.remove(&session_id)
                    }
                    _ => None,
                }
            };
            match pending {
                Some(slot) => {
                    let exit_code = slot.exit_after_replay.flatten();
                    tracing::info!(
                        %session_id,
                        through_seq,
                        ?exit_code,
                        "replay complete for an already-exited session; publishing terminal state"
                    );
                    slot.finish(&session_id, exit_code, String::new());
                }
                None => {
                    tracing::info!(
                        %session_id,
                        through_seq,
                        "fleetd replay complete; session is live"
                    );
                }
            }
        }
        FleetdToDaemon::ReplayUnavailable {
            session_id,
            requested_from,
            earliest_available,
            latest_available,
        } => {
            // A documented gap, loudly. fleetd's window did not reach back to
            // our cursor, so events in (requested_from, earliest_available)
            // are gone; the session keeps running and we ingest from
            // earliest_available onward.
            tracing::warn!(
                %session_id,
                requested_from,
                earliest_available,
                latest_available,
                "fleetd replay window does not reach our cursor; resuming with a gap"
            );
        }
        FleetdToDaemon::Error {
            session_id,
            code,
            message,
        } => {
            if let Some(session_id) = session_id.as_deref() {
                let mut sessions = shared.sessions.lock();
                if let Some(slot) = sessions.get_mut(session_id)
                    && let Some(started) = slot.started.take()
                {
                    let _ = started.send(Err(format!("{code}: {message}")));
                }
            }
            tracing::warn!(?session_id, %code, %message, "fleetd reported an error");
        }
        FleetdToDaemon::Ready { .. } => {
            tracing::warn!("unexpected second fleetd ready message; ignoring");
        }
        FleetdToDaemon::Unknown => {
            tracing::debug!("unknown fleetd message variant; skipping");
        }
    }
}

/// Ask fleetd what it is holding, reattach everything the task store knows
/// about, and replay each from the daemon's own durable cursor.
async fn readopt_live_sessions(
    shared: &Arc<Shared>,
    commands: &mpsc::UnboundedSender<DaemonToFleetd>,
) -> anyhow::Result<()> {
    let (tx, rx) = oneshot::channel();
    shared.list_waiters.lock().push(tx);
    commands
        .send(DaemonToFleetd::ListSessions)
        .map_err(|_| anyhow::anyhow!("connection dropped before ListSessions"))?;
    let summaries = tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .map_err(|_| anyhow::anyhow!("fleetd did not answer ListSessions in time"))?
        .map_err(|_| anyhow::anyhow!("connection dropped awaiting ListSessions"))?;

    for summary in summaries {
        // Already wired up (a reconnect where we never lost the slot, or our
        // own just-sent spawn): nothing to re-adopt.
        if shared.sessions.lock().contains_key(&summary.session_id) {
            continue;
        }
        readopt_one(shared, commands, summary);
    }
    Ok(())
}

fn readopt_one(
    shared: &Arc<Shared>,
    commands: &mpsc::UnboundedSender<DaemonToFleetd>,
    summary: SessionSummary,
) {
    let (events_tx, events_rx) = mpsc::unbounded_channel::<String>();
    let (outcome_tx, outcome_rx) = oneshot::channel();
    let control = control_lane(summary.session_id.clone(), commands.clone());
    let killer = WorkerKill::via_fleetd(summary.session_id.clone(), commands.clone());

    let Some(cursor) = super::readopt_harness_session(super::ReadoptedSession {
        session_id: summary.session_id.clone(),
        task_id: summary.task_id.clone(),
        pid: summary.pid,
        state: summary.state,
        control,
        killer,
        events: events_rx,
        outcome: outcome_rx,
    }) else {
        // Not ours, or terminal and already published. An unknown RUNNING
        // session is left strictly alone: it is somebody's live work, and the
        // daemon forgetting it (task TTL, a wiped store) is not a reason to
        // kill it. fleetd reaps terminal sessions itself once acked.
        if summary.state == SessionState::Running {
            tracing::warn!(
                session_id = %summary.session_id,
                task_id = %summary.task_id,
                pid = ?summary.pid,
                "fleetd holds a running session this daemon's task store does not \
                 know; leaving it alone (kill it by hand if it is an orphan)"
            );
        }
        return;
    };

    shared.sessions.lock().insert(
        summary.session_id.clone(),
        SessionSlot {
            events: events_tx,
            // Already started; nothing to acknowledge.
            started: None,
            outcome: Some(outcome_tx),
            // An already-exited session publishes terminal state when its
            // replay terminates: fleetd sent its `SessionExited` to the daemon
            // instance that is gone.
            exit_after_replay: (summary.state == SessionState::Exited).then_some(summary.exit_code),
            last_seq: cursor,
            commands: commands.clone(),
        },
    );

    tracing::info!(
        session_id = %summary.session_id,
        task_id = %summary.task_id,
        from_seq = cursor,
        last_seq = ?summary.last_seq,
        "re-adopting a fleetd session; replaying from our cursor"
    );
    let _ = commands.send(DaemonToFleetd::ReplayFrom {
        session_id: summary.session_id,
        from_seq: cursor,
    });
}

/// Start fleetd detached and wait, bounded, for its socket to appear.
///
/// Detached on purpose: `setsid` plus a fresh session means the supervisor is
/// not in the daemon's process group, so it survives the `launchctl kickstart`
/// that replaces `blackboxd`. That survival IS the point of the slice.
async fn start_fleetd(config: &FleetdConfig) -> anyhow::Result<()> {
    let binary = resolve_fleetd_binary(config)?;
    tokio::fs::create_dir_all(&config.state_dir).await.ok();

    tracing::info!(
        binary = %binary.display(),
        state_dir = %config.state_dir.display(),
        "fleetd socket absent; starting it detached"
    );

    let mut command = tokio::process::Command::new(&binary);
    command
        .arg("--state-dir")
        .arg(&config.state_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Reap it ourselves is exactly what we do NOT want: the child must
        // outlive us. `kill_on_drop` stays false (the default) and the new
        // session detaches it from our process group.
        .process_group(0);

    let mut child = command
        .spawn()
        .map_err(|error| anyhow::anyhow!("cannot start fleetd at {}: {error}", binary.display()))?;
    // Do not hold the handle: dropping a tokio Child without `kill_on_drop`
    // leaves the process running and lets tokio reap it in the background.
    let pid = child.id();
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    let deadline = tokio::time::Instant::now() + AUTOSTART_SOCKET_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if UnixStream::connect(&config.socket).await.is_ok() {
            tracing::info!(?pid, "fleetd is listening");
            return Ok(());
        }
        tokio::time::sleep(AUTOSTART_POLL_INTERVAL).await;
    }
    anyhow::bail!(
        "started fleetd ({}) but its socket {} did not come up within {}s",
        binary.display(),
        config.socket.display(),
        AUTOSTART_SOCKET_TIMEOUT.as_secs()
    )
}

/// `BLACKBOX_FLEETD_BIN`, else a `fleetd` sitting next to this daemon binary,
/// else whatever `fleetd` is on PATH. The sibling lookup is what makes a
/// `~/.local/bin` install work without configuration.
fn resolve_fleetd_binary(config: &FleetdConfig) -> anyhow::Result<PathBuf> {
    if let Some(binary) = config.binary.as_ref() {
        return Ok(binary.clone());
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(sibling) = current.parent().map(|dir| dir.join("fleetd"))
        && sibling.is_file()
    {
        return Ok(sibling);
    }
    Ok(PathBuf::from("fleetd"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_hang_off_the_state_dir() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config = FleetdConfig::in_state_dir(&root);
        assert_eq!(config.socket, root.join("fleetd.sock"));
        assert_eq!(config.token, root.join("fleetd.token"));
        assert_eq!(config.state_dir, root);
    }

    /// Prod and dev daemons have different state dirs, so they must never
    /// share a supervisor. Mirrors the assertion on fleetd's own side.
    #[test]
    fn distinct_state_dirs_yield_distinct_sockets() {
        let prod = FleetdConfig::in_state_dir("/state/prod");
        let dev = FleetdConfig::in_state_dir("/state/dev");
        assert_ne!(prod.socket, dev.socket);
        assert_ne!(prod.token, dev.token);
    }

    /// The daemon's advertised identity has to satisfy `bro_rpc`'s own
    /// validation, or every handshake fails at the first frame.
    #[test]
    fn build_identity_is_valid() {
        build_identity().validate().expect("valid build identity");
    }

    /// An explicit binary always wins over the sibling/PATH lookup.
    #[test]
    fn explicit_binary_wins() {
        let mut config = FleetdConfig::in_state_dir("/state/x");
        config.binary = Some(PathBuf::from("/opt/custom/fleetd"));
        assert_eq!(
            resolve_fleetd_binary(&config).unwrap(),
            PathBuf::from("/opt/custom/fleetd")
        );
    }

    /// A `SessionExited` must close the event lane before publishing the
    /// outcome, so the daemon's ingest loop drains to EOF and the terminal
    /// waiter's join completes rather than hanging.
    #[tokio::test]
    async fn session_exit_closes_events_then_publishes_outcome() {
        let shared = Arc::new(Shared {
            config: FleetdConfig::in_state_dir("/state/x"),
            sessions: Mutex::new(HashMap::new()),
            list_waiters: Mutex::new(Vec::new()),
            message_counter: AtomicU64::new(0),
        });
        let (commands_tx, mut commands_rx) = mpsc::unbounded_channel::<DaemonToFleetd>();
        let (events_tx, mut events_rx) = mpsc::unbounded_channel::<String>();
        let (outcome_tx, outcome_rx) = oneshot::channel();
        shared.sessions.lock().insert(
            "sess-1".to_string(),
            SessionSlot {
                events: events_tx,
                started: None,
                outcome: Some(outcome_tx),
                exit_after_replay: None,
                last_seq: 0,
                commands: commands_tx.clone(),
            },
        );

        handle_message(
            &shared,
            FleetdToDaemon::Event {
                session_id: "sess-1".to_string(),
                seq: Some(1),
                line: "{\"type\":\"assistant\"}".to_string(),
            },
        );
        handle_message(
            &shared,
            FleetdToDaemon::SessionExited {
                session_id: "sess-1".to_string(),
                exit_code: Some(0),
                stderr_tail: "warn\n".to_string(),
            },
        );

        assert_eq!(
            events_rx.recv().await.as_deref(),
            Some("{\"type\":\"assistant\"}")
        );
        assert!(
            events_rx.recv().await.is_none(),
            "event lane must close at exit"
        );
        let outcome = outcome_rx.await.expect("outcome published");
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stderr, "warn\n");
        assert!(
            !shared.sessions.lock().contains_key("sess-1"),
            "terminal sessions are dropped from the slot map"
        );
        // fleetd GCs a terminal session only once acked through its last seq.
        assert_eq!(
            commands_rx.recv().await,
            Some(DaemonToFleetd::EventAck {
                session_id: "sess-1".to_string(),
                through_seq: 1,
            })
        );
    }

    /// A session-scoped `Error` before `SessionStarted` must fail the pending
    /// spawn rather than leave it to time out.
    #[tokio::test]
    async fn session_error_fails_a_pending_spawn() {
        let shared = Arc::new(Shared {
            config: FleetdConfig::in_state_dir("/state/x"),
            sessions: Mutex::new(HashMap::new()),
            list_waiters: Mutex::new(Vec::new()),
            message_counter: AtomicU64::new(0),
        });
        let (commands_tx, _commands_rx) = mpsc::unbounded_channel::<DaemonToFleetd>();
        let (events_tx, _events_rx) = mpsc::unbounded_channel::<String>();
        let (started_tx, started_rx) = oneshot::channel();
        let (outcome_tx, _outcome_rx) = oneshot::channel();
        shared.sessions.lock().insert(
            "sess-1".to_string(),
            SessionSlot {
                events: events_tx,
                started: Some(started_tx),
                outcome: Some(outcome_tx),
                exit_after_replay: None,
                last_seq: 0,
                commands: commands_tx.clone(),
            },
        );

        handle_message(
            &shared,
            FleetdToDaemon::Error {
                session_id: Some("sess-1".to_string()),
                code: "spawn_failed".to_string(),
                message: "no such file".to_string(),
            },
        );

        let started = started_rx.await.expect("start resolved");
        assert_eq!(started, Err("spawn_failed: no such file".to_string()));
    }

    /// Message ids must be unique per connection: `validate_envelope` rejects
    /// empty ids, and a duplicate would make replies ambiguous.
    #[test]
    fn message_ids_are_unique_and_non_empty() {
        let shared = Shared {
            config: FleetdConfig::in_state_dir("/state/x"),
            sessions: Mutex::new(HashMap::new()),
            list_waiters: Mutex::new(Vec::new()),
            message_counter: AtomicU64::new(0),
        };
        let first = shared.next_message_id();
        let second = shared.next_message_id();
        assert!(!first.is_empty());
        assert_ne!(first, second);
    }
}

include!("fleetd_smoke.rs");
