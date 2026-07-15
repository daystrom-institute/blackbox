use std::collections::BTreeMap;

use bro_capabilities::{AttemptState, ExecutionAccepted, ExecutionServiceTier, RecordEnvelope};
use bro_core::{
    AgentId, AttemptId, Capability, CommandId, OperationId, Origin, Provider, SessionId, TaskId,
    WorkerId,
};
use bro_protocol::{
    AgentMailboxDeliveryState, BroReportV1, BuildIdentity, CommandOutcome, LeaseGrant,
    RosterSummaryV1, SessionCapabilityPolicy, SessionPolicy, TaskStatus, WorkerCommand,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;

use crate::error::{FleetError, FleetResult};

pub const CURRENT_SNAPSHOT_FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetSnapshot {
    pub format_version: u32,
    /// Global compare-and-swap fence for all durable mutations.
    pub generation: u64,
    /// Monotonic roster sequence. It changes only when a roster row changes.
    pub roster_generation: u64,
    /// Next one-based cursor in the immutable fleet record producer stream.
    #[serde(default = "default_next_record_cursor")]
    pub next_record_cursor: u64,
    /// Highest contiguous producer cursor acknowledged by blackboxd.
    #[serde(default)]
    pub acknowledged_record_cursor: u64,
    #[serde(default)]
    pub tasks: BTreeMap<TaskId, TaskRecord>,
    #[serde(default)]
    pub sessions: BTreeMap<SessionId, SessionRecord>,
    #[serde(default)]
    pub attempts: BTreeMap<AttemptId, AttemptRecord>,
    #[serde(default)]
    pub admissions: BTreeMap<String, AdmissionRecord>,
    #[serde(default)]
    pub runtime_leases: BTreeMap<TaskId, RuntimeLeaseRecord>,
    #[serde(default)]
    pub workers: BTreeMap<WorkerId, WorkerAuthorityRecord>,
    #[serde(default)]
    pub mailbox_deliveries: BTreeMap<String, MailboxDeliveryRecord>,
    #[serde(default)]
    pub worktrees: BTreeMap<String, WorktreeOwnershipRecord>,
    #[serde(default)]
    pub provider_lanes: BTreeMap<String, ProviderLaneRecord>,
    #[serde(default)]
    pub provider_allocations: BTreeMap<TaskId, ProviderAllocationRecord>,
    #[serde(default)]
    pub roster: BTreeMap<TaskId, RosterSummaryV1>,
    /// Unacknowledged immutable records, keyed by their numeric producer cursor.
    #[serde(default)]
    pub record_outbox: BTreeMap<u64, RecordEnvelope>,
}

impl Default for FleetSnapshot {
    fn default() -> Self {
        Self {
            format_version: CURRENT_SNAPSHOT_FORMAT,
            generation: 0,
            roster_generation: 0,
            next_record_cursor: default_next_record_cursor(),
            acknowledged_record_cursor: 0,
            tasks: BTreeMap::new(),
            sessions: BTreeMap::new(),
            attempts: BTreeMap::new(),
            admissions: BTreeMap::new(),
            runtime_leases: BTreeMap::new(),
            workers: BTreeMap::new(),
            mailbox_deliveries: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            provider_lanes: BTreeMap::new(),
            provider_allocations: BTreeMap::new(),
            roster: BTreeMap::new(),
            record_outbox: BTreeMap::new(),
        }
    }
}

const fn default_next_record_cursor() -> u64 {
    1
}

impl FleetSnapshot {
    pub fn validate(&self) -> FleetResult<()> {
        if self.format_version != CURRENT_SNAPSHOT_FORMAT {
            return Err(FleetError::UnsupportedSnapshotFormat {
                found: self.format_version,
                expected: CURRENT_SNAPSHOT_FORMAT,
            });
        }
        if self.next_record_cursor == 0
            || self.acknowledged_record_cursor >= self.next_record_cursor
        {
            return Err(FleetError::InvalidState(
                "fleet record cursors are incoherent".into(),
            ));
        }
        let mut expected_cursor = self
            .acknowledged_record_cursor
            .checked_add(1)
            .ok_or(FleetError::CounterExhausted("acknowledged record cursor"))?;
        let mut record_ids = std::collections::BTreeSet::new();
        for (cursor, record) in &self.record_outbox {
            if *cursor != expected_cursor
                || record.cursor != cursor.to_string()
                || record.producer != "fleetd"
                || !record_ids.insert(record.record_id.as_str())
            {
                return Err(FleetError::InvalidState(format!(
                    "fleet record outbox entry at cursor {cursor} is inconsistent"
                )));
            }
            expected_cursor = expected_cursor
                .checked_add(1)
                .ok_or(FleetError::CounterExhausted("record cursor"))?;
        }
        if expected_cursor != self.next_record_cursor {
            return Err(FleetError::InvalidState(
                "fleet record outbox has a cursor gap".into(),
            ));
        }
        for (key, task) in &self.tasks {
            if key != &task.task_id {
                return Err(FleetError::InvalidState(format!(
                    "task map key {key} differs from record {}",
                    task.task_id
                )));
            }
            if !self.sessions.contains_key(&task.session_id) {
                return Err(FleetError::InvalidState(format!(
                    "task {key} references missing session {}",
                    task.session_id
                )));
            }
            if let Some(attempt_id) = &task.attempt_id {
                let attempt = self.attempts.get(attempt_id).ok_or_else(|| {
                    FleetError::InvalidState(format!(
                        "task {key} references missing attempt {attempt_id}"
                    ))
                })?;
                if attempt.task_id != *key || attempt.session_id != task.session_id {
                    return Err(FleetError::InvalidState(format!(
                        "task {key} and attempt {attempt_id} disagree on authority identity"
                    )));
                }
                if task.status != status_for_attempt(attempt.state) {
                    return Err(FleetError::InvalidState(format!(
                        "task {key} status and attempt {attempt_id} state disagree"
                    )));
                }
            }
            if let Some(worker_id) = &task.worker_id {
                let worker = self.workers.get(worker_id).ok_or_else(|| {
                    FleetError::InvalidState(format!(
                        "task {key} references missing worker {worker_id}"
                    ))
                })?;
                if worker.task_id != *key || worker.session_id != task.session_id {
                    return Err(FleetError::InvalidState(format!(
                        "task {key} and worker {worker_id} disagree on authority identity"
                    )));
                }
            }
        }
        for (key, session) in &self.sessions {
            if key != &session.session_id {
                return Err(FleetError::InvalidState(format!(
                    "session map key {key} differs from record {}",
                    session.session_id
                )));
            }
            let mut session_tasks = std::collections::BTreeSet::new();
            for task_id in &session.task_ids {
                let Some(task) = self.tasks.get(task_id) else {
                    return Err(FleetError::InvalidState(format!(
                        "session {key} references missing task {task_id}"
                    )));
                };
                if task.session_id != *key || task.provider != session.provider {
                    return Err(FleetError::InvalidState(format!(
                        "session {key} and task {task_id} disagree on identity or provider"
                    )));
                }
                if !session_tasks.insert(task_id) {
                    return Err(FleetError::InvalidState(format!(
                        "session {key} contains duplicate task {task_id}"
                    )));
                }
            }
            if let Some(current_task_id) = &session.current_task_id
                && !session.task_ids.contains(current_task_id)
            {
                return Err(FleetError::InvalidState(format!(
                    "session {key} current task {current_task_id} is not a session member"
                )));
            }
            session
                .requested_capability_policy
                .validate()
                .map_err(|error| {
                    FleetError::InvalidState(format!(
                        "session {key} has invalid requested capability policy: {error}"
                    ))
                })?;
        }
        for (key, attempt) in &self.attempts {
            if key != &attempt.attempt_id {
                return Err(FleetError::InvalidState(format!(
                    "attempt map key {key} differs from record {}",
                    attempt.attempt_id
                )));
            }
            if !self.tasks.contains_key(&attempt.task_id)
                || !self.sessions.contains_key(&attempt.session_id)
            {
                return Err(FleetError::InvalidState(format!(
                    "attempt {key} references missing task or session"
                )));
            }
        }
        let mut live_worker_tasks = std::collections::BTreeSet::new();
        for (key, worker) in &self.workers {
            if key != &worker.worker_id {
                return Err(FleetError::InvalidState(format!(
                    "worker map key {key} differs from record {}",
                    worker.worker_id
                )));
            }
            if !self.tasks.contains_key(&worker.task_id)
                || !self.sessions.contains_key(&worker.session_id)
            {
                return Err(FleetError::InvalidState(format!(
                    "worker {key} references missing task or session"
                )));
            }
            let session = &self.sessions[&worker.session_id];
            if worker.requested_capability_policy != session.requested_capability_policy {
                return Err(FleetError::InvalidState(format!(
                    "worker {key} capability request differs from session {}",
                    worker.session_id
                )));
            }
            validate_worker_record(worker)?;
            if !worker.state.is_terminal() && !live_worker_tasks.insert(worker.task_id.clone()) {
                return Err(FleetError::InvalidState(format!(
                    "multiple nonterminal workers own task {}",
                    worker.task_id
                )));
            }
        }
        for (delivery_id, delivery) in &self.mailbox_deliveries {
            if delivery_id != &delivery.delivery_id
                || delivery.delivery_id.is_empty()
                || delivery.request_digest.is_empty()
                || delivery.canonical_target.is_empty()
                || delivery.through_sequence == 0
            {
                return Err(FleetError::InvalidState(format!(
                    "mailbox delivery {delivery_id} has invalid identity"
                )));
            }
            let session = self.sessions.get(&delivery.session_id).ok_or_else(|| {
                FleetError::InvalidState(format!(
                    "mailbox delivery {delivery_id} references missing session {}",
                    delivery.session_id
                ))
            })?;
            let binding = AgentSessionBinding {
                agent_id: delivery.target_agent_id.clone(),
                canonical_path: delivery.canonical_target.clone(),
            };
            if session.agent_binding.as_ref() != Some(&binding) {
                return Err(FleetError::InvalidState(format!(
                    "mailbox delivery {delivery_id} differs from its session binding"
                )));
            }
            let worker = self.workers.get(&delivery.worker_id).ok_or_else(|| {
                FleetError::InvalidState(format!(
                    "mailbox delivery {delivery_id} references missing worker {}",
                    delivery.worker_id
                ))
            })?;
            if worker.session_id != delivery.session_id {
                return Err(FleetError::InvalidState(format!(
                    "mailbox delivery {delivery_id} worker belongs to another session"
                )));
            }
            if delivery.state == AgentMailboxDeliveryState::Pending
                && worker
                    .pending_commands
                    .get(&delivery.command_seq)
                    .is_none_or(|command| command.command_id != delivery.command_id)
            {
                return Err(FleetError::InvalidState(format!(
                    "pending mailbox delivery {delivery_id} has no matching worker command"
                )));
            }
        }
        for (key, admission) in &self.admissions {
            if key != &admission.idempotency_key {
                return Err(FleetError::InvalidState(format!(
                    "admission map key differs from record {}",
                    admission.idempotency_key
                )));
            }
            if !self.attempts.contains_key(&admission.accepted.attempt_id) {
                return Err(FleetError::InvalidState(format!(
                    "admission {key} references missing attempt {}",
                    admission.accepted.attempt_id
                )));
            }
            let attempt = &self.attempts[&admission.accepted.attempt_id];
            if admission.accepted.operation_id != attempt.operation_id
                || admission.accepted.task_id != attempt.task_id
                || admission.accepted.session_id != attempt.session_id
                || admission.request_digest != attempt.request_digest
            {
                return Err(FleetError::InvalidState(format!(
                    "admission {key} disagrees with its accepted attempt"
                )));
            }
        }
        for (key, lease) in &self.runtime_leases {
            if key != &lease.task_id {
                return Err(FleetError::InvalidState(format!(
                    "runtime lease map key {key} differs from record {}",
                    lease.task_id
                )));
            }
            let session = self.sessions.get(&lease.session_id).ok_or_else(|| {
                FleetError::InvalidState(format!(
                    "runtime lease {key} references missing session {}",
                    lease.session_id
                ))
            })?;
            if session.provider != lease.provider {
                return Err(FleetError::InvalidState(format!(
                    "runtime lease {key} provider differs from its session"
                )));
            }
            if let Some(task) = self.tasks.get(key)
                && (task.session_id != lease.session_id || task.provider != lease.provider)
            {
                return Err(FleetError::InvalidState(format!(
                    "runtime lease {key} disagrees with its task"
                )));
            }
        }
        if self.roster.len() != self.tasks.len() {
            return Err(FleetError::InvalidState(
                "roster membership does not match task membership".into(),
            ));
        }
        for (task_id, summary) in &self.roster {
            if task_id != &summary.task_id || !self.tasks.contains_key(task_id) {
                return Err(FleetError::InvalidState(format!(
                    "roster row {task_id} is not backed by the matching task"
                )));
            }
            if summary != &crate::roster::project_task(&self.tasks[task_id]) {
                return Err(FleetError::InvalidState(format!(
                    "roster row {task_id} is stale"
                )));
            }
        }
        for (path, worktree) in &self.worktrees {
            if path != &worktree.path || worktree.ownership_generation == 0 {
                return Err(FleetError::InvalidState(format!(
                    "worktree ownership record {path} has an invalid key or generation"
                )));
            }
            if !self.tasks.contains_key(&worktree.task_id) {
                return Err(FleetError::InvalidState(format!(
                    "worktree {path} references missing task {}",
                    worktree.task_id
                )));
            }
        }
        for (key, lane) in &self.provider_lanes {
            if key != &lane.lane_id || lane.max_concurrent == 0 {
                return Err(FleetError::InvalidState(format!(
                    "provider lane {key} has an invalid identity or concurrency limit"
                )));
            }
        }
        let mut active_by_lane = BTreeMap::<&str, usize>::new();
        for (task_id, allocation) in &self.provider_allocations {
            if task_id != &allocation.task_id || !self.tasks.contains_key(task_id) {
                return Err(FleetError::InvalidState(format!(
                    "provider allocation {task_id} is not backed by its task"
                )));
            }
            let lane = self
                .provider_lanes
                .get(&allocation.lane_id)
                .ok_or_else(|| {
                    FleetError::InvalidState(format!(
                        "provider allocation {task_id} references missing lane {}",
                        allocation.lane_id
                    ))
                })?;
            if lane.provider != allocation.provider || lane.account != allocation.account {
                return Err(FleetError::InvalidState(format!(
                    "provider allocation {task_id} disagrees with lane {}",
                    allocation.lane_id
                )));
            }
            if allocation.state == ProviderAllocationState::Active {
                *active_by_lane.entry(&allocation.lane_id).or_default() += 1;
            }
        }
        for (lane_id, count) in active_by_lane {
            let lane = &self.provider_lanes[lane_id];
            if count > lane.max_concurrent {
                return Err(FleetError::InvalidState(format!(
                    "provider lane {lane_id} has {count} active allocations above its limit {}",
                    lane.max_concurrent
                )));
            }
        }
        Ok(())
    }
}

fn status_for_attempt(state: AttemptState) -> TaskStatus {
    match state {
        AttemptState::Accepted => TaskStatus::Pending,
        AttemptState::Running => TaskStatus::Running,
        AttemptState::Completed => TaskStatus::Completed,
        AttemptState::Failed | AttemptState::Lost => TaskStatus::Failed,
        AttemptState::Interrupted => TaskStatus::Cancelled,
    }
}

fn validate_worker_record(worker: &WorkerAuthorityRecord) -> FleetResult<()> {
    if worker.last_indexed_event_seq > worker.event_ack {
        return Err(FleetError::InvalidState(format!(
            "worker {} indexed event cursor exceeds its durable event acknowledgement",
            worker.worker_id
        )));
    }
    if worker.next_command_seq == 0 || worker.command_outcome_ack >= worker.next_command_seq {
        return Err(FleetError::InvalidState(format!(
            "worker {} has incoherent command sequence bounds",
            worker.worker_id
        )));
    }
    for sequence in worker.command_outcome_ack + 1..worker.next_command_seq {
        let command = worker.pending_commands.get(&sequence).ok_or_else(|| {
            FleetError::InvalidState(format!(
                "worker {} is missing pending command {sequence}",
                worker.worker_id
            ))
        })?;
        if command.command_seq != sequence {
            return Err(FleetError::InvalidState(format!(
                "worker {} pending command key differs from its sequence",
                worker.worker_id
            )));
        }
    }
    for (sequence, command) in &worker.pending_commands {
        if *sequence <= worker.command_outcome_ack
            || *sequence >= worker.next_command_seq
            || command.command_seq != *sequence
        {
            return Err(FleetError::InvalidState(format!(
                "worker {} has a pending command outside its unacknowledged range",
                worker.worker_id
            )));
        }
    }
    for (sequence, outcome) in &worker.pending_command_outcomes {
        let command = worker.pending_commands.get(sequence).ok_or_else(|| {
            FleetError::InvalidState(format!(
                "worker {} outcome has no pending command",
                worker.worker_id
            ))
        })?;
        if outcome.command_seq != *sequence || outcome.command_id != command.command_id {
            return Err(FleetError::InvalidState(format!(
                "worker {} command outcome identity mismatch",
                worker.worker_id
            )));
        }
    }
    if let Some(projection) = &worker.pending_terminal_projection
        && !projection.status.is_some_and(|status| status.is_terminal())
    {
        return Err(FleetError::InvalidState(format!(
            "worker {} has a nonterminal deferred projection",
            worker.worker_id
        )));
    }
    for hash in [
        &worker.bootstrap_proof_hash,
        &worker.reconnect_proof_hash,
        &worker.previous_reconnect_proof_hash,
        &worker.pending_reconnect_proof_hash,
    ]
    .into_iter()
    .flatten()
    {
        let Some(raw) = hash.strip_prefix("sha256:") else {
            return Err(FleetError::InvalidState(format!(
                "worker {} proof verifier is not a tagged digest",
                worker.worker_id
            )));
        };
        if raw.len() != 64 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(FleetError::InvalidState(format!(
                "worker {} proof verifier is malformed",
                worker.worker_id
            )));
        }
    }
    match worker.state {
        WorkerAuthorityState::AwaitingInitialConnection => {
            if worker.connection_generation != 0 || worker.bootstrap_proof_hash.is_none() {
                return Err(FleetError::InvalidState(format!(
                    "worker {} has invalid initial-connection authority",
                    worker.worker_id
                )));
            }
        }
        WorkerAuthorityState::Active | WorkerAuthorityState::Draining => {
            if worker.connection_generation == 0
                || worker.lease.is_none()
                || (worker.reconnect_proof_hash.is_none()
                    && worker.bootstrap_proof_hash.is_none()
                    && worker.pending_reconnect_proof_hash.is_none())
            {
                return Err(FleetError::InvalidState(format!(
                    "worker {} has invalid live authority",
                    worker.worker_id
                )));
            }
        }
        WorkerAuthorityState::AwaitingReattach => {
            if worker.connection_generation == 0
                || (worker.reconnect_proof_hash.is_none()
                    && worker.bootstrap_proof_hash.is_none()
                    && worker.pending_reconnect_proof_hash.is_none())
            {
                return Err(FleetError::InvalidState(format!(
                    "worker {} has invalid reconnect authority",
                    worker.worker_id
                )));
            }
        }
        WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost => {}
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: TaskId,
    pub session_id: SessionId,
    #[serde(default)]
    pub attempt_id: Option<AttemptId>,
    #[serde(default)]
    pub worker_id: Option<WorkerId>,
    pub status: TaskStatus,
    pub provider: Provider,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub turns: Option<u64>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub managed_worktree: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub transcript_cursor: Option<Value>,
    #[serde(default)]
    pub last_message: Option<String>,
    #[serde(default)]
    pub report: Option<BroReportV1>,
    #[serde(default)]
    pub error_teaser: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub agent_label: Option<String>,
    #[serde(default)]
    pub origin: Origin,
    #[serde(default)]
    pub workflow_owned: bool,
    #[serde(default)]
    pub interrupted: bool,
    /// Durable cancellation intent is distinct from a terminal interruption.
    /// The task remains live until the worker reports a committed outcome.
    #[serde(default)]
    pub cancellation_requested_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub recoverable: bool,
    pub started_at_unix_ms: u64,
    pub last_event_at_unix_ms: u64,
    #[serde(default)]
    pub completed_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: SessionId,
    pub provider: Provider,
    #[serde(default)]
    pub task_ids: Vec<TaskId>,
    #[serde(default)]
    pub current_task_id: Option<TaskId>,
    #[serde(default)]
    pub agent_binding: Option<AgentSessionBinding>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// Immutable remote authority requested when this durable session was
    /// admitted. Resumes must present the same request; fleetd may only narrow
    /// it against host policy when provisioning a worker.
    #[serde(default)]
    pub requested_capability_policy: SessionCapabilityPolicy,
    pub resumable: bool,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

/// Immutable routing identity supplied by blackopsd. fleetd owns only this
/// session binding, never the logical agent graph behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionBinding {
    pub agent_id: AgentId,
    pub canonical_path: String,
}

/// Retained idempotency and admission receipt for one mailbox delivery.
/// Message bodies remain in blackopsd and, until admission, the worker command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxDeliveryRecord {
    pub delivery_id: String,
    pub request_digest: String,
    pub target_agent_id: AgentId,
    pub canonical_target: String,
    pub session_id: SessionId,
    pub through_sequence: u64,
    pub worker_id: WorkerId,
    pub command_seq: u64,
    pub command_id: CommandId,
    pub state: AgentMailboxDeliveryState,
    #[serde(default)]
    pub last_error: Option<String>,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub attempt_id: AttemptId,
    pub operation_id: OperationId,
    pub task_id: TaskId,
    pub session_id: SessionId,
    pub provider: Provider,
    pub model: String,
    pub service_tier: ExecutionServiceTier,
    pub request_digest: String,
    pub state: AttemptState,
    #[serde(default)]
    pub result: Value,
    pub accepted_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdmissionRecord {
    pub idempotency_key: String,
    pub request_digest: String,
    pub accepted: ExecutionAccepted,
    pub accepted_at_unix_ms: u64,
}

/// Resolved execution allocation retained independently from task TTL so a
/// durable provider session can be resumed after its old roster row expires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeLeaseRecord {
    pub task_id: TaskId,
    pub session_id: SessionId,
    pub provider: Provider,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub durable: bool,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub project_dir: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    pub selection_trace_id: String,
    pub created_at_unix_ms: u64,
    pub last_seen_at_unix_ms: u64,
    #[serde(default)]
    pub provider_defaults: Option<ProviderDefaultsPolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDefaultsPolicy {
    Default,
    SuppressWhenSupported,
    StrictSuppress,
    ExplicitOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerAuthorityState {
    AwaitingInitialConnection,
    AwaitingReattach,
    Active,
    Draining,
    Terminal,
    Lost,
}

impl WorkerAuthorityState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal | Self::Lost)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerAuthorityRecord {
    pub worker_id: WorkerId,
    pub task_id: TaskId,
    pub session_id: SessionId,
    #[serde(default)]
    pub worker_build: Option<BuildIdentity>,
    #[serde(default)]
    pub protocol_version: Option<u16>,
    pub connection_generation: u64,
    #[serde(default)]
    pub bootstrap_proof_hash: Option<String>,
    #[serde(default)]
    pub reconnect_proof_hash: Option<String>,
    #[serde(default)]
    pub previous_reconnect_proof_hash: Option<String>,
    /// Verifier for the credential carried by the most recently written
    /// welcome. It is promoted only after a post-handshake frame proves that
    /// the worker received and durably installed that welcome.
    #[serde(default)]
    pub pending_reconnect_proof_hash: Option<String>,
    #[serde(default)]
    pub connection_confirmed: bool,
    #[serde(default)]
    pub lease: Option<LeaseGrant>,
    #[serde(default)]
    pub session_policy: Option<SessionPolicy>,
    /// Session request copied durably at provisioning so reconnect policy can
    /// be rebuilt without retaining the secret-bearing execution request.
    #[serde(default)]
    pub requested_capability_policy: SessionCapabilityPolicy,
    pub event_ack: u64,
    /// Highest worker event sequence covered by an acknowledged corpus record.
    #[serde(default)]
    pub last_indexed_event_seq: u64,
    pub next_command_seq: u64,
    pub command_outcome_ack: u64,
    #[serde(default)]
    pub pending_commands: BTreeMap<u64, WorkerCommand>,
    #[serde(default)]
    pub pending_command_outcomes: BTreeMap<u64, CommandOutcome>,
    /// A terminal provider result waits here until the worker reports that its
    /// session snapshot is durable. The task and attempt stay nonterminal in
    /// the meantime.
    #[serde(default)]
    pub pending_terminal_projection: Option<TaskEventProjection>,
    pub state: WorkerAuthorityState,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeOwnershipState {
    Active,
    Closing,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeOwnershipRecord {
    pub path: String,
    pub base_repo: String,
    pub task_id: TaskId,
    #[serde(default)]
    pub attempt_id: Option<AttemptId>,
    pub ownership_generation: u64,
    pub state: WorktreeOwnershipState,
    #[serde(default)]
    pub seed_dirs: Vec<String>,
    /// The durable claim is written before any filesystem mutation. This bit is
    /// set only after `git worktree add` has been verified against `base_repo`.
    #[serde(default)]
    pub materialized: bool,
    pub claimed_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCredentialStatus {
    Available,
    Missing,
    Expired,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderQuotaStatus {
    Available,
    Exhausted,
    ProbeFailed,
    #[default]
    Unknown,
}

/// Secret-free provider/account health and capacity. Credentials are resolved
/// through the host port only after this lane has been durably reserved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLaneRecord {
    pub lane_id: String,
    pub provider: Provider,
    #[serde(default)]
    pub account: Option<String>,
    pub max_concurrent: usize,
    #[serde(default)]
    pub credential_status: ProviderCredentialStatus,
    #[serde(default)]
    pub quota_status: ProviderQuotaStatus,
    #[serde(default)]
    pub cooldown_until_unix_ms: Option<u64>,
    #[serde(default)]
    pub default_for_provider: bool,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAllocationState {
    Active,
    Released,
}

/// Durable, secret-free provider capacity reservation. Keeping it in the
/// authority snapshot prevents a fleetd restart from forgetting in-flight
/// account quota.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAllocationRecord {
    pub task_id: TaskId,
    pub lane_id: String,
    pub provider: Provider,
    #[serde(default)]
    pub account: Option<String>,
    pub model: String,
    pub state: ProviderAllocationState,
    pub reserved_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskEventProjection {
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub last_message: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub turns: Option<u64>,
    #[serde(default)]
    pub report: Option<BroReportV1>,
    #[serde(default)]
    pub error_teaser: Option<String>,
    #[serde(default)]
    pub transcript_cursor: Option<Value>,
    #[serde(default)]
    pub completed_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub interrupted: Option<bool>,
    #[serde(default)]
    pub recoverable: Option<bool>,
}

/// One durably acknowledged worker event can update the live projection,
/// stage a terminal projection, or publish a previously staged terminal
/// projection after the session snapshot commit marker.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskEventObservation {
    #[serde(default)]
    pub projection: Option<TaskEventProjection>,
    #[serde(default)]
    pub defer_terminal: Option<TaskEventProjection>,
    #[serde(default)]
    pub commit_deferred_terminal: bool,
    /// Bounded metadata for the immutable worker event record. Full event
    /// payloads remain in the harness transcript and are addressed by sequence.
    #[serde(default)]
    pub record: Option<SessionEventDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventDescriptor {
    pub event_type: String,
    pub payload_bytes: u64,
    pub payload_sha256: String,
}

impl SessionEventDescriptor {
    pub fn from_event(event: &Value) -> Self {
        let encoded = event.to_string();
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .chars()
            .take(128)
            .collect();
        let payload_sha256 = format!(
            "sha256:{}",
            hex::encode(sha2::Sha256::digest(encoded.as_bytes()))
        );
        Self {
            event_type,
            payload_bytes: encoded.len() as u64,
            payload_sha256,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityRoute {
    Operational,
    Corpus,
}

impl TaskRecord {
    pub(crate) fn apply_projection(
        &mut self,
        projection: TaskEventProjection,
        occurred_at_unix_ms: u64,
    ) {
        if let Some(value) = projection.status {
            self.status = value;
        }
        if projection.last_message.is_some() {
            self.last_message = projection.last_message;
        }
        if projection.model.is_some() {
            self.model = projection.model;
        }
        if projection.cost.is_some() {
            self.cost = projection.cost;
        }
        if projection.turns.is_some() {
            self.turns = projection.turns;
        }
        if projection.report.is_some() {
            self.report = projection.report;
        }
        if projection.error_teaser.is_some() {
            self.error_teaser = projection.error_teaser;
        }
        if projection.transcript_cursor.is_some() {
            self.transcript_cursor = projection.transcript_cursor;
        }
        if projection.completed_at_unix_ms.is_some() {
            self.completed_at_unix_ms = projection.completed_at_unix_ms;
        }
        if self.status.is_terminal() && self.completed_at_unix_ms.is_none() {
            self.completed_at_unix_ms = Some(occurred_at_unix_ms);
        }
        if let Some(value) = projection.interrupted {
            self.interrupted = value;
        }
        if let Some(value) = projection.recoverable {
            self.recoverable = value;
        }
        self.last_event_at_unix_ms = self.last_event_at_unix_ms.max(occurred_at_unix_ms);
    }
}
