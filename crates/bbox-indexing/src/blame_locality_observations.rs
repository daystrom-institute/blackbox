//! Durable path-free observations for blame locality overlap and cutover.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bbox_corpus_core::json_store::atomic_write_bytes_locked;
use fs2::FileExt;
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const OBSERVATION_VERSION: u32 = 1;
const MAX_ID_BYTES: usize = 256;
const MAX_COUNTERS: usize = 65_536;
const MAX_COMPARISONS: usize = 16_384;
const MAX_OBSERVATION_BYTES: usize = 16 * 1024 * 1024;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BlameLocalityAuthorityV1 {
    ManagedWorkspace,
    Operator,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BlameLocalityTargetV1 {
    Path,
    Entity,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BlameLocalityOutcomeV1 {
    Completed,
    ShadowEqual,
    ShadowMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameLocalityCounterV1 {
    pub project_id: String,
    pub authority: BlameLocalityAuthorityV1,
    pub target: BlameLocalityTargetV1,
    pub outcome: BlameLocalityOutcomeV1,
    pub count: u64,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub last_unix_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameLocalityComparisonV1 {
    pub project_id: String,
    pub target: BlameLocalityTargetV1,
    pub local_response_sha256: String,
    pub legacy_response_sha256: String,
    pub equal: bool,
    pub sequence: u64,
    pub observed_at_unix_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameLocalityObservationSnapshotV1 {
    pub version: u32,
    pub sequence: u64,
    pub counters: Vec<BlameLocalityCounterV1>,
    pub comparisons: Vec<BlameLocalityComparisonV1>,
}

impl Default for BlameLocalityObservationSnapshotV1 {
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
pub struct BlameLocalityObservationsV1 {
    store_path: Option<Arc<PathBuf>>,
    state: Arc<Mutex<BlameLocalityObservationSnapshotV1>>,
}

impl BlameLocalityObservationsV1 {
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
            state: Arc::new(Mutex::new(BlameLocalityObservationSnapshotV1::default())),
        }
    }

    pub fn snapshot(&self) -> BlameLocalityObservationSnapshotV1 {
        self.state.lock().clone()
    }

    pub fn record_completed(
        &self,
        project_id: &str,
        authority: BlameLocalityAuthorityV1,
        target: BlameLocalityTargetV1,
    ) -> Result<u64> {
        self.mutate(|snapshot| {
            record_counter(
                snapshot,
                project_id,
                authority,
                target,
                BlameLocalityOutcomeV1::Completed,
            )
        })
    }

    pub fn record_comparison(
        &self,
        project_id: &str,
        target: BlameLocalityTargetV1,
        local_response_sha256: &str,
        legacy_response_sha256: &str,
    ) -> Result<u64> {
        validate_sha256(local_response_sha256)?;
        validate_sha256(legacy_response_sha256)?;
        self.mutate(|snapshot| {
            let equal = local_response_sha256 == legacy_response_sha256;
            let sequence = record_counter(
                snapshot,
                project_id,
                BlameLocalityAuthorityV1::Operator,
                target,
                if equal {
                    BlameLocalityOutcomeV1::ShadowEqual
                } else {
                    BlameLocalityOutcomeV1::ShadowMismatch
                },
            )?;
            let comparison = BlameLocalityComparisonV1 {
                project_id: project_id.to_string(),
                target,
                local_response_sha256: local_response_sha256.to_string(),
                legacy_response_sha256: legacy_response_sha256.to_string(),
                equal,
                sequence,
                observed_at_unix_secs: now_unix_secs(),
            };
            match snapshot.comparisons.binary_search_by(|current| {
                (current.project_id.as_str(), current.target)
                    .cmp(&(comparison.project_id.as_str(), comparison.target))
            }) {
                Ok(index) => snapshot.comparisons[index] = comparison,
                Err(index) => snapshot.comparisons.insert(index, comparison),
            }
            Ok(sequence)
        })
    }

    fn mutate(
        &self,
        mutation: impl FnOnce(&mut BlameLocalityObservationSnapshotV1) -> Result<u64>,
    ) -> Result<u64> {
        let mut state = self.state.lock();
        let (next, sequence) = if let Some(store_path) = &self.store_path {
            with_store_lock(store_path, || {
                let mut next = load_snapshot(store_path)?;
                let sequence = mutation(&mut next)?;
                validate_snapshot(&next)?;
                let bytes = serde_json::to_vec(&next)?;
                if bytes.len() > MAX_OBSERVATION_BYTES {
                    bail!("blame locality observations exceed their byte bound");
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
    snapshot: &mut BlameLocalityObservationSnapshotV1,
    project_id: &str,
    authority: BlameLocalityAuthorityV1,
    target: BlameLocalityTargetV1,
    outcome: BlameLocalityOutcomeV1,
) -> Result<u64> {
    validate_id(project_id)?;
    snapshot.sequence = snapshot
        .sequence
        .checked_add(1)
        .context("blame locality observation sequence exhausted")?;
    let sequence = snapshot.sequence;
    let now = now_unix_secs();
    let key = (project_id, authority, target, outcome);
    match snapshot.counters.binary_search_by(|counter| {
        (
            counter.project_id.as_str(),
            counter.authority,
            counter.target,
            counter.outcome,
        )
            .cmp(&key)
    }) {
        Ok(index) => {
            let counter = &mut snapshot.counters[index];
            counter.count = counter
                .count
                .checked_add(1)
                .context("blame locality observation counter exhausted")?;
            counter.last_sequence = sequence;
            counter.last_unix_secs = now;
        }
        Err(index) => snapshot.counters.insert(
            index,
            BlameLocalityCounterV1 {
                project_id: project_id.to_string(),
                authority,
                target,
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

fn load_snapshot(path: &Path) -> Result<BlameLocalityObservationSnapshotV1> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.len() > MAX_OBSERVATION_BYTES {
                bail!("blame locality observation file exceeds its byte bound");
            }
            let snapshot: BlameLocalityObservationSnapshotV1 = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            validate_snapshot(&snapshot)?;
            Ok(snapshot)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn validate_snapshot(snapshot: &BlameLocalityObservationSnapshotV1) -> Result<()> {
    if snapshot.version != OBSERVATION_VERSION {
        bail!("unsupported blame locality observation version");
    }
    if snapshot.counters.len() > MAX_COUNTERS || snapshot.comparisons.len() > MAX_COMPARISONS {
        bail!("blame locality observation cardinality exceeds its bound");
    }
    for counter in &snapshot.counters {
        validate_id(&counter.project_id)?;
        if counter.count == 0
            || counter.first_sequence == 0
            || counter.first_sequence > counter.last_sequence
            || counter.last_sequence > snapshot.sequence
        {
            bail!("invalid blame locality counter");
        }
    }
    for comparison in &snapshot.comparisons {
        validate_id(&comparison.project_id)?;
        validate_sha256(&comparison.local_response_sha256)?;
        validate_sha256(&comparison.legacy_response_sha256)?;
        if comparison.equal
            != (comparison.local_response_sha256 == comparison.legacy_response_sha256)
            || comparison.sequence == 0
            || comparison.sequence > snapshot.sequence
        {
            bail!("invalid blame locality comparison");
        }
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_ID_BYTES {
        bail!("invalid blame locality project id");
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid blame locality response checksum");
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
    let parent = path.parent().context("observation path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    lock.lock_exclusive()?;
    let result = operation();
    let unlock = FileExt::unlock(&lock).context("unlocking blame locality observations");
    match (result, unlock) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path.parent().context("observation path has no parent")?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparisons_are_bounded_durable_and_replace_by_target() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blame.json");
        let observations = BlameLocalityObservationsV1::open(&path).unwrap();
        observations
            .record_completed(
                "project",
                BlameLocalityAuthorityV1::Operator,
                BlameLocalityTargetV1::Path,
            )
            .unwrap();
        observations
            .record_comparison(
                "project",
                BlameLocalityTargetV1::Path,
                &"a".repeat(64),
                &"b".repeat(64),
            )
            .unwrap();
        observations
            .record_comparison(
                "project",
                BlameLocalityTargetV1::Path,
                &"c".repeat(64),
                &"c".repeat(64),
            )
            .unwrap();

        let reopened = BlameLocalityObservationsV1::open(&path).unwrap().snapshot();
        assert_eq!(reopened.sequence, 3);
        assert_eq!(reopened.comparisons.len(), 1);
        assert!(reopened.comparisons[0].equal);
    }
}
