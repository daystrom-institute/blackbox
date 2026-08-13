//! Durable evidence that one collected code generation survived startup and rebuild.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bbox_code_source::GenerationState;
use bbox_code_source_store::{CodeSourceStore, MixedActivationRecord, MixedStoredGeneration};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::atomic_write_bytes_locked;
use bbox_corpus_core::project_catalog::ProjectId;
use fs2::FileExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::code_source_locality_manifest::WorkspaceManifestState;

const OBSERVATION_VERSION: u32 = 1;
const MAX_OBSERVATIONS: usize = 65_536;
const MAX_OBSERVATION_BYTES: usize = 16 * 1024 * 1024;
pub const CODE_SOURCE_LOCALITY_OBSERVATIONS_FILE: &str = "code-source-locality-observations.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeSourceLocalityEvidenceKindV1 {
    StartupRecovery,
    FullRebuild,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSourceLocalityObservationV1 {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    pub producer_id: String,
    pub generation_id: String,
    pub selector: String,
    pub snapshot_id: String,
    pub document_count: u64,
    pub entity_inventory_sha256: String,
    pub evidence_kind: CodeSourceLocalityEvidenceKindV1,
    pub sequence: u64,
    pub observed_at_unix_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSourceLocalityObservationSnapshotV1 {
    pub version: u32,
    pub sequence: u64,
    pub observations: Vec<CodeSourceLocalityObservationV1>,
}

impl Default for CodeSourceLocalityObservationSnapshotV1 {
    fn default() -> Self {
        Self {
            version: OBSERVATION_VERSION,
            sequence: 0,
            observations: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct CodeSourceLocalityObservationsV1 {
    store_path: Option<Arc<PathBuf>>,
    state: Arc<Mutex<CodeSourceLocalityObservationSnapshotV1>>,
}

impl CodeSourceLocalityObservationsV1 {
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
            state: Arc::new(Mutex::new(Default::default())),
        }
    }

    pub fn snapshot(&self) -> CodeSourceLocalityObservationSnapshotV1 {
        self.state.lock().clone()
    }

    /// Record every exact, healthy collected activation after the caller has
    /// completed the named recovery boundary. Invalid or cutback-pending rows
    /// fail the call instead of minting evidence.
    pub fn record_verified_activations(
        &self,
        store: &CodeSourceStore,
        projects_path: &Path,
        evidence_kind: CodeSourceLocalityEvidenceKindV1,
    ) -> Result<u64> {
        let edges_dir =
            bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(projects_path);
        let mut current = Vec::new();
        for activation in store.activation_records_mixed()? {
            let MixedActivationRecord::CurrentV2(activation) = activation else {
                continue;
            };
            if activation.cutback.is_some() || activation.cutback_pending {
                continue;
            }
            let generation = store.find_generation_mixed(&activation.generation_id)?;
            let MixedStoredGeneration::CurrentV2(generation) = generation else {
                bail!("current activation references a legacy code generation");
            };
            activation.validate_against_generation(&generation)?;
            if generation.state != GenerationState::Active {
                continue;
            }
            // The generation is Active and validates against its activation
            // record. Because the state flip is written after the manifest
            // write, that is proof the activation completed and a disagreeing
            // derived entry is a lost write, not an in-flight activation, so
            // it is repaired from the journal instead of refusing to open.
            // Unrecognized shapes, including the cutback crash window, still
            // refuse. `ActivationInFlight` cannot arise here because that
            // classification requires a non-Active generation; it is folded
            // into the refusal arm so the match stays total.
            match crate::code_source_locality_manifest::classify_workspace_manifest(
                &edges_dir,
                &activation,
                generation.state,
            )? {
                WorkspaceManifestState::Agreed | WorkspaceManifestState::Reconciled { .. } => {}
                WorkspaceManifestState::ActivationInFlight { .. }
                | WorkspaceManifestState::Disagrees => {
                    bail!("active collected generation disagrees with the workspace manifest");
                }
            }
            current.push(CodeSourceLocalityObservationV1 {
                project_id: activation.project_id,
                scope: activation.published_scope,
                producer_id: generation.producer_id,
                generation_id: activation.generation_id,
                selector: activation.selector,
                snapshot_id: activation.snapshot_id,
                document_count: activation.document_count,
                entity_inventory_sha256: activation.entity_inventory_sha256,
                evidence_kind,
                sequence: 0,
                observed_at_unix_secs: 0,
            });
        }
        self.mutate(|snapshot| {
            for mut observation in current {
                snapshot.sequence = snapshot
                    .sequence
                    .checked_add(1)
                    .context("code-source locality observation sequence exhausted")?;
                observation.sequence = snapshot.sequence;
                observation.observed_at_unix_secs = now_unix_secs();
                let key = (observation.project_id.clone(), observation.evidence_kind);
                match snapshot.observations.binary_search_by(|current| {
                    (current.project_id.clone(), current.evidence_kind).cmp(&key)
                }) {
                    Ok(index) => snapshot.observations[index] = observation,
                    Err(index) => snapshot.observations.insert(index, observation),
                }
            }
            Ok(snapshot.sequence)
        })
    }

    fn mutate(
        &self,
        mutation: impl FnOnce(&mut CodeSourceLocalityObservationSnapshotV1) -> Result<u64>,
    ) -> Result<u64> {
        let mut state = self.state.lock();
        let (next, sequence) = if let Some(store_path) = &self.store_path {
            with_store_lock(store_path, || {
                let mut next = load_snapshot(store_path)?;
                let sequence = mutation(&mut next)?;
                validate_snapshot(&next)?;
                let bytes = serde_json::to_vec(&next)?;
                if bytes.len() > MAX_OBSERVATION_BYTES {
                    bail!("code-source locality observations exceed their byte bound");
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

pub fn observation_path_from_code_source_root(root: &Path) -> Result<PathBuf> {
    Ok(root
        .parent()
        .context("code-source store root has no state parent")?
        .join(CODE_SOURCE_LOCALITY_OBSERVATIONS_FILE))
}

fn load_snapshot(path: &Path) -> Result<CodeSourceLocalityObservationSnapshotV1> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.len() > MAX_OBSERVATION_BYTES {
                bail!("code-source locality observation file exceeds its byte bound");
            }
            let snapshot = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            validate_snapshot(&snapshot)?;
            Ok(snapshot)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn validate_snapshot(snapshot: &CodeSourceLocalityObservationSnapshotV1) -> Result<()> {
    if snapshot.version != OBSERVATION_VERSION {
        bail!("unsupported code-source locality observation version");
    }
    if snapshot.observations.len() > MAX_OBSERVATIONS {
        bail!("code-source locality observation cardinality exceeds its bound");
    }
    let mut previous = None;
    for observation in &snapshot.observations {
        observation.scope.validate()?;
        validate_id(&observation.producer_id)?;
        validate_sha256(&observation.generation_id)?;
        validate_id(&observation.selector)?;
        validate_id(&observation.snapshot_id)?;
        validate_sha256(&observation.entity_inventory_sha256)?;
        if observation.sequence == 0 || observation.sequence > snapshot.sequence {
            bail!("invalid code-source locality observation sequence");
        }
        let key = (observation.project_id.clone(), observation.evidence_kind);
        if previous.as_ref().is_some_and(|previous| previous >= &key) {
            bail!("code-source locality observations are not strictly sorted");
        }
        previous = Some(key);
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 512 {
        bail!("invalid code-source locality identifier");
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
        bail!("invalid code-source locality checksum");
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
    let unlock = FileExt::unlock(&lock).context("unlocking code-source locality observations");
    match (result, unlock) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    File::open(path.parent().context("observation path has no parent")?)?.sync_all()?;
    Ok(())
}
