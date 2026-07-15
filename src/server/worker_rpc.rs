//! Temporary worker RPC authority hosted by blackboxd during P4.
//!
//! P5 moves this listener and registry adapter into fleetd. Workers only know
//! the socket and authenticated session contract, so that move does not change
//! their protocol.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use bro_protocol::{
    BuildIdentity, CommandOutcomeAck, EventAck, FeaturePolicy, LeaseRenewal, PolicyIdentity,
    ProtocolError, ProtocolErrorCode, ReplayRequest, SessionPolicy, WORKER_PROTOCOL_V1,
    WorkerCommandKind, WorkerFeature, WorkerMessage,
};
use bro_rpc::{
    FleetHandshakeGrant, HandshakeAuthorityReject, HandshakeOptions, MessagePriority, PeerConfig,
    PeerHandle, RpcError, RpcPeer, accept_worker_with_authority,
};
use parking_lot::RwLock;
use sha2::{Digest as _, Sha256};
use tokio::net::{UnixListener, UnixStream};

use super::SharedState;
use crate::orchestration::worker_registry::{
    CommandOutcomePosition, EventPosition, PendingHandshakeDiagnostic, WorkerAuthorityRecord,
    WorkerAuthorityState,
};

const WORKER_SOCKET_ENV: &str = "BLACKBOX_WORKER_SOCKET";
const WORKER_CAPABILITY_SCOPE_ATTRIBUTE: &str = "worker_capability_authority";

fn worker_authority()
-> &'static RwLock<Option<Arc<crate::orchestration::worker_registry::WorkerRegistry>>> {
    static AUTHORITY: OnceLock<
        RwLock<Option<Arc<crate::orchestration::worker_registry::WorkerRegistry>>>,
    > = OnceLock::new();
    AUTHORITY.get_or_init(|| RwLock::new(None))
}

fn worker_endpoint() -> &'static RwLock<Option<PathBuf>> {
    static ENDPOINT: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();
    ENDPOINT.get_or_init(|| RwLock::new(None))
}

#[derive(Clone)]
struct LiveWorker {
    worker_id: bro_core::WorkerId,
    generation: u64,
    handle: PeerHandle,
}

fn live_workers() -> &'static RwLock<HashMap<String, LiveWorker>> {
    static WORKERS: OnceLock<RwLock<HashMap<String, LiveWorker>>> = OnceLock::new();
    WORKERS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn capability_routers()
-> &'static RwLock<HashMap<String, Arc<crate::orchestration::capabilities::WorkerCapabilityRouter>>>
{
    static ROUTERS: OnceLock<
        RwLock<HashMap<String, Arc<crate::orchestration::capabilities::WorkerCapabilityRouter>>>,
    > = OnceLock::new();
    ROUTERS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn remove_capability_router(worker_id: &bro_core::WorkerId) {
    capability_routers().write().remove(worker_id.as_str());
}

pub(crate) fn worker_socket_path(store_dir: &Path) -> PathBuf {
    std::env::var_os(WORKER_SOCKET_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| store_dir.join("run").join("worker.sock"))
}

pub(super) fn start_worker_rpc(shared: Arc<SharedState>) -> anyhow::Result<()> {
    let socket_path = worker_socket_path(&shared.store_dir);
    prepare_socket_parent(&socket_path)?;
    if socket_path.exists() {
        remove_stale_worker_socket(&socket_path)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    set_socket_permissions(&socket_path)?;
    *worker_authority().write() = Some(shared.worker_registry.clone());
    *worker_endpoint().write() = Some(socket_path.clone());
    shared.worker_registry.activate_reattach_window(now_ms())?;
    reconcile_terminal_worker_records(&shared);
    tracing::info!(path = %socket_path.display(), "worker RPC listener ready");
    let accept_state = shared.clone();
    tokio::spawn(async move {
        accept_loop(listener, accept_state).await;
    });
    spawn_terminal_publication_repair(shared.clone());
    spawn_lease_reaper(shared);
    Ok(())
}

fn spawn_terminal_publication_repair(shared: Arc<SharedState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            reconcile_terminal_worker_records(&shared);
            repair_pending_handshake_diagnostics(&shared).await;
            let tasks = { shared.task_store.read().all_tasks() };
            for task in tasks {
                let (task_id, terminal) = {
                    let inner = task.inner.lock();
                    (inner.id.clone(), inner.status.is_terminal())
                };
                if let Err(error) = crate::orchestration::drain_pending_worker_terminal_publication(
                    &task,
                    &shared.tail_tx,
                    &task_id,
                    Some(shared.system_events.clone()),
                    &shared.task_store,
                    &shared.store_dir,
                ) {
                    tracing::warn!(task_id = %task_id, %error, "worker terminal publication repair failed");
                }
                if terminal && let Err(error) = ensure_turn_completion_drain(&shared, &task_id) {
                    tracing::warn!(task_id = %task_id, %error, "worker terminal drain recovery failed");
                }
                tokio::task::yield_now().await;
            }
        }
    });
}

fn reconcile_terminal_worker_records(shared: &SharedState) {
    for record in shared
        .worker_registry
        .records_requiring_task_reconciliation()
    {
        let reason = match record.state {
            WorkerAuthorityState::Lost => {
                "worker authority was lost before task projection completed"
            }
            WorkerAuthorityState::Terminal => {
                "worker reached terminal authority before task projection completed"
            }
            _ => continue,
        };
        if let Err(error) = mark_worker_task_lost(shared, &record.worker_id, reason, true) {
            tracing::warn!(
                worker_id = %record.worker_id.as_str(),
                task_id = %record.task_id.as_str(),
                %error,
                "terminal worker authority reconciliation failed"
            );
        }
    }
}

async fn repair_pending_handshake_diagnostics(shared: &SharedState) {
    for (record, diagnostic) in shared.worker_registry.pending_handshake_diagnostics() {
        if let Err(error) = emit_worker_handshake_event(shared, &record, &diagnostic).await {
            tracing::warn!(
                worker_id = %record.worker_id.as_str(),
                generation = diagnostic.connection_generation,
                %error,
                "worker handshake system event repair failed"
            );
        }
    }
}

pub(crate) fn provision_task_worker(
    task_id: &str,
    session_id: &str,
    cwd: Option<&str>,
    initial_prompt: &str,
) -> anyhow::Result<
    Option<(
        crate::orchestration::worker_registry::WorkerBootstrap,
        PathBuf,
    )>,
> {
    let Some(authority) = worker_authority().read().clone() else {
        return Ok(None);
    };
    let Some(endpoint) = worker_endpoint().read().clone() else {
        return Ok(None);
    };
    let capability_project_root = cwd
        .map(std::path::Path::new)
        .map(std::path::Path::canonicalize)
        .transpose()?;
    if capability_project_root
        .as_ref()
        .is_some_and(|root| !root.is_dir())
    {
        anyhow::bail!("worker capability project root is not a directory");
    }
    let bootstrap = authority.provision_scoped_with_initial_command(
        bro_core::TaskId::new(task_id),
        bro_core::SessionId::new(session_id),
        None,
        capability_project_root,
        Some(WorkerCommandKind::UserTurn {
            text: initial_prompt.to_string(),
        }),
    )?;
    Ok(Some((bootstrap, endpoint)))
}

pub(crate) fn is_available() -> bool {
    worker_authority().read().is_some() && worker_endpoint().read().is_some()
}

pub(crate) fn abandon_task_worker(worker_id: &bro_core::WorkerId) {
    let Some(authority) = worker_authority().read().clone() else {
        return;
    };
    let task_id = authority
        .record(worker_id)
        .map(|record| record.task_id.as_str().to_string());
    if let Err(error) = authority.abandon(worker_id) {
        tracing::warn!(worker_id = %worker_id.as_str(), %error, "failed to abandon worker authority");
    }
    remove_capability_router(worker_id);
    if let Some(task_id) = task_id {
        let live = {
            let mut workers = live_workers().write();
            workers
                .get(&task_id)
                .is_some_and(|live| live.worker_id == *worker_id)
                .then(|| workers.remove(&task_id))
                .flatten()
        };
        if let Some(live) = live {
            live.handle.shutdown();
        }
    }
}

/// Persist a fleet command before offering it to the current connection.
/// `Ok(false)` means the task is not owned by the reconnectable worker path and
/// lets the Stage 1 compatibility channel handle it.
pub(crate) fn dispatch_task_command_if_owned(
    task_id: &str,
    command: WorkerCommandKind,
) -> Result<bool, String> {
    let Some(authority) = worker_authority().read().clone() else {
        return Ok(false);
    };
    let task_id = bro_core::TaskId::new(task_id);
    if !authority.owns_task(&task_id) {
        return Ok(false);
    }
    let (worker_id, command) = authority
        .enqueue_command(&task_id, command)
        .map_err(|error| format!("persisting worker command failed: {error:#}"))?;
    offer_live_command(&task_id, &worker_id, command);
    Ok(true)
}

/// Persist operator cancellation intent and graceful Shutdown in one worker
/// authority snapshot before offering it to a live generation.
pub(crate) fn dispatch_task_cancel_if_owned(
    task_id: &str,
    deadline_unix_ms: u64,
) -> Result<bool, String> {
    let Some(authority) = worker_authority().read().clone() else {
        return Ok(false);
    };
    let task_id = bro_core::TaskId::new(task_id);
    if !authority.owns_task(&task_id) {
        return Ok(false);
    }
    let (worker_id, command) = authority
        .enqueue_cancel_shutdown(&task_id, deadline_unix_ms)
        .map_err(|error| format!("persisting worker cancellation failed: {error:#}"))?;
    offer_live_command(&task_id, &worker_id, command);
    Ok(true)
}

fn ensure_turn_completion_drain(shared: &SharedState, task_id: &str) -> Result<(), String> {
    let task_id = bro_core::TaskId::new(task_id);
    let deadline = now_ms().saturating_add(10_000);
    if let Some((worker_id, command)) = shared
        .worker_registry
        .ensure_turn_completion_drain(&task_id, deadline)
        .map_err(|error| format!("persisting worker drain failed: {error:#}"))?
    {
        offer_live_command(&task_id, &worker_id, command);
    }
    Ok(())
}

fn offer_live_command(
    task_id: &bro_core::TaskId,
    worker_id: &bro_core::WorkerId,
    command: bro_protocol::WorkerCommand,
) {
    let Some(worker) = live_workers().read().get(task_id.as_str()).cloned() else {
        return;
    };
    if worker.worker_id != *worker_id {
        return;
    }
    let priority = command_priority(&command.command);
    if let Err(error) = worker
        .handle
        .send(WorkerMessage::Command(command), priority)
    {
        // Persistence is the acceptance point. A disconnected or saturated
        // live channel is repaired by command replay on the next generation.
        tracing::debug!(task_id = %task_id.as_str(), %error, "durable worker command awaits replay");
        worker.handle.shutdown();
    }
}

fn command_priority(command: &WorkerCommandKind) -> MessagePriority {
    match command {
        WorkerCommandKind::Interrupt
        | WorkerCommandKind::Drain { .. }
        | WorkerCommandKind::Shutdown { .. }
        | WorkerCommandKind::RequestStatus => MessagePriority::Control,
        WorkerCommandKind::UserTurn { .. }
        | WorkerCommandKind::Steer { .. }
        | WorkerCommandKind::AgentMailbox { .. }
        | WorkerCommandKind::SetModel { .. }
        | WorkerCommandKind::Compact => MessagePriority::Normal,
    }
}

fn spawn_lease_reaper(shared: Arc<SharedState>) {
    let authority = shared.worker_registry.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let authority = authority.clone();
            match tokio::task::spawn_blocking(move || authority.expire_stale(now_ms())).await {
                Ok(Ok(expired)) => {
                    for worker_id in expired {
                        tracing::warn!(worker_id = %worker_id.as_str(), "worker lease expired");
                        let shared = shared.clone();
                        let expired_worker = worker_id.clone();
                        if let Err(error) = tokio::task::spawn_blocking(move || {
                            mark_worker_task_lost(
                                &shared,
                                &expired_worker,
                                "worker lease expired before the session could reattach",
                                true,
                            )
                        })
                        .await
                        .unwrap_or_else(|error| {
                            Err(anyhow::anyhow!("worker-loss task panicked: {error}"))
                        }) {
                            tracing::warn!(worker_id = %worker_id.as_str(), %error, "failed to project worker lease loss");
                        }
                    }
                }
                Ok(Err(error)) => tracing::warn!(%error, "worker lease reap failed"),
                Err(error) => tracing::warn!(%error, "worker lease reap task failed"),
            }
        }
    });
}

fn mark_worker_task_lost(
    shared: &SharedState,
    worker_id: &bro_core::WorkerId,
    reason: &str,
    disconnect_live: bool,
) -> anyhow::Result<()> {
    remove_capability_router(worker_id);
    let Some(record) = shared.worker_registry.record(worker_id) else {
        return Ok(());
    };
    let Some(task) = shared.task_store.read().get(record.task_id.as_str()) else {
        return Ok(());
    };
    let changed = {
        let mut inner = task.inner.lock();
        if inner.status != crate::orchestration::TaskStatus::Running {
            false
        } else if record.cancel_requested {
            inner.status = crate::orchestration::TaskStatus::Cancelled;
            inner.completed_at = Some(now_ms());
            inner.exit_code = None;
            inner.recoverable = false;
            true
        } else {
            inner.status = crate::orchestration::TaskStatus::Failed;
            inner.completed_at = Some(now_ms());
            inner.exit_code = None;
            inner.recoverable = true;
            if !inner.stderr.is_empty() && !inner.stderr.ends_with('\n') {
                inner.stderr.push('\n');
            }
            inner.stderr.push_str("[blackbox] ");
            inner.stderr.push_str(reason);
            inner.stderr.push('\n');
            true
        }
    };
    let publication_reserved =
        crate::orchestration::reserve_owner_terminal_publication(&task, record.task_id.as_str());
    if changed || publication_reserved {
        task.emit_roster_updated();
        task.notify.notify_waiters();
        crate::orchestration::flush_persist_blocking(&shared.task_store, &shared.store_dir)?;
    }
    crate::orchestration::drain_pending_worker_terminal_publication(
        &task,
        &shared.tail_tx,
        record.task_id.as_str(),
        Some(shared.system_events.clone()),
        &shared.task_store,
        &shared.store_dir,
    )?;
    if disconnect_live {
        let mut workers = live_workers().write();
        if workers
            .get(record.task_id.as_str())
            .is_some_and(|worker| worker.worker_id == *worker_id)
        {
            if let Some(worker) = workers.remove(record.task_id.as_str()) {
                worker.handle.shutdown();
            }
        }
    }
    Ok(())
}

async fn accept_loop(listener: UnixListener, shared: Arc<SharedState>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(%error, "worker RPC accept failed; listener remains active");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        if let Err(error) = verify_peer_uid(&stream) {
            tracing::warn!(%error, "worker RPC rejected a peer credential");
            continue;
        }
        let shared = shared.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, shared).await {
                tracing::debug!(%error, "worker RPC connection ended");
            }
        });
    }
}

async fn serve_connection(stream: UnixStream, shared: Arc<SharedState>) -> anyhow::Result<()> {
    let registry = shared.worker_registry.clone();
    let handshake_state = shared.clone();
    let (negotiated, hello, welcome) = accept_worker_with_authority(
        stream,
        vec![WORKER_PROTOCOL_V1],
        HandshakeOptions::default(),
        move |hello, selected_protocol| {
            let registry = registry.clone();
            let handshake_state = handshake_state.clone();
            async move {
                validate_worker_build(&hello.worker_build)?;
                if live_workers()
                    .read()
                    .get(hello.task_id.as_str())
                    .is_some_and(|worker| worker.worker_id == hello.worker_id)
                {
                    return Err(HandshakeAuthorityReject::new(
                        "protocol.worker_busy",
                        "worker already has a live connection",
                        "a generation is installed for this worker",
                    ));
                }
                let candidate_registry = registry.clone();
                let candidate_hello = hello.clone();
                tokio::task::spawn_blocking(move || {
                    candidate_registry.validate_handshake_candidate(&candidate_hello)
                })
                .await
                .map_err(|error| {
                    HandshakeAuthorityReject::new(
                        "protocol.authority_unavailable",
                        "worker authority is unavailable",
                        format!("worker authentication task failed: {error}"),
                    )
                })?
                .map_err(registry_handshake_rejection)?;

                let scope_state = handshake_state.clone();
                let scope_hello = hello.clone();
                let scope_result = tokio::task::spawn_blocking(move || {
                    worker_capability_scope(&scope_state, &scope_hello)
                })
                .await
                .map_err(|error| {
                    HandshakeAuthorityReject::new(
                        "protocol.authority_unavailable",
                        "worker capability scope validation failed",
                        format!("worker capability scope task failed: {error}"),
                    )
                })?;
                let scope = match scope_result {
                    Ok(scope) => scope,
                    Err(failure) => {
                        if failure.terminal {
                            terminalize_handshake_failure(
                                handshake_state.clone(),
                                hello.worker_id.clone(),
                                failure.detail.clone(),
                            )
                            .await;
                        }
                        return Err(failure.into_rejection());
                    }
                };
                let proposed_policy = if let Some(policy) = registry
                    .record(&hello.worker_id)
                    .and_then(|record| record.session_policy)
                {
                    let policy_for_validation = policy.clone();
                    let policy_hello = hello.clone();
                    let persisted_scope_result = tokio::task::spawn_blocking(move || {
                        capability_scope_from_policy(&policy_for_validation, &policy_hello)
                    })
                    .await
                    .map_err(|error| {
                        HandshakeAuthorityReject::new(
                            "protocol.authority_unavailable",
                            "worker capability policy validation failed",
                            format!("worker capability policy task failed: {error}"),
                        )
                    })?;
                    let persisted_scope = match persisted_scope_result {
                        Ok(scope) => scope,
                        Err(error) => {
                            let failure = WorkerScopeFailure::terminal_policy(format!(
                                "persisted worker capability scope is invalid: {error:#}"
                            ));
                            terminalize_handshake_failure(
                                handshake_state.clone(),
                                hello.worker_id.clone(),
                                failure.detail.clone(),
                            )
                            .await;
                            return Err(failure.into_rejection());
                        }
                    };
                    if persisted_scope != scope {
                        let failure = WorkerScopeFailure::terminal_policy(
                            "persisted worker capability scope no longer matches task authority",
                        );
                        terminalize_handshake_failure(
                            handshake_state.clone(),
                            hello.worker_id.clone(),
                            failure.detail.clone(),
                        )
                        .await;
                        return Err(failure.into_rejection());
                    }
                    policy
                } else {
                    session_policy(&hello, &scope)?
                };
                let fleet_build = fleet_build_identity();
                let authority_hello = hello.clone();
                let authority_fleet_build = fleet_build.clone();
                let grant = tokio::task::spawn_blocking(move || {
                    registry.accept_handshake_with_policy(
                        &authority_hello,
                        selected_protocol,
                        proposed_policy,
                        authority_fleet_build,
                    )
                })
                .await
                .map_err(|error| {
                    HandshakeAuthorityReject::new(
                        "protocol.authority_unavailable",
                        "worker authority is unavailable",
                        format!("worker authority task failed: {error}"),
                    )
                })?
                .map_err(registry_handshake_rejection)?;
                Ok(FleetHandshakeGrant {
                    connection_generation: grant.connection_generation,
                    event_ack: grant.event_ack,
                    next_command_seq: grant.next_command_seq,
                    lease: grant.lease,
                    reconnect_proof: grant.reconnect_proof,
                    session_policy: grant.session_policy,
                    fleet_build,
                })
            }
        },
    )
    .await?;

    let worker_id = hello.worker_id.clone();
    if let Some(diagnostic) = shared
        .worker_registry
        .record(&worker_id)
        .and_then(|record| {
            record
                .pending_handshake_diagnostics
                .get(&welcome.connection_generation)
                .cloned()
                .map(|diagnostic| (record, diagnostic))
        })
        && let Err(error) = emit_worker_handshake_event(&shared, &diagnostic.0, &diagnostic.1).await
    {
        tracing::warn!(
            worker_id = %worker_id.as_str(),
            generation = welcome.connection_generation,
            %error,
            "worker handshake system event emit failed; durable repair remains pending"
        );
    }
    if let Some(task) = shared.task_store.read().get(hello.task_id.as_str()) {
        crate::orchestration::drain_pending_worker_terminal_publication(
            &task,
            &shared.tail_tx,
            hello.task_id.as_str(),
            Some(shared.system_events.clone()),
            &shared.task_store,
            &shared.store_dir,
        )?;
        if task.inner.lock().status.is_terminal() {
            ensure_turn_completion_drain(&shared, hello.task_id.as_str())
                .map_err(anyhow::Error::msg)?;
        }
    }
    let capability_scope = capability_scope_from_policy(&welcome.session_policy, &hello)?;
    let capability_router = {
        let mut routers = capability_routers().write();
        if let Some(existing) = routers.get(hello.worker_id.as_str()) {
            if existing.scope() != &capability_scope {
                anyhow::bail!("worker capability scope changed across reconnect generations");
            }
            existing.clone()
        } else {
            let router = Arc::new(
                crate::orchestration::capabilities::WorkerCapabilityRouter::new(
                    shared.clone(),
                    &welcome.session_policy,
                    capability_scope,
                ),
            );
            routers.insert(hello.worker_id.as_str().to_string(), router.clone());
            router
        }
    };
    let mut peer = RpcPeer::spawn(negotiated, PeerConfig::default())?;
    let handle = peer.handle();
    let generation = handle.binding().connection_generation;
    live_workers().write().insert(
        hello.task_id.as_str().to_string(),
        LiveWorker {
            worker_id: worker_id.clone(),
            generation,
            handle: handle.clone(),
        },
    );
    if welcome.event_ack < hello.last_local_event_seq {
        handle.send(
            WorkerMessage::ReplayRequest(ReplayRequest {
                from_event_seq: welcome.event_ack.saturating_add(1),
            }),
            MessagePriority::Control,
        )?;
    }
    for command in shared.worker_registry.pending_commands(&worker_id)? {
        let priority = command_priority(&command.command);
        handle.send(WorkerMessage::Command(command), priority)?;
    }
    while let Ok(envelope) = peer.recv().await {
        let message_id = envelope.message_id.clone();
        let application =
            handle_message(&shared, &hello, &peer, &capability_router, envelope).await;
        match application {
            Ok(()) => {
                let registry = shared.worker_registry.clone();
                let successful_worker = worker_id.clone();
                if let Err(error) = tokio::task::spawn_blocking(move || {
                    registry.clear_retryable_message_apply_failures(&successful_worker, generation)
                })
                .await?
                {
                    tracing::warn!(worker_id = %worker_id.as_str(), %error, "worker replay failure counter could not be cleared");
                    peer.handle().shutdown();
                    break;
                }
            }
            Err(error) => {
                match classify_message_apply_error(&error) {
                    MessageApplyFailureClass::Deterministic => {
                        let reason = format!("worker sent an invalid message: {error:#}");
                        let _ = peer.handle().send_protocol_error(ProtocolError {
                            code: ProtocolErrorCode::InvalidPayload,
                            message: "worker message violated the durable session contract"
                                .to_string(),
                            fatal: true,
                            related_message_id: Some(message_id),
                            expected_protocol_version: Some(WORKER_PROTOCOL_V1),
                            expected_connection_generation: Some(generation),
                        });
                        terminalize_message_failure(shared.clone(), worker_id.clone(), reason)
                            .await;
                        peer.handle().shutdown();
                        tracing::warn!(worker_id = %worker_id.as_str(), %error, "invalid worker message terminalized the session");
                    }
                    MessageApplyFailureClass::Retryable => {
                        let registry = shared.worker_registry.clone();
                        let failed_worker = worker_id.clone();
                        let attempts = tokio::task::spawn_blocking(move || {
                            registry
                                .record_retryable_message_apply_failure(&failed_worker, generation)
                        })
                        .await?;
                        match attempts {
                            Ok(attempts)
                                if attempts
                                    < shared
                                        .worker_registry
                                        .retryable_message_apply_failure_limit() =>
                            {
                                let _ = peer.handle().send_protocol_error(ProtocolError {
                                    code: ProtocolErrorCode::Internal,
                                    message:
                                        "worker message application failed and will be replayed"
                                            .to_string(),
                                    fatal: true,
                                    related_message_id: Some(message_id),
                                    expected_protocol_version: Some(WORKER_PROTOCOL_V1),
                                    expected_connection_generation: Some(generation),
                                });
                                peer.handle().shutdown();
                                tracing::warn!(worker_id = %worker_id.as_str(), attempts, %error, "worker message apply failed; closing generation for bounded replay");
                            }
                            Ok(attempts) => {
                                let reason = format!(
                                    "worker message application failed after {attempts} attempts: {error:#}"
                                );
                                let _ = peer.handle().send_protocol_error(ProtocolError {
                                    code: ProtocolErrorCode::Internal,
                                    message: "worker message replay budget was exhausted"
                                        .to_string(),
                                    fatal: true,
                                    related_message_id: Some(message_id),
                                    expected_protocol_version: Some(WORKER_PROTOCOL_V1),
                                    expected_connection_generation: Some(generation),
                                });
                                terminalize_message_failure(
                                    shared.clone(),
                                    worker_id.clone(),
                                    reason,
                                )
                                .await;
                                peer.handle().shutdown();
                                tracing::warn!(worker_id = %worker_id.as_str(), attempts, %error, "worker message replay budget exhausted");
                            }
                            Err(counter_error) => {
                                let reason = format!(
                                    "worker replay authority could not persist a bounded failure: {counter_error:#}; original error: {error:#}"
                                );
                                terminalize_message_failure(
                                    shared.clone(),
                                    worker_id.clone(),
                                    reason,
                                )
                                .await;
                                peer.handle().shutdown();
                            }
                        }
                    }
                }
                break;
            }
        }
    }
    {
        let mut workers = live_workers().write();
        if workers
            .get(hello.task_id.as_str())
            .is_some_and(|worker| worker.generation == generation)
        {
            workers.remove(hello.task_id.as_str());
        }
    }
    let registry = shared.worker_registry.clone();
    let disconnected_worker = worker_id.clone();
    let disconnected_generation = generation;
    tokio::task::spawn_blocking(move || {
        registry.mark_disconnected(&disconnected_worker, disconnected_generation)
    })
    .await??;
    Ok(())
}

async fn emit_worker_handshake_event(
    shared: &SharedState,
    record: &WorkerAuthorityRecord,
    diagnostic: &PendingHandshakeDiagnostic,
) -> anyhow::Result<()> {
    let mut correlation = serde_json::Map::new();
    correlation.insert(
        "worker_id".to_string(),
        serde_json::json!(record.worker_id.as_str()),
    );
    correlation.insert(
        "task_id".to_string(),
        serde_json::json!(record.task_id.as_str()),
    );
    correlation.insert(
        "session_id".to_string(),
        serde_json::json!(record.session_id.as_str()),
    );
    let draft = crate::system_events::SystemEventDraft {
        kind: crate::system_events::types::SystemEventKind::Unknown(
            "worker.handshake.accepted".to_string(),
        ),
        producer: "fleet.worker_rpc".to_string(),
        project: None,
        principal: None,
        subject: Some(crate::system_events::types::EventSubject {
            kind: "worker".to_string(),
            id: record.worker_id.as_str().to_string(),
        }),
        correlation,
        causation_id: None,
        payload: serde_json::json!({
            "protocol_version": diagnostic.protocol_version,
            "connection_generation": diagnostic.connection_generation,
            "worker_build": diagnostic.worker_build,
            "fleet_build": diagnostic.fleet_build,
        }),
    };
    let digest = Sha256::digest(record.worker_id.as_str().as_bytes());
    let event_id = format!(
        "worker-handshake:{}:{}",
        hex::encode(&digest[..16]),
        diagnostic.connection_generation
    );
    shared.system_events.emit_once(event_id, draft).await?;
    shared
        .worker_registry
        .clear_pending_handshake_diagnostic(&record.worker_id, diagnostic.connection_generation)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageApplyFailureClass {
    Deterministic,
    Retryable,
}

fn classify_message_apply_error(error: &anyhow::Error) -> MessageApplyFailureClass {
    if let Some(rpc) = error.downcast_ref::<RpcError>() {
        return match rpc {
            RpcError::Io(_)
            | RpcError::Timeout { .. }
            | RpcError::QueueFull { .. }
            | RpcError::TooManyInFlightRequests { .. }
            | RpcError::Disconnected { .. }
            | RpcError::ResponseChannelClosed => MessageApplyFailureClass::Retryable,
            RpcError::FrameTooLarge { .. }
            | RpcError::InvalidJson(_)
            | RpcError::UnexpectedHandshake(_)
            | RpcError::HandshakeRejected { .. }
            | RpcError::HandshakeAuthorityRejected { .. }
            | RpcError::VersionMismatch
            | RpcError::SelectedProtocolNotOffered { .. }
            | RpcError::Authentication(_)
            | RpcError::InvalidConfiguration(_)
            | RpcError::ProtocolVersionMismatch { .. }
            | RpcError::StaleGeneration { .. }
            | RpcError::InvalidMessageId { .. }
            | RpcError::CorrelationMismatch(_)
            | RpcError::DuplicateMessageId { .. } => MessageApplyFailureClass::Deterministic,
        };
    }
    if let Some(io) = error.downcast_ref::<std::io::Error>() {
        return match io.kind() {
            std::io::ErrorKind::InvalidData
            | std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::PermissionDenied => MessageApplyFailureClass::Deterministic,
            _ => MessageApplyFailureClass::Retryable,
        };
    }
    if error.downcast_ref::<serde_json::Error>().is_some() {
        return MessageApplyFailureClass::Deterministic;
    }
    if error.downcast_ref::<tokio::task::JoinError>().is_some() {
        return MessageApplyFailureClass::Retryable;
    }
    // Domain errors without a typed transient cause are session-contract
    // violations. Defaulting them to retry would make malformed events poison
    // every reconnect generation forever.
    MessageApplyFailureClass::Deterministic
}

async fn terminalize_message_failure(
    shared: Arc<SharedState>,
    worker_id: bro_core::WorkerId,
    reason: String,
) {
    let registry = shared.worker_registry.clone();
    let abandoned_worker = worker_id.clone();
    match tokio::task::spawn_blocking(move || registry.abandon(&abandoned_worker)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(worker_id = %worker_id.as_str(), %error, "failed to abandon invalid worker message")
        }
        Err(error) => {
            tracing::warn!(worker_id = %worker_id.as_str(), %error, "worker message abandonment task failed")
        }
    }
    let task_state = shared.clone();
    let failed_worker = worker_id.clone();
    match tokio::task::spawn_blocking(move || {
        mark_worker_task_lost(&task_state, &failed_worker, &reason, false)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(worker_id = %worker_id.as_str(), %error, "failed to project invalid worker message")
        }
        Err(error) => {
            tracing::warn!(worker_id = %worker_id.as_str(), %error, "worker message projection task failed")
        }
    }
}

async fn handle_message(
    shared: &Arc<SharedState>,
    hello: &bro_protocol::WorkerHello,
    peer: &RpcPeer,
    capability_router: &Arc<crate::orchestration::capabilities::WorkerCapabilityRouter>,
    envelope: bro_protocol::Envelope,
) -> anyhow::Result<()> {
    let handle = peer.handle();
    if shared
        .worker_registry
        .record(&hello.worker_id)
        .is_some_and(|record| record.bootstrap_proof_hash.is_some())
    {
        let registry = shared.worker_registry.clone();
        let worker_id = hello.worker_id.clone();
        let generation = handle.binding().connection_generation;
        tokio::task::spawn_blocking(move || registry.confirm_connection(&worker_id, generation))
            .await??;
    }
    // Retain the validated request envelope for response correlation. Worker
    // payloads are frame-bounded, so this clone is also bounded by bro-rpc.
    match envelope.body.clone() {
        WorkerMessage::Event(event) => {
            let task = shared
                .task_store
                .read()
                .get(hello.task_id.as_str())
                .ok_or_else(|| anyhow::anyhow!("worker task is not present"))?;
            let provider = task.inner.lock().provider;
            let event_seq = event.event_seq;
            let event_payload = event.event;
            let tail_tx = shared.tail_tx.clone();
            let task_id = hello.task_id.as_str().to_string();
            let system_events = shared.system_events.clone();
            let task_store = shared.task_store.clone();
            let store_dir = shared.store_dir.clone();
            let task_for_projection = task.clone();
            let projection = tokio::task::spawn_blocking(move || {
                crate::orchestration::ingest_worker_event_durably(
                    &task_for_projection,
                    provider,
                    event_payload,
                    event_seq,
                    &tail_tx,
                    &task_id,
                    Some(system_events),
                    &task_store,
                    &store_dir,
                )
            })
            .await??;
            let terminal_result = match projection {
                crate::orchestration::WorkerEventProjection::Gap { expected_event_seq } => {
                    handle.send(
                        WorkerMessage::ReplayRequest(ReplayRequest {
                            from_event_seq: expected_event_seq,
                        }),
                        MessagePriority::Control,
                    )?;
                    return Ok(());
                }
                crate::orchestration::WorkerEventProjection::Duplicate {
                    terminal_result, ..
                }
                | crate::orchestration::WorkerEventProjection::Applied { terminal_result } => {
                    terminal_result
                }
            };
            let registry = shared.worker_registry.clone();
            let worker_id = hello.worker_id.clone();
            let generation = handle.binding().connection_generation;
            let position = tokio::task::spawn_blocking(move || {
                registry.observe_event(&worker_id, generation, event_seq)
            })
            .await??;
            if let EventPosition::Gap { expected } = position {
                handle.send(
                    WorkerMessage::ReplayRequest(ReplayRequest {
                        from_event_seq: expected,
                    }),
                    MessagePriority::Control,
                )?;
                return Ok(());
            }
            let through_event_seq = shared
                .worker_registry
                .record(&hello.worker_id)
                .ok_or_else(|| anyhow::anyhow!("worker record disappeared"))?
                .event_ack;
            handle.send(
                WorkerMessage::EventAck(EventAck { through_event_seq }),
                MessagePriority::Control,
            )?;
            if terminal_result {
                ensure_turn_completion_drain(shared, hello.task_id.as_str())
                    .map_err(anyhow::Error::msg)?;
            }
        }
        WorkerMessage::Heartbeat(heartbeat) => {
            let registry = shared.worker_registry.clone();
            let worker_id = hello.worker_id.clone();
            let lease_id = heartbeat.lease_id;
            let generation = handle.binding().connection_generation;
            let lease = tokio::task::spawn_blocking(move || {
                registry.renew_lease(&worker_id, generation, &lease_id)
            })
            .await??;
            let renewed_at = now_ms();
            handle.send(
                WorkerMessage::LeaseRenewal(LeaseRenewal {
                    lease_id: lease.lease_id,
                    renewed_at_unix_ms: renewed_at,
                    expires_at_unix_ms: lease.expires_at_unix_ms,
                    next_heartbeat_due_unix_ms: renewed_at
                        .saturating_add(lease.heartbeat_interval_ms),
                }),
                MessagePriority::Control,
            )?;
        }
        WorkerMessage::CommandOutcome(outcome) => {
            let session_failed = outcome.terminal
                && outcome
                    .result_or_error
                    .get("code")
                    .and_then(serde_json::Value::as_str)
                    == Some("worker.session_failed");
            let registry = shared.worker_registry.clone();
            let worker_id = hello.worker_id.clone();
            let outcome_for_registry = outcome.clone();
            let generation = handle.binding().connection_generation;
            let position = tokio::task::spawn_blocking(move || {
                registry.observe_command_outcome(&worker_id, generation, &outcome_for_registry)
            })
            .await??;
            let through_command_seq = match position {
                CommandOutcomePosition::Duplicate {
                    through_command_seq,
                }
                | CommandOutcomePosition::Recorded {
                    through_command_seq,
                } => through_command_seq,
            };
            if through_command_seq > 0 {
                handle.send(
                    WorkerMessage::CommandOutcomeAck(CommandOutcomeAck {
                        through_command_seq,
                    }),
                    MessagePriority::Control,
                )?;
            }
            if shared
                .worker_registry
                .record(&hello.worker_id)
                .is_some_and(|record| {
                    record.state
                        == crate::orchestration::worker_registry::WorkerAuthorityState::Terminal
                })
            {
                remove_capability_router(&hello.worker_id);
            }
            if session_failed {
                let shared_for_failure = shared.clone();
                let worker_id = hello.worker_id.clone();
                tokio::task::spawn_blocking(move || {
                    mark_worker_task_lost(
                        &shared_for_failure,
                        &worker_id,
                        "worker session runtime failed before a durable result",
                        false,
                    )
                })
                .await??;
            }
        }
        WorkerMessage::CapabilityRequest(request) => {
            let authorized = shared
                .worker_registry
                .record(&hello.worker_id)
                .is_some_and(|record| {
                    record.connection_generation == handle.binding().connection_generation
                        && matches!(
                            record.state,
                            crate::orchestration::worker_registry::WorkerAuthorityState::Active
                                | crate::orchestration::worker_registry::WorkerAuthorityState::Draining
                        )
                });
            if !authorized {
                anyhow::bail!("worker capability authority is no longer active");
            }
            let router = capability_router.clone();
            let response_handle = handle.clone();
            tokio::spawn(async move {
                let response = router.dispatch(request).await;
                if let Err(error) = response_handle.respond(
                    &envelope,
                    WorkerMessage::CapabilityResponse(response),
                    MessagePriority::Normal,
                ) {
                    tracing::debug!(%error, "capability response could not be delivered");
                }
            });
        }
        WorkerMessage::Status(status) => {
            let registry = shared.worker_registry.clone();
            let worker_id = hello.worker_id.clone();
            let generation = handle.binding().connection_generation;
            let status_for_registry = status.clone();
            let observed = tokio::task::spawn_blocking(move || {
                registry.observe_status(&worker_id, generation, &status_for_registry)
            })
            .await??;
            if observed == crate::orchestration::worker_registry::WorkerAuthorityState::Terminal {
                let shared_for_terminal = shared.clone();
                let worker_id = hello.worker_id.clone();
                tokio::task::spawn_blocking(move || {
                    mark_worker_task_lost(
                        &shared_for_terminal,
                        &worker_id,
                        "worker terminated without a durable result event",
                        false,
                    )
                })
                .await??;
            }
        }
        WorkerMessage::ReplayUnavailable(unavailable) => {
            remove_capability_router(&hello.worker_id);
            let registry = shared.worker_registry.clone();
            let worker_id = hello.worker_id.clone();
            let lost_worker = worker_id.clone();
            tokio::task::spawn_blocking(move || registry.abandon(&lost_worker)).await??;
            let shared_for_loss = shared.clone();
            let reason = format!(
                "worker replay is unavailable from event {} (available {} through {})",
                unavailable.requested_from_event_seq,
                unavailable.earliest_available_event_seq,
                unavailable.latest_available_event_seq
            );
            tokio::task::spawn_blocking(move || {
                mark_worker_task_lost(&shared_for_loss, &worker_id, &reason, true)
            })
            .await??;
            handle.send_protocol_error(ProtocolError {
                code: ProtocolErrorCode::SequenceGap,
                message: "required worker event replay is unavailable".to_string(),
                fatal: true,
                related_message_id: Some(envelope.message_id),
                expected_protocol_version: Some(handle.binding().protocol_version),
                expected_connection_generation: Some(handle.binding().connection_generation),
            })?;
            handle.shutdown();
        }
        WorkerMessage::ProtocolError(error) => {
            tracing::warn!(
                worker_id = %hello.worker_id.as_str(),
                code = ?error.code,
                fatal = error.fatal,
                "worker reported a protocol error"
            );
            if error.fatal {
                let registry = shared.worker_registry.clone();
                let worker_id = hello.worker_id.clone();
                let abandoned_worker = worker_id.clone();
                tokio::task::spawn_blocking(move || registry.abandon(&abandoned_worker)).await??;
                let shared_for_failure = shared.clone();
                let reason = format!("worker reported fatal protocol error: {}", error.message);
                tokio::task::spawn_blocking(move || {
                    mark_worker_task_lost(&shared_for_failure, &worker_id, &reason, true)
                })
                .await??;
                handle.shutdown();
            }
        }
        WorkerMessage::EventAck(_)
        | WorkerMessage::ReplayRequest(_)
        | WorkerMessage::Command(_)
        | WorkerMessage::ServicePolicy(_)
        | WorkerMessage::CommandOutcomeAck(_)
        | WorkerMessage::CapabilityResponse(_)
        | WorkerMessage::LeaseRenewal(_)
        | WorkerMessage::DrainAck => {
            anyhow::bail!("message direction is invalid for a worker");
        }
    }
    Ok(())
}

#[derive(Debug)]
struct WorkerScopeFailure {
    code: &'static str,
    title: &'static str,
    detail: String,
    terminal: bool,
}

impl WorkerScopeFailure {
    fn authentication(detail: impl Into<String>, terminal: bool) -> Self {
        Self {
            code: "protocol.authentication_failed",
            title: "worker task authority is unavailable",
            detail: detail.into(),
            terminal,
        }
    }

    fn terminal_policy(detail: impl Into<String>) -> Self {
        Self {
            code: "protocol.policy_mismatch",
            title: "worker capability scope is unavailable",
            detail: detail.into(),
            terminal: true,
        }
    }

    fn into_rejection(self) -> HandshakeAuthorityReject {
        HandshakeAuthorityReject::new(self.code, self.title, self.detail)
    }
}

async fn terminalize_handshake_failure(
    shared: Arc<SharedState>,
    worker_id: bro_core::WorkerId,
    reason: String,
) {
    let registry = shared.worker_registry.clone();
    let abandoned_worker = worker_id.clone();
    match tokio::task::spawn_blocking(move || registry.abandon(&abandoned_worker)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(worker_id = %worker_id.as_str(), %error, "failed to abandon invalid worker handshake")
        }
        Err(error) => {
            tracing::warn!(worker_id = %worker_id.as_str(), %error, "worker handshake abandonment task failed")
        }
    }
    let task_state = shared.clone();
    let failed_worker = worker_id.clone();
    match tokio::task::spawn_blocking(move || {
        mark_worker_task_lost(&task_state, &failed_worker, &reason, true)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(worker_id = %worker_id.as_str(), %error, "failed to project invalid worker handshake")
        }
        Err(error) => {
            tracing::warn!(worker_id = %worker_id.as_str(), %error, "worker handshake projection task failed")
        }
    }
}

fn worker_capability_scope(
    shared: &SharedState,
    hello: &bro_protocol::WorkerHello,
) -> Result<crate::orchestration::capabilities::WorkerCapabilityScope, WorkerScopeFailure> {
    let record = shared
        .worker_registry
        .record(&hello.worker_id)
        .ok_or_else(|| {
            WorkerScopeFailure::authentication("worker identity is not provisioned", false)
        })?;
    if record.task_id != hello.task_id || record.session_id != hello.session_id {
        return Err(WorkerScopeFailure::authentication(
            "worker registry identity does not match capability scope",
            false,
        ));
    }
    let task = shared
        .task_store
        .read()
        .get(hello.task_id.as_str())
        .ok_or_else(|| {
            WorkerScopeFailure::authentication(
                "worker task projection is missing from fleet authority",
                true,
            )
        })?;
    let inner = task.inner.lock();
    if inner.session_id != hello.session_id.as_str() {
        return Err(WorkerScopeFailure::authentication(
            "worker task session does not match capability scope",
            true,
        ));
    }
    let task_project_root = inner
        .cwd
        .as_deref()
        .map(std::path::Path::new)
        .map(std::path::Path::canonicalize)
        .transpose()
        .map_err(|error| {
            WorkerScopeFailure::terminal_policy(format!(
                "worker task project authority cannot be resolved: {error}"
            ))
        })?;
    if task_project_root
        .as_ref()
        .is_some_and(|root| !root.is_dir())
    {
        return Err(WorkerScopeFailure::terminal_policy(
            "worker task project authority is not a directory",
        ));
    }
    let registry_project_root = record
        .capability_project_root
        .map(|project_root| {
            let canonical = project_root.canonicalize().map_err(|error| {
                WorkerScopeFailure::terminal_policy(format!(
                    "worker registry project authority cannot be resolved: {error}"
                ))
            })?;
            if canonical != project_root || !canonical.is_dir() {
                return Err(WorkerScopeFailure::terminal_policy(
                    "worker registry project authority is no longer stable",
                ));
            }
            Ok(project_root)
        })
        .transpose()?;
    if registry_project_root.is_some() && registry_project_root != task_project_root {
        return Err(WorkerScopeFailure::terminal_policy(
            "worker registry project authority does not match the task projection",
        ));
    }
    Ok(crate::orchestration::capabilities::WorkerCapabilityScope {
        task_id: hello.task_id.as_str().to_string(),
        session_id: hello.session_id.as_str().to_string(),
        project_root: registry_project_root.or(task_project_root),
    })
}

fn session_policy(
    hello: &bro_protocol::WorkerHello,
    scope: &crate::orchestration::capabilities::WorkerCapabilityScope,
) -> Result<SessionPolicy, HandshakeAuthorityReject> {
    let supported = [
        WorkerFeature::ORDERED_REPLAY,
        WorkerFeature::COMMAND_IDEMPOTENCY,
        WorkerFeature::GENERATION_FENCING,
        WorkerFeature::CAPABILITY_RPC,
        WorkerFeature::LEASE_RENEWAL,
        WorkerFeature::DRAIN_SHUTDOWN,
    ];
    let offered = hello.offered_protocol_features();
    let required = supported
        .into_iter()
        .map(WorkerFeature::new)
        .collect::<std::collections::BTreeSet<_>>();
    if !required.is_subset(&offered) {
        let missing = required
            .difference(&offered)
            .map(WorkerFeature::as_str)
            .collect::<Vec<_>>();
        return Err(HandshakeAuthorityReject::new(
            "protocol.policy_mismatch",
            "worker is missing required persistent-session features",
            format!("missing required worker features: {missing:?}"),
        ));
    }
    let enabled_features = supported
        .into_iter()
        .map(WorkerFeature::new)
        .filter(|feature| offered.contains(feature))
        .collect();
    let mut allowed_capabilities = vec!["corpus".to_string()];
    if scope.project_root.is_some() {
        allowed_capabilities.extend(["atom".to_string(), "refactor".to_string()]);
    }
    let digest = hex::encode(Sha256::digest(
        serde_json::to_vec(&(allowed_capabilities.as_slice(), &enabled_features, scope))
            .unwrap_or_default(),
    ));
    let mut policy = SessionPolicy {
        allowed_capabilities,
        attributes: Default::default(),
    };
    policy.attributes.insert(
        WORKER_CAPABILITY_SCOPE_ATTRIBUTE.to_string(),
        serde_json::to_value(scope).map_err(|error| {
            HandshakeAuthorityReject::new(
                "protocol.authority_unavailable",
                "worker capability scope could not be encoded",
                error.to_string(),
            )
        })?,
    );
    let _ = policy.set_feature_policy(FeaturePolicy {
        enabled_features,
        policy: PolicyIdentity {
            version: 1,
            digest: format!("sha256:{digest}"),
        },
    });
    Ok(policy)
}

fn validate_worker_build(build: &BuildIdentity) -> Result<(), HandshakeAuthorityReject> {
    let fleet = semver::Version::parse(env!("CARGO_PKG_VERSION")).map_err(|error| {
        HandshakeAuthorityReject::new(
            "protocol.authority_unavailable",
            "fleet build version is invalid",
            error.to_string(),
        )
    })?;
    validate_worker_build_against(&fleet, build)
}

fn fleet_build_identity() -> BuildIdentity {
    BuildIdentity {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_id: env!("BLACKBOX_BUILD_ID").to_string(),
    }
}

fn validate_worker_build_against(
    fleet: &semver::Version,
    build: &BuildIdentity,
) -> Result<(), HandshakeAuthorityReject> {
    let worker = semver::Version::parse(&build.version).map_err(|error| {
        HandshakeAuthorityReject::new(
            "protocol.unsupported_build",
            "worker build version is invalid",
            error.to_string(),
        )
    })?;
    let supported = if fleet.major == 0 || worker.major == 0 {
        fleet.major == worker.major && fleet.minor == worker.minor
    } else {
        fleet.major == worker.major && fleet.minor.abs_diff(worker.minor) <= 1
    };
    if !supported || build.build_id.is_empty() || build.build_id.len() > 128 {
        return Err(HandshakeAuthorityReject::new(
            "protocol.unsupported_build",
            "worker build is outside the supported rolling window",
            format!(
                "fleet {} does not support worker {} ({})",
                fleet, worker, build.build_id
            ),
        ));
    }
    Ok(())
}

fn capability_scope_from_policy(
    policy: &SessionPolicy,
    hello: &bro_protocol::WorkerHello,
) -> anyhow::Result<crate::orchestration::capabilities::WorkerCapabilityScope> {
    let value = policy
        .attributes
        .get(WORKER_CAPABILITY_SCOPE_ATTRIBUTE)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("session policy omitted worker capability authority"))?;
    let scope: crate::orchestration::capabilities::WorkerCapabilityScope =
        serde_json::from_value(value)?;
    if scope.task_id != hello.task_id.as_str() || scope.session_id != hello.session_id.as_str() {
        anyhow::bail!("session policy capability authority identity mismatch");
    }
    if let Some(project_root) = &scope.project_root {
        let canonical = project_root.canonicalize()?;
        if canonical != *project_root || !canonical.is_dir() {
            anyhow::bail!("session policy project authority is no longer stable");
        }
    }
    Ok(scope)
}

fn registry_handshake_rejection(error: anyhow::Error) -> HandshakeAuthorityReject {
    let detail = error.to_string();
    if detail.contains("live connection generation") {
        return HandshakeAuthorityReject::new(
            "protocol.worker_busy",
            "worker already has a live connection",
            detail,
        );
    }
    if detail.contains("proof")
        || detail.contains("identity")
        || detail.contains("not provisioned")
        || detail.contains("terminal")
        || detail.contains("command sequence")
    {
        return HandshakeAuthorityReject::new(
            "protocol.authentication_failed",
            "worker authentication failed",
            detail,
        );
    }
    if detail.contains("session policy") || detail.contains("policy features") {
        return HandshakeAuthorityReject::new(
            "protocol.policy_mismatch",
            "worker session policy is incompatible",
            detail,
        );
    }
    HandshakeAuthorityReject::new(
        "protocol.authority_unavailable",
        "worker authority is temporarily unavailable",
        detail,
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn prepare_socket_parent(socket_path: &Path) -> anyhow::Result<()> {
    let parent = socket_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("worker socket path has no parent"))?;
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            anyhow::bail!("worker socket parent is not a real directory");
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(parent)?;
        }
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if std::fs::symlink_metadata(parent)?.uid() != unsafe { libc::geteuid() } {
            anyhow::bail!("worker socket parent has a different owner");
        }
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_socket_permissions(socket_path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn remove_stale_worker_socket(socket_path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(socket_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
        if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
            anyhow::bail!(
                "worker socket path is not a socket: {}",
                socket_path.display()
            );
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            anyhow::bail!(
                "worker socket path has a different owner: {}",
                socket_path.display()
            );
        }
    }
    match std::os::unix::net::UnixStream::connect(socket_path) {
        Ok(_) => anyhow::bail!(
            "worker socket already has a live listener: {}",
            socket_path.display()
        ),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            std::fs::remove_file(socket_path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn verify_peer_uid(stream: &UnixStream) -> anyhow::Result<()> {
    let credential = stream.peer_cred()?;
    if credential.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!("peer UID does not match daemon UID");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_running_task(
        state: &SharedState,
        task_id: &str,
        session_id: &str,
        cwd: Option<&Path>,
    ) {
        let task = crate::orchestration::test_task(
            task_id,
            crate::orchestration::TaskStatus::Running,
            bro_core::Provider::Glm,
        );
        {
            let mut inner = task.inner.lock();
            inner.session_id = session_id.to_string();
            inner.cwd = cwd.map(|path| path.to_string_lossy().into_owned());
            inner.completed_at = None;
            inner.exit_code = None;
        }
        state
            .task_store
            .write()
            .insert(task_id.to_string(), task)
            .unwrap();
    }

    fn worker_hello(
        bootstrap: &crate::orchestration::worker_registry::WorkerBootstrap,
        proof: bro_protocol::AuthenticationProof,
    ) -> bro_protocol::WorkerHello {
        let mut hello = bro_protocol::WorkerHello {
            protocol_versions: vec![WORKER_PROTOCOL_V1],
            worker_build: BuildIdentity {
                version: env!("CARGO_PKG_VERSION").to_string(),
                build_id: "worker-test".to_string(),
            },
            worker_id: bootstrap.worker_id.clone(),
            task_id: bootstrap.task_id.clone(),
            session_id: bootstrap.session_id.clone(),
            bootstrap_or_resume_proof: proof,
            last_local_event_seq: 0,
            last_fleet_command_seq: 0,
            worker_capabilities: Vec::new(),
        };
        hello.set_offered_protocol_features([
            WorkerFeature::new(WorkerFeature::ORDERED_REPLAY),
            WorkerFeature::new(WorkerFeature::COMMAND_IDEMPOTENCY),
            WorkerFeature::new(WorkerFeature::GENERATION_FENCING),
            WorkerFeature::new(WorkerFeature::CAPABILITY_RPC),
            WorkerFeature::new(WorkerFeature::LEASE_RENEWAL),
            WorkerFeature::new(WorkerFeature::DRAIN_SHUTDOWN),
        ]);
        hello
    }

    #[test]
    fn worker_socket_parent_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let socket = root.join("run").join("worker.sock");
        prepare_socket_parent(&socket).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(socket.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn live_worker_socket_is_never_unlinked_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let socket = root.join("worker.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let error = remove_stale_worker_socket(&socket).unwrap_err();
        assert!(error.to_string().contains("live listener"));
        assert!(socket.exists());
    }

    #[test]
    fn refused_worker_socket_is_removed_before_rebind() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let socket = root.join("worker.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        drop(listener);
        remove_stale_worker_socket(&socket).unwrap();
        assert!(!socket.exists());
    }

    #[test]
    fn policy_selects_only_offered_protocol_features() {
        let mut hello = bro_protocol::WorkerHello {
            protocol_versions: vec![WORKER_PROTOCOL_V1],
            worker_build: BuildIdentity {
                version: "test".to_string(),
                build_id: "worker".to_string(),
            },
            worker_id: bro_core::WorkerId::new("worker-1"),
            task_id: bro_core::TaskId::new("task-1"),
            session_id: bro_core::SessionId::new("session-1"),
            bootstrap_or_resume_proof: bro_protocol::AuthenticationProof::new("secret"),
            last_local_event_seq: 0,
            last_fleet_command_seq: 0,
            worker_capabilities: Vec::new(),
        };
        hello.set_offered_protocol_features([
            WorkerFeature::new(WorkerFeature::ORDERED_REPLAY),
            WorkerFeature::new(WorkerFeature::COMMAND_IDEMPOTENCY),
            WorkerFeature::new(WorkerFeature::GENERATION_FENCING),
            WorkerFeature::new(WorkerFeature::CAPABILITY_RPC),
            WorkerFeature::new(WorkerFeature::LEASE_RENEWAL),
            WorkerFeature::new(WorkerFeature::DRAIN_SHUTDOWN),
        ]);
        let policy = session_policy(
            &hello,
            &crate::orchestration::capabilities::WorkerCapabilityScope {
                task_id: "task-1".to_string(),
                session_id: "session-1".to_string(),
                project_root: None,
            },
        )
        .unwrap()
        .feature_policy()
        .unwrap()
        .unwrap();
        assert_eq!(policy.enabled_features.len(), 6);
        assert!(
            policy
                .enabled_features
                .contains(&WorkerFeature::new(WorkerFeature::ORDERED_REPLAY))
        );
    }

    #[tokio::test]
    async fn authenticated_session_routes_capabilities_and_durable_commands() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        *worker_authority().write() = Some(state.worker_registry.clone());
        insert_running_task(&state, "rpc-task", "rpc-session", None);
        let bootstrap = state
            .worker_registry
            .provision(
                bro_core::TaskId::new("rpc-task"),
                bro_core::SessionId::new("rpc-session"),
                Some(bro_core::WorkerId::new("rpc-worker")),
            )
            .unwrap();
        let (worker_stream, fleet_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        worker_stream.set_nonblocking(true).unwrap();
        fleet_stream.set_nonblocking(true).unwrap();
        let worker_stream = UnixStream::from_std(worker_stream).unwrap();
        let fleet_stream = UnixStream::from_std(fleet_stream).unwrap();
        let server_state = state.clone();
        let server =
            tokio::spawn(async move { serve_connection(fleet_stream, server_state).await });

        let hello = worker_hello(&bootstrap, bootstrap.proof.clone());
        let (negotiated, _) =
            bro_rpc::connect_worker_with_options(worker_stream, hello, HandshakeOptions::default())
                .await
                .unwrap();
        let mut worker = RpcPeer::spawn(negotiated, PeerConfig::default()).unwrap();
        let capability = worker
            .handle()
            .request_with_id(
                "capability-1".to_string(),
                WorkerMessage::CapabilityRequest(bro_protocol::CapabilityRequest {
                    call_id: "capability-1".to_string(),
                    invocation_id: None,
                    capability: "corpus".to_string(),
                    operation: "search_corpus".to_string(),
                    bounded_payload: serde_json::json!({"query":"needle","limit":2}),
                    deadline_unix_ms: Some(now_ms().saturating_add(10_000)),
                }),
                MessagePriority::Normal,
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        let WorkerMessage::CapabilityResponse(capability) = capability.body else {
            panic!("expected capability response")
        };
        assert!(!capability.is_error);

        assert!(
            dispatch_task_command_if_owned(
                "rpc-task",
                WorkerCommandKind::UserTurn {
                    text: "hello".to_string(),
                },
            )
            .unwrap()
        );
        let envelope = worker.recv().await.unwrap();
        let WorkerMessage::Command(command) = envelope.body else {
            panic!("expected worker command")
        };
        worker
            .handle()
            .send(
                WorkerMessage::CommandOutcome(bro_protocol::CommandOutcome {
                    command_seq: command.command_seq,
                    command_id: command.command_id,
                    accepted: true,
                    terminal: true,
                    result_or_error: serde_json::json!({"queued": true}),
                }),
                MessagePriority::Control,
            )
            .unwrap();
        let ack = worker.recv().await.unwrap();
        assert!(matches!(
            ack.body,
            WorkerMessage::CommandOutcomeAck(CommandOutcomeAck {
                through_command_seq: 1
            })
        ));
        drop(worker);
        server.await.unwrap().unwrap();
        assert!(
            state
                .worker_registry
                .record(&bootstrap.worker_id)
                .unwrap()
                .pending_handshake_diagnostics
                .is_empty()
        );
    }

    #[tokio::test]
    async fn missing_task_projection_terminally_rejects_authenticated_worker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        let bootstrap = state
            .worker_registry
            .provision(
                bro_core::TaskId::new("missing-task"),
                bro_core::SessionId::new("missing-session"),
                Some(bro_core::WorkerId::new("missing-worker")),
            )
            .unwrap();
        let worker_id = bootstrap.worker_id.clone();
        let (worker_stream, fleet_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        worker_stream.set_nonblocking(true).unwrap();
        fleet_stream.set_nonblocking(true).unwrap();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            serve_connection(UnixStream::from_std(fleet_stream).unwrap(), server_state).await
        });

        let error = bro_rpc::connect_worker_with_options(
            UnixStream::from_std(worker_stream).unwrap(),
            worker_hello(&bootstrap, bootstrap.proof.clone()),
            HandshakeOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            RpcError::HandshakeRejected { ref code, .. }
                if code == "protocol.authentication_failed"
        ));
        assert!(server.await.unwrap().is_err());
        assert_eq!(
            state.worker_registry.record(&worker_id).unwrap().state,
            WorkerAuthorityState::Lost
        );
    }

    #[tokio::test]
    async fn vanished_persisted_project_scope_is_terminal_before_welcome() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project_root = root.join("project");
        std::fs::create_dir(&project_root).unwrap();
        let project_root = project_root.canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        insert_running_task(&state, "scope-task", "scope-session", Some(&project_root));
        let bootstrap = state
            .worker_registry
            .provision_scoped(
                bro_core::TaskId::new("scope-task"),
                bro_core::SessionId::new("scope-session"),
                Some(bro_core::WorkerId::new("scope-worker")),
                Some(project_root.clone()),
            )
            .unwrap();
        let initial_hello = worker_hello(&bootstrap, bootstrap.proof.clone());
        let scope = worker_capability_scope(&state, &initial_hello).unwrap();
        let policy = session_policy(&initial_hello, &scope).unwrap();
        let grant = state
            .worker_registry
            .accept_handshake_with_policy(
                &initial_hello,
                WORKER_PROTOCOL_V1,
                policy,
                fleet_build_identity(),
            )
            .unwrap();
        state
            .worker_registry
            .mark_disconnected(&bootstrap.worker_id, grant.connection_generation)
            .unwrap();
        std::fs::remove_dir(&project_root).unwrap();

        let reconnect_hello = worker_hello(&bootstrap, grant.reconnect_proof);
        let (worker_stream, fleet_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        worker_stream.set_nonblocking(true).unwrap();
        fleet_stream.set_nonblocking(true).unwrap();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            serve_connection(UnixStream::from_std(fleet_stream).unwrap(), server_state).await
        });
        let error = bro_rpc::connect_worker_with_options(
            UnixStream::from_std(worker_stream).unwrap(),
            reconnect_hello,
            HandshakeOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            RpcError::HandshakeRejected { ref code, .. }
                if code == "protocol.policy_mismatch"
        ));
        assert!(server.await.unwrap().is_err());
        assert_eq!(
            state
                .worker_registry
                .record(&bootstrap.worker_id)
                .unwrap()
                .state,
            WorkerAuthorityState::Lost
        );
        assert_eq!(
            state
                .task_store
                .read()
                .get("scope-task")
                .unwrap()
                .inner
                .lock()
                .status,
            crate::orchestration::TaskStatus::Failed
        );
    }

    #[tokio::test]
    async fn invalid_direction_message_terminalizes_instead_of_replaying() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        insert_running_task(&state, "poison-task", "poison-session", None);
        let bootstrap = state
            .worker_registry
            .provision(
                bro_core::TaskId::new("poison-task"),
                bro_core::SessionId::new("poison-session"),
                Some(bro_core::WorkerId::new("poison-worker")),
            )
            .unwrap();
        let worker_id = bootstrap.worker_id.clone();
        let (worker_stream, fleet_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        worker_stream.set_nonblocking(true).unwrap();
        fleet_stream.set_nonblocking(true).unwrap();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            serve_connection(UnixStream::from_std(fleet_stream).unwrap(), server_state).await
        });
        let (negotiated, _) = bro_rpc::connect_worker_with_options(
            UnixStream::from_std(worker_stream).unwrap(),
            worker_hello(&bootstrap, bootstrap.proof.clone()),
            HandshakeOptions::default(),
        )
        .await
        .unwrap();
        let worker = RpcPeer::spawn(negotiated, PeerConfig::default()).unwrap();
        worker
            .handle()
            .send(
                WorkerMessage::EventAck(EventAck {
                    through_event_seq: 0,
                }),
                MessagePriority::Control,
            )
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            state.worker_registry.record(&worker_id).unwrap().state,
            WorkerAuthorityState::Lost
        );
        assert_eq!(
            state
                .task_store
                .read()
                .get("poison-task")
                .unwrap()
                .inner
                .lock()
                .status,
            crate::orchestration::TaskStatus::Failed
        );
    }

    #[test]
    fn message_failure_classifier_separates_poison_from_transient_io() {
        let poison = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid worker event",
        ));
        assert_eq!(
            classify_message_apply_error(&poison),
            MessageApplyFailureClass::Deterministic
        );
        let transient = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            "temporary persistence failure",
        ));
        assert_eq!(
            classify_message_apply_error(&transient),
            MessageApplyFailureClass::Retryable
        );
        assert_eq!(
            classify_message_apply_error(&anyhow::anyhow!("unknown command outcome")),
            MessageApplyFailureClass::Deterministic
        );
    }

    #[test]
    fn persisted_lost_authority_is_reconciled_into_task_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = SharedState::for_test(&root);
        insert_running_task(&state, "lost-task", "lost-session", None);
        let bootstrap = state
            .worker_registry
            .provision(
                bro_core::TaskId::new("lost-task"),
                bro_core::SessionId::new("lost-session"),
                Some(bro_core::WorkerId::new("lost-worker")),
            )
            .unwrap();
        state.worker_registry.abandon(&bootstrap.worker_id).unwrap();

        reconcile_terminal_worker_records(&state);

        assert_eq!(
            state
                .task_store
                .read()
                .get("lost-task")
                .unwrap()
                .inner
                .lock()
                .status,
            crate::orchestration::TaskStatus::Failed
        );
    }

    #[test]
    fn worker_build_window_accepts_rolling_patches_and_rejects_other_minor_lines() {
        let fleet = semver::Version::parse("0.4.7").unwrap();
        validate_worker_build_against(
            &fleet,
            &BuildIdentity {
                version: "0.4.99".to_string(),
                build_id: "worker-current-line".to_string(),
            },
        )
        .unwrap();

        let rejection = validate_worker_build_against(
            &fleet,
            &BuildIdentity {
                version: "0.5.0".to_string(),
                build_id: "worker-next-line".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(rejection.code, "protocol.unsupported_build");
    }

    #[test]
    fn worker_build_window_accepts_one_minor_for_stable_major_only() {
        let fleet = semver::Version::parse("2.4.7").unwrap();
        for version in ["2.3.0", "2.4.99", "2.5.0"] {
            validate_worker_build_against(
                &fleet,
                &BuildIdentity {
                    version: version.to_string(),
                    build_id: format!("worker-{version}"),
                },
            )
            .unwrap();
        }
        for version in ["1.4.7", "2.2.9", "2.6.0", "3.4.0", "not-semver"] {
            let rejection = validate_worker_build_against(
                &fleet,
                &BuildIdentity {
                    version: version.to_string(),
                    build_id: "worker-outside-window".to_string(),
                },
            )
            .unwrap_err();
            assert_eq!(rejection.code, "protocol.unsupported_build");
        }
    }
}
