use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use bro_capabilities::{
    AttemptOutcome, AttemptState, ExecutionAccepted, ExecutionKind, ExecutionRequest,
    ExecutionToolPolicy, RecordEnvelope, WorkingSetIntent,
};
use bro_core::{AgentId, AttemptId, CommandId, Origin, SessionId, TaskId, WorkerId};
use bro_protocol::{
    AgentMailboxDelivery, AgentMailboxDeliveryReceipt, AgentMailboxDeliveryState,
    AuthenticationProof, BuildIdentity, CapabilityAuthorization, CapabilityRequest,
    CapabilityResponse, CommandOutcome, CommandOutcomeAck, EventAck, FleetWelcome, LeaseGrant,
    LeaseRenewal, RosterSnapshotV1, SessionCapabilityPolicy, SessionPolicy, TaskStatus,
    WORKER_PROTOCOL_V1, WorkerCommand, WorkerCommandKind, WorkerHello, WorkerLifecycleState,
    WorkerStatus,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{FleetError, FleetResult};
use crate::model::{
    AdmissionRecord, AgentSessionBinding, AttemptRecord, CapabilityRoute, FleetSnapshot,
    MailboxDeliveryRecord, ProviderAllocationRecord, ProviderAllocationState,
    ProviderCredentialStatus, ProviderLaneRecord, ProviderQuotaStatus, RuntimeLeaseRecord,
    SessionEventDescriptor, SessionRecord, TaskEventObservation, TaskEventProjection, TaskRecord,
    WorkerAuthorityRecord, WorkerAuthorityState, WorktreeOwnershipRecord, WorktreeOwnershipState,
};
use crate::ports::{CapabilityRouter, FleetRepository, IdentityGenerator};
use crate::roster::project_task;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeasePolicy {
    pub lease_duration_ms: u64,
    pub heartbeat_interval_ms: u64,
    pub reattach_grace_ms: u64,
}

impl Default for LeasePolicy {
    fn default() -> Self {
        Self {
            lease_duration_ms: 30_000,
            heartbeat_interval_ms: 10_000,
            reattach_grace_ms: 30_000,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProvisionedWorker {
    pub worker_id: WorkerId,
    pub bootstrap_proof: AuthenticationProof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProvisionedWorkerCommand {
    pub worker: ProvisionedWorker,
    pub command: WorkerCommand,
}

impl std::fmt::Debug for ProvisionedWorker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProvisionedWorker")
            .field("worker_id", &self.worker_id)
            .field("bootstrap_proof", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HandshakeAccepted {
    pub welcome: FleetWelcome,
    pub replay_from_event_seq: u64,
    pub replay_commands: Vec<WorkerCommand>,
}

pub struct FleetAuthority<R, I> {
    repository: R,
    identities: I,
    snapshot: Mutex<FleetSnapshot>,
}

impl<R, I> FleetAuthority<R, I>
where
    R: FleetRepository,
    I: IdentityGenerator,
{
    /// Persists a validated generation-zero import as the first authority
    /// generation, then opens it through the normal startup reconciliation.
    pub fn initialize(
        repository: R,
        identities: I,
        mut imported: FleetSnapshot,
        now_unix_ms: u64,
    ) -> FleetResult<Self> {
        if repository.load()?.is_some() {
            return Err(FleetError::InvalidState(
                "fleet authority is already initialized".into(),
            ));
        }
        if imported.generation != 0 {
            return Err(FleetError::InvalidState(
                "imported fleet snapshot must start at generation zero".into(),
            ));
        }
        imported.validate()?;
        imported.generation = 1;
        repository.compare_and_swap(0, &imported)?;
        Self::open(repository, identities, now_unix_ms)
    }

    /// Opens durable authority state and makes live connections reconnectable.
    /// The conversion is itself persisted before the authority is returned.
    pub fn open(repository: R, identities: I, now_unix_ms: u64) -> FleetResult<Self> {
        let snapshot = repository.load()?.unwrap_or_default();
        snapshot.validate()?;
        let authority = Self {
            repository,
            identities,
            snapshot: Mutex::new(snapshot),
        };
        let needs_reconciliation = {
            let snapshot = authority
                .snapshot
                .lock()
                .map_err(|_| FleetError::LockPoisoned)?;
            snapshot.workers.values().any(|worker| {
                matches!(
                    worker.state,
                    WorkerAuthorityState::AwaitingInitialConnection
                        | WorkerAuthorityState::Active
                        | WorkerAuthorityState::Draining
                )
            })
        };
        if needs_reconciliation {
            authority.commit(|snapshot| {
                let mut abandoned_initial_tasks = BTreeSet::new();
                for worker in snapshot.workers.values_mut() {
                    match worker.state {
                        WorkerAuthorityState::AwaitingInitialConnection => {
                            // The raw bootstrap credential deliberately never
                            // enters the snapshot. After a restart its verifier
                            // cannot launch a process, so retire this ownership
                            // and publish a deterministic recoverable loss
                            // instead of leaving the attempt pending forever.
                            worker.state = WorkerAuthorityState::Lost;
                            worker.updated_at_unix_ms = now_unix_ms;
                            abandoned_initial_tasks.insert(worker.task_id.clone());
                        }
                        WorkerAuthorityState::Active | WorkerAuthorityState::Draining => {
                            worker.state = WorkerAuthorityState::AwaitingReattach;
                            worker.updated_at_unix_ms = now_unix_ms;
                        }
                        _ => {}
                    }
                }
                for task_id in abandoned_initial_tasks {
                    fail_uncommitted_terminal_task(
                        snapshot,
                        &task_id,
                        "initial worker launch was not confirmed before fleetd restart",
                        now_unix_ms,
                    )?;
                }
                Ok(())
            })?;
        }
        Ok(authority)
    }

    pub fn snapshot(&self) -> FleetResult<FleetSnapshot> {
        self.snapshot
            .lock()
            .map_err(|_| FleetError::LockPoisoned)
            .map(|snapshot| snapshot.clone())
    }

    pub fn roster_snapshot(
        &self,
        daemon_version: Option<String>,
        daemon_build_id: Option<String>,
    ) -> FleetResult<RosterSnapshotV1> {
        let snapshot = self.snapshot.lock().map_err(|_| FleetError::LockPoisoned)?;
        Ok(RosterSnapshotV1 {
            version: snapshot.roster_generation,
            tasks: snapshot.roster.values().cloned().collect(),
            daemon_version,
            daemon_build_id,
        })
    }

    pub fn admit_execution(
        &self,
        request: ExecutionRequest,
        now_unix_ms: u64,
    ) -> FleetResult<ExecutionAccepted> {
        if request.idempotency_key.trim().is_empty() {
            return Err(FleetError::InvalidState(
                "execution idempotency key must not be empty".to_string(),
            ));
        }
        let requested_capability_policy = requested_capability_policy(&request.tool_policy)?;
        let digest = request_digest(&request)?;
        let agent_binding = agent_binding_from_labels(&request.labels)?;
        if let Some(admission) = self
            .snapshot
            .lock()
            .map_err(|_| FleetError::LockPoisoned)?
            .admissions
            .get(&request.idempotency_key)
            .cloned()
        {
            if admission.request_digest != digest {
                return Err(FleetError::IdempotencyConflict);
            }
            let mut accepted = admission.accepted;
            accepted.deduplicated = true;
            return Ok(accepted);
        }

        let attempt_id = AttemptId::new(self.identities.next_id("attempt"));
        let task_id = TaskId::new(self.identities.next_id("task"));
        let session_id = match &request.kind {
            ExecutionKind::Fresh { .. } => SessionId::new(self.identities.next_id("session")),
            ExecutionKind::Resume { session_id, .. }
            | ExecutionKind::MailboxResume { session_id } => session_id.clone(),
        };
        let accepted = ExecutionAccepted {
            operation_id: request.operation_id.clone(),
            attempt_id: attempt_id.clone(),
            task_id: task_id.clone(),
            session_id: session_id.clone(),
            deduplicated: false,
        };
        self.commit(move |snapshot| {
            if let Some(prior) = snapshot.admissions.get(&request.idempotency_key) {
                if prior.request_digest != digest {
                    return Err(FleetError::IdempotencyConflict);
                }
                let mut accepted = prior.accepted.clone();
                accepted.deduplicated = true;
                return Ok(accepted);
            }
            if matches!(
                request.kind,
                ExecutionKind::Resume { .. } | ExecutionKind::MailboxResume { .. }
            ) && !snapshot.sessions.contains_key(&session_id)
            {
                return Err(FleetError::not_found("session", session_id.to_string()));
            }

            let (cwd, managed_worktree) = working_set_coordinates(&request.working_set);
            let task = TaskRecord {
                task_id: task_id.clone(),
                session_id: session_id.clone(),
                attempt_id: Some(attempt_id.clone()),
                worker_id: None,
                status: TaskStatus::Pending,
                provider: request.provider,
                model: Some(request.model.clone()),
                cost: None,
                turns: None,
                cwd,
                managed_worktree,
                transcript_path: None,
                transcript_cursor: None,
                last_message: None,
                report: None,
                error_teaser: None,
                label: request.labels.get("label").cloned(),
                name: request.labels.get("name").cloned(),
                agent_label: request.labels.get("agent_label").cloned(),
                origin: request
                    .labels
                    .get("origin")
                    .map(|value| parse_origin(value))
                    .unwrap_or_default(),
                workflow_owned: request
                    .labels
                    .get("workflow_owned")
                    .is_some_and(|value| value == "true"),
                interrupted: false,
                cancellation_requested_at_unix_ms: None,
                recoverable: false,
                started_at_unix_ms: now_unix_ms,
                last_event_at_unix_ms: now_unix_ms,
                completed_at_unix_ms: None,
            };
            let attempt = AttemptRecord {
                attempt_id: attempt_id.clone(),
                operation_id: request.operation_id.clone(),
                task_id: task_id.clone(),
                session_id: session_id.clone(),
                provider: request.provider,
                model: request.model.clone(),
                service_tier: request.service_tier,
                request_digest: digest.clone(),
                state: AttemptState::Accepted,
                result: Value::Null,
                accepted_at_unix_ms: now_unix_ms,
                updated_at_unix_ms: now_unix_ms,
            };
            snapshot.tasks.insert(task_id.clone(), task);
            snapshot
                .attempts
                .insert(attempt_id.clone(), attempt.clone());
            let session = snapshot
                .sessions
                .entry(session_id.clone())
                .or_insert_with(|| SessionRecord {
                    session_id: session_id.clone(),
                    provider: request.provider,
                    task_ids: Vec::new(),
                    current_task_id: None,
                    agent_binding: None,
                    transcript_path: None,
                    requested_capability_policy: requested_capability_policy.clone(),
                    resumable: true,
                    created_at_unix_ms: now_unix_ms,
                    updated_at_unix_ms: now_unix_ms,
                });
            if session.provider != request.provider {
                return Err(FleetError::InvalidState(format!(
                    "resume provider {:?} differs from session provider {:?}",
                    request.provider, session.provider
                )));
            }
            let effective_capability_policy = if matches!(
                &request.kind,
                ExecutionKind::Resume { .. } | ExecutionKind::MailboxResume { .. }
            ) && requested_capability_policy.is_empty()
            {
                session.requested_capability_policy.clone()
            } else {
                requested_capability_policy.clone()
            };
            if session.requested_capability_policy != effective_capability_policy {
                return Err(FleetError::InvalidState(format!(
                    "resume capability policy differs from session {session_id} authority"
                )));
            }
            if let Some(binding) = agent_binding {
                match &session.agent_binding {
                    Some(existing) if existing != &binding => {
                        return Err(FleetError::InvalidState(format!(
                            "session {session_id} is already bound to another logical agent"
                        )));
                    }
                    None => session.agent_binding = Some(binding),
                    Some(_) => {}
                }
            }
            session.task_ids.push(task_id.clone());
            session.current_task_id = Some(task_id.clone());
            session.updated_at_unix_ms = now_unix_ms;
            snapshot.admissions.insert(
                request.idempotency_key.clone(),
                AdmissionRecord {
                    idempotency_key: request.idempotency_key.clone(),
                    request_digest: digest.clone(),
                    accepted: accepted.clone(),
                    accepted_at_unix_ms: now_unix_ms,
                },
            );
            queue_attempt_record(snapshot, &attempt)?;
            refresh_task_roster(snapshot, &task_id)?;
            Ok(accepted)
        })
    }

    pub fn attempt_outcome(&self, attempt_id: &AttemptId) -> FleetResult<AttemptOutcome> {
        let snapshot = self.snapshot.lock().map_err(|_| FleetError::LockPoisoned)?;
        let attempt = snapshot
            .attempts
            .get(attempt_id)
            .ok_or_else(|| FleetError::not_found("attempt", attempt_id.to_string()))?;
        Ok(AttemptOutcome {
            attempt_id: attempt_id.clone(),
            state: attempt.state,
            result: attempt.result.clone(),
        })
    }

    /// Returns the oldest immutable producer records. The records remain
    /// durable until a complete corpus receipt is acknowledged separately.
    pub fn pending_records(&self, limit: usize) -> FleetResult<Vec<RecordEnvelope>> {
        let snapshot = self.snapshot.lock().map_err(|_| FleetError::LockPoisoned)?;
        Ok(snapshot
            .record_outbox
            .values()
            .take(limit)
            .cloned()
            .collect())
    }

    /// Acknowledges a contiguous prefix only after blackboxd confirms the
    /// entire submitted batch. Repeating an already persisted acknowledgement
    /// is harmless, which closes the crash window after a successful ingest.
    pub fn acknowledge_records(&self, through_cursor: u64) -> FleetResult<usize> {
        self.commit(move |snapshot| {
            if through_cursor <= snapshot.acknowledged_record_cursor {
                return Ok(0);
            }
            if through_cursor >= snapshot.next_record_cursor {
                return Err(FleetError::InvalidState(format!(
                    "record acknowledgement {through_cursor} exceeds produced cursor {}",
                    snapshot.next_record_cursor.saturating_sub(1)
                )));
            }

            let start = snapshot
                .acknowledged_record_cursor
                .checked_add(1)
                .ok_or(FleetError::CounterExhausted("acknowledged record cursor"))?;
            let records = (start..=through_cursor)
                .map(|cursor| {
                    snapshot
                        .record_outbox
                        .get(&cursor)
                        .cloned()
                        .ok_or_else(|| {
                            FleetError::InvalidState(format!(
                                "record acknowledgement skipped missing cursor {cursor}"
                            ))
                        })
                        .map(|record| (cursor, record))
                })
                .collect::<FleetResult<Vec<_>>>()?;

            let mut changed_tasks = BTreeSet::new();
            for (record_cursor, record) in &records {
                let (Some(worker_id), Some(event_seq)) = (
                    record.attributes.get("worker_id"),
                    record
                        .attributes
                        .get("event_seq")
                        .and_then(|value| value.parse::<u64>().ok()),
                ) else {
                    continue;
                };
                let worker_id = WorkerId::new(worker_id.clone());
                let worker = snapshot.workers.get_mut(&worker_id).ok_or_else(|| {
                    FleetError::InvalidState(format!(
                        "indexed event record references missing worker {worker_id}"
                    ))
                })?;
                if event_seq > worker.event_ack {
                    return Err(FleetError::InvalidState(format!(
                        "indexed event {event_seq} exceeds worker {} acknowledgement {}",
                        worker.worker_id, worker.event_ack
                    )));
                }
                if event_seq > worker.last_indexed_event_seq {
                    worker.last_indexed_event_seq = event_seq;
                    let task_id = worker.task_id.clone();
                    if let Some(task) = snapshot.tasks.get_mut(&task_id) {
                        task.transcript_cursor = Some(serde_json::json!({
                            "worker_id": worker_id,
                            "event_seq": event_seq,
                            "record_cursor": record_cursor,
                        }));
                    }
                    changed_tasks.insert(task_id);
                }
            }
            for (cursor, _) in &records {
                snapshot.record_outbox.remove(cursor);
            }
            snapshot.acknowledged_record_cursor = through_cursor;
            for task_id in changed_tasks {
                refresh_task_roster(snapshot, &task_id)?;
            }
            Ok(records.len())
        })
    }

    /// Records a resolved allocator decision after admission. This is separate
    /// from the request because account and selection trace are host-resolved,
    /// while prompts and shell environment must never enter authority state.
    pub fn record_runtime_lease(&self, lease: RuntimeLeaseRecord) -> FleetResult<()> {
        self.commit(move |snapshot| {
            let task = snapshot
                .tasks
                .get(&lease.task_id)
                .ok_or_else(|| FleetError::not_found("task", lease.task_id.to_string()))?;
            if task.session_id != lease.session_id || task.provider != lease.provider {
                return Err(FleetError::InvalidState(
                    "runtime lease task, session, or provider mismatch".into(),
                ));
            }
            if lease.selection_trace_id.trim().is_empty() {
                return Err(FleetError::InvalidState(
                    "runtime lease selection trace id must not be empty".into(),
                ));
            }
            if let Some(session) = snapshot.sessions.get_mut(&lease.session_id) {
                session.resumable |= lease.durable;
                session.updated_at_unix_ms =
                    session.updated_at_unix_ms.max(lease.last_seen_at_unix_ms);
            }
            snapshot.runtime_leases.insert(lease.task_id.clone(), lease);
            Ok(())
        })
    }

    pub fn latest_durable_runtime_lease(
        &self,
        session_id: &SessionId,
    ) -> FleetResult<Option<RuntimeLeaseRecord>> {
        let snapshot = self.snapshot.lock().map_err(|_| FleetError::LockPoisoned)?;
        Ok(snapshot
            .runtime_leases
            .values()
            .filter(|lease| lease.session_id == *session_id && lease.durable)
            .max_by_key(|lease| (lease.last_seen_at_unix_ms, lease.created_at_unix_ms))
            .cloned())
    }

    pub fn transition_attempt(
        &self,
        attempt_id: &AttemptId,
        state: AttemptState,
        result: Value,
        now_unix_ms: u64,
    ) -> FleetResult<()> {
        let attempt_id = attempt_id.clone();
        self.commit(move |snapshot| {
            if let Some(attempt) = snapshot.attempts.get(&attempt_id)
                && attempt.state == state
            {
                if attempt.result == result {
                    return Ok(());
                }
                return Err(FleetError::InvalidState(
                    "repeated attempt outcome changed its payload".into(),
                ));
            }
            let (task_id, transitioned_attempt) = {
                let attempt = snapshot
                    .attempts
                    .get_mut(&attempt_id)
                    .ok_or_else(|| FleetError::not_found("attempt", attempt_id.to_string()))?;
                validate_attempt_transition(attempt.state, state)?;
                attempt.state = state;
                attempt.result = result;
                attempt.updated_at_unix_ms = now_unix_ms;
                (attempt.task_id.clone(), attempt.clone())
            };
            let task = snapshot
                .tasks
                .get_mut(&task_id)
                .ok_or_else(|| FleetError::not_found("task", task_id.to_string()))?;
            task.status = task_status_for_attempt(state);
            task.last_event_at_unix_ms = task.last_event_at_unix_ms.max(now_unix_ms);
            if task.status.is_terminal() {
                task.completed_at_unix_ms = Some(now_unix_ms);
                task.interrupted = state == AttemptState::Interrupted;
                task.recoverable = matches!(state, AttemptState::Interrupted | AttemptState::Lost);
            }
            queue_attempt_record(snapshot, &transitioned_attempt)?;
            refresh_task_roster(snapshot, &task_id)?;
            Ok(())
        })
    }

    pub fn record_transcript_path(
        &self,
        task_id: &TaskId,
        path: impl Into<String>,
        now_unix_ms: u64,
    ) -> FleetResult<()> {
        let task_id = task_id.clone();
        let path = path.into();
        if path.trim().is_empty() {
            return Err(FleetError::InvalidState(
                "transcript path must not be empty".into(),
            ));
        }
        self.commit(move |snapshot| {
            let session_id = {
                let task = snapshot
                    .tasks
                    .get_mut(&task_id)
                    .ok_or_else(|| FleetError::not_found("task", task_id.to_string()))?;
                if let Some(existing) = &task.transcript_path
                    && existing != &path
                {
                    return Err(FleetError::InvalidState(
                        "task transcript path changed after publication".into(),
                    ));
                }
                task.transcript_path = Some(path.clone());
                task.last_event_at_unix_ms = task.last_event_at_unix_ms.max(now_unix_ms);
                task.session_id.clone()
            };
            if let Some(session) = snapshot.sessions.get_mut(&session_id) {
                if session.transcript_path.is_none() {
                    session.transcript_path = Some(path);
                }
                session.updated_at_unix_ms = session.updated_at_unix_ms.max(now_unix_ms);
            }
            refresh_task_roster(snapshot, &task_id)
        })
    }

    pub fn provision_worker(
        &self,
        task_id: &TaskId,
        now_unix_ms: u64,
    ) -> FleetResult<ProvisionedWorker> {
        self.provision_worker_inner(task_id, None, now_unix_ms)
            .map(|(worker, _)| worker)
    }

    /// Provision worker authority and its first command in one durable
    /// generation. A worker can never connect to a task whose initial turn was
    /// only present in process memory.
    pub fn provision_worker_with_initial_command(
        &self,
        task_id: &TaskId,
        command: WorkerCommandKind,
        now_unix_ms: u64,
    ) -> FleetResult<ProvisionedWorkerCommand> {
        let (worker, command) = self.provision_worker_inner(task_id, Some(command), now_unix_ms)?;
        Ok(ProvisionedWorkerCommand {
            worker,
            command: command.expect("initial command was supplied"),
        })
    }

    fn provision_worker_inner(
        &self,
        task_id: &TaskId,
        initial_command: Option<WorkerCommandKind>,
        now_unix_ms: u64,
    ) -> FleetResult<(ProvisionedWorker, Option<WorkerCommand>)> {
        let worker_id = WorkerId::new(self.identities.next_id("worker"));
        let proof = AuthenticationProof::new(self.identities.next_id("proof"));
        let proof_hash = hash_secret(proof.expose_secret());
        let command_id = initial_command
            .as_ref()
            .map(|_| CommandId::new(self.identities.next_id("command")));
        let task_id = task_id.clone();
        let provisioned = ProvisionedWorker {
            worker_id: worker_id.clone(),
            bootstrap_proof: proof,
        };
        let persisted_command = self.commit(|snapshot| {
            let task_has_live_owner = snapshot
                .tasks
                .get(&task_id)
                .and_then(|task| task.worker_id.as_ref())
                .and_then(|worker_id| snapshot.workers.get(worker_id))
                .is_some_and(|worker| !worker.state.is_terminal());
            let task = snapshot
                .tasks
                .get_mut(&task_id)
                .ok_or_else(|| FleetError::not_found("task", task_id.to_string()))?;
            if task.status.is_terminal() {
                return Err(FleetError::InvalidState(format!(
                    "task {task_id} already has a terminal attempt"
                )));
            }
            if task_has_live_owner
                || snapshot
                    .workers
                    .values()
                    .any(|worker| worker.task_id == task_id && !worker.state.is_terminal())
            {
                return Err(FleetError::InvalidState(format!(
                    "task {task_id} already has nonterminal worker authority"
                )));
            }
            let session_id = task.session_id.clone();
            let requested_capability_policy = snapshot
                .sessions
                .get(&session_id)
                .ok_or_else(|| FleetError::not_found("session", session_id.to_string()))?
                .requested_capability_policy
                .clone();
            task.worker_id = Some(worker_id.clone());
            let initial = initial_command.map(|command| WorkerCommand {
                command_seq: 1,
                command_id: command_id.expect("command identity was allocated"),
                command,
            });
            let mut pending_commands = BTreeMap::new();
            if let Some(command) = &initial {
                pending_commands.insert(command.command_seq, command.clone());
            }
            snapshot.workers.insert(
                worker_id.clone(),
                WorkerAuthorityRecord {
                    worker_id: worker_id.clone(),
                    task_id: task_id.clone(),
                    session_id,
                    worker_build: None,
                    protocol_version: None,
                    connection_generation: 0,
                    bootstrap_proof_hash: Some(proof_hash),
                    reconnect_proof_hash: None,
                    previous_reconnect_proof_hash: None,
                    pending_reconnect_proof_hash: None,
                    connection_confirmed: false,
                    lease: None,
                    session_policy: None,
                    requested_capability_policy,
                    event_ack: 0,
                    last_indexed_event_seq: 0,
                    next_command_seq: if initial.is_some() { 2 } else { 1 },
                    command_outcome_ack: 0,
                    pending_commands,
                    pending_command_outcomes: BTreeMap::new(),
                    pending_terminal_projection: None,
                    state: WorkerAuthorityState::AwaitingInitialConnection,
                    updated_at_unix_ms: now_unix_ms,
                },
            );
            Ok(initial)
        })?;
        Ok((provisioned, persisted_command))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn accept_handshake(
        &self,
        hello: WorkerHello,
        session_policy: SessionPolicy,
        fleet_build: BuildIdentity,
        lease_policy: LeasePolicy,
        now_unix_ms: u64,
    ) -> FleetResult<HandshakeAccepted> {
        if !hello.protocol_versions.contains(&WORKER_PROTOCOL_V1) {
            return Err(FleetError::HandshakeRejected(format!(
                "worker does not support protocol {WORKER_PROTOCOL_V1}"
            )));
        }
        let reconnect_proof = AuthenticationProof::new(self.identities.next_id("reconnect"));
        let reconnect_hash = hash_secret(reconnect_proof.expose_secret());
        let lease_id = self.identities.next_id("lease");
        self.commit(move |snapshot| {
            let worker = snapshot
                .workers
                .get_mut(&hello.worker_id)
                .ok_or_else(|| FleetError::HandshakeRejected("worker is not provisioned".into()))?;
            if worker.task_id != hello.task_id || worker.session_id != hello.session_id {
                return Err(FleetError::HandshakeRejected(
                    "worker task or session identity does not match provisioning".into(),
                ));
            }
            if worker.state.is_terminal() {
                return Err(FleetError::HandshakeRejected(
                    "worker authority is terminal".into(),
                ));
            }
            if matches!(worker.state, WorkerAuthorityState::Active | WorkerAuthorityState::Draining)
                && worker
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.expires_at_unix_ms > now_unix_ms)
            {
                return Err(FleetError::HandshakeRejected(
                    "the current connection still holds a live lease".into(),
                ));
            }
            if hello.last_local_event_seq < worker.event_ack {
                return Err(FleetError::HandshakeRejected(format!(
                    "worker cannot replay fleet event acknowledgement {}",
                    worker.event_ack
                )));
            }
            let last_allocated_command = worker.next_command_seq.saturating_sub(1);
            if hello.last_fleet_command_seq > last_allocated_command {
                return Err(FleetError::HandshakeRejected(format!(
                    "worker reports command sequence {} beyond allocated sequence {last_allocated_command}",
                    hello.last_fleet_command_seq
                )));
            }
            let presented_hash = hash_secret(hello.bootstrap_or_resume_proof.expose_secret());
            let presented_pending = worker
                .pending_reconnect_proof_hash
                .as_deref()
                .is_some_and(|hash| hash == presented_hash);
            let proof_matches = worker
                .reconnect_proof_hash
                .as_deref()
                .is_some_and(|hash| hash == presented_hash)
                || presented_pending
                || worker
                    .previous_reconnect_proof_hash
                    .as_deref()
                    .is_some_and(|hash| hash == presented_hash)
                || (!worker.connection_confirmed
                    && worker.reconnect_proof_hash.is_none()
                    && worker
                        .bootstrap_proof_hash
                        .as_deref()
                        .is_some_and(|hash| hash == presented_hash));
            if !proof_matches {
                return Err(FleetError::HandshakeRejected(
                    "worker authentication proof is invalid".into(),
                ));
            }
            validate_session_policy_transition(worker.session_policy.as_ref(), &session_policy)?;

            // A worker that reconnects with the credential from an unconfirmed
            // welcome proves it durably installed that welcome. Promote the
            // verifier before issuing the next pending credential.
            if presented_pending {
                worker.previous_reconnect_proof_hash = worker.reconnect_proof_hash.take();
                worker.reconnect_proof_hash = worker.pending_reconnect_proof_hash.take();
                worker.bootstrap_proof_hash = None;
            }

            let connection_generation = next_counter(
                worker.connection_generation,
                "worker connection generation",
            )?;
            let lease = LeaseGrant {
                lease_id,
                expires_at_unix_ms: now_unix_ms.saturating_add(lease_policy.lease_duration_ms),
                heartbeat_interval_ms: lease_policy.heartbeat_interval_ms,
                reattach_grace_ms: lease_policy.reattach_grace_ms,
            };
            worker.pending_reconnect_proof_hash = Some(reconnect_hash);
            worker.connection_confirmed = false;
            worker.connection_generation = connection_generation;
            worker.worker_build = Some(hello.worker_build.clone());
            worker.protocol_version = Some(WORKER_PROTOCOL_V1);
            worker.lease = Some(lease.clone());
            worker.session_policy = Some(session_policy.clone());
            worker.state = WorkerAuthorityState::Active;
            worker.updated_at_unix_ms = now_unix_ms;
            let replay_from_event_seq = next_counter(worker.event_ack, "worker event sequence")?;
            let replay_command_start = next_counter(
                hello.last_fleet_command_seq,
                "worker command replay sequence",
            )?;
            let replay_commands = worker
                .pending_commands
                .range(replay_command_start..)
                .map(|(_, command)| command.clone())
                .collect();
            let next_command_seq = worker.next_command_seq;
            let event_ack = worker.event_ack;
            let task_id = worker.task_id.clone();

            let mut transitioned_attempt = None;
            if let Some(task) = snapshot.tasks.get_mut(&task_id) {
                task.status = TaskStatus::Running;
                task.last_event_at_unix_ms = task.last_event_at_unix_ms.max(now_unix_ms);
                if let Some(attempt_id) = &task.attempt_id
                    && let Some(attempt) = snapshot.attempts.get_mut(attempt_id)
                    && attempt.state == AttemptState::Accepted
                {
                    attempt.state = AttemptState::Running;
                    attempt.updated_at_unix_ms = now_unix_ms;
                    transitioned_attempt = Some(attempt.clone());
                }
            }
            if let Some(attempt) = transitioned_attempt.as_ref() {
                queue_attempt_record(snapshot, attempt)?;
            }
            refresh_task_roster(snapshot, &task_id)?;
            Ok(HandshakeAccepted {
                welcome: FleetWelcome {
                    selected_protocol: WORKER_PROTOCOL_V1,
                    connection_generation,
                    event_ack,
                    next_command_seq,
                    lease,
                    reconnect_proof,
                    session_policy,
                    fleet_build,
                },
                replay_from_event_seq,
                replay_commands,
            })
        })
    }

    /// Persist a complete monotonic service-policy revision for a connected
    /// worker before fleetd publishes it on that generation's control lane.
    pub fn update_session_policy(
        &self,
        worker_id: &WorkerId,
        connection_generation: u64,
        session_policy: SessionPolicy,
        now_unix_ms: u64,
    ) -> FleetResult<()> {
        let worker_id = worker_id.clone();
        self.commit(move |snapshot| {
            let worker = snapshot
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| FleetError::not_found("worker", worker_id.to_string()))?;
            fence_worker(worker, connection_generation)?;
            if !matches!(
                worker.state,
                WorkerAuthorityState::Active | WorkerAuthorityState::Draining
            ) {
                return Err(FleetError::InvalidState(format!(
                    "worker {worker_id} cannot accept a live service policy while {:?}",
                    worker.state
                )));
            }
            validate_session_policy_transition(worker.session_policy.as_ref(), &session_policy)?;
            worker.session_policy = Some(session_policy);
            worker.updated_at_unix_ms = worker.updated_at_unix_ms.max(now_unix_ms);
            Ok(())
        })
    }

    /// Confirm that at least one valid post-handshake frame arrived on this
    /// connection generation. Only then is the welcome credential promoted
    /// and the proof used for the prior generation retired.
    pub fn confirm_connection(
        &self,
        worker_id: &WorkerId,
        connection_generation: u64,
        now_unix_ms: u64,
    ) -> FleetResult<bool> {
        let worker_id = worker_id.clone();
        self.commit(move |snapshot| {
            let worker = snapshot
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| FleetError::not_found("worker", worker_id.to_string()))?;
            fence_worker(worker, connection_generation)?;
            if worker.connection_confirmed {
                return Ok(false);
            }
            let pending = worker.pending_reconnect_proof_hash.take().ok_or_else(|| {
                FleetError::InvalidState(
                    "unconfirmed worker connection has no pending reconnect proof".into(),
                )
            })?;
            worker.previous_reconnect_proof_hash = worker.reconnect_proof_hash.take();
            worker.reconnect_proof_hash = Some(pending);
            worker.bootstrap_proof_hash = None;
            worker.previous_reconnect_proof_hash = None;
            worker.connection_confirmed = true;
            worker.updated_at_unix_ms = now_unix_ms;
            Ok(true)
        })
    }

    pub fn observe_event(
        &self,
        worker_id: &WorkerId,
        connection_generation: u64,
        event_seq: u64,
        occurred_at_unix_ms: u64,
        projection: Option<TaskEventProjection>,
    ) -> FleetResult<EventAck> {
        self.observe_event_observation(
            worker_id,
            connection_generation,
            event_seq,
            occurred_at_unix_ms,
            TaskEventObservation {
                projection,
                ..TaskEventObservation::default()
            },
        )
    }

    pub fn observe_event_observation(
        &self,
        worker_id: &WorkerId,
        connection_generation: u64,
        event_seq: u64,
        occurred_at_unix_ms: u64,
        observation: TaskEventObservation,
    ) -> FleetResult<EventAck> {
        let worker_id = worker_id.clone();
        self.commit(move |snapshot| {
            let TaskEventObservation {
                projection,
                defer_terminal,
                commit_deferred_terminal,
                record,
            } = observation;
            let worker = snapshot
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| FleetError::not_found("worker", worker_id.to_string()))?;
            fence_worker(worker, connection_generation)?;
            if event_seq <= worker.event_ack {
                return Ok(EventAck {
                    through_event_seq: worker.event_ack,
                });
            }
            let expected = next_counter(worker.event_ack, "worker event sequence")?;
            if event_seq != expected {
                return Err(FleetError::SequenceGap {
                    stream: "worker events",
                    expected,
                    actual: event_seq,
                });
            }
            worker.event_ack = event_seq;
            worker.updated_at_unix_ms = occurred_at_unix_ms;
            let task_id = worker.task_id.clone();
            let session_id = worker.session_id.clone();
            let mut projections = Vec::new();
            if let Some(projection) = projection {
                projections.push(projection);
            }
            if let Some(deferred) = defer_terminal {
                if !deferred.status.is_some_and(|status| status.is_terminal()) {
                    return Err(FleetError::InvalidState(
                        "deferred worker projection must carry a terminal status".into(),
                    ));
                }
                if let Some(prior) = &worker.pending_terminal_projection
                    && prior != &deferred
                {
                    return Err(FleetError::InvalidState(
                        "worker replaced an uncommitted terminal projection".into(),
                    ));
                }
                worker.pending_terminal_projection = Some(deferred);
            }
            if commit_deferred_terminal
                && let Some(deferred) = worker.pending_terminal_projection.take()
            {
                projections.push(deferred);
            }

            let mut transitioned_attempts = Vec::new();
            for projection in projections {
                let projected_status = projection.status;
                let (attempt_id, task_status) = {
                    let task = snapshot
                        .tasks
                        .get_mut(&task_id)
                        .ok_or_else(|| FleetError::not_found("task", task_id.to_string()))?;
                    if let Some(status) = projected_status {
                        validate_task_transition(task.status, status)?;
                    }
                    task.apply_projection(projection, occurred_at_unix_ms);
                    (task.attempt_id.clone(), task.status)
                };
                if let Some(attempt_id) = attempt_id {
                    let attempt = snapshot
                        .attempts
                        .get_mut(&attempt_id)
                        .ok_or_else(|| FleetError::not_found("attempt", attempt_id.to_string()))?;
                    let next_state = attempt_state_for_task(task_status, attempt.state);
                    if next_state != attempt.state {
                        validate_attempt_transition(attempt.state, next_state)?;
                        attempt.state = next_state;
                        attempt.updated_at_unix_ms = occurred_at_unix_ms;
                        transitioned_attempts.push(attempt.clone());
                    }
                }
                refresh_task_roster(snapshot, &task_id)?;
            }
            for attempt in &transitioned_attempts {
                queue_attempt_record(snapshot, attempt)?;
            }
            if let Some(record) = record.as_ref() {
                queue_session_event_record(
                    snapshot,
                    &worker_id,
                    &task_id,
                    &session_id,
                    event_seq,
                    occurred_at_unix_ms,
                    record,
                )?;
            }
            Ok(EventAck {
                through_event_seq: event_seq,
            })
        })
    }

    pub fn enqueue_command(
        &self,
        task_id: &TaskId,
        command: WorkerCommandKind,
        now_unix_ms: u64,
    ) -> FleetResult<WorkerCommand> {
        let task_id = task_id.clone();
        let command_id = CommandId::new(self.identities.next_id("command"));
        let cancellation_intent = matches!(
            &command,
            WorkerCommandKind::Interrupt | WorkerCommandKind::Shutdown { .. }
        );
        self.commit(move |snapshot| {
            let worker_id = snapshot
                .tasks
                .get(&task_id)
                .and_then(|task| task.worker_id.clone())
                .or_else(|| {
                    snapshot
                        .workers
                        .values()
                        .filter(|worker| worker.task_id == task_id && !worker.state.is_terminal())
                        .max_by_key(|worker| worker.connection_generation)
                        .map(|worker| worker.worker_id.clone())
                })
                .ok_or_else(|| FleetError::NoLiveWorker(task_id.to_string()))?;
            let worker = snapshot
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| FleetError::NoLiveWorker(task_id.to_string()))?;
            if worker.state.is_terminal() {
                return Err(FleetError::NoLiveWorker(task_id.to_string()));
            }
            let command = WorkerCommand {
                command_seq: worker.next_command_seq,
                command_id,
                command,
            };
            worker.next_command_seq =
                next_counter(worker.next_command_seq, "worker command sequence")?;
            worker
                .pending_commands
                .insert(command.command_seq, command.clone());
            worker.updated_at_unix_ms = now_unix_ms;
            if cancellation_intent && let Some(task) = snapshot.tasks.get_mut(&task_id) {
                task.cancellation_requested_at_unix_ms
                    .get_or_insert(now_unix_ms);
            }
            Ok(command)
        })
    }

    /// Idempotently bind and enqueue one logical-agent mailbox delivery.
    /// The returned command is present while admission is pending so fleetd
    /// can replay it to a live handle without creating another sequence.
    pub fn enqueue_mailbox_delivery(
        &self,
        delivery: AgentMailboxDelivery,
        now_unix_ms: u64,
    ) -> FleetResult<(WorkerId, Option<WorkerCommand>, AgentMailboxDeliveryReceipt)> {
        validate_mailbox_delivery(&delivery)?;
        let digest = mailbox_delivery_digest(&delivery)?;
        let existing = {
            let snapshot = self.snapshot.lock().map_err(|_| FleetError::LockPoisoned)?;
            snapshot
                .mailbox_deliveries
                .get(&delivery.delivery_id)
                .cloned()
        };
        if let Some(existing) = existing {
            if existing.request_digest != digest {
                return Err(FleetError::IdempotencyConflict);
            }
            let snapshot = self.snapshot.lock().map_err(|_| FleetError::LockPoisoned)?;
            let command = (existing.state == AgentMailboxDeliveryState::Pending)
                .then(|| {
                    snapshot
                        .workers
                        .get(&existing.worker_id)
                        .and_then(|worker| worker.pending_commands.get(&existing.command_seq))
                        .cloned()
                })
                .flatten();
            return Ok((
                existing.worker_id.clone(),
                command,
                mailbox_delivery_receipt(&existing),
            ));
        }

        let command_id = CommandId::new(self.identities.next_id("command"));
        self.commit(move |snapshot| {
            if let Some(existing) = snapshot.mailbox_deliveries.get(&delivery.delivery_id) {
                if existing.request_digest != digest {
                    return Err(FleetError::IdempotencyConflict);
                }
                let command = (existing.state == AgentMailboxDeliveryState::Pending)
                    .then(|| {
                        snapshot
                            .workers
                            .get(&existing.worker_id)
                            .and_then(|worker| worker.pending_commands.get(&existing.command_seq))
                            .cloned()
                    })
                    .flatten();
                return Ok((
                    existing.worker_id.clone(),
                    command,
                    mailbox_delivery_receipt(existing),
                ));
            }

            let binding = AgentSessionBinding {
                agent_id: delivery.target_agent_id.clone(),
                canonical_path: delivery.canonical_target.clone(),
            };
            let task_id = {
                let session = snapshot
                    .sessions
                    .get_mut(&delivery.session_id)
                    .ok_or_else(|| {
                        FleetError::not_found("session", delivery.session_id.to_string())
                    })?;
                match &session.agent_binding {
                    Some(existing) if existing != &binding => {
                        return Err(FleetError::InvalidState(format!(
                            "session {} is bound to another canonical agent",
                            delivery.session_id
                        )));
                    }
                    None => session.agent_binding = Some(binding),
                    Some(_) => {}
                }
                session
                    .current_task_id
                    .clone()
                    .ok_or_else(|| FleetError::NoLiveWorker(delivery.session_id.to_string()))?
            };
            let worker_id = snapshot
                .tasks
                .get(&task_id)
                .and_then(|task| task.worker_id.clone())
                .or_else(|| {
                    snapshot
                        .workers
                        .values()
                        .filter(|worker| {
                            worker.session_id == delivery.session_id && !worker.state.is_terminal()
                        })
                        .max_by_key(|worker| worker.connection_generation)
                        .map(|worker| worker.worker_id.clone())
                })
                .ok_or_else(|| FleetError::NoLiveWorker(delivery.session_id.to_string()))?;
            let worker = snapshot
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| FleetError::NoLiveWorker(delivery.session_id.to_string()))?;
            if worker.state.is_terminal() || worker.session_id != delivery.session_id {
                return Err(FleetError::NoLiveWorker(delivery.session_id.to_string()));
            }
            let command = WorkerCommand {
                command_seq: worker.next_command_seq,
                command_id: command_id.clone(),
                command: WorkerCommandKind::AgentMailbox {
                    delivery: Box::new(delivery.clone()),
                },
            };
            worker.next_command_seq =
                next_counter(worker.next_command_seq, "worker command sequence")?;
            worker
                .pending_commands
                .insert(command.command_seq, command.clone());
            worker.updated_at_unix_ms = now_unix_ms;
            let record = MailboxDeliveryRecord {
                delivery_id: delivery.delivery_id.clone(),
                request_digest: digest.clone(),
                target_agent_id: delivery.target_agent_id.clone(),
                canonical_target: delivery.canonical_target.clone(),
                session_id: delivery.session_id.clone(),
                through_sequence: delivery.through_sequence,
                worker_id: worker_id.clone(),
                command_seq: command.command_seq,
                command_id: command.command_id.clone(),
                state: AgentMailboxDeliveryState::Pending,
                last_error: None,
                updated_at_unix_ms: now_unix_ms,
            };
            let receipt = mailbox_delivery_receipt(&record);
            snapshot
                .mailbox_deliveries
                .insert(record.delivery_id.clone(), record);
            Ok((worker_id, Some(command), receipt))
        })
    }

    pub fn observe_command_outcome(
        &self,
        worker_id: &WorkerId,
        connection_generation: u64,
        outcome: CommandOutcome,
        now_unix_ms: u64,
    ) -> FleetResult<CommandOutcomeAck> {
        let worker_id = worker_id.clone();
        self.commit(move |snapshot| {
            let worker = snapshot
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| FleetError::not_found("worker", worker_id.to_string()))?;
            fence_worker(worker, connection_generation)?;
            if outcome.command_seq <= worker.command_outcome_ack {
                return Ok(CommandOutcomeAck {
                    through_command_seq: worker.command_outcome_ack,
                });
            }
            let command = worker
                .pending_commands
                .get(&outcome.command_seq)
                .ok_or(FleetError::CommandOutcomeConflict)?;
            if command.command_id != outcome.command_id {
                return Err(FleetError::CommandOutcomeConflict);
            }
            let mailbox_command_id = command.command_id.clone();
            let mailbox_delivery_id = match &command.command {
                WorkerCommandKind::AgentMailbox { delivery } => Some(delivery.delivery_id.clone()),
                _ => None,
            };
            if let Some(prior) = worker.pending_command_outcomes.get(&outcome.command_seq) {
                let compatible = prior == &outcome
                    || (prior.command_seq == outcome.command_seq
                        && prior.command_id == outcome.command_id
                        && prior.accepted == outcome.accepted
                        && !prior.terminal
                        && outcome.terminal);
                if !compatible {
                    return Err(FleetError::CommandOutcomeConflict);
                }
            }
            let outcome_terminal = outcome.terminal;
            let outcome_accepted = outcome.accepted;
            worker
                .pending_command_outcomes
                .insert(outcome.command_seq, outcome);
            loop {
                let next = next_counter(
                    worker.command_outcome_ack,
                    "worker command outcome sequence",
                )?;
                if !worker
                    .pending_command_outcomes
                    .get(&next)
                    .is_some_and(|outcome| outcome.terminal)
                {
                    break;
                }
                worker.pending_command_outcomes.remove(&next);
                worker.pending_commands.remove(&next);
                worker.command_outcome_ack = next;
            }
            worker.updated_at_unix_ms = now_unix_ms;
            let ack = CommandOutcomeAck {
                through_command_seq: worker.command_outcome_ack,
            };
            if outcome_terminal && let Some(delivery_id) = mailbox_delivery_id {
                let delivery = snapshot
                    .mailbox_deliveries
                    .get_mut(&delivery_id)
                    .ok_or(FleetError::CommandOutcomeConflict)?;
                if delivery.worker_id != worker_id || delivery.command_id != mailbox_command_id {
                    return Err(FleetError::CommandOutcomeConflict);
                }
                delivery.state = if outcome_accepted {
                    AgentMailboxDeliveryState::Admitted
                } else {
                    AgentMailboxDeliveryState::Rejected
                };
                delivery.last_error = (!outcome_accepted)
                    .then(|| "worker rejected mailbox delivery admission".to_string());
                delivery.updated_at_unix_ms = now_unix_ms;
            }
            Ok(ack)
        })
    }

    pub fn renew_lease(
        &self,
        worker_id: &WorkerId,
        connection_generation: u64,
        lease_id: &str,
        now_unix_ms: u64,
    ) -> FleetResult<LeaseRenewal> {
        let worker_id = worker_id.clone();
        let lease_id = lease_id.to_string();
        self.commit(move |snapshot| {
            let worker = snapshot
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| FleetError::not_found("worker", worker_id.to_string()))?;
            fence_worker(worker, connection_generation)?;
            let lease = worker
                .lease
                .as_mut()
                .ok_or_else(|| FleetError::InvalidState("worker has no lease".into()))?;
            if lease.lease_id != lease_id {
                return Err(FleetError::InvalidState("lease identity mismatch".into()));
            }
            lease.expires_at_unix_ms = lease.expires_at_unix_ms.max(
                now_unix_ms.saturating_add(
                    lease
                        .heartbeat_interval_ms
                        .saturating_mul(3)
                        .max(lease.heartbeat_interval_ms),
                ),
            );
            worker.updated_at_unix_ms = now_unix_ms;
            Ok(LeaseRenewal {
                lease_id,
                renewed_at_unix_ms: now_unix_ms,
                expires_at_unix_ms: lease.expires_at_unix_ms,
                next_heartbeat_due_unix_ms: now_unix_ms.saturating_add(lease.heartbeat_interval_ms),
            })
        })
    }

    pub fn mark_disconnected(
        &self,
        worker_id: &WorkerId,
        connection_generation: u64,
        now_unix_ms: u64,
    ) -> FleetResult<bool> {
        let worker_id = worker_id.clone();
        self.commit(move |snapshot| {
            let worker = snapshot
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| FleetError::not_found("worker", worker_id.to_string()))?;
            if worker.connection_generation != connection_generation {
                return Ok(false);
            }
            if worker.state.is_terminal() {
                return Ok(false);
            }
            worker.state = WorkerAuthorityState::AwaitingReattach;
            worker.updated_at_unix_ms = now_unix_ms;
            Ok(true)
        })
    }

    pub fn observe_status(
        &self,
        worker_id: &WorkerId,
        connection_generation: u64,
        status: WorkerStatus,
        now_unix_ms: u64,
    ) -> FleetResult<()> {
        let worker_id = worker_id.clone();
        self.commit(move |snapshot| {
            let worker = snapshot
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| FleetError::not_found("worker", worker_id.to_string()))?;
            fence_worker(worker, connection_generation)?;
            if status.worker_id != worker.worker_id
                || status.task_id != worker.task_id
                || status.session_id != worker.session_id
                || status.connection_generation != connection_generation
            {
                return Err(FleetError::InvalidState(
                    "worker status identity differs from authenticated connection".into(),
                ));
            }
            if status.last_local_event_seq < worker.event_ack
                || status.last_fleet_command_seq >= worker.next_command_seq
            {
                return Err(FleetError::InvalidState(
                    "worker status sequence bounds contradict durable authority".into(),
                ));
            }
            worker.worker_build = Some(status.worker_build);
            worker.updated_at_unix_ms = now_unix_ms;
            let task_id = worker.task_id.clone();
            match status.state {
                WorkerLifecycleState::Draining => worker.state = WorkerAuthorityState::Draining,
                WorkerLifecycleState::Terminal => worker.state = WorkerAuthorityState::Terminal,
                WorkerLifecycleState::Active => worker.state = WorkerAuthorityState::Active,
                WorkerLifecycleState::Disconnected | WorkerLifecycleState::Reconnecting => {
                    worker.state = WorkerAuthorityState::AwaitingReattach;
                }
                WorkerLifecycleState::Starting | WorkerLifecycleState::Connecting => {}
            }
            if status.state == WorkerLifecycleState::Terminal {
                if let Some(worker) = snapshot.workers.get_mut(&worker_id) {
                    worker.pending_terminal_projection = None;
                }
                fail_uncommitted_terminal_task(
                    snapshot,
                    &task_id,
                    "worker exited before a committed terminal session snapshot",
                    now_unix_ms,
                )?;
            }
            Ok(())
        })
    }

    pub fn mark_worker_lost(
        &self,
        worker_id: &WorkerId,
        connection_generation: u64,
        reason: impl Into<String>,
        now_unix_ms: u64,
    ) -> FleetResult<()> {
        let worker_id = worker_id.clone();
        let reason = reason.into();
        self.commit(move |snapshot| {
            let worker = snapshot
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| FleetError::not_found("worker", worker_id.to_string()))?;
            fence_worker(worker, connection_generation)?;
            worker.state = WorkerAuthorityState::Lost;
            worker.pending_terminal_projection = None;
            worker.updated_at_unix_ms = now_unix_ms;
            let task_id = worker.task_id.clone();
            fail_uncommitted_terminal_task(snapshot, &task_id, &reason, now_unix_ms)
        })
    }

    pub fn expire_stale(&self, now_unix_ms: u64) -> FleetResult<Vec<WorkerId>> {
        self.commit(move |snapshot| {
            let expired: Vec<WorkerId> = snapshot
                .workers
                .values()
                .filter(|worker| {
                    !worker.state.is_terminal()
                        && worker.lease.as_ref().is_some_and(|lease| {
                            now_unix_ms
                                > lease
                                    .expires_at_unix_ms
                                    .saturating_add(lease.reattach_grace_ms)
                        })
                })
                .map(|worker| worker.worker_id.clone())
                .collect();
            let mut transitioned_attempts = Vec::new();
            for worker_id in &expired {
                let task_id = {
                    let worker = snapshot
                        .workers
                        .get_mut(worker_id)
                        .ok_or_else(|| FleetError::not_found("worker", worker_id.to_string()))?;
                    worker.state = WorkerAuthorityState::Lost;
                    worker.updated_at_unix_ms = now_unix_ms;
                    worker.task_id.clone()
                };
                if let Some(task) = snapshot.tasks.get_mut(&task_id) {
                    task.status = TaskStatus::Failed;
                    task.recoverable = true;
                    task.completed_at_unix_ms = Some(now_unix_ms);
                    task.last_event_at_unix_ms = task.last_event_at_unix_ms.max(now_unix_ms);
                    if let Some(attempt_id) = &task.attempt_id
                        && let Some(attempt) = snapshot.attempts.get_mut(attempt_id)
                    {
                        attempt.state = AttemptState::Lost;
                        attempt.updated_at_unix_ms = now_unix_ms;
                        transitioned_attempts.push(attempt.clone());
                    }
                    refresh_task_roster(snapshot, &task_id)?;
                }
            }
            for attempt in &transitioned_attempts {
                queue_attempt_record(snapshot, attempt)?;
            }
            Ok(expired)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn claim_worktree(
        &self,
        path: impl Into<String>,
        base_repo: impl Into<String>,
        task_id: &TaskId,
        attempt_id: Option<AttemptId>,
        seed_dirs: Vec<String>,
        now_unix_ms: u64,
    ) -> FleetResult<WorktreeOwnershipRecord> {
        let path = path.into();
        let base_repo = base_repo.into();
        let task_id = task_id.clone();
        self.commit(move |snapshot| {
            if !snapshot.tasks.contains_key(&task_id) {
                return Err(FleetError::not_found("task", task_id.to_string()));
            }
            if let Some(existing) = snapshot.worktrees.get(&path)
                && existing.state != WorktreeOwnershipState::Released
            {
                if existing.task_id != task_id {
                    return Err(FleetError::WorktreeConflict {
                        path: path.clone(),
                        owner: existing.task_id.to_string(),
                    });
                }
                if existing.state == WorktreeOwnershipState::Active
                    && existing.base_repo == base_repo
                    && existing.attempt_id == attempt_id
                    && existing.seed_dirs == seed_dirs
                {
                    return Ok(existing.clone());
                }
                return Err(FleetError::InvalidState(format!(
                    "worktree {path} already has non-idempotent ownership for task {task_id}"
                )));
            }
            let ownership_generation = snapshot
                .worktrees
                .get(&path)
                .map(|record| {
                    next_counter(record.ownership_generation, "worktree ownership generation")
                })
                .transpose()?
                .unwrap_or(1);
            let record = WorktreeOwnershipRecord {
                path: path.clone(),
                base_repo,
                task_id: task_id.clone(),
                attempt_id,
                ownership_generation,
                state: WorktreeOwnershipState::Active,
                seed_dirs,
                materialized: false,
                claimed_at_unix_ms: now_unix_ms,
                updated_at_unix_ms: now_unix_ms,
            };
            snapshot.worktrees.insert(path.clone(), record.clone());
            if let Some(task) = snapshot.tasks.get_mut(&task_id) {
                task.cwd = Some(path.clone());
                task.managed_worktree = Some(path.clone());
            }
            refresh_task_roster(snapshot, &task_id)?;
            Ok(record)
        })
    }

    pub fn mark_worktree_materialized(
        &self,
        path: &str,
        owner: &TaskId,
        ownership_generation: u64,
        now_unix_ms: u64,
    ) -> FleetResult<WorktreeOwnershipRecord> {
        let path = path.to_string();
        let owner = owner.clone();
        self.commit(move |snapshot| {
            let record = snapshot
                .worktrees
                .get_mut(&path)
                .ok_or_else(|| FleetError::not_found("worktree", path.clone()))?;
            fence_worktree(record, &owner, ownership_generation)?;
            if record.state != WorktreeOwnershipState::Active {
                return Err(FleetError::InvalidState(format!(
                    "worktree {path} is not active while marking it materialized"
                )));
            }
            record.materialized = true;
            record.updated_at_unix_ms = now_unix_ms;
            Ok(record.clone())
        })
    }

    pub fn configure_provider_lane(
        &self,
        mut lane: ProviderLaneRecord,
    ) -> FleetResult<ProviderLaneRecord> {
        self.commit(move |snapshot| {
            if lane.lane_id.trim().is_empty() || lane.max_concurrent == 0 {
                return Err(FleetError::InvalidState(
                    "provider lane identity and concurrency must be nonzero".into(),
                ));
            }
            if let Some(existing) = snapshot.provider_lanes.get(&lane.lane_id) {
                if existing.provider != lane.provider || existing.account != lane.account {
                    return Err(FleetError::InvalidState(format!(
                        "provider lane {} changed its durable identity",
                        lane.lane_id
                    )));
                }
                // Host credential availability is refreshed at startup. Quota
                // and cooldown observations remain authoritative across it.
                lane.quota_status = existing.quota_status;
                lane.cooldown_until_unix_ms = existing.cooldown_until_unix_ms;
            }
            snapshot
                .provider_lanes
                .insert(lane.lane_id.clone(), lane.clone());
            Ok(lane)
        })
    }

    pub fn update_provider_probe(
        &self,
        lane_id: &str,
        credential_status: ProviderCredentialStatus,
        quota_status: ProviderQuotaStatus,
        cooldown_until_unix_ms: Option<u64>,
        now_unix_ms: u64,
    ) -> FleetResult<ProviderLaneRecord> {
        let lane_id = lane_id.to_string();
        self.commit(move |snapshot| {
            let lane = snapshot
                .provider_lanes
                .get_mut(&lane_id)
                .ok_or_else(|| FleetError::not_found("provider lane", lane_id.clone()))?;
            lane.credential_status = credential_status;
            lane.quota_status = quota_status;
            lane.cooldown_until_unix_ms = cooldown_until_unix_ms;
            lane.updated_at_unix_ms = now_unix_ms;
            Ok(lane.clone())
        })
    }

    pub fn reconcile_provider_lanes(
        &self,
        configured_lane_ids: Vec<String>,
        now_unix_ms: u64,
    ) -> FleetResult<()> {
        self.commit(move |snapshot| {
            for lane in snapshot.provider_lanes.values_mut() {
                if !configured_lane_ids.contains(&lane.lane_id) {
                    lane.credential_status = ProviderCredentialStatus::Missing;
                    lane.updated_at_unix_ms = now_unix_ms;
                }
            }
            Ok(())
        })
    }

    pub fn reserve_provider(
        &self,
        task_id: &TaskId,
        preferred_account: Option<&str>,
        model: &str,
        now_unix_ms: u64,
    ) -> FleetResult<ProviderAllocationRecord> {
        let task_id = task_id.clone();
        let preferred_account = preferred_account.map(str::to_string);
        let model = model.to_string();
        self.commit(move |snapshot| {
            let task = snapshot
                .tasks
                .get(&task_id)
                .ok_or_else(|| FleetError::not_found("task", task_id.to_string()))?;
            if let Some(existing) = snapshot.provider_allocations.get(&task_id) {
                if existing.provider == task.provider
                    && existing.model == model
                    && existing.state == ProviderAllocationState::Active
                    && preferred_account
                        .as_ref()
                        .is_none_or(|account| existing.account.as_ref() == Some(account))
                {
                    return Ok(existing.clone());
                }
                return Err(FleetError::InvalidState(format!(
                    "task {task_id} already has a non-idempotent provider allocation"
                )));
            }

            let active_counts = snapshot
                .provider_allocations
                .values()
                .filter(|allocation| allocation.state == ProviderAllocationState::Active)
                .fold(
                    BTreeMap::<String, usize>::new(),
                    |mut counts, allocation| {
                        *counts.entry(allocation.lane_id.clone()).or_default() += 1;
                        counts
                    },
                );
            let mut candidates = snapshot
                .provider_lanes
                .values()
                .filter(|lane| lane.provider == task.provider)
                .filter(|lane| {
                    preferred_account
                        .as_ref()
                        .is_none_or(|account| lane.account.as_ref() == Some(account))
                })
                .filter(|lane| lane.credential_status == ProviderCredentialStatus::Available)
                .filter(|lane| {
                    lane.quota_status != ProviderQuotaStatus::Exhausted
                        || lane
                            .cooldown_until_unix_ms
                            .is_some_and(|until| until <= now_unix_ms)
                })
                .filter(|lane| {
                    lane.cooldown_until_unix_ms
                        .is_none_or(|until| until <= now_unix_ms)
                })
                .filter(|lane| {
                    active_counts.get(&lane.lane_id).copied().unwrap_or(0) < lane.max_concurrent
                })
                .cloned()
                .collect::<Vec<_>>();
            candidates.sort_by(|left, right| {
                right
                    .default_for_provider
                    .cmp(&left.default_for_provider)
                    .then_with(|| {
                        active_counts
                            .get(&left.lane_id)
                            .copied()
                            .unwrap_or(0)
                            .cmp(&active_counts.get(&right.lane_id).copied().unwrap_or(0))
                    })
                    .then_with(|| left.lane_id.cmp(&right.lane_id))
            });
            let lane = candidates.into_iter().next().ok_or_else(|| {
                FleetError::ProviderUnavailable(format!(
                    "no credentialed, quota-eligible {:?} account has capacity",
                    task.provider
                ))
            })?;
            let allocation = ProviderAllocationRecord {
                task_id: task_id.clone(),
                lane_id: lane.lane_id,
                provider: lane.provider,
                account: lane.account,
                model,
                state: ProviderAllocationState::Active,
                reserved_at_unix_ms: now_unix_ms,
                updated_at_unix_ms: now_unix_ms,
            };
            snapshot
                .provider_allocations
                .insert(task_id.clone(), allocation.clone());
            Ok(allocation)
        })
    }

    pub fn release_provider(
        &self,
        task_id: &TaskId,
        now_unix_ms: u64,
    ) -> FleetResult<Option<ProviderAllocationRecord>> {
        let task_id = task_id.clone();
        if let Some(existing) = self
            .snapshot
            .lock()
            .map_err(|_| FleetError::LockPoisoned)?
            .provider_allocations
            .get(&task_id)
            .cloned()
        {
            if existing.state == ProviderAllocationState::Released {
                return Ok(Some(existing));
            }
        } else {
            return Ok(None);
        }
        self.commit(move |snapshot| {
            let Some(allocation) = snapshot.provider_allocations.get_mut(&task_id) else {
                return Ok(None);
            };
            allocation.state = ProviderAllocationState::Released;
            allocation.updated_at_unix_ms = now_unix_ms;
            Ok(Some(allocation.clone()))
        })
    }

    pub fn observe_provider_runtime(
        &self,
        task_id: &TaskId,
        credential_status: Option<ProviderCredentialStatus>,
        quota_status: Option<ProviderQuotaStatus>,
        cooldown_until_unix_ms: Option<u64>,
        now_unix_ms: u64,
    ) -> FleetResult<Option<ProviderLaneRecord>> {
        let task_id = task_id.clone();
        self.commit(move |snapshot| {
            let Some(allocation) = snapshot.provider_allocations.get(&task_id) else {
                return Ok(None);
            };
            let lane = snapshot
                .provider_lanes
                .get_mut(&allocation.lane_id)
                .ok_or_else(|| {
                    FleetError::not_found("provider lane", allocation.lane_id.clone())
                })?;
            if let Some(status) = credential_status {
                lane.credential_status = status;
            }
            if let Some(status) = quota_status {
                lane.quota_status = status;
            }
            lane.cooldown_until_unix_ms = cooldown_until_unix_ms;
            lane.updated_at_unix_ms = now_unix_ms;
            Ok(Some(lane.clone()))
        })
    }

    pub fn mark_worktree_closing(
        &self,
        path: &str,
        owner: &TaskId,
        ownership_generation: u64,
        now_unix_ms: u64,
    ) -> FleetResult<WorktreeOwnershipRecord> {
        self.update_worktree_state(
            path,
            owner,
            ownership_generation,
            WorktreeOwnershipState::Closing,
            now_unix_ms,
        )
    }

    pub fn release_worktree(
        &self,
        path: &str,
        owner: &TaskId,
        ownership_generation: u64,
        now_unix_ms: u64,
    ) -> FleetResult<WorktreeOwnershipRecord> {
        self.update_worktree_state(
            path,
            owner,
            ownership_generation,
            WorktreeOwnershipState::Released,
            now_unix_ms,
        )
    }

    pub fn route_capability<C: CapabilityRouter>(
        &self,
        router: &C,
        worker_id: &WorkerId,
        connection_generation: u64,
        request: CapabilityRequest,
    ) -> FleetResult<CapabilityResponse> {
        let (route, session_id, _) =
            self.authorize_capability(worker_id, connection_generation, &request)?;
        router.route(route, worker_id, &session_id, request)
    }

    /// Perform the durable session and policy check before an async service
    /// host leaves the authority actor to call a remote capability service.
    pub fn authorize_capability(
        &self,
        worker_id: &WorkerId,
        connection_generation: u64,
        request: &CapabilityRequest,
    ) -> FleetResult<(CapabilityRoute, SessionId, CapabilityAuthorization)> {
        {
            let snapshot = self.snapshot.lock().map_err(|_| FleetError::LockPoisoned)?;
            let worker = snapshot
                .workers
                .get(worker_id)
                .ok_or_else(|| FleetError::not_found("worker", worker_id.to_string()))?;
            fence_worker(worker, connection_generation)?;
            if worker.state != WorkerAuthorityState::Active {
                return Err(FleetError::CapabilityUnauthorized(
                    "worker is not active".into(),
                ));
            }
            let task = snapshot.tasks.get(&worker.task_id).ok_or_else(|| {
                FleetError::CapabilityUnauthorized(
                    "worker task is missing from durable fleet authority".into(),
                )
            })?;
            if task.session_id != worker.session_id
                || task.worker_id.as_ref() != Some(&worker.worker_id)
            {
                return Err(FleetError::CapabilityUnauthorized(
                    "worker task binding differs from durable fleet authority".into(),
                ));
            }
            let attempt_id = task.attempt_id.clone().ok_or_else(|| {
                FleetError::CapabilityUnauthorized(
                    "worker task has no durable attempt identity".into(),
                )
            })?;
            let session = snapshot.sessions.get(&worker.session_id).ok_or_else(|| {
                FleetError::CapabilityUnauthorized(
                    "worker session is missing from durable fleet authority".into(),
                )
            })?;
            if session.current_task_id.as_ref() != Some(&worker.task_id)
                || session.task_ids.last() != Some(&worker.task_id)
            {
                return Err(FleetError::CapabilityUnauthorized(
                    "worker task is not the newest current session attempt".into(),
                ));
            }
            let session_attempt_generation = u64::try_from(session.task_ids.len())
                .map_err(|_| FleetError::CounterExhausted("session attempt generation"))?;
            if session_attempt_generation == 0 {
                return Err(FleetError::CapabilityUnauthorized(
                    "worker session has no durable attempts".into(),
                ));
            }
            let session_policy = worker.session_policy.as_ref().ok_or_else(|| {
                FleetError::CapabilityUnauthorized("worker has no admitted session policy".into())
            })?;
            if !session_policy
                .allowed_capabilities
                .contains(&request.capability)
            {
                return Err(FleetError::CapabilityUnauthorized(
                    request.capability.clone(),
                ));
            }
            let capability_policy = session_policy.capability_policy()?.ok_or_else(|| {
                FleetError::CapabilityUnauthorized(
                    "session has no fine-grained capability policy".into(),
                )
            })?;
            if !capability_policy.allows_operation(&request.capability, &request.operation) {
                return Err(FleetError::CapabilityUnauthorized(format!(
                    "{}/{}",
                    request.capability, request.operation
                )));
            }
            if request.capability == "atom" {
                let atom_ref = request
                    .bounded_payload
                    .get("atom")
                    .and_then(Value::as_str)
                    .map(bro_core::AtomRef::new)
                    .ok_or_else(|| {
                        FleetError::CapabilityUnauthorized(
                            "atom invocation omitted its exact atom ref".into(),
                        )
                    })?;
                if !capability_policy.allows_atom_ref(&atom_ref) {
                    return Err(FleetError::CapabilityUnauthorized(format!(
                        "atom ref {atom_ref}"
                    )));
                }
            }
            let policy = session_policy.feature_policy()?.ok_or_else(|| {
                FleetError::CapabilityUnauthorized(
                    "session capability policy has no durable identity".into(),
                )
            })?;
            let route = classify_capability(&request.capability)?;
            Ok((
                route,
                worker.session_id.clone(),
                CapabilityAuthorization {
                    worker_id: worker.worker_id.clone(),
                    session_id: worker.session_id.clone(),
                    task_id: worker.task_id.clone(),
                    attempt_id,
                    session_attempt_generation,
                    policy: policy.policy,
                    capability_policy,
                },
            ))
        }
    }

    fn update_worktree_state(
        &self,
        path: &str,
        owner: &TaskId,
        ownership_generation: u64,
        state: WorktreeOwnershipState,
        now_unix_ms: u64,
    ) -> FleetResult<WorktreeOwnershipRecord> {
        let path = path.to_string();
        let owner = owner.clone();
        self.commit(move |snapshot| {
            let updated = {
                let record = snapshot
                    .worktrees
                    .get_mut(&path)
                    .ok_or_else(|| FleetError::not_found("worktree", path.clone()))?;
                if record.task_id != owner {
                    return Err(FleetError::WorktreeOwnerMismatch {
                        path: path.clone(),
                        owner: owner.to_string(),
                    });
                }
                if record.ownership_generation != ownership_generation {
                    return Err(FleetError::StaleOwnershipGeneration {
                        expected: record.ownership_generation,
                        actual: ownership_generation,
                    });
                }
                let transition_allowed = record.state == state
                    || matches!(
                        (record.state, state),
                        (
                            WorktreeOwnershipState::Active,
                            WorktreeOwnershipState::Closing | WorktreeOwnershipState::Released
                        ) | (
                            WorktreeOwnershipState::Closing,
                            WorktreeOwnershipState::Released
                        )
                    );
                if !transition_allowed {
                    return Err(FleetError::InvalidState(format!(
                        "worktree {path} cannot transition from {:?} to {state:?}",
                        record.state
                    )));
                }
                record.state = state;
                record.updated_at_unix_ms = now_unix_ms;
                record.clone()
            };
            if state == WorktreeOwnershipState::Released {
                if let Some(task) = snapshot.tasks.get_mut(&updated.task_id)
                    && task.managed_worktree.as_deref() == Some(path.as_str())
                {
                    task.managed_worktree = None;
                    task.cwd = Some(updated.base_repo.clone());
                    task.last_event_at_unix_ms = task.last_event_at_unix_ms.max(now_unix_ms);
                }
                refresh_task_roster(snapshot, &updated.task_id)?;
            }
            Ok(updated)
        })
    }

    fn commit<T>(
        &self,
        mutation: impl FnOnce(&mut FleetSnapshot) -> FleetResult<T>,
    ) -> FleetResult<T> {
        let mut current = self.snapshot.lock().map_err(|_| FleetError::LockPoisoned)?;
        let expected_generation = current.generation;
        let mut next = current.clone();
        let result = mutation(&mut next)?;
        next.generation = next_counter(expected_generation, "snapshot generation")?;
        next.validate()?;
        self.repository
            .compare_and_swap(expected_generation, &next)?;
        *current = next;
        Ok(result)
    }
}

fn fence_worktree(
    record: &WorktreeOwnershipRecord,
    owner: &TaskId,
    ownership_generation: u64,
) -> FleetResult<()> {
    if &record.task_id != owner {
        return Err(FleetError::WorktreeOwnerMismatch {
            path: record.path.clone(),
            owner: owner.to_string(),
        });
    }
    if record.ownership_generation != ownership_generation {
        return Err(FleetError::StaleOwnershipGeneration {
            expected: record.ownership_generation,
            actual: ownership_generation,
        });
    }
    Ok(())
}

fn refresh_task_roster(snapshot: &mut FleetSnapshot, task_id: &TaskId) -> FleetResult<()> {
    let Some(task) = snapshot.tasks.get(task_id) else {
        if snapshot.roster.remove(task_id).is_some() {
            snapshot.roster_generation =
                next_counter(snapshot.roster_generation, "roster generation")?;
        }
        return Ok(());
    };
    let projected = project_task(task);
    if snapshot.roster.get(task_id) != Some(&projected) {
        snapshot.roster.insert(task_id.clone(), projected);
        snapshot.roster_generation = next_counter(snapshot.roster_generation, "roster generation")?;
    }
    Ok(())
}

const MAX_RECORD_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_RECORD_TEXT_CHARS: usize = 4 * 1024;

fn queue_attempt_record(snapshot: &mut FleetSnapshot, attempt: &AttemptRecord) -> FleetResult<()> {
    let state = attempt_state_name(attempt.state);
    let record_id = stable_record_id("fleetd:attempt", &format!("{}:{state}", attempt.attempt_id));
    let result = bound_json(attempt.result.clone(), MAX_RECORD_PAYLOAD_BYTES / 2)?;
    let payload = serde_json::json!({
        "attempt_id": bounded_text(&attempt.attempt_id.to_string(), 256),
        "operation_id": bounded_text(&attempt.operation_id.to_string(), 256),
        "task_id": bounded_text(&attempt.task_id.to_string(), 256),
        "session_id": bounded_text(&attempt.session_id.to_string(), 256),
        "provider": attempt.provider,
        "model": bounded_text(&attempt.model, MAX_RECORD_TEXT_CHARS),
        "service_tier": attempt.service_tier,
        "state": state,
        "accepted_at_unix_ms": attempt.accepted_at_unix_ms,
        "updated_at_unix_ms": attempt.updated_at_unix_ms,
        "result": result,
    });
    queue_record(
        snapshot,
        record_id,
        format!("attempt.{state}"),
        Some(bounded_text(&attempt.attempt_id.to_string(), 256)),
        BTreeMap::from([
            (
                "operation_id".into(),
                bounded_text(&attempt.operation_id.to_string(), 256),
            ),
            (
                "task_id".into(),
                bounded_text(&attempt.task_id.to_string(), 256),
            ),
            (
                "session_id".into(),
                bounded_text(&attempt.session_id.to_string(), 256),
            ),
        ]),
        payload,
    )
}

#[allow(clippy::too_many_arguments)]
fn queue_session_event_record(
    snapshot: &mut FleetSnapshot,
    worker_id: &WorkerId,
    task_id: &TaskId,
    session_id: &SessionId,
    event_seq: u64,
    occurred_at_unix_ms: u64,
    descriptor: &SessionEventDescriptor,
) -> FleetResult<()> {
    let record_id = stable_record_id("fleetd:event", &format!("{worker_id}:{event_seq}"));
    let transcript_path = snapshot
        .tasks
        .get(task_id)
        .and_then(|task| task.transcript_path.as_deref())
        .map(|path| bounded_text(path, MAX_RECORD_TEXT_CHARS));
    let payload = serde_json::json!({
        "task_id": bounded_text(&task_id.to_string(), 256),
        "session_id": bounded_text(&session_id.to_string(), 256),
        "worker_id": bounded_text(&worker_id.to_string(), 256),
        "event_seq": event_seq,
        "event_type": bounded_text(&descriptor.event_type, 128),
        "event_payload_bytes": descriptor.payload_bytes,
        "event_payload_sha256": descriptor.payload_sha256,
        "occurred_at_unix_ms": occurred_at_unix_ms,
        "transcript_path": transcript_path,
        "through_event_seq": event_seq,
    });
    queue_record(
        snapshot,
        record_id,
        "session.event_committed".into(),
        Some(bounded_text(&session_id.to_string(), 256)),
        BTreeMap::from([
            ("worker_id".into(), worker_id.to_string()),
            ("event_seq".into(), event_seq.to_string()),
            ("task_id".into(), bounded_text(&task_id.to_string(), 256)),
            (
                "session_id".into(),
                bounded_text(&session_id.to_string(), 256),
            ),
        ]),
        payload,
    )
}

fn queue_record(
    snapshot: &mut FleetSnapshot,
    record_id: String,
    kind: String,
    subject: Option<String>,
    attributes: BTreeMap<String, String>,
    payload: Value,
) -> FleetResult<()> {
    let payload = bound_json(payload, MAX_RECORD_PAYLOAD_BYTES)?;
    if let Some(existing) = snapshot
        .record_outbox
        .values()
        .find(|record| record.record_id == record_id)
    {
        if existing.kind == kind
            && existing.subject == subject
            && existing.attributes == attributes
            && existing.payload == payload
        {
            return Ok(());
        }
        return Err(FleetError::InvalidState(format!(
            "record ID {record_id} was reused with different content"
        )));
    }
    let cursor = snapshot.next_record_cursor;
    snapshot.next_record_cursor = next_counter(cursor, "record cursor")?;
    snapshot.record_outbox.insert(
        cursor,
        RecordEnvelope {
            record_id,
            producer: "fleetd".into(),
            cursor: cursor.to_string(),
            kind,
            occurred_at: None,
            subject,
            attributes,
            payload,
        },
    );
    Ok(())
}

fn stable_record_id(prefix: &str, coordinate: &str) -> String {
    let candidate = format!("{prefix}:{coordinate}");
    if candidate.len() <= 256 {
        candidate
    } else {
        format!(
            "{prefix}:sha256:{}",
            hex::encode(Sha256::digest(candidate.as_bytes()))
        )
    }
}

fn bound_json(value: Value, limit: usize) -> FleetResult<Value> {
    let encoded = serde_json::to_vec(&value)?;
    if encoded.len() <= limit {
        return Ok(value);
    }
    Ok(serde_json::json!({
        "truncated": true,
        "original_bytes": encoded.len(),
        "sha256": format!("sha256:{}", hex::encode(Sha256::digest(&encoded))),
    }))
}

fn bounded_text(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let bounded: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{bounded}...")
    } else {
        bounded
    }
}

fn attempt_state_name(state: AttemptState) -> &'static str {
    match state {
        AttemptState::Accepted => "accepted",
        AttemptState::Running => "running",
        AttemptState::Completed => "completed",
        AttemptState::Failed => "failed",
        AttemptState::Interrupted => "interrupted",
        AttemptState::Lost => "lost",
    }
}

fn fail_uncommitted_terminal_task(
    snapshot: &mut FleetSnapshot,
    task_id: &TaskId,
    reason: &str,
    now_unix_ms: u64,
) -> FleetResult<()> {
    let attempt_id = {
        let task = snapshot
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| FleetError::not_found("task", task_id.to_string()))?;
        if task.status.is_terminal() {
            return Ok(());
        }
        task.status = TaskStatus::Failed;
        task.error_teaser = Some(reason.to_string());
        task.recoverable = true;
        task.completed_at_unix_ms = Some(now_unix_ms);
        task.last_event_at_unix_ms = task.last_event_at_unix_ms.max(now_unix_ms);
        task.attempt_id.clone()
    };
    let mut transitioned_attempt = None;
    if let Some(attempt_id) = attempt_id
        && let Some(attempt) = snapshot.attempts.get_mut(&attempt_id)
    {
        attempt.state = AttemptState::Lost;
        attempt.result = serde_json::json!({"error": reason});
        attempt.updated_at_unix_ms = now_unix_ms;
        transitioned_attempt = Some(attempt.clone());
    }
    if let Some(attempt) = transitioned_attempt.as_ref() {
        queue_attempt_record(snapshot, attempt)?;
    }
    refresh_task_roster(snapshot, task_id)
}

fn next_counter(value: u64, name: &'static str) -> FleetResult<u64> {
    value
        .checked_add(1)
        .ok_or(FleetError::CounterExhausted(name))
}

fn validate_session_policy_transition(
    previous: Option<&SessionPolicy>,
    next: &SessionPolicy,
) -> FleetResult<()> {
    let Some(previous) = previous else {
        return Ok(());
    };
    if previous == next {
        return Ok(());
    }

    let previous_feature = previous.feature_policy().map_err(|error| {
        FleetError::HandshakeRejected(format!(
            "persisted session policy identity is malformed: {error}"
        ))
    })?;
    let next_feature = next.feature_policy().map_err(|error| {
        FleetError::HandshakeRejected(format!(
            "next session policy identity is malformed: {error}"
        ))
    })?;
    let Some(next_feature) = next_feature else {
        return Err(FleetError::HandshakeRejected(
            "versioned session policy update omitted its policy identity".into(),
        ));
    };
    if next_feature.policy.version == 0 || next_feature.policy.digest.trim().is_empty() {
        return Err(FleetError::HandshakeRejected(
            "session policy identity must have a nonzero version and digest".into(),
        ));
    }

    let Some(previous_feature) = previous_feature else {
        if next_feature.policy.version != 1 {
            return Err(FleetError::HandshakeRejected(
                "the first versioned session policy revision must be 1".into(),
            ));
        }
        return Ok(());
    };
    if previous_feature.enabled_features != next_feature.enabled_features {
        return Err(FleetError::HandshakeRejected(
            "required protocol features changed across reconnect".into(),
        ));
    }
    let expected_version = previous_feature
        .policy
        .version
        .checked_add(1)
        .ok_or(FleetError::CounterExhausted("session policy revision"))?;
    if next_feature.policy.version != expected_version {
        return Err(FleetError::HandshakeRejected(format!(
            "session policy update must advance revision {} to {expected_version}; got {}",
            previous_feature.policy.version, next_feature.policy.version
        )));
    }
    if next_feature.policy.digest == previous_feature.policy.digest {
        return Err(FleetError::HandshakeRejected(
            "session policy revision advanced without a digest change".into(),
        ));
    }
    Ok(())
}

fn fence_worker(worker: &WorkerAuthorityRecord, connection_generation: u64) -> FleetResult<()> {
    if worker.connection_generation != connection_generation {
        return Err(FleetError::StaleConnectionGeneration {
            expected: worker.connection_generation,
            actual: connection_generation,
        });
    }
    Ok(())
}

fn hash_secret(secret: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(secret.as_bytes())))
}

fn request_digest(request: &ExecutionRequest) -> FleetResult<String> {
    let value = serde_json::to_value(request)?;
    let canonical = canonical_json(value);
    let bytes = serde_json::to_vec(&canonical)?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

fn requested_capability_policy(
    tool_policy: &ExecutionToolPolicy,
) -> FleetResult<SessionCapabilityPolicy> {
    let allowed_operations = tool_policy
        .allowed_remote_operations
        .iter()
        .filter_map(|(capability, operations)| {
            let operations = operations.iter().cloned().collect::<BTreeSet<_>>();
            (!operations.is_empty()).then(|| (capability.clone(), operations))
        })
        .collect();
    let policy = SessionCapabilityPolicy {
        allowed_operations,
        allowed_atom_refs: tool_policy.allowed_atom_refs.iter().cloned().collect(),
    };
    policy.validate().map_err(FleetError::InvalidState)?;
    Ok(policy)
}

fn mailbox_delivery_digest(delivery: &AgentMailboxDelivery) -> FleetResult<String> {
    let value = serde_json::to_value(delivery)?;
    let canonical = canonical_json(value);
    let bytes = serde_json::to_vec(&canonical)?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

fn validate_mailbox_delivery(delivery: &AgentMailboxDelivery) -> FleetResult<()> {
    if delivery.delivery_id.trim().is_empty()
        || delivery.canonical_target.is_empty()
        || !delivery.canonical_target.starts_with('/')
        || delivery.canonical_target.ends_with('/')
        || delivery.through_sequence == 0
        || delivery.messages.is_empty()
        || delivery.messages.len() > 128
    {
        return Err(FleetError::InvalidState(
            "mailbox delivery identity or bounds are invalid".into(),
        ));
    }
    let mut expected = delivery
        .messages
        .first()
        .map(|message| message.sequence)
        .unwrap_or_default();
    let mut total_bytes = 0usize;
    for message in &delivery.messages {
        if message.sequence != expected
            || message.sequence == 0
            || message.recipient != delivery.target_agent_id
            || message.message_id.trim().is_empty()
            || message.body.trim().is_empty()
        {
            return Err(FleetError::InvalidState(
                "mailbox delivery contains an invalid or non-contiguous message".into(),
            ));
        }
        expected = next_counter(expected, "mailbox message sequence")?;
        total_bytes = total_bytes.saturating_add(message.body.len());
    }
    if delivery.messages.last().map(|message| message.sequence) != Some(delivery.through_sequence)
        || total_bytes > 1024 * 1024
    {
        return Err(FleetError::InvalidState(
            "mailbox delivery cursor or payload bound is invalid".into(),
        ));
    }
    Ok(())
}

fn mailbox_delivery_receipt(record: &MailboxDeliveryRecord) -> AgentMailboxDeliveryReceipt {
    AgentMailboxDeliveryReceipt {
        delivery_id: record.delivery_id.clone(),
        target_agent_id: record.target_agent_id.clone(),
        canonical_target: record.canonical_target.clone(),
        session_id: record.session_id.clone(),
        through_sequence: record.through_sequence,
        state: record.state,
        command_id: Some(record.command_id.clone()),
        error: record.last_error.clone(),
    }
}

fn agent_binding_from_labels(
    labels: &BTreeMap<String, String>,
) -> FleetResult<Option<AgentSessionBinding>> {
    match (
        labels.get("logical_agent_id"),
        labels.get("logical_agent_path"),
    ) {
        (None, None) => Ok(None),
        (Some(agent_id), Some(canonical_path))
            if !agent_id.trim().is_empty()
                && canonical_path.starts_with('/')
                && !canonical_path.ends_with('/') =>
        {
            Ok(Some(AgentSessionBinding {
                agent_id: AgentId::new(agent_id.clone()),
                canonical_path: canonical_path.clone(),
            }))
        }
        _ => Err(FleetError::InvalidState(
            "logical agent execution labels must contain one complete canonical binding".into(),
        )),
    }
}

fn canonical_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let sorted: BTreeMap<String, Value> = values
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        scalar => scalar,
    }
}

fn working_set_coordinates(intent: &WorkingSetIntent) -> (Option<String>, Option<String>) {
    match intent {
        WorkingSetIntent::Scratch => (None, None),
        WorkingSetIntent::Existing {
            cwd,
            managed_worktree,
        } => (Some(cwd.clone()), managed_worktree.then(|| cwd.clone())),
        WorkingSetIntent::CreateManagedWorktree { requested_path, .. } => {
            (requested_path.clone(), requested_path.clone())
        }
    }
}

fn parse_origin(value: &str) -> Origin {
    match value {
        "agentdispatch" | "agent_dispatch" => Origin::AgentDispatch,
        "cockpit" => Origin::Cockpit,
        "workflow" => Origin::Workflow,
        "atom" => Origin::Atom,
        "cron" => Origin::Cron,
        "webhook" => Origin::Webhook,
        _ => Origin::Unknown,
    }
}

fn validate_attempt_transition(from: AttemptState, to: AttemptState) -> FleetResult<()> {
    let valid = from == to
        || matches!(
            (from, to),
            (AttemptState::Accepted, AttemptState::Running)
                | (AttemptState::Accepted, AttemptState::Failed)
                | (AttemptState::Accepted, AttemptState::Interrupted)
                | (AttemptState::Accepted, AttemptState::Lost)
                | (AttemptState::Running, AttemptState::Completed)
                | (AttemptState::Running, AttemptState::Failed)
                | (AttemptState::Running, AttemptState::Interrupted)
                | (AttemptState::Running, AttemptState::Lost)
        );
    if valid {
        Ok(())
    } else {
        Err(FleetError::InvalidState(format!(
            "attempt transition from {from:?} to {to:?} is not allowed"
        )))
    }
}

fn validate_task_transition(from: TaskStatus, to: TaskStatus) -> FleetResult<()> {
    let valid = from == to
        || matches!(
            (from, to),
            (TaskStatus::Pending, TaskStatus::Running)
                | (TaskStatus::Pending, TaskStatus::Completed)
                | (TaskStatus::Pending, TaskStatus::Failed)
                | (TaskStatus::Pending, TaskStatus::Cancelled)
                | (TaskStatus::Running, TaskStatus::Completed)
                | (TaskStatus::Running, TaskStatus::Failed)
                | (TaskStatus::Running, TaskStatus::Cancelled)
        );
    if valid {
        Ok(())
    } else {
        Err(FleetError::InvalidState(format!(
            "task transition from {from:?} to {to:?} is not allowed"
        )))
    }
}

fn attempt_state_for_task(status: TaskStatus, current: AttemptState) -> AttemptState {
    match status {
        TaskStatus::Pending => AttemptState::Accepted,
        TaskStatus::Running => AttemptState::Running,
        TaskStatus::Completed => AttemptState::Completed,
        TaskStatus::Failed if current == AttemptState::Lost => AttemptState::Lost,
        TaskStatus::Failed => AttemptState::Failed,
        TaskStatus::Cancelled => AttemptState::Interrupted,
    }
}

fn task_status_for_attempt(state: AttemptState) -> TaskStatus {
    match state {
        AttemptState::Accepted => TaskStatus::Pending,
        AttemptState::Running => TaskStatus::Running,
        AttemptState::Completed => TaskStatus::Completed,
        AttemptState::Failed | AttemptState::Lost => TaskStatus::Failed,
        AttemptState::Interrupted => TaskStatus::Cancelled,
    }
}

fn classify_capability(capability: &str) -> FleetResult<CapabilityRoute> {
    if capability == "atom"
        || capability.starts_with("blackops.")
        || capability.starts_with("operational.")
    {
        Ok(CapabilityRoute::Operational)
    } else if matches!(capability, "corpus" | "refactor")
        || capability.starts_with("blackbox.")
        || capability.starts_with("corpus.")
    {
        Ok(CapabilityRoute::Corpus)
    } else {
        Err(FleetError::CapabilityUnauthorized(capability.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use bro_capabilities::{
        ExecutionCodeMode, ExecutionDispatchContext, ExecutionServiceTier, ExecutionToolPolicy,
        WorkingSetIntent,
    };
    use bro_core::{Capability, OperationId, Provider};
    use bro_protocol::{
        CapabilityResponse, FeaturePolicy, PolicyIdentity, WorkerCommandKind, WorkerFeature,
    };

    use super::*;
    use crate::model::ProviderDefaultsPolicy;
    use crate::persistence::MemoryFleetRepository;
    use crate::ports::FleetRepository;

    #[derive(Clone, Default)]
    struct TestIds(Arc<AtomicU64>);

    impl IdentityGenerator for TestIds {
        fn next_id(&self, prefix: &str) -> String {
            let ordinal = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            format!("{prefix}-{ordinal}")
        }
    }

    type TestAuthority = FleetAuthority<Arc<MemoryFleetRepository>, TestIds>;

    fn authority() -> (TestAuthority, Arc<MemoryFleetRepository>) {
        let repository = Arc::new(MemoryFleetRepository::default());
        let authority = FleetAuthority::open(repository.clone(), TestIds::default(), 1).unwrap();
        (authority, repository)
    }

    fn request(key: &str) -> ExecutionRequest {
        ExecutionRequest {
            operation_id: OperationId::new(format!("operation-{key}")),
            idempotency_key: key.to_string(),
            kind: ExecutionKind::Fresh {
                prompt: "secret prompt is hashed, not persisted".into(),
            },
            provider: Provider::Glm,
            model: "glm-test".into(),
            effort: Some("high".into()),
            service_tier: ExecutionServiceTier::Priority,
            code_mode: Some(ExecutionCodeMode::Only),
            dispatch_context: Some(ExecutionDispatchContext::default()),
            working_set: WorkingSetIntent::Scratch,
            shell_env: BTreeMap::from([("API_SECRET".into(), "do-not-persist".into())]),
            tool_policy: ExecutionToolPolicy::default(),
            system_prompt: Some("also hashed only".into()),
            output_schema: None,
            labels: BTreeMap::new(),
        }
    }

    fn hello(
        accepted: &ExecutionAccepted,
        provisioned: &ProvisionedWorker,
        proof: AuthenticationProof,
        last_event: u64,
        last_command: u64,
    ) -> WorkerHello {
        WorkerHello {
            protocol_versions: vec![WORKER_PROTOCOL_V1],
            worker_build: BuildIdentity {
                version: "1.0.0".into(),
                build_id: "worker-build".into(),
            },
            worker_id: provisioned.worker_id.clone(),
            task_id: accepted.task_id.clone(),
            session_id: accepted.session_id.clone(),
            bootstrap_or_resume_proof: proof,
            last_local_event_seq: last_event,
            last_fleet_command_seq: last_command,
            worker_capabilities: Vec::new(),
        }
    }

    fn connect(
        authority: &TestAuthority,
        key: &str,
        now: u64,
    ) -> (ExecutionAccepted, ProvisionedWorker, HandshakeAccepted) {
        let accepted = authority.admit_execution(request(key), now).unwrap();
        let provisioned = authority
            .provision_worker(&accepted.task_id, now + 1)
            .unwrap();
        let handshake = authority
            .accept_handshake(
                hello(
                    &accepted,
                    &provisioned,
                    provisioned.bootstrap_proof.clone(),
                    0,
                    0,
                ),
                versioned_policy(
                    1,
                    "sha256:test-policy",
                    &["corpus.search", "blackops.mailbox"],
                ),
                BuildIdentity {
                    version: "1.0.0".into(),
                    build_id: "fleet-build".into(),
                },
                LeasePolicy {
                    lease_duration_ms: 10,
                    heartbeat_interval_ms: 3,
                    reattach_grace_ms: 5,
                },
                now + 2,
            )
            .unwrap();
        (accepted, provisioned, handshake)
    }

    fn session_policy(capabilities: Vec<String>) -> SessionPolicy {
        let allowed_operations = capabilities
            .iter()
            .map(|capability| {
                let operation = match capability.as_str() {
                    "corpus.search" => "search",
                    "blackops.mailbox" => "deliver",
                    "atom" => "invoke_atom",
                    _ => "test",
                };
                (capability.clone(), BTreeSet::from([operation.to_string()]))
            })
            .collect();
        let mut policy = SessionPolicy {
            allowed_capabilities: capabilities,
            attributes: BTreeMap::new(),
        };
        policy
            .set_capability_policy(SessionCapabilityPolicy {
                allowed_operations,
                allowed_atom_refs: BTreeSet::new(),
            })
            .unwrap();
        policy
    }

    fn versioned_policy(version: u64, digest: &str, capabilities: &[&str]) -> SessionPolicy {
        let mut policy = session_policy(
            capabilities
                .iter()
                .map(|capability| (*capability).to_string())
                .collect(),
        );
        policy
            .set_feature_policy(FeaturePolicy {
                enabled_features: BTreeSet::from([WorkerFeature::new(
                    WorkerFeature::CAPABILITY_RPC,
                )]),
                policy: PolicyIdentity {
                    version,
                    digest: digest.to_string(),
                },
            })
            .unwrap();
        policy
    }

    fn mailbox_delivery(
        session_id: &SessionId,
        delivery_id: &str,
        agent_id: &str,
        path: &str,
        sequence: u64,
        wake: bool,
    ) -> AgentMailboxDelivery {
        let recipient = AgentId::new(agent_id);
        AgentMailboxDelivery {
            delivery_id: delivery_id.into(),
            target_agent_id: recipient.clone(),
            canonical_target: path.into(),
            session_id: session_id.clone(),
            through_sequence: sequence,
            wake,
            messages: vec![bro_protocol::AgentMailboxMessage {
                message_id: format!("message-{sequence}"),
                sequence,
                sender: Some(AgentId::new("agent-root")),
                recipient,
                kind: if wake {
                    bro_protocol::AgentMailboxMessageKind::Followup
                } else {
                    bro_protocol::AgentMailboxMessageKind::Send
                },
                body: format!("mailbox body {sequence}"),
                created_at_unix_ms: 100 + sequence,
            }],
        }
    }

    #[test]
    fn admission_is_durable_idempotent_and_secret_free() {
        let (authority, repository) = authority();
        let first = authority.admit_execution(request("same"), 10).unwrap();
        let duplicate = authority.admit_execution(request("same"), 11).unwrap();
        assert_eq!(first.attempt_id, duplicate.attempt_id);
        assert!(!first.deduplicated);
        assert!(duplicate.deduplicated);
        let snapshot = repository.load().unwrap().unwrap();
        assert_eq!(snapshot.attempts.len(), 1);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("do-not-persist"));
        assert!(!encoded.contains("secret prompt"));

        let mut conflict = request("same");
        conflict.model = "different".into();
        assert!(matches!(
            authority.admit_execution(conflict, 12),
            Err(FleetError::IdempotencyConflict)
        ));
    }

    #[test]
    fn admission_persists_remote_policy_and_resume_can_only_inherit_or_match() {
        let (authority, repository) = authority();
        let mut fresh = request("policy-fresh");
        fresh.tool_policy.allowed_remote_operations = BTreeMap::from([
            ("atom".into(), vec!["invoke_atom".into()]),
            ("blackops.agent".into(), vec!["spawn".into()]),
        ]);
        fresh.tool_policy.allowed_atom_refs = vec![bro_core::AtomRef::new("atom:review@v1")];
        let accepted = authority.admit_execution(fresh.clone(), 10).unwrap();
        let provisioned = authority.provision_worker(&accepted.task_id, 11).unwrap();
        let expected = SessionCapabilityPolicy {
            allowed_operations: BTreeMap::from([
                ("atom".into(), BTreeSet::from(["invoke_atom".into()])),
                ("blackops.agent".into(), BTreeSet::from(["spawn".into()])),
            ]),
            allowed_atom_refs: BTreeSet::from([bro_core::AtomRef::new("atom:review@v1")]),
        };
        let snapshot = repository.load().unwrap().unwrap();
        assert_eq!(
            snapshot.sessions[&accepted.session_id].requested_capability_policy,
            expected
        );
        assert_eq!(
            snapshot.workers[&provisioned.worker_id].requested_capability_policy,
            expected
        );

        let mut inherited_resume = request("policy-resume-inherit");
        inherited_resume.kind = ExecutionKind::Resume {
            session_id: accepted.session_id.clone(),
            prompt: "continue".into(),
        };
        inherited_resume.tool_policy = ExecutionToolPolicy::default();
        authority.admit_execution(inherited_resume, 12).unwrap();

        let mut matching_resume = request("policy-resume-match");
        matching_resume.kind = ExecutionKind::Resume {
            session_id: accepted.session_id.clone(),
            prompt: "continue exactly".into(),
        };
        matching_resume.tool_policy = fresh.tool_policy.clone();
        authority.admit_execution(matching_resume, 13).unwrap();

        let mut broader_resume = request("policy-resume-broaden");
        broader_resume.kind = ExecutionKind::Resume {
            session_id: accepted.session_id,
            prompt: "broaden".into(),
        };
        broader_resume.tool_policy = fresh.tool_policy;
        broader_resume
            .tool_policy
            .allowed_remote_operations
            .get_mut("blackops.agent")
            .unwrap()
            .push("interrupt".into());
        assert!(matches!(
            authority.admit_execution(broader_resume, 14),
            Err(FleetError::InvalidState(_))
        ));
    }

    #[test]
    fn idempotency_digest_covers_requested_remote_policy() {
        let (authority, _) = authority();
        let baseline = request("policy-digest");
        authority.admit_execution(baseline.clone(), 1).unwrap();

        let mut changed = baseline;
        changed.tool_policy.allowed_remote_operations =
            BTreeMap::from([("blackops.agent".into(), vec!["status".into()])]);
        assert!(matches!(
            authority.admit_execution(changed, 2),
            Err(FleetError::IdempotencyConflict)
        ));
    }

    #[test]
    fn mailbox_delivery_is_exactly_once_across_fleet_and_worker_restart() {
        let repository = Arc::new(MemoryFleetRepository::default());
        let identities = TestIds::default();
        let authority = FleetAuthority::open(repository.clone(), identities.clone(), 1).unwrap();
        let (accepted, _provisioned, handshake) = connect(&authority, "mailbox", 10);
        let delivery = mailbox_delivery(
            &accepted.session_id,
            "delivery-1",
            "agent-1",
            "/root/child",
            1,
            false,
        );
        let (worker_id, command, receipt) = authority
            .enqueue_mailbox_delivery(delivery.clone(), 20)
            .unwrap();
        let command = command.expect("new delivery command");
        assert_eq!(receipt.state, AgentMailboxDeliveryState::Pending);
        assert_eq!(command.command_seq, 1);
        drop(authority);

        let reopened = FleetAuthority::open(repository.clone(), identities.clone(), 21).unwrap();
        let (same_worker, replay, pending) = reopened
            .enqueue_mailbox_delivery(delivery.clone(), 22)
            .unwrap();
        assert_eq!(same_worker, worker_id);
        assert_eq!(
            replay.as_ref().map(|item| &item.command_id),
            Some(&command.command_id)
        );
        assert_eq!(pending.state, AgentMailboxDeliveryState::Pending);

        reopened
            .observe_command_outcome(
                &worker_id,
                handshake.welcome.connection_generation,
                CommandOutcome {
                    command_seq: command.command_seq,
                    command_id: command.command_id.clone(),
                    accepted: true,
                    terminal: true,
                    result_or_error: serde_json::json!({"admitted": true}),
                },
                23,
            )
            .unwrap();
        let (_, replay, admitted) = reopened
            .enqueue_mailbox_delivery(delivery.clone(), 24)
            .unwrap();
        assert!(replay.is_none());
        assert_eq!(admitted.state, AgentMailboxDeliveryState::Admitted);

        drop(reopened);
        let restarted = FleetAuthority::open(repository, identities, 25).unwrap();
        let (_, replay, admitted_after_restart) =
            restarted.enqueue_mailbox_delivery(delivery, 26).unwrap();
        assert!(replay.is_none());
        assert_eq!(
            admitted_after_restart.state,
            AgentMailboxDeliveryState::Admitted
        );
        let snapshot = restarted.snapshot().unwrap();
        assert_eq!(snapshot.mailbox_deliveries.len(), 1);
        assert!(snapshot.workers[&worker_id].pending_commands.is_empty());
    }

    #[test]
    fn mailbox_delivery_fails_closed_for_cross_agent_session_binding() {
        let (authority, _) = authority();
        let (accepted, _, _) = connect(&authority, "mailbox-scope", 10);
        authority
            .enqueue_mailbox_delivery(
                mailbox_delivery(
                    &accepted.session_id,
                    "delivery-a",
                    "agent-a",
                    "/root/a",
                    1,
                    false,
                ),
                20,
            )
            .unwrap();
        let error = authority
            .enqueue_mailbox_delivery(
                mailbox_delivery(
                    &accepted.session_id,
                    "delivery-b",
                    "agent-b",
                    "/other/tree",
                    2,
                    true,
                ),
                21,
            )
            .unwrap_err();
        assert!(matches!(error, FleetError::InvalidState(_)));

        let mut changed = mailbox_delivery(
            &accepted.session_id,
            "delivery-a",
            "agent-a",
            "/root/a",
            1,
            false,
        );
        changed.messages[0].body = "different payload".into();
        assert!(matches!(
            authority.enqueue_mailbox_delivery(changed, 22),
            Err(FleetError::IdempotencyConflict)
        ));
    }

    #[test]
    fn restart_marks_an_unlaunched_worker_lost_and_recoverable() {
        let repository = Arc::new(MemoryFleetRepository::default());
        let identities = TestIds::default();
        let authority = FleetAuthority::open(repository.clone(), identities.clone(), 1).unwrap();
        let execution = request("launch-window");
        let accepted = authority.admit_execution(execution.clone(), 2).unwrap();
        let initial = WorkerCommandKind::UserTurn {
            text: "original initial turn".into(),
        };
        let first = authority
            .provision_worker_with_initial_command(&accepted.task_id, initial.clone(), 3)
            .unwrap();
        let first_worker_id = first.worker.worker_id.clone();
        drop(authority);

        let reopened = FleetAuthority::open(repository, identities, 4).unwrap();
        let recovered = reopened.snapshot().unwrap();
        assert_eq!(
            recovered.workers[&first_worker_id].state,
            WorkerAuthorityState::Lost
        );
        assert_eq!(
            recovered.tasks[&accepted.task_id].worker_id.as_ref(),
            Some(&first_worker_id)
        );
        assert!(recovered.tasks[&accepted.task_id].recoverable);
        assert_eq!(
            recovered.attempts[&accepted.attempt_id].state,
            AttemptState::Lost
        );

        let duplicate = reopened.admit_execution(execution, 5).unwrap();
        assert!(duplicate.deduplicated);
        assert_eq!(duplicate.attempt_id, accepted.attempt_id);
        assert!(matches!(
            reopened.provision_worker_with_initial_command(&accepted.task_id, initial, 6),
            Err(FleetError::InvalidState(_))
        ));
    }

    #[test]
    fn concurrent_duplicate_admission_creates_one_attempt() {
        let (authority, _) = authority();
        let authority = Arc::new(authority);
        let left_authority = authority.clone();
        let left = std::thread::spawn(move || {
            left_authority
                .admit_execution(request("concurrent"), 10)
                .unwrap()
        });
        let right_authority = authority.clone();
        let right = std::thread::spawn(move || {
            right_authority
                .admit_execution(request("concurrent"), 10)
                .unwrap()
        });
        let left = left.join().unwrap();
        let right = right.join().unwrap();
        assert_eq!(left.attempt_id, right.attempt_id);
        assert_ne!(left.deduplicated, right.deduplicated);
        assert_eq!(authority.snapshot().unwrap().attempts.len(), 1);
    }

    #[test]
    fn repository_conflict_does_not_advance_local_state() {
        let repository = Arc::new(MemoryFleetRepository::default());
        let first = FleetAuthority::open(repository.clone(), TestIds::default(), 1).unwrap();
        let second = FleetAuthority::open(repository, TestIds::default(), 1).unwrap();
        first.admit_execution(request("first"), 10).unwrap();
        assert!(matches!(
            second.admit_execution(request("second"), 10),
            Err(FleetError::SnapshotGenerationConflict { .. })
        ));
        assert!(second.snapshot().unwrap().tasks.is_empty());
    }

    #[test]
    fn initialization_persists_an_import_and_counter_exhaustion_fails_closed() {
        let repository = Arc::new(MemoryFleetRepository::default());
        let initialized = FleetAuthority::initialize(
            repository.clone(),
            TestIds::default(),
            FleetSnapshot::default(),
            1,
        )
        .unwrap();
        assert_eq!(initialized.snapshot().unwrap().generation, 1);
        assert!(matches!(
            FleetAuthority::initialize(repository, TestIds::default(), FleetSnapshot::default(), 1),
            Err(FleetError::InvalidState(_))
        ));

        let exhausted_repository = Arc::new(MemoryFleetRepository::with_snapshot(FleetSnapshot {
            generation: u64::MAX,
            ..FleetSnapshot::default()
        }));
        let exhausted = FleetAuthority::open(exhausted_repository, TestIds::default(), 1).unwrap();
        assert!(matches!(
            exhausted.admit_execution(request("exhausted"), 2),
            Err(FleetError::CounterExhausted("snapshot generation"))
        ));
        assert!(exhausted.snapshot().unwrap().tasks.is_empty());
    }

    #[test]
    fn handshake_rotates_proofs_replays_commands_and_fences_generations() {
        let (authority, _) = authority();
        let (accepted, provisioned, first) = connect(&authority, "worker", 100);
        assert_eq!(first.welcome.connection_generation, 1);
        assert!(!format!("{provisioned:?}").contains("proof-"));
        let queued = authority
            .enqueue_command(
                &accepted.task_id,
                WorkerCommandKind::Steer {
                    text: "continue".into(),
                },
                103,
            )
            .unwrap();
        authority
            .mark_disconnected(&provisioned.worker_id, 1, 104)
            .unwrap();
        let second = authority
            .accept_handshake(
                hello(
                    &accepted,
                    &provisioned,
                    first.welcome.reconnect_proof.clone(),
                    0,
                    0,
                ),
                first.welcome.session_policy.clone(),
                first.welcome.fleet_build.clone(),
                LeasePolicy::default(),
                105,
            )
            .unwrap();
        assert_eq!(second.welcome.connection_generation, 2);
        assert_eq!(second.replay_commands, vec![queued]);
        assert!(matches!(
            authority.observe_event(&provisioned.worker_id, 1, 1, 106, None),
            Err(FleetError::StaleConnectionGeneration { .. })
        ));

        authority
            .mark_disconnected(&provisioned.worker_id, 2, 107)
            .unwrap();
        let recovered_lost_welcome = authority
            .accept_handshake(
                hello(&accepted, &provisioned, first.welcome.reconnect_proof, 0, 0),
                second.welcome.session_policy.clone(),
                second.welcome.fleet_build.clone(),
                LeasePolicy::default(),
                108,
            )
            .unwrap();
        assert_eq!(recovered_lost_welcome.welcome.connection_generation, 3);
    }

    #[test]
    fn reconnect_applies_monotonic_capability_revocation_and_restore() {
        let (authority, _) = authority();
        let accepted = authority
            .admit_execution(request("policy-revision"), 1)
            .unwrap();
        let provisioned = authority.provision_worker(&accepted.task_id, 2).unwrap();
        let first = authority
            .accept_handshake(
                hello(
                    &accepted,
                    &provisioned,
                    provisioned.bootstrap_proof.clone(),
                    0,
                    0,
                ),
                versioned_policy(1, "sha256:allow", &["corpus.search"]),
                BuildIdentity {
                    version: "1.0.0".into(),
                    build_id: "fleet-build".into(),
                },
                LeasePolicy::default(),
                3,
            )
            .unwrap();
        authority
            .mark_disconnected(&provisioned.worker_id, 1, 4)
            .unwrap();

        let revoked = authority
            .accept_handshake(
                hello(&accepted, &provisioned, first.welcome.reconnect_proof, 0, 0),
                versioned_policy(2, "sha256:revoke", &[]),
                first.welcome.fleet_build,
                LeasePolicy::default(),
                5,
            )
            .unwrap();
        assert_eq!(
            revoked.welcome.session_policy.allowed_capabilities,
            Vec::<String>::new()
        );
        assert_eq!(
            revoked
                .welcome
                .session_policy
                .feature_policy()
                .unwrap()
                .unwrap()
                .policy
                .version,
            2
        );
        assert!(matches!(
            authority.authorize_capability(
                &provisioned.worker_id,
                2,
                &CapabilityRequest {
                    call_id: "call-revoked".into(),
                    invocation_id: None,
                    capability: "corpus.search".into(),
                    operation: "search".into(),
                    bounded_payload: Value::Null,
                    deadline_unix_ms: None,
                }
            ),
            Err(FleetError::CapabilityUnauthorized(_))
        ));
        authority
            .mark_disconnected(&provisioned.worker_id, 2, 6)
            .unwrap();

        let restored = authority
            .accept_handshake(
                hello(
                    &accepted,
                    &provisioned,
                    revoked.welcome.reconnect_proof,
                    0,
                    0,
                ),
                versioned_policy(3, "sha256:restore", &["corpus.search"]),
                revoked.welcome.fleet_build,
                LeasePolicy::default(),
                7,
            )
            .unwrap();
        assert_eq!(restored.welcome.connection_generation, 3);
        assert_eq!(
            authority
                .authorize_capability(
                    &provisioned.worker_id,
                    3,
                    &CapabilityRequest {
                        call_id: "call-restored".into(),
                        invocation_id: None,
                        capability: "corpus.search".into(),
                        operation: "search".into(),
                        bounded_payload: Value::Null,
                        deadline_unix_ms: None,
                    }
                )
                .unwrap()
                .0,
            CapabilityRoute::Corpus
        );

        let prior = versioned_policy(3, "sha256:restore", &["corpus.search"]);
        assert!(
            validate_session_policy_transition(
                Some(&prior),
                &versioned_policy(3, "sha256:changed", &[])
            )
            .is_err()
        );
        assert!(
            validate_session_policy_transition(
                Some(&prior),
                &versioned_policy(5, "sha256:skipped", &[])
            )
            .is_err()
        );
    }

    #[test]
    fn initial_command_is_persisted_with_worker_provisioning() {
        let (authority, repository) = authority();
        let accepted = authority.admit_execution(request("initial"), 1).unwrap();
        let provisioned = authority
            .provision_worker_with_initial_command(
                &accepted.task_id,
                WorkerCommandKind::UserTurn {
                    text: "first turn".into(),
                },
                2,
            )
            .unwrap();
        assert_eq!(provisioned.command.command_seq, 1);
        let snapshot = repository.load().unwrap().unwrap();
        let worker = snapshot.workers.get(&provisioned.worker.worker_id).unwrap();
        assert_eq!(worker.next_command_seq, 2);
        assert_eq!(worker.pending_commands.get(&1), Some(&provisioned.command));
    }

    #[test]
    fn first_welcome_loss_preserves_bootstrap_until_connection_confirmation() {
        let (authority, _) = authority();
        let accepted = authority.admit_execution(request("welcome"), 1).unwrap();
        let provisioned = authority.provision_worker(&accepted.task_id, 2).unwrap();
        let first = authority
            .accept_handshake(
                hello(
                    &accepted,
                    &provisioned,
                    provisioned.bootstrap_proof.clone(),
                    0,
                    0,
                ),
                SessionPolicy {
                    allowed_capabilities: Vec::new(),
                    attributes: BTreeMap::new(),
                },
                BuildIdentity {
                    version: "1.0.0".into(),
                    build_id: "fleet".into(),
                },
                LeasePolicy::default(),
                3,
            )
            .unwrap();
        authority
            .mark_disconnected(&provisioned.worker_id, 1, 4)
            .unwrap();
        let second = authority
            .accept_handshake(
                hello(
                    &accepted,
                    &provisioned,
                    provisioned.bootstrap_proof.clone(),
                    0,
                    0,
                ),
                first.welcome.session_policy,
                first.welcome.fleet_build,
                LeasePolicy::default(),
                5,
            )
            .unwrap();
        assert_eq!(second.welcome.connection_generation, 2);
        authority
            .confirm_connection(&provisioned.worker_id, 2, 6)
            .unwrap();
        authority
            .mark_disconnected(&provisioned.worker_id, 2, 7)
            .unwrap();
        assert!(matches!(
            authority.accept_handshake(
                hello(
                    &accepted,
                    &provisioned,
                    provisioned.bootstrap_proof.clone(),
                    0,
                    0,
                ),
                second.welcome.session_policy,
                second.welcome.fleet_build,
                LeasePolicy::default(),
                8,
            ),
            Err(FleetError::HandshakeRejected(_))
        ));
    }

    #[test]
    fn event_ack_projection_and_command_outcome_ack_are_contiguous() {
        let (authority, _) = authority();
        let (accepted, provisioned, handshake) = connect(&authority, "streams", 10);
        let generation = handshake.welcome.connection_generation;
        let ack = authority
            .observe_event(
                &provisioned.worker_id,
                generation,
                1,
                13,
                Some(TaskEventProjection {
                    last_message: Some("durable update".into()),
                    turns: Some(1),
                    ..TaskEventProjection::default()
                }),
            )
            .unwrap();
        assert_eq!(ack.through_event_seq, 1);
        assert!(matches!(
            authority.observe_event(&provisioned.worker_id, generation, 3, 14, None),
            Err(FleetError::SequenceGap {
                expected: 2,
                actual: 3,
                ..
            })
        ));
        let duplicate = authority
            .observe_event(&provisioned.worker_id, generation, 1, 15, None)
            .unwrap();
        assert_eq!(duplicate.through_event_seq, 1);
        assert_eq!(
            authority
                .snapshot()
                .unwrap()
                .tasks
                .get(&accepted.task_id)
                .unwrap()
                .last_message
                .as_deref(),
            Some("durable update")
        );

        let first = authority
            .enqueue_command(&accepted.task_id, WorkerCommandKind::Interrupt, 16)
            .unwrap();
        let second = authority
            .enqueue_command(&accepted.task_id, WorkerCommandKind::Compact, 17)
            .unwrap();
        let second_ack = authority
            .observe_command_outcome(
                &provisioned.worker_id,
                generation,
                terminal_outcome(&second),
                18,
            )
            .unwrap();
        assert_eq!(second_ack.through_command_seq, 0);
        let first_ack = authority
            .observe_command_outcome(
                &provisioned.worker_id,
                generation,
                terminal_outcome(&first),
                19,
            )
            .unwrap();
        assert_eq!(first_ack.through_command_seq, 2);
        let duplicate_ack = authority
            .observe_command_outcome(
                &provisioned.worker_id,
                generation,
                terminal_outcome(&first),
                20,
            )
            .unwrap();
        assert_eq!(duplicate_ack.through_command_seq, 2);
    }

    #[test]
    fn record_outbox_tracks_lifecycle_and_bounded_transcript_coordinates() {
        let (authority, _) = authority();
        let (accepted, provisioned, handshake) = connect(&authority, "records", 10);
        authority
            .record_transcript_path(&accepted.task_id, "/tmp/session-events.jsonl", 12)
            .unwrap();
        let oversized_event = serde_json::json!({
            "type": "assistant",
            "private_payload": "x".repeat(2 * 1024 * 1024),
        });
        authority
            .observe_event_observation(
                &provisioned.worker_id,
                handshake.welcome.connection_generation,
                1,
                13,
                TaskEventObservation {
                    record: Some(SessionEventDescriptor::from_event(&oversized_event)),
                    ..TaskEventObservation::default()
                },
            )
            .unwrap();

        let snapshot = authority.snapshot().unwrap();
        let records: Vec<_> = snapshot.record_outbox.values().collect();
        assert_eq!(
            records
                .iter()
                .filter(|record| record.kind == "attempt.accepted")
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record.kind == "attempt.running")
                .count(),
            1
        );
        let event_record = records
            .iter()
            .find(|record| record.kind == "session.event_committed")
            .unwrap();
        let encoded = serde_json::to_vec(event_record).unwrap();
        assert!(encoded.len() < 80 * 1024);
        assert!(!String::from_utf8_lossy(&encoded).contains(&"x".repeat(1024)));
        assert_eq!(event_record.attributes.get("event_seq").unwrap(), "1");

        let through = *snapshot.record_outbox.keys().next_back().unwrap();
        assert_eq!(authority.acknowledge_records(through).unwrap(), 3);
        let acknowledged = authority.snapshot().unwrap();
        assert!(acknowledged.record_outbox.is_empty());
        assert_eq!(acknowledged.acknowledged_record_cursor, through);
        assert_eq!(
            acknowledged
                .workers
                .get(&provisioned.worker_id)
                .unwrap()
                .last_indexed_event_seq,
            1
        );
        assert_eq!(
            acknowledged.tasks[&accepted.task_id]
                .transcript_cursor
                .as_ref()
                .and_then(|cursor| cursor.get("event_seq"))
                .and_then(Value::as_u64),
            Some(1)
        );

        authority
            .observe_event_observation(
                &provisioned.worker_id,
                handshake.welcome.connection_generation,
                1,
                14,
                TaskEventObservation {
                    record: Some(SessionEventDescriptor::from_event(&oversized_event)),
                    ..TaskEventObservation::default()
                },
            )
            .unwrap();
        assert!(authority.snapshot().unwrap().record_outbox.is_empty());
    }

    #[test]
    fn oversized_attempt_result_is_replaced_by_a_stable_digest() {
        let (authority, _) = authority();
        let accepted = authority.admit_execution(request("bounded"), 10).unwrap();
        authority
            .transition_attempt(
                &accepted.attempt_id,
                AttemptState::Failed,
                serde_json::json!({"detail": "z".repeat(2 * 1024 * 1024)}),
                11,
            )
            .unwrap();
        let snapshot = authority.snapshot().unwrap();
        let failed = snapshot
            .record_outbox
            .values()
            .find(|record| record.kind == "attempt.failed")
            .unwrap();
        let encoded = serde_json::to_vec(failed).unwrap();
        assert!(encoded.len() < 80 * 1024);
        assert_eq!(failed.payload["result"]["truncated"], true);
        assert!(
            failed.payload["result"]["sha256"]
                .as_str()
                .is_some_and(|value| value.starts_with("sha256:"))
        );
    }

    #[test]
    fn terminal_projection_waits_for_snapshot_commit_marker() {
        let (authority, _) = authority();
        let (accepted, provisioned, handshake) = connect(&authority, "terminal-gate", 10);
        let generation = handshake.welcome.connection_generation;
        authority
            .observe_event_observation(
                &provisioned.worker_id,
                generation,
                1,
                13,
                TaskEventObservation {
                    projection: Some(TaskEventProjection {
                        turns: Some(2),
                        ..TaskEventProjection::default()
                    }),
                    defer_terminal: Some(TaskEventProjection {
                        status: Some(TaskStatus::Completed),
                        completed_at_unix_ms: Some(13),
                        ..TaskEventProjection::default()
                    }),
                    commit_deferred_terminal: false,
                    record: None,
                },
            )
            .unwrap();
        let staged = authority.snapshot().unwrap();
        assert_eq!(
            staged.tasks.get(&accepted.task_id).unwrap().status,
            TaskStatus::Running
        );
        assert_eq!(
            staged.attempts.get(&accepted.attempt_id).unwrap().state,
            AttemptState::Running
        );
        assert!(
            staged
                .workers
                .get(&provisioned.worker_id)
                .unwrap()
                .pending_terminal_projection
                .is_some()
        );

        authority
            .observe_event_observation(
                &provisioned.worker_id,
                generation,
                2,
                14,
                TaskEventObservation {
                    commit_deferred_terminal: true,
                    ..TaskEventObservation::default()
                },
            )
            .unwrap();
        let committed = authority.snapshot().unwrap();
        assert_eq!(
            committed.tasks.get(&accepted.task_id).unwrap().status,
            TaskStatus::Completed
        );
        assert_eq!(
            committed.attempts.get(&accepted.attempt_id).unwrap().state,
            AttemptState::Completed
        );
    }

    #[test]
    fn cancellation_intent_does_not_predeclare_a_terminal_outcome() {
        let (authority, _) = authority();
        let (accepted, _, _) = connect(&authority, "cancel-intent", 10);
        authority
            .enqueue_command(
                &accepted.task_id,
                WorkerCommandKind::Shutdown {
                    mode: bro_protocol::ShutdownMode::Graceful,
                    deadline_unix_ms: Some(100),
                    reason: Some("operator request".into()),
                },
                20,
            )
            .unwrap();
        let snapshot = authority.snapshot().unwrap();
        let task = snapshot.tasks.get(&accepted.task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.cancellation_requested_at_unix_ms, Some(20));
    }

    #[test]
    fn lease_expiry_marks_worker_attempt_task_and_roster_together() {
        let (authority, _) = authority();
        let (accepted, provisioned, handshake) = connect(&authority, "lease", 0);
        authority
            .mark_disconnected(
                &provisioned.worker_id,
                handshake.welcome.connection_generation,
                3,
            )
            .unwrap();
        assert!(authority.expire_stale(16).unwrap().is_empty());
        let expired = authority.expire_stale(18).unwrap();
        assert_eq!(expired, vec![provisioned.worker_id]);
        let snapshot = authority.snapshot().unwrap();
        assert_eq!(
            snapshot.tasks.get(&accepted.task_id).unwrap().status,
            TaskStatus::Failed
        );
        assert_eq!(
            snapshot.roster.get(&accepted.task_id).unwrap().status,
            TaskStatus::Failed
        );
        assert_eq!(
            snapshot.attempts.get(&accepted.attempt_id).unwrap().state,
            AttemptState::Lost
        );
    }

    #[test]
    fn startup_preserves_lease_and_changes_active_worker_to_reattach() {
        let (authority, repository) = authority();
        let (_, provisioned, _) = connect(&authority, "restart", 10);
        drop(authority);
        let recovered = FleetAuthority::open(repository, TestIds::default(), 20).unwrap();
        let worker = recovered
            .snapshot()
            .unwrap()
            .workers
            .get(&provisioned.worker_id)
            .unwrap()
            .clone();
        assert_eq!(worker.state, WorkerAuthorityState::AwaitingReattach);
        assert!(worker.lease.is_some());
    }

    #[test]
    fn worktree_ownership_rejects_conflict_and_stale_cleanup() {
        let (authority, _) = authority();
        let first = authority.admit_execution(request("first-tree"), 1).unwrap();
        let second = authority
            .admit_execution(request("second-tree"), 2)
            .unwrap();
        let claim = authority
            .claim_worktree(
                "/tmp/fleet-tree",
                "/tmp/base",
                &first.task_id,
                Some(first.attempt_id),
                vec!["target".into()],
                3,
            )
            .unwrap();
        assert!(matches!(
            authority.claim_worktree(
                "/tmp/fleet-tree",
                "/tmp/base",
                &second.task_id,
                Some(second.attempt_id.clone()),
                Vec::new(),
                4
            ),
            Err(FleetError::WorktreeConflict { .. })
        ));
        assert!(matches!(
            authority.release_worktree(
                "/tmp/fleet-tree",
                &first.task_id,
                claim.ownership_generation + 1,
                5
            ),
            Err(FleetError::StaleOwnershipGeneration { .. })
        ));
        authority
            .mark_worktree_closing(
                "/tmp/fleet-tree",
                &first.task_id,
                claim.ownership_generation,
                6,
            )
            .unwrap();
        authority
            .release_worktree(
                "/tmp/fleet-tree",
                &first.task_id,
                claim.ownership_generation,
                7,
            )
            .unwrap();
        authority
            .release_worktree(
                "/tmp/fleet-tree",
                &first.task_id,
                claim.ownership_generation,
                8,
            )
            .unwrap();
        let released = authority.snapshot().unwrap();
        let released_task = released.tasks.get(&first.task_id).unwrap();
        assert_eq!(released_task.managed_worktree, None);
        assert_eq!(released_task.cwd.as_deref(), Some("/tmp/base"));
        assert_eq!(released.roster[&first.task_id].managed_worktree, None);
        assert_eq!(
            released.roster[&first.task_id].cwd.as_deref(),
            Some("/tmp/base")
        );
        let reclaimed = authority
            .claim_worktree(
                "/tmp/fleet-tree",
                "/tmp/base",
                &second.task_id,
                Some(second.attempt_id),
                Vec::new(),
                9,
            )
            .unwrap();
        assert_eq!(
            reclaimed.ownership_generation,
            claim.ownership_generation + 1
        );
    }

    #[test]
    fn provider_capacity_cooldown_and_probe_state_survive_restart() {
        let (authority, repository) = authority();
        authority
            .configure_provider_lane(ProviderLaneRecord {
                lane_id: "glm:primary".into(),
                provider: Provider::Glm,
                account: Some("primary".into()),
                max_concurrent: 1,
                credential_status: ProviderCredentialStatus::Available,
                quota_status: ProviderQuotaStatus::Available,
                cooldown_until_unix_ms: None,
                default_for_provider: true,
                updated_at_unix_ms: 1,
            })
            .unwrap();
        let first = authority.admit_execution(request("provider-1"), 2).unwrap();
        let second = authority.admit_execution(request("provider-2"), 3).unwrap();
        let reserved = authority
            .reserve_provider(&first.task_id, Some("primary"), "glm-test", 4)
            .unwrap();
        assert_eq!(reserved.account.as_deref(), Some("primary"));
        assert!(matches!(
            authority.reserve_provider(&second.task_id, None, "glm-test", 4),
            Err(FleetError::ProviderUnavailable(_))
        ));
        drop(authority);

        let recovered = FleetAuthority::open(repository, TestIds::default(), 5).unwrap();
        assert!(matches!(
            recovered.reserve_provider(&second.task_id, None, "glm-test", 5),
            Err(FleetError::ProviderUnavailable(_))
        ));
        recovered.release_provider(&first.task_id, 6).unwrap();
        recovered
            .update_provider_probe(
                "glm:primary",
                ProviderCredentialStatus::Available,
                ProviderQuotaStatus::Available,
                Some(10),
                7,
            )
            .unwrap();
        assert!(matches!(
            recovered.reserve_provider(&second.task_id, None, "glm-test", 9),
            Err(FleetError::ProviderUnavailable(_))
        ));
        recovered
            .reserve_provider(&second.task_id, None, "glm-test", 10)
            .unwrap();
    }

    #[test]
    fn worktree_claim_is_distinct_from_materialization() {
        let (authority, _) = authority();
        let accepted = authority
            .admit_execution(request("materialization"), 1)
            .unwrap();
        let claim = authority
            .claim_worktree(
                "/tmp/materialized-fence",
                "/tmp/base",
                &accepted.task_id,
                Some(accepted.attempt_id),
                Vec::new(),
                2,
            )
            .unwrap();
        assert!(!claim.materialized);
        let ready = authority
            .mark_worktree_materialized(&claim.path, &claim.task_id, claim.ownership_generation, 3)
            .unwrap();
        assert!(ready.materialized);
    }

    #[test]
    fn runtime_lease_preserves_resolved_resume_allocation() {
        let (authority, _) = authority();
        let accepted = authority.admit_execution(request("runtime"), 1).unwrap();
        let lease = RuntimeLeaseRecord {
            task_id: accepted.task_id,
            session_id: accepted.session_id.clone(),
            provider: Provider::Glm,
            account: Some("account-a".into()),
            model: Some("glm-test".into()),
            effort: Some("high".into()),
            tier: Some("terra".into()),
            durable: true,
            capabilities: vec![Capability::Resume],
            project_dir: Some("/tmp/project".into()),
            cwd: Some("/tmp/project".into()),
            selection_trace_id: "trace-1".into(),
            created_at_unix_ms: 2,
            last_seen_at_unix_ms: 3,
            provider_defaults: Some(ProviderDefaultsPolicy::StrictSuppress),
        };
        authority.record_runtime_lease(lease.clone()).unwrap();
        assert_eq!(
            authority
                .latest_durable_runtime_lease(&accepted.session_id)
                .unwrap(),
            Some(lease)
        );
    }

    struct RecordingRouter;

    impl CapabilityRouter for RecordingRouter {
        fn route(
            &self,
            route: CapabilityRoute,
            _worker_id: &WorkerId,
            _session_id: &SessionId,
            request: CapabilityRequest,
        ) -> FleetResult<CapabilityResponse> {
            Ok(CapabilityResponse::success(
                request.call_id,
                serde_json::json!({"route": route}),
            ))
        }
    }

    #[test]
    fn capability_routing_is_session_scoped_and_fail_closed() {
        let (authority, _) = authority();
        let (_, provisioned, handshake) = connect(&authority, "capability", 10);
        let response = authority
            .route_capability(
                &RecordingRouter,
                &provisioned.worker_id,
                handshake.welcome.connection_generation,
                CapabilityRequest {
                    call_id: "call-1".into(),
                    invocation_id: None,
                    capability: "corpus.search".into(),
                    operation: "search".into(),
                    bounded_payload: Value::Null,
                    deadline_unix_ms: None,
                },
            )
            .unwrap();
        assert!(!response.is_error);
        assert!(matches!(
            authority.route_capability(
                &RecordingRouter,
                &provisioned.worker_id,
                handshake.welcome.connection_generation,
                CapabilityRequest {
                    call_id: "call-wrong-operation".into(),
                    invocation_id: None,
                    capability: "corpus.search".into(),
                    operation: "delete".into(),
                    bounded_payload: Value::Null,
                    deadline_unix_ms: None,
                }
            ),
            Err(FleetError::CapabilityUnauthorized(_))
        ));
        assert!(matches!(
            authority.route_capability(
                &RecordingRouter,
                &provisioned.worker_id,
                handshake.welcome.connection_generation,
                CapabilityRequest {
                    call_id: "call-2".into(),
                    invocation_id: None,
                    capability: "filesystem.delete".into(),
                    operation: "delete".into(),
                    bounded_payload: Value::Null,
                    deadline_unix_ms: None,
                }
            ),
            Err(FleetError::CapabilityUnauthorized(_))
        ));
    }

    #[test]
    fn capability_authorization_uses_the_current_durable_session_attempt_generation() {
        let (authority, _) = authority();
        let (first, first_worker, first_handshake) = connect(&authority, "capability-first", 10);
        let capability_request = CapabilityRequest {
            call_id: "call-current-attempt".into(),
            invocation_id: None,
            capability: "corpus.search".into(),
            operation: "search".into(),
            bounded_payload: Value::Null,
            deadline_unix_ms: None,
        };
        let (_, _, first_authorization) = authority
            .authorize_capability(
                &first_worker.worker_id,
                first_handshake.welcome.connection_generation,
                &capability_request,
            )
            .unwrap();
        assert_eq!(first_authorization.task_id, first.task_id);
        assert_eq!(first_authorization.attempt_id, first.attempt_id);
        assert_eq!(first_authorization.session_attempt_generation, 1);

        let mut resume = request("capability-second");
        resume.kind = ExecutionKind::Resume {
            session_id: first.session_id.clone(),
            prompt: "continue the durable session".into(),
        };
        let second = authority.admit_execution(resume, 20).unwrap();
        let second_worker = authority.provision_worker(&second.task_id, 21).unwrap();
        let second_handshake = authority
            .accept_handshake(
                hello(
                    &second,
                    &second_worker,
                    second_worker.bootstrap_proof.clone(),
                    0,
                    0,
                ),
                versioned_policy(
                    1,
                    "sha256:test-policy",
                    &["corpus.search", "blackops.mailbox"],
                ),
                BuildIdentity {
                    version: "1.0.0".into(),
                    build_id: "fleet-build".into(),
                },
                LeasePolicy::default(),
                22,
            )
            .unwrap();

        assert!(matches!(
            authority.authorize_capability(
                &first_worker.worker_id,
                first_handshake.welcome.connection_generation,
                &capability_request,
            ),
            Err(FleetError::CapabilityUnauthorized(_))
        ));
        let (_, _, second_authorization) = authority
            .authorize_capability(
                &second_worker.worker_id,
                second_handshake.welcome.connection_generation,
                &capability_request,
            )
            .unwrap();
        assert_eq!(second_authorization.session_id, first.session_id);
        assert_eq!(second_authorization.task_id, second.task_id);
        assert_eq!(second_authorization.attempt_id, second.attempt_id);
        assert_eq!(second_authorization.session_attempt_generation, 2);
    }

    #[test]
    fn atom_capability_requires_the_exact_versioned_ref() {
        let (authority, _) = authority();
        let accepted = authority
            .admit_execution(request("atom-policy"), 1)
            .unwrap();
        let provisioned = authority.provision_worker(&accepted.task_id, 2).unwrap();
        let allowed_ref = bro_core::AtomRef::new("atom:allowed@v1");
        let mut policy = versioned_policy(1, "sha256:atom-policy", &["atom"]);
        policy
            .set_capability_policy(SessionCapabilityPolicy {
                allowed_operations: BTreeMap::from([(
                    "atom".into(),
                    BTreeSet::from(["invoke_atom".into()]),
                )]),
                allowed_atom_refs: BTreeSet::from([allowed_ref.clone()]),
            })
            .unwrap();
        let handshake = authority
            .accept_handshake(
                hello(
                    &accepted,
                    &provisioned,
                    provisioned.bootstrap_proof.clone(),
                    0,
                    0,
                ),
                policy,
                BuildIdentity {
                    version: "1.0.0".into(),
                    build_id: "fleet-build".into(),
                },
                LeasePolicy::default(),
                3,
            )
            .unwrap();

        authority
            .authorize_capability(
                &provisioned.worker_id,
                handshake.welcome.connection_generation,
                &CapabilityRequest {
                    call_id: "call-allowed-atom".into(),
                    invocation_id: None,
                    capability: "atom".into(),
                    operation: "invoke_atom".into(),
                    bounded_payload: serde_json::json!({"atom": allowed_ref}),
                    deadline_unix_ms: None,
                },
            )
            .unwrap();
        assert!(matches!(
            authority.authorize_capability(
                &provisioned.worker_id,
                handshake.welcome.connection_generation,
                &CapabilityRequest {
                    call_id: "call-wrong-atom".into(),
                    invocation_id: None,
                    capability: "atom".into(),
                    operation: "invoke_atom".into(),
                    bounded_payload: serde_json::json!({"atom": "atom:other@v1"}),
                    deadline_unix_ms: None,
                },
            ),
            Err(FleetError::CapabilityUnauthorized(_))
        ));
    }

    fn terminal_outcome(command: &WorkerCommand) -> CommandOutcome {
        CommandOutcome {
            command_seq: command.command_seq,
            command_id: command.command_id.clone(),
            accepted: true,
            terminal: true,
            result_or_error: serde_json::json!({"ok": true}),
        }
    }
}
