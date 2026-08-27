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

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use bro_protocol::{
    BearerToken, DaemonToFleetd, FLEETD_PROTOCOL_VERSION, FleetdToDaemon, SessionState,
    SessionSummary, WorkerSpawnSpec, WorkspaceInspectionOutcome, WorkspaceInspectionRequest,
};
use bro_rpc::{BuildIdentity, Envelope, HandshakeOptions, NegotiatedIo, ServiceToken};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};

use super::executor::{HarnessExecutor, WorkerHandle, WorkerKill, WorkerLocality, WorkerOutcome};

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
/// Worker inspection executes a handful of local Git and filesystem reads.
const WORKSPACE_INSPECTION_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for fleetd's `Sessions` answer when proving whether a
/// colliding supervision key is still live, during re-adoption, and on each
/// heartbeat probe.
const LIST_SESSIONS_TIMEOUT: Duration = Duration::from_secs(10);
/// How often an established connection proves the link end-to-end. A write
/// into a dead TCP peer succeeds into the kernel buffer, so only a served
/// round-trip is evidence of liveness (see `spawn_heartbeat`).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// fleetd's error code for a spawn whose session id its registry still holds.
/// A terminal entry nobody acked keeps the key occupied on fleetd's side even
/// after the daemon has released its own slot.
const FLEETD_DUPLICATE_SESSION_CODE: &str = "session.duplicate";

/// The one fleetd this daemon owns. Multi-fleet routing is deliberately not
/// hidden in this type: v1 has one endpoint and therefore one owner/fencing
/// domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetdEndpoint {
    Unix(PathBuf),
    Tcp(String),
}

impl FleetdEndpoint {
    fn label(&self) -> String {
        match self {
            Self::Unix(path) => format!("unix://{}", path.display()),
            Self::Tcp(address) => format!("tcp://{address}"),
        }
    }

    fn is_remote(&self) -> bool {
        matches!(self, Self::Tcp(_))
    }
}

/// Where fleetd and its token live, plus how to start the local Unix form if
/// it is not running.
#[derive(Debug, Clone)]
pub struct FleetdConfig {
    pub endpoint: FleetdEndpoint,
    pub token: PathBuf,
    /// State dir handed to a fleetd we start ourselves, so it derives the same
    /// socket/token paths we are dialing.
    pub state_dir: PathBuf,
    /// Explicit fleetd binary. `None` means resolve at start time.
    pub binary: Option<PathBuf>,
    /// Explicit roots on an off-host fleetd machine. Same-host Unix operation
    /// leaves this absent because daemon and worker paths are identical.
    pub worker_locality: Option<WorkerLocality>,
    /// Cadence of the per-connection liveness probe. Production uses
    /// [`HEARTBEAT_INTERVAL`]; tests shrink it to exercise the drop path.
    pub heartbeat_interval: Duration,
    /// Deadline for `Sessions` answers (re-adoption, stale-key proof, and the
    /// heartbeat probe). Production uses [`LIST_SESSIONS_TIMEOUT`].
    pub list_sessions_timeout: Duration,
    /// Deadline for a workspace inspection answer. Production uses
    /// [`WORKSPACE_INSPECTION_TIMEOUT`].
    pub inspection_timeout: Duration,
}

impl FleetdConfig {
    pub fn in_state_dir(state_dir: impl AsRef<Path>) -> Self {
        let state_dir = state_dir.as_ref().to_path_buf();
        Self {
            endpoint: FleetdEndpoint::Unix(state_dir.join(SOCKET_FILE)),
            token: state_dir.join(TOKEN_FILE),
            state_dir,
            binary: std::env::var("BLACKBOX_FLEETD_BIN")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from),
            worker_locality: None,
            heartbeat_interval: HEARTBEAT_INTERVAL,
            list_sessions_timeout: LIST_SESSIONS_TIMEOUT,
            inspection_timeout: WORKSPACE_INSPECTION_TIMEOUT,
        }
    }

    /// Resolve the daemon's explicit fleetd settings. An absent endpoint is
    /// the same-host Unix default. Remote TCP is fail-closed: it requires an
    /// explicit token file and never inherits the daemon's state-local token.
    pub fn resolve(
        state_dir: impl AsRef<Path>,
        endpoint: Option<&str>,
        token_file: Option<&Path>,
        worker_home: Option<&Path>,
        worker_bro_home: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let state_dir = state_dir.as_ref().to_path_buf();
        let Some(endpoint) = endpoint.map(str::trim).filter(|value| !value.is_empty()) else {
            if token_file.is_some() || worker_home.is_some() || worker_bro_home.is_some() {
                anyhow::bail!(
                    "remote fleetd token/worker paths require daemon.fleetd_endpoint; refusing ambiguous off-host settings on the state-local Unix executor"
                );
            }
            return Ok(Self::in_state_dir(state_dir));
        };
        let Some(address) = endpoint.strip_prefix("tcp://") else {
            anyhow::bail!(
                "unsupported fleetd endpoint `{endpoint}`; expected tcp://host:port or omit it for the state-local Unix socket"
            );
        };
        validate_tcp_address(endpoint, address)?;
        let token = token_file.ok_or_else(|| {
            anyhow::anyhow!(
                "remote fleetd endpoint `{endpoint}` requires daemon.fleetd_token_file or BLACKBOX_FLEETD_TOKEN_FILE"
            )
        })?;
        let worker_home =
            required_absolute_worker_path(endpoint, "daemon.fleetd_worker_home", worker_home)?;
        let worker_bro_home = required_absolute_worker_path(
            endpoint,
            "daemon.fleetd_worker_bro_home",
            worker_bro_home,
        )?;
        Ok(Self {
            endpoint: FleetdEndpoint::Tcp(address.to_string()),
            token: token.to_path_buf(),
            state_dir,
            binary: None,
            worker_locality: Some(WorkerLocality {
                home: worker_home,
                bro_home: worker_bro_home,
            }),
            heartbeat_interval: HEARTBEAT_INTERVAL,
            list_sessions_timeout: LIST_SESSIONS_TIMEOUT,
            inspection_timeout: WORKSPACE_INSPECTION_TIMEOUT,
        })
    }
}

fn required_absolute_worker_path(
    endpoint: &str,
    setting: &str,
    path: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let path = path
        .ok_or_else(|| anyhow::anyhow!("remote fleetd endpoint `{endpoint}` requires {setting}"))?;
    if !path.is_absolute() {
        anyhow::bail!(
            "{setting} must be an absolute worker-local path, got {}",
            path.display()
        );
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        anyhow::bail!("{setting} must not contain `..`, got {}", path.display());
    }
    Ok(path.to_path_buf())
}

fn validate_tcp_address(endpoint: &str, address: &str) -> anyhow::Result<()> {
    if address.starts_with('[') {
        let parsed = address.parse::<std::net::SocketAddr>().map_err(|error| {
            anyhow::anyhow!("invalid fleetd TCP endpoint `{endpoint}`: {error}")
        })?;
        if parsed.port() == 0 {
            anyhow::bail!("fleetd TCP endpoint `{endpoint}` cannot use port zero");
        }
        return Ok(());
    }
    let Some((host, port)) = address.rsplit_once(':') else {
        anyhow::bail!("fleetd TCP endpoint `{endpoint}` must include host and port");
    };
    if host.trim().is_empty() || host.contains(':') {
        anyhow::bail!(
            "fleetd TCP endpoint `{endpoint}` has an invalid host; bracket IPv6 addresses"
        );
    }
    let port = port.parse::<u16>().map_err(|error| {
        anyhow::anyhow!("fleetd TCP endpoint `{endpoint}` has an invalid port: {error}")
    })?;
    if port == 0 {
        anyhow::bail!("fleetd TCP endpoint `{endpoint}` cannot use port zero");
    }
    Ok(())
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

/// The live connection: a command queue feeding the writer task, the
/// generation fleetd allocated for it, and the cancellation lever that tears
/// the whole actor down (writer, reader, heartbeat) as one unit.
struct Connection {
    generation: u64,
    commands: mpsc::UnboundedSender<DaemonToFleetd>,
    cancel: CancellationToken,
}

impl Connection {
    /// A queue that is open but cancelled is a corpse: the writer has already
    /// stopped draining it (or is about to), so handing it out would park the
    /// caller on a channel nobody serves.
    fn is_alive(&self) -> bool {
        !self.cancel.is_cancelled() && !self.commands.is_closed()
    }

    fn lane(&self) -> Lane {
        Lane {
            generation: self.generation,
            commands: self.commands.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

/// One caller's handle on the live connection: the command sender plus the
/// lever to invalidate the connection when a round-trip proves it dead. A
/// silently dead TCP peer (worker host reboot, dropped WAN path) accepts
/// writes into the kernel buffer indefinitely, so write success proves
/// nothing; callers whose reply deadline expires MUST cancel the lane rather
/// than leave the corpse installed for the next dispatch.
#[derive(Clone)]
struct Lane {
    generation: u64,
    commands: mpsc::UnboundedSender<DaemonToFleetd>,
    cancel: CancellationToken,
}

/// Shared client state. Sessions outlive any single connection, which is what
/// makes reconnect-and-replay work rather than reconnect-and-lose.
struct Shared {
    config: FleetdConfig,
    sessions: Mutex<HashMap<String, SessionSlot>>,
    /// Waiters for `Sessions` answers as `(waiter_id, generation, sender)`,
    /// matched to replies in FIFO order within their connection generation.
    /// The waiter id lets a timed-out waiter remove itself (leaving it queued
    /// would desync every later reply by one); the generation lets a dying
    /// reader clear ITS waiters without wiping a successor connection's, and
    /// keeps a stale connection's late reply from satisfying a fresh
    /// connection's waiter.
    list_waiters: Mutex<VecDeque<(u64, u64, oneshot::Sender<Vec<SessionSummary>>)>>,
    /// Workspace inspection replies are request-id correlated because more
    /// than one dispatch may inspect concurrently over the owner connection.
    /// The generation exists for the dying reader's scoped cleanup, exactly
    /// as for `list_waiters`.
    workspace_waiters: Mutex<HashMap<String, (u64, oneshot::Sender<WorkspaceInspectionOutcome>)>>,
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
                list_waiters: Mutex::new(VecDeque::new()),
                workspace_waiters: Mutex::new(HashMap::new()),
                message_counter: AtomicU64::new(0),
            }),
            connection: tokio::sync::Mutex::new(None),
        }
    }

    /// Return a lane on a live connection, dialing (and if necessary
    /// starting) fleetd first. Every successful dial runs re-adoption before
    /// returning, so a caller that gets a lane is talking to a fleetd whose
    /// live sessions are already reattached.
    async fn lane(&self) -> anyhow::Result<Lane> {
        let mut guard = self.connection.lock().await;
        if let Some(connection) = guard.as_ref()
            && connection.is_alive()
        {
            return Ok(connection.lane());
        }
        let connection = self.dial().await?;
        let lane = connection.lane();
        *guard = Some(connection);
        drop(guard);

        // Re-adoption runs outside the connection lock: it awaits a
        // ListSessions round trip, and holding the lock across that would
        // serialize every concurrent dispatch behind it for no reason.
        if let Err(error) = readopt_live_sessions(&self.shared, &lane).await {
            tracing::warn!(
                %error,
                generation = lane.generation,
                "fleetd re-adoption failed; live sessions may not be reattached"
            );
        }
        Ok(lane)
    }

    /// Connect, authenticate, and start the connection actor.
    async fn dial(&self) -> anyhow::Result<Connection> {
        match &self.shared.config.endpoint {
            FleetdEndpoint::Unix(socket) => {
                if UnixStream::connect(socket).await.is_err() {
                    start_fleetd(&self.shared.config).await?;
                }
                let stream = UnixStream::connect(socket).await.map_err(|error| {
                    anyhow::anyhow!(
                        "cannot reach fleetd at {}: {error}. Start it (launchd label \
                         com.daystrom.fleetd, or `fleetd --state-dir {}`), or set \
                         BLACKBOX_EXECUTOR=local to run workers as daemon children.",
                        socket.display(),
                        self.shared.config.state_dir.display()
                    )
                })?;
                self.finish_dial(stream).await
            }
            FleetdEndpoint::Tcp(address) => {
                let stream = TcpStream::connect(address).await.map_err(|error| {
                    anyhow::anyhow!(
                        "cannot reach remote fleetd at tcp://{address}: {error}. Remote fleetd is never auto-started and there is no local-executor fallback."
                    )
                })?;
                stream.set_nodelay(true).map_err(|error| {
                    anyhow::anyhow!("cannot configure remote fleetd socket: {error}")
                })?;
                self.finish_dial(stream).await
            }
        }
    }

    async fn finish_dial<S>(&self, stream: S) -> anyhow::Result<Connection>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let endpoint = self.shared.config.endpoint.label();
        // Same-host startup is symmetric: whichever process starts first may
        // create the token. A remote daemon is only a consumer and must never
        // create a replacement secret if its mount/config is wrong.
        let token = if self.shared.config.endpoint.is_remote() {
            ServiceToken::load(&self.shared.config.token)
        } else {
            ServiceToken::load_or_create(&self.shared.config.token)
        }
        .map_err(|error| {
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
        let cancel = CancellationToken::new();
        spawn_writer(
            self.shared.clone(),
            writer,
            commands_rx,
            generation,
            cancel.clone(),
        );
        spawn_reader(
            self.shared.clone(),
            reader,
            commands_tx.clone(),
            generation,
            cancel.clone(),
        );
        spawn_heartbeat(
            self.shared.clone(),
            commands_tx.clone(),
            generation,
            cancel.clone(),
        );

        tracing::info!(
            %endpoint,
            generation,
            fleetd_version = %welcome.build.version,
            fleetd_build = %welcome.build.build_id,
            "connected to fleetd"
        );
        Ok(Connection {
            generation,
            commands: commands_tx,
            cancel,
        })
    }
}

#[async_trait]
impl HarnessExecutor for FleetdExecutor {
    fn provider_binary_location(&self) -> super::executor::ProviderBinaryLocation {
        super::executor::ProviderBinaryLocation::ExecutorHost
    }

    fn worker_locality(&self) -> Option<&WorkerLocality> {
        self.shared.config.worker_locality.as_ref()
    }

    /// Inspection is a read-only, idempotent probe, so a failed attempt
    /// cancels the lane it ran on (installing nothing is better than leaving
    /// a corpse for the next dispatch) and retries exactly once on a fresh
    /// dial. That folds the single most common failure, a connection that
    /// silently died since the last dispatch, into one transparent redial
    /// instead of a failed dispatch.
    async fn inspect_workspace(
        &self,
        request: WorkspaceInspectionRequest,
    ) -> anyhow::Result<WorkspaceInspectionOutcome> {
        let mut last_error = None;
        for attempt in 0..2u8 {
            let lane = self.lane().await?;
            let request_id = self.shared.next_message_id();
            let (tx, rx) = oneshot::channel();
            self.shared
                .workspace_waiters
                .lock()
                .insert(request_id.clone(), (lane.generation, tx));
            if lane
                .commands
                .send(DaemonToFleetd::InspectWorkspace {
                    request_id: request_id.clone(),
                    request: request.clone(),
                })
                .is_err()
            {
                self.shared.workspace_waiters.lock().remove(&request_id);
                lane.cancel.cancel();
                last_error = Some(anyhow::anyhow!(
                    "fleetd connection dropped before workspace inspection was sent"
                ));
                continue;
            }
            match tokio::time::timeout(self.shared.config.inspection_timeout, rx).await {
                Ok(Ok(outcome)) => return Ok(outcome),
                Ok(Err(_)) => {
                    lane.cancel.cancel();
                    last_error = Some(anyhow::anyhow!(
                        "fleetd connection dropped before workspace inspection completed"
                    ));
                    continue;
                }
                Err(_) => {
                    self.shared.workspace_waiters.lock().remove(&request_id);
                    lane.cancel.cancel();
                    tracing::warn!(
                        generation = lane.generation,
                        attempt,
                        "workspace inspection went unanswered; dropping the fleetd connection"
                    );
                    last_error = Some(anyhow::anyhow!(
                        "fleetd did not answer workspace inspection within {}s",
                        self.shared.config.inspection_timeout.as_secs()
                    ));
                    continue;
                }
            }
        }
        Err(last_error.expect("two failed inspection attempts recorded an error"))
    }

    async fn spawn(&self, mut spec: WorkerSpawnSpec) -> anyhow::Result<WorkerHandle> {
        // Defense in depth at the process boundary: even if a future dispatch
        // composer accidentally derives these from the daemon container,
        // remote fleetd never receives an off-host BRO_HOME. The supervision
        // id is already the canonical event-log filename key.
        if let Some(locality) = self.shared.config.worker_locality.as_ref() {
            spec.bro_home = locality.bro_home.clone();
            spec.event_log_path = locality
                .bro_home
                .join("harness-sessions")
                .join(format!("{}.events.jsonl", spec.session_id));
        }
        let lane = self.lane().await?;
        let commands = lane.commands.clone();
        let session_id = spec.session_id.clone();

        // A supervision key may be released at most once per dispatch: if a
        // stale slot (or a stale fleetd registry entry) is cleared and the
        // key STILL collides afterwards, something genuinely live owns it.
        let mut released_stale_key = false;
        loop {
            let (events_tx, events_rx) = mpsc::unbounded_channel::<String>();
            let (started_tx, started_rx) = oneshot::channel();
            let (outcome_tx, outcome_rx) = oneshot::channel();
            let claimed = {
                let mut sessions = self.shared.sessions.lock();
                if sessions.contains_key(&session_id) {
                    false
                } else {
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
                    true
                }
            };
            if !claimed {
                if released_stale_key {
                    anyhow::bail!(
                        "fleetd session id `{session_id}` was claimed again while its stale \
                         key was being released; refusing to multiplex two dispatches onto \
                         one supervision key"
                    );
                }
                match release_stale_supervision_key(&self.shared, &lane, &session_id).await {
                    Ok(true) => {
                        released_stale_key = true;
                        continue;
                    }
                    Ok(false) => anyhow::bail!(
                        "fleetd session id `{session_id}` is genuinely live (fleetd reports a \
                         running worker under this supervision key); refusing to multiplex \
                         two dispatches onto one supervision key"
                    ),
                    Err(error) => anyhow::bail!(
                        "fleetd session id `{session_id}` is registered as live and fleetd \
                         could not confirm whether its worker is still running ({error}); \
                         refusing to multiplex two dispatches onto one supervision key"
                    ),
                }
            }

            if commands
                .send(DaemonToFleetd::Spawn {
                    spec: Box::new(spec.clone()),
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
                    // fleetd still holds a terminal entry under this key
                    // (an exited session nobody acked). Prove it is not
                    // live, release it, and try once more.
                    if message.starts_with(FLEETD_DUPLICATE_SESSION_CODE)
                        && !released_stale_key
                        && release_stale_supervision_key(&self.shared, &lane, &session_id).await?
                    {
                        released_stale_key = true;
                        continue;
                    }
                    anyhow::bail!("fleetd refused the spawn: {message}");
                }
                Ok(Err(_)) => {
                    self.shared.sessions.lock().remove(&session_id);
                    anyhow::bail!("fleetd connection dropped before the session started");
                }
                Err(_) => {
                    self.shared.sessions.lock().remove(&session_id);
                    // No retry here: unlike inspection, the spawn may have
                    // gone through on fleetd's side, and retrying would risk
                    // a second worker under the same supervision key. But an
                    // unanswered ack is still proof the connection is dead,
                    // so cancel it rather than leave the corpse installed.
                    lane.cancel.cancel();
                    anyhow::bail!(
                        "fleetd did not acknowledge the spawn within {}s",
                        SPAWN_ACK_TIMEOUT.as_secs()
                    );
                }
            };

            return Ok(WorkerHandle {
                control: control_lane(session_id.clone(), commands.clone()),
                events: events_rx,
                pid,
                killer: WorkerKill::via_fleetd(session_id, commands),
                outcome: outcome_rx,
            });
        }
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
/// wire. Exits when the queue closes, the socket errors, or the connection is
/// cancelled; every exit path cancels the connection so the reader and
/// heartbeat die with it and `Connection::is_alive` turns false for the next
/// dispatch to redial on.
fn spawn_writer<W>(
    shared: Arc<Shared>,
    mut writer: NegotiatedIo<W>,
    mut commands: mpsc::UnboundedReceiver<DaemonToFleetd>,
    generation: u64,
    cancel: CancellationToken,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let body = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                body = commands.recv() => match body {
                    Some(body) => body,
                    None => break,
                },
            };
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
        cancel.cancel();
    });
}

/// Reader task: fan every `FleetdToDaemon` out to the session it names.
fn spawn_reader<R>(
    shared: Arc<Shared>,
    mut reader: NegotiatedIo<R>,
    commands: mpsc::UnboundedSender<DaemonToFleetd>,
    generation: u64,
    cancel: CancellationToken,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            // `read_envelope` is not cancel-safe, but that only matters for a
            // reader that keeps reading afterwards: on cancellation the whole
            // half is dropped, so a torn mid-frame read is unreachable.
            let envelope = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                result = reader.read_envelope::<FleetdToDaemon>() => match result {
                    Ok(envelope) => envelope,
                    Err(error) => {
                        tracing::info!(%error, "fleetd connection closed");
                        break;
                    }
                },
            };
            handle_message(&shared, envelope.body, generation);
        }
        // Reader death is connection death: without a read half no reply can
        // ever arrive, so an "alive" writer would only park callers on their
        // reply deadlines (2026-08-27: exactly that corpse pattern held the
        // dispatch plane down for hours after a worker-host reboot).
        cancel.cancel();
        // The socket is gone. Live sessions keep their slots: fleetd kept the
        // children running, and the next dial re-adopts and replays them.
        // Pending spawn acks cannot be satisfied, so their waiters are dropped
        // (the `Err` arm in `spawn` turns that into a loud dispatch failure).
        // Waiter cleanup is scoped to THIS generation: a retry on a fresh
        // connection may already have waiters registered, and wiping those
        // would fail the very dispatch the redial just rescued.
        let mut sessions = shared.sessions.lock();
        for slot in sessions.values_mut() {
            if slot.commands.same_channel(&commands) {
                slot.started.take();
            }
        }
        drop(sessions);
        shared
            .list_waiters
            .lock()
            .retain(|(_, waiter_generation, _)| *waiter_generation != generation);
        shared
            .workspace_waiters
            .lock()
            .retain(|_, (waiter_generation, _)| *waiter_generation != generation);
    });
}

/// Heartbeat task: prove the connection end-to-end on a cadence, and cancel
/// it the moment a probe goes unanswered.
///
/// A worker host that reboots (or a WAN path that dies underneath a long-idle
/// connection) sends no FIN or RST: reads block forever and writes keep
/// succeeding into the kernel buffer, so neither the reader's error path nor
/// the writer's can notice. Only a served round-trip proves the peer is
/// there. The probe is a `ListSessions` rather than a dedicated ping because
/// deployed fleetd builds answer it today, whereas an unknown variant falls
/// into their `#[serde(other)]` skip and a probe nobody answers would kill
/// every healthy connection to an older fleetd.
fn spawn_heartbeat(
    shared: Arc<Shared>,
    commands: mpsc::UnboundedSender<DaemonToFleetd>,
    generation: u64,
    cancel: CancellationToken,
) {
    let interval = shared.config.heartbeat_interval;
    let lane = Lane {
        generation,
        commands,
        cancel: cancel.clone(),
    };
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(interval) => {}
            }
            if let Err(error) = list_sessions(&shared, &lane).await {
                tracing::warn!(
                    %error,
                    generation,
                    "fleetd heartbeat went unanswered; dropping the connection"
                );
                cancel.cancel();
                return;
            }
        }
    });
}

fn handle_message(shared: &Arc<Shared>, message: FleetdToDaemon, generation: u64) {
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
            // Replies match waiters FIFO within the connection that carried
            // them: a stale connection's late answer must not satisfy (and
            // thereby desync) a fresh connection's queue.
            let waiter = {
                let mut waiters = shared.list_waiters.lock();
                waiters
                    .iter()
                    .position(|(_, waiter_generation, _)| *waiter_generation == generation)
                    .and_then(|index| waiters.remove(index))
            };
            if let Some((_, _, waiter)) = waiter {
                let _ = waiter.send(sessions);
            }
        }
        FleetdToDaemon::WorkspaceInspected {
            request_id,
            outcome,
        } => {
            let waiter = shared.workspace_waiters.lock().remove(&request_id);
            if let Some((_, waiter)) = waiter {
                let _ = waiter.send(outcome);
            } else {
                tracing::debug!(%request_id, "late or unknown workspace inspection reply");
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
            // A dead session gets no `ReplayComplete` after this, and nothing
            // else will ever come for it: publish its terminal state now,
            // acking through the end of fleetd's window so it can GC. Leaving
            // the slot would register a dead supervision key as live.
            finish_dead_session_slot(shared, &session_id, Some(latest_available));
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
                drop(sessions);
                // A replay that failed, or a session fleetd does not know,
                // will never terminate a dead session's slot on its own.
                if code == "replay.failed" || code == "session.unknown" {
                    finish_dead_session_slot(shared, session_id, None);
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

/// Publish terminal state for a slot that was re-adopted as already exited
/// (or reconciled to that state) but whose replay terminator will not arrive.
/// A live slot is left untouched: its terminal state still arrives on the wire.
fn finish_dead_session_slot(shared: &Arc<Shared>, session_id: &str, through_seq: Option<u64>) {
    let slot = {
        let mut sessions = shared.sessions.lock();
        match sessions.get(session_id) {
            Some(slot) if slot.exit_after_replay.is_some() => sessions.remove(session_id),
            _ => None,
        }
    };
    let Some(mut slot) = slot else {
        return;
    };
    if let Some(through_seq) = through_seq {
        slot.last_seq = slot.last_seq.max(through_seq);
    }
    let exit_code = slot.exit_after_replay.flatten();
    tracing::info!(
        %session_id,
        ?exit_code,
        last_seq = slot.last_seq,
        "replay will not terminate for an already-exited session; publishing terminal state"
    );
    slot.finish(session_id, exit_code, String::new());
}

/// One `ListSessions` round trip.
async fn list_sessions(shared: &Arc<Shared>, lane: &Lane) -> anyhow::Result<Vec<SessionSummary>> {
    let (tx, rx) = oneshot::channel();
    let waiter_id = shared.message_counter.fetch_add(1, Ordering::Relaxed);
    shared
        .list_waiters
        .lock()
        .push_back((waiter_id, lane.generation, tx));
    let unqueue = || {
        shared
            .list_waiters
            .lock()
            .retain(|(id, _, _)| *id != waiter_id);
    };
    if lane.commands.send(DaemonToFleetd::ListSessions).is_err() {
        unqueue();
        anyhow::bail!("connection dropped before ListSessions");
    }
    match tokio::time::timeout(shared.config.list_sessions_timeout, rx).await {
        Ok(Ok(sessions)) => Ok(sessions),
        Ok(Err(_)) => anyhow::bail!("connection dropped awaiting ListSessions"),
        Err(_) => {
            unqueue();
            anyhow::bail!("fleetd did not answer ListSessions in time")
        }
    }
}

/// Prove whether the supervision key `session_id` is still owned by a live
/// child, and if it is not, release it on both sides of the seam.
///
/// This is the restart-orphan repair. After a daemon restart the client
/// re-adopts sessions fleetd reports as already `Exited` and expects their
/// replay terminator to publish terminal state and free the slot; when that
/// terminator never comes (the cursor fell outside fleetd's log window, the
/// replay failed, the connection dropped mid-replay) the slot leaks and every
/// `bro_resume` on the same provider session id collides with a dead key. The
/// task record meanwhile advertises exactly that resume as the recovery path.
///
/// fleetd is the authority on liveness, so this asks it rather than trusting
/// the slot: a slot fleetd reports `Running` is genuinely live and stays
/// refused (that is the real double-dispatch protection); a slot fleetd
/// reports `Exited` or does not know at all is dead. Releasing means
/// publishing the dead slot's terminal outcome (so its task leaves `Running`)
/// and acking fleetd through the highest seq either side saw, which lets
/// fleetd GC its own terminal entry so the follow-up `Spawn` is not refused
/// as a duplicate.
///
/// A slot still awaiting its own `SessionStarted` is an in-flight dispatch,
/// live by definition, and is never released.
///
/// Returns `Ok(true)` when the key was released and the caller may retry.
async fn release_stale_supervision_key(
    shared: &Arc<Shared>,
    lane: &Lane,
    session_id: &str,
) -> anyhow::Result<bool> {
    let commands = &lane.commands;
    let in_flight = shared
        .sessions
        .lock()
        .get(session_id)
        .is_some_and(|slot| slot.started.is_some());
    if in_flight {
        return Ok(false);
    }

    let summaries = list_sessions(shared, lane).await?;
    let summary = summaries
        .into_iter()
        .find(|summary| summary.session_id == session_id);
    if let Some(summary) = summary.as_ref()
        && summary.state == SessionState::Running
    {
        return Ok(false);
    }
    let fleetd_last_seq = summary.as_ref().and_then(|summary| summary.last_seq);
    let fleetd_exit_code = summary.as_ref().and_then(|summary| summary.exit_code);

    let slot = {
        let mut sessions = shared.sessions.lock();
        // Re-check under the lock: a spawn may have claimed the key while
        // ListSessions was in flight. That slot is live; leave it.
        match sessions.get(session_id) {
            Some(slot) if slot.started.is_some() => return Ok(false),
            _ => sessions.remove(session_id),
        }
    };
    match slot {
        Some(mut slot) => {
            tracing::warn!(
                %session_id,
                fleetd_state = ?summary.as_ref().map(|summary| summary.state),
                slot_last_seq = slot.last_seq,
                ?fleetd_last_seq,
                "releasing a stale supervision key: fleetd no longer holds a live \
                 session under it; publishing the dead session's terminal state"
            );
            slot.commands = commands.clone();
            slot.last_seq = slot.last_seq.max(fleetd_last_seq.unwrap_or(0));
            let exit_code = slot.exit_after_replay.flatten().or(fleetd_exit_code);
            slot.finish(
                session_id,
                exit_code,
                "\n[blackbox] supervision key released: fleetd no longer holds a live \
                 session for this task; the worker is gone."
                    .to_string(),
            );
        }
        None => {
            if summary.is_some() {
                tracing::info!(
                    %session_id,
                    ?fleetd_last_seq,
                    "acking a terminal fleetd session nobody claimed so its key is freed"
                );
                let _ = commands.send(DaemonToFleetd::EventAck {
                    session_id: session_id.to_string(),
                    through_seq: fleetd_last_seq.unwrap_or(0),
                });
            }
        }
    }
    Ok(true)
}

/// Ask fleetd what it is holding, reattach everything the task store knows
/// about, and replay each from the daemon's own durable cursor.
///
/// Slots that survived the previous connection are reconciled against
/// fleetd's answer rather than trusted: their command sender is refreshed
/// (a terminal ack on the old, closed sender would be lost and fleetd would
/// keep the key forever), a session fleetd no longer holds is dead and its
/// slot is finished, a session fleetd now reports `Exited` whose exit we
/// never saw is marked terminal-after-replay, and an interrupted replay is
/// re-issued from the slot's cursor. Without this a daemon reconnect leaves
/// dead keys registered as live.
async fn readopt_live_sessions(shared: &Arc<Shared>, lane: &Lane) -> anyhow::Result<()> {
    let commands = &lane.commands;
    let summaries = list_sessions(shared, lane).await?;

    let mut replays: Vec<(String, u64)> = Vec::new();
    let mut finished: Vec<(String, SessionSlot)> = Vec::new();
    {
        let mut sessions = shared.sessions.lock();
        let reported: HashMap<&str, &SessionSummary> = summaries
            .iter()
            .map(|summary| (summary.session_id.as_str(), summary))
            .collect();
        let known: Vec<String> = sessions.keys().cloned().collect();
        for session_id in known {
            let Some(slot) = sessions.get_mut(&session_id) else {
                continue;
            };
            slot.commands = commands.clone();
            if slot.started.is_some() {
                // An in-flight spawn on this connection; its own ack path
                // owns the slot.
                continue;
            }
            match reported.get(session_id.as_str()) {
                None => {
                    if let Some(slot) = sessions.remove(&session_id) {
                        finished.push((session_id, slot));
                    }
                }
                Some(summary) => {
                    if summary.state == SessionState::Exited && slot.exit_after_replay.is_none() {
                        slot.exit_after_replay = Some(summary.exit_code);
                    }
                    if slot.exit_after_replay.is_some() {
                        replays.push((session_id, slot.last_seq));
                    }
                }
            }
        }
    }
    for (session_id, slot) in finished {
        tracing::warn!(
            %session_id,
            "fleetd no longer holds this session; publishing terminal state for its slot"
        );
        slot.finish(
            &session_id,
            None,
            "\n[blackbox] fleetd no longer holds this session (fleetd restarted or \
             forgot it); the worker is gone."
                .to_string(),
        );
    }
    for (session_id, from_seq) in replays {
        tracing::info!(
            %session_id,
            from_seq,
            "re-issuing replay for an exited session whose terminal state is still pending"
        );
        let _ = commands.send(DaemonToFleetd::ReplayFrom {
            session_id,
            from_seq,
        });
    }

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
        workspace_id: summary.workspace_id.clone(),
        workspace_scope: summary.workspace_scope.clone(),
        workspace_binding_token: summary.workspace_binding_token.clone(),
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
    let FleetdEndpoint::Unix(socket) = &config.endpoint else {
        anyhow::bail!("remote fleetd is never auto-started")
    };
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
        if UnixStream::connect(socket).await.is_ok() {
            tracing::info!(?pid, "fleetd is listening");
            return Ok(());
        }
        tokio::time::sleep(AUTOSTART_POLL_INTERVAL).await;
    }
    anyhow::bail!(
        "started fleetd ({}) but its socket {} did not come up within {}s",
        binary.display(),
        socket.display(),
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

    use std::sync::atomic::AtomicUsize;

    use tokio::net::UnixListener;

    /// How a scripted fleetd answers `ReplayFrom`.
    #[derive(Debug, Clone, Copy)]
    enum ReplayScript {
        /// Never answer: the daemon must not depend on a terminator arriving.
        Silent,
        /// The log window does not reach the requested cursor.
        Unavailable { earliest: u64, latest: u64 },
    }

    /// How the fake treats `InspectWorkspace` requests.
    #[derive(Clone, Copy)]
    enum InspectionScript {
        /// Answer every inspection with `Unmanaged`.
        Answer,
        /// Never answer: the daemon-side deadline must fire.
        Silent,
        /// Stay silent on the first accepted connection and answer on every
        /// later one: the shape of a dead link healed by one redial.
        SilentFirstConnection,
    }

    /// Per-lane behavior knobs for [`FakeFleetd`].
    #[derive(Clone, Copy)]
    struct Script {
        /// Answer `ListSessions`? `false` starves re-adoption and the
        /// heartbeat alike, the shape of a silently dead peer.
        answer_lists: bool,
        inspections: InspectionScript,
    }

    impl Default for Script {
        fn default() -> Self {
            Self {
                answer_lists: true,
                inspections: InspectionScript::Answer,
            }
        }
    }

    /// A scripted fleetd on a real Unix socket. It speaks the real wire
    /// contract (handshake, bearer gate, generation-stamped envelopes) but
    /// its registry is a plain list the test seeds, so every liveness answer
    /// is exact. Ack-driven GC mirrors fleetd's own rule: an exited session
    /// is forgotten once acked through its last seq.
    struct FakeFleetd {
        sessions: Arc<Mutex<Vec<SessionSummary>>>,
        acks: Arc<Mutex<Vec<(String, u64)>>>,
        spawns: Arc<Mutex<Vec<String>>>,
        replays: Arc<Mutex<Vec<(String, u64)>>>,
        lists: Arc<AtomicUsize>,
        inspections: Arc<Mutex<Vec<String>>>,
        /// Connections that passed the bearer gate. The executor's Unix dial
        /// makes a probe connect first, so raw accept counts overcount real
        /// connections; scripts and asserts key on this ordinal instead.
        authenticated: Arc<AtomicUsize>,
        replay: ReplayScript,
        script: Script,
    }

    impl FakeFleetd {
        fn serve(
            state_dir: &Path,
            sessions: Vec<SessionSummary>,
            replay: ReplayScript,
        ) -> Arc<Self> {
            Self::serve_with(state_dir, sessions, replay, Script::default())
        }

        fn serve_with(
            state_dir: &Path,
            sessions: Vec<SessionSummary>,
            replay: ReplayScript,
            script: Script,
        ) -> Arc<Self> {
            let token = ServiceToken::load_or_create(&state_dir.join(TOKEN_FILE)).unwrap();
            let listener = UnixListener::bind(state_dir.join(SOCKET_FILE)).unwrap();
            let fake = Arc::new(Self {
                sessions: Arc::new(Mutex::new(sessions)),
                acks: Arc::new(Mutex::new(Vec::new())),
                spawns: Arc::new(Mutex::new(Vec::new())),
                replays: Arc::new(Mutex::new(Vec::new())),
                lists: Arc::new(AtomicUsize::new(0)),
                inspections: Arc::new(Mutex::new(Vec::new())),
                authenticated: Arc::new(AtomicUsize::new(0)),
                replay,
                script,
            });
            let served = fake.clone();
            tokio::spawn(async move {
                let mut generation = 0u64;
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    generation += 1;
                    let fake = served.clone();
                    let token = token.clone();
                    tokio::spawn(async move {
                        let _ = fake.serve_connection(stream, generation, token).await;
                    });
                }
            });
            fake
        }

        async fn serve_connection(
            &self,
            stream: UnixStream,
            generation: u64,
            token: ServiceToken,
        ) -> anyhow::Result<()> {
            let (mut io, _hello, _welcome) = bro_rpc::accept(
                stream,
                build_identity(),
                vec![FLEETD_PROTOCOL_VERSION],
                generation,
                HandshakeOptions::default(),
            )
            .await?;
            let binding = io.binding();
            let mut counter = 0u64;
            let first = io.read_envelope::<DaemonToFleetd>().await?.body;
            let DaemonToFleetd::Authenticate { token: presented } = first else {
                anyhow::bail!("first message must authenticate");
            };
            assert!(token.verify(presented.expose_secret()));
            let ordinal = self.authenticated.fetch_add(1, Ordering::SeqCst) + 1;
            reply(
                &mut io,
                binding,
                generation,
                &mut counter,
                FleetdToDaemon::Ready {
                    connection_generation: generation,
                },
            )
            .await?;

            loop {
                let body = io.read_envelope::<DaemonToFleetd>().await?.body;
                match body {
                    DaemonToFleetd::ListSessions => {
                        self.lists.fetch_add(1, Ordering::SeqCst);
                        if !self.script.answer_lists {
                            continue;
                        }
                        let sessions = self.sessions.lock().clone();
                        reply(
                            &mut io,
                            binding,
                            generation,
                            &mut counter,
                            FleetdToDaemon::Sessions { sessions },
                        )
                        .await?;
                    }
                    DaemonToFleetd::InspectWorkspace { request_id, .. } => {
                        self.inspections.lock().push(request_id.clone());
                        let answer = match self.script.inspections {
                            InspectionScript::Answer => true,
                            InspectionScript::Silent => false,
                            InspectionScript::SilentFirstConnection => ordinal > 1,
                        };
                        if answer {
                            reply(
                                &mut io,
                                binding,
                                generation,
                                &mut counter,
                                FleetdToDaemon::WorkspaceInspected {
                                    request_id,
                                    outcome: WorkspaceInspectionOutcome::Unmanaged,
                                },
                            )
                            .await?;
                        }
                    }
                    DaemonToFleetd::Spawn { spec } => {
                        self.spawns.lock().push(spec.session_id.clone());
                        let duplicate = self
                            .sessions
                            .lock()
                            .iter()
                            .any(|summary| summary.session_id == spec.session_id);
                        if duplicate {
                            reply(
                                &mut io,
                                binding,
                                generation,
                                &mut counter,
                                FleetdToDaemon::Error {
                                    session_id: Some(spec.session_id.clone()),
                                    code: FLEETD_DUPLICATE_SESSION_CODE.to_string(),
                                    message: "a session with this id is already registered"
                                        .to_string(),
                                },
                            )
                            .await?;
                            continue;
                        }
                        self.sessions.lock().push(summary(
                            &spec.session_id,
                            SessionState::Running,
                            None,
                            None,
                        ));
                        reply(
                            &mut io,
                            binding,
                            generation,
                            &mut counter,
                            FleetdToDaemon::SessionStarted {
                                session_id: spec.session_id.clone(),
                                task_id: spec.task_id.clone(),
                                workspace_id: None,
                                pid: Some(4242),
                            },
                        )
                        .await?;
                    }
                    DaemonToFleetd::EventAck {
                        session_id,
                        through_seq,
                    } => {
                        self.acks.lock().push((session_id.clone(), through_seq));
                        self.sessions.lock().retain(|summary| {
                            !(summary.session_id == session_id
                                && summary.state == SessionState::Exited
                                && through_seq >= summary.last_seq.unwrap_or(0))
                        });
                    }
                    DaemonToFleetd::ReplayFrom {
                        session_id,
                        from_seq,
                    } => {
                        self.replays.lock().push((session_id.clone(), from_seq));
                        if let ReplayScript::Unavailable { earliest, latest } = self.replay {
                            reply(
                                &mut io,
                                binding,
                                generation,
                                &mut counter,
                                FleetdToDaemon::ReplayUnavailable {
                                    session_id,
                                    requested_from: from_seq,
                                    earliest_available: earliest,
                                    latest_available: latest,
                                },
                            )
                            .await?;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    async fn reply(
        io: &mut NegotiatedIo<UnixStream>,
        binding: bro_rpc::ConnectionBinding,
        generation: u64,
        counter: &mut u64,
        body: FleetdToDaemon,
    ) -> Result<(), bro_rpc::RpcError> {
        *counter += 1;
        let envelope = Envelope {
            protocol_version: binding.protocol_version,
            connection_generation: binding.connection_generation,
            message_id: format!("fake-{generation}-{}", *counter),
            reply_to: None,
            body,
        };
        io.write_envelope(&envelope).await
    }

    fn summary(
        session_id: &str,
        state: SessionState,
        last_seq: Option<u64>,
        exit_code: Option<i32>,
    ) -> SessionSummary {
        SessionSummary {
            session_id: session_id.to_string(),
            task_id: format!("task-{session_id}"),
            workspace_id: None,
            workspace_scope: None,
            workspace_binding_token: None,
            pid: Some(4242),
            state,
            last_seq,
            event_log_path: PathBuf::from(format!("/nowhere/{session_id}.events.jsonl")),
            exit_code,
        }
    }

    fn spec(session_id: &str) -> WorkerSpawnSpec {
        WorkerSpawnSpec {
            task_id: format!("task-{session_id}"),
            session_id: session_id.to_string(),
            workspace_id: None,
            workspace_scope: None,
            provider: bro_core::Provider::Glm,
            bin_override: None,
            argv: vec![],
            cwd: None,
            env: Default::default(),
            env_unset: vec![],
            initial_messages: vec![],
            bro_home: PathBuf::from("/nowhere"),
            event_log_path: PathBuf::from(format!("/nowhere/{session_id}.events.jsonl")),
        }
    }

    /// Plant a slot the way the client leaves one behind, returning the
    /// outcome receiver its terminal waiter would be holding.
    fn plant_slot(
        executor: &FleetdExecutor,
        session_id: &str,
        exit_after_replay: Option<Option<i32>>,
        last_seq: u64,
        in_flight: bool,
    ) -> oneshot::Receiver<WorkerOutcome> {
        let (events_tx, _events_rx) = mpsc::unbounded_channel::<String>();
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let (started_tx, _started_rx) = oneshot::channel();
        // A sender from a connection that no longer exists, exactly what a
        // slot that survived a reconnect holds.
        let (dead_commands, _) = mpsc::unbounded_channel::<DaemonToFleetd>();
        executor.shared.sessions.lock().insert(
            session_id.to_string(),
            SessionSlot {
                events: events_tx,
                started: in_flight.then_some(started_tx),
                outcome: Some(outcome_tx),
                exit_after_replay,
                last_seq,
                commands: dead_commands,
            },
        );
        outcome_rx
    }

    fn slot_present(executor: &FleetdExecutor, session_id: &str) -> bool {
        executor.shared.sessions.lock().contains_key(session_id)
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// The incident shape: a restart re-adopted a session fleetd reports as
    /// already exited, its replay never terminated, and the slot stayed
    /// registered as live. `bro_resume` on the same provider session id
    /// (the same supervision key) must spawn, publishing the dead session's
    /// terminal state and acking fleetd so it releases the key too.
    #[tokio::test]
    async fn restart_orphaned_key_allows_resume() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeFleetd::serve(
            dir.path(),
            vec![summary(
                "sess-orphan",
                SessionState::Exited,
                Some(40),
                Some(1),
            )],
            ReplayScript::Silent,
        );
        let executor = FleetdExecutor::new(FleetdConfig::in_state_dir(dir.path()));
        let outcome = plant_slot(&executor, "sess-orphan", Some(Some(1)), 10, false);

        let handle = executor.spawn(spec("sess-orphan")).await.unwrap();
        assert_eq!(handle.pid, Some(4242));

        let outcome = tokio::time::timeout(Duration::from_secs(5), outcome)
            .await
            .expect("dead session publishes terminal state")
            .unwrap();
        assert_eq!(outcome.exit_code, Some(1));
        assert!(outcome.stderr.contains("supervision key released"));
        assert!(
            fake.acks.lock().contains(&("sess-orphan".to_string(), 40)),
            "fleetd is acked through its own last seq so it can GC the key: {:?}",
            fake.acks.lock()
        );
        assert_eq!(fake.spawns.lock().as_slice(), ["sess-orphan"]);
        assert!(
            slot_present(&executor, "sess-orphan"),
            "the resume owns the key now"
        );
    }

    /// The real double-dispatch protection: fleetd still reports a running
    /// worker under the key, so a second dispatch is refused and the live
    /// slot is untouched.
    #[tokio::test]
    async fn genuinely_live_key_still_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeFleetd::serve(
            dir.path(),
            vec![summary("sess-live", SessionState::Running, Some(40), None)],
            ReplayScript::Silent,
        );
        let executor = FleetdExecutor::new(FleetdConfig::in_state_dir(dir.path()));
        let mut outcome = plant_slot(&executor, "sess-live", None, 40, false);

        let error = match executor.spawn(spec("sess-live")).await {
            Ok(_) => panic!("a live key must be refused"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("genuinely live"), "{error}");
        assert!(error.contains("refusing to multiplex"), "{error}");
        assert!(fake.spawns.lock().is_empty(), "no spawn reaches fleetd");
        assert!(
            fake.acks.lock().is_empty(),
            "a live session is never acked away"
        );
        assert!(slot_present(&executor, "sess-live"));
        assert!(
            outcome.try_recv().is_err(),
            "the live slot's outcome is untouched"
        );
    }

    /// A slot still awaiting its own `SessionStarted` is an in-flight
    /// dispatch and is never released, whatever fleetd's list says.
    #[tokio::test]
    async fn in_flight_dispatch_key_refuses_without_release() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeFleetd::serve(dir.path(), vec![], ReplayScript::Silent);
        let executor = FleetdExecutor::new(FleetdConfig::in_state_dir(dir.path()));
        let _outcome = plant_slot(&executor, "sess-inflight", None, 0, true);

        let error = match executor.spawn(spec("sess-inflight")).await {
            Ok(_) => panic!("an in-flight key must be refused"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("refusing to multiplex"), "{error}");
        assert!(fake.spawns.lock().is_empty());
        assert!(slot_present(&executor, "sess-inflight"));
    }

    /// fleetd's own registry can hold the key after the daemon released its
    /// slot (an exited session nobody acked). The duplicate refusal is
    /// repaired by proving the entry is terminal, acking it, and retrying.
    #[tokio::test]
    async fn unacked_terminal_fleetd_entry_is_acked_and_spawn_retried() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeFleetd::serve(
            dir.path(),
            vec![summary(
                "sess-unacked",
                SessionState::Exited,
                Some(7),
                Some(0),
            )],
            ReplayScript::Silent,
        );
        let executor = FleetdExecutor::new(FleetdConfig::in_state_dir(dir.path()));

        let handle = executor.spawn(spec("sess-unacked")).await.unwrap();
        assert_eq!(handle.pid, Some(4242));
        assert_eq!(
            fake.spawns.lock().as_slice(),
            ["sess-unacked", "sess-unacked"],
            "first attempt refused as duplicate, second succeeds"
        );
        assert!(fake.acks.lock().contains(&("sess-unacked".to_string(), 7)));
    }

    /// Reconnect reconciliation: a session fleetd no longer holds (fleetd
    /// restarted, or forgot it) is dead, so its surviving slot is finished
    /// instead of staying registered as live forever.
    #[tokio::test]
    async fn session_fleetd_no_longer_holds_is_finished_on_reconnect() {
        let dir = tempfile::tempdir().unwrap();
        let _fake = FakeFleetd::serve(dir.path(), vec![], ReplayScript::Silent);
        let executor = FleetdExecutor::new(FleetdConfig::in_state_dir(dir.path()));
        let outcome = plant_slot(&executor, "sess-forgotten", None, 12, false);

        executor.lane().await.unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(5), outcome)
            .await
            .expect("terminal state published")
            .unwrap();
        assert_eq!(outcome.exit_code, None);
        assert!(outcome.stderr.contains("fleetd no longer holds"));
        assert!(!slot_present(&executor, "sess-forgotten"));
    }

    /// Reconnect reconciliation: an interrupted replay for a dead session is
    /// re-issued on the NEW connection, and when fleetd's window does not
    /// reach the cursor the slot is finished (acked through the end of the
    /// window) rather than waiting for a `ReplayComplete` that never comes.
    #[tokio::test]
    async fn replay_unavailable_terminates_a_dead_session_slot() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeFleetd::serve(
            dir.path(),
            vec![summary(
                "sess-gap",
                SessionState::Exited,
                Some(120),
                Some(2),
            )],
            ReplayScript::Unavailable {
                earliest: 100,
                latest: 120,
            },
        );
        let executor = FleetdExecutor::new(FleetdConfig::in_state_dir(dir.path()));
        let outcome = plant_slot(&executor, "sess-gap", Some(Some(2)), 10, false);

        executor.lane().await.unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(5), outcome)
            .await
            .expect("terminal state published")
            .unwrap();
        assert_eq!(outcome.exit_code, Some(2));
        settle().await;
        assert_eq!(
            fake.replays.lock().as_slice(),
            [("sess-gap".to_string(), 10)]
        );
        assert!(fake.acks.lock().contains(&("sess-gap".to_string(), 120)));
        assert!(!slot_present(&executor, "sess-gap"));
    }

    /// A slot re-adopted as live whose exit was lost across a disconnect is
    /// reconciled from fleetd's answer: it becomes terminal-after-replay and
    /// its replay is re-issued from the slot's cursor.
    #[tokio::test]
    async fn exit_lost_across_disconnect_is_reconciled_from_fleetd() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeFleetd::serve(
            dir.path(),
            vec![summary(
                "sess-late-exit",
                SessionState::Exited,
                Some(30),
                Some(0),
            )],
            ReplayScript::Silent,
        );
        let executor = FleetdExecutor::new(FleetdConfig::in_state_dir(dir.path()));
        let _outcome = plant_slot(&executor, "sess-late-exit", None, 25, false);

        executor.lane().await.unwrap();
        settle().await;

        assert_eq!(
            fake.replays.lock().as_slice(),
            [("sess-late-exit".to_string(), 25)]
        );
        let exit_after_replay = executor
            .shared
            .sessions
            .lock()
            .get("sess-late-exit")
            .map(|slot| slot.exit_after_replay);
        assert_eq!(exit_after_replay, Some(Some(Some(0))));
    }

    #[test]
    fn paths_hang_off_the_state_dir() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config = FleetdConfig::in_state_dir(&root);
        assert_eq!(
            config.endpoint,
            FleetdEndpoint::Unix(root.join("fleetd.sock"))
        );
        assert_eq!(config.token, root.join("fleetd.token"));
        assert_eq!(config.state_dir, root);
    }

    /// Prod and dev daemons have different state dirs, so they must never
    /// share a supervisor. Mirrors the assertion on fleetd's own side.
    #[test]
    fn distinct_state_dirs_yield_distinct_sockets() {
        let prod = FleetdConfig::in_state_dir("/state/prod");
        let dev = FleetdConfig::in_state_dir("/state/dev");
        assert_ne!(prod.endpoint, dev.endpoint);
        assert_ne!(prod.token, dev.token);
    }

    #[test]
    fn fleetd_owns_provider_binary_resolution() {
        let directory = tempfile::tempdir().unwrap();
        let executor = FleetdExecutor::new(FleetdConfig::in_state_dir(directory.path()));
        assert_eq!(
            executor.provider_binary_location(),
            super::super::executor::ProviderBinaryLocation::ExecutorHost
        );
        assert_eq!(
            super::super::executor::LocalExecutor.provider_binary_location(),
            super::super::executor::ProviderBinaryLocation::DaemonHost
        );
    }

    #[test]
    fn remote_endpoint_requires_an_explicit_token_file() {
        let error = FleetdConfig::resolve(
            "/state/cage",
            Some("tcp://fleet.tailnet:7265"),
            None,
            Some(Path::new("/worker/home")),
            Some(Path::new("/worker/state/bro")),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires daemon.fleetd_token_file")
        );

        let config = FleetdConfig::resolve(
            "/state/cage",
            Some("tcp://fleet.tailnet:7265"),
            Some(Path::new("/run/secrets/fleetd-token")),
            Some(Path::new("/worker/home")),
            Some(Path::new("/worker/state/bro")),
        )
        .unwrap();
        assert_eq!(
            config.endpoint,
            FleetdEndpoint::Tcp("fleet.tailnet:7265".to_string())
        );
        assert_eq!(config.token, PathBuf::from("/run/secrets/fleetd-token"));
        assert!(config.binary.is_none());
        assert_eq!(
            config.worker_locality,
            Some(WorkerLocality {
                home: PathBuf::from("/worker/home"),
                bro_home: PathBuf::from("/worker/state/bro"),
            })
        );
    }

    #[test]
    fn remote_endpoint_requires_absolute_worker_roots() {
        for (home, bro_home) in [
            (None, Some(Path::new("/worker/state/bro"))),
            (Some(Path::new("/worker/home")), None),
            (
                Some(Path::new("relative/home")),
                Some(Path::new("/worker/state/bro")),
            ),
            (
                Some(Path::new("/worker/home")),
                Some(Path::new("relative/state/bro")),
            ),
        ] {
            assert!(
                FleetdConfig::resolve(
                    "/state/cage",
                    Some("tcp://fleet.tailnet:7265"),
                    Some(Path::new("/run/secrets/fleetd-token")),
                    home,
                    bro_home,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn explicit_token_without_endpoint_is_rejected() {
        let error = FleetdConfig::resolve(
            "/state/local",
            None,
            Some(Path::new("/run/secrets/ambiguous")),
            None,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("require daemon.fleetd_endpoint"));
    }

    #[test]
    fn malformed_remote_endpoints_fail_during_startup_resolution() {
        for endpoint in [
            "tcp://host",
            "tcp://host:",
            "tcp://host:0",
            "tcp://host:not-a-port",
            "tcp://2001:db8::1:7265",
            "tcp://[2001:db8::1]",
        ] {
            assert!(
                FleetdConfig::resolve(
                    "/state/cage",
                    Some(endpoint),
                    Some(Path::new("/run/secrets/fleetd-token")),
                    Some(Path::new("/worker/home")),
                    Some(Path::new("/worker/state/bro")),
                )
                .is_err(),
                "{endpoint} must fail before daemon startup completes"
            );
        }
        FleetdConfig::resolve(
            "/state/cage",
            Some("tcp://[2001:db8::1]:7265"),
            Some(Path::new("/run/secrets/fleetd-token")),
            Some(Path::new("/worker/home")),
            Some(Path::new("/worker/state/bro")),
        )
        .expect("bracketed IPv6 with a nonzero port is valid");
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
            list_waiters: Mutex::new(VecDeque::new()),
            workspace_waiters: Mutex::new(HashMap::new()),
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
            1,
        );
        handle_message(
            &shared,
            FleetdToDaemon::SessionExited {
                session_id: "sess-1".to_string(),
                exit_code: Some(0),
                stderr_tail: "warn\n".to_string(),
            },
            1,
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
            list_waiters: Mutex::new(VecDeque::new()),
            workspace_waiters: Mutex::new(HashMap::new()),
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
            1,
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
            list_waiters: Mutex::new(VecDeque::new()),
            workspace_waiters: Mutex::new(HashMap::new()),
            message_counter: AtomicU64::new(0),
        };
        let first = shared.next_message_id();
        let second = shared.next_message_id();
        assert!(!first.is_empty());
        assert_ne!(first, second);
    }

    /// Timings small enough to exercise the liveness paths in-test. The
    /// probe deadline stays above the heartbeat interval so a healthy but
    /// merely slow fake is never misread as dead.
    fn fast_config(dir: &Path) -> FleetdConfig {
        let mut config = FleetdConfig::in_state_dir(dir);
        config.heartbeat_interval = Duration::from_millis(40);
        config.list_sessions_timeout = Duration::from_millis(120);
        config.inspection_timeout = Duration::from_millis(150);
        config
    }

    fn inspection_request() -> WorkspaceInspectionRequest {
        WorkspaceInspectionRequest {
            cwd: "/somewhere/checkout".to_string(),
            candidate_scopes: Vec::new(),
        }
    }

    /// The 2026-08-27 incident shape: the peer stops serving round-trips (a
    /// worker host reboot sends no FIN, so reads block and writes keep
    /// landing in the kernel buffer). The heartbeat must notice within its
    /// cadence and cancel the connection, and the next caller must get a
    /// fresh dial instead of the corpse.
    #[tokio::test]
    async fn heartbeat_drops_a_connection_that_stops_answering() {
        let dir = tempfile::tempdir().unwrap();
        let _fake = FakeFleetd::serve_with(
            dir.path(),
            Vec::new(),
            ReplayScript::Silent,
            Script {
                answer_lists: false,
                inspections: InspectionScript::Answer,
            },
        );
        let executor = FleetdExecutor::new(fast_config(dir.path()));

        let first = executor.lane().await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !first.cancel.is_cancelled() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "heartbeat never dropped the unanswered connection"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let second = executor.lane().await.unwrap();
        assert_ne!(
            first.generation, second.generation,
            "a cancelled connection must not be handed out again"
        );
    }

    /// An unanswered inspection proves the connection dead: the attempt must
    /// cancel it and retry once on a fresh dial, and when that one also goes
    /// unanswered, fail with the deadline error while leaving no corpse
    /// installed (the next lane dials a third connection).
    #[tokio::test]
    async fn unanswered_inspection_invalidates_the_connection_and_retries_once() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeFleetd::serve_with(
            dir.path(),
            Vec::new(),
            ReplayScript::Silent,
            Script {
                answer_lists: true,
                inspections: InspectionScript::Silent,
            },
        );
        let mut config = fast_config(dir.path());
        // Keep the heartbeat out of the picture: this test drives
        // invalidation through the inspection deadline alone.
        config.heartbeat_interval = Duration::from_secs(3600);
        let executor = FleetdExecutor::new(config);

        let error = executor
            .inspect_workspace(inspection_request())
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("did not answer workspace inspection"),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            fake.inspections.lock().len(),
            2,
            "exactly one retry on a fresh connection"
        );
        assert_eq!(
            fake.authenticated.load(Ordering::SeqCst),
            2,
            "each attempt ran on its own connection"
        );
        executor.lane().await.unwrap();
        assert_eq!(
            fake.authenticated.load(Ordering::SeqCst),
            3,
            "both failed attempts left cancelled connections behind, so the next lane redials"
        );
    }

    /// The healed-by-redial shape: the stale connection swallows the first
    /// inspection, the retry's fresh connection answers, and the caller sees
    /// a success instead of a failed dispatch.
    #[tokio::test]
    async fn inspection_recovers_on_a_fresh_connection() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeFleetd::serve_with(
            dir.path(),
            Vec::new(),
            ReplayScript::Silent,
            Script {
                answer_lists: true,
                inspections: InspectionScript::SilentFirstConnection,
            },
        );
        let mut config = fast_config(dir.path());
        config.heartbeat_interval = Duration::from_secs(3600);
        let executor = FleetdExecutor::new(config);

        let outcome = executor
            .inspect_workspace(inspection_request())
            .await
            .unwrap();
        assert_eq!(outcome, WorkspaceInspectionOutcome::Unmanaged);
        assert_eq!(
            fake.inspections.lock().len(),
            2,
            "first attempt timed out, retry was answered"
        );
    }
}

include!("fleetd_smoke.rs");
