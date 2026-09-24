//! Durable checkout-owner project render operations.
//!
//! A project render that no bound workspace applies is executed by the code
//! collector that owns the checkout. The daemon selects the view and builds
//! the plan; this runtime persists each operation and its immutable plan
//! bytes before delivery, offers `render_project` commands only to a
//! producer that polls the render lane, pages the persisted plan, and records
//! the owner's exact receipt.
//!
//! Every fresh render is a new operation with its own id and a per-project
//! sequence. A newer operation supersedes older pending ones, so a delayed
//! owner never applies an older plan over newer output. Recovery retrieves
//! the recorded receipt of one named operation; it never re-delivers a
//! settled one, and a historical receipt is never reported as current after
//! a newer operation for the same project exists.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_project_render::transport::{
    ProjectRenderPlanV1, ProjectRenderReceiptV1, ProjectRenderViewV1, format_render_operation_id,
    transport_chunk_of, validate_render_operation_id,
};
use bbox_project_render::wire::{
    MAX_RENDER_OPERATIONS_PER_POLL, RENDER_LANE_SCHEMA_VERSION, RENDER_PROJECT_COMMAND_KIND,
    RenderLanePollRequestV1, RenderLanePollResponseV1, RenderOperationDeliveryV1,
    RenderOperationErrorV1,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::producer_commands::{
    PRODUCER_COMMAND_REDELIVERY_SECS, PRODUCER_PRESENCE_FRESH_SECS, ProducerCommandClock,
    SystemProducerCommandClock,
};

const INDEX_VERSION: u32 = 1;
const INDEX_FILE: &str = "index.json";
const PLANS_DIR: &str = "plans";
/// Settled records retained for explicit recovery, oldest dropped first.
const MAX_RETAINED_OPERATIONS: usize = 512;
/// Pending operations across all projects; a new render beyond it refuses.
const MAX_PENDING_OPERATIONS: usize = 256;
const MAX_INDEX_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const DEFAULT_RENDER_WAIT: Duration = Duration::from_secs(20);

/// Settlement of one operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum RenderOperationState {
    /// Plan persisted; awaiting the owner.
    Pending,
    /// A newer operation for the project was issued before this one
    /// settled. It is no longer delivered.
    Superseded { by: String },
    /// The owner applied the plan and returned this exact receipt.
    Completed {
        receipt: ProjectRenderReceiptV1,
        validation: RenderCompletionValidation,
        /// A newer operation already existed when this receipt arrived.
        #[serde(default)]
        late: bool,
    },
    /// The owner refused or could not apply the plan; it wrote nothing.
    Failed { error: RenderOperationErrorV1 },
}

/// Whether a completed receipt still matches the daemon's current plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum RenderCompletionValidation {
    /// Not yet compared with the current plan.
    Unverified,
    /// The current plan and owner authority were unchanged at completion.
    Current,
    /// Authority or knowledge changed after the plan was issued.
    Stale { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RenderOperationRecord {
    pub operation_id: String,
    pub producer_id: String,
    pub project_id: String,
    pub scope: PublishedScope,
    pub sequence: u64,
    /// Daemon-clock issuance carried in the plan's producer authority.
    pub issued_at_ms: u64,
    pub plan_sha256: String,
    pub plan_bytes: usize,
    pub provider: Option<String>,
    pub dry_run: bool,
    pub view: ProjectRenderViewV1,
    pub requested_scope: String,
    pub created_at_unix_secs: u64,
    #[serde(default)]
    pub delivered_at_unix_secs: Option<u64>,
    #[serde(default)]
    pub settled_at_unix_secs: Option<u64>,
    #[serde(flatten)]
    pub state: RenderOperationState,
}

impl RenderOperationRecord {
    pub(crate) fn is_pending(&self) -> bool {
        matches!(self.state, RenderOperationState::Pending)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RenderOperationIndex {
    version: u32,
    /// Last sequence issued per project.
    #[serde(default)]
    sequences: BTreeMap<String, u64>,
    operations: Vec<RenderOperationRecord>,
}

impl Default for RenderOperationIndex {
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            sequences: BTreeMap::new(),
            operations: Vec::new(),
        }
    }
}

/// What one producer's latest render-lane poll proved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderLanePresence {
    pub producer_id: String,
    pub supports_current_transport: bool,
    pub covered_scopes: BTreeSet<PublishedScope>,
    pub collector_version: String,
    pub fresh: bool,
}

#[derive(Debug, Clone)]
struct LaneRecord {
    supports_current_transport: bool,
    covered_scopes: BTreeSet<PublishedScope>,
    collector_version: String,
    last_seen_secs: u64,
}

/// Why an owner result was not recorded as a fresh settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderResultStatus {
    Accepted,
    AlreadySettled,
    Superseded,
}

impl RenderResultStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::AlreadySettled => "already_settled",
            Self::Superseded => "superseded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenderLaneError {
    UnknownOperation,
    WrongProducer,
    Settled,
    Conflict(String),
    Invalid(String),
}

pub(crate) struct NewRenderOperation<'a> {
    pub producer_id: &'a str,
    pub project_id: &'a str,
    pub scope: &'a PublishedScope,
    pub provider: Option<String>,
    pub dry_run: bool,
    pub view: ProjectRenderViewV1,
    pub requested_scope: String,
}

pub(crate) struct RenderOperationRuntime {
    root: Option<PathBuf>,
    state: Mutex<RuntimeState>,
    clock: Arc<dyn ProducerCommandClock>,
    settled: watch::Sender<u64>,
    wait: Mutex<Duration>,
}

struct RuntimeState {
    index: RenderOperationIndex,
    /// Plan bytes of pending operations without a durable root.
    memory_plans: BTreeMap<String, Vec<u8>>,
    lanes: BTreeMap<String, LaneRecord>,
}

impl RenderOperationRuntime {
    /// Open the durable operation store under `root`.
    pub(crate) fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root.join(PLANS_DIR))
            .with_context(|| format!("creating {}", root.display()))?;
        let index = load_index(&root.join(INDEX_FILE))?;
        Ok(Self::with_parts(
            Some(root.to_path_buf()),
            index,
            Arc::new(SystemProducerCommandClock),
        ))
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> Self {
        Self::with_parts(
            None,
            RenderOperationIndex::default(),
            Arc::new(SystemProducerCommandClock),
        )
    }

    #[cfg(test)]
    pub(crate) fn open_with_clock(root: &Path, clock: Arc<dyn ProducerCommandClock>) -> Self {
        std::fs::create_dir_all(root.join(PLANS_DIR)).unwrap();
        let index = load_index(&root.join(INDEX_FILE)).unwrap();
        Self::with_parts(Some(root.to_path_buf()), index, clock)
    }

    fn with_parts(
        root: Option<PathBuf>,
        index: RenderOperationIndex,
        clock: Arc<dyn ProducerCommandClock>,
    ) -> Self {
        let (settled, _) = watch::channel(0);
        Self {
            root,
            state: Mutex::new(RuntimeState {
                index,
                memory_plans: BTreeMap::new(),
                lanes: BTreeMap::new(),
            }),
            clock,
            settled,
            wait: Mutex::new(DEFAULT_RENDER_WAIT),
        }
    }

    pub(crate) fn wait_timeout(&self) -> Duration {
        *self.wait.lock()
    }

    #[cfg(test)]
    pub(crate) fn set_wait_timeout_for_test(&self, wait: Duration) {
        *self.wait.lock() = wait;
    }

    #[cfg(test)]
    pub(crate) fn age_lane_for_test(&self, producer_id: &str, age_secs: u64) {
        let now = self.clock.now_secs();
        if let Some(lane) = self.state.lock().lanes.get_mut(producer_id) {
            lane.last_seen_secs = now.saturating_sub(age_secs);
        }
    }

    /// The render-lane presence of one producer, if it ever polled.
    pub(crate) fn lane(&self, producer_id: &str) -> Option<RenderLanePresence> {
        let now = self.clock.now_secs();
        self.state
            .lock()
            .lanes
            .get(producer_id)
            .map(|lane| RenderLanePresence {
                producer_id: producer_id.to_string(),
                supports_current_transport: lane.supports_current_transport,
                covered_scopes: lane.covered_scopes.clone(),
                collector_version: lane.collector_version.clone(),
                fresh: now.saturating_sub(lane.last_seen_secs) <= PRODUCER_PRESENCE_FRESH_SECS,
            })
    }

    /// Record one authenticated render-lane poll and hand back pending
    /// operations this producer owns. `authorized` is the producer's current
    /// scope-to-project grant; an operation whose scope left the grant, or
    /// that the collector no longer reports holding, is not delivered.
    pub(crate) fn poll(
        &self,
        producer_id: &str,
        request: &RenderLanePollRequestV1,
        authorized: &BTreeMap<PublishedScope, String>,
    ) -> Result<RenderLanePollResponseV1> {
        let now = self.clock.now_secs();
        let covered: BTreeSet<PublishedScope> = request.covered_scopes.iter().cloned().collect();
        let mut state = self.state.lock();
        state.lanes.insert(
            producer_id.to_string(),
            LaneRecord {
                supports_current_transport: request.supports_current_transport(),
                covered_scopes: covered.clone(),
                collector_version: request.collector_version.clone(),
                last_seen_secs: now,
            },
        );
        if !request.supports_current_transport() {
            return Ok(RenderLanePollResponseV1 {
                schema_version: RENDER_LANE_SCHEMA_VERSION,
                operations: Vec::new(),
            });
        }
        let mut next = state.index.clone();
        let mut operations = Vec::new();
        let mut candidates = next
            .operations
            .iter_mut()
            .filter(|record| {
                record.is_pending()
                    && record.producer_id == producer_id
                    && covered.contains(&record.scope)
                    && authorized.get(&record.scope) == Some(&record.project_id)
                    && record.delivered_at_unix_secs.is_none_or(|delivered| {
                        now.saturating_sub(delivered) >= PRODUCER_COMMAND_REDELIVERY_SECS
                    })
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|record| record.sequence);
        for record in candidates.into_iter().take(MAX_RENDER_OPERATIONS_PER_POLL) {
            record.delivered_at_unix_secs = Some(now);
            operations.push(RenderOperationDeliveryV1 {
                operation_id: record.operation_id.clone(),
                kind: RENDER_PROJECT_COMMAND_KIND.to_string(),
                scope: record.scope.clone(),
                sequence: record.sequence,
                plan_sha256: record.plan_sha256.clone(),
                plan_bytes: record.plan_bytes,
            });
        }
        if !operations.is_empty() {
            self.persist_index(&next)?;
            state.index = next;
        }
        Ok(RenderLanePollResponseV1 {
            schema_version: RENDER_LANE_SCHEMA_VERSION,
            operations,
        })
    }

    /// Mint the id and sequence of a new operation. The caller builds the
    /// plan with them and hands it to [`Self::create`], which rechecks the
    /// sequence against concurrent renders of the same project.
    pub(crate) fn reserve(&self, project_id: &str) -> Result<(String, u64)> {
        let state = self.state.lock();
        let pending = state
            .index
            .operations
            .iter()
            .filter(|record| record.is_pending())
            .count();
        if pending >= MAX_PENDING_OPERATIONS {
            bail!(
                "error.render_operations_full: {pending} checkout-owner renders are still pending; retry after owners settle them"
            );
        }
        let last = state.index.sequences.get(project_id).copied().unwrap_or(0);
        // Sequences stay monotonic even if this store is lost: the owner
        // compares them against the newest operation it applied.
        let now_millis = self.clock.now_secs().saturating_mul(1000);
        let sequence = last.saturating_add(1).max(now_millis);
        let operation_id = format_render_operation_id(uuid::Uuid::new_v4().as_u128());
        Ok((operation_id, sequence))
    }

    /// Persist a reserved operation with its immutable plan bytes. Older
    /// pending operations for the same project become superseded.
    pub(crate) fn create(
        &self,
        request: NewRenderOperation<'_>,
        plan: &ProjectRenderPlanV1,
    ) -> Result<RenderOperationRecord> {
        let producer = plan
            .producer
            .as_ref()
            .context("checkout-owner render plan has no producer authority")?;
        let (bytes, plan_sha256) = plan.transport_bytes_and_sha256()?;
        if producer.producer_id != request.producer_id
            || plan.project_id != request.project_id
            || &plan.scope != request.scope
        {
            bail!("checkout-owner render plan does not match its operation");
        }
        let now = self.clock.now_secs();
        let record = RenderOperationRecord {
            operation_id: producer.operation_id.clone(),
            producer_id: request.producer_id.to_string(),
            project_id: request.project_id.to_string(),
            scope: request.scope.clone(),
            sequence: producer.sequence,
            issued_at_ms: producer.issued_at_ms,
            plan_sha256,
            plan_bytes: bytes.len(),
            provider: request.provider,
            dry_run: request.dry_run,
            view: request.view,
            requested_scope: request.requested_scope,
            created_at_unix_secs: now,
            delivered_at_unix_secs: None,
            settled_at_unix_secs: None,
            state: RenderOperationState::Pending,
        };
        let mut state = self.state.lock();
        let last = state
            .index
            .sequences
            .get(&record.project_id)
            .copied()
            .unwrap_or(0);
        if record.sequence <= last {
            bail!("error.render_plan_stale: a newer render of this project was issued first");
        }
        if let Some(root) = &self.root {
            write_plan(root, &record.operation_id, &bytes)?;
        } else {
            state
                .memory_plans
                .insert(record.operation_id.clone(), bytes);
        }
        let mut next = state.index.clone();
        next.sequences
            .insert(record.project_id.clone(), record.sequence);
        let mut superseded = Vec::new();
        for older in next.operations.iter_mut() {
            if older.project_id == record.project_id && older.is_pending() {
                older.state = RenderOperationState::Superseded {
                    by: record.operation_id.clone(),
                };
                older.settled_at_unix_secs = Some(now);
                superseded.push(older.operation_id.clone());
            }
        }
        next.operations.push(record.clone());
        let dropped = retain(&mut next);
        self.persist_index(&next)?;
        state.index = next;
        for operation_id in superseded.iter().chain(&dropped) {
            self.drop_plan(&mut state, operation_id);
        }
        drop(state);
        self.notify();
        Ok(record)
    }

    pub(crate) fn record(&self, operation_id: &str) -> Option<RenderOperationRecord> {
        self.state
            .lock()
            .index
            .operations
            .iter()
            .find(|record| record.operation_id == operation_id)
            .cloned()
    }

    /// Run `record` only if `sequence` is still the newest operation issued
    /// for the project. Operation creation waits while it runs, so a newer
    /// operation cannot be issued between the check and the effect.
    pub(crate) fn while_latest<T>(
        &self,
        project_id: &str,
        sequence: u64,
        record: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        let state = self.state.lock();
        if state.index.sequences.get(project_id) != Some(&sequence) {
            return Ok(None);
        }
        let result = record().map(Some);
        drop(state);
        result
    }

    /// The newest issued sequence for a project.
    pub(crate) fn latest_sequence(&self, project_id: &str) -> Option<u64> {
        self.state.lock().index.sequences.get(project_id).copied()
    }

    /// Page the persisted plan of a pending operation to its owner.
    pub(crate) fn plan_page(
        &self,
        producer_id: &str,
        operation_id: &str,
        offset: usize,
        authorized: &BTreeMap<PublishedScope, String>,
    ) -> std::result::Result<
        bbox_project_render::transport::ProjectRenderPlanChunkV1,
        RenderLaneError,
    > {
        validate_render_operation_id(operation_id)
            .map_err(|error| RenderLaneError::Invalid(error.to_string()))?;
        let state = self.state.lock();
        let record = state
            .index
            .operations
            .iter()
            .find(|record| record.operation_id == operation_id)
            .ok_or(RenderLaneError::UnknownOperation)?;
        if record.producer_id != producer_id
            || authorized.get(&record.scope) != Some(&record.project_id)
        {
            return Err(RenderLaneError::WrongProducer);
        }
        if !record.is_pending() {
            return Err(RenderLaneError::Settled);
        }
        let bytes = match &self.root {
            Some(root) => read_plan(root, operation_id)
                .map_err(|error| RenderLaneError::Invalid(format!("{error:#}")))?,
            None => state
                .memory_plans
                .get(operation_id)
                .cloned()
                .ok_or(RenderLaneError::UnknownOperation)?,
        };
        transport_chunk_of(&bytes, &record.plan_sha256, offset, None)
            .map_err(|error| RenderLaneError::Invalid(error.to_string()))
    }

    /// The immutable plan issued for one operation, while it is retained.
    pub(crate) fn issued_plan(&self, operation_id: &str) -> Result<ProjectRenderPlanV1> {
        let bytes = {
            let state = self.state.lock();
            match &self.root {
                Some(root) => read_plan(root, operation_id)?,
                None => state
                    .memory_plans
                    .get(operation_id)
                    .cloned()
                    .context("render operation plan is no longer retained")?,
            }
        };
        let plan: ProjectRenderPlanV1 = serde_json::from_slice(&bytes)?;
        plan.validate()?;
        Ok(plan)
    }

    /// Record the owner's terminal result for one delivered operation.
    pub(crate) fn settle(
        &self,
        producer_id: &str,
        operation_id: &str,
        plan_sha256: &str,
        authorized: &BTreeMap<PublishedScope, String>,
        result: std::result::Result<ProjectRenderReceiptV1, RenderOperationErrorV1>,
    ) -> std::result::Result<RenderResultStatus, RenderLaneError> {
        let now = self.clock.now_secs();
        let mut state = self.state.lock();
        let Some(position) = state
            .index
            .operations
            .iter()
            .position(|record| record.operation_id == operation_id)
        else {
            return Err(RenderLaneError::UnknownOperation);
        };
        let record = state.index.operations[position].clone();
        if record.producer_id != producer_id
            || authorized.get(&record.scope) != Some(&record.project_id)
        {
            return Err(RenderLaneError::WrongProducer);
        }
        if record.plan_sha256 != plan_sha256 {
            return Err(RenderLaneError::Conflict(
                "result names a different plan than the operation issued".into(),
            ));
        }
        if let Ok(receipt) = &result {
            let plan = match &self.root {
                Some(root) => read_plan(root, operation_id).ok(),
                None => state.memory_plans.get(operation_id).cloned(),
            }
            .map(|bytes| serde_json::from_slice::<ProjectRenderPlanV1>(&bytes));
            match plan {
                Some(Ok(plan)) => receipt
                    .validate_against(&plan)
                    .map_err(|error| RenderLaneError::Invalid(format!("{error:#}")))?,
                Some(Err(error)) => return Err(RenderLaneError::Invalid(error.to_string())),
                // The plan is gone only once the operation settled; the
                // settled branches below decide.
                None if record.is_pending() => {
                    return Err(RenderLaneError::Invalid(
                        "render operation plan is no longer retained".into(),
                    ));
                }
                None => {}
            }
        }
        let late = !record.is_pending();
        let state_value = match (&record.state, &result) {
            (RenderOperationState::Pending, _) | (RenderOperationState::Superseded { .. }, _) => {
                match result {
                    Ok(receipt) => RenderOperationState::Completed {
                        receipt,
                        validation: if late {
                            RenderCompletionValidation::Stale {
                                reason: "a newer render of this project was issued before this receipt arrived".into(),
                            }
                        } else {
                            RenderCompletionValidation::Unverified
                        },
                        late,
                    },
                    Err(error) => {
                        if late {
                            return Ok(RenderResultStatus::Superseded);
                        }
                        RenderOperationState::Failed { error }
                    }
                }
            }
            (RenderOperationState::Completed { receipt, .. }, Ok(submitted))
                if receipt == submitted =>
            {
                return Ok(RenderResultStatus::AlreadySettled);
            }
            (RenderOperationState::Failed { error }, Err(submitted)) if error == submitted => {
                return Ok(RenderResultStatus::AlreadySettled);
            }
            _ => {
                return Err(RenderLaneError::Conflict(
                    "operation already settled with a different result".into(),
                ));
            }
        };
        let mut next = state.index.clone();
        next.operations[position].state = state_value;
        next.operations[position].settled_at_unix_secs = Some(now);
        self.persist_index(&next)
            .map_err(|error| RenderLaneError::Invalid(format!("{error:#}")))?;
        state.index = next;
        self.drop_plan(&mut state, operation_id);
        drop(state);
        self.notify();
        Ok(if late {
            RenderResultStatus::Superseded
        } else {
            RenderResultStatus::Accepted
        })
    }

    /// Record the daemon's revalidation of a completed receipt.
    pub(crate) fn set_validation(
        &self,
        operation_id: &str,
        validation: RenderCompletionValidation,
    ) -> Result<()> {
        let mut state = self.state.lock();
        let mut next = state.index.clone();
        let Some(record) = next
            .operations
            .iter_mut()
            .find(|record| record.operation_id == operation_id)
        else {
            bail!("render operation {operation_id} is no longer retained");
        };
        match &mut record.state {
            RenderOperationState::Completed {
                validation: current,
                ..
            } if *current == RenderCompletionValidation::Unverified => *current = validation,
            _ => return Ok(()),
        }
        self.persist_index(&next)?;
        state.index = next;
        Ok(())
    }

    /// Wait until the operation leaves `Pending` or the timeout elapses.
    pub(crate) async fn wait_settled(
        &self,
        operation_id: &str,
        timeout: Duration,
    ) -> Option<RenderOperationRecord> {
        let mut receiver = self.settled.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.record(operation_id) {
                Some(record) if !record.is_pending() => return Some(record),
                None => return None,
                Some(_) => {}
            }
            if tokio::time::timeout_at(deadline, receiver.changed())
                .await
                .is_err()
            {
                return self.record(operation_id);
            }
        }
    }

    fn notify(&self) {
        self.settled.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }

    fn drop_plan(&self, state: &mut RuntimeState, operation_id: &str) {
        state.memory_plans.remove(operation_id);
        if let Some(root) = &self.root {
            let path = root.join(PLANS_DIR).join(format!("{operation_id}.json"));
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(operation_id, error = %error, "render operation plan cleanup failed");
            }
        }
    }

    fn persist_index(&self, index: &RenderOperationIndex) -> Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let bytes = serde_json::to_vec(index)?;
        if bytes.len() > MAX_INDEX_BYTES {
            bail!("render operation index exceeds its byte bound");
        }
        bbox_corpus_core::json_store::atomic_write_bytes_locked(&root.join(INDEX_FILE), &bytes)?;
        sync_directory(root)
    }
}

/// Drop the oldest settled records beyond the retention bound. Pending
/// records are never dropped.
fn retain(index: &mut RenderOperationIndex) -> Vec<String> {
    let mut dropped = Vec::new();
    while index.operations.len() > MAX_RETAINED_OPERATIONS {
        let Some(position) = index
            .operations
            .iter()
            .position(|record| !record.is_pending())
        else {
            break;
        };
        dropped.push(index.operations.remove(position).operation_id);
    }
    dropped
}

fn load_index(path: &Path) -> Result<RenderOperationIndex> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.len() > MAX_INDEX_BYTES {
                bail!("render operation index exceeds its byte bound");
            }
            let index: RenderOperationIndex = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            if index.version != INDEX_VERSION {
                bail!(
                    "unsupported render operation index version {}",
                    index.version
                );
            }
            for record in &index.operations {
                validate_render_operation_id(&record.operation_id)?;
            }
            Ok(index)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_plan(root: &Path, operation_id: &str, bytes: &[u8]) -> Result<()> {
    validate_render_operation_id(operation_id)?;
    let path = root.join(PLANS_DIR).join(format!("{operation_id}.json"));
    bbox_corpus_core::json_store::atomic_write_bytes_locked(&path, bytes)?;
    sync_directory(&root.join(PLANS_DIR))
}

fn read_plan(root: &Path, operation_id: &str) -> Result<Vec<u8>> {
    validate_render_operation_id(operation_id)?;
    let path = root.join(PLANS_DIR).join(format!("{operation_id}.json"));
    std::fs::read(&path).context("render operation plan is no longer retained")
}

fn sync_directory(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_project_render::model::{
        Approval, Category, KnowledgeEntry, Priority, RenderPlacement, Scope, Status,
    };
    use bbox_project_render::transport::{
        PROJECT_RENDER_TRANSPORT_SCOPE, PROJECT_RENDER_TRANSPORT_VERSION,
        ProjectRenderProducerAuthorityV1,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FakeClock(AtomicU64);

    impl ProducerCommandClock for FakeClock {
        fn now_secs(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    const PROJECT: &str = "p_render_operations";

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo-render-operations", ".").unwrap()
    }

    fn grant() -> BTreeMap<PublishedScope, String> {
        BTreeMap::from([(scope(), PROJECT.to_string())])
    }

    fn poll_request(covered: Vec<PublishedScope>) -> RenderLanePollRequestV1 {
        RenderLanePollRequestV1 {
            schema_version: RENDER_LANE_SCHEMA_VERSION,
            render_transport_versions: vec![PROJECT_RENDER_TRANSPORT_VERSION],
            covered_scopes: covered,
            collector_version: "0.0.1".into(),
        }
    }

    fn entry(content: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            id: "render-operations-entry".into(),
            title: "entry".into(),
            content: content.into(),
            cluster: None,
            variants: Default::default(),
            category: Category::Convention,
            scope: Scope::Project,
            project: Some(PROJECT_RENDER_TRANSPORT_SCOPE.into()),
            project_id: Some(PROJECT.into()),
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            render_placement: RenderPlacement::Inline,
            decay: false,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-09-01T00:00:00Z".into(),
            updated_at: "2026-09-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn new_operation() -> NewRenderOperation<'static> {
        static SCOPE: std::sync::OnceLock<PublishedScope> = std::sync::OnceLock::new();
        NewRenderOperation {
            producer_id: "producer-a",
            project_id: PROJECT,
            scope: SCOPE.get_or_init(scope),
            provider: Some("claude".into()),
            dry_run: false,
            view: ProjectRenderViewV1::Published,
            requested_scope: "project".into(),
        }
    }

    fn create(runtime: &RenderOperationRuntime, content: &str) -> RenderOperationRecord {
        let (operation_id, sequence) = runtime.reserve(PROJECT).unwrap();
        let plan = ProjectRenderPlanV1 {
            version: PROJECT_RENDER_TRANSPORT_VERSION,
            project_id: PROJECT.into(),
            scope: scope(),
            workspace_id: String::new(),
            producer: Some(ProjectRenderProducerAuthorityV1 {
                producer_id: "producer-a".into(),
                operation_id,
                sequence,
                issued_at_ms: 1_000,
            }),
            provider: Some("claude".into()),
            dry_run: false,
            view: ProjectRenderViewV1::Published,
            requested_scope: "project".into(),
            entries: vec![entry(content)],
            diagnostics: None,
        };
        runtime.create(new_operation(), &plan).unwrap()
    }

    fn receipt_for(
        runtime: &RenderOperationRuntime,
        record: &RenderOperationRecord,
    ) -> ProjectRenderReceiptV1 {
        let plan = runtime.issued_plan(&record.operation_id).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        bbox_project_render::execute::execute_project_render_plan_as(
            &plan,
            &root,
            &scope(),
            bbox_project_render::transport::ExpectedRenderAuthority::Producer {
                operation_id: &record.operation_id,
            },
            Duration::from_secs(5),
        )
        .unwrap()
        .receipt
    }

    #[test]
    fn delivery_requires_lane_coverage_grant_and_the_redelivery_window() {
        let directory = tempfile::tempdir().unwrap();
        let clock = Arc::new(FakeClock(AtomicU64::new(1_000)));
        let runtime = RenderOperationRuntime::open_with_clock(directory.path(), clock.clone());
        let record = create(&runtime, "first");

        let other = runtime
            .poll("producer-b", &poll_request(vec![scope()]), &grant())
            .unwrap();
        assert!(
            other.operations.is_empty(),
            "another producer never sees it"
        );
        let uncovered = runtime
            .poll("producer-a", &poll_request(vec![]), &grant())
            .unwrap();
        assert!(
            uncovered.operations.is_empty(),
            "the owner must hold the checkout"
        );
        let revoked = runtime
            .poll("producer-a", &poll_request(vec![scope()]), &BTreeMap::new())
            .unwrap();
        assert!(
            revoked.operations.is_empty(),
            "the grant must still authorize it"
        );
        let mut old = poll_request(vec![scope()]);
        old.render_transport_versions = vec![1];
        assert!(
            runtime
                .poll("producer-a", &old, &grant())
                .unwrap()
                .operations
                .is_empty()
        );
        assert!(
            !runtime
                .lane("producer-a")
                .unwrap()
                .supports_current_transport
        );

        let delivered = runtime
            .poll("producer-a", &poll_request(vec![scope()]), &grant())
            .unwrap();
        assert_eq!(delivered.operations.len(), 1);
        assert_eq!(delivered.operations[0].operation_id, record.operation_id);
        assert_eq!(delivered.operations[0].kind, RENDER_PROJECT_COMMAND_KIND);
        delivered.validate().unwrap();
        let duplicate = runtime
            .poll("producer-a", &poll_request(vec![scope()]), &grant())
            .unwrap();
        assert!(
            duplicate.operations.is_empty(),
            "no duplicate within the window"
        );
        clock
            .0
            .fetch_add(PRODUCER_COMMAND_REDELIVERY_SECS, Ordering::Relaxed);
        let redelivered = runtime
            .poll("producer-a", &poll_request(vec![scope()]), &grant())
            .unwrap();
        assert_eq!(
            redelivered.operations.len(),
            1,
            "an unacknowledged operation redelivers"
        );
        clock
            .0
            .fetch_add(PRODUCER_PRESENCE_FRESH_SECS + 1, Ordering::Relaxed);
        assert!(!runtime.lane("producer-a").unwrap().fresh);
    }

    #[test]
    fn plan_pages_are_authorized_per_page_and_close_on_settlement() {
        let runtime = RenderOperationRuntime::in_memory();
        let record = create(&runtime, "paged");
        let chunk = runtime
            .plan_page("producer-a", &record.operation_id, 0, &grant())
            .unwrap();
        assert_eq!(chunk.plan_sha256, record.plan_sha256);
        assert_eq!(
            runtime.plan_page("producer-b", &record.operation_id, 0, &grant()),
            Err(RenderLaneError::WrongProducer)
        );
        assert_eq!(
            runtime.plan_page("producer-a", &record.operation_id, 0, &BTreeMap::new()),
            Err(RenderLaneError::WrongProducer),
            "a revoked grant stops paging"
        );
        let receipt = receipt_for(&runtime, &record);
        runtime
            .settle(
                "producer-a",
                &record.operation_id,
                &record.plan_sha256,
                &grant(),
                Ok(receipt),
            )
            .unwrap();
        assert_eq!(
            runtime.plan_page("producer-a", &record.operation_id, 0, &grant()),
            Err(RenderLaneError::Settled)
        );
    }

    #[test]
    fn results_are_idempotent_exact_and_bound_to_their_plan() {
        let runtime = RenderOperationRuntime::in_memory();
        let record = create(&runtime, "exact");
        let receipt = receipt_for(&runtime, &record);
        let mut forged = receipt.clone();
        forged.projections[0].projection_bytes = Some(1);
        assert!(matches!(
            runtime.settle(
                "producer-a",
                &record.operation_id,
                &record.plan_sha256,
                &grant(),
                Ok(forged)
            ),
            Err(RenderLaneError::Invalid(_))
        ));
        assert!(matches!(
            runtime.settle(
                "producer-a",
                &record.operation_id,
                &"0".repeat(64),
                &grant(),
                Ok(receipt.clone())
            ),
            Err(RenderLaneError::Conflict(_))
        ));
        assert_eq!(
            runtime.settle(
                "producer-a",
                &record.operation_id,
                &record.plan_sha256,
                &grant(),
                Ok(receipt.clone())
            ),
            Ok(RenderResultStatus::Accepted)
        );
        assert_eq!(
            runtime.settle(
                "producer-a",
                &record.operation_id,
                &record.plan_sha256,
                &grant(),
                Ok(receipt.clone())
            ),
            Ok(RenderResultStatus::AlreadySettled),
            "a duplicate acknowledgment does not rewrite history"
        );
        assert!(matches!(
            runtime.settle(
                "producer-a",
                &record.operation_id,
                &record.plan_sha256,
                &grant(),
                Err(RenderOperationErrorV1 {
                    code: "late".into(),
                    message: "late".into()
                })
            ),
            Err(RenderLaneError::Conflict(_))
        ));
    }

    #[test]
    fn a_newer_operation_supersedes_pending_ones_and_late_results_are_not_current() {
        let runtime = RenderOperationRuntime::in_memory();
        let older = create(&runtime, "older");
        let older_receipt = receipt_for(&runtime, &older);
        let newer = create(&runtime, "newer");
        assert!(newer.sequence > older.sequence);
        assert_eq!(
            runtime.record(&older.operation_id).unwrap().state,
            RenderOperationState::Superseded {
                by: newer.operation_id.clone()
            }
        );
        let delivered = runtime
            .poll("producer-a", &poll_request(vec![scope()]), &grant())
            .unwrap();
        assert_eq!(delivered.operations.len(), 1);
        assert_eq!(delivered.operations[0].operation_id, newer.operation_id);
        assert_eq!(
            runtime.plan_page("producer-a", &older.operation_id, 0, &grant()),
            Err(RenderLaneError::Settled)
        );
        // An owner that applied the older plan before it was superseded
        // still reports it; the record says it is not current.
        assert_eq!(
            runtime.settle(
                "producer-a",
                &older.operation_id,
                &older.plan_sha256,
                &grant(),
                Ok(older_receipt)
            ),
            Ok(RenderResultStatus::Superseded)
        );
        match runtime.record(&older.operation_id).unwrap().state {
            RenderOperationState::Completed {
                validation: RenderCompletionValidation::Stale { .. },
                late: true,
                ..
            } => {}
            state => panic!("unexpected state {state:?}"),
        }
        assert_eq!(runtime.latest_sequence(PROJECT), Some(newer.sequence));
    }

    #[test]
    fn operations_and_plans_survive_restart_and_settled_plans_are_released() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let record = {
            let runtime = RenderOperationRuntime::open(&root).unwrap();
            create(&runtime, "durable")
        };
        let restarted = RenderOperationRuntime::open(&root).unwrap();
        assert_eq!(restarted.record(&record.operation_id).unwrap(), record);
        let delivered = restarted
            .poll("producer-a", &poll_request(vec![scope()]), &grant())
            .unwrap();
        assert_eq!(delivered.operations.len(), 1);
        let receipt = receipt_for(&restarted, &record);
        restarted
            .settle(
                "producer-a",
                &record.operation_id,
                &record.plan_sha256,
                &grant(),
                Ok(receipt.clone()),
            )
            .unwrap();
        assert!(
            !root
                .join(PLANS_DIR)
                .join(format!("{}.json", record.operation_id))
                .exists()
        );
        let again = RenderOperationRuntime::open(&root).unwrap();
        match again.record(&record.operation_id).unwrap().state {
            RenderOperationState::Completed {
                receipt: stored, ..
            } => assert_eq!(stored, receipt),
            state => panic!("unexpected state {state:?}"),
        }
        // Sequences stay monotonic across a restart.
        let next = create(&again, "after restart");
        assert!(next.sequence > record.sequence);
    }

    #[test]
    fn retention_drops_only_settled_records() {
        let mut index = RenderOperationIndex::default();
        for number in 0..(MAX_RETAINED_OPERATIONS as u128 + 3) {
            let mut record = RenderOperationRecord {
                operation_id: format_render_operation_id(number),
                producer_id: "producer-a".into(),
                project_id: PROJECT.into(),
                scope: scope(),
                sequence: number as u64 + 1,
                issued_at_ms: 1,
                plan_sha256: "a".repeat(64),
                plan_bytes: 1,
                provider: None,
                dry_run: false,
                view: ProjectRenderViewV1::Published,
                requested_scope: "project".into(),
                created_at_unix_secs: 1,
                delivered_at_unix_secs: None,
                settled_at_unix_secs: None,
                state: RenderOperationState::Pending,
            };
            if number > 1 {
                record.state = RenderOperationState::Superseded { by: "x".into() };
            }
            index.operations.push(record);
        }
        let dropped = retain(&mut index);
        assert_eq!(dropped.len(), 3);
        assert_eq!(index.operations.len(), MAX_RETAINED_OPERATIONS);
        assert!(index.operations[0].is_pending() && index.operations[1].is_pending());
    }

    #[tokio::test]
    async fn waiters_wake_on_settlement_and_time_out_while_pending() {
        let runtime = Arc::new(RenderOperationRuntime::in_memory());
        let record = create(&runtime, "wait");
        let pending = runtime
            .wait_settled(&record.operation_id, Duration::from_millis(10))
            .await
            .unwrap();
        assert!(pending.is_pending());
        let receipt = receipt_for(&runtime, &record);
        let waiter = {
            let runtime = runtime.clone();
            let operation_id = record.operation_id.clone();
            tokio::spawn(async move {
                runtime
                    .wait_settled(&operation_id, Duration::from_secs(5))
                    .await
            })
        };
        tokio::task::yield_now().await;
        runtime
            .settle(
                "producer-a",
                &record.operation_id,
                &record.plan_sha256,
                &grant(),
                Ok(receipt),
            )
            .unwrap();
        assert!(!waiter.await.unwrap().unwrap().is_pending());
    }
}
