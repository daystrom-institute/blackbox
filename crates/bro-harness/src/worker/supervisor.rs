//! Persistent worker supervisor for one harness session.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bro_core::{CommandId, SessionId, TaskId, WorkerId};
use bro_protocol::{
    AuthenticationProof, BuildIdentity, CommandOutcome, CommandOutcomeAck, DrainCompletion,
    EventAck, Heartbeat, LeaseGrant, LeaseRenewal, ProtocolError, ProtocolErrorCode, ReplayRequest,
    ReplayUnavailable, SessionPolicy, ShutdownCompletion, ShutdownMode, WorkerCommand,
    WorkerCommandKind, WorkerFeature, WorkerHello, WorkerLifecycleState, WorkerMessage,
    WorkerStatus,
};
use bro_rpc::{MessagePriority, PeerConfig, PeerHandle, RpcError, RpcPeer};
use serde_json::{Value, json};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, oneshot, watch};
use uuid::Uuid;

use crate::agent_loop::{ServicePolicyUpdate, SessionInput, SessionInputSender};
use crate::cli::Cli;
use crate::event_log::{CommittedEvent, EventLog, EventLogHealth, ReplayDiagnostic, ReplayLimits};
use crate::session_environment::DaemonSessionEnvironment;

use super::capability_rpc::{RpcCapabilityClient, RpcCapabilityConnection};
use super::command_journal::{CommandDisposition, CommandJournal, CommandJournalError};

const EVENT_REPLAY_CHUNK_EVENTS: usize = 64;
const EVENT_REPLAY_CHUNK_BYTES: u64 = 512 * 1024;
const EVENT_SEND_RETRY: Duration = Duration::from_millis(5);
const EVENT_LOG_HEALTH_POLL: Duration = Duration::from_millis(50);
const TERMINAL_ACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct WorkerIdentity {
    worker_id: WorkerId,
    task_id: TaskId,
    session_id: SessionId,
    build: BuildIdentity,
    protocol_versions: Vec<u16>,
}

impl WorkerIdentity {
    fn hello(
        &self,
        proof: AuthenticationProof,
        last_local_event_seq: u64,
        last_fleet_command_seq: u64,
    ) -> WorkerHello {
        let mut hello = WorkerHello {
            protocol_versions: self.protocol_versions.clone(),
            worker_build: self.build.clone(),
            worker_id: self.worker_id.clone(),
            task_id: self.task_id.clone(),
            session_id: self.session_id.clone(),
            bootstrap_or_resume_proof: proof,
            last_local_event_seq,
            last_fleet_command_seq,
            worker_capabilities: vec!["persistent_session_runtime".to_string()],
        };
        hello.set_offered_protocol_features(required_features());
        hello
    }

    fn status(
        &self,
        binding: bro_rpc::ConnectionBinding,
        last_local_event_seq: u64,
        last_fleet_command_seq: u64,
        state: WorkerLifecycleState,
    ) -> WorkerStatus {
        WorkerStatus {
            worker_id: self.worker_id.clone(),
            task_id: self.task_id.clone(),
            session_id: self.session_id.clone(),
            worker_build: self.build.clone(),
            protocol_version: binding.protocol_version,
            connection_generation: binding.connection_generation,
            last_local_event_seq,
            last_fleet_command_seq,
            state,
        }
    }
}

#[derive(Debug, Clone)]
struct SessionEnd {
    error: Option<String>,
}

struct StableSession {
    input: Option<SessionInputSender>,
    abort: tokio::task::AbortHandle,
    done: watch::Receiver<Option<SessionEnd>>,
    event_log: Arc<EventLog>,
    committed: broadcast::Receiver<CommittedEvent>,
    pending_admissions: VecDeque<WorkerCommand>,
    pending_turns: VecDeque<PendingTurnGroup>,
    pending_effects: VecDeque<PendingEffect>,
    terminal_command: Option<WorkerCommand>,
    terminal_outcome: Option<CommandOutcome>,
    forced_by_deadline: bool,
    sent_through_event_seq: u64,
    acked_through_event_seq: u64,
    observed_through_event_seq: u64,
}

impl StableSession {
    fn send(&self, input: SessionInput) -> std::result::Result<(), ()> {
        self.input.as_ref().ok_or(())?.send(input).map_err(|_| ())
    }

    fn begin_terminal(&mut self, command: WorkerCommand, force: bool) {
        if force {
            let _ = self.send(SessionInput::Control {
                subtype: "interrupt".to_string(),
                request_id: Some(command.command_id.as_str().to_string()),
                raw: json!({"type":"control_request","subtype":"interrupt"}),
            });
        }
        self.terminal_command = Some(command);
        if force {
            self.input.take();
            self.abort.abort();
        } else {
            self.close_input_at_safe_boundary();
        }
    }

    fn close_input_at_safe_boundary(&mut self) {
        if self.terminal_command.is_some()
            && self.pending_turns.is_empty()
            && self.pending_admissions.is_empty()
            && self.pending_effects.is_empty()
        {
            self.input.take();
        }
    }

    fn is_done(&self) -> bool {
        self.done.borrow().is_some()
    }
}

struct PendingTurnGroup {
    commands: Vec<WorkerCommand>,
    result: Option<Value>,
}

struct PendingEffect {
    command: WorkerCommand,
    dispatched: bool,
    completion: Option<Value>,
}

#[derive(Debug)]
struct QueuedCommand {
    envelope: bro_protocol::Envelope,
    command: WorkerCommand,
}

struct CommandInbox {
    control: BTreeMap<u64, QueuedCommand>,
    normal: BTreeMap<u64, QueuedCommand>,
    control_capacity: usize,
    normal_capacity: usize,
}

impl CommandInbox {
    fn new(control_capacity: usize, normal_capacity: usize) -> Result<Self> {
        if control_capacity == 0 || normal_capacity == 0 {
            bail!("worker command queue capacities must be greater than zero");
        }
        Ok(Self {
            control: BTreeMap::new(),
            normal: BTreeMap::new(),
            control_capacity,
            normal_capacity,
        })
    }

    fn push(&mut self, queued: QueuedCommand) -> std::result::Result<(), &'static str> {
        let priority = is_priority_command(&queued.command.command);
        let queue = if priority {
            if self.control.len() >= self.control_capacity {
                return Err("control");
            }
            &mut self.control
        } else {
            if self.normal.len() >= self.normal_capacity {
                return Err("normal");
            }
            &mut self.normal
        };
        queue.entry(queued.command.command_seq).or_insert(queued);
        Ok(())
    }

    fn pop_sequence(&mut self, sequence: u64) -> Option<QueuedCommand> {
        self.control
            .remove(&sequence)
            .or_else(|| self.normal.remove(&sequence))
    }

    fn pop_control_sequence(&mut self, sequence: u64) -> Option<QueuedCommand> {
        self.control.remove(&sequence)
    }
}

struct LeaseState {
    lease_id: String,
    expires_at_unix_ms: u64,
    heartbeat_interval: Duration,
    next_heartbeat: tokio::time::Instant,
}

impl LeaseState {
    fn from_grant(grant: &LeaseGrant) -> Self {
        let interval_ms = grant.heartbeat_interval_ms.max(1);
        Self {
            lease_id: grant.lease_id.clone(),
            expires_at_unix_ms: grant.expires_at_unix_ms,
            heartbeat_interval: Duration::from_millis(interval_ms),
            next_heartbeat: tokio::time::Instant::now() + Duration::from_millis(interval_ms),
        }
    }

    fn renew(&mut self, renewal: LeaseRenewal) -> Result<()> {
        if renewal.lease_id != self.lease_id {
            bail!("fleet renewed a different worker lease");
        }
        self.expires_at_unix_ms = renewal.expires_at_unix_ms;
        let delay = renewal
            .next_heartbeat_due_unix_ms
            .saturating_sub(now_ms())
            .max(1);
        self.next_heartbeat = tokio::time::Instant::now() + Duration::from_millis(delay);
        Ok(())
    }

    fn heartbeat_sent(&mut self) {
        self.next_heartbeat = tokio::time::Instant::now() + self.heartbeat_interval;
    }
}

pub async fn run_worker(mut cli: Cli) -> Result<()> {
    validate_worker_cli(&cli)?;
    let mut session_environment = Some(DaemonSessionEnvironment::take()?);
    let socket = required(&cli.fleet_socket, "--fleet-socket")?;
    let task_id = TaskId::new(required(&cli.task_id, "--task-id")?);
    let session_id = SessionId::new(required(&cli.session_id, "--session-id")?);
    let bootstrap_path = required(&cli.bootstrap_secret_file, "--bootstrap-secret-file")?;
    let reconnect_credential_path = cli
        .worker_reconnect_credential
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate::session::sessions_dir().join(format!("{}.worker-reconnect", session_id.as_str()))
        });
    let proof = read_worker_credential(&bootstrap_path, &reconnect_credential_path).await?;
    let identity = WorkerIdentity {
        worker_id: WorkerId::new(
            cli.worker_id
                .clone()
                .unwrap_or_else(|| format!("worker-{}", Uuid::new_v4())),
        ),
        task_id,
        session_id: session_id.clone(),
        build: BuildIdentity {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_id: env!("BRO_HARNESS_BUILD_ID").to_string(),
        },
        protocol_versions: cli.worker_protocol_versions.clone(),
    };
    let command_path = cli
        .worker_command_journal
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate::session::sessions_dir()
                .join(format!("{}.worker-commands.jsonl", session_id.as_str()))
        });
    let command_journal =
        Arc::new(CommandJournal::open(command_path).context("opening worker command journal")?);
    let (capability_client, capability_connection) = RpcCapabilityClient::new(session_id.clone());
    let mut reconnect_proof = AuthenticationProof::new(proof);
    let mut prior_policy = None;
    let mut stable_session: Option<StableSession> = None;
    let mut first_connection = true;
    let mut backoff = Duration::from_millis(cli.worker_reconnect_initial_ms.max(1));
    let max_backoff = Duration::from_millis(
        cli.worker_reconnect_max_ms
            .max(cli.worker_reconnect_initial_ms)
            .max(1),
    );

    // The initial prompt is fleet command data in worker mode, never an argv
    // side channel. Root package F sends it after replay gating.
    cli.prompt = None;

    loop {
        let lifecycle = if first_connection {
            WorkerLifecycleState::Connecting
        } else {
            WorkerLifecycleState::Reconnecting
        };
        let last_event = stable_session
            .as_ref()
            .map(|session| latest_event_seq(&session.event_log))
            .transpose()?
            .unwrap_or(0);
        let hello = identity.hello(
            reconnect_proof.clone(),
            last_event,
            command_journal.last_command_seq(),
        );
        let stream = match UnixStream::connect(&socket).await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, socket = %socket, "fleet worker socket unavailable");
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(max_backoff);
                first_connection = false;
                continue;
            }
        };
        let (negotiated, welcome) = match bro_rpc::connect_worker_with_options(
            stream,
            hello,
            Default::default(),
        )
        .await
        {
            Ok(value) => value,
            Err(error @ RpcError::HandshakeRejected { .. }) => {
                if is_terminal_handshake_rejection(&error) {
                    return Err(error.into());
                }
                tracing::warn!(%error, "fleet temporarily rejected worker handshake; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(max_backoff);
                first_connection = false;
                continue;
            }
            Err(error @ RpcError::SelectedProtocolNotOffered { .. })
            | Err(error @ RpcError::VersionMismatch) => return Err(error.into()),
            Err(error) => {
                tracing::warn!(%error, "worker handshake failed; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(max_backoff);
                first_connection = false;
                continue;
            }
        };
        backoff = Duration::from_millis(cli.worker_reconnect_initial_ms.max(1));
        persist_reconnect_credential(
            reconnect_credential_path.clone(),
            welcome.reconnect_proof.expose_secret().to_string(),
        )
        .await?;
        reconnect_proof = welcome.reconnect_proof.clone();
        validate_welcome_policy(&welcome.session_policy, &mut prior_policy)?;

        if stable_session.is_none() {
            let services =
                capability_client.reconnectable_session_services(&welcome.session_policy);
            let environment = session_environment
                .take()
                .ok_or_else(|| anyhow!("worker session environment was already consumed"))?;
            stable_session = Some(start_session(cli.clone(), services, environment).await?);
        }
        let session = stable_session.as_mut().expect("session initialized above");
        session
            .send(SessionInput::ServicePolicy(service_policy_update(
                &welcome.session_policy,
                true,
            )?))
            .map_err(|_| anyhow!("session closed before service policy reconciliation"))?;
        if welcome.event_ack > latest_event_seq(&session.event_log)? {
            bail!("fleet acknowledged events the worker has not committed");
        }
        if welcome.event_ack < session.acked_through_event_seq {
            bail!(
                "fleet durable event acknowledgment regressed from {} to {}",
                session.acked_through_event_seq,
                welcome.event_ack
            );
        }
        session.acked_through_event_seq = welcome.event_ack;
        validate_next_command_seq(welcome.next_command_seq, command_journal.next_command_seq())?;

        let mut peer = RpcPeer::spawn(negotiated, PeerConfig::default())?;
        let handle = peer.handle();
        let generation = handle.binding().connection_generation;
        let mut inbox = CommandInbox::new(cli.worker_control_capacity, cli.worker_input_capacity)?;
        let mut lease = LeaseState::from_grant(&welcome.lease);
        let reconnect_result = reconnect_and_replay(
            &mut peer,
            &handle,
            session,
            &mut prior_policy,
            &identity,
            &command_journal,
            &mut inbox,
            welcome.event_ack,
            lifecycle,
            &mut lease,
        )
        .await;
        if let Err(error) = reconnect_result {
            capability_connection.disconnect_generation(generation);
            let current_policy = prior_policy
                .as_ref()
                .ok_or_else(|| anyhow!("worker lost its current service policy"))?;
            let _ = session.send(SessionInput::ServicePolicy(service_policy_update(
                current_policy,
                false,
            )?));
            if let Err(log_error) = ensure_event_log_healthy(&session.event_log) {
                let _ = send_fatal_event_log_error(&handle, &log_error).await;
                handle.shutdown();
                return Err(error.context(log_error));
            }
            tracing::warn!(%error, generation, "worker replay generation failed");
            handle.shutdown();
            first_connection = false;
            continue;
        }

        let current_policy = prior_policy
            .as_ref()
            .ok_or_else(|| anyhow!("worker lost its current service policy"))?;
        capability_connection.activate(handle.clone(), current_policy);
        let active_result = run_active_generation(
            &mut peer,
            &handle,
            session,
            &mut prior_policy,
            &capability_connection,
            &identity,
            &command_journal,
            &mut inbox,
            &mut lease,
        )
        .await;
        capability_connection.disconnect_generation(generation);
        let current_policy = prior_policy
            .as_ref()
            .ok_or_else(|| anyhow!("worker lost its current service policy"))?;
        let _ = session.send(SessionInput::ServicePolicy(service_policy_update(
            current_policy,
            false,
        )?));
        match active_result {
            Ok(GenerationEnd::SessionTerminal) => {
                match finish_terminal_session(
                    session,
                    &command_journal,
                    &mut peer,
                    &identity,
                    generation,
                    &mut lease,
                )
                .await?
                {
                    TerminalFinish::Complete => return Ok(()),
                    TerminalFinish::Reconnect(reason) => {
                        tracing::warn!(%reason, generation, "terminal worker is reconnecting to finish durable closeout");
                        handle.shutdown();
                    }
                }
            }
            Ok(GenerationEnd::Disconnected(reason)) => {
                tracing::warn!(%reason, generation, "fleet worker generation disconnected");
            }
            Err(error) => {
                if let Err(log_error) = ensure_event_log_healthy(&session.event_log) {
                    let _ = send_fatal_event_log_error(&handle, &log_error).await;
                    handle.shutdown();
                    return Err(error.context(log_error));
                }
                tracing::warn!(%error, generation, "fleet worker generation failed");
            }
        }
        first_connection = false;
    }
}

async fn start_session(
    cli: Cli,
    services: crate::capabilities::HarnessSessionServices,
    environment: DaemonSessionEnvironment,
) -> Result<StableSession> {
    let (input, input_rx) = crate::agent_loop::session_input_channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (done_tx, done_rx) = watch::channel(None);
    let task = tokio::spawn(async move {
        environment
            .scope(crate::agent_loop::run_worker_session(
                cli, input_rx, services, ready_tx,
            ))
            .await
    });
    let abort = task.abort_handle();
    tokio::spawn(async move {
        let end = match task.await {
            Ok(Ok(())) => SessionEnd { error: None },
            Ok(Err(error)) => SessionEnd {
                error: Some(format!("{error:#}")),
            },
            Err(error) if error.is_cancelled() => SessionEnd {
                error: Some("session runtime was force-stopped".to_string()),
            },
            Err(error) => SessionEnd {
                error: Some(format!("session runtime task failed: {error}")),
            },
        };
        done_tx.send_replace(Some(end));
    });
    let event_log = ready_rx
        .await
        .context("worker session failed before event log initialization")?;
    let committed = event_log.subscribe_committed();
    Ok(StableSession {
        input: Some(input),
        abort,
        done: done_rx,
        event_log,
        committed,
        pending_admissions: VecDeque::new(),
        pending_turns: VecDeque::new(),
        pending_effects: VecDeque::new(),
        terminal_command: None,
        terminal_outcome: None,
        forced_by_deadline: false,
        sent_through_event_seq: 0,
        acked_through_event_seq: 0,
        observed_through_event_seq: 0,
    })
}

// The explicit parameters preserve disjoint mutable borrows across this protocol phase.
#[allow(clippy::too_many_arguments)]
async fn reconnect_and_replay(
    peer: &mut RpcPeer,
    handle: &PeerHandle,
    session: &mut StableSession,
    prior_policy: &mut Option<SessionPolicy>,
    identity: &WorkerIdentity,
    journal: &CommandJournal,
    inbox: &mut CommandInbox,
    event_ack: u64,
    lifecycle: WorkerLifecycleState,
    lease: &mut LeaseState,
) -> Result<()> {
    let log = session.event_log.clone();
    let replay_handle = handle.clone();
    let replay = tokio::spawn(async move {
        send_event_suffix(&replay_handle, &log, event_ack.saturating_add(1)).await
    });
    tokio::pin!(replay);
    let mut replay_target = None;
    let mut health_tick = tokio::time::interval(EVENT_LOG_HEALTH_POLL);
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ensure_event_log_healthy(&session.event_log)?;
        enforce_terminal_deadline(session);
        if replay_target.is_some_and(|target| session.acked_through_event_seq >= target) {
            break;
        }
        if now_ms() > lease.expires_at_unix_ms {
            bail!("worker lease expired during event reconciliation");
        }
        let heartbeat_sleep = tokio::time::sleep_until(lease.next_heartbeat);
        tokio::pin!(heartbeat_sleep);
        tokio::select! {
            biased;
            result = &mut replay, if replay_target.is_none() => {
                let target = result.context("event replay task failed")??;
                session.sent_through_event_seq = session.sent_through_event_seq.max(target);
                replay_target = Some(target);
            }
            envelope = peer.recv() => {
                let envelope = envelope?;
                handle_reconnecting_message(
                    handle,
                    session,
                    prior_policy,
                    identity,
                    journal,
                    inbox,
                    envelope,
                    lifecycle,
                    lease,
                ).await?;
            }
            _ = &mut heartbeat_sleep => {
                send_with_retry(
                    handle,
                    WorkerMessage::Heartbeat(Heartbeat {
                        lease_id: lease.lease_id.clone(),
                        observed_at_unix_ms: now_ms(),
                    }),
                    MessagePriority::Control,
                ).await?;
                lease.heartbeat_sent();
            }
            _ = health_tick.tick() => {
                enforce_terminal_deadline(session);
                ensure_event_log_healthy(&session.event_log)?;
            }
        }
    }

    reconcile_terminal_turns(session, journal, handle, event_ack.saturating_add(1)).await?;
    for outcome in journal.unacknowledged_outcomes() {
        send_with_retry(
            handle,
            WorkerMessage::CommandOutcome(outcome),
            MessagePriority::Replay,
        )
        .await?;
    }
    Ok(())
}

// The explicit parameters preserve disjoint mutable borrows across this protocol phase.
#[allow(clippy::too_many_arguments)]
async fn handle_reconnecting_message(
    handle: &PeerHandle,
    session: &mut StableSession,
    prior_policy: &mut Option<SessionPolicy>,
    identity: &WorkerIdentity,
    journal: &CommandJournal,
    inbox: &mut CommandInbox,
    envelope: bro_protocol::Envelope,
    lifecycle: WorkerLifecycleState,
    lease: &mut LeaseState,
) -> Result<()> {
    match envelope.body.clone() {
        WorkerMessage::Command(command) if command.command_seq <= journal.last_command_seq() => {
            apply_command(
                handle, session, identity, journal, envelope, command, lifecycle,
            )
            .await?;
        }
        WorkerMessage::Command(command) if is_priority_command(&command.command) => {
            if command.command_seq == journal.next_command_seq() {
                apply_command(
                    handle, session, identity, journal, envelope, command, lifecycle,
                )
                .await?;
                while let Some(queued) = inbox.pop_control_sequence(journal.next_command_seq()) {
                    apply_command(
                        handle,
                        session,
                        identity,
                        journal,
                        queued.envelope,
                        queued.command,
                        lifecycle,
                    )
                    .await?;
                }
            } else {
                inbox
                    .push(QueuedCommand { envelope, command })
                    .map_err(|queue| anyhow!("worker {queue} command queue is full"))?;
            }
        }
        WorkerMessage::Command(command) => {
            inbox
                .push(QueuedCommand { envelope, command })
                .map_err(|queue| anyhow!("worker {queue} command queue is full"))?;
        }
        WorkerMessage::EventAck(EventAck { through_event_seq }) => {
            let latest = latest_event_seq(&session.event_log)?;
            if through_event_seq > latest {
                bail!(
                    "fleet acknowledged event {through_event_seq} beyond worker durable event {latest}"
                );
            }
            session.acked_through_event_seq =
                session.acked_through_event_seq.max(through_event_seq);
        }
        WorkerMessage::LeaseRenewal(renewal) => lease.renew(renewal)?,
        WorkerMessage::CommandOutcomeAck(ack) => {
            journal.acknowledge_outcomes(ack.through_command_seq)?;
        }
        WorkerMessage::ReplayRequest(request) => {
            session.sent_through_event_seq = session
                .sent_through_event_seq
                .max(send_event_suffix(handle, &session.event_log, request.from_event_seq).await?);
        }
        WorkerMessage::ServicePolicy(policy) => {
            validate_welcome_policy(&policy, prior_policy)?;
            session
                .send(SessionInput::ServicePolicy(service_policy_update(
                    &policy, true,
                )?))
                .map_err(|_| anyhow!("session closed before live service policy update"))?;
        }
        WorkerMessage::ProtocolError(error) if error.fatal => {
            bail!("fleet reported fatal protocol error: {}", error.message);
        }
        WorkerMessage::ProtocolError(_) => {}
        other => bail!("unexpected worker message while replaying: {other:?}"),
    }
    Ok(())
}

enum GenerationEnd {
    SessionTerminal,
    Disconnected(bro_rpc::DisconnectReason),
}

enum TerminalFinish {
    Complete,
    Reconnect(String),
}

// The explicit parameters preserve disjoint mutable borrows across this protocol phase.
#[allow(clippy::too_many_arguments)]
async fn run_active_generation(
    peer: &mut RpcPeer,
    handle: &PeerHandle,
    session: &mut StableSession,
    prior_policy: &mut Option<SessionPolicy>,
    capability_connection: &RpcCapabilityConnection,
    identity: &WorkerIdentity,
    journal: &CommandJournal,
    inbox: &mut CommandInbox,
    lease: &mut LeaseState,
) -> Result<GenerationEnd> {
    drain_command_inbox(
        handle,
        session,
        identity,
        journal,
        inbox,
        WorkerLifecycleState::Active,
    )
    .await?;
    send_status(
        handle,
        identity,
        session,
        journal,
        WorkerLifecycleState::Active,
    )?;

    loop {
        if let Err(error) = ensure_event_log_healthy(&session.event_log) {
            let _ = send_fatal_event_log_error(handle, &error).await;
            return Err(error);
        }
        if session.is_done() {
            return Ok(GenerationEnd::SessionTerminal);
        }
        enforce_terminal_deadline(session);
        if now_ms() > lease.expires_at_unix_ms {
            handle.shutdown();
            return Ok(GenerationEnd::Disconnected(
                bro_rpc::DisconnectReason::LocalShutdown,
            ));
        }
        let heartbeat_sleep = tokio::time::sleep_until(lease.next_heartbeat);
        let deadline_tick = tokio::time::sleep(Duration::from_millis(50));
        tokio::pin!(heartbeat_sleep);
        tokio::pin!(deadline_tick);
        tokio::select! {
            biased;
            changed = session.done.changed() => {
                let _ = changed;
                if session.is_done() {
                    return Ok(GenerationEnd::SessionTerminal);
                }
            }
            envelope = peer.recv() => {
                let envelope = match envelope {
                    Ok(envelope) => envelope,
                    Err(_) => return Ok(GenerationEnd::Disconnected(handle.wait_disconnected().await)),
                };
                handle_active_message(
                    handle,
                    session,
                    prior_policy,
                    capability_connection,
                    identity,
                    journal,
                    inbox,
                    envelope,
                    lease,
                )
                .await?;
            }
            committed = session.committed.recv() => {
                match committed {
                    Ok(event) => {
                        if event.event_seq <= session.sent_through_event_seq {
                            continue;
                        }
                        if event.event_seq > session.sent_through_event_seq.saturating_add(1) {
                            session.sent_through_event_seq = reconcile_and_send_event_suffix(
                                session,
                                journal,
                                handle,
                                session.sent_through_event_seq.saturating_add(1),
                            )
                            .await?;
                            continue;
                        }
                        observe_terminal_event(session, journal, handle, &event).await?;
                        session.sent_through_event_seq = event.event_seq;
                        send_committed_event(handle, event, MessagePriority::Normal).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        session.sent_through_event_seq = reconcile_and_send_event_suffix(
                            session,
                            journal,
                            handle,
                            session.sent_through_event_seq.saturating_add(1),
                        )
                        .await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        bail!("session event-log committed channel closed");
                    }
                }
            }
            _ = &mut heartbeat_sleep => {
                send_with_retry(
                    handle,
                    WorkerMessage::Heartbeat(Heartbeat {
                        lease_id: lease.lease_id.clone(),
                        observed_at_unix_ms: now_ms(),
                    }),
                    MessagePriority::Control,
                ).await?;
                lease.heartbeat_sent();
            }
            _ = &mut deadline_tick => {
                enforce_terminal_deadline(session);
                if let Err(error) = ensure_event_log_healthy(&session.event_log) {
                    let _ = send_fatal_event_log_error(handle, &error).await;
                    return Err(error);
                }
            }
        }
    }
}

// The explicit parameters preserve disjoint mutable borrows across this protocol phase.
#[allow(clippy::too_many_arguments)]
async fn handle_active_message(
    handle: &PeerHandle,
    session: &mut StableSession,
    prior_policy: &mut Option<SessionPolicy>,
    capability_connection: &RpcCapabilityConnection,
    identity: &WorkerIdentity,
    journal: &CommandJournal,
    inbox: &mut CommandInbox,
    envelope: bro_protocol::Envelope,
    lease: &mut LeaseState,
) -> Result<()> {
    match envelope.body.clone() {
        WorkerMessage::Command(command) => {
            let next_command_seq = journal.next_command_seq();
            if command.command_seq <= journal.last_command_seq()
                || command.command_seq == next_command_seq
            {
                apply_command(
                    handle,
                    session,
                    identity,
                    journal,
                    envelope,
                    command,
                    WorkerLifecycleState::Active,
                )
                .await?;
            } else {
                inbox
                    .push(QueuedCommand { envelope, command })
                    .map_err(|queue| anyhow!("worker {queue} command queue is full"))?;
            }
            drain_command_inbox(
                handle,
                session,
                identity,
                journal,
                inbox,
                WorkerLifecycleState::Active,
            )
            .await?;
        }
        WorkerMessage::EventAck(EventAck { through_event_seq }) => {
            let latest = latest_event_seq(&session.event_log)?;
            if through_event_seq > latest {
                bail!(
                    "fleet acknowledged event {through_event_seq} beyond worker durable event {latest}"
                );
            }
            session.acked_through_event_seq =
                session.acked_through_event_seq.max(through_event_seq);
        }
        WorkerMessage::CommandOutcomeAck(CommandOutcomeAck {
            through_command_seq,
        }) => journal.acknowledge_outcomes(through_command_seq)?,
        WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq }) => {
            let _ = send_event_suffix(handle, &session.event_log, from_event_seq).await?;
        }
        WorkerMessage::ServicePolicy(policy) => {
            validate_welcome_policy(&policy, prior_policy)?;
            if !capability_connection
                .update_policy(handle.binding().connection_generation, &policy)?
            {
                bail!("live service policy targeted a stale connection generation");
            }
            session
                .send(SessionInput::ServicePolicy(service_policy_update(
                    &policy, true,
                )?))
                .map_err(|_| anyhow!("session closed before live service policy update"))?;
        }
        WorkerMessage::LeaseRenewal(renewal) => lease.renew(renewal)?,
        WorkerMessage::ProtocolError(error) if error.fatal => {
            bail!("fleet reported fatal protocol error: {}", error.message)
        }
        WorkerMessage::ProtocolError(_) => {}
        other => bail!("unexpected active worker message: {other:?}"),
    }
    Ok(())
}

async fn drain_command_inbox(
    handle: &PeerHandle,
    session: &mut StableSession,
    identity: &WorkerIdentity,
    journal: &CommandJournal,
    inbox: &mut CommandInbox,
    lifecycle: WorkerLifecycleState,
) -> Result<()> {
    while let Some(queued) = inbox.pop_sequence(journal.next_command_seq()) {
        apply_command(
            handle,
            session,
            identity,
            journal,
            queued.envelope,
            queued.command,
            lifecycle,
        )
        .await?;
    }
    Ok(())
}

async fn apply_command(
    handle: &PeerHandle,
    session: &mut StableSession,
    identity: &WorkerIdentity,
    journal: &CommandJournal,
    _envelope: bro_protocol::Envelope,
    command: WorkerCommand,
    lifecycle: WorkerLifecycleState,
) -> Result<()> {
    if lifecycle == WorkerLifecycleState::Active {
        reconcile_committed_before_admission(session, journal, handle).await?;
    }
    match journal.prepare(&command) {
        Ok(CommandDisposition::Duplicate(outcome)) => {
            send_with_retry(
                handle,
                WorkerMessage::CommandOutcome(outcome),
                MessagePriority::Control,
            )
            .await?;
            return Ok(());
        }
        Ok(CommandDisposition::Apply) => {}
        Err(error @ CommandJournalError::SequenceGap { .. })
        | Err(error @ CommandJournalError::IdentityConflict { .. })
        | Err(error @ CommandJournalError::Indeterminate { .. }) => {
            let _ = handle.send_protocol_error(protocol_error_from_journal(&error));
            return Err(error.into());
        }
        Err(error) => return Err(error.into()),
    }

    let is_status = matches!(&command.command, WorkerCommandKind::RequestStatus);
    let is_terminal_request = matches!(
        &command.command,
        WorkerCommandKind::Drain { .. } | WorkerCommandKind::Shutdown { .. }
    );
    let needs_post_admission_reconcile = matches!(
        &command.command,
        WorkerCommandKind::UserTurn { .. }
            | WorkerCommandKind::Steer { .. }
            | WorkerCommandKind::AgentMailbox { .. }
            | WorkerCommandKind::SetModel { .. }
            | WorkerCommandKind::Compact
    );
    let active_turn = !session.pending_turns.is_empty() || !session.pending_admissions.is_empty();
    let defer_compact = active_turn && matches!(&command.command, WorkerCommandKind::Compact);
    let defer_set_model =
        active_turn && matches!(&command.command, WorkerCommandKind::SetModel { .. });
    let mut outcome = CommandOutcome {
        command_seq: command.command_seq,
        command_id: CommandId::new(command.command_id.as_str()),
        accepted: true,
        terminal: true,
        result_or_error: json!({"accepted": true}),
    };
    let input_result = match &command.command {
        WorkerCommandKind::UserTurn { text } => {
            if lifecycle != WorkerLifecycleState::Active || session.terminal_command.is_some() {
                Err("session is not admitting new turns")
            } else {
                outcome.terminal = false;
                outcome.result_or_error = json!({"queued": true});
                session
                    .send(SessionInput::WorkerTurn {
                        text: text.clone(),
                        command_id: command.command_id.as_str().to_string(),
                    })
                    .map_err(|_| "session input closed")
            }
        }
        WorkerCommandKind::Steer { text } => {
            if lifecycle != WorkerLifecycleState::Active || session.terminal_command.is_some() {
                Err("session is not admitting steers")
            } else {
                outcome.terminal = false;
                outcome.result_or_error = json!({"queued": true});
                session
                    .send(SessionInput::WorkerSteer {
                        text: text.clone(),
                        command_id: command.command_id.as_str().to_string(),
                    })
                    .map_err(|_| "session input closed")
            }
        }
        WorkerCommandKind::AgentMailbox { delivery } => {
            if lifecycle != WorkerLifecycleState::Active || session.terminal_command.is_some() {
                Err("session is not admitting agent mailbox deliveries")
            } else {
                outcome.terminal = false;
                outcome.result_or_error = json!({"queued": true});
                session
                    .send(SessionInput::AgentMailbox {
                        delivery: (**delivery).clone(),
                        command_id: command.command_id.as_str().to_string(),
                    })
                    .map_err(|_| "session input closed")
            }
        }
        WorkerCommandKind::Interrupt => {
            outcome.terminal = false;
            outcome.result_or_error = json!({"queued": true});
            session
                .send(SessionInput::Control {
                    subtype: "interrupt".to_string(),
                    request_id: Some(command.command_id.as_str().to_string()),
                    raw: json!({"type":"control_request","subtype":"interrupt"}),
                })
                .map_err(|_| "session input closed")
        }
        WorkerCommandKind::SetModel { model } => {
            if lifecycle != WorkerLifecycleState::Active || session.terminal_command.is_some() {
                Err("session is not admitting model changes")
            } else {
                outcome.terminal = false;
                outcome.result_or_error = json!({"queued": true});
                if defer_set_model {
                    Ok(())
                } else {
                    session
                        .send(SessionInput::Control {
                            subtype: "set_model".to_string(),
                            request_id: Some(command.command_id.as_str().to_string()),
                            raw: json!({"type":"control_request","subtype":"set_model","model":model}),
                        })
                        .map_err(|_| "session input closed")
                }
            }
        }
        WorkerCommandKind::Compact => {
            if lifecycle != WorkerLifecycleState::Active || session.terminal_command.is_some() {
                Err("session is not admitting compaction")
            } else {
                outcome.terminal = false;
                outcome.result_or_error = json!({"queued": true});
                if defer_compact {
                    Ok(())
                } else {
                    session
                        .send(SessionInput::User("/compact".to_string()))
                        .map_err(|_| "session input closed")
                }
            }
        }
        WorkerCommandKind::RequestStatus => {
            outcome.result_or_error = serde_json::to_value(identity.status(
                handle.binding(),
                latest_event_seq(&session.event_log)?,
                journal.last_command_seq(),
                lifecycle,
            ))?;
            Ok(())
        }
        WorkerCommandKind::Drain { .. } => {
            outcome.terminal = false;
            outcome.result_or_error = json!({"draining": true});
            session.begin_terminal(command.clone(), false);
            Ok(())
        }
        WorkerCommandKind::Shutdown { mode, .. } => {
            outcome.terminal = false;
            outcome.result_or_error = json!({"shutting_down": true, "mode": mode});
            session.begin_terminal(command.clone(), *mode == ShutdownMode::Force);
            Ok(())
        }
    };
    if let Err(message) = input_result {
        outcome.accepted = false;
        outcome.terminal = true;
        outcome.result_or_error = json!({"code":"worker.command_rejected","message":message});
    }
    if outcome.accepted
        && !outcome.terminal
        && matches!(
            &command.command,
            WorkerCommandKind::UserTurn { .. }
                | WorkerCommandKind::Steer { .. }
                | WorkerCommandKind::AgentMailbox { .. }
        )
    {
        session.pending_admissions.push_back(command);
    } else if outcome.accepted
        && !outcome.terminal
        && matches!(
            &command.command,
            WorkerCommandKind::Interrupt
                | WorkerCommandKind::SetModel { .. }
                | WorkerCommandKind::Compact
        )
    {
        session.pending_effects.push_back(PendingEffect {
            command,
            dispatched: !(defer_compact || defer_set_model),
            completion: None,
        });
    }
    journal.finish(outcome.clone())?;
    send_with_retry(
        handle,
        WorkerMessage::CommandOutcome(outcome.clone()),
        MessagePriority::Control,
    )
    .await?;
    if lifecycle == WorkerLifecycleState::Active
        && outcome.accepted
        && !outcome.terminal
        && needs_post_admission_reconcile
    {
        reconcile_committed_before_admission(session, journal, handle).await?;
    }
    if session.pending_turns.is_empty() && session.pending_admissions.is_empty() {
        dispatch_deferred_effects(session, journal, handle).await?;
    }
    if is_status {
        send_status(handle, identity, session, journal, lifecycle)?;
    } else if outcome.accepted && is_terminal_request {
        send_status(
            handle,
            identity,
            session,
            journal,
            WorkerLifecycleState::Draining,
        )?;
    }
    Ok(())
}

async fn observe_terminal_event(
    session: &mut StableSession,
    journal: &CommandJournal,
    handle: &PeerHandle,
    event: &CommittedEvent,
) -> Result<()> {
    if event.event_seq <= session.observed_through_event_seq {
        return Ok(());
    }
    observe_terminal_event_inner(session, journal, handle, event).await?;
    session.observed_through_event_seq = event.event_seq;
    Ok(())
}

async fn observe_terminal_event_inner(
    session: &mut StableSession,
    journal: &CommandJournal,
    handle: &PeerHandle,
    event: &CommittedEvent,
) -> Result<()> {
    if event.event.get("type").and_then(Value::as_str) == Some("harness_milestone")
        && event.event.get("milestone").and_then(Value::as_str) == Some("worker_input_admitted")
    {
        let command_id = event
            .event
            .get("command_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("worker input admission omitted command_id"))?;
        let disposition = event
            .event
            .get("disposition")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("worker input admission omitted disposition"))?;
        let Some(index) = session
            .pending_admissions
            .iter()
            .position(|command| command.command_id.as_str() == command_id)
        else {
            return Ok(());
        };
        let command = session
            .pending_admissions
            .remove(index)
            .expect("pending admission index came from the same queue");
        match (&command.command, disposition) {
            (WorkerCommandKind::UserTurn { .. }, "turn_started" | "turn_queued") => {
                session.pending_turns.push_back(PendingTurnGroup {
                    commands: vec![command],
                    result: None,
                });
            }
            (WorkerCommandKind::Steer { .. }, "steer_injected") => {
                if let Some(group) = session
                    .pending_turns
                    .iter_mut()
                    .find(|group| group.result.is_none())
                {
                    group.commands.push(command);
                } else {
                    finish_rejected_admission(journal, handle, command, event.event.clone())
                        .await?;
                }
            }
            (WorkerCommandKind::Steer { .. }, "rejected_idle") => {
                finish_rejected_admission(journal, handle, command, event.event.clone()).await?;
            }
            (
                WorkerCommandKind::AgentMailbox { delivery },
                "mailbox_queued" | "mailbox_duplicate",
            ) => {
                let result = json!({
                    "delivery_id": delivery.delivery_id,
                    "through_sequence": delivery.through_sequence,
                    "admitted": true,
                    "duplicate": disposition == "mailbox_duplicate",
                });
                finish_mailbox_admission(journal, handle, command, result).await?;
            }
            _ => bail!(
                "worker input admission disposition {disposition} does not match command kind"
            ),
        }
        if session.pending_turns.is_empty() && session.pending_admissions.is_empty() {
            dispatch_deferred_effects(session, journal, handle).await?;
        }
        session.close_input_at_safe_boundary();
        return Ok(());
    }

    if event.event.get("type").and_then(Value::as_str) == Some("control_response") {
        let request_id = event
            .event
            .get("response")
            .and_then(|response| response.get("request_id"))
            .and_then(Value::as_str);
        if let Some(request_id) = request_id
            && let Some(index) = session.pending_effects.iter().position(|effect| {
                effect.dispatched && effect.command.command_id.as_str() == request_id
            })
        {
            if matches!(
                &session.pending_effects[index].command.command,
                WorkerCommandKind::Interrupt
            ) {
                finish_pending_effect(session, journal, handle, event.event.clone(), index).await?;
            } else {
                session.pending_effects[index].completion = Some(event.event.clone());
            }
        }
        session.close_input_at_safe_boundary();
        return Ok(());
    }

    if event.event.get("type").and_then(Value::as_str) == Some("harness_milestone")
        && event.event.get("milestone").and_then(Value::as_str)
            == Some("session_snapshot_committed")
    {
        if let Some(group) = session
            .pending_turns
            .pop_front_if(|group| group.result.is_some())
        {
            let result = group
                .result
                .expect("snapshot-ready pending turn has a result");
            finish_turn_commands(journal, handle, group.commands, result).await?;
        }
        while let Some(index) = session
            .pending_effects
            .iter()
            .position(|effect| effect.completion.is_some())
        {
            let completion = session.pending_effects[index]
                .completion
                .clone()
                .expect("pending effect completion was checked above");
            finish_pending_effect(session, journal, handle, completion, index).await?;
        }
        if session.pending_turns.is_empty() && session.pending_admissions.is_empty() {
            dispatch_deferred_effects(session, journal, handle).await?;
        }
        session.close_input_at_safe_boundary();
        return Ok(());
    }

    let boundary = if event.event.get("type").and_then(Value::as_str) == Some("result") {
        Some(TurnOutcomeBoundary::Result)
    } else if event.event.get("type").and_then(Value::as_str) == Some("harness_milestone")
        && event.event.get("milestone").and_then(Value::as_str) == Some("compact_boundary")
    {
        Some(TurnOutcomeBoundary::Compact)
    } else {
        None
    };
    let Some(boundary) = boundary else {
        return Ok(());
    };

    match boundary {
        TurnOutcomeBoundary::Result => {
            if let Some(group) = session
                .pending_turns
                .iter_mut()
                .find(|group| group.result.is_none())
            {
                group.result = Some(event.event.clone());
            }
        }
        TurnOutcomeBoundary::Compact => {
            if let Some(effect) = session.pending_effects.iter_mut().find(|effect| {
                effect.dispatched
                    && effect.completion.is_none()
                    && matches!(&effect.command.command, WorkerCommandKind::Compact)
            }) {
                effect.completion = Some(event.event.clone());
            }
        }
    }
    session.close_input_at_safe_boundary();
    Ok(())
}

async fn finish_turn_commands(
    journal: &CommandJournal,
    handle: &PeerHandle,
    commands: Vec<WorkerCommand>,
    result: Value,
) -> Result<()> {
    for command in commands {
        let outcome = CommandOutcome {
            command_seq: command.command_seq,
            command_id: command.command_id,
            accepted: true,
            terminal: true,
            result_or_error: result.clone(),
        };
        journal.finish(outcome.clone())?;
        send_with_retry(
            handle,
            WorkerMessage::CommandOutcome(outcome),
            MessagePriority::Control,
        )
        .await?;
    }
    Ok(())
}

async fn finish_rejected_admission(
    journal: &CommandJournal,
    handle: &PeerHandle,
    command: WorkerCommand,
    admission: Value,
) -> Result<()> {
    let outcome = CommandOutcome {
        command_seq: command.command_seq,
        command_id: command.command_id,
        accepted: true,
        terminal: true,
        result_or_error: json!({
            "code": "worker.command_rejected",
            "message": "steer arrived without an active turn",
            "admission": admission,
        }),
    };
    journal.finish(outcome.clone())?;
    send_with_retry(
        handle,
        WorkerMessage::CommandOutcome(outcome),
        MessagePriority::Control,
    )
    .await
}

async fn finish_mailbox_admission(
    journal: &CommandJournal,
    handle: &PeerHandle,
    command: WorkerCommand,
    result: Value,
) -> Result<()> {
    let outcome = CommandOutcome {
        command_seq: command.command_seq,
        command_id: command.command_id,
        accepted: true,
        terminal: true,
        result_or_error: result,
    };
    journal.finish(outcome.clone())?;
    send_with_retry(
        handle,
        WorkerMessage::CommandOutcome(outcome),
        MessagePriority::Control,
    )
    .await
}

async fn finish_pending_effect(
    session: &mut StableSession,
    journal: &CommandJournal,
    handle: &PeerHandle,
    completion: Value,
    index: usize,
) -> Result<()> {
    let command = session
        .pending_effects
        .remove(index)
        .ok_or_else(|| anyhow!("pending effect disappeared before completion"))?
        .command;
    let outcome = CommandOutcome {
        command_seq: command.command_seq,
        command_id: command.command_id,
        accepted: true,
        terminal: true,
        result_or_error: completion,
    };
    journal.finish(outcome.clone())?;
    send_with_retry(
        handle,
        WorkerMessage::CommandOutcome(outcome),
        MessagePriority::Control,
    )
    .await
}

async fn dispatch_deferred_effects(
    session: &mut StableSession,
    journal: &CommandJournal,
    handle: &PeerHandle,
) -> Result<()> {
    let sender = session.input.clone();
    let mut failed = Vec::new();
    for (index, effect) in session.pending_effects.iter_mut().enumerate() {
        if effect.dispatched {
            continue;
        }
        let input = match &effect.command.command {
            WorkerCommandKind::Compact => SessionInput::User("/compact".to_string()),
            WorkerCommandKind::SetModel { model } => SessionInput::Control {
                subtype: "set_model".to_string(),
                request_id: Some(effect.command.command_id.as_str().to_string()),
                raw: json!({"type":"control_request","subtype":"set_model","model":model}),
            },
            other => bail!("unsupported deferred worker effect: {other:?}"),
        };
        if sender
            .as_ref()
            .is_some_and(|sender| sender.send(input).is_ok())
        {
            effect.dispatched = true;
        } else {
            failed.push(index);
        }
    }

    for index in failed.into_iter().rev() {
        let command = session
            .pending_effects
            .remove(index)
            .expect("failed deferred effect index came from the same queue")
            .command;
        let outcome = CommandOutcome {
            command_seq: command.command_seq,
            command_id: command.command_id,
            accepted: true,
            terminal: true,
            result_or_error: json!({
                "code": "worker.command_failed",
                "message": "session input closed before deferred command admission",
            }),
        };
        journal.finish(outcome.clone())?;
        send_with_retry(
            handle,
            WorkerMessage::CommandOutcome(outcome),
            MessagePriority::Control,
        )
        .await?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum TurnOutcomeBoundary {
    Result,
    Compact,
}

async fn reconcile_terminal_turns(
    session: &mut StableSession,
    journal: &CommandJournal,
    handle: &PeerHandle,
    mut from: u64,
) -> Result<()> {
    loop {
        ensure_event_log_healthy(&session.event_log)?;
        let batch = session.event_log.replay_from_with_limits(
            from,
            ReplayLimits {
                max_events: EVENT_REPLAY_CHUNK_EVENTS,
                max_bytes: EVENT_REPLAY_CHUNK_BYTES,
            },
        )?;
        for event in &batch.events {
            observe_terminal_event(session, journal, handle, event).await?;
        }
        match batch.next_event_seq {
            Some(next) => from = next,
            None => return Ok(()),
        }
    }
}

async fn reconcile_committed_before_admission(
    session: &mut StableSession,
    journal: &CommandJournal,
    handle: &PeerHandle,
) -> Result<()> {
    let latest = latest_event_seq(&session.event_log)?;
    if latest <= session.sent_through_event_seq {
        return Ok(());
    }
    session.sent_through_event_seq = reconcile_and_send_event_suffix(
        session,
        journal,
        handle,
        session.sent_through_event_seq.saturating_add(1),
    )
    .await?;
    Ok(())
}

async fn reconcile_and_send_event_suffix(
    session: &mut StableSession,
    journal: &CommandJournal,
    handle: &PeerHandle,
    mut from: u64,
) -> Result<u64> {
    let mut sent_through = from.saturating_sub(1);
    loop {
        ensure_event_log_healthy(&session.event_log)?;
        let batch = session.event_log.replay_from_with_limits(
            from,
            ReplayLimits {
                max_events: EVENT_REPLAY_CHUNK_EVENTS,
                max_bytes: EVENT_REPLAY_CHUNK_BYTES,
            },
        )?;
        if let Some(earliest) = batch.diagnostics.iter().find_map(|diagnostic| {
            if let ReplayDiagnostic::RequestedBeforeAvailable { available_from, .. } = diagnostic {
                Some(*available_from)
            } else {
                None
            }
        }) {
            send_with_retry(
                handle,
                WorkerMessage::ReplayUnavailable(ReplayUnavailable {
                    requested_from_event_seq: from,
                    earliest_available_event_seq: earliest,
                    latest_available_event_seq: batch.available_through.unwrap_or(0),
                }),
                MessagePriority::Control,
            )
            .await?;
            return Ok(sent_through);
        }
        for event in batch.events {
            observe_terminal_event(session, journal, handle, &event).await?;
            sent_through = sent_through.max(event.event_seq);
            send_committed_event(handle, event, MessagePriority::Replay).await?;
        }
        match batch.next_event_seq {
            Some(next) => {
                from = next;
                tokio::task::yield_now().await;
            }
            None => return Ok(sent_through),
        }
    }
}

async fn send_event_suffix(handle: &PeerHandle, log: &EventLog, mut from: u64) -> Result<u64> {
    let mut sent_through = from.saturating_sub(1);
    loop {
        ensure_event_log_healthy(log)?;
        let batch = log.replay_from_with_limits(
            from,
            ReplayLimits {
                max_events: EVENT_REPLAY_CHUNK_EVENTS,
                max_bytes: EVENT_REPLAY_CHUNK_BYTES,
            },
        )?;
        let before_available = batch
            .diagnostics
            .iter()
            .find_map(|diagnostic| match diagnostic {
                ReplayDiagnostic::RequestedBeforeAvailable { available_from, .. } => {
                    Some(*available_from)
                }
                _ => None,
            });
        if let Some(earliest) = before_available {
            send_with_retry(
                handle,
                WorkerMessage::ReplayUnavailable(ReplayUnavailable {
                    requested_from_event_seq: from,
                    earliest_available_event_seq: earliest,
                    latest_available_event_seq: batch.available_through.unwrap_or(0),
                }),
                MessagePriority::Control,
            )
            .await?;
            return Ok(sent_through);
        }
        for event in batch.events {
            sent_through = sent_through.max(event.event_seq);
            send_committed_event(handle, event, MessagePriority::Replay).await?;
        }
        match batch.next_event_seq {
            Some(next) => {
                from = next;
                tokio::task::yield_now().await;
            }
            None => return Ok(sent_through),
        }
    }
}

async fn send_committed_event(
    handle: &PeerHandle,
    event: CommittedEvent,
    priority: MessagePriority,
) -> Result<()> {
    send_with_retry(handle, WorkerMessage::Event(event), priority).await
}

async fn send_with_retry(
    handle: &PeerHandle,
    body: WorkerMessage,
    priority: MessagePriority,
) -> Result<()> {
    loop {
        match handle.send(body.clone(), priority) {
            Ok(_) => return Ok(()),
            Err(RpcError::QueueFull { .. }) => tokio::time::sleep(EVENT_SEND_RETRY).await,
            Err(error) => return Err(error.into()),
        }
    }
}

async fn finish_terminal_session(
    session: &mut StableSession,
    journal: &CommandJournal,
    peer: &mut RpcPeer,
    identity: &WorkerIdentity,
    generation: u64,
    lease: &mut LeaseState,
) -> Result<TerminalFinish> {
    let event_log = session.event_log.clone();
    tokio::task::spawn_blocking(move || event_log.flush_blocking_result())
        .await
        .context("worker event-log flush task panicked")??;
    let expected_forced_stop = session.forced_by_deadline
        || session.terminal_command.as_ref().is_some_and(|command| {
            matches!(
                &command.command,
                WorkerCommandKind::Shutdown {
                    mode: ShutdownMode::Force,
                    ..
                }
            )
        });
    let unexpected_session_error = session.done.borrow().clone().and_then(|end| end.error);
    let unexpected_session_error = (!expected_forced_stop)
        .then_some(unexpected_session_error)
        .flatten();
    let handle = peer.handle();
    let final_event_seq = latest_event_seq(&session.event_log)?;
    if session.sent_through_event_seq < final_event_seq {
        session.sent_through_event_seq = reconcile_and_send_event_suffix(
            session,
            journal,
            &handle,
            session.sent_through_event_seq.saturating_add(1),
        )
        .await?;
    }
    if session.sent_through_event_seq < final_event_seq {
        bail!("worker could not replay final event suffix through event {final_event_seq}");
    }
    if session.acked_through_event_seq < final_event_seq
        && let AckWait::Reconnect(reason) = wait_for_terminal_ack(
            peer,
            &handle,
            session,
            journal,
            lease,
            TerminalAckTarget::Event(final_event_seq),
        )
        .await?
    {
        return Ok(TerminalFinish::Reconnect(reason));
    }

    let terminalize_pending = expected_forced_stop || unexpected_session_error.is_some();
    let pending_terminal_seq = if terminalize_pending {
        match terminalize_pending_commands(
            session,
            journal,
            &handle,
            final_event_seq,
            unexpected_session_error.as_deref(),
        )
        .await
        {
            Ok(target) => target,
            Err(error) => return Ok(TerminalFinish::Reconnect(error.to_string())),
        }
    } else {
        None
    };

    if session.terminal_outcome.is_none()
        && let Some(command) = session.terminal_command.as_ref()
    {
        let result_or_error = if let Some(error) = &unexpected_session_error {
            json!({
                "code": "worker.session_failed",
                "message": error,
                "through_event_seq": final_event_seq,
            })
        } else {
            match &command.command {
                WorkerCommandKind::Drain { .. } => serde_json::to_value(DrainCompletion {
                    through_event_seq: final_event_seq,
                    completed_at_unix_ms: now_ms(),
                    forced_by_deadline: session.forced_by_deadline,
                })?,
                WorkerCommandKind::Shutdown { mode, .. } => {
                    serde_json::to_value(ShutdownCompletion {
                        through_event_seq: final_event_seq,
                        completed_at_unix_ms: now_ms(),
                        forced: *mode == ShutdownMode::Force || session.forced_by_deadline,
                    })?
                }
                _ => json!({"terminal": true}),
            }
        };
        let outcome = CommandOutcome {
            command_seq: command.command_seq,
            command_id: command.command_id.clone(),
            accepted: true,
            terminal: true,
            result_or_error,
        };
        journal.finish(outcome.clone())?;
        session.terminal_outcome = Some(outcome);
    }
    if let Some(outcome) = session.terminal_outcome.clone() {
        let terminal_seq = outcome.command_seq;
        if let Err(error) = send_with_retry(
            &handle,
            WorkerMessage::CommandOutcome(outcome),
            MessagePriority::Control,
        )
        .await
        {
            return Ok(TerminalFinish::Reconnect(error.to_string()));
        }
        if let AckWait::Reconnect(reason) = wait_for_terminal_ack(
            peer,
            &handle,
            session,
            journal,
            lease,
            TerminalAckTarget::Command(terminal_seq),
        )
        .await?
        {
            return Ok(TerminalFinish::Reconnect(reason));
        }
    } else {
        let pending_terminal_seq = pending_terminal_seq.or_else(|| {
            terminalize_pending.then(|| {
                journal
                    .unacknowledged_outcomes()
                    .into_iter()
                    .filter(|outcome| outcome.terminal)
                    .map(|outcome| outcome.command_seq)
                    .max()
            })?
        });
        if let Some(pending_terminal_seq) = pending_terminal_seq
            && let AckWait::Reconnect(reason) = wait_for_terminal_ack(
                peer,
                &handle,
                session,
                journal,
                lease,
                TerminalAckTarget::Command(pending_terminal_seq),
            )
            .await?
        {
            return Ok(TerminalFinish::Reconnect(reason));
        }
    }

    if let Some(error) = unexpected_session_error {
        let protocol_error = anyhow!("worker session {generation} terminated with error: {error}");
        let _ = send_fatal_worker_error(&handle, protocol_error.to_string()).await;
        return Err(protocol_error);
    }

    if let Err(error) = send_with_retry(
        &handle,
        WorkerMessage::Status(identity.status(
            handle.binding(),
            final_event_seq,
            journal.last_command_seq(),
            WorkerLifecycleState::Terminal,
        )),
        MessagePriority::Control,
    )
    .await
    {
        return Ok(TerminalFinish::Reconnect(error.to_string()));
    }
    tokio::task::yield_now().await;
    Ok(TerminalFinish::Complete)
}

async fn terminalize_pending_commands(
    session: &mut StableSession,
    journal: &CommandJournal,
    handle: &PeerHandle,
    through_event_seq: u64,
    session_error: Option<&str>,
) -> Result<Option<u64>> {
    let mut commands: Vec<WorkerCommand> = session.pending_admissions.drain(..).collect();
    for group in session.pending_turns.drain(..) {
        commands.extend(group.commands);
    }
    commands.extend(
        session
            .pending_effects
            .drain(..)
            .map(|effect| effect.command),
    );
    commands.sort_by_key(|command| command.command_seq);

    let mut outcomes = Vec::with_capacity(commands.len());
    for command in commands {
        let result_or_error = if let Some(error) = session_error {
            json!({
                "code": "worker.session_failed",
                "message": error,
                "through_event_seq": through_event_seq,
            })
        } else {
            json!({
                "code": "worker.command_cancelled",
                "message": if session.forced_by_deadline {
                    "command cancelled when the worker shutdown deadline expired"
                } else {
                    "command cancelled by forced worker shutdown"
                },
                "through_event_seq": through_event_seq,
                "forced_by_deadline": session.forced_by_deadline,
            })
        };
        let outcome = CommandOutcome {
            command_seq: command.command_seq,
            command_id: command.command_id,
            accepted: true,
            terminal: true,
            result_or_error,
        };
        journal.finish(outcome.clone())?;
        outcomes.push(outcome);
    }

    let target = outcomes.last().map(|outcome| outcome.command_seq);
    for outcome in outcomes {
        send_with_retry(
            handle,
            WorkerMessage::CommandOutcome(outcome),
            MessagePriority::Control,
        )
        .await?;
    }
    session.close_input_at_safe_boundary();
    Ok(target)
}

#[derive(Clone, Copy)]
enum TerminalAckTarget {
    Event(u64),
    Command(u64),
}

enum AckWait {
    Reached,
    Reconnect(String),
}

async fn wait_for_terminal_ack(
    peer: &mut RpcPeer,
    handle: &PeerHandle,
    session: &mut StableSession,
    journal: &CommandJournal,
    lease: &mut LeaseState,
    target: TerminalAckTarget,
) -> Result<AckWait> {
    let deadline = tokio::time::Instant::now() + TERMINAL_ACK_TIMEOUT;
    loop {
        ensure_event_log_healthy(&session.event_log)?;
        enforce_terminal_deadline(session);
        if terminal_ack_reached(session, target, None) {
            return Ok(AckWait::Reached);
        }
        if now_ms() > lease.expires_at_unix_ms {
            return Ok(AckWait::Reconnect(
                "worker lease expired during terminal acknowledgment".to_string(),
            ));
        }
        let heartbeat_sleep = tokio::time::sleep_until(lease.next_heartbeat);
        let timeout = tokio::time::sleep_until(deadline);
        let terminal_deadline_tick = tokio::time::sleep(EVENT_LOG_HEALTH_POLL);
        tokio::pin!(heartbeat_sleep);
        tokio::pin!(timeout);
        tokio::pin!(terminal_deadline_tick);
        tokio::select! {
            biased;
            envelope = peer.recv() => {
                let envelope = match envelope {
                    Ok(envelope) => envelope,
                    Err(error) => return Ok(AckWait::Reconnect(error.to_string())),
                };
                match envelope.body {
                    WorkerMessage::CommandOutcomeAck(CommandOutcomeAck { through_command_seq }) => {
                        journal.acknowledge_outcomes(through_command_seq)?;
                        if terminal_ack_reached(session, target, Some(through_command_seq)) {
                            return Ok(AckWait::Reached);
                        }
                    }
                    WorkerMessage::EventAck(EventAck { through_event_seq }) => {
                        let latest = latest_event_seq(&session.event_log)?;
                        if through_event_seq > latest {
                            bail!(
                                "fleet acknowledged event {through_event_seq} beyond worker durable event {latest}"
                            );
                        }
                        session.acked_through_event_seq = session
                            .acked_through_event_seq
                            .max(through_event_seq);
                        if terminal_ack_reached(session, target, None) {
                            return Ok(AckWait::Reached);
                        }
                    }
                    WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq }) => {
                        let _ = send_event_suffix(handle, &session.event_log, from_event_seq).await?;
                    }
                    WorkerMessage::LeaseRenewal(renewal) => lease.renew(renewal)?,
                    WorkerMessage::ProtocolError(error) if error.fatal => {
                        bail!("fleet reported fatal protocol error: {}", error.message)
                    }
                    WorkerMessage::ProtocolError(_) => {}
                    other => tracing::debug!(?other, "ignoring fleet message while awaiting terminal outcome acknowledgment"),
                }
            }
            _ = &mut heartbeat_sleep => {
                send_with_retry(
                    handle,
                    WorkerMessage::Heartbeat(Heartbeat {
                        lease_id: lease.lease_id.clone(),
                        observed_at_unix_ms: now_ms(),
                    }),
                    MessagePriority::Control,
                ).await?;
                lease.heartbeat_sent();
            }
            _ = &mut terminal_deadline_tick => {
                enforce_terminal_deadline(session);
            }
            _ = &mut timeout => {
                return Ok(AckWait::Reconnect(format!(
                    "fleet did not reach the terminal durable acknowledgment within {} ms",
                    TERMINAL_ACK_TIMEOUT.as_millis()
                )));
            }
        }
    }
}

fn terminal_ack_reached(
    session: &StableSession,
    target: TerminalAckTarget,
    command_ack: Option<u64>,
) -> bool {
    match target {
        TerminalAckTarget::Event(event_seq) => session.acked_through_event_seq >= event_seq,
        TerminalAckTarget::Command(command_seq) => {
            command_ack.is_some_and(|through| through >= command_seq)
        }
    }
}

fn send_status(
    handle: &PeerHandle,
    identity: &WorkerIdentity,
    session: &StableSession,
    journal: &CommandJournal,
    state: WorkerLifecycleState,
) -> Result<()> {
    handle.send(
        WorkerMessage::Status(identity.status(
            handle.binding(),
            latest_event_seq(&session.event_log)?,
            journal.last_command_seq(),
            state,
        )),
        MessagePriority::Control,
    )?;
    Ok(())
}

fn latest_event_seq(log: &EventLog) -> Result<u64> {
    match log.health() {
        EventLogHealth::Healthy { .. } => Ok(log.last_committed_event_seq()),
        EventLogHealth::Fatal { failure, .. } => Err(failure.into()),
        EventLogHealth::Disabled => bail!("worker event log is disabled"),
    }
}

fn ensure_event_log_healthy(log: &EventLog) -> Result<()> {
    latest_event_seq(log).map(|_| ())
}

async fn send_fatal_event_log_error(handle: &PeerHandle, error: &anyhow::Error) -> Result<()> {
    send_fatal_worker_error(handle, format!("worker event log failed: {error:#}")).await
}

async fn send_fatal_worker_error(handle: &PeerHandle, message: String) -> Result<()> {
    tokio::time::timeout(
        Duration::from_millis(250),
        send_with_retry(
            handle,
            WorkerMessage::ProtocolError(ProtocolError {
                code: ProtocolErrorCode::Internal,
                message,
                fatal: true,
                related_message_id: None,
                expected_protocol_version: None,
                expected_connection_generation: None,
            }),
            MessagePriority::Control,
        ),
    )
    .await
    .context("timed out reporting fatal worker error")?
}

fn enforce_terminal_deadline(session: &mut StableSession) {
    let Some(command) = session.terminal_command.as_ref() else {
        return;
    };
    let deadline = match &command.command {
        WorkerCommandKind::Drain {
            deadline_unix_ms, ..
        }
        | WorkerCommandKind::Shutdown {
            deadline_unix_ms, ..
        } => *deadline_unix_ms,
        _ => None,
    };
    if deadline.is_some_and(|deadline| now_ms() >= deadline) && !session.forced_by_deadline {
        session.forced_by_deadline = true;
        session.input.take();
        session.abort.abort();
    }
}

fn validate_worker_cli(cli: &Cli) -> Result<()> {
    if cli.worker_probe {
        bail!("--worker and --worker-probe are mutually exclusive");
    }
    if cli.worker_protocol_versions.is_empty() {
        bail!("--worker-protocol-versions must advertise at least one version");
    }
    if !cli.daemon_worker {
        bail!("--worker requires --daemon-worker credential isolation");
    }
    if cli.worker_control_capacity == 0 || cli.worker_input_capacity == 0 {
        bail!("worker queue capacities must be greater than zero");
    }
    Ok(())
}

fn is_terminal_handshake_rejection(error: &RpcError) -> bool {
    let RpcError::HandshakeRejected { code, .. } = error else {
        return false;
    };
    matches!(
        code.as_str(),
        "protocol.authentication_failed"
            | "protocol.version_mismatch"
            | "protocol.unsupported_protocol"
            | "protocol.unsupported_build"
            | "protocol.policy_mismatch"
    )
}

fn validate_next_command_seq(fleet_next: u64, worker_next: u64) -> Result<()> {
    if fleet_next < worker_next {
        bail!("fleet command sequence {fleet_next} is behind worker sequence {worker_next}");
    }
    Ok(())
}

fn validate_welcome_policy(
    policy: &SessionPolicy,
    prior_policy: &mut Option<SessionPolicy>,
) -> Result<()> {
    let feature_policy = policy
        .feature_policy()?
        .ok_or_else(|| anyhow!("fleet welcome omitted negotiated worker feature policy"))?;
    let required = required_features().into_iter().collect::<BTreeSet<_>>();
    if !required.is_subset(&feature_policy.enabled_features) {
        let missing = required
            .difference(&feature_policy.enabled_features)
            .map(|feature| feature.as_str())
            .collect::<Vec<_>>();
        bail!("fleet did not enable required worker features: {missing:?}");
    }
    if feature_policy.policy.version == 0 || feature_policy.policy.digest.trim().is_empty() {
        bail!("fleet supplied an invalid service policy revision")
    }
    if let Some(prior) = prior_policy.as_ref() {
        let prior_features = prior
            .feature_policy()?
            .ok_or_else(|| anyhow!("prior fleet policy omitted negotiated worker features"))?;
        if prior_features.enabled_features != feature_policy.enabled_features {
            bail!("required worker feature policy changed across reconnect")
        }
        if feature_policy.policy.version < prior_features.policy.version
            || feature_policy.policy.version > prior_features.policy.version.saturating_add(1)
        {
            bail!("service policy revision is non-contiguous across reconnect")
        }
        if feature_policy.policy.version == prior_features.policy.version
            && (feature_policy.policy.digest != prior_features.policy.digest || policy != prior)
        {
            bail!("service policy changed without a revision bump")
        }
    }
    *prior_policy = Some(policy.clone());
    Ok(())
}

fn service_policy_update(policy: &SessionPolicy, connected: bool) -> Result<ServicePolicyUpdate> {
    let revision = policy
        .feature_policy()?
        .ok_or_else(|| anyhow!("fleet welcome omitted negotiated worker feature policy"))?
        .policy;
    let downstream_availability = policy
        .downstream_service_availability()?
        .ok_or_else(|| anyhow!("fleet service policy omitted downstream availability"))?;
    Ok(ServicePolicyUpdate {
        revision,
        allowed_capabilities: policy.allowed_capabilities.iter().cloned().collect(),
        downstream_availability,
        connected,
    })
}

fn required_features() -> Vec<WorkerFeature> {
    [
        WorkerFeature::ORDERED_REPLAY,
        WorkerFeature::COMMAND_IDEMPOTENCY,
        WorkerFeature::GENERATION_FENCING,
        WorkerFeature::CAPABILITY_RPC,
        WorkerFeature::LEASE_RENEWAL,
        WorkerFeature::DRAIN_SHUTDOWN,
    ]
    .into_iter()
    .map(WorkerFeature::new)
    .collect()
}

fn protocol_error_from_journal(error: &CommandJournalError) -> ProtocolError {
    let code = match error {
        CommandJournalError::SequenceGap { .. } => ProtocolErrorCode::SequenceGap,
        CommandJournalError::IdentityConflict { .. }
        | CommandJournalError::Indeterminate { .. } => ProtocolErrorCode::DuplicateMessageId,
        _ => ProtocolErrorCode::Internal,
    };
    ProtocolError {
        code,
        message: error.to_string(),
        fatal: true,
        related_message_id: None,
        expected_protocol_version: None,
        expected_connection_generation: None,
    }
}

fn is_priority_command(command: &WorkerCommandKind) -> bool {
    matches!(
        command,
        WorkerCommandKind::Interrupt
            | WorkerCommandKind::Drain { .. }
            | WorkerCommandKind::Shutdown { .. }
            | WorkerCommandKind::RequestStatus
    )
}

fn required(value: &Option<String>, flag: &str) -> Result<String> {
    value
        .as_ref()
        .filter(|value| !value.is_empty())
        .cloned()
        .with_context(|| format!("{flag} is required in worker mode"))
}

async fn read_worker_credential(
    bootstrap_path: &str,
    reconnect_path: &std::path::Path,
) -> Result<String> {
    if tokio::fs::try_exists(reconnect_path)
        .await
        .with_context(|| format!("checking reconnect credential {}", reconnect_path.display()))?
    {
        return read_private_secret(reconnect_path).await;
    }
    read_private_secret(std::path::Path::new(bootstrap_path)).await
}

async fn read_private_secret(path: &std::path::Path) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("reading credential file metadata {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!("worker credential file must not be accessible by group or other users");
        }
    }
    let secret = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading worker credential file {}", path.display()))?;
    let secret = secret.trim_end().to_string();
    if secret.is_empty() {
        bail!("worker credential file is empty");
    }
    Ok(secret)
}

async fn persist_reconnect_credential(path: PathBuf, secret: String) -> Result<()> {
    tokio::task::spawn_blocking(move || persist_reconnect_credential_blocking(&path, &secret))
        .await
        .context("reconnect credential writer panicked")?
}

#[allow(clippy::disallowed_methods)]
fn persist_reconnect_credential_blocking(path: &std::path::Path, secret: &str) -> Result<()> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("reconnect credential path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("worker-reconnect"),
        Uuid::new_v4()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(secret.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
// Filesystem fixtures intentionally exercise private reconnect credential persistence.
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::collections::BTreeMap;

    use bro_protocol::{
        AuthenticationProof, BuildIdentity, DownstreamServiceAvailability, FeaturePolicy,
        FleetWelcome, LeaseGrant, PolicyIdentity, ServiceAvailability, SessionPolicy,
        WORKER_PROTOCOL_V1, WorkerHello,
    };
    use bro_rpc::{FleetHandshakeGrant, HandshakeAuthorityReject, HandshakeOptions, PeerConfig};

    use super::*;

    fn identity() -> WorkerIdentity {
        WorkerIdentity {
            worker_id: WorkerId::new("worker-1"),
            task_id: TaskId::new("task-1"),
            session_id: SessionId::new("session-1"),
            build: BuildIdentity {
                version: "test".to_string(),
                build_id: "worker".to_string(),
            },
            protocol_versions: vec![WORKER_PROTOCOL_V1],
        }
    }

    fn stable_session(
        event_log: Arc<EventLog>,
        end: Option<SessionEnd>,
    ) -> (StableSession, crate::agent_loop::SessionInputReceiver) {
        let committed = event_log.subscribe_committed();
        let (input, input_rx) = crate::agent_loop::session_input_channel();
        let pending_task = tokio::spawn(std::future::pending::<()>());
        let abort = pending_task.abort_handle();
        let (_done_tx, done) = watch::channel(end);
        (
            StableSession {
                input: Some(input),
                abort,
                done,
                event_log,
                committed,
                pending_admissions: VecDeque::new(),
                pending_turns: VecDeque::new(),
                pending_effects: VecDeque::new(),
                terminal_command: None,
                terminal_outcome: None,
                forced_by_deadline: false,
                sent_through_event_seq: 0,
                acked_through_event_seq: 0,
                observed_through_event_seq: 0,
            },
            input_rx,
        )
    }

    fn worker_command(seq: u64, kind: WorkerCommandKind) -> WorkerCommand {
        WorkerCommand {
            command_seq: seq,
            command_id: CommandId::new(format!("command-{seq}")),
            command: kind,
        }
    }

    fn prepare_pending(journal: &CommandJournal, command: &WorkerCommand) {
        assert!(matches!(
            journal.prepare(command).unwrap(),
            CommandDisposition::Apply
        ));
        journal
            .finish(CommandOutcome {
                command_seq: command.command_seq,
                command_id: command.command_id.clone(),
                accepted: true,
                terminal: false,
                result_or_error: json!({"queued": true}),
            })
            .unwrap();
    }

    fn committed_event(event_seq: u64, event: Value) -> CommittedEvent {
        CommittedEvent {
            event_seq,
            occurred_at_unix_ms: now_ms(),
            event,
        }
    }

    fn snapshot_event(event_seq: u64) -> CommittedEvent {
        committed_event(
            event_seq,
            json!({
                "type": "harness_milestone",
                "milestone": "session_snapshot_committed",
            }),
        )
    }

    fn admission_event(
        event_seq: u64,
        command_id: &str,
        kind: &str,
        disposition: &str,
    ) -> CommittedEvent {
        committed_event(
            event_seq,
            json!({
                "type": "harness_milestone",
                "milestone": "worker_input_admitted",
                "command_id": command_id,
                "kind": kind,
                "disposition": disposition,
            }),
        )
    }

    async fn peer_pair(generation: u64) -> (RpcPeer, RpcPeer, FleetWelcome) {
        peer_pair_with_worker_config(generation, PeerConfig::default()).await
    }

    async fn peer_pair_with_worker_config(
        generation: u64,
        worker_config: PeerConfig,
    ) -> (RpcPeer, RpcPeer, FleetWelcome) {
        let (worker_io, fleet_io) = tokio::io::duplex(256 * 1024);
        let mut session_policy = SessionPolicy {
            allowed_capabilities: Vec::new(),
            attributes: BTreeMap::new(),
        };
        session_policy
            .set_feature_policy(FeaturePolicy {
                enabled_features: required_features().into_iter().collect(),
                policy: PolicyIdentity {
                    version: 1,
                    digest: "sha256:test".to_string(),
                },
            })
            .unwrap();
        session_policy
            .set_downstream_service_availability(DownstreamServiceAvailability {
                blackops: ServiceAvailability::Available,
                corpus: ServiceAvailability::Available,
            })
            .unwrap();
        let hello = WorkerHello {
            protocol_versions: vec![WORKER_PROTOCOL_V1],
            worker_build: BuildIdentity {
                version: "test".to_string(),
                build_id: "worker".to_string(),
            },
            worker_id: WorkerId::new("worker-1"),
            task_id: TaskId::new("task-1"),
            session_id: SessionId::new("session-1"),
            bootstrap_or_resume_proof: AuthenticationProof::new("bootstrap"),
            last_local_event_seq: 0,
            last_fleet_command_seq: 0,
            worker_capabilities: required_features()
                .into_iter()
                .map(|feature| feature.as_str().to_string())
                .collect(),
        };
        let grant = FleetHandshakeGrant {
            connection_generation: generation,
            event_ack: 0,
            next_command_seq: 1,
            lease: LeaseGrant {
                lease_id: format!("lease-{generation}"),
                expires_at_unix_ms: now_ms() + 60_000,
                heartbeat_interval_ms: 10_000,
                reattach_grace_ms: 10_000,
            },
            reconnect_proof: AuthenticationProof::new(format!("reconnect-{generation}")),
            session_policy,
            fleet_build: BuildIdentity {
                version: "test".to_string(),
                build_id: "fleet".to_string(),
            },
        };
        let worker =
            bro_rpc::connect_worker_with_options(worker_io, hello, HandshakeOptions::default());
        let fleet = bro_rpc::accept_worker_with_authority(
            fleet_io,
            vec![WORKER_PROTOCOL_V1],
            HandshakeOptions::default(),
            move |_, _| async move { Ok::<FleetHandshakeGrant, HandshakeAuthorityReject>(grant) },
        );
        let (worker, fleet) = tokio::join!(worker, fleet);
        let (worker_io, welcome) = worker.unwrap();
        let (fleet_io, _, _) = fleet.unwrap();
        (
            RpcPeer::spawn(worker_io, worker_config).unwrap(),
            RpcPeer::spawn(fleet_io, PeerConfig::default()).unwrap(),
            welcome,
        )
    }

    fn command(seq: u64, command: WorkerCommandKind) -> QueuedCommand {
        QueuedCommand {
            envelope: bro_protocol::Envelope {
                protocol_version: 1,
                connection_generation: 1,
                message_id: format!("message-{seq}"),
                reply_to: None,
                body: WorkerMessage::DrainAck,
            },
            command: WorkerCommand {
                command_seq: seq,
                command_id: CommandId::new(format!("command-{seq}")),
                command,
            },
        }
    }

    #[test]
    fn control_admission_remains_available_when_normal_queue_is_full() {
        let mut inbox = CommandInbox::new(1, 1).unwrap();
        inbox
            .push(command(
                1,
                WorkerCommandKind::UserTurn {
                    text: "first".to_string(),
                },
            ))
            .unwrap();
        assert_eq!(
            inbox.push(command(
                2,
                WorkerCommandKind::UserTurn {
                    text: "second".to_string(),
                },
            )),
            Err("normal")
        );
        inbox
            .push(command(2, WorkerCommandKind::Interrupt))
            .unwrap();
        assert!(matches!(
            inbox.pop_sequence(2).unwrap().command.command,
            WorkerCommandKind::Interrupt
        ));
    }

    #[tokio::test]
    async fn active_control_command_waits_for_an_overtaken_normal_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        let (mut session, mut input_rx) = stable_session(event_log, None);
        let (worker_peer, mut fleet_peer, welcome) = peer_pair(30).await;
        let handle = worker_peer.handle();
        let identity = identity();
        let (_capability_client, capability_connection) =
            RpcCapabilityClient::new(identity.session_id.clone());
        let mut prior_policy = None;
        let mut inbox = CommandInbox::new(4, 4).unwrap();
        let mut lease = LeaseState::from_grant(&welcome.lease);
        let interrupt = worker_command(2, WorkerCommandKind::Interrupt);
        let turn = worker_command(
            1,
            WorkerCommandKind::UserTurn {
                text: "first".to_string(),
            },
        );

        handle_active_message(
            &handle,
            &mut session,
            &mut prior_policy,
            &capability_connection,
            &identity,
            &journal,
            &mut inbox,
            bro_protocol::Envelope {
                protocol_version: WORKER_PROTOCOL_V1,
                connection_generation: 30,
                message_id: "message-2".to_string(),
                reply_to: None,
                body: WorkerMessage::Command(interrupt),
            },
            &mut lease,
        )
        .await
        .unwrap();
        assert_eq!(journal.next_command_seq(), 1);
        assert!(inbox.control.contains_key(&2));
        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        handle_active_message(
            &handle,
            &mut session,
            &mut prior_policy,
            &capability_connection,
            &identity,
            &journal,
            &mut inbox,
            bro_protocol::Envelope {
                protocol_version: WORKER_PROTOCOL_V1,
                connection_generation: 30,
                message_id: "message-1".to_string(),
                reply_to: None,
                body: WorkerMessage::Command(turn),
            },
            &mut lease,
        )
        .await
        .unwrap();

        assert_eq!(journal.last_command_seq(), 2);
        for expected in 1..=2 {
            let WorkerMessage::CommandOutcome(outcome) = fleet_peer.recv().await.unwrap().body
            else {
                panic!("expected sequenced command outcome")
            };
            assert_eq!(outcome.command_seq, expected);
        }
        assert!(matches!(
            input_rx.try_recv(),
            Ok(SessionInput::WorkerTurn { text, command_id })
                if text == "first" && command_id == "command-1"
        ));
        assert!(matches!(
            input_rx.try_recv(),
            Ok(SessionInput::Control { subtype, request_id, .. })
                if subtype == "interrupt" && request_id.as_deref() == Some("command-2")
        ));
        assert!(inbox.control.is_empty());
        assert!(inbox.normal.is_empty());
        session.abort.abort();
    }

    #[tokio::test]
    async fn queue_full_after_session_admission_preserves_command_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        let (session, mut input_rx) = stable_session(event_log, None);
        let worker_config = PeerConfig {
            control_queue_bytes: 1,
            read_idle_timeout: None,
            ..PeerConfig::default()
        };
        let (worker_peer, _fleet_peer, _) = peer_pair_with_worker_config(39, worker_config).await;
        let handle = worker_peer.handle();
        let task_handle = handle.clone();
        let command = worker_command(
            1,
            WorkerCommandKind::UserTurn {
                text: "tracked before send".to_string(),
            },
        );
        let task = tokio::spawn(async move {
            let mut session = session;
            let result = apply_command(
                &task_handle,
                &mut session,
                &identity(),
                &journal,
                bro_protocol::Envelope {
                    protocol_version: WORKER_PROTOCOL_V1,
                    connection_generation: 39,
                    message_id: "message-1".to_string(),
                    reply_to: None,
                    body: WorkerMessage::Command(command.clone()),
                },
                command,
                WorkerLifecycleState::Active,
            )
            .await;
            (result, session, journal)
        });

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), input_rx.recv())
                .await
                .unwrap(),
            Some(SessionInput::WorkerTurn { text, command_id })
                if text == "tracked before send" && command_id == "command-1"
        ));
        handle.shutdown();
        let (result, session, journal) = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();

        assert!(result.is_err());
        assert_eq!(session.pending_admissions.len(), 1);
        assert_eq!(session.pending_admissions.front().unwrap().command_seq, 1);
        assert!(
            journal.unacknowledged_outcomes().iter().any(|outcome| {
                outcome.command_seq == 1 && outcome.accepted && !outcome.terminal
            })
        );
        session.abort.abort();
    }

    #[test]
    fn reconnect_backoff_configuration_fails_closed_on_zero_queue_budget() {
        let cli = <Cli as clap::Parser>::try_parse_from([
            "bro-harness",
            "--worker",
            "--worker-control-capacity",
            "0",
        ])
        .unwrap();
        assert!(validate_worker_cli(&cli).is_err());
    }

    #[test]
    fn worker_mode_requires_daemon_credential_isolation() {
        let cli = <Cli as clap::Parser>::try_parse_from(["bro-harness", "--worker"]).unwrap();
        assert!(validate_worker_cli(&cli).is_err());

        let cli =
            <Cli as clap::Parser>::try_parse_from(["bro-harness", "--worker", "--daemon-worker"])
                .unwrap();
        assert!(validate_worker_cli(&cli).is_ok());
    }

    #[test]
    fn fleet_may_have_an_assigned_suffix_but_cannot_move_behind_worker() {
        assert!(validate_next_command_seq(7, 4).is_ok());
        assert!(validate_next_command_seq(4, 4).is_ok());
        assert!(validate_next_command_seq(3, 4).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rotated_reconnect_credential_is_private_and_preferred_on_restart() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let bootstrap = root.join("bootstrap");
        std::fs::write(&bootstrap, "one-time-bootstrap\n").unwrap();
        let mut bootstrap_permissions = std::fs::metadata(&bootstrap).unwrap().permissions();
        bootstrap_permissions.set_mode(0o600);
        std::fs::set_permissions(&bootstrap, bootstrap_permissions).unwrap();
        let reconnect = root.join("state").join("worker-reconnect");

        assert_eq!(
            read_worker_credential(bootstrap.to_str().unwrap(), &reconnect)
                .await
                .unwrap(),
            "one-time-bootstrap"
        );
        persist_reconnect_credential(reconnect.clone(), "rotated-proof".to_string())
            .await
            .unwrap();
        assert_eq!(
            read_worker_credential(bootstrap.to_str().unwrap(), &reconnect)
                .await
                .unwrap(),
            "rotated-proof"
        );
        assert_eq!(
            std::fs::metadata(&reconnect).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn replay_starts_after_every_ack_boundary_without_duplication() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let log = EventLog::at_path(root.join("session.events.jsonl"));
        for index in 1..=4 {
            log.try_append_event(&json!({"type":"probe","index":index}))
                .unwrap();
        }
        log.flush_blocking_result().unwrap();

        for ack in 0..=4 {
            let (worker_peer, mut fleet_peer, _) = peer_pair(ack + 1).await;
            let handle = worker_peer.handle();
            let sent = send_event_suffix(&handle, &log, ack + 1).await.unwrap();
            assert_eq!(sent, 4.max(ack));
            let mut sequences = Vec::new();
            for _ in ack..4 {
                let envelope = fleet_peer.recv().await.unwrap();
                let WorkerMessage::Event(event) = envelope.body else {
                    panic!("expected replay event")
                };
                sequences.push(event.event_seq);
            }
            assert_eq!(sequences, ((ack + 1)..=4).collect::<Vec<_>>());
            drop(worker_peer);
        }
    }

    #[test]
    fn lease_renewal_is_bound_to_the_granted_lease() {
        let mut lease = LeaseState::from_grant(&LeaseGrant {
            lease_id: "lease-1".to_string(),
            expires_at_unix_ms: now_ms() + 1_000,
            heartbeat_interval_ms: 100,
            reattach_grace_ms: 1_000,
        });
        assert!(
            lease
                .renew(LeaseRenewal {
                    lease_id: "other".to_string(),
                    renewed_at_unix_ms: now_ms(),
                    expires_at_unix_ms: now_ms() + 2_000,
                    next_heartbeat_due_unix_ms: now_ms() + 100,
                })
                .is_err()
        );
        lease
            .renew(LeaseRenewal {
                lease_id: "lease-1".to_string(),
                renewed_at_unix_ms: now_ms(),
                expires_at_unix_ms: now_ms() + 2_000,
                next_heartbeat_due_unix_ms: now_ms() + 100,
            })
            .unwrap();
    }

    #[test]
    fn only_auth_version_and_policy_handshake_rejections_are_terminal() {
        for code in [
            "protocol.authentication_failed",
            "protocol.version_mismatch",
            "protocol.unsupported_protocol",
            "protocol.unsupported_build",
            "protocol.policy_mismatch",
        ] {
            assert!(is_terminal_handshake_rejection(
                &RpcError::HandshakeRejected {
                    code: code.to_string(),
                    message: "terminal".to_string(),
                    supported_protocol_versions: vec![WORKER_PROTOCOL_V1],
                }
            ));
        }
        for code in [
            "protocol.worker_busy",
            "protocol.authority_unavailable",
            "protocol.transient",
        ] {
            assert!(!is_terminal_handshake_rejection(
                &RpcError::HandshakeRejected {
                    code: code.to_string(),
                    message: "retry".to_string(),
                    supported_protocol_versions: vec![WORKER_PROTOCOL_V1],
                }
            ));
        }
    }

    #[tokio::test]
    async fn active_service_policy_reaches_the_existing_session_without_reconnect() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        let (mut session, mut input_rx) = stable_session(event_log, None);
        let (worker_peer, _fleet_peer, welcome) = peer_pair(32).await;
        let handle = worker_peer.handle();
        let identity = identity();
        let (_capability_client, capability_connection) =
            RpcCapabilityClient::new(identity.session_id.clone());
        capability_connection.activate(handle.clone(), &welcome.session_policy);
        let mut prior_policy = Some(welcome.session_policy.clone());
        let mut next_policy = welcome.session_policy.clone();
        next_policy.allowed_capabilities = vec!["blackops.agent".into(), "corpus".into()];
        next_policy
            .set_downstream_service_availability(DownstreamServiceAvailability {
                blackops: ServiceAvailability::Unavailable,
                corpus: ServiceAvailability::Available,
            })
            .unwrap();
        let mut feature = next_policy.feature_policy().unwrap().unwrap();
        feature.policy = PolicyIdentity {
            version: 2,
            digest: "sha256:live-policy-2".into(),
        };
        next_policy.set_feature_policy(feature).unwrap();
        let mut inbox = CommandInbox::new(4, 4).unwrap();
        let mut lease = LeaseState::from_grant(&welcome.lease);

        handle_active_message(
            &handle,
            &mut session,
            &mut prior_policy,
            &capability_connection,
            &identity,
            &journal,
            &mut inbox,
            bro_protocol::Envelope {
                protocol_version: WORKER_PROTOCOL_V1,
                connection_generation: 32,
                message_id: "service-policy-2".into(),
                reply_to: None,
                body: WorkerMessage::ServicePolicy(next_policy.clone()),
            },
            &mut lease,
        )
        .await
        .unwrap();

        let SessionInput::ServicePolicy(update) = input_rx.recv().await.unwrap() else {
            panic!("live policy did not use the session safe-boundary input")
        };
        assert_eq!(update.revision.version, 2);
        assert_eq!(
            update.downstream_availability.blackops,
            ServiceAvailability::Unavailable
        );
        assert_eq!(
            update.downstream_availability.corpus,
            ServiceAvailability::Available
        );
        assert_eq!(prior_policy, Some(next_policy));
        session.abort.abort();
    }

    #[tokio::test]
    async fn replay_waits_for_durable_ack_while_control_and_lease_traffic_stay_live() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        event_log
            .try_append_event(&json!({"type":"probe"}))
            .unwrap();
        event_log.flush_blocking_result().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let (mut session, mut input_rx) = stable_session(event_log, None);
        let (mut worker_peer, mut fleet_peer, welcome) = peer_pair(31).await;
        let worker_handle = worker_peer.handle();
        let identity = identity();
        let mut inbox = CommandInbox::new(4, 4).unwrap();
        let mut lease = LeaseState::from_grant(&LeaseGrant {
            lease_id: welcome.lease.lease_id.clone(),
            expires_at_unix_ms: now_ms() + 60_000,
            heartbeat_interval_ms: 5,
            reattach_grace_ms: 10_000,
        });
        let join = tokio::spawn(async move {
            let mut prior_policy = None;
            let result = reconnect_and_replay(
                &mut worker_peer,
                &worker_handle,
                &mut session,
                &mut prior_policy,
                &identity,
                &journal,
                &mut inbox,
                0,
                WorkerLifecycleState::Reconnecting,
                &mut lease,
            )
            .await;
            let first_input = input_rx.try_recv();
            let second_input = input_rx.try_recv();
            session.abort.abort();
            (result, lease.expires_at_unix_ms, first_input, second_input)
        });

        let fleet_handle = fleet_peer.handle();
        let mut saw_replay = false;
        let mut saw_heartbeat = false;
        while !saw_replay || !saw_heartbeat {
            let body = tokio::time::timeout(Duration::from_secs(1), fleet_peer.recv())
                .await
                .unwrap()
                .unwrap()
                .body;
            saw_replay |= matches!(body, WorkerMessage::Event(_));
            saw_heartbeat |= matches!(body, WorkerMessage::Heartbeat(_));
        }
        assert!(!join.is_finished());

        fleet_handle
            .send(
                WorkerMessage::Command(worker_command(1, WorkerCommandKind::Interrupt)),
                MessagePriority::Control,
            )
            .unwrap();
        fleet_handle
            .send(
                WorkerMessage::Command(worker_command(
                    2,
                    WorkerCommandKind::UserTurn {
                        text: "must wait".to_string(),
                    },
                )),
                MessagePriority::Normal,
            )
            .unwrap();
        let renewed_expiry = now_ms() + 120_000;
        fleet_handle
            .send(
                WorkerMessage::LeaseRenewal(LeaseRenewal {
                    lease_id: welcome.lease.lease_id,
                    renewed_at_unix_ms: now_ms(),
                    expires_at_unix_ms: renewed_expiry,
                    next_heartbeat_due_unix_ms: now_ms() + 10_000,
                }),
                MessagePriority::Control,
            )
            .unwrap();

        loop {
            let body = tokio::time::timeout(Duration::from_secs(1), fleet_peer.recv())
                .await
                .unwrap()
                .unwrap()
                .body;
            if matches!(
                body,
                WorkerMessage::CommandOutcome(CommandOutcome { command_seq: 1, .. })
            ) {
                break;
            }
        }
        assert!(!join.is_finished());
        fleet_handle
            .send(
                WorkerMessage::EventAck(EventAck {
                    through_event_seq: 1,
                }),
                MessagePriority::Control,
            )
            .unwrap();

        let (result, observed_expiry, first_input, second_input) =
            tokio::time::timeout(Duration::from_secs(1), join)
                .await
                .unwrap()
                .unwrap();
        result.unwrap();
        assert_eq!(observed_expiry, renewed_expiry);
        assert!(matches!(first_input, Ok(SessionInput::Control { .. })));
        assert!(matches!(
            second_input,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn fast_user_inputs_share_the_active_turn_and_post_result_input_starts_another() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        let (mut session, _input_rx) = stable_session(event_log, None);
        let (worker_peer, mut fleet_peer, _) = peer_pair(35).await;
        let handle = worker_peer.handle();
        let identity = identity();

        for command in [
            worker_command(
                1,
                WorkerCommandKind::UserTurn {
                    text: "first".to_string(),
                },
            ),
            worker_command(
                2,
                WorkerCommandKind::Steer {
                    text: "in turn".to_string(),
                },
            ),
        ] {
            apply_command(
                &handle,
                &mut session,
                &identity,
                &journal,
                bro_protocol::Envelope {
                    protocol_version: WORKER_PROTOCOL_V1,
                    connection_generation: 35,
                    message_id: format!("message-{}", command.command_seq),
                    reply_to: None,
                    body: WorkerMessage::Command(command.clone()),
                },
                command,
                WorkerLifecycleState::Active,
            )
            .await
            .unwrap();
            let WorkerMessage::CommandOutcome(_) = fleet_peer.recv().await.unwrap().body else {
                panic!("expected queued outcome")
            };
        }
        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &admission_event(1, "command-1", "user_turn", "turn_started"),
        )
        .await
        .unwrap();
        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &admission_event(2, "command-2", "steer", "steer_injected"),
        )
        .await
        .unwrap();
        assert_eq!(session.pending_turns.len(), 1);
        assert_eq!(session.pending_turns.front().unwrap().commands.len(), 2);

        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &committed_event(3, json!({"type":"result"})),
        )
        .await
        .unwrap();
        assert_eq!(session.pending_turns.len(), 1);
        observe_terminal_event(&mut session, &journal, &handle, &snapshot_event(4))
            .await
            .unwrap();
        for expected in 1..=2 {
            let WorkerMessage::CommandOutcome(outcome) = fleet_peer.recv().await.unwrap().body
            else {
                panic!("expected terminal turn outcome")
            };
            assert_eq!(outcome.command_seq, expected);
        }
        assert!(session.pending_turns.is_empty());

        let next = worker_command(
            3,
            WorkerCommandKind::UserTurn {
                text: "next turn".to_string(),
            },
        );
        apply_command(
            &handle,
            &mut session,
            &identity,
            &journal,
            bro_protocol::Envelope {
                protocol_version: WORKER_PROTOCOL_V1,
                connection_generation: 35,
                message_id: "message-3".to_string(),
                reply_to: None,
                body: WorkerMessage::Command(next.clone()),
            },
            next,
            WorkerLifecycleState::Active,
        )
        .await
        .unwrap();
        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &admission_event(5, "command-3", "user_turn", "turn_started"),
        )
        .await
        .unwrap();
        assert_eq!(session.pending_turns.len(), 1);
        assert_eq!(session.pending_turns.front().unwrap().commands.len(), 1);
        session.abort.abort();
    }

    #[tokio::test]
    async fn committed_result_is_reconciled_before_the_next_turn_is_grouped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        let (mut session, _input_rx) = stable_session(event_log.clone(), None);
        let (worker_peer, mut fleet_peer, _) = peer_pair(36).await;
        let handle = worker_peer.handle();
        let identity = identity();

        let first = worker_command(
            1,
            WorkerCommandKind::UserTurn {
                text: "first".to_string(),
            },
        );
        apply_command(
            &handle,
            &mut session,
            &identity,
            &journal,
            bro_protocol::Envelope {
                protocol_version: WORKER_PROTOCOL_V1,
                connection_generation: 36,
                message_id: "message-1".to_string(),
                reply_to: None,
                body: WorkerMessage::Command(first.clone()),
            },
            first,
            WorkerLifecycleState::Active,
        )
        .await
        .unwrap();
        let _ = fleet_peer.recv().await.unwrap();
        event_log
            .try_append_milestone(
                "worker_input_admitted",
                "session-1",
                json!({
                    "command_id":"command-1",
                    "kind":"user_turn",
                    "disposition":"turn_started",
                }),
            )
            .unwrap();
        event_log
            .try_append_event(&json!({"type":"result","turn":1}))
            .unwrap();
        event_log.flush_blocking_result().unwrap();

        let second = worker_command(
            2,
            WorkerCommandKind::UserTurn {
                text: "second".to_string(),
            },
        );
        apply_command(
            &handle,
            &mut session,
            &identity,
            &journal,
            bro_protocol::Envelope {
                protocol_version: WORKER_PROTOCOL_V1,
                connection_generation: 36,
                message_id: "message-2".to_string(),
                reply_to: None,
                body: WorkerMessage::Command(second.clone()),
            },
            second,
            WorkerLifecycleState::Active,
        )
        .await
        .unwrap();

        assert_eq!(session.pending_turns.len(), 1);
        assert!(session.pending_turns.front().unwrap().result.is_some());
        assert_eq!(session.pending_admissions.len(), 1);
        assert_eq!(session.sent_through_event_seq, 2);
        event_log
            .try_append_milestone("session_snapshot_committed", "session-1", json!({}))
            .unwrap();
        event_log
            .try_append_milestone(
                "worker_input_admitted",
                "session-1",
                json!({
                    "command_id":"command-2",
                    "kind":"user_turn",
                    "disposition":"turn_started",
                }),
            )
            .unwrap();
        event_log.flush_blocking_result().unwrap();
        reconcile_committed_before_admission(&mut session, &journal, &handle)
            .await
            .unwrap();
        assert_eq!(session.pending_turns.len(), 1);
        assert_eq!(
            session.pending_turns.front().unwrap().commands[0].command_seq,
            2
        );
        assert!(session.pending_admissions.is_empty());
        assert_eq!(session.sent_through_event_seq, 4);
        session.abort.abort();
    }

    #[tokio::test]
    async fn compact_is_deferred_until_the_active_turn_result() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        let (mut session, mut input_rx) = stable_session(event_log, None);
        let (worker_peer, mut fleet_peer, _) = peer_pair(37).await;
        let handle = worker_peer.handle();
        let identity = identity();

        for command in [
            worker_command(
                1,
                WorkerCommandKind::UserTurn {
                    text: "active".to_string(),
                },
            ),
            worker_command(2, WorkerCommandKind::Compact),
        ] {
            apply_command(
                &handle,
                &mut session,
                &identity,
                &journal,
                bro_protocol::Envelope {
                    protocol_version: WORKER_PROTOCOL_V1,
                    connection_generation: 37,
                    message_id: format!("message-{}", command.command_seq),
                    reply_to: None,
                    body: WorkerMessage::Command(command.clone()),
                },
                command,
                WorkerLifecycleState::Active,
            )
            .await
            .unwrap();
            let _ = fleet_peer.recv().await.unwrap();
        }
        assert!(matches!(
            input_rx.try_recv(),
            Ok(SessionInput::WorkerTurn { text, command_id })
                if text == "active" && command_id == "command-1"
        ));
        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(!session.pending_effects.front().unwrap().dispatched);

        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &admission_event(1, "command-1", "user_turn", "turn_started"),
        )
        .await
        .unwrap();
        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &committed_event(2, json!({"type":"result"})),
        )
        .await
        .unwrap();
        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        observe_terminal_event(&mut session, &journal, &handle, &snapshot_event(3))
            .await
            .unwrap();
        assert!(matches!(input_rx.try_recv(), Ok(SessionInput::User(text)) if text == "/compact"));
        assert!(session.pending_effects.front().unwrap().dispatched);
        let WorkerMessage::CommandOutcome(turn_outcome) = fleet_peer.recv().await.unwrap().body
        else {
            panic!("expected turn outcome")
        };
        assert_eq!(turn_outcome.command_seq, 1);

        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &committed_event(
                4,
                json!({"type":"harness_milestone","milestone":"compact_boundary"}),
            ),
        )
        .await
        .unwrap();
        assert!(
            session
                .pending_effects
                .front()
                .unwrap()
                .completion
                .is_some()
        );
        observe_terminal_event(&mut session, &journal, &handle, &snapshot_event(5))
            .await
            .unwrap();
        let WorkerMessage::CommandOutcome(compact_outcome) = fleet_peer.recv().await.unwrap().body
        else {
            panic!("expected compact outcome")
        };
        assert_eq!(compact_outcome.command_seq, 2);
        assert!(session.pending_effects.is_empty());
        session.abort.abort();
    }

    #[tokio::test]
    async fn controls_terminalize_on_correlated_receipt_and_deferred_model_application() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        let (mut session, mut input_rx) = stable_session(event_log, None);
        let (worker_peer, mut fleet_peer, _) = peer_pair(38).await;
        let handle = worker_peer.handle();
        let identity = identity();

        let interrupt = worker_command(1, WorkerCommandKind::Interrupt);
        apply_command(
            &handle,
            &mut session,
            &identity,
            &journal,
            bro_protocol::Envelope {
                protocol_version: WORKER_PROTOCOL_V1,
                connection_generation: 38,
                message_id: "message-1".to_string(),
                reply_to: None,
                body: WorkerMessage::Command(interrupt.clone()),
            },
            interrupt,
            WorkerLifecycleState::Active,
        )
        .await
        .unwrap();
        let WorkerMessage::CommandOutcome(initial) = fleet_peer.recv().await.unwrap().body else {
            panic!("expected interrupt admission outcome")
        };
        assert!(!initial.terminal);
        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &committed_event(
                1,
                json!({"type":"control_response","response":{"subtype":"success","request_id":"command-1"}}),
            ),
        )
        .await
        .unwrap();
        let WorkerMessage::CommandOutcome(applied) = fleet_peer.recv().await.unwrap().body else {
            panic!("expected interrupt receipt outcome")
        };
        assert!(applied.terminal);

        let turn = worker_command(
            2,
            WorkerCommandKind::UserTurn {
                text: "active".to_string(),
            },
        );
        let set_model = worker_command(
            3,
            WorkerCommandKind::SetModel {
                model: "next-model".to_string(),
            },
        );
        for command in [turn, set_model] {
            apply_command(
                &handle,
                &mut session,
                &identity,
                &journal,
                bro_protocol::Envelope {
                    protocol_version: WORKER_PROTOCOL_V1,
                    connection_generation: 38,
                    message_id: format!("message-{}", command.command_seq),
                    reply_to: None,
                    body: WorkerMessage::Command(command.clone()),
                },
                command,
                WorkerLifecycleState::Active,
            )
            .await
            .unwrap();
            let _ = fleet_peer.recv().await.unwrap();
        }
        assert!(
            matches!(input_rx.try_recv(), Ok(SessionInput::Control { subtype, .. }) if subtype == "interrupt")
        );
        assert!(matches!(
            input_rx.try_recv(),
            Ok(SessionInput::WorkerTurn { text, command_id })
                if text == "active" && command_id == "command-2"
        ));
        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &admission_event(2, "command-2", "user_turn", "turn_started"),
        )
        .await
        .unwrap();
        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &committed_event(3, json!({"type":"result"})),
        )
        .await
        .unwrap();
        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        observe_terminal_event(&mut session, &journal, &handle, &snapshot_event(4))
            .await
            .unwrap();
        assert!(matches!(
            input_rx.try_recv(),
            Ok(SessionInput::Control { subtype, .. }) if subtype == "set_model"
        ));
        let WorkerMessage::CommandOutcome(turn_outcome) = fleet_peer.recv().await.unwrap().body
        else {
            panic!("expected turn outcome")
        };
        assert_eq!(turn_outcome.command_seq, 2);

        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &committed_event(
                5,
                json!({"type":"control_response","response":{"subtype":"success","request_id":"command-3"}}),
            ),
        )
        .await
        .unwrap();
        assert!(
            session
                .pending_effects
                .front()
                .unwrap()
                .completion
                .is_some()
        );
        observe_terminal_event(&mut session, &journal, &handle, &snapshot_event(6))
            .await
            .unwrap();
        let WorkerMessage::CommandOutcome(model_outcome) = fleet_peer.recv().await.unwrap().body
        else {
            panic!("expected model application outcome")
        };
        assert_eq!(model_outcome.command_seq, 3);
        assert!(model_outcome.terminal);
        session.abort.abort();
    }

    #[tokio::test]
    async fn result_and_compact_boundaries_finish_only_their_pending_groups() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        let (mut session, mut input_rx) = stable_session(event_log, None);
        let (worker_peer, mut fleet_peer, _) = peer_pair(32).await;
        let handle = worker_peer.handle();

        let commands = [
            worker_command(
                1,
                WorkerCommandKind::UserTurn {
                    text: "turn one".to_string(),
                },
            ),
            worker_command(
                2,
                WorkerCommandKind::Steer {
                    text: "steer one".to_string(),
                },
            ),
            worker_command(
                3,
                WorkerCommandKind::UserTurn {
                    text: "turn two".to_string(),
                },
            ),
            worker_command(
                4,
                WorkerCommandKind::Steer {
                    text: "steer two".to_string(),
                },
            ),
            worker_command(5, WorkerCommandKind::Compact),
            worker_command(
                6,
                WorkerCommandKind::UserTurn {
                    text: "sequential turn".to_string(),
                },
            ),
        ];
        for command in &commands {
            prepare_pending(&journal, command);
        }
        session.pending_turns.push_back(PendingTurnGroup {
            commands: commands[..4].to_vec(),
            result: None,
        });
        session.pending_effects.push_back(PendingEffect {
            command: commands[4].clone(),
            dispatched: true,
            completion: None,
        });
        session.terminal_command = Some(worker_command(
            7,
            WorkerCommandKind::Drain {
                deadline_unix_ms: None,
                reason: None,
                safe_boundary: Default::default(),
            },
        ));

        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &committed_event(1, json!({"type":"result","turn":1})),
        )
        .await
        .unwrap();
        assert_eq!(session.pending_turns.len(), 1);
        assert!(session.input.is_some());
        observe_terminal_event(&mut session, &journal, &handle, &snapshot_event(2))
            .await
            .unwrap();
        assert!(session.pending_turns.is_empty());
        for expected in 1..=4 {
            let WorkerMessage::CommandOutcome(outcome) = fleet_peer.recv().await.unwrap().body
            else {
                panic!("expected turn outcome")
            };
            assert_eq!(outcome.command_seq, expected);
        }

        session.pending_turns.push_back(PendingTurnGroup {
            commands: vec![commands[5].clone()],
            result: None,
        });
        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &committed_event(3, json!({"type":"result","turn":2})),
        )
        .await
        .unwrap();
        assert_eq!(session.pending_turns.len(), 1);
        assert!(session.input.is_some());
        observe_terminal_event(&mut session, &journal, &handle, &snapshot_event(4))
            .await
            .unwrap();
        assert!(session.pending_turns.is_empty());
        let WorkerMessage::CommandOutcome(outcome) = fleet_peer.recv().await.unwrap().body else {
            panic!("expected sequential turn outcome")
        };
        assert_eq!(outcome.command_seq, 6);

        observe_terminal_event(
            &mut session,
            &journal,
            &handle,
            &committed_event(
                5,
                json!({"type":"harness_milestone","milestone":"compact_boundary"}),
            ),
        )
        .await
        .unwrap();
        assert!(
            session
                .pending_effects
                .front()
                .unwrap()
                .completion
                .is_some()
        );
        assert!(session.input.is_some());
        observe_terminal_event(&mut session, &journal, &handle, &snapshot_event(6))
            .await
            .unwrap();
        let WorkerMessage::CommandOutcome(outcome) = fleet_peer.recv().await.unwrap().body else {
            panic!("expected compact outcome")
        };
        assert_eq!(outcome.command_seq, 5);
        assert!(session.input.is_none());
        assert!(input_rx.recv().await.is_none());
        session.abort.abort();
    }

    #[tokio::test]
    async fn force_shutdown_terminalizes_active_turn_before_cumulative_ack() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        let (mut session, _input_rx) = stable_session(
            event_log,
            Some(SessionEnd {
                error: Some("session runtime was force-stopped".to_string()),
            }),
        );
        let (mut worker_peer, mut fleet_peer, welcome) = peer_pair(40).await;
        let handle = worker_peer.handle();
        let identity = identity();

        for command in [
            worker_command(
                1,
                WorkerCommandKind::UserTurn {
                    text: "active".to_string(),
                },
            ),
            worker_command(
                2,
                WorkerCommandKind::Shutdown {
                    mode: ShutdownMode::Force,
                    deadline_unix_ms: None,
                    reason: Some("operator force".to_string()),
                },
            ),
        ] {
            apply_command(
                &handle,
                &mut session,
                &identity,
                &journal,
                bro_protocol::Envelope {
                    protocol_version: WORKER_PROTOCOL_V1,
                    connection_generation: 40,
                    message_id: format!("message-{}", command.command_seq),
                    reply_to: None,
                    body: WorkerMessage::Command(command.clone()),
                },
                command,
                WorkerLifecycleState::Active,
            )
            .await
            .unwrap();
        }
        let mut initial_outcomes = 0;
        let mut saw_draining = false;
        while initial_outcomes < 2 || !saw_draining {
            match fleet_peer.recv().await.unwrap().body {
                WorkerMessage::CommandOutcome(outcome) => {
                    assert!(!outcome.terminal);
                    initial_outcomes += 1;
                }
                WorkerMessage::Status(status) => {
                    assert_eq!(status.state, WorkerLifecycleState::Draining);
                    saw_draining = true;
                }
                other => panic!("unexpected initial force-shutdown frame: {other:?}"),
            }
        }

        let mut lease = LeaseState::from_grant(&welcome.lease);
        let join = tokio::spawn(async move {
            let result = finish_terminal_session(
                &mut session,
                &journal,
                &mut worker_peer,
                &identity,
                40,
                &mut lease,
            )
            .await;
            (result, journal)
        });

        let WorkerMessage::CommandOutcome(cancelled) = fleet_peer.recv().await.unwrap().body else {
            panic!("expected active-turn cancellation outcome")
        };
        assert_eq!(cancelled.command_seq, 1);
        assert!(cancelled.terminal);
        assert_eq!(
            cancelled.result_or_error["code"],
            "worker.command_cancelled"
        );
        let WorkerMessage::CommandOutcome(shutdown) = fleet_peer.recv().await.unwrap().body else {
            panic!("expected terminal shutdown outcome")
        };
        assert_eq!(shutdown.command_seq, 2);
        assert!(shutdown.terminal);
        assert!(!join.is_finished());

        fleet_peer
            .handle()
            .send(
                WorkerMessage::CommandOutcomeAck(CommandOutcomeAck {
                    through_command_seq: 2,
                }),
                MessagePriority::Control,
            )
            .unwrap();
        let WorkerMessage::Status(status) = fleet_peer.recv().await.unwrap().body else {
            panic!("expected terminal worker status")
        };
        assert_eq!(status.state, WorkerLifecycleState::Terminal);
        let (finish, journal) = join.await.unwrap();
        assert!(matches!(finish.unwrap(), TerminalFinish::Complete));
        assert!(journal.unacknowledged_outcomes().is_empty());
    }

    #[tokio::test]
    async fn terminal_closeout_waits_for_final_event_then_command_ack_before_status() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        event_log
            .try_append_event(&json!({"type":"result","result":"done"}))
            .unwrap();
        event_log.flush_blocking_result().unwrap();
        let (mut session, _input_rx) = stable_session(event_log, Some(SessionEnd { error: None }));
        let terminal_command = worker_command(
            1,
            WorkerCommandKind::Drain {
                deadline_unix_ms: None,
                reason: None,
                safe_boundary: Default::default(),
            },
        );
        prepare_pending(&journal, &terminal_command);
        session.terminal_command = Some(terminal_command);
        let (mut worker_peer, mut fleet_peer, welcome) = peer_pair(33).await;
        let fleet_handle = fleet_peer.handle();
        let identity = identity();
        let mut lease = LeaseState::from_grant(&welcome.lease);
        let join = tokio::spawn(async move {
            let result = finish_terminal_session(
                &mut session,
                &journal,
                &mut worker_peer,
                &identity,
                33,
                &mut lease,
            )
            .await;
            session.abort.abort();
            result
        });

        let WorkerMessage::Event(event) = fleet_peer.recv().await.unwrap().body else {
            panic!("expected final event replay")
        };
        assert_eq!(event.event_seq, 1);
        assert!(!join.is_finished());
        fleet_handle
            .send(
                WorkerMessage::EventAck(EventAck {
                    through_event_seq: 1,
                }),
                MessagePriority::Control,
            )
            .unwrap();

        let WorkerMessage::CommandOutcome(outcome) = fleet_peer.recv().await.unwrap().body else {
            panic!("expected terminal command outcome")
        };
        assert_eq!(outcome.command_seq, 1);
        assert!(outcome.terminal);
        assert!(!join.is_finished());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), fleet_peer.recv())
                .await
                .is_err()
        );
        fleet_handle
            .send(
                WorkerMessage::CommandOutcomeAck(CommandOutcomeAck {
                    through_command_seq: 1,
                }),
                MessagePriority::Control,
            )
            .unwrap();

        let WorkerMessage::Status(status) = fleet_peer.recv().await.unwrap().body else {
            panic!("expected terminal status")
        };
        assert_eq!(status.state, WorkerLifecycleState::Terminal);
        assert!(matches!(
            join.await.unwrap().unwrap(),
            TerminalFinish::Complete
        ));
    }

    #[tokio::test]
    async fn terminal_ack_disconnect_requests_another_generation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        let (mut session, _input_rx) = stable_session(event_log, Some(SessionEnd { error: None }));
        let terminal_command = worker_command(
            1,
            WorkerCommandKind::Shutdown {
                mode: ShutdownMode::Graceful,
                deadline_unix_ms: None,
                reason: None,
            },
        );
        prepare_pending(&journal, &terminal_command);
        session.terminal_command = Some(terminal_command);
        let (mut worker_peer, mut fleet_peer, welcome) = peer_pair(34).await;
        let identity = identity();
        let mut lease = LeaseState::from_grant(&welcome.lease);
        let join = tokio::spawn(async move {
            let result = finish_terminal_session(
                &mut session,
                &journal,
                &mut worker_peer,
                &identity,
                34,
                &mut lease,
            )
            .await;
            session.abort.abort();
            result
        });

        let WorkerMessage::CommandOutcome(_) = fleet_peer.recv().await.unwrap().body else {
            panic!("expected terminal command outcome")
        };
        drop(fleet_peer);
        assert!(matches!(
            join.await.unwrap().unwrap(),
            TerminalFinish::Reconnect(_)
        ));
    }

    #[tokio::test]
    async fn status_and_drain_commands_use_durable_outcomes_and_close_input() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = CommandJournal::open(root.join("commands.jsonl")).unwrap();
        let event_log = Arc::new(EventLog::at_path(root.join("session.events.jsonl")));
        let committed = event_log.subscribe_committed();
        let (input, mut input_rx) = crate::agent_loop::session_input_channel();
        let pending_task = tokio::spawn(std::future::pending::<()>());
        let abort = pending_task.abort_handle();
        let (_done_tx, done) = watch::channel(None);
        let mut session = StableSession {
            input: Some(input),
            abort,
            done,
            event_log,
            committed,
            pending_admissions: VecDeque::new(),
            pending_turns: VecDeque::new(),
            pending_effects: VecDeque::new(),
            terminal_command: None,
            terminal_outcome: None,
            forced_by_deadline: false,
            sent_through_event_seq: 0,
            acked_through_event_seq: 0,
            observed_through_event_seq: 0,
        };
        let identity = WorkerIdentity {
            worker_id: WorkerId::new("worker-1"),
            task_id: TaskId::new("task-1"),
            session_id: SessionId::new("session-1"),
            build: BuildIdentity {
                version: "test".to_string(),
                build_id: "worker".to_string(),
            },
            protocol_versions: vec![WORKER_PROTOCOL_V1],
        };
        let (worker_peer, mut fleet_peer, _) = peer_pair(20).await;
        let handle = worker_peer.handle();

        let status_command = WorkerCommand {
            command_seq: 1,
            command_id: CommandId::new("status-1"),
            command: WorkerCommandKind::RequestStatus,
        };
        let status_envelope = bro_protocol::Envelope {
            protocol_version: WORKER_PROTOCOL_V1,
            connection_generation: 20,
            message_id: "status-message".to_string(),
            reply_to: None,
            body: WorkerMessage::Command(status_command.clone()),
        };
        apply_command(
            &handle,
            &mut session,
            &identity,
            &journal,
            status_envelope,
            status_command.clone(),
            WorkerLifecycleState::Active,
        )
        .await
        .unwrap();
        let status_frames = [
            fleet_peer.recv().await.unwrap().body,
            fleet_peer.recv().await.unwrap().body,
        ];
        assert!(status_frames.iter().any(|body| matches!(
            body,
            WorkerMessage::CommandOutcome(CommandOutcome { terminal: true, .. })
        )));
        assert!(status_frames.iter().any(|body| matches!(
            body,
            WorkerMessage::Status(WorkerStatus {
                state: WorkerLifecycleState::Active,
                ..
            })
        )));
        assert!(matches!(
            journal.prepare(&status_command).unwrap(),
            CommandDisposition::Duplicate(CommandOutcome { terminal: true, .. })
        ));

        let drain_command = WorkerCommand {
            command_seq: 2,
            command_id: CommandId::new("drain-2"),
            command: WorkerCommandKind::Drain {
                deadline_unix_ms: None,
                reason: Some("rolling replacement".to_string()),
                safe_boundary: Default::default(),
            },
        };
        let drain_envelope = bro_protocol::Envelope {
            protocol_version: WORKER_PROTOCOL_V1,
            connection_generation: 20,
            message_id: "drain-message".to_string(),
            reply_to: None,
            body: WorkerMessage::Command(drain_command.clone()),
        };
        apply_command(
            &handle,
            &mut session,
            &identity,
            &journal,
            drain_envelope,
            drain_command.clone(),
            WorkerLifecycleState::Active,
        )
        .await
        .unwrap();
        let WorkerMessage::CommandOutcome(wire_outcome) = fleet_peer.recv().await.unwrap().body
        else {
            panic!("expected drain command outcome")
        };
        assert!(session.terminal_command.is_some());
        assert!(input_rx.recv().await.is_none());
        let CommandDisposition::Duplicate(outcome) = journal.prepare(&drain_command).unwrap()
        else {
            panic!("expected durable drain outcome")
        };
        assert!(outcome.accepted);
        assert!(!outcome.terminal);
        assert_eq!(wire_outcome, outcome);
        pending_task.abort();
    }

    #[test]
    fn welcome_policy_allows_monotonic_grant_changes_and_rejects_identity_drift() {
        fn policy(version: u64, digest: &str, allowed: &[&str]) -> SessionPolicy {
            let mut policy = SessionPolicy {
                allowed_capabilities: allowed.iter().map(|value| (*value).to_string()).collect(),
                attributes: BTreeMap::new(),
            };
            policy
                .set_feature_policy(FeaturePolicy {
                    enabled_features: required_features().into_iter().collect(),
                    policy: PolicyIdentity {
                        version,
                        digest: digest.to_string(),
                    },
                })
                .unwrap();
            policy
        }

        let mut prior = None;
        let first = policy(1, "sha256:first", &["corpus"]);
        validate_welcome_policy(&first, &mut prior).unwrap();
        validate_welcome_policy(&first, &mut prior).unwrap();
        assert!(
            validate_welcome_policy(
                &policy(1, "sha256:first", &["corpus", "blackops.agent"]),
                &mut prior,
            )
            .is_err()
        );
        validate_welcome_policy(&policy(2, "sha256:second", &["blackops.agent"]), &mut prior)
            .unwrap();
        assert!(validate_welcome_policy(&policy(4, "sha256:skip", &[]), &mut prior).is_err());
        assert!(
            validate_welcome_policy(&policy(1, "sha256:first", &["corpus"]), &mut prior).is_err()
        );
    }
}
