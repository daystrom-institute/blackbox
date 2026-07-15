// Migration is an explicit startup/offline operation. The crate has no async
// runtime, and service hosts must invoke these synchronous readers off workers.
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use bro_core::{Capability, Origin, Provider, SessionId, TaskId};
use bro_protocol::{BroReportV1, TaskStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{FleetError, FleetResult};
use crate::model::{
    FleetSnapshot, ProviderDefaultsPolicy, RuntimeLeaseRecord, SessionRecord, TaskRecord,
    WorkerAuthorityRecord, WorkerAuthorityState,
};
use crate::roster::project_task;

const LEGACY_WORKER_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyMigrationOptions {
    pub now_unix_ms: u64,
    /// Optional operator-selected retention boundary. Migration defaults to no
    /// TTL because reconnect reconciliation must precede retention policy.
    pub retain_tasks_started_at_or_after: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum MigrationDiagnostic {
    TaskDroppedByRetention {
        task_id: TaskId,
    },
    RunningTaskWithoutReconnectableWorker {
        task_id: TaskId,
    },
    WorkerRecoveredFromRuntimeLease {
        worker_id: bro_core::WorkerId,
    },
    TerminalTaskClosedWorker {
        task_id: TaskId,
        worker_id: bro_core::WorkerId,
    },
    ActiveWorkerMadeReconnectable {
        worker_id: bro_core::WorkerId,
    },
    StaleTemporaryFileIgnored {
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MigrationReport {
    pub snapshot: FleetSnapshot,
    pub diagnostics: Vec<MigrationDiagnostic>,
    /// SHA-256 digests of the exact source bytes used to construct the result.
    pub source_digests: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyTaskRecord {
    pub id: String,
    pub provider: Provider,
    pub session_id: String,
    pub events: Vec<Value>,
    #[serde(default)]
    pub model: Option<String>,
    pub last_assistant_message: Option<String>,
    pub usage: Option<Value>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub stderr: String,
    pub status: LegacyTaskStatus,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub exit_code: Option<i32>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub managed_worktree: Option<String>,
    #[serde(default)]
    pub bro_label: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub agent_label: Option<String>,
    #[serde(default)]
    pub report: Option<LegacyBroReport>,
    #[serde(default)]
    pub interrupted: bool,
    #[serde(default)]
    pub recoverable: bool,
    #[serde(default)]
    pub transcript_location: Option<LegacyTranscriptLocation>,
    #[serde(default)]
    pub transcript_cursor: Option<Value>,
    #[serde(default)]
    pub live_cursor: u64,
    #[serde(default)]
    pub supervision: Value,
    #[serde(default)]
    pub origin: Origin,
    #[serde(default)]
    pub workflow_owned: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegacyTaskStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyBroReport {
    pub message: String,
    #[serde(default)]
    pub needs: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(rename = "reportedAt")]
    pub reported_at: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyTranscriptLocation {
    pub provider: String,
    pub storage: String,
    pub path: PathBuf,
    pub account: Option<String>,
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub is_subagent: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyWorkerAuthority {
    pub version: u32,
    pub records: BTreeMap<String, WorkerAuthorityRecord>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LegacyRuntimeLeaseStore {
    #[serde(default)]
    pub leases: BTreeMap<String, LegacyRuntimeLease>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyRuntimeLease {
    pub task_id: String,
    pub session_id: String,
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
    pub created_at: u64,
    pub last_seen_at: u64,
    #[serde(default)]
    pub brofile_context: Option<LegacyBrofileContext>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyBrofileContext {
    #[serde(default)]
    pub provider_defaults: Option<ProviderDefaultsPolicy>,
}

pub fn read_legacy_tasks(path: &Path) -> FleetResult<Vec<LegacyTaskRecord>> {
    let bytes = fs::read(path)?;
    decode_legacy_tasks(&bytes)
}

pub fn read_legacy_worker_authority(path: &Path) -> FleetResult<LegacyWorkerAuthority> {
    let bytes = fs::read(path)?;
    decode_legacy_worker_authority(&bytes)
}

pub fn read_legacy_runtime_leases(path: &Path) -> FleetResult<LegacyRuntimeLeaseStore> {
    let bytes = fs::read(path)?;
    decode_legacy_runtime_leases(&bytes)
}

/// Reads and reconciles the legacy authority files without mutating them.
/// Each present source is read twice and rejected if it changes during import.
pub fn migrate_legacy_files(
    tasks_path: &Path,
    workers_path: Option<&Path>,
    runtime_leases_path: Option<&Path>,
    options: LegacyMigrationOptions,
) -> FleetResult<MigrationReport> {
    let stale_temporary_files =
        find_stale_temporary_files(tasks_path, workers_path, runtime_leases_path)?;
    let task_bytes = read_stable(tasks_path)?;
    let worker_bytes = workers_path.map(read_stable).transpose()?;
    let lease_bytes = runtime_leases_path.map(read_stable).transpose()?;
    let tasks = decode_legacy_tasks(&task_bytes)?;
    let workers = worker_bytes
        .as_deref()
        .map(decode_legacy_worker_authority)
        .transpose()?
        .unwrap_or_else(|| LegacyWorkerAuthority {
            version: LEGACY_WORKER_VERSION,
            records: BTreeMap::new(),
        });
    let leases = lease_bytes
        .as_deref()
        .map(decode_legacy_runtime_leases)
        .transpose()?
        .unwrap_or_default();

    let mut source_digests = BTreeMap::new();
    source_digests.insert(path_key(tasks_path), digest_bytes(&task_bytes));
    if let (Some(path), Some(bytes)) = (workers_path, &worker_bytes) {
        source_digests.insert(path_key(path), digest_bytes(bytes));
    }
    if let (Some(path), Some(bytes)) = (runtime_leases_path, &lease_bytes) {
        source_digests.insert(path_key(path), digest_bytes(bytes));
    }
    let mut report = reconcile(tasks, workers, leases, options, source_digests)?;
    report
        .diagnostics
        .extend(stale_temporary_files.into_iter().map(|path| {
            MigrationDiagnostic::StaleTemporaryFileIgnored {
                path: path.to_string_lossy().into_owned(),
            }
        }));
    report
        .diagnostics
        .sort_by_key(|diagnostic| format!("{diagnostic:?}"));
    Ok(report)
}

fn find_stale_temporary_files(
    tasks_path: &Path,
    workers_path: Option<&Path>,
    runtime_leases_path: Option<&Path>,
) -> FleetResult<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    for path in [Some(tasks_path), runtime_leases_path]
        .into_iter()
        .flatten()
    {
        let temporary = path.with_file_name(format!(
            "{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("state.json")
        ));
        if temporary.try_exists()? {
            candidates.push(temporary);
        }
    }
    if let Some(worker_path) = workers_path {
        let parent = worker_path.parent().unwrap_or_else(|| Path::new("."));
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".worker-authority.") && name.ends_with(".tmp") {
                candidates.push(entry.path());
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    Ok(candidates)
}

fn decode_legacy_tasks(bytes: &[u8]) -> FleetResult<Vec<LegacyTaskRecord>> {
    let tasks: Vec<LegacyTaskRecord> = serde_json::from_slice(bytes)?;
    let mut identifiers = BTreeSet::new();
    for task in &tasks {
        validate_identifier("task", &task.id)?;
        validate_identifier("session", &task.session_id)?;
        if !identifiers.insert(task.id.clone()) {
            return Err(FleetError::InvalidState(format!(
                "duplicate legacy task id {}",
                task.id
            )));
        }
    }
    Ok(tasks)
}

fn decode_legacy_worker_authority(bytes: &[u8]) -> FleetResult<LegacyWorkerAuthority> {
    let mut authority: LegacyWorkerAuthority = serde_json::from_slice(bytes)?;
    if authority.version != LEGACY_WORKER_VERSION {
        return Err(FleetError::UnsupportedLegacyWorkerVersion(
            authority.version,
        ));
    }
    let mut live_tasks = BTreeSet::new();
    for (key, worker) in &mut authority.records {
        if key != worker.worker_id.as_str() {
            return Err(FleetError::InvalidState(format!(
                "legacy worker map key {key} differs from record {}",
                worker.worker_id
            )));
        }
        validate_identifier("worker", worker.worker_id.as_str())?;
        validate_identifier("task", worker.task_id.as_str())?;
        validate_identifier("session", worker.session_id.as_str())?;
        if worker.next_command_seq == 0 || worker.command_outcome_ack >= worker.next_command_seq {
            return Err(FleetError::InvalidState(format!(
                "worker {key} has incoherent command acknowledgement bounds"
            )));
        }
        for sequence in worker.command_outcome_ack.saturating_add(1)..worker.next_command_seq {
            let command = worker.pending_commands.get(&sequence).ok_or_else(|| {
                FleetError::InvalidState(format!(
                    "worker {key} is missing pending command {sequence}"
                ))
            })?;
            if command.command_seq != sequence {
                return Err(FleetError::InvalidState(format!(
                    "worker {key} command key {sequence} differs from embedded sequence {}",
                    command.command_seq
                )));
            }
        }
        for (sequence, command) in &worker.pending_commands {
            if *sequence <= worker.command_outcome_ack
                || *sequence >= worker.next_command_seq
                || command.command_seq != *sequence
            {
                return Err(FleetError::InvalidState(format!(
                    "worker {key} has pending command outside its unacknowledged sequence range"
                )));
            }
        }
        for (sequence, outcome) in &worker.pending_command_outcomes {
            let command = worker.pending_commands.get(sequence).ok_or_else(|| {
                FleetError::InvalidState(format!(
                    "worker {key} outcome {sequence} has no pending command"
                ))
            })?;
            if outcome.command_seq != *sequence || outcome.command_id != command.command_id {
                return Err(FleetError::InvalidState(format!(
                    "worker {key} outcome {sequence} conflicts with its command"
                )));
            }
        }
        normalize_proof_hash(&mut worker.bootstrap_proof_hash, key)?;
        normalize_proof_hash(&mut worker.reconnect_proof_hash, key)?;
        normalize_proof_hash(&mut worker.previous_reconnect_proof_hash, key)?;
        normalize_proof_hash(&mut worker.pending_reconnect_proof_hash, key)?;
        match worker.state {
            WorkerAuthorityState::AwaitingInitialConnection => {
                if worker.connection_generation != 0 || worker.bootstrap_proof_hash.is_none() {
                    return Err(FleetError::InvalidState(format!(
                        "awaiting-initial worker {key} has incoherent bootstrap authority"
                    )));
                }
            }
            WorkerAuthorityState::Active | WorkerAuthorityState::Draining => {
                if worker.connection_generation == 0
                    || worker.lease.is_none()
                    || worker.reconnect_proof_hash.is_none()
                {
                    return Err(FleetError::InvalidState(format!(
                        "live worker {key} is missing generation, lease, or reconnect verifier"
                    )));
                }
            }
            WorkerAuthorityState::AwaitingReattach => {
                if worker.connection_generation == 0 || worker.reconnect_proof_hash.is_none() {
                    return Err(FleetError::InvalidState(format!(
                        "reattaching worker {key} is missing reconnect authority"
                    )));
                }
            }
            WorkerAuthorityState::Terminal | WorkerAuthorityState::Lost => {}
        }
        if !worker.state.is_terminal() && !live_tasks.insert(worker.task_id.clone()) {
            return Err(FleetError::InvalidState(format!(
                "multiple nonterminal workers own task {}",
                worker.task_id
            )));
        }
    }
    Ok(authority)
}

fn decode_legacy_runtime_leases(bytes: &[u8]) -> FleetResult<LegacyRuntimeLeaseStore> {
    let leases: LegacyRuntimeLeaseStore = serde_json::from_slice(bytes)?;
    for (key, lease) in &leases.leases {
        validate_identifier("task", key)?;
        validate_identifier("task", &lease.task_id)?;
        validate_identifier("session", &lease.session_id)?;
        if key != &lease.task_id {
            return Err(FleetError::InvalidState(format!(
                "legacy runtime lease map key {key} differs from record {}",
                lease.task_id
            )));
        }
        if lease.selection_trace_id.trim().is_empty() {
            return Err(FleetError::InvalidState(format!(
                "legacy runtime lease {key} has an empty selection trace id"
            )));
        }
    }
    Ok(leases)
}

fn reconcile(
    tasks: Vec<LegacyTaskRecord>,
    workers: LegacyWorkerAuthority,
    leases: LegacyRuntimeLeaseStore,
    options: LegacyMigrationOptions,
    source_digests: BTreeMap<String, String>,
) -> FleetResult<MigrationReport> {
    let mut snapshot = FleetSnapshot::default();
    let mut diagnostics = Vec::new();

    for (key, lease) in leases.leases {
        let task_id = TaskId::new(key);
        snapshot.runtime_leases.insert(
            task_id.clone(),
            RuntimeLeaseRecord {
                task_id,
                session_id: SessionId::new(lease.session_id),
                provider: lease.provider,
                account: lease.account,
                model: lease.model,
                effort: lease.effort,
                tier: lease.tier,
                durable: lease.durable,
                capabilities: lease.capabilities,
                project_dir: lease.project_dir,
                cwd: lease.cwd,
                selection_trace_id: lease.selection_trace_id,
                created_at_unix_ms: lease.created_at,
                last_seen_at_unix_ms: lease.last_seen_at,
                provider_defaults: lease
                    .brofile_context
                    .and_then(|context| context.provider_defaults),
            },
        );
    }

    for legacy in tasks {
        let task_id = TaskId::new(legacy.id.clone());
        if options
            .retain_tasks_started_at_or_after
            .is_some_and(|minimum| legacy.started_at < minimum)
        {
            diagnostics.push(MigrationDiagnostic::TaskDroppedByRetention { task_id });
            continue;
        }
        let session_id = SessionId::new(legacy.session_id.clone());
        let reconnectable = workers.records.values().any(|worker| {
            worker.task_id == task_id
                && worker.session_id == session_id
                && !worker.state.is_terminal()
        });
        let mut status = legacy_status(legacy.status);
        let mut completed_at = legacy.completed_at;
        let mut recoverable = legacy.recoverable;
        let mut error_teaser = stderr_teaser(&legacy.stderr);
        if status == TaskStatus::Running && !reconnectable {
            status = TaskStatus::Failed;
            completed_at = Some(options.now_unix_ms);
            recoverable = true;
            error_teaser = Some("live worker authority was unavailable during migration".into());
            diagnostics.push(MigrationDiagnostic::RunningTaskWithoutReconnectableWorker {
                task_id: task_id.clone(),
            });
        }
        let transcript_path = legacy
            .transcript_location
            .as_ref()
            .map(|location| location.path.to_string_lossy().into_owned());
        let model = legacy.model.or_else(|| infer_model(&legacy.events));
        let report = legacy.report.map(|report| BroReportV1 {
            message: report.message,
            needs: report.needs,
            data: report.data,
            reported_at: report.reported_at,
            reported_ago: "unknown".to_string(),
        });
        let last_event_at = completed_at.unwrap_or(legacy.started_at).max(
            legacy
                .supervision
                .get("last_event_at_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        let workflow_owned = legacy
            .workflow_owned
            .unwrap_or(matches!(legacy.origin, Origin::Workflow | Origin::Atom));
        let task = TaskRecord {
            task_id: task_id.clone(),
            session_id: session_id.clone(),
            attempt_id: None,
            worker_id: workers
                .records
                .values()
                .filter(|worker| worker.task_id == task_id && !worker.state.is_terminal())
                .max_by_key(|worker| worker.connection_generation)
                .map(|worker| worker.worker_id.clone()),
            status,
            provider: legacy.provider,
            model,
            cost: legacy.cost_usd,
            turns: legacy.num_turns,
            cwd: legacy.cwd,
            managed_worktree: legacy.managed_worktree,
            transcript_path: transcript_path.clone(),
            transcript_cursor: legacy.transcript_cursor,
            last_message: legacy.last_assistant_message,
            report,
            error_teaser,
            label: legacy.bro_label,
            name: legacy.name,
            agent_label: legacy.agent_label,
            origin: legacy.origin,
            workflow_owned,
            interrupted: legacy.interrupted,
            cancellation_requested_at_unix_ms: None,
            recoverable,
            started_at_unix_ms: legacy.started_at,
            last_event_at_unix_ms: last_event_at,
            completed_at_unix_ms: completed_at,
        };
        insert_session_task(&mut snapshot, &task, transcript_path)?;
        snapshot.tasks.insert(task_id, task);
    }

    for (_, mut worker) in workers.records {
        if !snapshot.tasks.contains_key(&worker.task_id) {
            let runtime = snapshot
                .runtime_leases
                .get(&worker.task_id)
                .cloned()
                .ok_or_else(|| {
                    FleetError::InvalidState(format!(
                        "worker {} references missing task {} without a runtime lease",
                        worker.worker_id, worker.task_id
                    ))
                })?;
            if runtime.session_id != worker.session_id {
                return Err(FleetError::InvalidState(format!(
                    "worker {} session differs from runtime lease",
                    worker.worker_id
                )));
            }
            let task = TaskRecord {
                task_id: worker.task_id.clone(),
                session_id: worker.session_id.clone(),
                attempt_id: None,
                worker_id: Some(worker.worker_id.clone()),
                status: if worker.state.is_terminal() {
                    TaskStatus::Failed
                } else {
                    TaskStatus::Running
                },
                provider: runtime.provider,
                model: runtime.model,
                cost: None,
                turns: None,
                cwd: runtime.cwd,
                managed_worktree: None,
                transcript_path: None,
                transcript_cursor: None,
                last_message: None,
                report: None,
                error_teaser: None,
                label: None,
                name: None,
                agent_label: None,
                origin: Origin::Unknown,
                workflow_owned: false,
                interrupted: false,
                cancellation_requested_at_unix_ms: None,
                recoverable: worker.state.is_terminal(),
                started_at_unix_ms: runtime.created_at_unix_ms,
                last_event_at_unix_ms: worker.updated_at_unix_ms,
                completed_at_unix_ms: worker
                    .state
                    .is_terminal()
                    .then_some(worker.updated_at_unix_ms),
            };
            insert_session_task(&mut snapshot, &task, None)?;
            snapshot.tasks.insert(worker.task_id.clone(), task);
            diagnostics.push(MigrationDiagnostic::WorkerRecoveredFromRuntimeLease {
                worker_id: worker.worker_id.clone(),
            });
        }
        let task = snapshot.tasks.get(&worker.task_id).ok_or_else(|| {
            FleetError::InvalidState(format!(
                "worker {} task reconciliation was lost",
                worker.worker_id
            ))
        })?;
        if task.session_id != worker.session_id {
            return Err(FleetError::InvalidState(format!(
                "worker {} session differs from task {}",
                worker.worker_id, worker.task_id
            )));
        }
        let session = snapshot.sessions.get(&task.session_id).ok_or_else(|| {
            FleetError::InvalidState(format!(
                "task {} session reconciliation was lost",
                task.task_id
            ))
        })?;
        if task.provider != session.provider {
            return Err(FleetError::InvalidState(format!(
                "task {} provider differs from its session",
                task.task_id
            )));
        }
        if task.status.is_terminal() && !worker.state.is_terminal() {
            worker.state = WorkerAuthorityState::Terminal;
            worker.updated_at_unix_ms = options.now_unix_ms;
            diagnostics.push(MigrationDiagnostic::TerminalTaskClosedWorker {
                task_id: task.task_id.clone(),
                worker_id: worker.worker_id.clone(),
            });
        } else if matches!(
            worker.state,
            WorkerAuthorityState::Active | WorkerAuthorityState::Draining
        ) {
            worker.state = WorkerAuthorityState::AwaitingReattach;
            worker.updated_at_unix_ms = options.now_unix_ms;
            diagnostics.push(MigrationDiagnostic::ActiveWorkerMadeReconnectable {
                worker_id: worker.worker_id.clone(),
            });
        }
        snapshot.workers.insert(worker.worker_id.clone(), worker);
    }

    for (task_id, lease) in &snapshot.runtime_leases {
        if let Some(task) = snapshot.tasks.get(task_id)
            && (task.provider != lease.provider || task.session_id != lease.session_id)
        {
            return Err(FleetError::InvalidState(format!(
                "runtime lease {task_id} conflicts with imported task"
            )));
        }
        if !snapshot.sessions.contains_key(&lease.session_id) {
            snapshot.sessions.insert(
                lease.session_id.clone(),
                SessionRecord {
                    session_id: lease.session_id.clone(),
                    provider: lease.provider,
                    task_ids: Vec::new(),
                    current_task_id: None,
                    agent_binding: None,
                    transcript_path: None,
                    requested_capability_policy: Default::default(),
                    resumable: lease.durable,
                    created_at_unix_ms: lease.created_at_unix_ms,
                    updated_at_unix_ms: lease.last_seen_at_unix_ms,
                },
            );
        }
    }
    for (task_id, task) in &snapshot.tasks {
        snapshot.roster.insert(task_id.clone(), project_task(task));
    }
    snapshot.roster_generation = snapshot.roster.len() as u64;
    snapshot.validate()?;
    diagnostics.sort_by_key(|diagnostic| format!("{diagnostic:?}"));
    Ok(MigrationReport {
        snapshot,
        diagnostics,
        source_digests,
    })
}

fn insert_session_task(
    snapshot: &mut FleetSnapshot,
    task: &TaskRecord,
    transcript_path: Option<String>,
) -> FleetResult<()> {
    let session = snapshot
        .sessions
        .entry(task.session_id.clone())
        .or_insert_with(|| SessionRecord {
            session_id: task.session_id.clone(),
            provider: task.provider,
            task_ids: Vec::new(),
            current_task_id: None,
            agent_binding: None,
            transcript_path: transcript_path.clone(),
            requested_capability_policy: Default::default(),
            resumable: true,
            created_at_unix_ms: task.started_at_unix_ms,
            updated_at_unix_ms: task.last_event_at_unix_ms,
        });
    if session.provider != task.provider {
        return Err(FleetError::InvalidState(format!(
            "session {} has conflicting task providers",
            task.session_id
        )));
    }
    session.task_ids.push(task.task_id.clone());
    let replace_current = session
        .current_task_id
        .as_ref()
        .and_then(|current| snapshot.tasks.get(current))
        .is_none_or(|current| current.started_at_unix_ms <= task.started_at_unix_ms);
    if replace_current {
        session.current_task_id = Some(task.task_id.clone());
    }
    session.created_at_unix_ms = session.created_at_unix_ms.min(task.started_at_unix_ms);
    session.updated_at_unix_ms = session.updated_at_unix_ms.max(task.last_event_at_unix_ms);
    if session.transcript_path.is_none() {
        session.transcript_path = transcript_path;
    }
    Ok(())
}

fn normalize_proof_hash(hash: &mut Option<String>, worker_id: &str) -> FleetResult<()> {
    let Some(value) = hash.as_mut() else {
        return Ok(());
    };
    let raw = value.strip_prefix("sha256:").unwrap_or(value);
    if raw.len() != 64 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(FleetError::InvalidState(format!(
            "worker {worker_id} contains a malformed proof hash"
        )));
    }
    *value = format!("sha256:{}", raw.to_ascii_lowercase());
    Ok(())
}

fn legacy_status(status: LegacyTaskStatus) -> TaskStatus {
    match status {
        LegacyTaskStatus::Running => TaskStatus::Running,
        LegacyTaskStatus::Completed => TaskStatus::Completed,
        LegacyTaskStatus::Failed => TaskStatus::Failed,
        LegacyTaskStatus::Cancelled => TaskStatus::Cancelled,
    }
}

fn infer_model(events: &[Value]) -> Option<String> {
    events.iter().find_map(|event| {
        event
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| {
                event
                    .get("message")
                    .and_then(|message| message.get("model"))
                    .and_then(Value::as_str)
            })
            .map(str::to_string)
    })
}

fn stderr_teaser(stderr: &str) -> Option<String> {
    let line = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?
        .trim();
    let mut characters = line.chars();
    let bounded: String = characters.by_ref().take(200).collect();
    Some(if characters.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    })
}

fn read_stable(path: &Path) -> FleetResult<Vec<u8>> {
    let first = fs::read(path)?;
    let second = fs::read(path)?;
    if digest_bytes(&first) != digest_bytes(&second) {
        return Err(FleetError::InvalidState(format!(
            "legacy source {} changed during migration",
            path.display()
        )));
    }
    Ok(first)
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn validate_identifier(kind: &str, value: &str) -> FleetResult<()> {
    if value.trim().is_empty() {
        return Err(FleetError::InvalidState(format!(
            "legacy {kind} identifier must not be empty"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use bro_core::WorkerId;
    use serde_json::json;

    use super::*;

    fn task_value(id: &str, session_id: &str, status: &str) -> Value {
        json!({
            "id": id,
            "provider": "glm",
            "session_id": session_id,
            "events": [],
            "last_assistant_message": null,
            "usage": null,
            "cost_usd": null,
            "num_turns": null,
            "stderr": "",
            "status": status,
            "started_at": 10,
            "completed_at": if status == "running" { Value::Null } else { json!(20) },
            "exit_code": if status == "completed" { json!(0) } else { Value::Null },
            "cwd": null
        })
    }

    fn worker_value(worker_id: &str, task_id: &str, session_id: &str, state: &str) -> Value {
        let initial = state == "awaiting_initial_connection";
        json!({
            "worker_id": worker_id,
            "task_id": task_id,
            "session_id": session_id,
            "worker_build": if initial { Value::Null } else { json!({"version":"1", "build_id":"build"}) },
            "protocol_version": if initial { Value::Null } else { json!(1) },
            "connection_generation": if initial { 0 } else { 1 },
            "bootstrap_proof_hash": if initial { json!("b".repeat(64)) } else { Value::Null },
            "reconnect_proof_hash": if initial { Value::Null } else { json!("a".repeat(64)) },
            "previous_reconnect_proof_hash": null,
            "lease": if initial { Value::Null } else { json!({
                "lease_id":"lease-1",
                "expires_at_unix_ms":200,
                "heartbeat_interval_ms":10,
                "reattach_grace_ms":30
            }) },
            "event_ack": 0,
            "next_command_seq": 1,
            "command_outcome_ack": 0,
            "pending_commands": {},
            "pending_command_outcomes": {},
            "state": state,
            "updated_at_unix_ms": 12,
            "future_worker_field": true
        })
    }

    fn workers_value(records: Vec<(&str, Value)>) -> Value {
        let records: serde_json::Map<String, Value> = records
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        json!({"version": 1, "records": records, "future_top_level": true})
    }

    fn runtime_lease_value(task_id: &str, session_id: &str) -> Value {
        json!({
            "task_id": task_id,
            "session_id": session_id,
            "provider": "glm",
            "account": "account-a",
            "model": "glm-4.7",
            "effort": "high",
            "tier": "terra",
            "durable": true,
            "capabilities": ["resume", "tool_use"],
            "project_dir": "/tmp/project",
            "cwd": "/tmp/project",
            "selection_trace_id": "trace-1",
            "created_at": 9,
            "last_seen_at": 13,
            "brofile_context": {"provider_defaults":"strict_suppress"},
            "future_lease_field": true
        })
    }

    fn write_json(path: &Path, value: &Value) {
        fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn migrate_values(
        tasks: Value,
        workers: Option<Value>,
        leases: Option<Value>,
        options: LegacyMigrationOptions,
    ) -> FleetResult<MigrationReport> {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let tasks_path = root.join("tasks.json");
        write_json(&tasks_path, &tasks);
        let workers_path = workers.map(|value| {
            let path = root.join("worker-authority.json");
            write_json(&path, &value);
            path
        });
        let leases_path = leases.map(|value| {
            let allocator = root.join("allocator");
            fs::create_dir_all(&allocator).unwrap();
            let path = allocator.join("leases.json");
            write_json(&path, &value);
            path
        });
        migrate_legacy_files(
            &tasks_path,
            workers_path.as_deref(),
            leases_path.as_deref(),
            options,
        )
    }

    #[test]
    fn full_current_files_reconcile_running_worker_and_allocation() {
        let mut task = task_value("task-1", "session-1", "running");
        let object = task.as_object_mut().unwrap();
        object.insert(
            "events".into(),
            json!([{"message":{"model":"inferred-model"}}]),
        );
        object.insert("last_assistant_message".into(), json!("still working"));
        object.insert(
            "usage".into(),
            json!({
                "input_tokens":10,
                "output_tokens":3,
                "cached_input_tokens":2,
                "cache_creation_input_tokens":1
            }),
        );
        object.insert("cost_usd".into(), json!(0.2));
        object.insert("num_turns".into(), json!(4));
        object.insert("stderr".into(), json!("old\nlatest error"));
        object.insert("managed_worktree".into(), json!("/tmp/worktree"));
        object.insert("bro_label".into(), json!("team::worker"));
        object.insert("name".into(), json!("migration probe"));
        object.insert("agent_label".into(), json!("agent:test@v1"));
        object.insert(
            "report".into(),
            json!({"message":"progress", "needs":"review", "data":{"n":1}, "reportedAt":11}),
        );
        object.insert("interrupted".into(), json!(false));
        object.insert("recoverable".into(), json!(false));
        object.insert(
            "transcript_location".into(),
            json!({
                "provider":"glm",
                "storage":"jsonl_file",
                "path":"/tmp/session.events.jsonl",
                "account":null,
                "session_id":"session-1",
                "project":null,
                "cwd":"/tmp/worktree",
                "is_subagent":false
            }),
        );
        object.insert(
            "transcript_cursor".into(),
            json!({"type":"byte_offset", "offset":42}),
        );
        object.insert("live_cursor".into(), json!(7));
        object.insert(
            "supervision".into(),
            json!({"last_event_at_ms":14, "future_supervision_field":true}),
        );
        object.insert("origin".into(), json!("workflow"));
        object.insert("workflow_owned".into(), Value::Null);
        object.insert("future_task_field".into(), json!(true));

        let report = migrate_values(
            json!([task]),
            Some(workers_value(vec![(
                "worker-1",
                worker_value("worker-1", "task-1", "session-1", "active"),
            )])),
            Some(json!({
                "leases": {"task-1": runtime_lease_value("task-1", "session-1")},
                "future_store_field": true
            })),
            LegacyMigrationOptions {
                now_unix_ms: 100,
                retain_tasks_started_at_or_after: None,
            },
        )
        .unwrap();
        let task = report.snapshot.tasks.get(&TaskId::new("task-1")).unwrap();
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.model.as_deref(), Some("inferred-model"));
        assert_eq!(
            task.transcript_path.as_deref(),
            Some("/tmp/session.events.jsonl")
        );
        assert_eq!(
            task.transcript_cursor,
            Some(json!({"type":"byte_offset", "offset":42}))
        );
        assert_eq!(task.error_teaser.as_deref(), Some("latest error"));
        assert!(task.workflow_owned);
        assert_eq!(task.last_event_at_unix_ms, 14);
        let worker = report
            .snapshot
            .workers
            .get(&WorkerId::new("worker-1"))
            .unwrap();
        assert_eq!(worker.state, WorkerAuthorityState::AwaitingReattach);
        assert_eq!(
            worker.reconnect_proof_hash.as_deref(),
            Some(format!("sha256:{}", "a".repeat(64)).as_str())
        );
        let lease = report
            .snapshot
            .runtime_leases
            .get(&TaskId::new("task-1"))
            .unwrap();
        assert_eq!(lease.account.as_deref(), Some("account-a"));
        assert_eq!(lease.effort.as_deref(), Some("high"));
        assert_eq!(
            lease.provider_defaults,
            Some(ProviderDefaultsPolicy::StrictSuppress)
        );
        assert!(report.snapshot.attempts.is_empty());
        assert!(report.snapshot.admissions.is_empty());
        assert!(report.snapshot.worktrees.is_empty());
        assert_eq!(report.snapshot.roster.len(), 1);
        assert!(report.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            MigrationDiagnostic::ActiveWorkerMadeReconnectable { .. }
        )));
    }

    #[test]
    fn running_task_without_live_worker_becomes_recoverable_failure() {
        let report = migrate_values(
            json!([task_value("task-1", "session-1", "running")]),
            None,
            None,
            LegacyMigrationOptions {
                now_unix_ms: 55,
                ..LegacyMigrationOptions::default()
            },
        )
        .unwrap();
        let task = &report.snapshot.tasks[&TaskId::new("task-1")];
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(task.recoverable);
        assert_eq!(task.completed_at_unix_ms, Some(55));
        assert_eq!(task.last_event_at_unix_ms, 55);
    }

    #[test]
    fn awaiting_initial_worker_preserves_running_status_and_bootstrap_hash() {
        let report = migrate_values(
            json!([task_value("task-1", "session-1", "running")]),
            Some(workers_value(vec![(
                "worker-1",
                worker_value(
                    "worker-1",
                    "task-1",
                    "session-1",
                    "awaiting_initial_connection",
                ),
            )])),
            None,
            LegacyMigrationOptions::default(),
        )
        .unwrap();
        assert_eq!(
            report.snapshot.tasks[&TaskId::new("task-1")].status,
            TaskStatus::Running
        );
        let worker = &report.snapshot.workers[&WorkerId::new("worker-1")];
        assert_eq!(
            worker.state,
            WorkerAuthorityState::AwaitingInitialConnection
        );
        assert!(
            worker
                .bootstrap_proof_hash
                .as_deref()
                .unwrap()
                .starts_with("sha256:")
        );
    }

    #[test]
    fn terminal_task_closes_a_split_brain_live_worker() {
        let report = migrate_values(
            json!([task_value("task-1", "session-1", "completed")]),
            Some(workers_value(vec![(
                "worker-1",
                worker_value("worker-1", "task-1", "session-1", "draining"),
            )])),
            None,
            LegacyMigrationOptions {
                now_unix_ms: 99,
                ..LegacyMigrationOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            report.snapshot.workers[&WorkerId::new("worker-1")].state,
            WorkerAuthorityState::Terminal
        );
        assert!(report.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            MigrationDiagnostic::TerminalTaskClosedWorker { .. }
        )));
    }

    #[test]
    fn orphan_worker_is_recovered_only_from_matching_runtime_lease() {
        let workers = workers_value(vec![(
            "worker-1",
            worker_value("worker-1", "task-1", "session-1", "active"),
        )]);
        let recovered = migrate_values(
            json!([]),
            Some(workers.clone()),
            Some(json!({
                "leases":{"task-1":runtime_lease_value("task-1", "session-1")}
            })),
            LegacyMigrationOptions {
                now_unix_ms: 50,
                ..LegacyMigrationOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            recovered.snapshot.tasks[&TaskId::new("task-1")].status,
            TaskStatus::Running
        );
        assert!(recovered.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            MigrationDiagnostic::WorkerRecoveredFromRuntimeLease { .. }
        )));

        let error = migrate_values(
            json!([]),
            Some(workers),
            None,
            LegacyMigrationOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(error, FleetError::InvalidState(_)));
    }

    #[test]
    fn workflow_ownership_defaults_only_when_legacy_value_is_absent_or_null() {
        let mut missing = task_value("task-missing", "session-1", "completed");
        missing["origin"] = json!("workflow");
        let mut null_value = task_value("task-null", "session-1", "completed");
        null_value["origin"] = json!("atom");
        null_value["workflow_owned"] = Value::Null;
        let mut explicit_false = task_value("task-false", "session-1", "completed");
        explicit_false["origin"] = json!("workflow");
        explicit_false["workflow_owned"] = json!(false);
        let report = migrate_values(
            json!([missing, null_value, explicit_false]),
            None,
            None,
            LegacyMigrationOptions::default(),
        )
        .unwrap();
        assert!(report.snapshot.tasks[&TaskId::new("task-missing")].workflow_owned);
        assert!(report.snapshot.tasks[&TaskId::new("task-null")].workflow_owned);
        assert!(!report.snapshot.tasks[&TaskId::new("task-false")].workflow_owned);
    }

    #[test]
    fn retention_is_opt_in_and_does_not_fabricate_attempts() {
        let task = task_value("task-old", "session-old", "completed");
        let retained = migrate_values(
            json!([task.clone()]),
            None,
            None,
            LegacyMigrationOptions::default(),
        )
        .unwrap();
        assert!(
            retained
                .snapshot
                .tasks
                .contains_key(&TaskId::new("task-old"))
        );
        let dropped = migrate_values(
            json!([task]),
            None,
            None,
            LegacyMigrationOptions {
                now_unix_ms: 100,
                retain_tasks_started_at_or_after: Some(11),
            },
        )
        .unwrap();
        assert!(dropped.snapshot.tasks.is_empty());
        assert!(dropped.snapshot.attempts.is_empty());
        assert!(matches!(
            dropped.diagnostics.as_slice(),
            [MigrationDiagnostic::TaskDroppedByRetention { .. }]
        ));
    }

    #[test]
    fn task_reader_fails_closed_on_partial_corruption_and_duplicate_ids() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("tasks.json");
        fs::write(&path, b"not json").unwrap();
        assert!(matches!(
            read_legacy_tasks(&path),
            Err(FleetError::Serialization(_))
        ));
        write_json(
            &path,
            &json!([
                task_value("task-1", "session-1", "completed"),
                {"id":"broken"}
            ]),
        );
        assert!(matches!(
            read_legacy_tasks(&path),
            Err(FleetError::Serialization(_))
        ));
        write_json(
            &path,
            &json!([
                task_value("task-1", "session-1", "completed"),
                task_value("task-1", "session-1", "failed")
            ]),
        );
        assert!(matches!(
            read_legacy_tasks(&path),
            Err(FleetError::InvalidState(_))
        ));
        let mut empty = task_value("", "session-1", "completed");
        empty["future"] = json!(true);
        write_json(&path, &json!([empty]));
        assert!(matches!(
            read_legacy_tasks(&path),
            Err(FleetError::InvalidState(_))
        ));
    }

    #[test]
    fn worker_reader_rejects_versions_identity_and_proof_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("worker-authority.json");
        write_json(&path, &json!({"version":2,"records":{}}));
        assert!(matches!(
            read_legacy_worker_authority(&path),
            Err(FleetError::UnsupportedLegacyWorkerVersion(2))
        ));

        write_json(
            &path,
            &workers_value(vec![(
                "wrong-key",
                worker_value("worker-1", "task-1", "session-1", "active"),
            )]),
        );
        assert!(matches!(
            read_legacy_worker_authority(&path),
            Err(FleetError::InvalidState(_))
        ));

        let mut malformed = worker_value("worker-1", "task-1", "session-1", "active");
        malformed["reconnect_proof_hash"] = json!("raw-secret");
        write_json(&path, &workers_value(vec![("worker-1", malformed)]));
        assert!(matches!(
            read_legacy_worker_authority(&path),
            Err(FleetError::InvalidState(_))
        ));
    }

    #[test]
    fn worker_reader_rejects_command_gaps_and_outcome_identity_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("worker-authority.json");
        let mut worker = worker_value("worker-1", "task-1", "session-1", "active");
        worker["next_command_seq"] = json!(3);
        worker["pending_commands"] = json!({
            "1":{"command_seq":1,"command_id":"command-1","command":{"type":"interrupt"}}
        });
        write_json(&path, &workers_value(vec![("worker-1", worker.clone())]));
        assert!(matches!(
            read_legacy_worker_authority(&path),
            Err(FleetError::InvalidState(_))
        ));

        worker["pending_commands"] = json!({
            "1":{"command_seq":1,"command_id":"command-1","command":{"type":"interrupt"}},
            "2":{"command_seq":2,"command_id":"command-2","command":{"type":"compact"}}
        });
        worker["pending_command_outcomes"] = json!({
            "2":{
                "command_seq":2,
                "command_id":"wrong-command",
                "accepted":true,
                "terminal":true,
                "result_or_error":null
            }
        });
        write_json(&path, &workers_value(vec![("worker-1", worker)]));
        assert!(matches!(
            read_legacy_worker_authority(&path),
            Err(FleetError::InvalidState(_))
        ));
    }

    #[test]
    fn multiple_live_workers_and_cross_file_provider_conflicts_fail_closed() {
        let workers = workers_value(vec![
            (
                "worker-1",
                worker_value("worker-1", "task-1", "session-1", "active"),
            ),
            (
                "worker-2",
                worker_value("worker-2", "task-1", "session-1", "awaiting_reattach"),
            ),
        ]);
        assert!(matches!(
            migrate_values(
                json!([task_value("task-1", "session-1", "running")]),
                Some(workers),
                None,
                LegacyMigrationOptions::default()
            ),
            Err(FleetError::InvalidState(_))
        ));

        let mut conflicting_lease = runtime_lease_value("task-1", "session-1");
        conflicting_lease["provider"] = json!("deepseek");
        assert!(matches!(
            migrate_values(
                json!([task_value("task-1", "session-1", "completed")]),
                None,
                Some(json!({"leases":{"task-1":conflicting_lease}})),
                LegacyMigrationOptions::default()
            ),
            Err(FleetError::InvalidState(_))
        ));
    }

    #[test]
    fn runtime_lease_reader_rejects_map_key_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("leases.json");
        write_json(
            &path,
            &json!({"leases":{"wrong":runtime_lease_value("task-1", "session-1")}}),
        );
        assert!(matches!(
            read_legacy_runtime_leases(&path),
            Err(FleetError::InvalidState(_))
        ));
    }

    #[test]
    fn identical_imports_produce_byte_equivalent_authority_state() {
        let tasks = json!([task_value("task-1", "session-1", "running")]);
        let workers = workers_value(vec![(
            "worker-1",
            worker_value("worker-1", "task-1", "session-1", "active"),
        )]);
        let options = LegacyMigrationOptions {
            now_unix_ms: 100,
            retain_tasks_started_at_or_after: None,
        };
        let first =
            migrate_values(tasks.clone(), Some(workers.clone()), None, options.clone()).unwrap();
        let second = migrate_values(tasks, Some(workers), None, options).unwrap();
        assert_eq!(first.snapshot, second.snapshot);
        assert_eq!(first.diagnostics, second.diagnostics);
        assert_eq!(
            serde_json::to_vec(&first.snapshot).unwrap(),
            serde_json::to_vec(&second.snapshot).unwrap()
        );
    }

    #[test]
    fn stale_legacy_temporary_files_are_reported_but_never_promoted() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let tasks_path = root.join("tasks.json");
        let workers_path = root.join("worker-authority.json");
        write_json(
            &tasks_path,
            &json!([task_value("task-good", "session-good", "completed")]),
        );
        write_json(&workers_path, &workers_value(Vec::new()));
        write_json(
            &root.join("tasks.json.tmp"),
            &json!([task_value("task-stale", "session-stale", "completed")]),
        );
        write_json(
            &root.join(".worker-authority.crash.tmp"),
            &workers_value(vec![(
                "worker-stale",
                worker_value(
                    "worker-stale",
                    "task-stale",
                    "session-stale",
                    "awaiting_initial_connection",
                ),
            )]),
        );
        let report = migrate_legacy_files(
            &tasks_path,
            Some(&workers_path),
            None,
            LegacyMigrationOptions::default(),
        )
        .unwrap();
        assert!(
            report
                .snapshot
                .tasks
                .contains_key(&TaskId::new("task-good"))
        );
        assert!(
            !report
                .snapshot
                .tasks
                .contains_key(&TaskId::new("task-stale"))
        );
        assert_eq!(
            report
                .diagnostics
                .iter()
                .filter(|diagnostic| matches!(
                    diagnostic,
                    MigrationDiagnostic::StaleTemporaryFileIgnored { .. }
                ))
                .count(),
            2
        );
    }
}
