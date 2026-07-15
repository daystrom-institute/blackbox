use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

use bro_core::{SessionId, TaskId, WorkerId};
use bro_protocol::{
    BuildIdentity, CapabilityError, CapabilityErrorCode, CapabilityResponse,
    DOWNSTREAM_SERVICE_AVAILABILITY_ATTRIBUTE, DownstreamServiceAvailability, FeaturePolicy,
    PolicyIdentity, ProtocolError, ProtocolErrorCode, ReplayRequest, SessionCapabilityPolicy,
    SessionPolicy, WORKER_FEATURE_POLICY_ATTRIBUTE, WORKER_PROTOCOL_V1, WorkerCommand,
    WorkerCommandKind, WorkerFeature, WorkerHello, WorkerMessage,
};
use bro_rpc::{
    FleetHandshakeGrant, HandshakeAuthorityReject, HandshakeOptions, MessagePriority, PeerConfig,
    PeerHandle, RpcPeer, accept_worker_with_authority,
};
use fleet_core::{
    FleetError, HandshakeAccepted, LeasePolicy, ProviderCredentialStatus, ProviderQuotaStatus,
};
use semver::Version;
use sha2::{Digest, Sha256};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::authority_actor::AuthorityActor;
use crate::projection::observation_for_event;
use crate::roster::{RosterHub, fleet_build_id};
use crate::{CapabilityBroker, FleetdError, FleetdResult};

const MAX_IN_FLIGHT_CAPABILITY_CALLS_PER_WORKER: usize = 8;
const MAX_PENDING_WORKER_HANDSHAKES: usize = 32;
const MAX_WORKER_HANDSHAKE_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
struct LiveWorker {
    generation: u64,
    handle: PeerHandle,
}

#[derive(Clone, Default)]
pub(crate) struct LiveWorkers {
    workers: Arc<RwLock<HashMap<WorkerId, LiveWorker>>>,
}

impl LiveWorkers {
    pub(crate) fn send(&self, worker_id: &WorkerId, command: WorkerCommand) -> FleetdResult<bool> {
        let worker = self
            .workers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(worker_id)
            .cloned();
        let Some(worker) = worker else {
            return Ok(false);
        };
        let priority = command_priority(&command.command);
        match worker
            .handle
            .send(WorkerMessage::Command(command), priority)
        {
            Ok(_) => Ok(true),
            Err(error) => {
                worker.handle.shutdown();
                Err(error.into())
            }
        }
    }

    fn install(&self, worker_id: WorkerId, generation: u64, handle: PeerHandle) {
        self.workers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(worker_id, LiveWorker { generation, handle });
    }

    fn remove_generation(&self, worker_id: &WorkerId, generation: u64) {
        let mut workers = self
            .workers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if workers
            .get(worker_id)
            .is_some_and(|worker| worker.generation == generation)
        {
            workers.remove(worker_id);
        }
    }

    fn entries(&self) -> Vec<(WorkerId, u64)> {
        self.workers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(worker_id, worker)| (worker_id.clone(), worker.generation))
            .collect()
    }

    fn send_service_policy(
        &self,
        worker_id: &WorkerId,
        generation: u64,
        policy: SessionPolicy,
    ) -> FleetdResult<bool> {
        let worker = self
            .workers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(worker_id)
            .filter(|worker| worker.generation == generation)
            .cloned();
        let Some(worker) = worker else {
            return Ok(false);
        };
        match worker.handle.send(
            WorkerMessage::ServicePolicy(policy),
            MessagePriority::Control,
        ) {
            Ok(_) => Ok(true),
            Err(error) => {
                worker.handle.shutdown();
                Err(error.into())
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct WorkerServer {
    socket_path: PathBuf,
    worker_root: PathBuf,
    authority: AuthorityActor,
    roster: RosterHub,
    broker: Arc<dyn CapabilityBroker>,
    live: LiveWorkers,
    allowed_capabilities: Vec<String>,
    downstream_availability: Arc<RwLock<DownstreamServiceAvailability>>,
    pending_handshakes: Arc<Semaphore>,
    lease_policy: LeasePolicy,
    ready: Arc<AtomicBool>,
}

impl WorkerServer {
    pub(crate) fn new(
        socket_path: PathBuf,
        worker_root: PathBuf,
        authority: AuthorityActor,
        roster: RosterHub,
        broker: Arc<dyn CapabilityBroker>,
        allowed_capabilities: Vec<String>,
        lease_policy: LeasePolicy,
    ) -> Self {
        Self {
            socket_path,
            worker_root,
            authority,
            roster,
            broker,
            live: LiveWorkers::default(),
            allowed_capabilities,
            downstream_availability: Arc::new(
                RwLock::new(DownstreamServiceAvailability::default()),
            ),
            pending_handshakes: Arc::new(Semaphore::new(MAX_PENDING_WORKER_HANDSHAKES)),
            lease_policy,
            ready: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn with_ready_flag(mut self, ready: Arc<AtomicBool>) -> Self {
        self.ready = ready;
        self
    }

    pub(crate) fn live_workers(&self) -> LiveWorkers {
        self.live.clone()
    }

    pub(crate) async fn run(self, shutdown: CancellationToken) -> FleetdResult<()> {
        let initial_availability = self.broker.downstream_availability().await;
        *self
            .downstream_availability
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = initial_availability;
        let listener = bind_private_socket(&self.socket_path).await?;
        self.ready.store(true, Ordering::Release);
        tracing::info!(socket = %self.socket_path.display(), "fleet worker socket ready");
        let mut connections = tokio::task::JoinSet::new();
        let monitor_server = self.clone();
        let monitor_shutdown = shutdown.clone();
        let mut availability_monitor = tokio::spawn(async move {
            monitor_server
                .monitor_downstream_availability(monitor_shutdown)
                .await
        });
        let mut terminal_error = None;
        let mut accept_error_backoff = Duration::from_millis(10);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                joined = &mut availability_monitor => {
                    match joined {
                        Ok(Ok(())) if shutdown.is_cancelled() => break,
                        Ok(Ok(())) => {
                            terminal_error = Some(FleetdError::AuthorityUnavailable);
                        }
                        Ok(Err(error)) => terminal_error = Some(error),
                        Err(error) => {
                            terminal_error = Some(FleetdError::Conflict(format!(
                                "downstream availability monitor failed: {error}"
                            )));
                        }
                    }
                    shutdown.cancel();
                    break;
                }
                accepted = listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(accepted) => {
                            accept_error_backoff = Duration::from_millis(10);
                            accepted
                        }
                        Err(error) => {
                            tracing::warn!(%error, "fleet worker socket accept failed; retrying");
                            tokio::time::sleep(accept_error_backoff).await;
                            accept_error_backoff = accept_error_backoff
                                .saturating_mul(2)
                                .min(Duration::from_secs(1));
                            continue;
                        }
                    };
                    let handshake_permit = match self.pending_handshakes.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            tracing::warn!(
                                limit = MAX_PENDING_WORKER_HANDSHAKES,
                                "dropping worker connection because handshake capacity is full"
                            );
                            continue;
                        }
                    };
                    let server = self.clone();
                    let connection_shutdown = shutdown.clone();
                    connections.spawn(async move {
                        if let Err(error) = server
                            .handle_connection(
                                stream,
                                connection_shutdown.clone(),
                                handshake_permit,
                            )
                            .await
                            && !connection_shutdown.is_cancelled()
                        {
                            tracing::warn!(%error, "fleet worker connection closed");
                        }
                    });
                }
                Some(joined) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = joined {
                        tracing::warn!(%error, "fleet worker connection task failed");
                    }
                }
            }
        }
        let drained = tokio::time::timeout(Duration::from_secs(5), async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            // Dropping a connection peer closes its socket and makes the
            // worker enter reconnect. Abort only connection tasks, never
            // worker processes.
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        }
        availability_monitor.abort();
        let _ = tokio::fs::remove_file(&self.socket_path).await;
        self.ready.store(false, Ordering::Release);
        if let Some(error) = terminal_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    async fn monitor_downstream_availability(
        &self,
        shutdown: CancellationToken,
    ) -> FleetdResult<()> {
        let interval = self
            .broker
            .availability_poll_interval()
            .max(Duration::from_millis(10));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = ticker.tick() => {
                    let availability = self.broker.downstream_availability().await;
                    let prior = {
                        let mut current = self
                            .downstream_availability
                            .write()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let prior = *current;
                        *current = availability;
                        prior
                    };
                    if availability != prior {
                        tracing::info!(
                            blackops = ?availability.blackops,
                            corpus = ?availability.corpus,
                            "downstream service availability changed"
                        );
                    }
                    self.reconcile_live_service_policies(availability).await?;
                }
            }
        }
    }

    async fn reconcile_live_service_policies(
        &self,
        availability: DownstreamServiceAvailability,
    ) -> FleetdResult<()> {
        let live = self.live.entries();
        if live.is_empty() {
            return Ok(());
        }
        let snapshot = self
            .authority
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await?;
        for (worker_id, generation) in live {
            let Some((task_id, session_id, previous)) =
                snapshot.workers.get(&worker_id).and_then(|worker| {
                    worker
                        .session_policy
                        .clone()
                        .map(|policy| (worker.task_id.clone(), worker.session_id.clone(), policy))
                })
            else {
                continue;
            };
            let Some(policy) =
                revise_session_policy(&task_id, &session_id, &previous, availability)?
            else {
                continue;
            };
            let policy_for_authority = policy.clone();
            let authority_worker_id = worker_id.clone();
            let updated = self
                .authority
                .call(move |authority| {
                    authority
                        .update_session_policy(
                            &authority_worker_id,
                            generation,
                            policy_for_authority,
                            now_ms(),
                        )
                        .map_err(Into::into)
                })
                .await;
            match updated {
                Ok(()) => {}
                Err(FleetdError::Authority(FleetError::StaleConnectionGeneration { .. }))
                | Err(FleetdError::Authority(FleetError::NotFound { .. })) => continue,
                Err(error) => return Err(error),
            }
            if let Err(error) = self
                .live
                .send_service_policy(&worker_id, generation, policy)
            {
                tracing::debug!(%worker_id, %error, "live service policy delivery raced disconnect");
            }
        }
        Ok(())
    }

    async fn handle_connection(
        &self,
        stream: UnixStream,
        shutdown: CancellationToken,
        handshake_permit: OwnedSemaphorePermit,
    ) -> FleetdResult<()> {
        #[cfg(unix)]
        {
            let peer_uid = stream.peer_cred()?.uid();
            // SAFETY: geteuid has no preconditions and does not mutate memory.
            let service_uid = unsafe { libc::geteuid() };
            if peer_uid != service_uid {
                return Err(FleetdError::Conflict(
                    "worker socket peer belongs to another user".into(),
                ));
            }
        }
        let accepted_slot: Arc<Mutex<Option<(WorkerId, HandshakeAccepted)>>> =
            Arc::new(Mutex::new(None));
        let authority = self.authority.clone();
        let allowed_capabilities = self.allowed_capabilities.clone();
        let downstream_availability = self.downstream_availability.clone();
        let lease_policy = self.lease_policy;
        let accepted_for_handshake = accepted_slot.clone();
        let handshake = accept_worker_with_authority(
            stream,
            vec![WORKER_PROTOCOL_V1],
            worker_handshake_options(),
            move |hello, selected_protocol| async move {
                validate_worker_offer(&hello, selected_protocol)?;
                let hello_for_authority = hello.clone();
                let worker_id = hello.worker_id.clone();
                let availability = *downstream_availability
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let accepted = authority
                    .call(move |authority| {
                        let snapshot = authority.snapshot()?;
                        let existing_policy = snapshot
                            .workers
                            .get(&hello_for_authority.worker_id)
                            .and_then(|worker| worker.session_policy.clone());
                        let requested_policy = snapshot
                            .workers
                            .get(&hello_for_authority.worker_id)
                            .map(|worker| worker.requested_capability_policy.clone())
                            .ok_or_else(|| {
                                FleetdError::Conflict(format!(
                                    "worker {} has no durable capability request",
                                    hello_for_authority.worker_id
                                ))
                            })?;
                        let session_policy = build_session_policy(
                            &hello_for_authority,
                            allowed_capabilities,
                            &requested_policy,
                            existing_policy.as_ref(),
                            availability,
                        )?;
                        authority
                            .accept_handshake(
                                hello_for_authority,
                                session_policy,
                                fleet_build(),
                                lease_policy,
                                now_ms(),
                            )
                            .map_err(FleetdError::from)
                    })
                    .await
                    .map_err(handshake_rejection)?;
                let grant = FleetHandshakeGrant {
                    connection_generation: accepted.welcome.connection_generation,
                    event_ack: accepted.welcome.event_ack,
                    next_command_seq: accepted.welcome.next_command_seq,
                    lease: accepted.welcome.lease.clone(),
                    reconnect_proof: accepted.welcome.reconnect_proof.clone(),
                    session_policy: accepted.welcome.session_policy.clone(),
                    fleet_build: accepted.welcome.fleet_build.clone(),
                };
                *accepted_for_handshake
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((worker_id, accepted));
                Ok(grant)
            },
        )
        .await;
        let (negotiated, hello, welcome) = match handshake {
            Ok(value) => value,
            Err(error) => {
                let accepted = accepted_slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                if let Some((worker_id, accepted)) = accepted {
                    let generation = accepted.welcome.connection_generation;
                    let _ = self
                        .authority
                        .call(move |authority| {
                            authority
                                .mark_disconnected(&worker_id, generation, now_ms())
                                .map(|_| ())
                                .map_err(Into::into)
                        })
                        .await;
                }
                return Err(error.into());
            }
        };
        let (_, accepted) = accepted_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .ok_or_else(|| FleetdError::Conflict("handshake grant was not retained".into()))?;
        // Authenticated peers are bounded separately by the durable worker
        // roster and per-worker capability semaphore. Release the scarce
        // pre-auth slot before starting the long-lived peer loop.
        drop(handshake_permit);
        let mut peer = RpcPeer::spawn(negotiated, PeerConfig::default())?;
        let handle = peer.handle();
        let generation = welcome.connection_generation;
        self.live
            .install(hello.worker_id.clone(), generation, handle.clone());

        if accepted.replay_from_event_seq <= hello.last_local_event_seq {
            handle.send(
                WorkerMessage::ReplayRequest(ReplayRequest {
                    from_event_seq: accepted.replay_from_event_seq,
                }),
                MessagePriority::Control,
            )?;
        }
        for command in accepted.replay_commands {
            handle.send(
                WorkerMessage::Command(command.clone()),
                command_priority(&command.command),
            )?;
        }

        let capability_slots = Arc::new(Semaphore::new(MAX_IN_FLIGHT_CAPABILITY_CALLS_PER_WORKER));
        let mut capability_calls = JoinSet::new();
        let mut confirmed = false;
        loop {
            let received = loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        capability_calls.abort_all();
                        handle.shutdown();
                        self.live.remove_generation(&hello.worker_id, generation);
                        let worker_id = hello.worker_id.clone();
                        let _ = self.authority.call(move |authority| {
                            authority
                                .mark_disconnected(&worker_id, generation, now_ms())
                                .map(|_| ())
                                .map_err(Into::into)
                        }).await;
                        return Ok(());
                    }
                    joined = capability_calls.join_next(), if !capability_calls.is_empty() => {
                        match joined {
                            Some(Ok(Ok(()))) | None => {}
                            Some(Ok(Err(error))) => {
                                tracing::warn!(%error, "capability response delivery failed");
                            }
                            Some(Err(error)) => {
                                tracing::warn!(%error, "capability broker task failed");
                            }
                        }
                    }
                    received = peer.recv() => break received,
                }
            };
            let envelope = match received {
                Ok(envelope) => envelope,
                Err(error) => {
                    capability_calls.abort_all();
                    self.live.remove_generation(&hello.worker_id, generation);
                    let worker_id = hello.worker_id.clone();
                    let _ = self
                        .authority
                        .call(move |authority| {
                            authority
                                .mark_disconnected(&worker_id, generation, now_ms())
                                .map_err(Into::into)
                        })
                        .await;
                    return Err(error.into());
                }
            };
            if !confirmed {
                let worker_id = hello.worker_id.clone();
                self.authority
                    .call(move |authority| {
                        authority
                            .confirm_connection(&worker_id, generation, now_ms())
                            .map(|_| ())
                            .map_err(Into::into)
                    })
                    .await?;
                let _ = tokio::fs::remove_file(
                    self.worker_root
                        .join(hello.worker_id.as_str())
                        .join("bootstrap.secret"),
                )
                .await;
                confirmed = true;
            }
            if !self
                .handle_message(
                    &hello,
                    generation,
                    &handle,
                    envelope,
                    &capability_slots,
                    &mut capability_calls,
                )
                .await?
            {
                break;
            }
        }
        capability_calls.abort_all();
        self.live.remove_generation(&hello.worker_id, generation);
        Ok(())
    }

    async fn handle_message(
        &self,
        hello: &WorkerHello,
        generation: u64,
        handle: &PeerHandle,
        envelope: bro_protocol::Envelope,
        capability_slots: &Arc<Semaphore>,
        capability_calls: &mut JoinSet<FleetdResult<()>>,
    ) -> FleetdResult<bool> {
        match envelope.body.clone() {
            WorkerMessage::Event(event) => {
                let provider_signal =
                    provider_runtime_signal(&event.event, event.occurred_at_unix_ms);
                let worker_id = hello.worker_id.clone();
                let observation = observation_for_event(&event.event, event.occurred_at_unix_ms);
                let ack = self
                    .authority
                    .call(move |authority| {
                        authority
                            .observe_event_observation(
                                &worker_id,
                                generation,
                                event.event_seq,
                                event.occurred_at_unix_ms,
                                observation,
                            )
                            .map_err(Into::into)
                    })
                    .await?;
                if let Some((credential, quota, cooldown)) = provider_signal {
                    let task_id = hello.task_id.clone();
                    self.authority
                        .call(move |authority| {
                            authority
                                .observe_provider_runtime(
                                    &task_id,
                                    credential,
                                    quota,
                                    cooldown,
                                    now_ms(),
                                )
                                .map(|_| ())
                                .map_err(Into::into)
                        })
                        .await?;
                }
                self.refresh_roster().await?;
                handle.send(WorkerMessage::EventAck(ack), MessagePriority::Control)?;
            }
            WorkerMessage::CommandOutcome(outcome) => {
                let worker_id = hello.worker_id.clone();
                let ack = self
                    .authority
                    .call(move |authority| {
                        authority
                            .observe_command_outcome(&worker_id, generation, outcome, now_ms())
                            .map_err(Into::into)
                    })
                    .await?;
                handle.send(
                    WorkerMessage::CommandOutcomeAck(ack),
                    MessagePriority::Control,
                )?;
            }
            WorkerMessage::Heartbeat(heartbeat) => {
                let worker_id = hello.worker_id.clone();
                let renewal = self
                    .authority
                    .call(move |authority| {
                        authority
                            .renew_lease(&worker_id, generation, &heartbeat.lease_id, now_ms())
                            .map_err(Into::into)
                    })
                    .await?;
                handle.send(
                    WorkerMessage::LeaseRenewal(renewal),
                    MessagePriority::Control,
                )?;
            }
            WorkerMessage::CapabilityRequest(request) => {
                let worker_id = hello.worker_id.clone();
                let request_for_auth = request.clone();
                let (route, session_id, authorization) = self
                    .authority
                    .call(move |authority| {
                        authority
                            .authorize_capability(&worker_id, generation, &request_for_auth)
                            .map_err(Into::into)
                    })
                    .await?;
                let permit = match capability_slots.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        let response = CapabilityResponse::error(
                            request.call_id,
                            CapabilityError {
                                code: CapabilityErrorCode::Unavailable,
                                message: format!(
                                    "worker capability concurrency limit of {} is exhausted",
                                    MAX_IN_FLIGHT_CAPABILITY_CALLS_PER_WORKER
                                ),
                                retryable: true,
                                details: None,
                            },
                        )?;
                        handle.respond(
                            &envelope,
                            WorkerMessage::CapabilityResponse(response),
                            MessagePriority::Normal,
                        )?;
                        return Ok(true);
                    }
                };
                let broker = self.broker.clone();
                let worker_id = hello.worker_id.clone();
                let response_handle = handle.clone();
                capability_calls.spawn(async move {
                    let _permit = permit;
                    let call_id = request.call_id.clone();
                    let response = match broker
                        .call(route, &worker_id, &session_id, authorization, request)
                        .await
                    {
                        Ok(response) => response,
                        Err(error) => CapabilityResponse::error(
                            call_id,
                            CapabilityError {
                                code: CapabilityErrorCode::Unavailable,
                                message: error.to_string(),
                                retryable: true,
                                details: None,
                            },
                        )?,
                    };
                    response_handle.respond(
                        &envelope,
                        WorkerMessage::CapabilityResponse(response),
                        MessagePriority::Normal,
                    )?;
                    Ok(())
                });
            }
            WorkerMessage::Status(status) => {
                let terminal = status.state == bro_protocol::WorkerLifecycleState::Terminal;
                let worker_id = hello.worker_id.clone();
                self.authority
                    .call(move |authority| {
                        authority
                            .observe_status(&worker_id, generation, status, now_ms())
                            .map_err(Into::into)
                    })
                    .await?;
                self.refresh_roster().await?;
                if terminal {
                    return Ok(false);
                }
            }
            WorkerMessage::ReplayUnavailable(unavailable) => {
                let worker_id = hello.worker_id.clone();
                self.authority
                    .call(move |authority| {
                        authority
                            .mark_worker_lost(
                                &worker_id,
                                generation,
                                format!(
                                    "worker replay unavailable from event {} (available {} through {})",
                                    unavailable.requested_from_event_seq,
                                    unavailable.earliest_available_event_seq,
                                    unavailable.latest_available_event_seq
                                ),
                                now_ms(),
                            )
                            .map_err(Into::into)
                    })
                    .await?;
                self.refresh_roster().await?;
                return Ok(false);
            }
            WorkerMessage::ProtocolError(error) if error.fatal => {
                let worker_id = hello.worker_id.clone();
                let reason = error.message;
                self.authority
                    .call(move |authority| {
                        authority
                            .mark_worker_lost(&worker_id, generation, reason, now_ms())
                            .map_err(Into::into)
                    })
                    .await?;
                self.refresh_roster().await?;
                return Ok(false);
            }
            WorkerMessage::ProtocolError(_) => {}
            _ => {
                handle.send_protocol_error(ProtocolError {
                    code: ProtocolErrorCode::InvalidPayload,
                    message: "message direction is invalid for a worker peer".into(),
                    fatal: true,
                    related_message_id: Some(envelope.message_id),
                    expected_protocol_version: Some(WORKER_PROTOCOL_V1),
                    expected_connection_generation: Some(generation),
                })?;
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn refresh_roster(&self) -> FleetdResult<()> {
        let snapshot = self
            .authority
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await?;
        self.roster.publish_authority(&snapshot);
        Ok(())
    }
}

fn worker_handshake_options() -> HandshakeOptions {
    HandshakeOptions {
        max_frame_bytes: MAX_WORKER_HANDSHAKE_FRAME_BYTES,
        timeout: Duration::from_secs(10),
    }
}

type ProviderRuntimeSignal = (
    Option<ProviderCredentialStatus>,
    Option<ProviderQuotaStatus>,
    Option<u64>,
);

fn provider_runtime_signal(
    event: &serde_json::Value,
    occurred_at: u64,
) -> Option<ProviderRuntimeSignal> {
    if event.get("type").and_then(serde_json::Value::as_str) != Some("result") {
        return None;
    }
    let rendered = event.to_string().to_ascii_lowercase();
    if rendered.contains("rate_limit")
        || rendered.contains("rate limit")
        || rendered.contains("status 429")
    {
        return Some((
            None,
            Some(ProviderQuotaStatus::Exhausted),
            Some(occurred_at.saturating_add(60_000)),
        ));
    }
    if rendered.contains("unauthorized")
        || rendered.contains("invalid api key")
        || rendered.contains("authentication failed")
    {
        return Some((Some(ProviderCredentialStatus::Missing), None, None));
    }
    if !event
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Some((
            Some(ProviderCredentialStatus::Available),
            Some(ProviderQuotaStatus::Available),
            None,
        ));
    }
    Some((None, Some(ProviderQuotaStatus::ProbeFailed), None))
}

fn build_session_policy(
    hello: &WorkerHello,
    allowed_capabilities: Vec<String>,
    requested: &SessionCapabilityPolicy,
    previous: Option<&SessionPolicy>,
    availability: DownstreamServiceAvailability,
) -> FleetdResult<SessionPolicy> {
    let offered = hello.offered_protocol_features();
    let required = required_features();
    if !required.is_subset(&offered) {
        return Err(FleetdError::Conflict(format!(
            "worker omitted required protocol features: {:?}",
            required.difference(&offered).collect::<Vec<_>>()
        )));
    }
    requested
        .validate()
        .map_err(FleetdError::InvalidConfiguration)?;
    let host_capabilities = allowed_capabilities.into_iter().collect::<BTreeSet<_>>();
    let mut capability_policy = SessionCapabilityPolicy {
        allowed_operations: requested
            .allowed_operations
            .iter()
            .filter(|(capability, _)| host_capabilities.contains(*capability))
            .map(|(capability, operations)| (capability.clone(), operations.clone()))
            .collect(),
        allowed_atom_refs: BTreeSet::new(),
    };
    if capability_policy.allows_operation("atom", "invoke_atom") {
        capability_policy.allowed_atom_refs = requested.allowed_atom_refs.clone();
    }
    capability_policy
        .validate()
        .map_err(FleetdError::InvalidConfiguration)?;
    let allowed_capabilities = capability_policy
        .allowed_operations
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut attributes = BTreeMap::new();
    attributes.insert(
        DOWNSTREAM_SERVICE_AVAILABILITY_ATTRIBUTE.to_string(),
        serde_json::to_value(availability)?,
    );
    attributes.insert(
        bro_protocol::SESSION_CAPABILITY_POLICY_ATTRIBUTE.to_string(),
        serde_json::to_value(&capability_policy)?,
    );
    let digest = session_policy_digest(
        &hello.task_id,
        &hello.session_id,
        &allowed_capabilities,
        &required,
        &attributes,
    )?;
    let version = match previous {
        Some(previous) => match previous.feature_policy()? {
            Some(previous_feature) => {
                if previous_feature.enabled_features != required {
                    return Err(FleetdError::Conflict(
                        "required protocol features changed across reconnect".into(),
                    ));
                }
                let mut previous_capabilities = previous.allowed_capabilities.clone();
                previous_capabilities.sort();
                previous_capabilities.dedup();
                let mut previous_attributes = previous.attributes.clone();
                previous_attributes.remove(WORKER_FEATURE_POLICY_ATTRIBUTE);
                let expected_previous_digest = session_policy_digest(
                    &hello.task_id,
                    &hello.session_id,
                    &previous_capabilities,
                    &previous_feature.enabled_features,
                    &previous_attributes,
                )?;
                if expected_previous_digest != previous_feature.policy.digest {
                    return Err(FleetdError::Conflict(
                        "persisted session policy digest does not match its admitted policy".into(),
                    ));
                }
                if digest == previous_feature.policy.digest {
                    previous_feature.policy.version
                } else {
                    previous_feature
                        .policy
                        .version
                        .checked_add(1)
                        .ok_or_else(|| {
                            FleetdError::Conflict("session policy revision exhausted".into())
                        })?
                }
            }
            None => 1,
        },
        None => 1,
    };
    let mut policy = SessionPolicy {
        allowed_capabilities,
        attributes,
    };
    policy.set_feature_policy(FeaturePolicy {
        enabled_features: required,
        policy: PolicyIdentity { version, digest },
    })?;
    Ok(policy)
}

fn revise_session_policy(
    task_id: &TaskId,
    session_id: &SessionId,
    previous: &SessionPolicy,
    availability: DownstreamServiceAvailability,
) -> FleetdResult<Option<SessionPolicy>> {
    if previous.downstream_service_availability()? == Some(availability) {
        return Ok(None);
    }
    let feature = previous.feature_policy()?.ok_or_else(|| {
        FleetdError::Conflict("live service policy omitted its revision identity".into())
    })?;
    let mut allowed_capabilities = previous.allowed_capabilities.clone();
    allowed_capabilities.sort();
    allowed_capabilities.dedup();
    let mut attributes = previous.attributes.clone();
    attributes.remove(WORKER_FEATURE_POLICY_ATTRIBUTE);
    let expected_previous_digest = session_policy_digest(
        task_id,
        session_id,
        &allowed_capabilities,
        &feature.enabled_features,
        &attributes,
    )?;
    if expected_previous_digest != feature.policy.digest {
        return Err(FleetdError::Conflict(
            "persisted session policy digest does not match its admitted policy".into(),
        ));
    }
    attributes.insert(
        DOWNSTREAM_SERVICE_AVAILABILITY_ATTRIBUTE.to_string(),
        serde_json::to_value(availability)?,
    );
    let digest = session_policy_digest(
        task_id,
        session_id,
        &allowed_capabilities,
        &feature.enabled_features,
        &attributes,
    )?;
    let version = feature
        .policy
        .version
        .checked_add(1)
        .ok_or_else(|| FleetdError::Conflict("session policy revision exhausted".into()))?;
    let mut policy = SessionPolicy {
        allowed_capabilities,
        attributes,
    };
    policy.set_feature_policy(FeaturePolicy {
        enabled_features: feature.enabled_features,
        policy: PolicyIdentity { version, digest },
    })?;
    Ok(Some(policy))
}

fn session_policy_digest(
    task_id: &TaskId,
    session_id: &SessionId,
    allowed_capabilities: &[String],
    enabled_features: &BTreeSet<WorkerFeature>,
    attributes: &BTreeMap<String, serde_json::Value>,
) -> FleetdResult<String> {
    let digest_value = serde_json::json!({
        "task_id": task_id,
        "session_id": session_id,
        "allowed_capabilities": allowed_capabilities,
        "features": enabled_features,
        "attributes": attributes,
    });
    Ok(format!(
        "sha256:{}",
        hex::encode(Sha256::digest(serde_json::to_vec(&digest_value)?))
    ))
}

fn validate_worker_offer(
    hello: &WorkerHello,
    selected_protocol: u16,
) -> Result<(), HandshakeAuthorityReject> {
    if selected_protocol != WORKER_PROTOCOL_V1 {
        return Err(HandshakeAuthorityReject::new(
            "protocol.unsupported_protocol",
            "worker protocol is not supported",
            format!("selected unsupported protocol {selected_protocol}"),
        ));
    }
    validate_worker_build(&hello.worker_build).map_err(|message| {
        HandshakeAuthorityReject::new(
            "protocol.unsupported_build",
            "worker build is outside the supported rolling window",
            message,
        )
    })?;
    let required = required_features();
    let offered = hello.offered_protocol_features();
    if !required.is_subset(&offered) {
        return Err(HandshakeAuthorityReject::new(
            "protocol.unsupported_protocol",
            "worker is missing required protocol features",
            format!(
                "missing protocol features: {:?}",
                required.difference(&offered).collect::<Vec<_>>()
            ),
        ));
    }
    Ok(())
}

fn validate_worker_build(build: &BuildIdentity) -> Result<(), String> {
    if build.build_id.is_empty() || build.build_id.len() > 128 {
        return Err("worker build identity is empty or oversized".into());
    }
    let fleet = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|error| format!("fleet version is invalid: {error}"))?;
    let worker = Version::parse(&build.version)
        .map_err(|error| format!("worker version is invalid: {error}"))?;
    let supported = if fleet.major == 0 {
        worker.major == 0 && worker.minor == fleet.minor
    } else {
        worker.major == fleet.major && worker.minor.abs_diff(fleet.minor) <= 1
    };
    supported
        .then_some(())
        .ok_or_else(|| format!("worker version {worker} is incompatible with fleet {fleet}"))
}

fn required_features() -> BTreeSet<WorkerFeature> {
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

fn handshake_rejection(error: FleetdError) -> HandshakeAuthorityReject {
    let message = error.to_string();
    let code = match &error {
        FleetdError::Authority(FleetError::HandshakeRejected(reason))
            if reason.contains("policy") =>
        {
            "protocol.policy_mismatch"
        }
        FleetdError::Authority(FleetError::HandshakeRejected(reason))
            if reason.contains("lease") =>
        {
            "protocol.duplicate_live_owner"
        }
        FleetdError::Authority(FleetError::HandshakeRejected(_)) => {
            "protocol.authentication_failed"
        }
        _ => "protocol.internal",
    };
    HandshakeAuthorityReject::new(code, "worker handshake was rejected", message)
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

fn fleet_build() -> BuildIdentity {
    BuildIdentity {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_id: fleet_build_id(),
    }
}

async fn bind_private_socket(path: &Path) -> FleetdResult<UnixListener> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(parent).await?;
    let parent_metadata = tokio::fs::symlink_metadata(parent).await?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "worker socket parent {} is not a real directory",
            parent.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
    }
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_socket() => match UnixStream::connect(path).await {
            Ok(_) => {
                return Err(FleetdError::Conflict(format!(
                    "worker socket {} already has a live listener",
                    path.display()
                )));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                tokio::fs::remove_file(path).await?
            }
            Err(error) => {
                return Err(FleetdError::Conflict(format!(
                    "worker socket {} could not be safely probed: {error}",
                    path.display()
                )));
            }
        },
        Ok(_) => {
            return Err(FleetdError::InvalidConfiguration(format!(
                "worker socket path {} exists and is not a socket",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(listener)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) async fn deliver_task_command(
    authority: &AuthorityActor,
    live: &LiveWorkers,
    task_id: &TaskId,
    command: WorkerCommandKind,
) -> FleetdResult<WorkerCommand> {
    let task_id = task_id.clone();
    let (worker_id, command) = authority
        .call(move |authority| {
            let command = authority.enqueue_command(&task_id, command, now_ms())?;
            let snapshot = authority.snapshot()?;
            let worker_id = snapshot
                .tasks
                .get(&task_id)
                .and_then(|task| task.worker_id.clone())
                .ok_or_else(|| FleetdError::NotFound(format!("worker for task {task_id}")))?;
            Ok((worker_id, command))
        })
        .await?;
    let _ = live.send(&worker_id, command.clone());
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unauthenticated_worker_handshakes_are_tightly_bounded() {
        let options = worker_handshake_options();
        assert_eq!(options.max_frame_bytes, 64 * 1024);
        assert_eq!(options.timeout, Duration::from_secs(10));

        let limiter = Arc::new(Semaphore::new(MAX_PENDING_WORKER_HANDSHAKES));
        let permits = (0..MAX_PENDING_WORKER_HANDSHAKES)
            .map(|_| limiter.clone().try_acquire_owned().unwrap())
            .collect::<Vec<_>>();
        assert!(limiter.clone().try_acquire_owned().is_err());
        drop(permits);
        assert!(limiter.try_acquire_owned().is_ok());
    }

    #[test]
    fn session_policy_intersects_host_routes_and_digest_covers_exact_authority() {
        let hello = WorkerHello {
            protocol_versions: vec![WORKER_PROTOCOL_V1],
            worker_build: BuildIdentity {
                version: env!("CARGO_PKG_VERSION").into(),
                build_id: "worker-build".into(),
            },
            worker_id: WorkerId::new("worker-1"),
            task_id: TaskId::new("task-1"),
            session_id: SessionId::new("session-1"),
            bootstrap_or_resume_proof: bro_protocol::AuthenticationProof::new("proof"),
            last_local_event_seq: 0,
            last_fleet_command_seq: 0,
            worker_capabilities: required_features()
                .into_iter()
                .map(|feature| feature.as_str().to_string())
                .collect(),
        };
        let atom_ref = bro_core::AtomRef::new("atom:review@v1");
        let requested = SessionCapabilityPolicy {
            allowed_operations: BTreeMap::from([
                ("atom".into(), BTreeSet::from(["invoke_atom".into()])),
                ("blackops.agent".into(), BTreeSet::from(["spawn".into()])),
                ("corpus.search".into(), BTreeSet::from(["search".into()])),
            ]),
            allowed_atom_refs: BTreeSet::from([atom_ref.clone()]),
        };
        let policy = build_session_policy(
            &hello,
            vec!["blackops.agent".into(), "atom".into()],
            &requested,
            None,
            DownstreamServiceAvailability::default(),
        )
        .unwrap();

        assert_eq!(
            policy.allowed_capabilities,
            vec!["atom".to_string(), "blackops.agent".to_string()]
        );
        assert_eq!(
            policy.capability_policy().unwrap().unwrap(),
            SessionCapabilityPolicy {
                allowed_operations: BTreeMap::from([
                    ("atom".into(), BTreeSet::from(["invoke_atom".into()]),),
                    ("blackops.agent".into(), BTreeSet::from(["spawn".into()]),),
                ]),
                allowed_atom_refs: BTreeSet::from([atom_ref]),
            }
        );

        let mut tampered = policy;
        tampered
            .set_capability_policy(SessionCapabilityPolicy::default())
            .unwrap();
        assert!(matches!(
            build_session_policy(
                &hello,
                vec!["blackops.agent".into(), "atom".into()],
                &requested,
                Some(&tampered),
                DownstreamServiceAvailability::default(),
            ),
            Err(FleetdError::Conflict(_))
        ));
    }
}
