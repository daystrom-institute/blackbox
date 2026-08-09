//! Durable, path-free observations for knowledge transport overlap.
//!
//! Checkout lease counters prove the negative half of cutover: covered
//! projects stop opening local trees. These observations prove the positive
//! half during overlap by recording which logical operation used local or
//! remote state and by retaining the latest bounded shadow comparison for
//! each project/workspace lane.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bbox_corpus_core::json_store::atomic_write_bytes_locked;
use fs2::FileExt;
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const OBSERVATION_VERSION: u32 = 1;
const MAX_ID_BYTES: usize = 256;
const MAX_COUNTERS: usize = 65_536;
const MAX_COMPARISONS: usize = 16_384;
const MAX_OBSERVATION_BYTES: usize = 32 * 1024 * 1024;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeTransportOperationV1 {
    PublishedKnowledge,
    PublishedGaps,
    ProvisionalOwnKnowledge,
    ProvisionalOwnGaps,
    ProvisionalAllKnowledge,
    ProvisionalAllGaps,
    ProjectKnowledgeMutation,
    ProjectGapMutation,
    AcceptedPublicationMutation,
    WatcherRefresh,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeTransportOutcomeV1 {
    Local,
    Remote,
    ShadowEqual,
    ShadowMismatch,
    Degraded,
    AuthoritativeRefusal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportOperationCounterV1 {
    pub project_id: String,
    pub operation: KnowledgeTransportOperationV1,
    pub outcome: KnowledgeTransportOutcomeV1,
    pub count: u64,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub last_unix_secs: u64,
}

/// Latest exact shadow comparison for one logical lane.
///
/// `reference_snapshot_id` names the already-authoritative accepted or local
/// overlay result. `transport_snapshot_id` names the remote transport result.
/// Both are content identities, never paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportShadowComparisonV1 {
    pub project_id: String,
    pub operation: KnowledgeTransportOperationV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub reference_snapshot_id: String,
    pub transport_snapshot_id: String,
    pub equal: bool,
    pub sequence: u64,
    pub observed_at_unix_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportObservationSnapshotV1 {
    pub version: u32,
    pub sequence: u64,
    pub counters: Vec<KnowledgeTransportOperationCounterV1>,
    pub comparisons: Vec<KnowledgeTransportShadowComparisonV1>,
}

impl Default for KnowledgeTransportObservationSnapshotV1 {
    fn default() -> Self {
        Self {
            version: OBSERVATION_VERSION,
            sequence: 0,
            counters: Vec::new(),
            comparisons: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct KnowledgeTransportObservationsV1 {
    store_path: Option<Arc<PathBuf>>,
    state: Arc<Mutex<KnowledgeTransportObservationSnapshotV1>>,
}

impl KnowledgeTransportObservationsV1 {
    pub fn open(store_path: impl Into<PathBuf>) -> Result<Self> {
        let store_path = store_path.into();
        let snapshot = load_snapshot(&store_path)?;
        Ok(Self {
            store_path: Some(Arc::new(store_path)),
            state: Arc::new(Mutex::new(snapshot)),
        })
    }

    pub fn in_memory() -> Self {
        Self {
            store_path: None,
            state: Arc::new(Mutex::new(
                KnowledgeTransportObservationSnapshotV1::default(),
            )),
        }
    }

    pub fn snapshot(&self) -> KnowledgeTransportObservationSnapshotV1 {
        self.state.lock().clone()
    }

    pub fn record(
        &self,
        project_id: &str,
        operation: KnowledgeTransportOperationV1,
        outcome: KnowledgeTransportOutcomeV1,
    ) -> Result<u64> {
        self.mutate(|snapshot| record_counter(snapshot, project_id, operation, outcome))
    }

    pub fn record_shadow(
        &self,
        project_id: &str,
        operation: KnowledgeTransportOperationV1,
        workspace_id: Option<&str>,
        reference_snapshot_id: &str,
        transport_snapshot_id: &str,
    ) -> Result<u64> {
        self.mutate(|snapshot| {
            let equal = reference_snapshot_id == transport_snapshot_id;
            let sequence = record_counter(
                snapshot,
                project_id,
                operation,
                if equal {
                    KnowledgeTransportOutcomeV1::ShadowEqual
                } else {
                    KnowledgeTransportOutcomeV1::ShadowMismatch
                },
            )?;
            let observed_at_unix_secs = now_unix_secs();
            let comparison = KnowledgeTransportShadowComparisonV1 {
                project_id: project_id.to_string(),
                operation,
                workspace_id: workspace_id.map(str::to_owned),
                reference_snapshot_id: reference_snapshot_id.to_string(),
                transport_snapshot_id: transport_snapshot_id.to_string(),
                equal,
                sequence,
                observed_at_unix_secs,
            };
            match snapshot.comparisons.binary_search_by(|current| {
                comparison_key(current).cmp(&comparison_key(&comparison))
            }) {
                Ok(index) => snapshot.comparisons[index] = comparison,
                Err(index) => snapshot.comparisons.insert(index, comparison),
            }
            Ok(sequence)
        })
    }

    fn mutate(
        &self,
        mutation: impl FnOnce(&mut KnowledgeTransportObservationSnapshotV1) -> Result<u64>,
    ) -> Result<u64> {
        let mut state = self.state.lock();
        let (next, sequence) = if let Some(store_path) = &self.store_path {
            with_store_lock(store_path, || {
                let mut next = load_snapshot(store_path)?;
                let sequence = mutation(&mut next)?;
                validate_snapshot(&next)?;
                let bytes = serde_json::to_vec(&next)?;
                if bytes.len() > MAX_OBSERVATION_BYTES {
                    anyhow::bail!("knowledge transport observations exceed their byte bound");
                }
                atomic_write_bytes_locked(store_path, &bytes)?;
                sync_parent_directory(store_path)?;
                Ok((next, sequence))
            })?
        } else {
            let mut next = state.clone();
            let sequence = mutation(&mut next)?;
            validate_snapshot(&next)?;
            (next, sequence)
        };
        *state = next;
        Ok(sequence)
    }
}

fn record_counter(
    snapshot: &mut KnowledgeTransportObservationSnapshotV1,
    project_id: &str,
    operation: KnowledgeTransportOperationV1,
    outcome: KnowledgeTransportOutcomeV1,
) -> Result<u64> {
    validate_id(project_id, "project id")?;
    snapshot.sequence = snapshot
        .sequence
        .checked_add(1)
        .context("knowledge transport observation sequence exhausted")?;
    let sequence = snapshot.sequence;
    let now = now_unix_secs();
    let key = (project_id, operation, outcome);
    match snapshot.counters.binary_search_by(|counter| {
        (
            counter.project_id.as_str(),
            counter.operation,
            counter.outcome,
        )
            .cmp(&key)
    }) {
        Ok(index) => {
            let counter = &mut snapshot.counters[index];
            counter.count = counter
                .count
                .checked_add(1)
                .context("knowledge transport observation counter exhausted")?;
            counter.last_sequence = sequence;
            counter.last_unix_secs = now;
        }
        Err(index) => snapshot.counters.insert(
            index,
            KnowledgeTransportOperationCounterV1 {
                project_id: project_id.to_string(),
                operation,
                outcome,
                count: 1,
                first_sequence: sequence,
                last_sequence: sequence,
                last_unix_secs: now,
            },
        ),
    }
    Ok(sequence)
}

fn comparison_key(
    comparison: &KnowledgeTransportShadowComparisonV1,
) -> (&str, KnowledgeTransportOperationV1, Option<&str>) {
    (
        comparison.project_id.as_str(),
        comparison.operation,
        comparison.workspace_id.as_deref(),
    )
}

fn load_snapshot(path: &Path) -> Result<KnowledgeTransportObservationSnapshotV1> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.is_empty() || bytes.len() > MAX_OBSERVATION_BYTES {
                anyhow::bail!("knowledge transport observation file is empty or oversized");
            }
            let snapshot = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            validate_snapshot(&snapshot)
                .with_context(|| format!("validating {}", path.display()))?;
            Ok(snapshot)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(KnowledgeTransportObservationSnapshotV1::default())
        }
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn validate_snapshot(snapshot: &KnowledgeTransportObservationSnapshotV1) -> Result<()> {
    if snapshot.version != OBSERVATION_VERSION {
        anyhow::bail!(
            "unsupported knowledge transport observation version {}",
            snapshot.version
        );
    }
    if snapshot.counters.len() > MAX_COUNTERS || snapshot.comparisons.len() > MAX_COMPARISONS {
        anyhow::bail!("knowledge transport observation set exceeds its row bound");
    }
    if !snapshot.counters.windows(2).all(|pair| {
        (
            pair[0].project_id.as_str(),
            pair[0].operation,
            pair[0].outcome,
        ) < (
            pair[1].project_id.as_str(),
            pair[1].operation,
            pair[1].outcome,
        )
    }) || !snapshot
        .comparisons
        .windows(2)
        .all(|pair| comparison_key(&pair[0]) < comparison_key(&pair[1]))
    {
        anyhow::bail!("knowledge transport observations are not canonical");
    }
    let mut counter_keys = BTreeSet::new();
    for counter in &snapshot.counters {
        validate_id(&counter.project_id, "project id")?;
        if counter.count == 0
            || counter.first_sequence == 0
            || counter.first_sequence > counter.last_sequence
            || counter.last_sequence > snapshot.sequence
            || !counter_keys.insert((
                counter.project_id.as_str(),
                counter.operation,
                counter.outcome,
            ))
        {
            anyhow::bail!("invalid knowledge transport observation counter");
        }
    }
    let mut comparison_keys = BTreeSet::new();
    for comparison in &snapshot.comparisons {
        validate_id(&comparison.project_id, "project id")?;
        if let Some(workspace_id) = &comparison.workspace_id {
            validate_id(workspace_id, "workspace id")?;
        }
        validate_id(&comparison.reference_snapshot_id, "reference snapshot id")?;
        validate_id(&comparison.transport_snapshot_id, "transport snapshot id")?;
        if comparison.equal
            != (comparison.reference_snapshot_id == comparison.transport_snapshot_id)
            || comparison.sequence == 0
            || comparison.sequence > snapshot.sequence
            || !comparison_keys.insert(comparison_key(comparison))
        {
            anyhow::bail!("invalid knowledge transport shadow comparison");
        }
        let expected_outcome = if comparison.equal {
            KnowledgeTransportOutcomeV1::ShadowEqual
        } else {
            KnowledgeTransportOutcomeV1::ShadowMismatch
        };
        if !snapshot.counters.iter().any(|counter| {
            counter.project_id == comparison.project_id
                && counter.operation == comparison.operation
                && counter.outcome == expected_outcome
                && counter.last_sequence >= comparison.sequence
        }) {
            anyhow::bail!("knowledge transport shadow comparison has no matching counter");
        }
    }
    Ok(())
}

fn validate_id(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || value.chars().any(char::is_control)
        || value.contains('/')
        || value.contains('\\')
    {
        anyhow::bail!("invalid knowledge transport {label}");
    }
    Ok(())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn with_store_lock<T>(path: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let parent = path
        .parent()
        .context("knowledge transport observation path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let lock_path = parent.join(format!(
        ".{}.lock",
        path.file_name()
            .and_then(|name| name.to_str())
            .context("knowledge transport observation filename is invalid")?
    ));
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    lock.lock_exclusive()?;
    let result = operation();
    FileExt::unlock(&lock)?;
    result
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("knowledge transport observation path has no parent")?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_bounded_operation_and_shadow_evidence_across_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("knowledge-transport-observations.json");
        let observations = KnowledgeTransportObservationsV1::open(&path).unwrap();
        observations
            .record(
                "p_1",
                KnowledgeTransportOperationV1::PublishedKnowledge,
                KnowledgeTransportOutcomeV1::Remote,
            )
            .unwrap();
        observations
            .record_shadow(
                "p_1",
                KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
                Some("checkout_1"),
                "snapshot_1",
                "snapshot_1",
            )
            .unwrap();
        drop(observations);

        let reopened = KnowledgeTransportObservationsV1::open(&path).unwrap();
        let snapshot = reopened.snapshot();
        assert_eq!(snapshot.sequence, 2);
        assert_eq!(snapshot.counters.len(), 2);
        assert_eq!(snapshot.comparisons.len(), 1);
        assert!(snapshot.comparisons[0].equal);
    }

    #[test]
    fn latest_shadow_replaces_same_project_workspace_lane() {
        let observations = KnowledgeTransportObservationsV1::in_memory();
        observations
            .record_shadow(
                "p_1",
                KnowledgeTransportOperationV1::ProvisionalAllGaps,
                Some("checkout_1"),
                "gap_1",
                "gap_2",
            )
            .unwrap();
        observations
            .record_shadow(
                "p_1",
                KnowledgeTransportOperationV1::ProvisionalAllGaps,
                Some("checkout_1"),
                "gap_3",
                "gap_3",
            )
            .unwrap();
        let snapshot = observations.snapshot();
        assert_eq!(snapshot.comparisons.len(), 1);
        assert!(snapshot.comparisons[0].equal);
        assert_eq!(snapshot.comparisons[0].sequence, 2);
    }
}
