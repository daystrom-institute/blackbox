//! Temporary blackbox-hosted worker identity, lease, and replay authority.
//!
//! The public surface is deliberately independent of `SharedState`. P5 moves
//! this state machine into fleet-core without changing the worker contract.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bro_core::{CommandId, SessionId, TaskId, WorkerId};
use bro_protocol::{
    AuthenticationProof, BuildIdentity, CommandOutcome, DrainBoundary, LeaseGrant, SessionPolicy,
    WorkerCommand, WorkerCommandKind, WorkerHello, WorkerLifecycleState, WorkerStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const REGISTRY_FORMAT_VERSION: u16 = 1;
const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 5_000;
const DEFAULT_LEASE_DURATION_MS: u64 = 20_000;
const DEFAULT_REATTACH_GRACE_MS: u64 = 30_000;
const HANDSHAKE_INSTALL_GRACE_MS: u64 = 2_000;
const MAX_PENDING_NORMAL_COMMANDS: usize = 128;
const MAX_PENDING_CONTROL_COMMANDS: usize = 32;
const MAX_PENDING_NORMAL_COMMAND_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING_CONTROL_COMMAND_BYTES: usize = 512 * 1024;
const MAX_RETRYABLE_MESSAGE_APPLY_FAILURES: u8 = 3;

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

/// Durable diagnostic intent for an accepted worker generation. The worker
/// authority commit and this marker share one snapshot, so a daemon crash
/// before system-event publication is repaired without inventing a handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingHandshakeDiagnostic {
    pub connection_generation: u64,
    pub protocol_version: u16,
    pub worker_build: BuildIdentity,
    pub fleet_build: BuildIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerAuthorityRecord {
    pub worker_id: WorkerId,
    pub task_id: TaskId,
    pub session_id: SessionId,
    #[serde(default)]
    pub capability_project_root: Option<PathBuf>,
    pub worker_build: Option<BuildIdentity>,
    pub protocol_version: Option<u16>,
    pub connection_generation: u64,
    #[serde(default)]
    pub connection_installed: bool,
    pub bootstrap_proof_hash: Option<String>,
    pub reconnect_proof_hash: Option<String>,
    #[serde(default)]
    pub pending_reconnect_proof_hash: Option<String>,
    pub previous_reconnect_proof_hash: Option<String>,
    pub lease: Option<LeaseGrant>,
    pub event_ack: u64,
    pub next_command_seq: u64,
    pub command_outcome_ack: u64,
    #[serde(default)]
    pub pending_commands: BTreeMap<u64, WorkerCommand>,
    #[serde(default)]
    pub pending_command_outcomes: BTreeMap<u64, CommandOutcome>,
    #[serde(default)]
    pub session_policy: Option<SessionPolicy>,
    /// Durable operator cancellation intent. This survives the cross-store
    /// window between Shutdown issuance and the Task snapshot so recovery
    /// projects Cancelled rather than a spurious worker failure.
    #[serde(default)]
    pub cancel_requested: bool,
    #[serde(default)]
    pub pending_handshake_diagnostics: BTreeMap<u64, PendingHandshakeDiagnostic>,
    /// Consecutive retryable failures while applying worker messages. This is
    /// durable across reconnects so a persistent storage or transport failure
    /// cannot turn into an unbounded replay loop.
    #[serde(default)]
    pub retryable_message_apply_failures: u8,
    pub state: WorkerAuthorityState,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistrySnapshot {
    version: u16,
    records: BTreeMap<String, WorkerAuthorityRecord>,
}

impl Default for RegistrySnapshot {
    fn default() -> Self {
        Self {
            version: REGISTRY_FORMAT_VERSION,
            records: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct WorkerBootstrap {
    pub worker_id: WorkerId,
    #[cfg(test)]
    pub task_id: TaskId,
    pub session_id: SessionId,
    #[cfg(test)]
    pub proof: AuthenticationProof,
    pub proof_path: PathBuf,
}

#[derive(Debug)]
pub struct RegistryHandshakeGrant {
    pub connection_generation: u64,
    pub event_ack: u64,
    pub next_command_seq: u64,
    pub lease: LeaseGrant,
    pub reconnect_proof: AuthenticationProof,
    pub session_policy: SessionPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPosition {
    Duplicate,
    Contiguous,
    Gap { expected: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcomePosition {
    Duplicate { through_command_seq: u64 },
    Recorded { through_command_seq: u64 },
}

pub struct WorkerRegistry {
    path: PathBuf,
    bootstrap_dir: PathBuf,
    state: Mutex<RegistrySnapshot>,
}

impl WorkerRegistry {
    pub fn open(state_root: &Path) -> anyhow::Result<Self> {
        let path = state_root.join("worker-authority.json");
        let bootstrap_dir = state_root.join("worker-bootstrap");
        let mut state = if path.exists() {
            validate_private_file(&path)?;
            let bytes = std::fs::read(&path)?;
            serde_json::from_slice::<RegistrySnapshot>(&bytes)?
        } else {
            RegistrySnapshot::default()
        };
        if state.version != REGISTRY_FORMAT_VERSION {
            anyhow::bail!(
                "unsupported worker registry format {}; expected {}",
                state.version,
                REGISTRY_FORMAT_VERSION
            );
        }
        let now = now_ms();
        let mut recovered = false;
        for record in state.records.values_mut() {
            if matches!(
                record.state,
                WorkerAuthorityState::Active | WorkerAuthorityState::Draining
            ) {
                record.state = WorkerAuthorityState::AwaitingReattach;
                record.connection_installed = false;
                record.updated_at_unix_ms = now;
                recovered = true;
            }
        }
        let registry = Self {
            path,
            bootstrap_dir,
            state: Mutex::new(state),
        };
        if recovered {
            registry.persist_locked(&registry.lock())?;
        }
        registry.sweep_bootstrap_files()?;
        Ok(registry)
    }

    #[allow(dead_code)]
    pub fn provision(
        &self,
        task_id: TaskId,
        session_id: SessionId,
        requested_worker_id: Option<WorkerId>,
    ) -> anyhow::Result<WorkerBootstrap> {
        self.provision_scoped(task_id, session_id, requested_worker_id, None)
    }

    #[allow(dead_code)]
    pub fn provision_scoped(
        &self,
        task_id: TaskId,
        session_id: SessionId,
        requested_worker_id: Option<WorkerId>,
        capability_project_root: Option<PathBuf>,
    ) -> anyhow::Result<WorkerBootstrap> {
        self.provision_scoped_with_initial_command(
            task_id,
            session_id,
            requested_worker_id,
            capability_project_root,
            None,
        )
    }

    /// Atomically provision worker authority together with its first command.
    /// A child can therefore connect immediately without racing an owner crash
    /// between process spawn and durable command issuance.
    pub fn provision_scoped_with_initial_command(
        &self,
        task_id: TaskId,
        session_id: SessionId,
        requested_worker_id: Option<WorkerId>,
        capability_project_root: Option<PathBuf>,
        initial_command: Option<WorkerCommandKind>,
    ) -> anyhow::Result<WorkerBootstrap> {
        let worker_id = requested_worker_id
            .unwrap_or_else(|| WorkerId::new(format!("worker-{}", uuid::Uuid::new_v4())));
        let proof = AuthenticationProof::new(format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ));
        let now = now_ms();
        let mut state = self.lock();
        let before = state.clone();
        if let Some(existing) = state.records.get(worker_id.as_str())
            && !matches!(
                existing.state,
                WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost
            )
        {
            anyhow::bail!("worker {} is already provisioned", worker_id.as_str());
        }
        let mut record = WorkerAuthorityRecord {
            worker_id: worker_id.clone(),
            task_id: task_id.clone(),
            session_id: session_id.clone(),
            capability_project_root,
            worker_build: None,
            protocol_version: None,
            connection_generation: 0,
            connection_installed: false,
            bootstrap_proof_hash: Some(hash_proof(&proof)),
            reconnect_proof_hash: None,
            pending_reconnect_proof_hash: None,
            previous_reconnect_proof_hash: None,
            lease: None,
            event_ack: 0,
            next_command_seq: 1,
            command_outcome_ack: 0,
            pending_commands: BTreeMap::new(),
            pending_command_outcomes: BTreeMap::new(),
            session_policy: None,
            cancel_requested: false,
            pending_handshake_diagnostics: BTreeMap::new(),
            retryable_message_apply_failures: 0,
            state: WorkerAuthorityState::AwaitingInitialConnection,
            updated_at_unix_ms: now,
        };
        if let Some(command) = initial_command {
            enforce_command_backlog(&record, &command)?;
            let issued = WorkerCommand {
                command_seq: 1,
                command_id: CommandId::new(format!("command-{}", uuid::Uuid::new_v4())),
                command,
            };
            record.pending_commands.insert(1, issued);
            record.next_command_seq = 2;
        }
        let proof_path = self.write_bootstrap_proof(&worker_id, &proof)?;
        state.records.insert(worker_id.as_str().to_string(), record);
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            let _ = std::fs::remove_file(&proof_path);
            return Err(error);
        }
        Ok(WorkerBootstrap {
            worker_id,
            #[cfg(test)]
            task_id,
            session_id,
            #[cfg(test)]
            proof,
            proof_path,
        })
    }

    #[cfg(test)]
    pub fn accept_handshake(
        &self,
        hello: &WorkerHello,
        selected_protocol: u16,
    ) -> anyhow::Result<RegistryHandshakeGrant> {
        self.accept_handshake_with_policy(
            hello,
            selected_protocol,
            SessionPolicy {
                allowed_capabilities: Vec::new(),
                attributes: BTreeMap::new(),
            },
            BuildIdentity {
                version: "test".to_string(),
                build_id: "fleet-test-build".to_string(),
            },
        )
    }

    /// Authenticate a candidate without changing authority. Callers use this
    /// before validating task projection and filesystem scope, then repeat the
    /// same checks atomically in `accept_handshake_with_policy` before commit.
    pub fn validate_handshake_candidate(&self, hello: &WorkerHello) -> anyhow::Result<()> {
        let state = self.lock();
        let record = state
            .records
            .get(hello.worker_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("worker identity is not provisioned"))?;
        validate_handshake_record(record, hello).map(|_| ())
    }

    pub fn accept_handshake_with_policy(
        &self,
        hello: &WorkerHello,
        selected_protocol: u16,
        proposed_policy: SessionPolicy,
        fleet_build: BuildIdentity,
    ) -> anyhow::Result<RegistryHandshakeGrant> {
        let mut state = self.lock();
        let before = state.clone();
        let record = state
            .records
            .get_mut(hello.worker_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("worker identity is not provisioned"))?;
        let proof_match = validate_handshake_record(record, hello)?;
        let session_policy = record.session_policy.clone().unwrap_or(proposed_policy);
        let required_by_policy = session_policy
            .feature_policy()?
            .map(|policy| policy.enabled_features)
            .unwrap_or_default();
        let offered = hello.offered_protocol_features();
        if !required_by_policy.is_subset(&offered) {
            anyhow::bail!("worker no longer offers its durable session policy features");
        }

        match proof_match {
            HandshakeProofMatch::Pending => {
                record.reconnect_proof_hash = record.pending_reconnect_proof_hash.take();
            }
            HandshakeProofMatch::LegacyPrevious => {
                record.reconnect_proof_hash = record.previous_reconnect_proof_hash.take();
            }
            HandshakeProofMatch::Bootstrap | HandshakeProofMatch::Confirmed => {}
        }

        let reconnect_proof = AuthenticationProof::new(format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ));
        let now = now_ms();
        let lease = LeaseGrant {
            lease_id: format!("lease-{}", uuid::Uuid::new_v4()),
            expires_at_unix_ms: now.saturating_add(DEFAULT_LEASE_DURATION_MS),
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            reattach_grace_ms: DEFAULT_REATTACH_GRACE_MS,
        };
        record.connection_generation = record.connection_generation.saturating_add(1);
        let connection_generation = record.connection_generation;
        record.connection_installed = false;
        record.pending_reconnect_proof_hash = Some(hash_proof(&reconnect_proof));
        record.previous_reconnect_proof_hash = None;
        record.worker_build = Some(hello.worker_build.clone());
        record.protocol_version = Some(selected_protocol);
        record.session_policy = Some(session_policy.clone());
        record.lease = Some(lease.clone());
        record.state = WorkerAuthorityState::Active;
        record.updated_at_unix_ms = now;
        record.pending_handshake_diagnostics.insert(
            connection_generation,
            PendingHandshakeDiagnostic {
                connection_generation,
                protocol_version: selected_protocol,
                worker_build: hello.worker_build.clone(),
                fleet_build,
            },
        );
        let grant = RegistryHandshakeGrant {
            connection_generation,
            event_ack: record.event_ack,
            next_command_seq: record.next_command_seq,
            lease,
            reconnect_proof,
            session_policy,
        };
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok(grant)
    }

    pub fn pending_handshake_diagnostics(
        &self,
    ) -> Vec<(WorkerAuthorityRecord, PendingHandshakeDiagnostic)> {
        self.lock()
            .records
            .values()
            .flat_map(|record| {
                record
                    .pending_handshake_diagnostics
                    .values()
                    .cloned()
                    .map(|diagnostic| (record.clone(), diagnostic))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn clear_pending_handshake_diagnostic(
        &self,
        worker_id: &WorkerId,
        generation: u64,
    ) -> anyhow::Result<()> {
        let mut state = self.lock();
        let before = state.clone();
        let Some(record) = state.records.get_mut(worker_id.as_str()) else {
            return Ok(());
        };
        if record
            .pending_handshake_diagnostics
            .remove(&generation)
            .is_none()
        {
            return Ok(());
        }
        record.updated_at_unix_ms = now_ms();
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok(())
    }

    pub fn record_retryable_message_apply_failure(
        &self,
        worker_id: &WorkerId,
        generation: u64,
    ) -> anyhow::Result<u8> {
        let mut state = self.lock();
        let before = state.clone();
        let record = state
            .records
            .get_mut(worker_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("unknown worker"))?;
        require_generation(record, generation)?;
        record.retryable_message_apply_failures = record
            .retryable_message_apply_failures
            .saturating_add(1)
            .min(MAX_RETRYABLE_MESSAGE_APPLY_FAILURES);
        record.updated_at_unix_ms = now_ms();
        let attempts = record.retryable_message_apply_failures;
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok(attempts)
    }

    pub fn clear_retryable_message_apply_failures(
        &self,
        worker_id: &WorkerId,
        generation: u64,
    ) -> anyhow::Result<()> {
        let mut state = self.lock();
        let before = state.clone();
        let Some(record) = state.records.get_mut(worker_id.as_str()) else {
            return Ok(());
        };
        if record.connection_generation != generation
            || record.retryable_message_apply_failures == 0
            || matches!(
                record.state,
                WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost
            )
        {
            return Ok(());
        }
        record.retryable_message_apply_failures = 0;
        record.updated_at_unix_ms = now_ms();
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok(())
    }

    pub fn retryable_message_apply_failure_limit(&self) -> u8 {
        MAX_RETRYABLE_MESSAGE_APPLY_FAILURES
    }

    /// Commit receipt of FleetWelcome after the worker proves it by sending a
    /// generation-bound frame. Until this point the bootstrap verifier remains
    /// valid so a lost first Welcome cannot strand the worker.
    pub fn confirm_connection(&self, worker_id: &WorkerId, generation: u64) -> anyhow::Result<()> {
        let mut state = self.lock();
        let before = state.clone();
        let record = state
            .records
            .get_mut(worker_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("unknown worker"))?;
        require_generation(record, generation)?;
        if record.bootstrap_proof_hash.is_none() && record.connection_installed {
            return Ok(());
        }
        record.bootstrap_proof_hash = None;
        if let Some(pending) = record.pending_reconnect_proof_hash.take() {
            record.reconnect_proof_hash = Some(pending);
        }
        record.previous_reconnect_proof_hash = None;
        record.connection_installed = true;
        record.updated_at_unix_ms = now_ms();
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        self.remove_bootstrap_files(worker_id);
        Ok(())
    }

    pub fn observe_event(
        &self,
        worker_id: &WorkerId,
        generation: u64,
        event_seq: u64,
    ) -> anyhow::Result<EventPosition> {
        let mut state = self.lock();
        let before = state.clone();
        let record = state
            .records
            .get_mut(worker_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("unknown worker"))?;
        require_generation(record, generation)?;
        let expected = record.event_ack.saturating_add(1);
        if event_seq <= record.event_ack {
            return Ok(EventPosition::Duplicate);
        }
        if event_seq != expected {
            return Ok(EventPosition::Gap { expected });
        }
        record.event_ack = event_seq;
        record.updated_at_unix_ms = now_ms();
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok(EventPosition::Contiguous)
    }

    pub fn renew_lease(
        &self,
        worker_id: &WorkerId,
        generation: u64,
        lease_id: &str,
    ) -> anyhow::Result<LeaseGrant> {
        let mut state = self.lock();
        let before = state.clone();
        let record = state
            .records
            .get_mut(worker_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("unknown worker"))?;
        require_generation(record, generation)?;
        if !matches!(
            record.state,
            WorkerAuthorityState::Active | WorkerAuthorityState::Draining
        ) {
            anyhow::bail!("worker lease is not renewable in its current state");
        }
        let lease = record
            .lease
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("worker has no active lease"))?;
        if lease.lease_id != lease_id {
            anyhow::bail!("stale worker lease");
        }
        lease.expires_at_unix_ms = now_ms().saturating_add(DEFAULT_LEASE_DURATION_MS);
        record.updated_at_unix_ms = now_ms();
        let renewed = lease.clone();
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok(renewed)
    }

    pub fn observe_command_outcome(
        &self,
        worker_id: &WorkerId,
        generation: u64,
        outcome: &CommandOutcome,
    ) -> anyhow::Result<CommandOutcomePosition> {
        let mut state = self.lock();
        let before = state.clone();
        let record = state
            .records
            .get_mut(worker_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("unknown worker"))?;
        require_generation(record, generation)?;
        let command_seq = outcome.command_seq;
        if command_seq <= record.command_outcome_ack {
            return Ok(CommandOutcomePosition::Duplicate {
                through_command_seq: record.command_outcome_ack,
            });
        }
        let command = record
            .pending_commands
            .get(&command_seq)
            .ok_or_else(|| anyhow::anyhow!("worker returned an outcome for an unknown command"))?;
        if command.command_id != outcome.command_id {
            anyhow::bail!("worker command outcome identity does not match fleet authority");
        }
        let command_kind = command.command.clone();
        if let Some(previous) = record.pending_command_outcomes.get(&command_seq)
            && (previous.command_id != outcome.command_id
                || previous.accepted != outcome.accepted
                || previous.terminal && previous != outcome)
        {
            anyhow::bail!("worker command outcome changed incompatibly");
        }
        record
            .pending_command_outcomes
            .insert(command_seq, outcome.clone());
        if outcome.accepted {
            match &command_kind {
                WorkerCommandKind::Drain { .. } | WorkerCommandKind::Shutdown { .. }
                    if outcome.terminal =>
                {
                    record.state = WorkerAuthorityState::Terminal;
                    record.lease = None;
                }
                WorkerCommandKind::Drain { .. } | WorkerCommandKind::Shutdown { .. } => {
                    record.state = WorkerAuthorityState::Draining;
                }
                _ => {}
            }
        }
        loop {
            let next = record.command_outcome_ack.saturating_add(1);
            if !record
                .pending_command_outcomes
                .get(&next)
                .is_some_and(|outcome| outcome.terminal)
            {
                break;
            }
            record.command_outcome_ack = next;
            record.pending_commands.remove(&next);
            record.pending_command_outcomes.remove(&next);
        }
        record.updated_at_unix_ms = now_ms();
        let through_command_seq = record.command_outcome_ack;
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok(CommandOutcomePosition::Recorded {
            through_command_seq,
        })
    }

    pub fn enqueue_command(
        &self,
        task_id: &TaskId,
        command: WorkerCommandKind,
    ) -> anyhow::Result<(WorkerId, WorkerCommand)> {
        let mut state = self.lock();
        let before = state.clone();
        let record = state
            .records
            .values_mut()
            .filter(|record| {
                record.task_id == *task_id
                    && !matches!(
                        record.state,
                        WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost
                    )
            })
            .max_by_key(|record| record.connection_generation)
            .ok_or_else(|| anyhow::anyhow!("task has no reconnectable worker authority"))?;
        enforce_command_backlog(record, &command)?;
        let command_seq = record.next_command_seq;
        let issued = WorkerCommand {
            command_seq,
            command_id: CommandId::new(format!("command-{}", uuid::Uuid::new_v4())),
            command,
        };
        record.next_command_seq = command_seq.saturating_add(1);
        record.pending_commands.insert(command_seq, issued.clone());
        record.updated_at_unix_ms = now_ms();
        let worker_id = record.worker_id.clone();
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok((worker_id, issued))
    }

    pub fn enqueue_cancel_shutdown(
        &self,
        task_id: &TaskId,
        deadline_unix_ms: u64,
    ) -> anyhow::Result<(WorkerId, WorkerCommand)> {
        let mut state = self.lock();
        let before = state.clone();
        let record = state
            .records
            .values_mut()
            .filter(|record| {
                record.task_id == *task_id
                    && !matches!(
                        record.state,
                        WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost
                    )
            })
            .max_by_key(|record| record.connection_generation)
            .ok_or_else(|| anyhow::anyhow!("task has no reconnectable worker authority"))?;
        if let Some(existing) = record.pending_commands.values().find(|command| {
            matches!(&command.command, WorkerCommandKind::Shutdown { .. })
                && record.cancel_requested
        }) {
            return Ok((record.worker_id.clone(), existing.clone()));
        }
        let command = WorkerCommandKind::Shutdown {
            mode: bro_protocol::ShutdownMode::Graceful,
            deadline_unix_ms: Some(deadline_unix_ms),
            reason: Some("task cancelled by operator".to_string()),
        };
        enforce_command_backlog(record, &command)?;
        let command_seq = record.next_command_seq;
        let issued = WorkerCommand {
            command_seq,
            command_id: CommandId::new(format!("command-{}", uuid::Uuid::new_v4())),
            command,
        };
        record.next_command_seq = command_seq.saturating_add(1);
        record.pending_commands.insert(command_seq, issued.clone());
        record.cancel_requested = true;
        record.updated_at_unix_ms = now_ms();
        let worker_id = record.worker_id.clone();
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok((worker_id, issued))
    }

    /// Persist the one-shot compatibility drain exactly once. Replayed result
    /// events call this again after a crash, so the dedupe check must live in
    /// the same authority snapshot as the newly issued command.
    pub fn ensure_turn_completion_drain(
        &self,
        task_id: &TaskId,
        deadline_unix_ms: u64,
    ) -> anyhow::Result<Option<(WorkerId, WorkerCommand)>> {
        let mut state = self.lock();
        let before = state.clone();
        let Some(record) = state
            .records
            .values_mut()
            .filter(|record| {
                record.task_id == *task_id
                    && !matches!(
                        record.state,
                        WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost
                    )
            })
            .max_by_key(|record| record.connection_generation)
        else {
            return Ok(None);
        };
        if record.pending_commands.values().any(|command| {
            matches!(
                &command.command,
                WorkerCommandKind::Drain { .. } | WorkerCommandKind::Shutdown { .. }
            )
        }) || record.state == WorkerAuthorityState::Draining
        {
            return Ok(None);
        }
        let drain_kind = WorkerCommandKind::Drain {
            deadline_unix_ms: Some(deadline_unix_ms),
            reason: Some("turn completed".to_string()),
            safe_boundary: DrainBoundary::ImmediateIfIdle,
        };
        enforce_command_backlog(record, &drain_kind)?;
        let command_seq = record.next_command_seq;
        let issued = WorkerCommand {
            command_seq,
            command_id: CommandId::new(format!("command-{}", uuid::Uuid::new_v4())),
            command: drain_kind,
        };
        record.next_command_seq = command_seq.saturating_add(1);
        record.pending_commands.insert(command_seq, issued.clone());
        record.updated_at_unix_ms = now_ms();
        let worker_id = record.worker_id.clone();
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok(Some((worker_id, issued)))
    }

    pub fn pending_commands(&self, worker_id: &WorkerId) -> anyhow::Result<Vec<WorkerCommand>> {
        let state = self.lock();
        let record = state
            .records
            .get(worker_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("unknown worker"))?;
        Ok(record.pending_commands.values().cloned().collect())
    }

    pub fn owns_task(&self, task_id: &TaskId) -> bool {
        self.lock().records.values().any(|record| {
            record.task_id == *task_id
                && !matches!(
                    record.state,
                    WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost
                )
        })
    }

    pub fn reconnectable_task_ids(&self) -> std::collections::BTreeSet<String> {
        self.lock()
            .records
            .values()
            .filter(|record| {
                !matches!(
                    record.state,
                    WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost
                )
            })
            .map(|record| record.task_id.as_str().to_string())
            .collect()
    }

    /// Start the owner reattach grace only after the worker socket is bound.
    /// Registry load may precede expensive daemon/index startup by longer than
    /// the grace window, which must not consume workers' opportunity to attach.
    pub fn activate_reattach_window(&self, at_unix_ms: u64) -> anyhow::Result<()> {
        let mut state = self.lock();
        let before = state.clone();
        let mut changed = false;
        for record in state.records.values_mut() {
            if matches!(
                record.state,
                WorkerAuthorityState::AwaitingInitialConnection
                    | WorkerAuthorityState::AwaitingReattach
            ) {
                record.updated_at_unix_ms = at_unix_ms;
                changed = true;
            }
        }
        if changed && let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok(())
    }

    pub fn mark_disconnected(&self, worker_id: &WorkerId, generation: u64) -> anyhow::Result<()> {
        let mut state = self.lock();
        let before = state.clone();
        let Some(record) = state.records.get_mut(worker_id.as_str()) else {
            return Ok(());
        };
        if record.connection_generation != generation {
            return Ok(());
        }
        if !matches!(
            record.state,
            WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost
        ) {
            record.state = WorkerAuthorityState::AwaitingReattach;
            record.connection_installed = false;
            record.updated_at_unix_ms = now_ms();
            if let Err(error) = self.persist_locked(&state) {
                *state = before;
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn observe_status(
        &self,
        worker_id: &WorkerId,
        generation: u64,
        status: &WorkerStatus,
    ) -> anyhow::Result<WorkerAuthorityState> {
        let mut state = self.lock();
        let before = state.clone();
        let record = state
            .records
            .get_mut(worker_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("unknown worker"))?;
        require_generation(record, generation)?;
        if status.worker_id != record.worker_id
            || status.task_id != record.task_id
            || status.session_id != record.session_id
            || status.connection_generation != generation
        {
            anyhow::bail!("worker status identity does not match fleet authority");
        }
        match status.state {
            WorkerLifecycleState::Draining => record.state = WorkerAuthorityState::Draining,
            WorkerLifecycleState::Terminal => {
                record.state = WorkerAuthorityState::Terminal;
                record.lease = None;
            }
            WorkerLifecycleState::Active if record.state != WorkerAuthorityState::Draining => {
                record.state = WorkerAuthorityState::Active;
            }
            WorkerLifecycleState::Connecting | WorkerLifecycleState::Reconnecting => {}
            WorkerLifecycleState::Active => {}
            _ => {}
        }
        record.updated_at_unix_ms = now_ms();
        let observed = record.state;
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        Ok(observed)
    }

    pub fn abandon(&self, worker_id: &WorkerId) -> anyhow::Result<()> {
        let mut state = self.lock();
        let before = state.clone();
        let Some(record) = state.records.get_mut(worker_id.as_str()) else {
            return Ok(());
        };
        record.state = WorkerAuthorityState::Lost;
        record.connection_installed = false;
        record.updated_at_unix_ms = now_ms();
        if let Err(error) = self.persist_locked(&state) {
            *state = before;
            return Err(error);
        }
        self.remove_bootstrap_files(worker_id);
        Ok(())
    }

    pub fn expire_stale(&self, at_unix_ms: u64) -> anyhow::Result<Vec<WorkerId>> {
        let mut state = self.lock();
        let before = state.clone();
        let mut expired = Vec::new();
        for record in state.records.values_mut() {
            let deadline = match record.state {
                WorkerAuthorityState::Active | WorkerAuthorityState::Draining => {
                    record.lease.as_ref().map(|lease| {
                        lease
                            .expires_at_unix_ms
                            .saturating_add(lease.reattach_grace_ms)
                    })
                }
                WorkerAuthorityState::AwaitingReattach => Some(
                    record
                        .updated_at_unix_ms
                        .saturating_add(DEFAULT_REATTACH_GRACE_MS),
                ),
                WorkerAuthorityState::AwaitingInitialConnection => Some(
                    record
                        .updated_at_unix_ms
                        .saturating_add(DEFAULT_REATTACH_GRACE_MS),
                ),
                _ => None,
            };
            if deadline.is_some_and(|deadline| deadline <= at_unix_ms) {
                record.state = WorkerAuthorityState::Lost;
                record.connection_installed = false;
                record.updated_at_unix_ms = at_unix_ms;
                expired.push(record.worker_id.clone());
            }
        }
        if !expired.is_empty() {
            if let Err(error) = self.persist_locked(&state) {
                *state = before;
                return Err(error);
            }
        }
        Ok(expired)
    }

    pub fn record(&self, worker_id: &WorkerId) -> Option<WorkerAuthorityRecord> {
        self.lock().records.get(worker_id.as_str()).cloned()
    }

    /// Authority records whose terminal state may need to be projected into
    /// the independently persisted task store after an owner crash.
    pub fn records_requiring_task_reconciliation(&self) -> Vec<WorkerAuthorityRecord> {
        self.lock()
            .records
            .values()
            .filter(|record| {
                matches!(
                    record.state,
                    WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost
                )
            })
            .cloned()
            .collect()
    }

    fn write_bootstrap_proof(
        &self,
        worker_id: &WorkerId,
        proof: &AuthenticationProof,
    ) -> anyhow::Result<PathBuf> {
        create_private_dir(&self.bootstrap_dir)?;
        let path = self.bootstrap_dir.join(format!(
            "{}.{}.secret",
            worker_id.as_str(),
            uuid::Uuid::new_v4().simple()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(proof.expose_secret().as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        sync_directory(&self.bootstrap_dir)?;
        Ok(path)
    }

    fn remove_bootstrap_files(&self, worker_id: &WorkerId) {
        let prefix = format!("{}.", worker_id.as_str());
        let Ok(entries) = std::fs::read_dir(&self.bootstrap_dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".secret"))
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let _ = sync_directory(&self.bootstrap_dir);
    }

    fn sweep_bootstrap_files(&self) -> anyhow::Result<()> {
        if !self.bootstrap_dir.exists() {
            return Ok(());
        }
        create_private_dir(&self.bootstrap_dir)?;
        let entries = std::fs::read_dir(&self.bootstrap_dir)?;
        let state = self.lock();
        let mut removed = false;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                std::fs::remove_file(&path)?;
                removed = true;
                continue;
            };
            let record = state
                .records
                .values()
                .filter(|record| {
                    name.starts_with(&format!("{}.", record.worker_id.as_str()))
                        && name.ends_with(".secret")
                })
                .max_by_key(|record| record.worker_id.as_str().len());
            let expected = record.and_then(|record| record.bootstrap_proof_hash.as_deref());
            let keep = if let Some(expected) = expected {
                validate_private_file(&path)?;
                let bytes = std::fs::read(&path)?;
                if bytes.len() > 4 * 1024 {
                    false
                } else {
                    let secret = String::from_utf8_lossy(&bytes);
                    let actual = hex::encode(Sha256::digest(secret.trim_end().as_bytes()));
                    constant_time_eq(expected.as_bytes(), actual.as_bytes())
                }
            } else {
                false
            };
            if !keep {
                std::fs::remove_file(&path)?;
                removed = true;
            }
        }
        drop(state);
        if removed {
            sync_directory(&self.bootstrap_dir)?;
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistrySnapshot> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn persist_locked(&self, state: &RegistrySnapshot) -> anyhow::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("worker registry path has no parent"))?;
        create_private_dir(parent)?;
        let tmp = parent.join(format!(
            ".worker-authority.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let bytes = serde_json::to_vec_pretty(state)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }
        sync_directory(parent)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandshakeProofMatch {
    Bootstrap,
    Confirmed,
    Pending,
    LegacyPrevious,
}

fn validate_handshake_record(
    record: &WorkerAuthorityRecord,
    hello: &WorkerHello,
) -> anyhow::Result<HandshakeProofMatch> {
    if record.task_id != hello.task_id || record.session_id != hello.session_id {
        anyhow::bail!("worker task or session identity does not match its provisioned record");
    }
    if matches!(
        record.state,
        WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost
    ) {
        anyhow::bail!("worker record is terminal");
    }
    if record.connection_installed
        || matches!(
            record.state,
            WorkerAuthorityState::Active | WorkerAuthorityState::Draining
        ) && record
            .updated_at_unix_ms
            .saturating_add(HANDSHAKE_INSTALL_GRACE_MS)
            > now_ms()
    {
        anyhow::bail!("worker already has a live connection generation");
    }
    if hello.last_fleet_command_seq >= record.next_command_seq {
        anyhow::bail!("worker reported a command sequence not issued by fleet authority");
    }

    let supplied_hash = hash_proof(&hello.bootstrap_or_resume_proof);
    let matches = |expected: Option<&str>| {
        expected
            .is_some_and(|expected| constant_time_eq(expected.as_bytes(), supplied_hash.as_bytes()))
    };
    if matches(record.bootstrap_proof_hash.as_deref()) {
        Ok(HandshakeProofMatch::Bootstrap)
    } else if matches(record.reconnect_proof_hash.as_deref()) {
        Ok(HandshakeProofMatch::Confirmed)
    } else if matches(record.pending_reconnect_proof_hash.as_deref()) {
        Ok(HandshakeProofMatch::Pending)
    } else if matches(record.previous_reconnect_proof_hash.as_deref()) {
        Ok(HandshakeProofMatch::LegacyPrevious)
    } else {
        anyhow::bail!("worker proof did not match its provisioned identity")
    }
}

fn require_generation(record: &WorkerAuthorityRecord, generation: u64) -> anyhow::Result<()> {
    if record.connection_generation != generation {
        anyhow::bail!("stale worker connection generation");
    }
    Ok(())
}

fn enforce_command_backlog(
    record: &WorkerAuthorityRecord,
    next: &WorkerCommandKind,
) -> anyhow::Result<()> {
    let is_control = matches!(
        next,
        WorkerCommandKind::Interrupt
            | WorkerCommandKind::Drain { .. }
            | WorkerCommandKind::Shutdown { .. }
            | WorkerCommandKind::RequestStatus
    );
    let control_count = record
        .pending_commands
        .values()
        .filter(|command| {
            matches!(
                &command.command,
                WorkerCommandKind::Interrupt
                    | WorkerCommandKind::Drain { .. }
                    | WorkerCommandKind::Shutdown { .. }
                    | WorkerCommandKind::RequestStatus
            )
        })
        .count();
    let normal_count = record.pending_commands.len().saturating_sub(control_count);
    if (is_control && control_count >= MAX_PENDING_CONTROL_COMMANDS)
        || (!is_control && normal_count >= MAX_PENDING_NORMAL_COMMANDS)
    {
        anyhow::bail!("worker durable command backlog is full");
    }
    let current_bytes = record
        .pending_commands
        .values()
        .filter(|command| is_control_command(&command.command) == is_control)
        .try_fold(0_usize, |total, command| {
            serde_json::to_vec(command).map(|bytes| total.saturating_add(bytes.len()))
        })?;
    let next_bytes = serde_json::to_vec(next)?.len().saturating_add(256);
    let byte_budget = if is_control {
        MAX_PENDING_CONTROL_COMMAND_BYTES
    } else {
        MAX_PENDING_NORMAL_COMMAND_BYTES
    };
    if current_bytes.saturating_add(next_bytes) > byte_budget {
        anyhow::bail!("worker durable command lane byte budget is full");
    }
    Ok(())
}

fn is_control_command(command: &WorkerCommandKind) -> bool {
    matches!(
        command,
        WorkerCommandKind::Interrupt
            | WorkerCommandKind::Drain { .. }
            | WorkerCommandKind::Shutdown { .. }
            | WorkerCommandKind::RequestStatus
    )
}

fn hash_proof(proof: &AuthenticationProof) -> String {
    hex::encode(Sha256::digest(proof.expose_secret().as_bytes()))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "private state path is not a real directory: {}",
                    path.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
        }
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "private state directory has a different owner: {}",
                    path.display()
                ),
            ));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_private_file(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "private state path is not a regular file: {}",
                path.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "private state file ownership or mode is unsafe: {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    let directory = std::fs::File::open(path)?;
    directory.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(bootstrap: &WorkerBootstrap, proof: AuthenticationProof) -> WorkerHello {
        WorkerHello {
            protocol_versions: vec![1],
            worker_build: BuildIdentity {
                version: "test".to_string(),
                build_id: "worker-build".to_string(),
            },
            worker_id: bootstrap.worker_id.clone(),
            task_id: bootstrap.task_id.clone(),
            session_id: bootstrap.session_id.clone(),
            bootstrap_or_resume_proof: proof,
            last_local_event_seq: 0,
            last_fleet_command_seq: 0,
            worker_capabilities: Vec::new(),
        }
    }

    #[test]
    fn provision_authenticate_rotate_and_recover() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(
                TaskId::new("task-1"),
                SessionId::new("session-1"),
                Some(WorkerId::new("worker-1")),
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&bootstrap.proof_path)
                .unwrap()
                .trim(),
            bootstrap.proof.expose_secret()
        );
        let first = registry
            .accept_handshake(&hello(&bootstrap, bootstrap.proof.clone()), 1)
            .unwrap();
        assert_eq!(first.connection_generation, 1);
        registry
            .confirm_connection(&bootstrap.worker_id, first.connection_generation)
            .unwrap();
        assert!(
            registry
                .accept_handshake(&hello(&bootstrap, first.reconnect_proof.clone()), 1)
                .is_err(),
            "a live generation must not be taken over"
        );
        registry
            .mark_disconnected(&bootstrap.worker_id, first.connection_generation)
            .unwrap();
        let second = registry
            .accept_handshake(&hello(&bootstrap, first.reconnect_proof), 1)
            .unwrap();
        assert_eq!(second.connection_generation, 2);
        drop(registry);

        let recovered = WorkerRegistry::open(&root).unwrap();
        let record = recovered.record(&bootstrap.worker_id).unwrap();
        assert_eq!(record.state, WorkerAuthorityState::AwaitingReattach);
        assert_eq!(record.connection_generation, 2);
        assert!(record.bootstrap_proof_hash.is_none());
    }

    #[test]
    fn lost_first_welcome_retains_bootstrap_until_worker_confirms() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(TaskId::new("task-1"), SessionId::new("session-1"), None)
            .unwrap();
        let first = registry
            .accept_handshake(&hello(&bootstrap, bootstrap.proof.clone()), 1)
            .unwrap();
        assert_eq!(first.connection_generation, 1);
        assert!(bootstrap.proof_path.exists());
        assert!(
            registry
                .record(&bootstrap.worker_id)
                .unwrap()
                .bootstrap_proof_hash
                .is_some()
        );

        {
            let mut state = registry.lock();
            state
                .records
                .get_mut(bootstrap.worker_id.as_str())
                .unwrap()
                .updated_at_unix_ms = 0;
        }
        let second = registry
            .accept_handshake(&hello(&bootstrap, bootstrap.proof.clone()), 1)
            .unwrap();
        assert_eq!(second.connection_generation, 2);
        registry
            .confirm_connection(&bootstrap.worker_id, second.connection_generation)
            .unwrap();
        assert!(!bootstrap.proof_path.exists());
        assert!(
            registry
                .record(&bootstrap.worker_id)
                .unwrap()
                .bootstrap_proof_hash
                .is_none()
        );
    }

    #[test]
    fn startup_removes_orphan_bootstrap_secrets_but_keeps_the_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(TaskId::new("task-1"), SessionId::new("session-1"), None)
            .unwrap();
        let orphan = root
            .join("worker-bootstrap")
            .join("worker-orphan.00000000.secret");
        {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&orphan).unwrap();
            file.write_all(b"orphan\n").unwrap();
            file.sync_all().unwrap();
        }
        drop(registry);

        let _recovered = WorkerRegistry::open(&root).unwrap();
        assert!(bootstrap.proof_path.exists());
        assert!(!orphan.exists());
    }

    #[test]
    fn repeated_lost_reconnect_welcomes_preserve_confirmed_proof() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(TaskId::new("task-1"), SessionId::new("session-1"), None)
            .unwrap();
        let first = registry
            .accept_handshake(&hello(&bootstrap, bootstrap.proof.clone()), 1)
            .unwrap();
        let confirmed_proof = first.reconnect_proof.clone();
        registry
            .confirm_connection(&bootstrap.worker_id, first.connection_generation)
            .unwrap();
        registry
            .mark_disconnected(&bootstrap.worker_id, first.connection_generation)
            .unwrap();

        let lost_one = registry
            .accept_handshake(&hello(&bootstrap, confirmed_proof.clone()), 1)
            .unwrap();
        assert_eq!(lost_one.connection_generation, 2);
        {
            let mut state = registry.lock();
            state
                .records
                .get_mut(bootstrap.worker_id.as_str())
                .unwrap()
                .updated_at_unix_ms = 0;
        }
        let lost_two = registry
            .accept_handshake(&hello(&bootstrap, confirmed_proof.clone()), 1)
            .unwrap();
        assert_eq!(lost_two.connection_generation, 3);
        {
            let mut state = registry.lock();
            state
                .records
                .get_mut(bootstrap.worker_id.as_str())
                .unwrap()
                .updated_at_unix_ms = 0;
        }
        let delivered = registry
            .accept_handshake(&hello(&bootstrap, confirmed_proof), 1)
            .unwrap();
        assert_eq!(delivered.connection_generation, 4);
        registry
            .confirm_connection(&bootstrap.worker_id, delivered.connection_generation)
            .unwrap();
        let record = registry.record(&bootstrap.worker_id).unwrap();
        assert!(record.connection_installed);
        assert!(record.pending_reconnect_proof_hash.is_none());
    }

    #[test]
    fn identity_and_proof_mismatch_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(TaskId::new("task-1"), SessionId::new("session-1"), None)
            .unwrap();
        let mut wrong_task = hello(&bootstrap, bootstrap.proof.clone());
        wrong_task.task_id = TaskId::new("task-other");
        assert!(registry.accept_handshake(&wrong_task, 1).is_err());
        assert!(
            registry
                .accept_handshake(&hello(&bootstrap, AuthenticationProof::new("wrong")), 1)
                .is_err()
        );
    }

    #[test]
    fn event_ack_is_contiguous_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(TaskId::new("task-1"), SessionId::new("session-1"), None)
            .unwrap();
        let grant = registry
            .accept_handshake(&hello(&bootstrap, bootstrap.proof.clone()), 1)
            .unwrap();
        assert_eq!(
            registry
                .observe_event(&bootstrap.worker_id, grant.connection_generation, 1)
                .unwrap(),
            EventPosition::Contiguous
        );
        assert_eq!(
            registry
                .observe_event(&bootstrap.worker_id, grant.connection_generation, 1)
                .unwrap(),
            EventPosition::Duplicate
        );
        assert_eq!(
            registry
                .observe_event(&bootstrap.worker_id, grant.connection_generation, 3)
                .unwrap(),
            EventPosition::Gap { expected: 2 }
        );
        drop(registry);
        assert_eq!(
            WorkerRegistry::open(&root)
                .unwrap()
                .record(&bootstrap.worker_id)
                .unwrap()
                .event_ack,
            1
        );
    }

    #[test]
    fn commands_are_durable_and_generation_fenced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(TaskId::new("task-1"), SessionId::new("session-1"), None)
            .unwrap();
        let grant = registry
            .accept_handshake(&hello(&bootstrap, bootstrap.proof.clone()), 1)
            .unwrap();
        let (worker_id, command) = registry
            .enqueue_command(
                &bootstrap.task_id,
                WorkerCommandKind::UserTurn {
                    text: "hello".to_string(),
                },
            )
            .unwrap();
        assert_eq!(worker_id, bootstrap.worker_id);
        assert!(
            registry
                .observe_command_outcome(
                    &worker_id,
                    grant.connection_generation.saturating_add(1),
                    &CommandOutcome {
                        command_seq: command.command_seq,
                        command_id: command.command_id.clone(),
                        accepted: true,
                        terminal: true,
                        result_or_error: serde_json::json!({"queued": true}),
                    },
                )
                .is_err()
        );
        drop(registry);

        let recovered = WorkerRegistry::open(&root).unwrap();
        assert_eq!(
            recovered.pending_commands(&worker_id).unwrap(),
            vec![command.clone()]
        );
        assert_eq!(
            recovered
                .observe_command_outcome(
                    &worker_id,
                    grant.connection_generation,
                    &CommandOutcome {
                        command_seq: command.command_seq,
                        command_id: command.command_id,
                        accepted: true,
                        terminal: true,
                        result_or_error: serde_json::json!({"queued": true}),
                    },
                )
                .unwrap(),
            CommandOutcomePosition::Recorded {
                through_command_seq: command.command_seq,
            }
        );
        assert!(recovered.pending_commands(&worker_id).unwrap().is_empty());
    }

    #[test]
    fn provisioning_persists_the_initial_turn_in_the_same_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision_scoped_with_initial_command(
                TaskId::new("task-initial"),
                SessionId::new("session-initial"),
                Some(WorkerId::new("worker-initial")),
                None,
                Some(WorkerCommandKind::UserTurn {
                    text: "durable first turn".to_string(),
                }),
            )
            .unwrap();
        drop(registry);

        let recovered = WorkerRegistry::open(&root).unwrap();
        let record = recovered.record(&bootstrap.worker_id).unwrap();
        assert_eq!(record.next_command_seq, 2);
        let commands = recovered.pending_commands(&bootstrap.worker_id).unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command_seq, 1);
        assert_eq!(
            commands[0].command,
            WorkerCommandKind::UserTurn {
                text: "durable first turn".to_string(),
            }
        );
    }

    #[test]
    fn cancellation_intent_and_shutdown_are_persisted_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(
                TaskId::new("task-cancel"),
                SessionId::new("session-cancel"),
                None,
            )
            .unwrap();
        let (_, shutdown) = registry
            .enqueue_cancel_shutdown(&bootstrap.task_id, 123_456)
            .unwrap();
        drop(registry);

        let recovered = WorkerRegistry::open(&root).unwrap();
        let record = recovered.record(&bootstrap.worker_id).unwrap();
        assert!(record.cancel_requested);
        assert_eq!(
            recovered.pending_commands(&bootstrap.worker_id).unwrap(),
            vec![shutdown]
        );
    }

    #[test]
    fn accepted_handshake_diagnostic_survives_restart_until_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(
                TaskId::new("task-diagnostic"),
                SessionId::new("session-diagnostic"),
                None,
            )
            .unwrap();
        let grant = registry
            .accept_handshake(&hello(&bootstrap, bootstrap.proof.clone()), 1)
            .unwrap();
        let pending = registry.pending_handshake_diagnostics();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].1,
            PendingHandshakeDiagnostic {
                connection_generation: grant.connection_generation,
                protocol_version: 1,
                worker_build: BuildIdentity {
                    version: "test".to_string(),
                    build_id: "worker-build".to_string(),
                },
                fleet_build: BuildIdentity {
                    version: "test".to_string(),
                    build_id: "fleet-test-build".to_string(),
                },
            }
        );
        drop(registry);

        let recovered = WorkerRegistry::open(&root).unwrap();
        assert_eq!(recovered.pending_handshake_diagnostics().len(), 1);
        recovered
            .clear_pending_handshake_diagnostic(&bootstrap.worker_id, grant.connection_generation)
            .unwrap();
        drop(recovered);
        assert!(
            WorkerRegistry::open(&root)
                .unwrap()
                .pending_handshake_diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn reattach_grace_starts_when_the_owner_socket_becomes_ready() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(
                TaskId::new("task-reattach"),
                SessionId::new("session-reattach"),
                None,
            )
            .unwrap();
        let grant = registry
            .accept_handshake(&hello(&bootstrap, bootstrap.proof.clone()), 1)
            .unwrap();
        registry
            .confirm_connection(&bootstrap.worker_id, grant.connection_generation)
            .unwrap();
        registry
            .mark_disconnected(&bootstrap.worker_id, grant.connection_generation)
            .unwrap();

        let socket_ready_at = 1_000_000;
        registry.activate_reattach_window(socket_ready_at).unwrap();
        assert!(
            registry
                .expire_stale(
                    socket_ready_at
                        .saturating_add(DEFAULT_REATTACH_GRACE_MS)
                        .saturating_sub(1)
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            registry
                .expire_stale(socket_ready_at.saturating_add(DEFAULT_REATTACH_GRACE_MS))
                .unwrap(),
            vec![bootstrap.worker_id]
        );
    }

    #[test]
    fn saturated_normal_byte_lane_does_not_block_control_commands() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(
                TaskId::new("task-control-lane"),
                SessionId::new("session-control-lane"),
                None,
            )
            .unwrap();
        let mut record = registry.record(&bootstrap.worker_id).unwrap();
        record.pending_commands.insert(
            1,
            WorkerCommand {
                command_seq: 1,
                command_id: CommandId::new("large-normal-command"),
                command: WorkerCommandKind::UserTurn {
                    text: "x".repeat(MAX_PENDING_NORMAL_COMMAND_BYTES),
                },
            },
        );

        assert!(
            enforce_command_backlog(
                &record,
                &WorkerCommandKind::UserTurn {
                    text: "another normal command".to_string(),
                }
            )
            .is_err()
        );
        enforce_command_backlog(&record, &WorkerCommandKind::Interrupt).unwrap();
    }

    #[test]
    fn retryable_message_failures_are_bounded_and_clear_after_success() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(
                TaskId::new("task-retry-budget"),
                SessionId::new("session-retry-budget"),
                None,
            )
            .unwrap();
        let grant = registry
            .accept_handshake(&hello(&bootstrap, bootstrap.proof.clone()), 1)
            .unwrap();
        for expected in 1..=MAX_RETRYABLE_MESSAGE_APPLY_FAILURES {
            assert_eq!(
                registry
                    .record_retryable_message_apply_failure(
                        &bootstrap.worker_id,
                        grant.connection_generation,
                    )
                    .unwrap(),
                expected
            );
        }
        assert_eq!(
            registry
                .record_retryable_message_apply_failure(
                    &bootstrap.worker_id,
                    grant.connection_generation,
                )
                .unwrap(),
            MAX_RETRYABLE_MESSAGE_APPLY_FAILURES
        );
        registry
            .clear_retryable_message_apply_failures(
                &bootstrap.worker_id,
                grant.connection_generation,
            )
            .unwrap();
        assert_eq!(
            registry
                .record(&bootstrap.worker_id)
                .unwrap()
                .retryable_message_apply_failures,
            0
        );
    }

    #[test]
    fn terminal_command_ack_advances_over_out_of_order_completions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let registry = WorkerRegistry::open(&root).unwrap();
        let bootstrap = registry
            .provision(TaskId::new("task-1"), SessionId::new("session-1"), None)
            .unwrap();
        let grant = registry
            .accept_handshake(&hello(&bootstrap, bootstrap.proof.clone()), 1)
            .unwrap();
        let (_, first) = registry
            .enqueue_command(
                &bootstrap.task_id,
                WorkerCommandKind::UserTurn {
                    text: "first".to_string(),
                },
            )
            .unwrap();
        let (_, second) = registry
            .enqueue_command(&bootstrap.task_id, WorkerCommandKind::Interrupt)
            .unwrap();
        let terminal = |command: &WorkerCommand| CommandOutcome {
            command_seq: command.command_seq,
            command_id: command.command_id.clone(),
            accepted: true,
            terminal: true,
            result_or_error: serde_json::json!({"done": true}),
        };
        assert_eq!(
            registry
                .observe_command_outcome(
                    &bootstrap.worker_id,
                    grant.connection_generation,
                    &terminal(&second),
                )
                .unwrap(),
            CommandOutcomePosition::Recorded {
                through_command_seq: 0,
            }
        );
        assert_eq!(
            registry
                .observe_command_outcome(
                    &bootstrap.worker_id,
                    grant.connection_generation,
                    &terminal(&first),
                )
                .unwrap(),
            CommandOutcomePosition::Recorded {
                through_command_seq: 2,
            }
        );
        assert!(
            registry
                .pending_commands(&bootstrap.worker_id)
                .unwrap()
                .is_empty()
        );
    }
}
