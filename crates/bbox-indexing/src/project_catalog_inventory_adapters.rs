//! Side-effect-free migration inventory adapters.
//!
//! Owner crates decode their own durable formats. This module composes those
//! exact, locked snapshots into the path-redacted inventory evidence consumed
//! by the project-catalog migration facade. It never repairs a source store,
//! follows a symlink, reads a working-tree identity file, or invents a second
//! durable wire format.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use bbox_code_source::{GenerationState, validate_collected_materialization_selector};
use bbox_code_source_store::{
    ActivationRecord, ActivationRecordV2, CodeSourceStore, CollisionRetirementLifecycleStateV1,
    CollisionRetirementSelectorEvidenceV1, MigrationLegacyAnchorEvidenceV1,
    MigrationLegacyGenerationEvidenceV1, MigrationLegacyInventoryV1,
    MigrationOwnedLegacyInventoryV1, StoreLimits, StoredGenerationV2,
    decode_migration_effective_source_manifest_v1, encode_activation_v2_for_migration,
    encode_stored_generation_v2_for_migration, verify_generation_manifest_for_migration,
};
use bbox_corpus_core::git::{
    StableGitRepository, VerifiedCommit, open_stable_git_repository,
    read_verified_committed_file_bytes_optional_bounded,
};
use bbox_corpus_core::identity::{PublishedScope, resolve_recorded_repo_id};
use bbox_corpus_core::json_store::NofollowDirectory;
use bbox_corpus_core::project_catalog::{
    AttachmentId, CommitNamespace, LegacyProjectStoreV1, MAX_LEGACY_PROJECT_STORE_BYTES, ProjectId,
    RecordedRepoAuthority, decode_legacy_project_store,
};
use bbox_corpus_core::project_catalog_snapshot::{
    LegacyProjectSelectorKindV1, OwnerSnapshotLimitsV1, OwnerSnapshotRowValueV1,
    OwnerSnapshotStateV1, OwnerSnapshotV1, capture_legacy_proposal_owner_snapshot,
    capture_legacy_task_owner_snapshot,
};
use bbox_corpus_index::index::migration_inventory::{
    CorpusMigrationSnapshotLimitsV1, CorpusMigrationSourceStateV1, CorpusOwnerMigrationSnapshotV1,
    capture_owner_migration_snapshot_no_create,
};
use bbox_edge_sidecar::migration_inventory::{
    EdgeMigrationSnapshotLimitsV1, EdgeMigrationSnapshotV1, EdgeMigrationSourceStateV1,
    capture_migration_snapshot_no_create as capture_edge_migration_snapshot_no_create,
};
use bbox_vectors::migration_inventory::{
    VectorMigrationSnapshotLimitsV1, VectorMigrationSnapshotV1, VectorMigrationSourceStateV1,
    capture_migration_snapshot_no_create as capture_vector_migration_snapshot_no_create,
};
use sha2::{Digest, Sha256};

use crate::project_catalog_inventory::{
    AttachmentCandidateObservationV1, CheckoutMarkerStateV1, CheckoutObservationV1,
    CodeSourceObservationV1, CollectedEvidenceMemberV1, CollectedGenerationObservationV1,
    CollectedGenerationRoleV1, CollisionLifecycleObservationV1,
    CollisionLifecycleStateObservationV1, DurableSelectorEvidenceV1, EdgeWorkspaceObservationV1,
    GitEvidenceMemberV1, GitMetadataObservationV1, ImmutableArtifactObservationV1,
    ImmutableCollectedDescriptorV1, ImmutableInventoryLaneEvidenceV1, ImmutableInventoryLaneKindV1,
    ImmutableInventoryOwnerKindV1, InventorySourceStateV1, InventoryTargetKindV1,
    InventoryTargetObservationV1, LegacyCommitNamespaceAttributionV1,
    LegacyCommitNamespaceInventoryV1, LegacyNamespaceClusterObservationV1, LegacyPathObservationV1,
    LegacyPathStoreKindV1, LegacyProjectObservationV1, LegacyProjectPathStatusV1,
    LegacyProjectRecordInventoryV1, LegacySelectorKindV1, MaterializedAliasObservationV1,
    MutableInventorySourceEvidenceV1, MutableInventorySourceKindV1,
    MutableInventorySourceLocatorV1, OwnerSubsourceEvidenceV1,
    PROJECT_CATALOG_INVENTORY_VERSION_V1, ProjectScopedRefObservationV1,
    ProjectScopedRefStoreKindV1, PublisherPinObservationV1, QuarantinedGenerationObservationV1,
    RecordedAuthorityEvidenceMemberV1, RepoGroupingProofV1,
    RetainedGenerationOwnerResolutionObservationV1, Sha256ValueV1,
    UnboundPublisherPinObservationV1, UnboundPublisherPinReasonV1, V1ProjectCatalogInventory,
    digest_path, mutable_source_row_set_hash,
};
use crate::publisher::{MigrationPublisherRefSnapshotV1, PublisherRefRow, PublisherRefStore};

const MAX_AUTHORIZED_PATH_BYTES: usize = 4_096;
const MAX_COMMITTED_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_CHECKOUT_MARKER_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryAdapterError {
    code: &'static str,
    detail: String,
}

impl InventoryAdapterError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail
                .into()
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .take(384)
                .collect(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for InventoryAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for InventoryAdapterError {}

type AdapterResult<T> = Result<T, InventoryAdapterError>;

#[derive(Clone)]
struct AuthorizedInventoryPath {
    path: PathBuf,
    authority: Arc<NofollowDirectory>,
    authority_canonical_path: PathBuf,
    target_identity: Option<FileIdentityV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FileIdentityV1 {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl AuthorizedInventoryPath {
    fn new(path: impl AsRef<Path>) -> AdapterResult<Self> {
        let path = path.as_ref().to_path_buf();
        let byte_len = path
            .to_str()
            .ok_or_else(|| invalid_input("authorized path is not utf8"))?
            .len();
        if !path.is_absolute()
            || path.file_name().is_none()
            || byte_len > MAX_AUTHORIZED_PATH_BYTES
            || path
                .components()
                .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        {
            return Err(invalid_input("authorized path is unsafe"));
        }
        let (authority_path, missing_suffix) = deepest_existing_directory(&path)?;
        let authority = Arc::new(
            NofollowDirectory::open_existing(&authority_path)
                .map_err(|_| invalid_input("authorized path authority is unsafe"))?
                .ok_or_else(|| invalid_input("authorized path authority is missing"))?,
        );
        let canonical_authority = authority_path
            .canonicalize()
            .map_err(|_| invalid_input("authorized path authority cannot be canonicalized"))?;
        authority
            .ensure_still_current()
            .map_err(|_| invalid_input("authorized path authority changed"))?;
        let canonical_path = missing_suffix
            .iter()
            .fold(canonical_authority.clone(), |path, component| {
                path.join(component)
            });
        let target_identity = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(invalid_input("authorized path contains a symlink"));
                }
                Some(file_identity(&metadata))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(invalid_input("authorized path cannot be inspected")),
        };
        Ok(Self {
            path: canonical_path,
            authority,
            authority_canonical_path: canonical_authority,
            target_identity,
        })
    }

    fn as_path(&self) -> &Path {
        &self.path
    }

    fn join(&self, relative: &str) -> AdapterResult<Self> {
        if relative.is_empty()
            || Path::new(relative).is_absolute()
            || Path::new(relative)
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(invalid_input("authorized relative path is unsafe"));
        }
        Self::new(self.path.join(relative))
    }

    fn ensure_authority(&self) -> AdapterResult<()> {
        self.authority
            .ensure_still_current()
            .map_err(|_| invalid_source("authorized_path_authority_changed"))?;
        let current_identity = match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid_source("authorized_path_target_changed"));
            }
            Ok(metadata) => Some(file_identity(&metadata)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(invalid_source("authorized_path_target_unreadable")),
        };
        if current_identity != self.target_identity {
            return Err(invalid_source("authorized_path_target_changed"));
        }
        Ok(())
    }

    fn authority_path_for_read(&self) -> &Path {
        &self.authority_canonical_path
    }

    fn checkout_identity_key(&self) -> AdapterResult<(PathBuf, FileIdentityV1)> {
        Ok((
            self.path.clone(),
            self.target_identity
                .ok_or_else(|| invalid_source("checkout root identity is missing"))?,
        ))
    }
}

impl PartialEq for AuthorizedInventoryPath {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.target_identity == other.target_identity
    }
}

impl Eq for AuthorizedInventoryPath {}

impl fmt::Debug for AuthorizedInventoryPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthorizedInventoryPath(<redacted>)")
    }
}

fn deepest_existing_directory(path: &Path) -> AdapterResult<(PathBuf, Vec<std::ffi::OsString>)> {
    let mut current = PathBuf::from("/");
    let mut components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .peekable();
    while let Some(component) = components.next() {
        let candidate = current.join(&component);
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid_input("authorized path contains a symlink"));
            }
            Ok(metadata) if metadata.is_dir() => current = candidate,
            Ok(_) if components.peek().is_none() => {
                return Ok((current, vec![component]));
            }
            Ok(_) => return Err(invalid_input("authorized path parent is not a directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut suffix = vec![component];
                suffix.extend(components);
                return Ok((current, suffix));
            }
            Err(_) => return Err(invalid_input("authorized path cannot be inspected")),
        }
    }
    Ok((current, Vec::new()))
}

fn file_identity(metadata: &fs::Metadata) -> FileIdentityV1 {
    FileIdentityV1 {
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    }
}

#[derive(Clone, PartialEq, Eq)]
struct ExactSourceBytesV1 {
    bytes: Vec<u8>,
    content_hash: Sha256ValueV1,
    fingerprint: Sha256ValueV1,
}

impl fmt::Debug for ExactSourceBytesV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExactSourceBytesV1")
            .field("byte_len", &self.bytes.len())
            .field("content_hash", &self.content_hash)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

impl ExactSourceBytesV1 {
    fn new(bytes: Vec<u8>) -> Self {
        let content_hash = Sha256ValueV1::digest(&bytes);
        let mut fingerprint = Vec::new();
        fingerprint.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        fingerprint.extend_from_slice(content_hash.as_str().as_bytes());
        Self {
            bytes,
            content_hash,
            fingerprint: Sha256ValueV1::digest(&fingerprint),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthorizedFileObservationV1 {
    NotFound,
    Present(ExactSourceBytesV1),
    Invalid { diagnostic_code: String },
}

#[derive(Clone, PartialEq, Eq)]
struct ExactDecodedSourceV1<T> {
    source: ExactSourceBytesV1,
    value: T,
    was_missing: bool,
}

impl<T: fmt::Debug> fmt::Debug for ExactDecodedSourceV1<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExactDecodedSourceV1")
            .field("source", &self.source)
            .field("value", &self.value)
            .field("was_missing", &self.was_missing)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DecodedSourceObservationV1<T> {
    NotFound,
    Valid(ExactDecodedSourceV1<T>),
    Invalid {
        source: Option<ExactSourceBytesV1>,
        diagnostic_code: String,
    },
}

fn read_authorized_file(
    path: &AuthorizedInventoryPath,
    max_bytes: usize,
) -> AdapterResult<AuthorizedFileObservationV1> {
    read_authorized_file_with_hook(path, max_bytes, || {})
}

fn read_authorized_file_with_hook(
    path: &AuthorizedInventoryPath,
    max_bytes: usize,
    after_present_inspection: impl FnOnce(),
) -> AdapterResult<AuthorizedFileObservationV1> {
    path.ensure_authority()?;
    let refreshed = AuthorizedInventoryPath::new(path.as_path())?;
    match inspect_path(refreshed.as_path()) {
        InspectedPath::Missing => {
            refreshed.ensure_authority()?;
            path.ensure_authority()?;
            return Ok(AuthorizedFileObservationV1::NotFound);
        }
        InspectedPath::Symlinked => return Ok(invalid_file("source_path_symlinked")),
        InspectedPath::Unreadable => return Ok(invalid_file("source_path_unreadable")),
        InspectedPath::NonRegular | InspectedPath::Directory => {
            return Ok(invalid_file("source_path_non_regular"));
        }
        InspectedPath::Regular { len } if len > max_bytes as u64 => {
            return Ok(invalid_file("source_byte_limit_exceeded"));
        }
        InspectedPath::Regular { .. } => {}
    }
    after_present_inspection();
    let parent = refreshed
        .as_path()
        .parent()
        .ok_or_else(|| invalid_input("authorized file has no parent"))?;
    let name = refreshed
        .as_path()
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_input("authorized file has an invalid basename"))?;
    if refreshed.authority_path_for_read() != parent {
        return Ok(AuthorizedFileObservationV1::NotFound);
    }
    let bytes =
        match refreshed
            .authority
            .read_regular(name, max_bytes, "migration inventory source")
        {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                if refreshed.ensure_authority().is_err() || path.ensure_authority().is_err() {
                    return Ok(invalid_file("source_path_changed"));
                }
                return Ok(invalid_file("source_read_invalid"));
            }
            Err(_) => return Ok(invalid_file("source_read_invalid")),
        };
    if refreshed.authority.ensure_still_current().is_err() || path.ensure_authority().is_err() {
        return Ok(invalid_file("source_path_changed"));
    }
    Ok(AuthorizedFileObservationV1::Present(
        ExactSourceBytesV1::new(bytes),
    ))
}

enum InspectedPath {
    Missing,
    Symlinked,
    Unreadable,
    NonRegular,
    Directory,
    Regular { len: u64 },
}

fn inspect_path(path: &Path) -> InspectedPath {
    let mut current = PathBuf::from("/");
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return InspectedPath::Missing;
            }
            Err(_) => return InspectedPath::Unreadable,
        };
        if metadata.file_type().is_symlink() {
            return InspectedPath::Symlinked;
        }
        let last = index + 1 == components.len();
        if !last && !metadata.is_dir() {
            return InspectedPath::NonRegular;
        }
        if last {
            return if metadata.is_dir() {
                InspectedPath::Directory
            } else if metadata.is_file() {
                InspectedPath::Regular {
                    len: metadata.len(),
                }
            } else {
                InspectedPath::NonRegular
            };
        }
    }
    InspectedPath::Directory
}

fn invalid_file(code: &str) -> AuthorizedFileObservationV1 {
    AuthorizedFileObservationV1::Invalid {
        diagnostic_code: code.to_string(),
    }
}

fn capture_legacy_projects_source(
    path: &AuthorizedInventoryPath,
) -> AdapterResult<DecodedSourceObservationV1<LegacyProjectStoreV1>> {
    Ok(decode_source(
        read_authorized_file(path, MAX_LEGACY_PROJECT_STORE_BYTES)?,
        |bytes| decode_legacy_project_store(bytes).map_err(|_| ()),
        "legacy_projects_invalid",
    ))
}

fn accept_missing_legacy_projects_source(
    observed: DecodedSourceObservationV1<LegacyProjectStoreV1>,
) -> AdapterResult<ExactDecodedSourceV1<LegacyProjectStoreV1>> {
    match observed {
        DecodedSourceObservationV1::NotFound => Ok(ExactDecodedSourceV1 {
            source: ExactSourceBytesV1::new(Vec::new()),
            value: LegacyProjectStoreV1::default(),
            was_missing: true,
        }),
        DecodedSourceObservationV1::Valid(source) => Ok(source),
        DecodedSourceObservationV1::Invalid { .. } => {
            Err(invalid_source("legacy_projects_invalid"))
        }
    }
}

struct InventoryLegacySourceV1 {
    exact: ExactDecodedSourceV1<LegacyProjectStoreV1>,
    state: InventorySourceStateV1,
}

fn accept_legacy_projects_source_for_inventory(
    observed: DecodedSourceObservationV1<LegacyProjectStoreV1>,
) -> InventoryLegacySourceV1 {
    match observed {
        DecodedSourceObservationV1::NotFound => {
            let exact = ExactDecodedSourceV1 {
                source: ExactSourceBytesV1::new(Vec::new()),
                value: LegacyProjectStoreV1::default(),
                was_missing: true,
            };
            InventoryLegacySourceV1 {
                state: InventorySourceStateV1::Missing {
                    fingerprint: missing_source_fingerprint("legacy-project-store"),
                },
                exact,
            }
        }
        DecodedSourceObservationV1::Valid(exact) => InventoryLegacySourceV1 {
            state: present_source_state(&exact.source),
            exact,
        },
        DecodedSourceObservationV1::Invalid {
            source,
            diagnostic_code,
        } => {
            let fingerprint = source
                .as_ref()
                .map(|source| source.fingerprint.clone())
                .unwrap_or_else(|| missing_source_fingerprint("legacy-project-store"));
            let content_hash = source.as_ref().map(|source| source.content_hash.clone());
            InventoryLegacySourceV1 {
                exact: ExactDecodedSourceV1 {
                    source: source.unwrap_or_else(|| ExactSourceBytesV1::new(Vec::new())),
                    value: LegacyProjectStoreV1::default(),
                    was_missing: false,
                },
                state: InventorySourceStateV1::Corrupt {
                    fingerprint,
                    content_hash,
                    diagnostic_code,
                },
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublisherRefInventoryV1 {
    pub rows: Vec<PublisherRefRow>,
}

struct LockedPublisherRefSourceV1 {
    source: ExactDecodedSourceV1<PublisherRefInventoryV1>,
    _owner: MigrationPublisherRefSnapshotV1,
}

fn capture_publisher_ref_source(
    store: &PublisherRefStore,
) -> AdapterResult<LockedPublisherRefSourceV1> {
    let snapshot = store
        .snapshot_migration_source()
        .map_err(|_| invalid_source("publisher_refs_invalid"))?;
    let source = ExactDecodedSourceV1 {
        source: ExactSourceBytesV1::new(snapshot.bytes.clone()),
        value: PublisherRefInventoryV1 {
            rows: snapshot.rows.clone(),
        },
        was_missing: snapshot.was_missing,
    };
    Ok(LockedPublisherRefSourceV1 {
        source,
        _owner: snapshot,
    })
}

#[derive(Debug, Clone)]
struct CommittedConfigSourceV1 {
    pub repository_root: PathBuf,
    pub commit: VerifiedCommit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommittedAuthorityProbeV1 {
    source_evidence: MutableInventorySourceEvidenceV1,
    pub authority: Option<RecordedRepoAuthority>,
    pub published_scope: Option<PublishedScope>,
}

fn observe_committed_authority_probe(
    source_id: &str,
    project_id: &ProjectId,
    project_root: &AuthorizedInventoryPath,
    source: Option<&CommittedConfigSourceV1>,
) -> AdapterResult<CommittedAuthorityProbeV1> {
    let Some(source) = source else {
        return Ok(CommittedAuthorityProbeV1 {
            source_evidence: source_evidence(
                source_id,
                MutableInventorySourceKindV1::CommittedAuthorityProbe,
                MutableInventorySourceLocatorV1::CommittedProjectConfigUnavailable {
                    project_id: project_id.clone(),
                },
                InventorySourceStateV1::Missing {
                    fingerprint: missing_source_fingerprint(source_id),
                },
                BTreeSet::new(),
            ),
            authority: None,
            published_scope: None,
        });
    };
    let relative_root = project_root
        .as_path()
        .strip_prefix(&source.repository_root)
        .map_err(|_| invalid_input("project root is outside committed repository authority"))?;
    let relative_root = relative_root
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .ok_or_else(|| invalid_input("project root relation is not utf8"))
        })
        .collect::<AdapterResult<Vec<_>>>()?
        .join("/");
    let repo_relative_path = if relative_root.is_empty() {
        ".bbox/config.toml".to_string()
    } else {
        format!("{relative_root}/.bbox/config.toml")
    };
    let bytes = read_verified_committed_file_bytes_optional_bounded(
        &source.commit,
        &repo_relative_path,
        MAX_COMMITTED_CONFIG_BYTES,
    )
    .map_err(|_| invalid_source("committed_config_read_invalid"))?;
    let locator = MutableInventorySourceLocatorV1::CommittedProjectConfig {
        project_id: project_id.clone(),
        commit_oid: source.commit.oid().to_string(),
        repo_relative_path,
    };
    let Some(bytes) = bytes else {
        return Ok(CommittedAuthorityProbeV1 {
            source_evidence: source_evidence(
                source_id,
                MutableInventorySourceKindV1::CommittedAuthorityProbe,
                locator,
                InventorySourceStateV1::Missing {
                    fingerprint: missing_source_fingerprint(source_id),
                },
                BTreeSet::new(),
            ),
            authority: None,
            published_scope: None,
        });
    };
    let exact = ExactSourceBytesV1::new(bytes);
    let text = std::str::from_utf8(&exact.bytes)
        .map_err(|_| invalid_source("committed_config_not_utf8"))?;
    let inputs = bbox_config::config::repo_id_inputs_from_project_config_source(
        project_root.as_path(),
        text,
    )
    .map_err(|_| invalid_source("committed_config_invalid"))?;
    let authority = resolve_recorded_repo_id(&inputs)
        .map(|value| RecordedRepoAuthority::parse(value).map_err(|_| ()))
        .transpose()
        .map_err(|_| invalid_source("committed_authority_invalid"))?;
    let published_scope = authority
        .as_ref()
        .map(|authority| {
            PublishedScope::try_new(
                authority.as_str(),
                if relative_root.is_empty() {
                    "."
                } else {
                    relative_root.as_str()
                },
            )
        })
        .transpose()
        .map_err(|_| invalid_source("committed_scope_invalid"))?;
    Ok(CommittedAuthorityProbeV1 {
        source_evidence: source_evidence(
            source_id,
            MutableInventorySourceKindV1::CommittedAuthorityProbe,
            locator,
            present_source_state(&exact),
            BTreeSet::new(),
        ),
        authority,
        published_scope,
    })
}

#[derive(Debug, Clone)]
struct LegacyProjectProbeInputV1 {
    pub project_id: ProjectId,
    pub authorized_canonical_path: AuthorizedInventoryPath,
    pub repository: Option<StableGitRepository>,
    pub committed_config: Option<CommittedConfigSourceV1>,
}

#[derive(Debug, Clone)]
struct LegacyProjectsCaptureV1 {
    observations: Vec<LegacyProjectObservationV1>,
    source_evidence: Vec<MutableInventorySourceEvidenceV1>,
    owner_state: InventorySourceStateV1,
    published_scopes: BTreeMap<ProjectId, PublishedScope>,
    project_roots: BTreeMap<ProjectId, AuthorizedInventoryPath>,
    repositories: BTreeMap<ProjectId, StableGitRepository>,
    runtime_project_paths: BTreeMap<String, AuthorizedInventoryPath>,
}

fn derive_legacy_project_probes(
    source: &ExactDecodedSourceV1<LegacyProjectStoreV1>,
    rehearsal_root: Option<&Path>,
) -> AdapterResult<Vec<LegacyProjectProbeInputV1>> {
    derive_legacy_project_probes_with_hook(source, rehearsal_root, |_| {})
}

fn derive_legacy_project_probes_with_hook(
    source: &ExactDecodedSourceV1<LegacyProjectStoreV1>,
    rehearsal_root: Option<&Path>,
    mut after_repository_open: impl FnMut(&StableGitRepository),
) -> AdapterResult<Vec<LegacyProjectProbeInputV1>> {
    source
        .value
        .projects
        .iter()
        .map(|record| {
            let project_id = ProjectId::parse(record.project_id.clone())
                .map_err(|_| invalid_source("legacy_project_id_invalid"))?;
            let project_root = AuthorizedInventoryPath::new(&record.canonical_path)?;
            validate_authorized_containment(std::slice::from_ref(&project_root), rehearsal_root)?;
            let repository = if record.is_git_repo
                && matches!(
                    inspect_path(project_root.as_path()),
                    InspectedPath::Directory
                ) {
                open_stable_git_repository(&project_root.authority)
                    .map_err(|_| invalid_source("stable_git_repository_open_failed"))?
            } else {
                None
            };
            if let Some(repository) = &repository {
                validate_stable_repository_containment(repository, rehearsal_root)?;
                after_repository_open(repository);
                project_root.ensure_authority()?;
            }
            let committed_config = repository
                .as_ref()
                .map(|repository| {
                    repository
                        .verified_head()
                        .map_err(|_| invalid_source("committed_config_commit_invalid"))
                })
                .transpose()?
                .flatten()
                .map(|commit| CommittedConfigSourceV1 {
                    repository_root: repository
                        .as_ref()
                        .expect("verified commit has a repository authority")
                        .repository_root()
                        .to_path_buf(),
                    commit,
                });
            Ok(LegacyProjectProbeInputV1 {
                project_id,
                authorized_canonical_path: project_root,
                repository,
                committed_config,
            })
        })
        .collect()
}

fn observe_legacy_projects(
    source: &ExactDecodedSourceV1<LegacyProjectStoreV1>,
    probes: Vec<LegacyProjectProbeInputV1>,
) -> AdapterResult<LegacyProjectsCaptureV1> {
    let probe_count = probes.len();
    let mut probes = probes
        .into_iter()
        .map(|probe| (probe.project_id.clone(), probe))
        .collect::<BTreeMap<_, _>>();
    if probe_count != probes.len() || probes.len() != source.value.projects.len() {
        return Err(invalid_input("legacy project probes are not exact"));
    }
    let mut observations = Vec::new();
    let mut source_evidence = Vec::new();
    let mut published_scopes = BTreeMap::new();
    let mut project_roots = BTreeMap::new();
    let mut repositories = BTreeMap::new();
    let mut runtime_project_paths = BTreeMap::new();
    for record in &source.value.projects {
        let project_id = ProjectId::parse(record.project_id.clone())
            .map_err(|_| invalid_source("legacy_project_id_invalid"))?;
        let probe = probes
            .remove(&project_id)
            .ok_or_else(|| invalid_input("legacy project probe is missing"))?;
        if probe.authorized_canonical_path.as_path() != Path::new(&record.canonical_path) {
            return Err(invalid_input("legacy project path authorization disagrees"));
        }
        let path_status = match inspect_path(probe.authorized_canonical_path.as_path()) {
            InspectedPath::Missing => LegacyProjectPathStatusV1::Missing,
            InspectedPath::Directory => LegacyProjectPathStatusV1::Present,
            _ => return Err(invalid_source("legacy_project_path_invalid")),
        };
        let observation_id =
            stable_observation_id_v1("legacy-project", &[project_id.as_str().as_bytes()])?;
        let authority_source_id =
            stable_observation_id_v1("committed-config", &[project_id.as_str().as_bytes()])?;
        let mut authority_probe = observe_committed_authority_probe(
            &authority_source_id,
            &project_id,
            &probe.authorized_canonical_path,
            probe.committed_config.as_ref(),
        )?;
        let committed_authority = authority_probe.authority.map(|authority| {
            crate::project_catalog_inventory::CommittedAuthorityObservationV1 {
                observation_id: stable_observation_id_v1(
                    "committed-authority",
                    &[
                        project_id.as_str().as_bytes(),
                        authority.as_str().as_bytes(),
                    ],
                )
                .expect("code-owned observation kind is valid"),
                authority,
            }
        });
        let authority_row_id = committed_authority
            .as_ref()
            .map_or_else(|| observation_id.clone(), |row| row.observation_id.clone());
        authority_probe.source_evidence.row_observation_ids = BTreeSet::from([authority_row_id]);
        authority_probe.source_evidence.row_set_sha256 =
            mutable_source_row_set_hash(&authority_probe.source_evidence.row_observation_ids);
        let committed_scope = authority_probe.published_scope.clone();
        if let Some(scope) = authority_probe.published_scope {
            published_scopes.insert(project_id.clone(), scope);
        }
        project_roots.insert(project_id.clone(), probe.authorized_canonical_path.clone());
        if let Some(repository) = probe.repository {
            repositories.insert(project_id.clone(), repository);
        }
        runtime_project_paths.insert(observation_id.clone(), probe.authorized_canonical_path);
        source_evidence.push(authority_probe.source_evidence);
        observations.push(LegacyProjectObservationV1 {
            observation_id,
            record: LegacyProjectRecordInventoryV1::from_legacy(record.clone()),
            path_status,
            committed_authority,
            committed_scope,
        });
    }
    observations.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    source_evidence.sort_by(|left, right| left.source_id.cmp(&right.source_id));
    Ok(LegacyProjectsCaptureV1 {
        observations,
        source_evidence,
        owner_state: if source.was_missing {
            // A no-follow observed absence accepted by the first-install
            // bootstrap is complete evidence for the empty v1 owner. Preserve
            // the physical absence separately in `was_missing` and mutable
            // source evidence, but do not turn a complete empty owner into an
            // incomplete immutable lane.
            InventorySourceStateV1::Present {
                fingerprint: missing_source_fingerprint("legacy-project-store"),
                content_hash: source.source.content_hash.clone(),
                byte_len: 0,
            }
        } else {
            present_source_state(&source.source)
        },
        published_scopes,
        project_roots,
        repositories,
        runtime_project_paths,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublisherPinsCaptureV1 {
    bound: Vec<PublisherPinObservationV1>,
    unbound: Vec<UnboundPublisherPinObservationV1>,
}

fn verified_commit_declares_scope(
    repository: &StableGitRepository,
    commit_oid: &str,
    root_relpath: &str,
    expected_scope: &PublishedScope,
) -> bool {
    let Ok(commit) = repository.verify_commit_oid(commit_oid) else {
        return false;
    };
    let config_relpath = if root_relpath == "." || root_relpath.is_empty() {
        ".bbox/config.toml".to_string()
    } else {
        format!("{root_relpath}/.bbox/config.toml")
    };
    let Ok(Some(bytes)) = read_verified_committed_file_bytes_optional_bounded(
        &commit,
        &config_relpath,
        MAX_COMMITTED_CONFIG_BYTES,
    ) else {
        return false;
    };
    let Ok(source) = std::str::from_utf8(&bytes) else {
        return false;
    };
    let project_root = repository.repository_root().join(
        (root_relpath != ".")
            .then_some(root_relpath)
            .unwrap_or_default(),
    );
    let Ok(inputs) =
        bbox_config::config::repo_id_inputs_from_project_config_source(&project_root, source)
    else {
        return false;
    };
    let Some(repo_id) = resolve_recorded_repo_id(&inputs) else {
        return false;
    };
    PublishedScope::try_new(
        repo_id,
        if root_relpath.is_empty() {
            "."
        } else {
            root_relpath
        },
    )
    .is_ok_and(|scope| &scope == expected_scope)
}

fn derive_publisher_pins(
    source: &ExactDecodedSourceV1<PublisherRefInventoryV1>,
    legacy: &LegacyProjectsCaptureV1,
    project_authority_scopes: &BTreeMap<ProjectId, PublishedScope>,
    lanes: &ImmutableInventoryLanesV1,
) -> AdapterResult<PublisherPinsCaptureV1> {
    let mut rows = Vec::new();
    let mut unbound = Vec::new();
    let git_lane_complete = matches!(
        lanes.git_metadata.evidence.completeness,
        crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Complete
    );
    for publisher in &source.value.rows {
        let owners = project_authority_scopes
            .iter()
            .filter(|(_, scope)| **scope == publisher.scope)
            .map(|(project_id, _)| project_id.clone())
            .collect::<BTreeSet<_>>();
        if owners.len() != 1 {
            let observation_id = stable_observation_id_v1(
                "unbound-publisher-pin",
                &[
                    publisher.scope.repo_id().as_bytes(),
                    publisher.scope.bbox_root_relpath().as_bytes(),
                    publisher.branch_ref.as_bytes(),
                ],
            )?;
            unbound.push(UnboundPublisherPinObservationV1 {
                observation_id,
                expected_scope: publisher.scope.clone(),
                full_ref: publisher.branch_ref.clone(),
                candidate_project_ids: owners.clone(),
                reason: if owners.is_empty() {
                    UnboundPublisherPinReasonV1::OwnerlessScope
                } else {
                    UnboundPublisherPinReasonV1::DuplicateScopeOwners
                },
            });
            continue;
        }
        let project_id = owners.first().expect("one publisher scope owner").clone();
        let project = legacy
            .observations
            .iter()
            .find(|row| row.record.project_id == project_id.as_str())
            .ok_or_else(|| invalid_source("publisher_project_observation_missing"))?;
        let repository = legacy.repositories.get(&project_id);
        let resolved_commit = match repository {
            Some(repository) => repository
                .resolve_commit_oid(&publisher.branch_ref)
                .map_err(|_| invalid_source("publisher_ref_resolution_unavailable"))?,
            None => None,
        };
        let root_relpath = legacy
            .project_roots
            .get(&project_id)
            .and_then(|root| {
                repository.and_then(|repository| {
                    root.as_path()
                        .strip_prefix(repository.repository_root())
                        .ok()
                })
            })
            .and_then(|relative| relative.to_str())
            .map(|relative| {
                if relative.is_empty() {
                    ".".to_string()
                } else {
                    relative.replace('\\', "/")
                }
            });
        let resolved_scope = resolved_commit.as_ref().and_then(|commit| {
            repository
                .zip(root_relpath.as_deref())
                .filter(|(repository, root_relpath)| {
                    verified_commit_declares_scope(
                        repository,
                        commit,
                        root_relpath,
                        &publisher.scope,
                    )
                })
                .map(|_| publisher.scope.clone())
        });
        let observation_id = stable_observation_id_v1(
            "publisher-pin",
            &[
                project_id.as_str().as_bytes(),
                publisher.scope.repo_id().as_bytes(),
                publisher.scope.bbox_root_relpath().as_bytes(),
                publisher.branch_ref.as_bytes(),
            ],
        )?;
        let mut source_observation_ids =
            BTreeSet::from([observation_id.clone(), project.observation_id.clone()]);
        if let Some(authority) = &project.committed_authority {
            source_observation_ids.insert(authority.observation_id.clone());
        }
        let candidates = lanes
            .attachment_candidates
            .rows
            .iter()
            .filter(|attachment| {
                attachment.project_id == project_id
                    && attachment.observed_scope.as_ref() == Some(&publisher.scope)
            })
            .collect::<Vec<_>>();
        let candidate_attachment_ids = candidates
            .iter()
            .map(|attachment| attachment.attachment_id.clone())
            .collect::<BTreeSet<_>>();
        let mut provenance_git_ids = BTreeSet::new();
        for attachment in candidates {
            if !lanes
                .checkouts
                .rows
                .iter()
                .any(|checkout| checkout.observation_id == attachment.checkout_observation_id)
            {
                return Err(invalid_source(
                    "publisher_candidate_checkout_evidence_missing",
                ));
            }
            let matching_git = lanes
                .git_metadata
                .rows
                .iter()
                .filter(|row| {
                    row.project_id == project_id
                        && row.checkout_observation_id == attachment.checkout_observation_id
                        && row.resolved_refs.get(&publisher.branch_ref) == resolved_commit.as_ref()
                })
                .collect::<Vec<_>>();
            if git_lane_complete && resolved_scope.is_some() && matching_git.len() != 1 {
                return Err(invalid_source(
                    "publisher_candidate_git_provenance_not_unique",
                ));
            }
            source_observation_ids.insert(attachment.observation_id.clone());
            source_observation_ids.insert(attachment.checkout_observation_id.clone());
            if let Some(matching_git) = matching_git.first() {
                provenance_git_ids.insert(matching_git.observation_id.clone());
            }
        }
        if git_lane_complete && resolved_scope.is_some() {
            if provenance_git_ids.is_empty() {
                provenance_git_ids.extend(
                    lanes
                        .git_metadata
                        .rows
                        .iter()
                        .filter(|row| {
                            row.project_id == project_id
                                && row.resolved_refs.get(&publisher.branch_ref)
                                    == resolved_commit.as_ref()
                        })
                        .map(|row| row.observation_id.clone()),
                );
            }
            if provenance_git_ids.is_empty() {
                return Err(invalid_source("publisher_git_evidence_missing"));
            }
        }
        source_observation_ids.extend(provenance_git_ids);
        rows.push(PublisherPinObservationV1 {
            observation_id,
            project_id,
            expected_scope: publisher.scope.clone(),
            full_ref: publisher.branch_ref.clone(),
            candidate_attachment_ids,
            resolved_scope,
            resolved_commit,
            source_observation_ids,
        });
    }
    rows.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    unbound.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    Ok(PublisherPinsCaptureV1 {
        bound: rows,
        unbound,
    })
}

struct CodeSourceInventorySnapshotV1<'a> {
    anchor_source: ExactSourceBytesV1,
    anchor_missing: bool,
    owner: MigrationOwnedLegacyInventoryV1<'a>,
}

fn capture_code_source_inventory<'a>(
    store: &'a CodeSourceStore,
    catalog_scopes: &BTreeSet<PublishedScope>,
) -> AdapterResult<CodeSourceInventorySnapshotV1<'a>> {
    let owned = store
        .snapshot_legacy_migration_for_scopes(catalog_scopes)
        .map_err(|_| invalid_source("code_source_inventory_invalid"))?;
    owned
        .inventory
        .validate_evidence()
        .map_err(|_| invalid_source("code_source_inventory_evidence_invalid"))?;
    let (anchor_source, anchor_missing) = match &owned.inventory.anchor {
        MigrationLegacyAnchorEvidenceV1::Missing => (ExactSourceBytesV1::new(Vec::new()), true),
        MigrationLegacyAnchorEvidenceV1::Present { bytes, .. } => {
            (ExactSourceBytesV1::new(bytes.clone()), false)
        }
    };
    Ok(CodeSourceInventorySnapshotV1 {
        anchor_source,
        anchor_missing,
        owner: owned,
    })
}

#[derive(Debug, Clone)]
struct CodeSourceCaptureV1 {
    observation: CodeSourceObservationV1,
    source_evidence: Vec<MutableInventorySourceEvidenceV1>,
}

#[derive(Debug, Clone)]
struct CodeSourceInventoryCaptureV1 {
    sources: Vec<CodeSourceCaptureV1>,
    project_authority_scopes: BTreeMap<ProjectId, PublishedScope>,
    retained_owner_resolutions: Vec<RetainedGenerationOwnerResolutionObservationV1>,
    retained_owner_source_evidence: Vec<MutableInventorySourceEvidenceV1>,
}

fn observe_code_sources(
    snapshot: &CodeSourceInventorySnapshotV1<'_>,
    project_scopes: &BTreeMap<ProjectId, PublishedScope>,
    missing_checkout_projects: &BTreeSet<ProjectId>,
) -> AdapterResult<CodeSourceInventoryCaptureV1> {
    let generations_by_id = snapshot
        .owner
        .inventory
        .generations
        .iter()
        .map(|row| (row.generation_id.clone(), row))
        .collect::<BTreeMap<_, _>>();
    let mut owner_by_generation = BTreeMap::<String, ProjectId>::new();
    let effective_by_project = match &snapshot.owner.inventory.anchor {
        MigrationLegacyAnchorEvidenceV1::Missing => BTreeMap::new(),
        MigrationLegacyAnchorEvidenceV1::Present { bytes, .. } => {
            let manifest = decode_migration_effective_source_manifest_v1(bytes)
                .map_err(|_| invalid_source("effective_source_manifest_invalid"))?;
            let count = manifest.selections.len();
            let rows = manifest
                .selections
                .into_iter()
                .map(|selection| (selection.project_id.clone(), selection))
                .collect::<BTreeMap<_, _>>();
            if rows.len() != count {
                return Err(invalid_source("effective_source_project_duplicate"));
            }
            rows
        }
    };
    let mut project_authority_scopes = project_scopes.clone();
    for activation in &snapshot.owner.inventory.activations {
        let selection = effective_by_project
            .get(&activation.project_id)
            .ok_or_else(|| invalid_source("activation_effective_selection_missing"))?;
        let generation = generations_by_id
            .get(&activation.record.generation_id)
            .ok_or_else(|| invalid_source("activation_generation_missing"))?;
        if selection.generation_id != activation.record.generation_id
            || selection.selector != activation.record.selector
            || selection.published_scope != generation.published_scope
        {
            return Err(invalid_source(
                "descriptor_activation_effective_evidence_mismatch",
            ));
        }
        if project_scopes
            .get(&activation.project_id)
            .is_some_and(|scope| scope != &selection.published_scope)
        {
            return Err(invalid_source("active_descriptor_committed_scope_mismatch"));
        }
        match project_authority_scopes.get(&activation.project_id) {
            Some(scope) if scope != &selection.published_scope => {
                return Err(invalid_source(
                    "active_descriptor_retained_owner_scope_mismatch",
                ));
            }
            Some(_) => {}
            None if missing_checkout_projects.contains(&activation.project_id) => {
                project_authority_scopes.insert(
                    activation.project_id.clone(),
                    selection.published_scope.clone(),
                );
            }
            None => {}
        }
        insert_generation_owner(
            &mut owner_by_generation,
            &activation.record.generation_id,
            &activation.project_id,
        )?;
    }
    if effective_by_project.len() != snapshot.owner.inventory.activations.len() {
        return Err(invalid_source(
            "effective_selection_activation_set_mismatch",
        ));
    }
    for collision in &snapshot.owner.inventory.collision_pending {
        for generation_id in collision.record.entries.keys() {
            insert_generation_owner(
                &mut owner_by_generation,
                generation_id,
                &collision.project_id,
            )?;
        }
    }
    let mut retained_owner_resolutions = Vec::new();
    let mut retained_owner_source_evidence = Vec::new();
    for generation in &snapshot.owner.inventory.generations {
        if owner_by_generation.contains_key(&generation.generation_id) {
            continue;
        }
        let candidates = project_authority_scopes
            .iter()
            .filter(|(_, scope)| **scope == generation.published_scope)
            .map(|(project_id, _)| project_id)
            .collect::<Vec<_>>();
        if candidates.len() > 1 {
            let observation_id = stable_observation_id_v1(
                "retained-owner-resolution",
                &[
                    generation.generation_id.as_bytes(),
                    bbox_code_source::scope_hash(&generation.published_scope).as_bytes(),
                ],
            )?;
            let (descriptor, manifest, planned_metadata_v2_hash) =
                describe_generation(generation, &snapshot.owner.limits)?;
            retained_owner_resolutions.push(RetainedGenerationOwnerResolutionObservationV1 {
                observation_id: observation_id.clone(),
                generation_id: generation.generation_id.clone(),
                published_scope: generation.published_scope.clone(),
                candidate_project_ids: candidates.into_iter().cloned().collect(),
                descriptor,
                manifest,
                selector_evidence: DurableSelectorEvidenceV1::NoDurableSelector,
                planned_metadata_v2_hash,
            });
            let scope_hash =
                Sha256ValueV1::parse(bbox_code_source::scope_hash(&generation.published_scope))
                    .map_err(|_| invalid_source("code_source_scope_hash_invalid"))?;
            let row_ids = BTreeSet::from([observation_id]);
            retained_owner_source_evidence.push(source_evidence(
                &stable_observation_id_v1(
                    "generation-metadata",
                    &[
                        b"retained-owner-resolution",
                        generation.generation_id.as_bytes(),
                    ],
                )?,
                MutableInventorySourceKindV1::CodeSourceGenerationMetadata,
                MutableInventorySourceLocatorV1::CodeSourceGenerationMetadata {
                    scope_hash: scope_hash.clone(),
                    generation_id: generation.generation_id.clone(),
                },
                present_bytes_state(&generation.metadata_bytes),
                row_ids.clone(),
            ));
            retained_owner_source_evidence.push(source_evidence(
                &stable_observation_id_v1(
                    "generation-manifest",
                    &[
                        b"retained-owner-resolution",
                        generation.generation_id.as_bytes(),
                    ],
                )?,
                MutableInventorySourceKindV1::CodeSourceGenerationManifest,
                MutableInventorySourceLocatorV1::CodeSourceGenerationManifest {
                    scope_hash,
                    generation_id: generation.generation_id.clone(),
                },
                present_bytes_state(&generation.manifest_bytes),
                row_ids,
            ));
            continue;
        }
        if candidates.is_empty() {
            return Err(invalid_source("retained_generation_owner_missing"));
        }
        owner_by_generation.insert(generation.generation_id.clone(), candidates[0].clone());
    }
    for collision in &snapshot.owner.inventory.collision_pending {
        let expected_generation_ids = owner_by_generation
            .iter()
            .filter_map(|(generation_id, project_id)| {
                (project_id == &collision.project_id).then_some(generation_id.clone())
            })
            .collect::<BTreeSet<_>>();
        if collision
            .record
            .entries
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            != expected_generation_ids
        {
            return Err(invalid_source(
                "collision_lifecycle_owner_generation_set_mismatch",
            ));
        }
    }
    let mut collision_by_generation = BTreeMap::new();
    for row in &snapshot.owner.inventory.collision_pending {
        for (generation_id, entry) in &row.record.entries {
            if collision_by_generation
                .insert(
                    (row.project_id.clone(), generation_id.clone()),
                    (row, entry),
                )
                .is_some()
            {
                return Err(invalid_source(
                    "collision_lifecycle_generation_is_duplicated",
                ));
            }
        }
    }
    let ambiguous_generation_ids = retained_owner_resolutions
        .iter()
        .map(|row| row.generation_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut grouped = BTreeMap::<ProjectId, Vec<&MigrationLegacyGenerationEvidenceV1>>::new();
    for generation in &snapshot.owner.inventory.generations {
        let Some(project_id) = owner_by_generation.get(&generation.generation_id) else {
            if ambiguous_generation_ids.contains(generation.generation_id.as_str()) {
                continue;
            }
            return Err(invalid_source("protected_generation_owner_missing"));
        };
        grouped
            .entry(project_id.clone())
            .or_default()
            .push(generation);
    }
    for activation in &snapshot.owner.inventory.activations {
        grouped.entry(activation.project_id.clone()).or_default();
    }
    let mut captures = Vec::new();
    for (project_id, mut generation_rows) in grouped {
        generation_rows.sort_by(|left, right| left.generation_id.cmp(&right.generation_id));
        let activation = snapshot
            .owner
            .inventory
            .activations
            .iter()
            .find(|row| row.project_id == project_id);
        let source_observation_id =
            stable_observation_id_v1("code-source", &[project_id.as_str().as_bytes()])?;
        let active_generation_id = activation.map(|row| row.record.generation_id.as_str());
        let mut generations = Vec::new();
        let mut quarantine = Vec::new();
        let mut evidence = Vec::new();
        for generation in generation_rows {
            let observation_id = stable_observation_id_v1(
                if collision_by_generation
                    .contains_key(&(project_id.clone(), generation.generation_id.clone()))
                {
                    "quarantined-generation"
                } else {
                    "collected-generation"
                },
                &[
                    project_id.as_str().as_bytes(),
                    generation.generation_id.as_bytes(),
                ],
            )?;
            let (descriptor, manifest, planned_metadata_v2_hash) =
                describe_generation(generation, &snapshot.owner.limits)?;
            let row_ids = BTreeSet::from([observation_id.clone()]);
            let scope_hash =
                Sha256ValueV1::parse(bbox_code_source::scope_hash(&generation.published_scope))
                    .map_err(|_| invalid_source("code_source_scope_hash_invalid"))?;
            evidence.push(source_evidence(
                &stable_observation_id_v1(
                    "generation-metadata",
                    &[
                        project_id.as_str().as_bytes(),
                        generation.generation_id.as_bytes(),
                    ],
                )?,
                MutableInventorySourceKindV1::CodeSourceGenerationMetadata,
                MutableInventorySourceLocatorV1::CodeSourceGenerationMetadata {
                    scope_hash: scope_hash.clone(),
                    generation_id: generation.generation_id.clone(),
                },
                present_bytes_state(&generation.metadata_bytes),
                row_ids.clone(),
            ));
            evidence.push(source_evidence(
                &stable_observation_id_v1(
                    "generation-manifest",
                    &[
                        project_id.as_str().as_bytes(),
                        generation.generation_id.as_bytes(),
                    ],
                )?,
                MutableInventorySourceKindV1::CodeSourceGenerationManifest,
                MutableInventorySourceLocatorV1::CodeSourceGenerationManifest {
                    scope_hash,
                    generation_id: generation.generation_id.clone(),
                },
                present_bytes_state(&generation.manifest_bytes),
                row_ids,
            ));
            if let Some((collision, collision_entry)) =
                collision_by_generation.get(&(project_id.clone(), generation.generation_id.clone()))
            {
                quarantine.push(QuarantinedGenerationObservationV1 {
                    observation_id,
                    project_id: project_id.clone(),
                    generation_id: generation.generation_id.clone(),
                    descriptor,
                    manifest,
                    manifest_hash: Sha256ValueV1::parse(generation.manifest_sha256.clone())
                        .map_err(|_| invalid_source("generation_manifest_hash_invalid"))?,
                    planned_metadata_v2_hash,
                    collision_lifecycle: CollisionLifecycleObservationV1 {
                        version: collision.record.version,
                        state: match collision_entry.state {
                            CollisionRetirementLifecycleStateV1::Pending => {
                                CollisionLifecycleStateObservationV1::Pending
                            }
                            CollisionRetirementLifecycleStateV1::Queued => {
                                CollisionLifecycleStateObservationV1::Queued
                            }
                            CollisionRetirementLifecycleStateV1::Completed => {
                                CollisionLifecycleStateObservationV1::Completed
                            }
                        },
                        project_id: collision.record.project_id.clone(),
                        former_scope: collision_entry.former_scope.clone(),
                        generation_id: generation.generation_id.clone(),
                        selector_evidence: match &collision_entry.selector_evidence {
                            CollisionRetirementSelectorEvidenceV1::ExactMaterialized(selector) => {
                                DurableSelectorEvidenceV1::ExactMaterialized {
                                    selector_hash: Sha256ValueV1::digest(selector.as_bytes()),
                                }
                            }
                            CollisionRetirementSelectorEvidenceV1::NoDurableSelector => {
                                DurableSelectorEvidenceV1::NoDurableSelector
                            }
                        },
                        snapshot_id: collision_entry.snapshot_id.clone(),
                        manifest_sha256: Sha256ValueV1::parse(
                            collision_entry.manifest_sha256.clone(),
                        )
                        .map_err(|_| invalid_source("collision_manifest_hash_invalid"))?,
                        inventory_hash: Sha256ValueV1::parse(
                            collision_entry.inventory_hash.clone(),
                        )
                        .map_err(|_| invalid_source("collision_inventory_hash_invalid"))?,
                        plan_hash: Sha256ValueV1::parse(collision_entry.plan_hash.clone())
                            .map_err(|_| invalid_source("collision_plan_hash_invalid"))?,
                    },
                });
            } else {
                let active = active_generation_id == Some(generation.generation_id.as_str());
                let activation_scope = if active {
                    Some(
                        effective_by_project
                            .get(&project_id)
                            .ok_or_else(|| invalid_source("active_effective_selection_missing"))?
                            .published_scope
                            .clone(),
                    )
                } else {
                    None
                };
                let selector_evidence = if active {
                    DurableSelectorEvidenceV1::ExactMaterialized {
                        selector_hash: Sha256ValueV1::digest(
                            activation
                                .expect("active generation has activation")
                                .record
                                .selector
                                .as_bytes(),
                        ),
                    }
                } else {
                    DurableSelectorEvidenceV1::NoDurableSelector
                };
                generations.push(CollectedGenerationObservationV1 {
                    observation_id,
                    project_id: project_id.clone(),
                    role: if active {
                        CollectedGenerationRoleV1::Active
                    } else {
                        CollectedGenerationRoleV1::Retained
                    },
                    generation_id: generation.generation_id.clone(),
                    activation_scope,
                    descriptor,
                    manifest,
                    selector_evidence,
                    checkout_missing: missing_checkout_projects.contains(&project_id),
                    planned_metadata_v2_hash,
                });
            }
        }
        if !quarantine.is_empty() {
            let collision = snapshot
                .owner
                .inventory
                .collision_pending
                .iter()
                .find(|collision| collision.project_id == project_id)
                .ok_or_else(|| invalid_source("collision_lifecycle_missing"))?;
            evidence.push(source_evidence(
                &stable_observation_id_v1(
                    "collision-lifecycle",
                    &[project_id.as_str().as_bytes()],
                )?,
                MutableInventorySourceKindV1::CodeSourceCollisionLifecycle,
                MutableInventorySourceLocatorV1::CodeSourceCollisionLifecycle {
                    project_id: project_id.clone(),
                },
                present_bytes_state(&collision.bytes),
                quarantine
                    .iter()
                    .map(|generation| generation.observation_id.clone())
                    .collect(),
            ));
        }
        let planned_activation_v2_hash = match activation {
            Some(activation) if active_generation_id.is_some() => {
                let generation = generations_by_id
                    .get(&activation.record.generation_id)
                    .ok_or_else(|| invalid_source("activation_generation_missing"))?;
                if collision_by_generation
                    .contains_key(&(project_id.clone(), generation.generation_id.clone()))
                {
                    None
                } else {
                    Some(planned_activation_hash(&activation.record, generation)?)
                }
            }
            None => None,
            Some(_) => return Err(invalid_source("activation_generation_invalid")),
        };
        let activation_source_id =
            stable_observation_id_v1("activation-source", &[project_id.as_str().as_bytes()])?;
        let activation_state = activation.map_or_else(
            || InventorySourceStateV1::Missing {
                fingerprint: missing_source_fingerprint(&activation_source_id),
            },
            |row| present_bytes_state(&row.bytes),
        );
        evidence.push(source_evidence(
            &activation_source_id,
            MutableInventorySourceKindV1::CodeSourceActivation,
            MutableInventorySourceLocatorV1::CodeSourceActivation {
                project_id: project_id.clone(),
            },
            activation_state,
            BTreeSet::from([source_observation_id.clone()]),
        ));
        evidence.sort_by(|left, right| left.source_id.cmp(&right.source_id));
        generations.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        quarantine.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        captures.push(CodeSourceCaptureV1 {
            observation: CodeSourceObservationV1 {
                observation_id: source_observation_id,
                project_id,
                generations,
                quarantine,
                effective_manifest_hash: snapshot.anchor_source.content_hash.clone(),
                planned_activation_v2_hash,
            },
            source_evidence: evidence,
        });
    }
    captures.sort_by(|left, right| {
        left.observation
            .observation_id
            .cmp(&right.observation.observation_id)
    });
    retained_owner_resolutions
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    retained_owner_source_evidence.sort_by(|left, right| left.source_id.cmp(&right.source_id));
    Ok(CodeSourceInventoryCaptureV1 {
        sources: captures,
        project_authority_scopes,
        retained_owner_resolutions,
        retained_owner_source_evidence,
    })
}

fn insert_generation_owner(
    owners: &mut BTreeMap<String, ProjectId>,
    generation_id: &str,
    project_id: &ProjectId,
) -> AdapterResult<()> {
    if owners
        .insert(generation_id.to_string(), project_id.clone())
        .is_some_and(|prior| prior != *project_id)
    {
        return Err(invalid_source("generation_owner_conflict"));
    }
    Ok(())
}

fn describe_generation(
    generation: &MigrationLegacyGenerationEvidenceV1,
    limits: &StoreLimits,
) -> AdapterResult<(
    ImmutableCollectedDescriptorV1,
    ImmutableArtifactObservationV1,
    Sha256ValueV1,
)> {
    let converted = StoredGenerationV2::from_v1_for_migration(
        generation.record.clone(),
        generation.published_scope.clone(),
    )
    .map_err(|_| invalid_source("stored_generation_v2_conversion_invalid"))?;
    let converted_bytes = encode_stored_generation_v2_for_migration(&converted)
        .map_err(|_| invalid_source("stored_generation_v2_encode_invalid"))?;
    let descriptor_bytes = serde_json::to_vec(&generation.record.descriptor)
        .map_err(|_| invalid_source("stored_generation_descriptor_encode"))?;
    let descriptor = ImmutableCollectedDescriptorV1::Valid {
        descriptor_hash: Sha256ValueV1::digest(&descriptor_bytes),
        published_scope: generation.published_scope.clone(),
    };
    let manifest = match verify_generation_manifest_for_migration(
        &generation.manifest_bytes,
        &generation.record.descriptor,
        &generation.record.producer_id,
        &generation.generation_id,
        limits,
    ) {
        Ok(_) => ImmutableArtifactObservationV1::Valid {
            content_hash: Sha256ValueV1::digest(&generation.manifest_bytes),
        },
        Err(_) => ImmutableArtifactObservationV1::Corrupt {
            diagnostic_code: "collected_manifest_invalid".to_string(),
        },
    };
    Ok((
        descriptor,
        manifest,
        Sha256ValueV1::digest(&converted_bytes),
    ))
}

fn planned_activation_hash(
    activation: &ActivationRecord,
    generation: &MigrationLegacyGenerationEvidenceV1,
) -> AdapterResult<Sha256ValueV1> {
    if activation.generation_id != generation.generation_id
        || validate_collected_materialization_selector(
            activation.project_id.as_str(),
            &activation.generation_id,
            &activation.selector,
        )
        .is_err()
        || generation.record.state != GenerationState::Active
    {
        return Err(invalid_source("activation_generation_mismatch"));
    }
    let converted_generation = StoredGenerationV2::from_v1_for_migration(
        generation.record.clone(),
        generation.published_scope.clone(),
    )
    .map_err(|_| invalid_source("stored_generation_v2_conversion_invalid"))?;
    let converted =
        ActivationRecordV2::from_v1_for_migration(activation.clone(), &converted_generation)
            .map_err(|_| invalid_source("activation_v2_conversion_invalid"))?;
    let bytes = encode_activation_v2_for_migration(&converted)
        .map_err(|_| invalid_source("activation_v2_encode_invalid"))?;
    Ok(Sha256ValueV1::digest(&bytes))
}

#[derive(Debug, Clone)]
struct CheckoutCaptureV1 {
    observation: CheckoutObservationV1,
    runtime_root: AuthorizedInventoryPath,
    repository: Option<StableGitRepository>,
    root_source_evidence: MutableInventorySourceEvidenceV1,
    marker_source_evidence: MutableInventorySourceEvidenceV1,
}

fn capture_checkout_roots(
    roots: &[AuthorizedInventoryPath],
    rehearsal_root: Option<&Path>,
) -> AdapterResult<Vec<CheckoutCaptureV1>> {
    let mut captures = roots
        .iter()
        .map(|root| observe_checkout(root, rehearsal_root))
        .collect::<AdapterResult<Vec<_>>>()?;
    captures.sort_by(|left, right| {
        left.observation
            .observation_id
            .cmp(&right.observation.observation_id)
    });
    Ok(captures)
}

fn discover_attachment_candidate_keys_locked(
    legacy: &LegacyProjectsCaptureV1,
    checkout_captures: &[CheckoutCaptureV1],
) -> AdapterResult<Vec<AttachmentCandidateKeyV1>> {
    let mut keys = Vec::new();
    for project in &legacy.observations {
        if project.path_status != LegacyProjectPathStatusV1::Present {
            continue;
        }
        let project_id = ProjectId::parse(project.record.project_id.clone())
            .map_err(|_| invalid_source("legacy_project_id_invalid"))?;
        let project_root = legacy
            .project_roots
            .get(&project_id)
            .ok_or_else(|| invalid_source("legacy_project_root_missing"))?
            .as_path();
        for checkout in checkout_captures {
            let checkout_root = checkout.runtime_root.as_path();
            let Ok(relative) = project_root.strip_prefix(checkout_root) else {
                continue;
            };
            if relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(invalid_source("attachment_relative_path_invalid"));
            }
            let base_relpath = if relative.as_os_str().is_empty() {
                ".".to_string()
            } else {
                relative
                    .to_str()
                    .ok_or_else(|| invalid_source("attachment_relative_path_not_utf8"))?
                    .replace(std::path::MAIN_SEPARATOR, "/")
            };
            keys.push(AttachmentCandidateKeyV1 {
                project_id: project_id.clone(),
                checkout_observation_id: checkout.observation.observation_id.clone(),
                base_relpath,
            });
        }
    }
    keys.sort();
    if keys.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid_source("attachment_candidate_key_duplicate"));
    }
    Ok(keys)
}

fn observe_checkout(
    root: &AuthorizedInventoryPath,
    rehearsal_root: Option<&Path>,
) -> AdapterResult<CheckoutCaptureV1> {
    if !matches!(inspect_path(root.as_path()), InspectedPath::Directory) {
        return Err(invalid_source("checkout_root_invalid"));
    }
    let lease = NofollowDirectory::open_existing(root.as_path())
        .map_err(|_| invalid_source("checkout_root_lease_invalid"))?
        .ok_or_else(|| invalid_source("checkout_root_missing"))?;
    let literal_root = root
        .as_path()
        .to_str()
        .ok_or_else(|| invalid_input("checkout root is not utf8"))?;
    let root_digest = digest_path(literal_root);
    let root_fingerprint = directory_fingerprint(root.as_path(), &root_digest)?;
    let marker = read_authorized_file(
        &root.join(".bbox/local/checkout-id")?,
        MAX_CHECKOUT_MARKER_BYTES,
    )?;
    lease
        .ensure_still_current()
        .map_err(|_| invalid_source("checkout_root_changed"))?;
    if directory_fingerprint(root.as_path(), &root_digest)? != root_fingerprint {
        return Err(invalid_source("checkout_root_changed"));
    }
    let repository = open_stable_git_repository(&root.authority)
        .map_err(|_| invalid_source("stable_checkout_repository_open_failed"))?;
    if let Some(repository) = &repository {
        validate_stable_repository_containment(repository, rehearsal_root)?;
    }
    let observation_id = stable_observation_id_v1("checkout", &[root_digest.as_str().as_bytes()])?;
    let marker_source_id =
        stable_observation_id_v1("checkout-marker-source", &[root_digest.as_str().as_bytes()])?;
    let row_ids = BTreeSet::from([observation_id.clone()]);
    Ok(CheckoutCaptureV1 {
        observation: CheckoutObservationV1 {
            observation_id,
            canonical_root_digest: root_digest.clone(),
            marker_state: marker_state(&marker),
        },
        runtime_root: root.clone(),
        repository,
        root_source_evidence: source_evidence(
            &stable_observation_id_v1("checkout-root-source", &[root_digest.as_str().as_bytes()])?,
            MutableInventorySourceKindV1::CheckoutRoot,
            MutableInventorySourceLocatorV1::CheckoutRoot {
                canonical_root_digest: root_digest.clone(),
            },
            InventorySourceStateV1::Present {
                fingerprint: root_fingerprint.clone(),
                content_hash: root_fingerprint,
                byte_len: 0,
            },
            row_ids.clone(),
        ),
        marker_source_evidence: source_evidence(
            &marker_source_id,
            MutableInventorySourceKindV1::CheckoutMarker,
            MutableInventorySourceLocatorV1::CheckoutMarker {
                canonical_root_digest: root_digest,
            },
            file_observation_state(&marker, &marker_source_id),
            row_ids,
        ),
    })
}

fn directory_fingerprint(path: &Path, path_digest: &Sha256ValueV1) -> AdapterResult<Sha256ValueV1> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| invalid_source("checkout_root_unreadable"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid_source("checkout_root_shape_invalid"));
    }
    let mut bytes = b"blackbox.project-catalog.checkout-root-runtime.v1\0".to_vec();
    bytes.extend_from_slice(path_digest.as_str().as_bytes());
    #[cfg(unix)]
    {
        bytes.extend_from_slice(&metadata.dev().to_be_bytes());
        bytes.extend_from_slice(&metadata.ino().to_be_bytes());
        bytes.extend_from_slice(&metadata.mode().to_be_bytes());
    }
    Ok(Sha256ValueV1::digest(&bytes))
}

fn marker_state(source: &AuthorizedFileObservationV1) -> CheckoutMarkerStateV1 {
    match source {
        AuthorizedFileObservationV1::NotFound => CheckoutMarkerStateV1::MissingOrEmpty,
        AuthorizedFileObservationV1::Invalid { diagnostic_code }
            if diagnostic_code == "source_path_symlinked" =>
        {
            CheckoutMarkerStateV1::Symlinked
        }
        AuthorizedFileObservationV1::Invalid { diagnostic_code }
            if diagnostic_code == "source_byte_limit_exceeded"
                || diagnostic_code == "source_path_non_regular" =>
        {
            CheckoutMarkerStateV1::Malformed {
                diagnostic_code: "checkout_marker_shape_invalid".to_string(),
            }
        }
        AuthorizedFileObservationV1::Invalid { .. } => CheckoutMarkerStateV1::Unreadable {
            diagnostic_code: "checkout_marker_unreadable".to_string(),
        },
        AuthorizedFileObservationV1::Present(source) => {
            let Ok(value) = std::str::from_utf8(&source.bytes) else {
                return CheckoutMarkerStateV1::Malformed {
                    diagnostic_code: "checkout_marker_not_utf8".to_string(),
                };
            };
            let value = value.trim();
            if value.is_empty() {
                CheckoutMarkerStateV1::MissingOrEmpty
            } else if value.len() == 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                CheckoutMarkerStateV1::Valid {
                    checkout_id: value.to_string(),
                }
            } else {
                CheckoutMarkerStateV1::Malformed {
                    diagnostic_code: "checkout_marker_id_invalid".to_string(),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImmutableLaneCaptureV1<T> {
    evidence: ImmutableInventoryLaneEvidenceV1,
    rows: Vec<T>,
}

#[derive(Clone, PartialEq, Eq)]
struct ImmutableInventoryLanesV1 {
    project_scoped_refs: ImmutableLaneCaptureV1<ProjectScopedRefObservationV1>,
    edge_workspaces: ImmutableLaneCaptureV1<EdgeWorkspaceObservationV1>,
    git_metadata: ImmutableLaneCaptureV1<GitMetadataObservationV1>,
    checkouts: ImmutableLaneCaptureV1<CheckoutObservationV1>,
    attachment_candidates: ImmutableLaneCaptureV1<AttachmentCandidateObservationV1>,
    inventory_targets: ImmutableLaneCaptureV1<InventoryTargetObservationV1>,
    materialized_aliases: ImmutableLaneCaptureV1<MaterializedAliasObservationV1>,
    legacy_path_observations: ImmutableLaneCaptureV1<LegacyPathObservationV1>,
    repo_grouping_proofs: ImmutableLaneCaptureV1<RepoGroupingProofV1>,
    legacy_namespace_clusters: ImmutableLaneCaptureV1<LegacyNamespaceClusterObservationV1>,
}

#[derive(Clone)]
struct RequiredOwnerLaneCaptureV1 {
    lanes: ImmutableInventoryLanesV1,
    legacy_commit_namespaces: Vec<LegacyCommitNamespaceInventoryV1>,
    git_common_directories: BTreeMap<String, AuthorizedInventoryPath>,
    legacy_selectors: BTreeMap<String, RuntimeLiteralBindingV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AttachmentCandidateKeyV1 {
    pub(crate) project_id: ProjectId,
    pub(crate) checkout_observation_id: String,
    pub(crate) base_relpath: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AttachmentCandidateIdentityPlanV1 {
    pub(crate) identities: BTreeMap<AttachmentCandidateKeyV1, AttachmentId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectCatalogOwnerInventoryPathsV1 {
    pub(crate) corpus_index_root: PathBuf,
    pub(crate) git_cursor_root: PathBuf,
    pub(crate) vector_root: PathBuf,
    pub(crate) edge_root: PathBuf,
    pub(crate) knowledge_store_path: PathBuf,
    pub(crate) gap_store_path: PathBuf,
    pub(crate) thread_store_path: PathBuf,
    pub(crate) note_store_path: PathBuf,
    pub(crate) pin_store_path: PathBuf,
    pub(crate) roadmap_store_path: PathBuf,
    pub(crate) packet_root: PathBuf,
    pub(crate) task_store_path: PathBuf,
    pub(crate) proposal_root: PathBuf,
    pub(crate) slack_store_root: PathBuf,
    pub(crate) whiteboard_root: PathBuf,
    pub(crate) artifact_root: PathBuf,
    pub(crate) provenance_notes_ref: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectCatalogOwnerInventoryLimitsV1 {
    pub(crate) corpus: CorpusMigrationSnapshotLimitsV1,
    pub(crate) vectors: VectorMigrationSnapshotLimitsV1,
    pub(crate) edges: EdgeMigrationSnapshotLimitsV1,
    pub(crate) durable_owners: OwnerSnapshotLimitsV1,
}

impl Default for ProjectCatalogOwnerInventoryLimitsV1 {
    fn default() -> Self {
        Self {
            corpus: CorpusMigrationSnapshotLimitsV1::default(),
            vectors: VectorMigrationSnapshotLimitsV1::default(),
            edges: EdgeMigrationSnapshotLimitsV1::default(),
            durable_owners: OwnerSnapshotLimitsV1::default(),
        }
    }
}

pub(crate) struct ProjectCatalogMigrationInventoryRequestV1<'a> {
    pub(crate) rehearsal_root: Option<PathBuf>,
    pub(crate) legacy_project_store_path: PathBuf,
    pub(crate) publisher_ref_store: &'a PublisherRefStore,
    pub(crate) code_source_store_root: PathBuf,
    pub(crate) code_source_store_limits: StoreLimits,
    pub(crate) checkout_roots: Vec<PathBuf>,
    pub(crate) owner_paths: ProjectCatalogOwnerInventoryPathsV1,
    pub(crate) owner_limits: ProjectCatalogOwnerInventoryLimitsV1,
    pub(crate) attachment_identity_plan: &'a AttachmentCandidateIdentityPlanV1,
}

pub(crate) struct ProjectCatalogAttachmentCandidateDiscoveryRequestV1 {
    pub(crate) rehearsal_root: Option<PathBuf>,
    pub(crate) legacy_project_store_path: PathBuf,
    pub(crate) checkout_roots: Vec<PathBuf>,
}

#[derive(Clone)]
pub(crate) struct InventoryRuntimeBindingsV1 {
    legacy_project_store_source: ExactSourceBytesV1,
    legacy_project_store_was_missing: bool,
    legacy_project_paths: BTreeMap<String, AuthorizedInventoryPath>,
    checkout_paths: BTreeMap<String, AuthorizedInventoryPath>,
    checkout_repositories: BTreeMap<String, StableGitRepository>,
    git_common_directories: BTreeMap<String, AuthorizedInventoryPath>,
    legacy_selectors: BTreeMap<String, RuntimeLiteralBindingV1>,
}

#[derive(Clone)]
struct RuntimeLiteralBindingV1 {
    digest: Sha256ValueV1,
    literal: String,
}

impl fmt::Debug for InventoryRuntimeBindingsV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InventoryRuntimeBindingsV1")
            .field(
                "legacy_project_store_byte_len",
                &self.legacy_project_store_source.bytes.len(),
            )
            .field(
                "legacy_project_path_count",
                &self.legacy_project_paths.len(),
            )
            .field("checkout_path_count", &self.checkout_paths.len())
            .field(
                "checkout_repository_count",
                &self.checkout_repositories.len(),
            )
            .field(
                "git_common_directory_count",
                &self.git_common_directories.len(),
            )
            .field("legacy_selector_count", &self.legacy_selectors.len())
            .finish()
    }
}

impl InventoryRuntimeBindingsV1 {
    pub(crate) fn legacy_project_store_bytes(&self) -> &[u8] {
        &self.legacy_project_store_source.bytes
    }

    pub(crate) fn legacy_project_store_was_missing(&self) -> bool {
        self.legacy_project_store_was_missing
    }

    pub(crate) fn legacy_project_paths(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.legacy_project_paths
            .iter()
            .map(|(id, path)| (id.as_str(), path.as_path()))
    }

    pub(crate) fn checkout_paths(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.checkout_paths
            .iter()
            .map(|(id, path)| (id.as_str(), path.as_path()))
    }

    pub(crate) fn checkout_repositories(
        &self,
    ) -> impl Iterator<Item = (&str, &StableGitRepository)> {
        self.checkout_repositories
            .iter()
            .map(|(id, repository)| (id.as_str(), repository))
    }

    pub(crate) fn legacy_selectors(&self) -> impl Iterator<Item = (&str, &str)> {
        self.legacy_selectors
            .iter()
            .map(|(id, binding)| (id.as_str(), binding.literal.as_str()))
    }

    fn validate_pairing(&self, inventory: &V1ProjectCatalogInventory) -> AdapterResult<()> {
        if self.legacy_project_store_source.content_hash != inventory.source_store_hash {
            return Err(invalid_source(
                "runtime legacy project source hash mismatch",
            ));
        }
        let project_digests = inventory
            .legacy_projects
            .iter()
            .map(|row| {
                (
                    row.observation_id.as_str(),
                    &row.record.canonical_path_digest,
                )
            })
            .collect::<BTreeMap<_, _>>();
        validate_authorized_path_bindings(&self.legacy_project_paths, &project_digests)?;

        let checkout_digests = inventory
            .checkouts
            .iter()
            .map(|row| (row.observation_id.as_str(), &row.canonical_root_digest))
            .collect::<BTreeMap<_, _>>();
        validate_authorized_path_bindings(&self.checkout_paths, &checkout_digests)?;
        if self
            .checkout_repositories
            .keys()
            .any(|observation_id| !self.checkout_paths.contains_key(observation_id))
        {
            return Err(invalid_source(
                "runtime checkout repository lacks its path binding",
            ));
        }

        let git_digests = inventory
            .git_metadata
            .iter()
            .filter_map(|row| {
                row.common_directory_digest
                    .as_ref()
                    .map(|digest| (row.observation_id.as_str(), digest))
            })
            .collect::<BTreeMap<_, _>>();
        validate_authorized_path_bindings(&self.git_common_directories, &git_digests)?;

        let selector_digests = inventory
            .legacy_path_observations
            .iter()
            .map(|row| (row.observation_id.as_str(), &row.selector_digest))
            .collect::<BTreeMap<_, _>>();
        if self.legacy_selectors.len() != selector_digests.len() {
            return Err(invalid_source(
                "runtime selector binding coverage is incomplete",
            ));
        }
        for (observation_id, expected) in selector_digests {
            let binding = self
                .legacy_selectors
                .get(observation_id)
                .ok_or_else(|| invalid_source("runtime selector binding is missing"))?;
            if &binding.digest != expected || digest_path(&binding.literal) != *expected {
                return Err(invalid_source("runtime selector binding digest mismatch"));
            }
        }
        Ok(())
    }
}

fn validate_authorized_path_bindings(
    bindings: &BTreeMap<String, AuthorizedInventoryPath>,
    expected: &BTreeMap<&str, &Sha256ValueV1>,
) -> AdapterResult<()> {
    if bindings.len() != expected.len() {
        return Err(invalid_source(
            "runtime path binding coverage is incomplete",
        ));
    }
    for (observation_id, digest) in expected {
        let binding = bindings
            .get(*observation_id)
            .ok_or_else(|| invalid_source("runtime path binding is missing"))?;
        binding.ensure_authority()?;
        let literal = binding
            .as_path()
            .to_str()
            .ok_or_else(|| invalid_source("runtime path binding is not utf8"))?;
        if digest_path(literal) != **digest {
            return Err(invalid_source("runtime path binding digest mismatch"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectCatalogMigrationInventoryResultV1 {
    pub(crate) inventory: V1ProjectCatalogInventory,
    /// Host-local authorities are retained outside canonical inventory JSON.
    runtime_bindings: InventoryRuntimeBindingsV1,
    pub(crate) code_source_owner_inventory: MigrationLegacyInventoryV1,
    pub(crate) publisher_ref_source_was_missing: bool,
    pub(crate) code_source_canonical_sha256: Sha256ValueV1,
}

impl ProjectCatalogMigrationInventoryResultV1 {
    pub(crate) fn runtime_bindings(&self) -> &InventoryRuntimeBindingsV1 {
        &self.runtime_bindings
    }
}

pub(crate) struct ProjectCatalogMigrationInventoryFacadeV1;

impl ProjectCatalogMigrationInventoryFacadeV1 {
    pub(crate) fn discover_checkout_observation_bindings(
        checkout_roots: Vec<PathBuf>,
    ) -> Result<BTreeMap<String, PathBuf>, InventoryAdapterError> {
        let checkout_roots = authorize_checkout_roots(&checkout_roots)?;
        capture_checkout_roots(&checkout_roots, None)?
            .into_iter()
            .map(|capture| {
                Ok((
                    capture.observation.observation_id,
                    capture.runtime_root.as_path().to_path_buf(),
                ))
            })
            .collect()
    }

    pub(crate) fn discover_attachment_candidate_keys(
        request: ProjectCatalogAttachmentCandidateDiscoveryRequestV1,
    ) -> Result<Vec<AttachmentCandidateKeyV1>, InventoryAdapterError> {
        let legacy_project_store_path =
            AuthorizedInventoryPath::new(&request.legacy_project_store_path)?;
        let checkout_roots = authorize_checkout_roots(&request.checkout_roots)?;
        let projects_path = legacy_project_store_path.as_path().to_path_buf();
        crate::project_catalog_store::capture_migration_preflight_with(
            &projects_path,
            |error| invalid_source(error.to_string()),
            || {
                let observed = capture_legacy_projects_source(&legacy_project_store_path)?;
                if matches!(observed, DecodedSourceObservationV1::Invalid { .. }) {
                    return Ok(Vec::new());
                }
                let legacy_source = accept_missing_legacy_projects_source(observed)?;
                let probes = derive_legacy_project_probes(
                    &legacy_source,
                    request.rehearsal_root.as_deref(),
                )?;
                validate_probe_containment(&probes, request.rehearsal_root.as_deref())?;
                validate_authorized_containment(
                    &checkout_roots,
                    request.rehearsal_root.as_deref(),
                )?;
                let legacy = observe_legacy_projects(&legacy_source, probes)?;
                let checkout_captures =
                    capture_checkout_roots(&checkout_roots, request.rehearsal_root.as_deref())?;
                discover_attachment_candidate_keys_locked(&legacy, &checkout_captures)
            },
        )
    }

    pub(crate) fn capture(
        request: ProjectCatalogMigrationInventoryRequestV1<'_>,
    ) -> Result<ProjectCatalogMigrationInventoryResultV1, InventoryAdapterError> {
        let legacy_project_store_path =
            AuthorizedInventoryPath::new(&request.legacy_project_store_path)?;
        let checkout_roots = authorize_checkout_roots(&request.checkout_roots)?;
        validate_authorized_containment(&checkout_roots, request.rehearsal_root.as_deref())?;
        let owner_paths = authorize_owner_paths(request.owner_paths)?;
        let code_source_store = CodeSourceStore::open_existing_for_migration(
            &request.code_source_store_root,
            request.code_source_store_limits,
        )
        .map_err(|_| invalid_source("code_source_store_existing_open_failed"))?
        .ok_or_else(|| invalid_source("code_source_store_missing"))?;
        let projects_path = legacy_project_store_path.as_path().to_path_buf();
        let authorized = AuthorizedProjectCatalogMigrationInventoryRequestV1 {
            legacy_project_store_path,
            publisher_ref_store: request.publisher_ref_store,
            code_source_store,
            checkout_roots,
            owner_paths,
            owner_limits: request.owner_limits,
            attachment_identity_plan: request.attachment_identity_plan,
            rehearsal_root: request.rehearsal_root,
        };
        crate::project_catalog_store::capture_migration_preflight_with(
            &projects_path,
            |error| invalid_source(error.to_string()),
            || capture_inventory_locked(authorized),
        )
    }
}

struct AuthorizedProjectCatalogMigrationInventoryRequestV1<'a> {
    legacy_project_store_path: AuthorizedInventoryPath,
    publisher_ref_store: &'a PublisherRefStore,
    code_source_store: CodeSourceStore,
    checkout_roots: Vec<AuthorizedInventoryPath>,
    owner_paths: AuthorizedProjectCatalogOwnerInventoryPathsV1,
    owner_limits: ProjectCatalogOwnerInventoryLimitsV1,
    attachment_identity_plan: &'a AttachmentCandidateIdentityPlanV1,
    rehearsal_root: Option<PathBuf>,
}

fn validate_authorized_containment(
    paths: &[AuthorizedInventoryPath],
    rehearsal_root: Option<&Path>,
) -> AdapterResult<()> {
    let Some(rehearsal_root) = rehearsal_root else {
        return Ok(());
    };
    let canonical_root = rehearsal_root
        .canonicalize()
        .map_err(|_| invalid_input("rehearsal root is not canonicalizable"))?;
    for path in paths {
        path.ensure_authority()?;
        if !path.as_path().starts_with(&canonical_root) {
            return Err(invalid_input("runtime authority escapes rehearsal root"));
        }
        path.ensure_authority()?;
    }
    Ok(())
}

fn validate_stable_repository_containment(
    repository: &StableGitRepository,
    rehearsal_root: Option<&Path>,
) -> AdapterResult<()> {
    let Some(rehearsal_root) = rehearsal_root else {
        return Ok(());
    };
    let canonical_root = rehearsal_root
        .canonicalize()
        .map_err(|_| invalid_input("rehearsal root is not canonicalizable"))?;
    if repository
        .authority_paths()
        .iter()
        .any(|path| !path.starts_with(&canonical_root))
    {
        return Err(invalid_input("stable Git authority escapes rehearsal root"));
    }
    Ok(())
}

fn validate_probe_containment(
    probes: &[LegacyProjectProbeInputV1],
    rehearsal_root: Option<&Path>,
) -> AdapterResult<()> {
    let paths = probes
        .iter()
        .map(|probe| probe.authorized_canonical_path.clone())
        .collect::<Vec<_>>();
    validate_authorized_containment(&paths, rehearsal_root)
}

#[derive(Clone)]
struct AuthorizedProjectCatalogOwnerInventoryPathsV1 {
    corpus_index_root: AuthorizedInventoryPath,
    git_cursor_root: AuthorizedInventoryPath,
    vector_root: AuthorizedInventoryPath,
    edge_root: AuthorizedInventoryPath,
    knowledge_store_path: AuthorizedInventoryPath,
    gap_store_path: AuthorizedInventoryPath,
    thread_store_path: AuthorizedInventoryPath,
    note_store_path: AuthorizedInventoryPath,
    pin_store_path: AuthorizedInventoryPath,
    roadmap_store_path: AuthorizedInventoryPath,
    packet_root: AuthorizedInventoryPath,
    task_store_path: AuthorizedInventoryPath,
    proposal_root: AuthorizedInventoryPath,
    slack_store_root: AuthorizedInventoryPath,
    whiteboard_root: AuthorizedInventoryPath,
    artifact_root: AuthorizedInventoryPath,
    provenance_notes_ref: String,
}

fn authorize_checkout_roots(paths: &[PathBuf]) -> AdapterResult<Vec<AuthorizedInventoryPath>> {
    let roots = paths
        .iter()
        .map(AuthorizedInventoryPath::new)
        .collect::<AdapterResult<Vec<_>>>()?;
    let mut canonical_paths = BTreeSet::new();
    let mut identities = BTreeSet::new();
    for root in &roots {
        let (canonical_path, identity) = root.checkout_identity_key()?;
        if !canonical_paths.insert(canonical_path) || !identities.insert(identity) {
            return Err(invalid_input("checkout roots contain a canonical alias"));
        }
    }
    Ok(roots)
}

fn authorize_owner_paths(
    paths: ProjectCatalogOwnerInventoryPathsV1,
) -> AdapterResult<AuthorizedProjectCatalogOwnerInventoryPathsV1> {
    if paths.provenance_notes_ref.is_empty()
        || paths.provenance_notes_ref.len() > MAX_AUTHORIZED_PATH_BYTES
        || paths
            .provenance_notes_ref
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        || bbox_provenance::validate_notes_ref(&paths.provenance_notes_ref).is_err()
    {
        return Err(invalid_input("provenance notes ref is invalid"));
    }
    Ok(AuthorizedProjectCatalogOwnerInventoryPathsV1 {
        corpus_index_root: AuthorizedInventoryPath::new(paths.corpus_index_root)?,
        git_cursor_root: AuthorizedInventoryPath::new(paths.git_cursor_root)?,
        vector_root: AuthorizedInventoryPath::new(paths.vector_root)?,
        edge_root: AuthorizedInventoryPath::new(paths.edge_root)?,
        knowledge_store_path: AuthorizedInventoryPath::new(paths.knowledge_store_path)?,
        gap_store_path: AuthorizedInventoryPath::new(paths.gap_store_path)?,
        thread_store_path: AuthorizedInventoryPath::new(paths.thread_store_path)?,
        note_store_path: AuthorizedInventoryPath::new(paths.note_store_path)?,
        pin_store_path: AuthorizedInventoryPath::new(paths.pin_store_path)?,
        roadmap_store_path: AuthorizedInventoryPath::new(paths.roadmap_store_path)?,
        packet_root: AuthorizedInventoryPath::new(paths.packet_root)?,
        task_store_path: AuthorizedInventoryPath::new(paths.task_store_path)?,
        proposal_root: AuthorizedInventoryPath::new(paths.proposal_root)?,
        slack_store_root: AuthorizedInventoryPath::new(paths.slack_store_root)?,
        whiteboard_root: AuthorizedInventoryPath::new(paths.whiteboard_root)?,
        artifact_root: AuthorizedInventoryPath::new(paths.artifact_root)?,
        provenance_notes_ref: paths.provenance_notes_ref,
    })
}

fn capture_inventory_locked(
    request: AuthorizedProjectCatalogMigrationInventoryRequestV1<'_>,
) -> AdapterResult<ProjectCatalogMigrationInventoryResultV1> {
    let captured_legacy = accept_legacy_projects_source_for_inventory(
        capture_legacy_projects_source(&request.legacy_project_store_path)?,
    );
    let legacy_source = &captured_legacy.exact;
    let probes = derive_legacy_project_probes(&legacy_source, request.rehearsal_root.as_deref())?;
    validate_probe_containment(&probes, request.rehearsal_root.as_deref())?;
    let mut legacy = observe_legacy_projects(&legacy_source, probes)?;
    let catalog_scopes = legacy
        .published_scopes
        .values()
        .cloned()
        .collect::<BTreeSet<_>>();
    let publisher_locked = capture_publisher_ref_source(request.publisher_ref_store)?;
    let code_snapshot = capture_code_source_inventory(&request.code_source_store, &catalog_scopes)?;
    let missing_checkout_projects = legacy
        .observations
        .iter()
        .filter(|row| row.path_status == LegacyProjectPathStatusV1::Missing)
        .map(|row| {
            ProjectId::parse(row.record.project_id.clone())
                .expect("validated legacy project id remains valid")
        })
        .collect::<BTreeSet<_>>();
    let mut code_capture = observe_code_sources(
        &code_snapshot,
        &legacy.published_scopes,
        &missing_checkout_projects,
    )?;
    let mut code_sources = code_capture.sources;
    let publisher_source = &publisher_locked.source;
    let checkout_captures =
        capture_checkout_roots(&request.checkout_roots, request.rehearsal_root.as_deref())?;
    let legacy_row_ids = legacy
        .observations
        .iter()
        .map(|row| row.observation_id.clone())
        .collect();
    let mut mutable_source_evidence = vec![source_evidence(
        "legacy-project-store",
        MutableInventorySourceKindV1::LegacyProjectStore,
        MutableInventorySourceLocatorV1::LegacyProjectStore,
        captured_legacy.state,
        legacy_row_ids,
    )];
    mutable_source_evidence.append(&mut legacy.source_evidence);
    let effective_row_ids = code_sources
        .iter()
        .flat_map(|capture| {
            std::iter::once(capture.observation.observation_id.clone())
                .chain(
                    capture
                        .observation
                        .generations
                        .iter()
                        .map(|row| row.observation_id.clone()),
                )
                .chain(
                    capture
                        .observation
                        .quarantine
                        .iter()
                        .map(|row| row.observation_id.clone()),
                )
        })
        .chain(
            code_capture
                .retained_owner_resolutions
                .iter()
                .map(|row| row.observation_id.clone()),
        )
        .collect();
    mutable_source_evidence.push(source_evidence(
        "effective-source-manifest",
        MutableInventorySourceKindV1::EffectiveSourceManifest,
        MutableInventorySourceLocatorV1::CodeSourceAnchor,
        if code_snapshot.anchor_missing {
            InventorySourceStateV1::Missing {
                fingerprint: missing_source_fingerprint("effective-source-manifest"),
            }
        } else {
            present_source_state(&code_snapshot.anchor_source)
        },
        effective_row_ids,
    ));
    for capture in &mut code_sources {
        mutable_source_evidence.append(&mut capture.source_evidence);
    }
    mutable_source_evidence.append(&mut code_capture.retained_owner_source_evidence);
    let mut checkout_path_bindings = BTreeMap::new();
    let mut checkout_repository_bindings = BTreeMap::new();
    for capture in &checkout_captures {
        checkout_path_bindings.insert(
            capture.observation.observation_id.clone(),
            capture.runtime_root.clone(),
        );
        if let Some(repository) = &capture.repository {
            checkout_repository_bindings.insert(
                capture.observation.observation_id.clone(),
                repository.clone(),
            );
        }
        mutable_source_evidence.push(capture.root_source_evidence.clone());
        mutable_source_evidence.push(capture.marker_source_evidence.clone());
    }
    let owner_capture = capture_required_owner_lanes(
        &request.owner_paths,
        request.owner_limits,
        &legacy,
        &code_sources,
        publisher_source,
        &checkout_captures,
        request.attachment_identity_plan,
    )?;
    let mut lanes = owner_capture.lanes;
    sort_lane_rows(&mut lanes);
    validate_lane_kinds(&lanes)?;
    let publisher_pins = derive_publisher_pins(
        publisher_source,
        &legacy,
        &code_capture.project_authority_scopes,
        &lanes,
    )?;
    let publisher_row_ids = publisher_pins
        .bound
        .iter()
        .map(|row| row.observation_id.clone())
        .chain(
            publisher_pins
                .unbound
                .iter()
                .map(|row| row.observation_id.clone()),
        )
        .collect();
    mutable_source_evidence.push(exact_source_evidence(
        "publisher-ref-store",
        MutableInventorySourceKindV1::PublisherRefStore,
        MutableInventorySourceLocatorV1::PublisherRefStore,
        publisher_source,
        publisher_row_ids,
    ));
    mutable_source_evidence.sort_by(|left, right| left.source_id.cmp(&right.source_id));
    let runtime_bindings = InventoryRuntimeBindingsV1 {
        legacy_project_store_source: legacy_source.source.clone(),
        legacy_project_store_was_missing: legacy_source.was_missing,
        legacy_project_paths: legacy.runtime_project_paths,
        checkout_paths: checkout_path_bindings,
        checkout_repositories: checkout_repository_bindings,
        git_common_directories: owner_capture.git_common_directories,
        legacy_selectors: owner_capture.legacy_selectors,
    };
    let code_source_owner_inventory = code_snapshot.owner.inventory.clone();
    let code_source_canonical_sha256 =
        Sha256ValueV1::parse(code_snapshot.owner.inventory.canonical_sha256.clone())
            .map_err(|_| invalid_source("code_source_canonical_hash_invalid"))?;
    let code_source_generation_set_sha256 =
        Sha256ValueV1::parse(code_snapshot.owner.inventory.generation_set_sha256.clone())
            .map_err(|_| invalid_source("code_source_generation_set_hash_invalid"))?;
    let inventory = V1ProjectCatalogInventory {
        version: PROJECT_CATALOG_INVENTORY_VERSION_V1,
        source_store_hash: legacy_source.source.content_hash.clone(),
        publisher_ref_source_hash: publisher_source.source.content_hash.clone(),
        publisher_ref_source_bytes: publisher_source.source.bytes.clone(),
        code_source_inventory_hash: code_source_canonical_sha256.clone(),
        code_source_generation_count: code_snapshot.owner.inventory.generation_count,
        code_source_generation_set_sha256: code_source_generation_set_sha256.clone(),
        mutable_source_evidence,
        immutable_lane_evidence: lane_evidence(&lanes),
        legacy_projects: legacy.observations,
        code_sources: code_sources
            .into_iter()
            .map(|capture| capture.observation)
            .collect(),
        retained_owner_resolutions: code_capture.retained_owner_resolutions,
        publisher_pins: publisher_pins.bound,
        unbound_publisher_pins: publisher_pins.unbound,
        project_scoped_refs: lanes.project_scoped_refs.rows,
        edge_workspaces: lanes.edge_workspaces.rows,
        git_metadata: lanes.git_metadata.rows,
        checkouts: lanes.checkouts.rows,
        attachment_candidates: lanes.attachment_candidates.rows,
        inventory_targets: lanes.inventory_targets.rows,
        materialized_aliases: lanes.materialized_aliases.rows,
        legacy_path_observations: lanes.legacy_path_observations.rows,
        repo_grouping_proofs: lanes.repo_grouping_proofs.rows,
        legacy_namespace_clusters: lanes.legacy_namespace_clusters.rows,
        legacy_commit_namespaces: owner_capture.legacy_commit_namespaces,
    };
    inventory
        .validate()
        .map_err(|error| invalid_source(error.to_string()))?;
    runtime_bindings.validate_pairing(&inventory)?;
    Ok(ProjectCatalogMigrationInventoryResultV1 {
        code_source_canonical_sha256,
        code_source_owner_inventory,
        publisher_ref_source_was_missing: publisher_source.was_missing,
        inventory,
        runtime_bindings,
    })
}

#[derive(Clone)]
struct DurableOwnerSnapshotsV1 {
    knowledge: Vec<OwnerSnapshotV1>,
    gap: Vec<OwnerSnapshotV1>,
    thread: Vec<OwnerSnapshotV1>,
    note: Vec<OwnerSnapshotV1>,
    pin: Vec<OwnerSnapshotV1>,
    roadmap: Vec<OwnerSnapshotV1>,
    packet: Vec<OwnerSnapshotV1>,
    task: Vec<OwnerSnapshotV1>,
    proposal: Vec<OwnerSnapshotV1>,
    slack_binding: Vec<OwnerSnapshotV1>,
    whiteboard: Vec<OwnerSnapshotV1>,
    artifact: Vec<OwnerSnapshotV1>,
    provenance: Vec<OwnerSnapshotV1>,
    transcript_edge: Vec<OwnerSnapshotV1>,
}

fn capture_required_owner_lanes(
    paths: &AuthorizedProjectCatalogOwnerInventoryPathsV1,
    limits: ProjectCatalogOwnerInventoryLimitsV1,
    legacy: &LegacyProjectsCaptureV1,
    code_sources: &[CodeSourceCaptureV1],
    publisher_source: &ExactDecodedSourceV1<PublisherRefInventoryV1>,
    checkout_captures: &[CheckoutCaptureV1],
    attachment_identity_plan: &AttachmentCandidateIdentityPlanV1,
) -> AdapterResult<RequiredOwnerLaneCaptureV1> {
    let expected_provenance_projects = legacy
        .observations
        .iter()
        .filter(|project| {
            project.record.is_git_repo && project.path_status == LegacyProjectPathStatusV1::Present
        })
        .map(|project| {
            ProjectId::parse(project.record.project_id.clone())
                .map_err(|_| invalid_source("legacy_project_id_invalid"))
        })
        .collect::<AdapterResult<BTreeSet<_>>>()?;
    let supplied_provenance_projects = legacy.repositories.keys().cloned().collect::<BTreeSet<_>>();
    if expected_provenance_projects != supplied_provenance_projects {
        return Err(invalid_input(
            "provenance owner sources do not exactly cover present Git projects",
        ));
    }
    let corpus = capture_owner_path(&paths.corpus_index_root, |path| {
        capture_owner_migration_snapshot_no_create(
            path,
            paths.git_cursor_root.as_path(),
            limits.corpus,
        )
    })?;
    let vectors = capture_owner_path(&paths.vector_root, |path| {
        capture_vector_migration_snapshot_no_create(path, limits.vectors)
    })?;
    let edges = capture_owner_path(&paths.edge_root, |path| {
        capture_edge_migration_snapshot_no_create(path, limits.edges)
    })?;
    ensure_owner_inventory_available(&corpus, &vectors, &edges)?;
    paths.git_cursor_root.ensure_authority()?;

    let durable = capture_durable_owner_snapshots(paths, limits.durable_owners, legacy)?;
    let project_scoped_refs = capture_project_scoped_refs_lane(&corpus, &vectors)?;
    let edge_workspaces = capture_edge_workspaces_lane(&edges)?;
    let checkouts = capture_checkouts_lane(checkout_captures)?;
    let attachment_candidates =
        capture_attachment_candidates_lane(legacy, checkout_captures, attachment_identity_plan)?;
    let materialized_aliases = capture_materialized_aliases_lane(legacy)?;
    let inventory_targets = capture_inventory_targets_lane(&durable.artifact, &durable.provenance)?;
    let (legacy_path_observations, legacy_selectors) =
        capture_legacy_path_observations_lane(&durable)?;
    let (git_metadata, legacy_commit_namespaces, git_common_directories) =
        capture_git_metadata_lane(
            &corpus,
            &vectors,
            legacy,
            publisher_source,
            checkout_captures,
            &attachment_candidates.rows,
        )?;
    let repo_grouping_proofs =
        capture_repo_grouping_proofs_lane(legacy, code_sources, &git_metadata)?;
    let legacy_namespace_clusters = capture_legacy_namespace_clusters_lane(legacy, &git_metadata)?;

    Ok(RequiredOwnerLaneCaptureV1 {
        lanes: ImmutableInventoryLanesV1 {
            project_scoped_refs,
            edge_workspaces,
            git_metadata,
            checkouts,
            attachment_candidates,
            inventory_targets,
            materialized_aliases,
            legacy_path_observations,
            repo_grouping_proofs,
            legacy_namespace_clusters,
        },
        legacy_commit_namespaces,
        git_common_directories,
        legacy_selectors,
    })
}

fn ensure_owner_inventory_available(
    corpus: &CorpusOwnerMigrationSnapshotV1,
    vectors: &VectorMigrationSnapshotV1,
    edges: &EdgeMigrationSnapshotV1,
) -> AdapterResult<()> {
    for state in [
        &corpus.index.state,
        &corpus.code_metadata.state,
        &corpus.git_cursors.state,
    ] {
        if let CorpusMigrationSourceStateV1::Unavailable { diagnostic_code } = state {
            return Err(invalid_source(format!(
                "corpus_owner_inventory_unavailable:{diagnostic_code}"
            )));
        }
    }
    if let VectorMigrationSourceStateV1::Unavailable { diagnostic_code } = &vectors.state {
        return Err(invalid_source(format!(
            "vector_owner_inventory_unavailable:{diagnostic_code}"
        )));
    }
    if let EdgeMigrationSourceStateV1::Unavailable { diagnostic_code } = &edges.state {
        return Err(invalid_source(format!(
            "edge_owner_inventory_unavailable:{diagnostic_code}"
        )));
    }
    Ok(())
}

fn capture_owner_path<T>(
    path: &AuthorizedInventoryPath,
    capture: impl FnOnce(&Path) -> T,
) -> AdapterResult<T> {
    path.ensure_authority()?;
    let value = capture(path.as_path());
    path.ensure_authority()?;
    Ok(value)
}

fn capture_owner_snapshot_path(
    path: &AuthorizedInventoryPath,
    capture: impl FnOnce(
        &Path,
    ) -> Result<
        OwnerSnapshotV1,
        bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
    >,
) -> AdapterResult<OwnerSnapshotV1> {
    capture_owner_path(path, capture)?.map_err(|error| {
        invalid_input(format!("owner snapshot limits are invalid: {}", error.code))
    })
}

fn capture_durable_owner_snapshots(
    paths: &AuthorizedProjectCatalogOwnerInventoryPathsV1,
    limits: OwnerSnapshotLimitsV1,
    legacy: &LegacyProjectsCaptureV1,
) -> AdapterResult<DurableOwnerSnapshotsV1> {
    let knowledge = capture_owner_snapshot_path(&paths.knowledge_store_path, |path| {
        bbox_knowledge::knowledge::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let gap = capture_owner_snapshot_path(&paths.gap_store_path, |path| {
        bbox_gaps::gaps::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let thread = capture_owner_snapshot_path(&paths.thread_store_path, |path| {
        bbox_threads::threads::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let note = capture_owner_snapshot_path(&paths.note_store_path, |path| {
        bbox_threads::notes::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let pin = capture_owner_snapshot_path(&paths.pin_store_path, |path| {
        bbox_stores::pins::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let roadmap = capture_owner_snapshot_path(&paths.roadmap_store_path, |path| {
        bbox_stores::roadmap::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let packet = capture_owner_snapshot_path(&paths.packet_root, |path| {
        bbox_packets::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let task = capture_owner_snapshot_path(&paths.task_store_path, |path| {
        capture_legacy_task_owner_snapshot(path, limits)
    })?;
    let proposal = capture_owner_snapshot_path(&paths.proposal_root, |path| {
        capture_legacy_proposal_owner_snapshot(path, limits)
    })?;
    let slack_channels = capture_owner_snapshot_path(&paths.slack_store_root, |path| {
        bbox_slack::slack_channel_bindings::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let slack_proposals = capture_owner_snapshot_path(&paths.slack_store_root, |path| {
        bbox_slack::slack_proposal_links::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let whiteboard = capture_owner_snapshot_path(&paths.whiteboard_root, |path| {
        bbox_whiteboards::whiteboards::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let artifact = capture_owner_snapshot_path(&paths.artifact_root, |path| {
        bbox_artifacts::artifacts::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let transcript_edge = capture_owner_snapshot_path(&paths.edge_root, |path| {
        bbox_edge_sidecar::edge_sidecar::capture_project_catalog_owner_snapshot(path, limits)
    })?;
    let mut provenance = Vec::new();
    for (project_id, repository) in &legacy.repositories {
        provenance.push(
            bbox_provenance::capture_project_catalog_owner_snapshot_stable(
                repository,
                &paths.provenance_notes_ref,
                project_id.as_str(),
                limits,
            )
            .map_err(|error| {
                invalid_input(format!("owner snapshot limits are invalid: {}", error.code))
            })?,
        );
    }
    Ok(DurableOwnerSnapshotsV1 {
        knowledge: vec![knowledge],
        gap: vec![gap],
        thread: vec![thread],
        note: vec![note],
        pin: vec![pin],
        roadmap: vec![roadmap],
        packet: vec![packet],
        task: vec![task],
        proposal: vec![proposal],
        slack_binding: vec![slack_channels, slack_proposals],
        whiteboard: vec![whiteboard],
        artifact: vec![artifact],
        provenance,
        transcript_edge: vec![transcript_edge],
    })
}

fn snapshot_owner_state(
    source_id: &str,
    snapshots: &[OwnerSnapshotV1],
) -> AdapterResult<InventorySourceStateV1> {
    let mut digest = Sha256::new();
    digest.update(b"blackbox.project-catalog.owner-snapshot-set.v1\0");
    digest.update((source_id.len() as u64).to_be_bytes());
    digest.update(source_id.as_bytes());
    let mut byte_len = 0u64;
    let mut first_corrupt = None;
    let mut missing = false;
    for snapshot in snapshots {
        digest.update((snapshot.source_id.len() as u64).to_be_bytes());
        digest.update(snapshot.source_id.as_bytes());
        digest.update(snapshot.canonical_sha256.as_bytes());
        match &snapshot.state {
            OwnerSnapshotStateV1::Present {
                content_sha256,
                byte_len: source_len,
            } => {
                digest.update(b"present");
                digest.update(content_sha256.as_bytes());
                byte_len = byte_len
                    .checked_add(*source_len)
                    .ok_or_else(|| invalid_source("owner_snapshot_byte_count_overflow"))?;
            }
            OwnerSnapshotStateV1::Missing { fingerprint } => {
                missing = true;
                digest.update(b"missing");
                digest.update(fingerprint.as_bytes());
            }
            OwnerSnapshotStateV1::Corrupt {
                diagnostic_code,
                fingerprint,
            } => {
                first_corrupt.get_or_insert_with(|| diagnostic_code.clone());
                digest.update(b"corrupt");
                digest.update(diagnostic_code.as_bytes());
                digest.update(fingerprint.as_bytes());
            }
        }
    }
    let fingerprint = Sha256ValueV1::parse(hex::encode(digest.finalize()))
        .expect("SHA-256 encoding is always a valid hash");
    Ok(if let Some(diagnostic_code) = first_corrupt {
        InventorySourceStateV1::Corrupt {
            fingerprint: fingerprint.clone(),
            content_hash: Some(fingerprint),
            diagnostic_code,
        }
    } else if missing {
        InventorySourceStateV1::Missing { fingerprint }
    } else {
        InventorySourceStateV1::Present {
            fingerprint: fingerprint.clone(),
            content_hash: fingerprint,
            byte_len,
        }
    })
}

fn direct_owner_state(
    source_id: &str,
    state: &str,
    content_fingerprint: Option<&str>,
    diagnostic_code: Option<&str>,
) -> InventorySourceStateV1 {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(source_id.as_bytes());
    bytes.extend_from_slice(state.as_bytes());
    if let Some(value) = content_fingerprint {
        bytes.extend_from_slice(value.as_bytes());
    }
    let fingerprint = Sha256ValueV1::digest(&bytes);
    match state {
        "present" => InventorySourceStateV1::Present {
            fingerprint: fingerprint.clone(),
            content_hash: content_fingerprint
                .and_then(|value| Sha256ValueV1::parse(value.to_string()).ok())
                .unwrap_or(fingerprint),
            byte_len: 0,
        },
        "missing" => InventorySourceStateV1::Missing { fingerprint },
        _ => InventorySourceStateV1::Corrupt {
            fingerprint,
            content_hash: None,
            diagnostic_code: diagnostic_code
                .unwrap_or("owner_source_corrupt")
                .to_string(),
        },
    }
}

fn lane_capture<T>(
    lane_kind: ImmutableInventoryLaneKindV1,
    source_id: &str,
    mut owners: Vec<(
        ImmutableInventoryOwnerKindV1,
        String,
        InventorySourceStateV1,
        BTreeSet<String>,
    )>,
    mut rows: Vec<T>,
) -> AdapterResult<ImmutableLaneCaptureV1<T>> {
    let complete = owners
        .iter()
        .all(|(_, _, state, _)| matches!(state, InventorySourceStateV1::Present { .. }));
    if !complete {
        rows.clear();
        for (_, _, _, row_ids) in &mut owners {
            row_ids.clear();
        }
    }
    let owner_subsources = owners
        .into_iter()
        .map(
            |(owner_kind, owner_source_id, source_state, row_observation_ids)| {
                OwnerSubsourceEvidenceV1::new(
                    owner_kind,
                    owner_source_id,
                    source_state,
                    row_observation_ids,
                )
            },
        )
        .collect();
    let evidence = ImmutableInventoryLaneEvidenceV1::from_owner_subsources(
        lane_kind,
        source_id,
        rows.len() as u64,
        owner_subsources,
    )
    .map_err(|error| invalid_source(error.to_string()))?;
    Ok(ImmutableLaneCaptureV1 { evidence, rows })
}

fn corpus_source_state(
    source_id: &str,
    state: &CorpusMigrationSourceStateV1,
    fingerprint: Option<&str>,
) -> InventorySourceStateV1 {
    match state {
        CorpusMigrationSourceStateV1::Present => {
            direct_owner_state(source_id, "present", fingerprint, None)
        }
        CorpusMigrationSourceStateV1::Missing => {
            direct_owner_state(source_id, "missing", None, None)
        }
        CorpusMigrationSourceStateV1::Corrupt { diagnostic_code } => {
            direct_owner_state(source_id, "corrupt", fingerprint, Some(diagnostic_code))
        }
        CorpusMigrationSourceStateV1::Unavailable { .. } => {
            unreachable!("unavailable corpus owner evidence is rejected before lane projection")
        }
    }
}

fn vector_source_state(snapshot: &VectorMigrationSnapshotV1) -> InventorySourceStateV1 {
    match &snapshot.state {
        VectorMigrationSourceStateV1::Present => direct_owner_state(
            "vector-metadata",
            "present",
            snapshot.source_fingerprint_sha256.as_deref(),
            None,
        ),
        VectorMigrationSourceStateV1::Missing => {
            direct_owner_state("vector-metadata", "missing", None, None)
        }
        VectorMigrationSourceStateV1::Corrupt { diagnostic_code } => direct_owner_state(
            "vector-metadata",
            "corrupt",
            snapshot.source_fingerprint_sha256.as_deref(),
            Some(diagnostic_code),
        ),
        VectorMigrationSourceStateV1::Unavailable { .. } => {
            unreachable!("unavailable vector owner evidence is rejected before lane projection")
        }
    }
}

fn edge_source_state(snapshot: &EdgeMigrationSnapshotV1) -> InventorySourceStateV1 {
    match &snapshot.state {
        EdgeMigrationSourceStateV1::Present => direct_owner_state(
            "edge-manifest",
            "present",
            snapshot.source_fingerprint_sha256.as_deref(),
            None,
        ),
        EdgeMigrationSourceStateV1::Missing => {
            direct_owner_state("edge-manifest", "missing", None, None)
        }
        EdgeMigrationSourceStateV1::Corrupt { diagnostic_code } => direct_owner_state(
            "edge-manifest",
            "corrupt",
            snapshot.source_fingerprint_sha256.as_deref(),
            Some(diagnostic_code),
        ),
        EdgeMigrationSourceStateV1::Unavailable { .. } => {
            unreachable!("unavailable edge owner evidence is rejected before lane projection")
        }
    }
}

fn capture_project_scoped_refs_lane(
    corpus: &CorpusOwnerMigrationSnapshotV1,
    vectors: &VectorMigrationSnapshotV1,
) -> AdapterResult<ImmutableLaneCaptureV1<ProjectScopedRefObservationV1>> {
    let corpus_state = corpus_source_state(
        "tantivy",
        &corpus.index.state,
        corpus.index.source_fingerprint_sha256.as_deref(),
    );
    let vector_state = vector_source_state(vectors);
    let mut rows = Vec::new();
    let mut tantivy_row_ids = BTreeSet::new();
    let mut vector_row_ids = BTreeSet::new();
    if matches!(corpus_state, InventorySourceStateV1::Present { .. }) {
        for row in &corpus.index.project_scoped_refs {
            let project_id = ProjectId::parse(row.project_id.clone())
                .map_err(|_| invalid_source("tantivy_project_id_invalid"))?;
            for occurrence in 0..row.document_count {
                let stable_row_id = stable_observation_id_v1(
                    "tantivy-row",
                    &[
                        row.project_id.as_bytes(),
                        row.entity_ref.as_bytes(),
                        &occurrence.to_be_bytes(),
                    ],
                )?;
                let observation_id = stable_observation_id_v1(
                    "project-ref",
                    &[b"tantivy", stable_row_id.as_bytes()],
                )?;
                tantivy_row_ids.insert(observation_id.clone());
                rows.push(ProjectScopedRefObservationV1 {
                    observation_id,
                    store_kind: ProjectScopedRefStoreKindV1::Tantivy,
                    project_id: project_id.clone(),
                    stable_row_id,
                    entity_ref_hash: Sha256ValueV1::digest(row.entity_ref.as_bytes()),
                });
            }
        }
    }
    if matches!(vector_state, InventorySourceStateV1::Present { .. }) {
        for row in &vectors.project_scoped_refs {
            let project_id = ProjectId::parse(row.project_id.clone())
                .map_err(|_| invalid_source("vector_project_id_invalid"))?;
            let stable_row_id = stable_observation_id_v1(
                "vector-row",
                &[
                    row.route.as_bytes(),
                    row.project_id.as_bytes(),
                    row.entity_ref.as_bytes(),
                    row.content_hash.as_bytes(),
                ],
            )?;
            let observation_id =
                stable_observation_id_v1("project-ref", &[b"vector", stable_row_id.as_bytes()])?;
            vector_row_ids.insert(observation_id.clone());
            rows.push(ProjectScopedRefObservationV1 {
                observation_id,
                store_kind: ProjectScopedRefStoreKindV1::VectorMetadata,
                project_id,
                stable_row_id,
                entity_ref_hash: Sha256ValueV1::digest(row.entity_ref.as_bytes()),
            });
        }
    }
    lane_capture(
        ImmutableInventoryLaneKindV1::ProjectScopedRefs,
        "project-scoped-refs",
        vec![
            (
                ImmutableInventoryOwnerKindV1::Tantivy,
                "tantivy".to_string(),
                corpus_state,
                tantivy_row_ids,
            ),
            (
                ImmutableInventoryOwnerKindV1::VectorMetadata,
                "vector-metadata".to_string(),
                vector_state,
                vector_row_ids,
            ),
        ],
        rows,
    )
}

fn capture_edge_workspaces_lane(
    edges: &EdgeMigrationSnapshotV1,
) -> AdapterResult<ImmutableLaneCaptureV1<EdgeWorkspaceObservationV1>> {
    let state = edge_source_state(edges);
    let mut rows = Vec::new();
    let mut row_ids = BTreeSet::new();
    if matches!(state, InventorySourceStateV1::Present { .. }) {
        for workspace in &edges.workspaces {
            let project_id = ProjectId::parse(workspace.project_id.clone())
                .map_err(|_| invalid_source("edge_workspace_project_id_invalid"))?;
            let observation_id = stable_observation_id_v1(
                "edge-workspace",
                &[
                    workspace.workspace_id.as_bytes(),
                    workspace.project_id.as_bytes(),
                ],
            )?;
            let selector_bytes = serde_json::to_vec(&(
                &workspace.active_snapshot_id,
                &workspace.active_dirty_overlay_id,
                &workspace.active_snapshot_path,
                &workspace.dirty_overlay_path,
                &workspace.repo_materialization,
                &workspace.code_source_selector,
                &workspace.code_source_generation,
            ))
            .map_err(|_| invalid_source("edge_workspace_selector_encode_failed"))?;
            row_ids.insert(observation_id.clone());
            rows.push(EdgeWorkspaceObservationV1 {
                observation_id,
                workspace_id: workspace.workspace_id.clone(),
                project_ids: BTreeSet::from([project_id]),
                manifest_hash: Sha256ValueV1::parse(
                    workspace.manifest_source_fingerprint_sha256.clone(),
                )
                .map_err(|_| invalid_source("edge_workspace_manifest_hash_invalid"))?,
                active_selector_hash: Sha256ValueV1::digest(&selector_bytes),
            });
        }
    }
    lane_capture(
        ImmutableInventoryLaneKindV1::EdgeWorkspaces,
        "edge-workspaces",
        vec![(
            ImmutableInventoryOwnerKindV1::EdgeManifest,
            "edge-manifest".to_string(),
            state,
            row_ids,
        )],
        rows,
    )
}

fn checkout_owner_state(
    checkout_captures: &[CheckoutCaptureV1],
) -> AdapterResult<InventorySourceStateV1> {
    let rows = checkout_captures
        .iter()
        .map(|capture| &capture.observation)
        .collect::<Vec<_>>();
    let bytes =
        serde_json::to_vec(&rows).map_err(|_| invalid_source("checkout_evidence_encode_failed"))?;
    Ok(InventorySourceStateV1::Present {
        fingerprint: Sha256ValueV1::digest(&bytes),
        content_hash: Sha256ValueV1::digest(&bytes),
        byte_len: bytes.len() as u64,
    })
}

fn capture_checkouts_lane(
    checkout_captures: &[CheckoutCaptureV1],
) -> AdapterResult<ImmutableLaneCaptureV1<CheckoutObservationV1>> {
    let rows = checkout_captures
        .iter()
        .map(|capture| capture.observation.clone())
        .collect::<Vec<_>>();
    let row_ids = rows
        .iter()
        .map(|row| row.observation_id.clone())
        .collect::<BTreeSet<_>>();
    lane_capture(
        ImmutableInventoryLaneKindV1::Checkouts,
        "checkouts",
        vec![(
            ImmutableInventoryOwnerKindV1::Checkout,
            "checkout".to_string(),
            checkout_owner_state(checkout_captures)?,
            row_ids,
        )],
        rows,
    )
}

pub(crate) fn attachment_observation_id(key: &AttachmentCandidateKeyV1) -> AdapterResult<String> {
    stable_observation_id_v1(
        "attachment",
        &[
            key.project_id.as_str().as_bytes(),
            key.checkout_observation_id.as_bytes(),
            key.base_relpath.as_bytes(),
        ],
    )
}

fn capture_attachment_candidates_lane(
    legacy: &LegacyProjectsCaptureV1,
    checkout_captures: &[CheckoutCaptureV1],
    plan: &AttachmentCandidateIdentityPlanV1,
) -> AdapterResult<ImmutableLaneCaptureV1<AttachmentCandidateObservationV1>> {
    let keys = discover_attachment_candidate_keys_locked(legacy, checkout_captures)?;
    let expected = keys.iter().cloned().collect::<BTreeSet<_>>();
    let supplied = plan.identities.keys().cloned().collect::<BTreeSet<_>>();
    if expected != supplied {
        return Err(invalid_input(
            "attachment identity plan does not exactly cover discovered candidates",
        ));
    }
    if plan.identities.values().collect::<BTreeSet<_>>().len() != plan.identities.len() {
        return Err(invalid_input(
            "attachment identity plan reuses an attachment id",
        ));
    }
    let mut rows = Vec::new();
    for key in keys {
        let attachment_id = plan
            .identities
            .get(&key)
            .ok_or_else(|| invalid_input("attachment identity plan entry is missing"))?
            .clone();
        let observed_scope = legacy.published_scopes.get(&key.project_id).cloned();
        rows.push(AttachmentCandidateObservationV1 {
            observation_id: attachment_observation_id(&key)?,
            attachment_id,
            project_id: key.project_id,
            checkout_observation_id: key.checkout_observation_id,
            base_relpath: key.base_relpath,
            observed_scope,
        });
    }
    let row_ids = rows
        .iter()
        .map(|row| row.observation_id.clone())
        .collect::<BTreeSet<_>>();
    lane_capture(
        ImmutableInventoryLaneKindV1::AttachmentCandidates,
        "attachment-candidates",
        vec![
            (
                ImmutableInventoryOwnerKindV1::LegacyProjectStore,
                "legacy-project-store".to_string(),
                legacy.owner_state.clone(),
                row_ids.clone(),
            ),
            (
                ImmutableInventoryOwnerKindV1::Checkout,
                "checkout".to_string(),
                checkout_owner_state(checkout_captures)?,
                row_ids,
            ),
        ],
        rows,
    )
}

fn capture_materialized_aliases_lane(
    legacy: &LegacyProjectsCaptureV1,
) -> AdapterResult<ImmutableLaneCaptureV1<MaterializedAliasObservationV1>> {
    let mut rows = Vec::new();
    for project in &legacy.observations {
        let project_id = ProjectId::parse(project.record.project_id.clone())
            .map_err(|_| invalid_source("legacy_project_id_invalid"))?;
        for alias in &project.record.aliases {
            rows.push(MaterializedAliasObservationV1 {
                observation_id: stable_observation_id_v1(
                    "materialized-alias",
                    &[project_id.as_str().as_bytes(), alias.as_bytes()],
                )?,
                alias: alias.clone(),
                project_id: project_id.clone(),
                registered_at: Some(project.record.registered_at.clone()),
            });
        }
    }
    let row_ids = rows
        .iter()
        .map(|row| row.observation_id.clone())
        .collect::<BTreeSet<_>>();
    lane_capture(
        ImmutableInventoryLaneKindV1::MaterializedAliases,
        "materialized-aliases",
        vec![(
            ImmutableInventoryOwnerKindV1::LegacyProjectStore,
            "legacy-project-store".to_string(),
            legacy.owner_state.clone(),
            row_ids,
        )],
        rows,
    )
}

fn capture_inventory_targets_lane(
    artifacts: &[OwnerSnapshotV1],
    provenance: &[OwnerSnapshotV1],
) -> AdapterResult<ImmutableLaneCaptureV1<InventoryTargetObservationV1>> {
    let artifact_state = snapshot_owner_state("artifact", artifacts)?;
    let provenance_state = snapshot_owner_state("provenance", provenance)?;
    let mut rows = Vec::new();
    let mut owner_rows = BTreeMap::<ImmutableInventoryOwnerKindV1, BTreeSet<String>>::new();
    for (owner_kind, target_kind, snapshots) in [
        (
            ImmutableInventoryOwnerKindV1::Artifact,
            InventoryTargetKindV1::ProjectArtifact,
            artifacts,
        ),
        (
            ImmutableInventoryOwnerKindV1::Provenance,
            InventoryTargetKindV1::ProvenanceNote,
            provenance,
        ),
    ] {
        for snapshot in snapshots {
            for raw in &snapshot.rows {
                let OwnerSnapshotRowValueV1::InventoryTarget {
                    project_id,
                    target_sha256,
                } = &raw.value
                else {
                    continue;
                };
                let project_id = ProjectId::parse(project_id.clone())
                    .map_err(|_| invalid_source("inventory_target_project_id_invalid"))?;
                let stable_target_id = stable_observation_id_v1(
                    "target-row",
                    &[
                        owner_kind_token(owner_kind).as_bytes(),
                        raw.stable_row_id.as_bytes(),
                    ],
                )?;
                let observation_id = stable_observation_id_v1(
                    "inventory-target",
                    &[
                        owner_kind_token(owner_kind).as_bytes(),
                        stable_target_id.as_bytes(),
                    ],
                )?;
                owner_rows
                    .entry(owner_kind)
                    .or_default()
                    .insert(observation_id.clone());
                rows.push(InventoryTargetObservationV1 {
                    observation_id,
                    target_kind,
                    project_id,
                    stable_target_id,
                    target_hash: Sha256ValueV1::parse(target_sha256.clone())
                        .map_err(|_| invalid_source("inventory_target_hash_invalid"))?,
                });
            }
        }
    }
    lane_capture(
        ImmutableInventoryLaneKindV1::InventoryTargets,
        "inventory-targets",
        vec![
            (
                ImmutableInventoryOwnerKindV1::Artifact,
                "artifact".to_string(),
                artifact_state,
                owner_rows
                    .get(&ImmutableInventoryOwnerKindV1::Artifact)
                    .cloned()
                    .unwrap_or_default(),
            ),
            (
                ImmutableInventoryOwnerKindV1::Provenance,
                "provenance".to_string(),
                provenance_state,
                owner_rows
                    .get(&ImmutableInventoryOwnerKindV1::Provenance)
                    .cloned()
                    .unwrap_or_default(),
            ),
        ],
        rows,
    )
}

fn owner_kind_token(kind: ImmutableInventoryOwnerKindV1) -> &'static str {
    match kind {
        ImmutableInventoryOwnerKindV1::Tantivy => "tantivy",
        ImmutableInventoryOwnerKindV1::VectorMetadata => "vector",
        ImmutableInventoryOwnerKindV1::EdgeManifest => "edge",
        ImmutableInventoryOwnerKindV1::GitMetadata => "git",
        ImmutableInventoryOwnerKindV1::Checkout => "checkout",
        ImmutableInventoryOwnerKindV1::LegacyProjectStore => "legacy-project",
        ImmutableInventoryOwnerKindV1::Knowledge => "knowledge",
        ImmutableInventoryOwnerKindV1::Gap => "gap",
        ImmutableInventoryOwnerKindV1::Thread => "thread",
        ImmutableInventoryOwnerKindV1::Note => "note",
        ImmutableInventoryOwnerKindV1::Pin => "pin",
        ImmutableInventoryOwnerKindV1::Roadmap => "roadmap",
        ImmutableInventoryOwnerKindV1::Packet => "packet",
        ImmutableInventoryOwnerKindV1::Task => "task",
        ImmutableInventoryOwnerKindV1::Proposal => "proposal",
        ImmutableInventoryOwnerKindV1::SlackBinding => "slack",
        ImmutableInventoryOwnerKindV1::Whiteboard => "whiteboard",
        ImmutableInventoryOwnerKindV1::Artifact => "artifact",
        ImmutableInventoryOwnerKindV1::Provenance => "provenance",
        ImmutableInventoryOwnerKindV1::TranscriptEdge => "transcript-edge",
        ImmutableInventoryOwnerKindV1::DerivedRepoGrouping => "derived-repo",
        ImmutableInventoryOwnerKindV1::DerivedLegacyNamespaceClusters => "derived-namespace",
    }
}

fn legacy_store_token(kind: LegacyPathStoreKindV1) -> &'static str {
    match kind {
        LegacyPathStoreKindV1::Knowledge => "knowledge",
        LegacyPathStoreKindV1::Gap => "gap",
        LegacyPathStoreKindV1::Thread => "thread",
        LegacyPathStoreKindV1::Note => "note",
        LegacyPathStoreKindV1::Pin => "pin",
        LegacyPathStoreKindV1::Roadmap => "roadmap",
        LegacyPathStoreKindV1::Packet => "packet",
        LegacyPathStoreKindV1::Task => "task",
        LegacyPathStoreKindV1::Proposal => "proposal",
        LegacyPathStoreKindV1::SlackBinding => "slack",
        LegacyPathStoreKindV1::Whiteboard => "whiteboard",
        LegacyPathStoreKindV1::Artifact => "artifact",
        LegacyPathStoreKindV1::Provenance => "provenance",
        LegacyPathStoreKindV1::TranscriptEdge => "transcript-edge",
    }
}

fn legacy_owner_kind(kind: LegacyPathStoreKindV1) -> ImmutableInventoryOwnerKindV1 {
    match kind {
        LegacyPathStoreKindV1::Knowledge => ImmutableInventoryOwnerKindV1::Knowledge,
        LegacyPathStoreKindV1::Gap => ImmutableInventoryOwnerKindV1::Gap,
        LegacyPathStoreKindV1::Thread => ImmutableInventoryOwnerKindV1::Thread,
        LegacyPathStoreKindV1::Note => ImmutableInventoryOwnerKindV1::Note,
        LegacyPathStoreKindV1::Pin => ImmutableInventoryOwnerKindV1::Pin,
        LegacyPathStoreKindV1::Roadmap => ImmutableInventoryOwnerKindV1::Roadmap,
        LegacyPathStoreKindV1::Packet => ImmutableInventoryOwnerKindV1::Packet,
        LegacyPathStoreKindV1::Task => ImmutableInventoryOwnerKindV1::Task,
        LegacyPathStoreKindV1::Proposal => ImmutableInventoryOwnerKindV1::Proposal,
        LegacyPathStoreKindV1::SlackBinding => ImmutableInventoryOwnerKindV1::SlackBinding,
        LegacyPathStoreKindV1::Whiteboard => ImmutableInventoryOwnerKindV1::Whiteboard,
        LegacyPathStoreKindV1::Artifact => ImmutableInventoryOwnerKindV1::Artifact,
        LegacyPathStoreKindV1::Provenance => ImmutableInventoryOwnerKindV1::Provenance,
        LegacyPathStoreKindV1::TranscriptEdge => ImmutableInventoryOwnerKindV1::TranscriptEdge,
    }
}

fn legacy_owner_snapshots(
    durable: &DurableOwnerSnapshotsV1,
    kind: LegacyPathStoreKindV1,
) -> &[OwnerSnapshotV1] {
    match kind {
        LegacyPathStoreKindV1::Knowledge => &durable.knowledge,
        LegacyPathStoreKindV1::Gap => &durable.gap,
        LegacyPathStoreKindV1::Thread => &durable.thread,
        LegacyPathStoreKindV1::Note => &durable.note,
        LegacyPathStoreKindV1::Pin => &durable.pin,
        LegacyPathStoreKindV1::Roadmap => &durable.roadmap,
        LegacyPathStoreKindV1::Packet => &durable.packet,
        LegacyPathStoreKindV1::Task => &durable.task,
        LegacyPathStoreKindV1::Proposal => &durable.proposal,
        LegacyPathStoreKindV1::SlackBinding => &durable.slack_binding,
        LegacyPathStoreKindV1::Whiteboard => &durable.whiteboard,
        LegacyPathStoreKindV1::Artifact => &durable.artifact,
        LegacyPathStoreKindV1::Provenance => &durable.provenance,
        LegacyPathStoreKindV1::TranscriptEdge => &durable.transcript_edge,
    }
}

fn selector_kind(kind: LegacyProjectSelectorKindV1) -> LegacySelectorKindV1 {
    match kind {
        LegacyProjectSelectorKindV1::Project => LegacySelectorKindV1::Project,
        LegacyProjectSelectorKindV1::ProjectAndRelativePath => {
            LegacySelectorKindV1::ProjectAndRelativePath
        }
        LegacyProjectSelectorKindV1::AbsolutePath => LegacySelectorKindV1::AbsolutePath,
    }
}

fn capture_legacy_path_observations_lane(
    durable: &DurableOwnerSnapshotsV1,
) -> AdapterResult<(
    ImmutableLaneCaptureV1<LegacyPathObservationV1>,
    BTreeMap<String, RuntimeLiteralBindingV1>,
)> {
    let kinds = [
        LegacyPathStoreKindV1::Knowledge,
        LegacyPathStoreKindV1::Gap,
        LegacyPathStoreKindV1::Thread,
        LegacyPathStoreKindV1::Note,
        LegacyPathStoreKindV1::Pin,
        LegacyPathStoreKindV1::Roadmap,
        LegacyPathStoreKindV1::Packet,
        LegacyPathStoreKindV1::Task,
        LegacyPathStoreKindV1::Proposal,
        LegacyPathStoreKindV1::SlackBinding,
        LegacyPathStoreKindV1::Whiteboard,
        LegacyPathStoreKindV1::Artifact,
        LegacyPathStoreKindV1::Provenance,
        LegacyPathStoreKindV1::TranscriptEdge,
    ];
    let mut rows = Vec::new();
    let mut bindings = BTreeMap::new();
    let mut owners = Vec::new();
    for kind in kinds {
        let snapshots = legacy_owner_snapshots(durable, kind);
        let state = snapshot_owner_state(legacy_store_token(kind), snapshots)?;
        let mut row_ids = BTreeSet::new();
        if matches!(state, InventorySourceStateV1::Present { .. }) {
            for snapshot in snapshots {
                for raw in &snapshot.rows {
                    let OwnerSnapshotRowValueV1::LegacyProjectSelector {
                        selector_kind: raw_kind,
                        literal_selector,
                    } = &raw.value
                    else {
                        continue;
                    };
                    let stable_row_id = stable_observation_id_v1(
                        "legacy-row",
                        &[
                            legacy_store_token(kind).as_bytes(),
                            raw.stable_row_id.as_bytes(),
                        ],
                    )?;
                    let observation_id = stable_observation_id_v1(
                        "legacy-path",
                        &[
                            legacy_store_token(kind).as_bytes(),
                            stable_row_id.as_bytes(),
                        ],
                    )?;
                    let digest = digest_path(literal_selector);
                    if bindings
                        .insert(
                            observation_id.clone(),
                            RuntimeLiteralBindingV1 {
                                digest: digest.clone(),
                                literal: literal_selector.clone(),
                            },
                        )
                        .is_some()
                    {
                        return Err(invalid_source("legacy_path_observation_duplicate"));
                    }
                    row_ids.insert(observation_id.clone());
                    rows.push(LegacyPathObservationV1 {
                        observation_id,
                        store_kind: kind,
                        stable_row_id,
                        selector_kind: selector_kind(*raw_kind),
                        selector_digest: digest,
                    });
                }
            }
        }
        owners.push((
            legacy_owner_kind(kind),
            legacy_store_token(kind).to_string(),
            state,
            row_ids,
        ));
    }
    let lane = lane_capture(
        ImmutableInventoryLaneKindV1::LegacyPathObservations,
        "legacy-path-observations",
        owners,
        rows,
    )?;
    if lane.rows.is_empty() {
        bindings.clear();
    }
    Ok((lane, bindings))
}

fn aggregate_inventory_states(
    source_id: &str,
    states: &[InventorySourceStateV1],
) -> InventorySourceStateV1 {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(source_id.as_bytes());
    let mut byte_len = 0u64;
    let mut first_corrupt = None;
    let mut missing = false;
    for state in states {
        match state {
            InventorySourceStateV1::Present {
                fingerprint,
                content_hash,
                byte_len: source_len,
            } => {
                bytes.extend_from_slice(b"present");
                bytes.extend_from_slice(fingerprint.as_str().as_bytes());
                bytes.extend_from_slice(content_hash.as_str().as_bytes());
                byte_len = byte_len.saturating_add(*source_len);
            }
            InventorySourceStateV1::Missing { fingerprint } => {
                missing = true;
                bytes.extend_from_slice(b"missing");
                bytes.extend_from_slice(fingerprint.as_str().as_bytes());
            }
            InventorySourceStateV1::Corrupt {
                fingerprint,
                diagnostic_code,
                ..
            } => {
                first_corrupt.get_or_insert_with(|| diagnostic_code.clone());
                bytes.extend_from_slice(b"corrupt");
                bytes.extend_from_slice(fingerprint.as_str().as_bytes());
                bytes.extend_from_slice(diagnostic_code.as_bytes());
            }
        }
    }
    let fingerprint = Sha256ValueV1::digest(&bytes);
    if let Some(diagnostic_code) = first_corrupt {
        InventorySourceStateV1::Corrupt {
            fingerprint: fingerprint.clone(),
            content_hash: Some(fingerprint),
            diagnostic_code,
        }
    } else if missing {
        InventorySourceStateV1::Missing { fingerprint }
    } else {
        InventorySourceStateV1::Present {
            fingerprint: fingerprint.clone(),
            content_hash: fingerprint,
            byte_len,
        }
    }
}

fn empty_set_commitment(domain: &[u8]) -> Sha256ValueV1 {
    Sha256ValueV1::digest(domain)
}

fn namespace_attribution(
    namespace: &str,
    legacy: &LegacyProjectsCaptureV1,
) -> LegacyCommitNamespaceAttributionV1 {
    let proved = legacy
        .observations
        .iter()
        .filter_map(|project| {
            let authority = project.committed_authority.as_ref()?;
            (authority.authority.as_str() == namespace)
                .then(|| ProjectId::parse(project.record.project_id.clone()).ok())
                .flatten()
        })
        .collect::<BTreeSet<_>>();
    if !proved.is_empty() {
        return LegacyCommitNamespaceAttributionV1::Proved {
            project_ids: proved,
        };
    }
    let candidates = legacy
        .observations
        .iter()
        .filter(|project| project.record.repo_id.as_deref() == Some(namespace))
        .filter_map(|project| ProjectId::parse(project.record.project_id.clone()).ok())
        .collect::<BTreeSet<_>>();
    if candidates.len() >= 2 {
        LegacyCommitNamespaceAttributionV1::Ambiguous {
            candidate_project_ids: candidates,
        }
    } else {
        LegacyCommitNamespaceAttributionV1::Unclaimed
    }
}

fn attributed_projects(attribution: &LegacyCommitNamespaceAttributionV1) -> BTreeSet<ProjectId> {
    match attribution {
        LegacyCommitNamespaceAttributionV1::Proved { project_ids } => project_ids.clone(),
        LegacyCommitNamespaceAttributionV1::Ambiguous {
            candidate_project_ids,
        } => candidate_project_ids.clone(),
        LegacyCommitNamespaceAttributionV1::Unclaimed => BTreeSet::new(),
    }
}

fn capture_commit_namespaces(
    corpus: &CorpusOwnerMigrationSnapshotV1,
    vectors: &VectorMigrationSnapshotV1,
    legacy: &LegacyProjectsCaptureV1,
) -> AdapterResult<Vec<LegacyCommitNamespaceInventoryV1>> {
    let corpus_rows = corpus
        .index
        .commit_namespaces
        .iter()
        .map(|row| (row.namespace.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let vector_rows = vectors
        .commit_namespaces
        .iter()
        .map(|row| (row.namespace.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let namespaces = corpus_rows
        .keys()
        .chain(vector_rows.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut rows = Vec::new();
    for namespace in namespaces {
        let corpus_row = corpus_rows.get(namespace).copied();
        let vector_row = vector_rows.get(namespace).copied();
        let observation_id = stable_observation_id_v1("commit-namespace", &[namespace.as_bytes()])?;
        rows.push(LegacyCommitNamespaceInventoryV1 {
            observation_id,
            namespace: CommitNamespace::parse(namespace.to_string())
                .map_err(|_| invalid_source("legacy_commit_namespace_invalid"))?,
            commit_document_count: corpus_row
                .map(|row| row.commit_document_count)
                .unwrap_or_default(),
            commit_document_set_sha256: corpus_row
                .map(|row| Sha256ValueV1::parse(row.commit_document_commitment_sha256.clone()))
                .transpose()
                .map_err(|_| invalid_source("commit_document_commitment_invalid"))?
                .unwrap_or_else(|| {
                    empty_set_commitment(b"blackbox.corpus-index.commit-namespace.v1\0")
                }),
            vector_key_count: vector_row
                .map(|row| row.vector_key_count)
                .unwrap_or_default(),
            vector_key_set_sha256: vector_row
                .map(|row| Sha256ValueV1::parse(row.vector_key_commitment_sha256.clone()))
                .transpose()
                .map_err(|_| invalid_source("vector_key_commitment_invalid"))?
                .unwrap_or_else(|| empty_set_commitment(b"blackbox.vectors.commit-namespace.v1\0")),
            attribution: namespace_attribution(namespace, legacy),
        });
    }
    rows.sort_by(|left, right| left.namespace.cmp(&right.namespace));
    Ok(rows)
}

fn capture_git_metadata_lane(
    corpus: &CorpusOwnerMigrationSnapshotV1,
    vectors: &VectorMigrationSnapshotV1,
    legacy: &LegacyProjectsCaptureV1,
    publisher_source: &ExactDecodedSourceV1<PublisherRefInventoryV1>,
    checkout_captures: &[CheckoutCaptureV1],
    attachment_candidates: &[AttachmentCandidateObservationV1],
) -> AdapterResult<(
    ImmutableLaneCaptureV1<GitMetadataObservationV1>,
    Vec<LegacyCommitNamespaceInventoryV1>,
    BTreeMap<String, AuthorizedInventoryPath>,
)> {
    let index_state = corpus_source_state(
        "tantivy",
        &corpus.index.state,
        corpus.index.source_fingerprint_sha256.as_deref(),
    );
    let code_metadata_state = corpus_source_state(
        "tantivy-code-metadata",
        &corpus.code_metadata.state,
        corpus.code_metadata.source_fingerprint_sha256.as_deref(),
    );
    let tantivy_state = aggregate_inventory_states("tantivy", &[index_state, code_metadata_state]);
    let vector_state = vector_source_state(vectors);
    let cursor_state = corpus_source_state(
        "git-cursors",
        &corpus.git_cursors.state,
        corpus.git_cursors.source_fingerprint_sha256.as_deref(),
    );
    let legacy_commit_namespaces = if matches!(
        (&tantivy_state, &vector_state),
        (
            InventorySourceStateV1::Present { .. },
            InventorySourceStateV1::Present { .. }
        )
    ) {
        capture_commit_namespaces(corpus, vectors, legacy)?
    } else {
        Vec::new()
    };
    let namespace_projects = legacy_commit_namespaces
        .iter()
        .map(|row| {
            (
                row.namespace.as_str().to_string(),
                attributed_projects(&row.attribution),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let cursor_by_project = corpus
        .git_cursors
        .rows
        .iter()
        .map(|row| (row.project_id.as_str(), row.last_ingested_sha.clone()))
        .collect::<BTreeMap<_, _>>();
    let checkout_by_id = checkout_captures
        .iter()
        .map(|capture| (capture.observation.observation_id.as_str(), capture))
        .collect::<BTreeMap<_, _>>();
    let legacy_by_project = legacy
        .observations
        .iter()
        .map(|project| (project.record.project_id.as_str(), project))
        .collect::<BTreeMap<_, _>>();
    let mut rows = Vec::new();
    let mut runtime_common_dirs = BTreeMap::new();
    let mut git_probe_corrupt = None;
    for attachment in attachment_candidates {
        let project = legacy_by_project
            .get(attachment.project_id.as_str())
            .copied()
            .ok_or_else(|| invalid_source("attachment_legacy_project_missing"))?;
        let checkout = checkout_by_id
            .get(attachment.checkout_observation_id.as_str())
            .copied()
            .ok_or_else(|| invalid_source("attachment_checkout_missing"))?;
        let repository = checkout.repository.as_ref();
        let common_directory =
            repository.map(|repository| repository.common_directory().to_path_buf());
        let head = repository
            .map(StableGitRepository::verified_head)
            .transpose()
            .map_err(|_| invalid_source("git_repository_head_unavailable"))?
            .flatten();
        let first_commit = match (repository, head.as_ref()) {
            (Some(repository), Some(head)) => repository
                .first_commit_oid(head.oid())
                .map_err(|_| invalid_source("git_first_commit_unavailable"))?,
            _ => None,
        };
        if project.record.is_git_repo
            && (common_directory.is_none() || head.is_some() && first_commit.is_none())
        {
            git_probe_corrupt.get_or_insert("git_repository_evidence_unavailable");
            continue;
        }
        let materialized_commit_namespaces = namespace_projects
            .iter()
            .filter(|(_, projects)| projects.contains(&attachment.project_id))
            .map(|(namespace, _)| namespace.clone())
            .collect::<BTreeSet<_>>();
        let mut resolved_refs = BTreeMap::new();
        for publisher in &publisher_source.value.rows {
            let belongs = legacy
                .published_scopes
                .get(&attachment.project_id)
                .is_some_and(|scope| scope == &publisher.scope);
            if !belongs {
                continue;
            }
            let repository = repository
                .ok_or_else(|| invalid_source("publisher_repository_authority_missing"))?;
            if let Some(commit) = repository
                .resolve_commit_oid(&publisher.branch_ref)
                .map_err(|_| invalid_source("publisher_ref_invalid"))?
                .filter(|commit| {
                    verified_commit_declares_scope(
                        repository,
                        commit,
                        &attachment.base_relpath,
                        &publisher.scope,
                    )
                })
            {
                resolved_refs.insert(publisher.branch_ref.clone(), commit);
            }
        }
        let observation_id = stable_observation_id_v1(
            "git-metadata",
            &[
                attachment.project_id.as_str().as_bytes(),
                attachment.checkout_observation_id.as_bytes(),
            ],
        )?;
        let common_directory_digest = common_directory
            .as_ref()
            .and_then(|path| path.to_str())
            .map(digest_path);
        if let Some(common_directory) = common_directory {
            let authorized = AuthorizedInventoryPath::new(common_directory)?;
            runtime_common_dirs.insert(observation_id.clone(), authorized);
        }
        rows.push(GitMetadataObservationV1 {
            observation_id,
            project_id: attachment.project_id.clone(),
            checkout_observation_id: attachment.checkout_observation_id.clone(),
            common_directory_digest,
            full_first_commit: first_commit,
            materialized_commit_namespaces,
            last_ingested_sha: cursor_by_project
                .get(attachment.project_id.as_str())
                .cloned()
                .flatten(),
            resolved_refs,
        });
    }
    let represented_cursor_projects = rows
        .iter()
        .map(|row| row.project_id.as_str())
        .collect::<BTreeSet<_>>();
    if corpus
        .git_cursors
        .rows
        .iter()
        .any(|cursor| !represented_cursor_projects.contains(cursor.project_id.as_str()))
    {
        git_probe_corrupt.get_or_insert("git_cursor_checkout_evidence_missing");
    }
    let direct_git_state = if let Some(code) = git_probe_corrupt {
        direct_owner_state("git-metadata", "corrupt", None, Some(code))
    } else {
        let rows_bytes =
            serde_json::to_vec(&rows).map_err(|_| invalid_source("git_metadata_encode_failed"))?;
        aggregate_inventory_states(
            "git-metadata",
            &[
                cursor_state,
                InventorySourceStateV1::Present {
                    fingerprint: Sha256ValueV1::digest(&rows_bytes),
                    content_hash: Sha256ValueV1::digest(&rows_bytes),
                    byte_len: rows_bytes.len() as u64,
                },
            ],
        )
    };
    let git_row_ids = rows
        .iter()
        .map(|row| row.observation_id.clone())
        .collect::<BTreeSet<_>>();
    let namespace_row_ids = legacy_commit_namespaces
        .iter()
        .map(|row| row.observation_id.clone())
        .collect::<BTreeSet<_>>();
    let mut lane = lane_capture(
        ImmutableInventoryLaneKindV1::GitMetadata,
        "git-metadata",
        vec![
            (
                ImmutableInventoryOwnerKindV1::GitMetadata,
                "git-metadata".to_string(),
                direct_git_state,
                git_row_ids,
            ),
            (
                ImmutableInventoryOwnerKindV1::Tantivy,
                "tantivy".to_string(),
                tantivy_state,
                namespace_row_ids.clone(),
            ),
            (
                ImmutableInventoryOwnerKindV1::VectorMetadata,
                "vector-metadata".to_string(),
                vector_state,
                namespace_row_ids,
            ),
        ],
        rows,
    )?;
    if matches!(
        lane.evidence.completeness,
        crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Complete
    ) {
        lane.evidence = ImmutableInventoryLaneEvidenceV1::from_owner_subsources(
            ImmutableInventoryLaneKindV1::GitMetadata,
            "git-metadata",
            lane.rows.len() as u64 + legacy_commit_namespaces.len() as u64,
            lane.evidence.owner_subsources.clone(),
        )
        .map_err(|error| invalid_source(error.to_string()))?;
    }
    if lane.rows.is_empty() {
        runtime_common_dirs.clear();
    }
    let legacy_commit_namespaces = if lane.evidence.completeness
        == crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Complete
    {
        legacy_commit_namespaces
    } else {
        Vec::new()
    };
    Ok((lane, legacy_commit_namespaces, runtime_common_dirs))
}

fn capture_repo_grouping_proofs_lane(
    legacy: &LegacyProjectsCaptureV1,
    code_sources: &[CodeSourceCaptureV1],
    git_metadata: &ImmutableLaneCaptureV1<GitMetadataObservationV1>,
) -> AdapterResult<ImmutableLaneCaptureV1<RepoGroupingProofV1>> {
    let derived_state = aggregate_inventory_states(
        "derived-repo-grouping",
        &[
            legacy.owner_state.clone(),
            git_metadata.evidence.source_state.clone(),
        ],
    );
    let mut rows = Vec::new();
    if matches!(derived_state, InventorySourceStateV1::Present { .. }) {
        let mut authority_groups =
            BTreeMap::<RecordedRepoAuthority, Vec<RecordedAuthorityEvidenceMemberV1>>::new();
        for project in &legacy.observations {
            let Some(authority) = &project.committed_authority else {
                continue;
            };
            let project_id = ProjectId::parse(project.record.project_id.clone())
                .map_err(|_| invalid_source("legacy_project_id_invalid"))?;
            authority_groups
                .entry(authority.authority.clone())
                .or_default()
                .push(RecordedAuthorityEvidenceMemberV1 {
                    project_id,
                    authority: authority.authority.clone(),
                    authority_observation_id: authority.observation_id.clone(),
                });
        }
        for (authority, mut members) in authority_groups {
            if members.len() < 2 {
                continue;
            }
            members.sort_by(|left, right| left.project_id.cmp(&right.project_id));
            rows.push(RepoGroupingProofV1::IdenticalCommittedRecordedAuthority {
                proof_id: stable_observation_id_v1(
                    "repo-proof",
                    &[b"recorded-authority", authority.as_str().as_bytes()],
                )?,
                members,
            });
        }

        let mut git_groups = BTreeMap::<(Sha256ValueV1, String), Vec<GitEvidenceMemberV1>>::new();
        for git in &git_metadata.rows {
            let (Some(common), Some(first_commit)) =
                (&git.common_directory_digest, &git.full_first_commit)
            else {
                continue;
            };
            git_groups
                .entry((common.clone(), first_commit.clone()))
                .or_default()
                .push(GitEvidenceMemberV1 {
                    project_id: git.project_id.clone(),
                    git_observation_id: git.observation_id.clone(),
                });
        }
        for ((common, first_commit), mut members) in git_groups {
            members.sort_by(|left, right| left.project_id.cmp(&right.project_id));
            members.dedup_by(|left, right| left.project_id == right.project_id);
            if members.len() < 2 {
                continue;
            }
            rows.push(
                RepoGroupingProofV1::SharedGitCommonDirectoryAndFirstCommit {
                    proof_id: stable_observation_id_v1(
                        "repo-proof",
                        &[
                            b"git-common-first",
                            common.as_str().as_bytes(),
                            first_commit.as_bytes(),
                        ],
                    )?,
                    members,
                },
            );
        }

        let mut collected_groups = BTreeMap::<String, Vec<CollectedEvidenceMemberV1>>::new();
        for source in code_sources {
            for generation in &source.observation.generations {
                if !generation.checkout_missing {
                    continue;
                }
                let Some(scope) = &generation.activation_scope else {
                    continue;
                };
                if !matches!(
                    &generation.descriptor,
                    crate::project_catalog_inventory::ImmutableCollectedDescriptorV1::Valid {
                        published_scope,
                        ..
                    } if published_scope == scope
                ) {
                    continue;
                }
                collected_groups
                    .entry(scope.repo_id().to_string())
                    .or_default()
                    .push(CollectedEvidenceMemberV1 {
                        project_id: generation.project_id.clone(),
                        generation_observation_id: generation.observation_id.clone(),
                    });
            }
        }
        for (repo_id, mut members) in collected_groups {
            members.sort_by(|left, right| left.project_id.cmp(&right.project_id));
            members.dedup_by(|left, right| left.project_id == right.project_id);
            if members.len() < 2 {
                continue;
            }
            rows.push(
                RepoGroupingProofV1::CollectedDescriptorActivationAgreement {
                    proof_id: stable_observation_id_v1(
                        "repo-proof",
                        &[b"collected-agreement", repo_id.as_bytes()],
                    )?,
                    members,
                },
            );
        }
    }
    let row_ids = rows
        .iter()
        .map(|proof| proof.proof_id().to_string())
        .collect::<BTreeSet<_>>();
    lane_capture(
        ImmutableInventoryLaneKindV1::RepoGroupingProofs,
        "repo-grouping-proofs",
        vec![(
            ImmutableInventoryOwnerKindV1::DerivedRepoGrouping,
            "derived-repo-grouping".to_string(),
            derived_state,
            row_ids,
        )],
        rows,
    )
}

fn capture_legacy_namespace_clusters_lane(
    legacy: &LegacyProjectsCaptureV1,
    git_metadata: &ImmutableLaneCaptureV1<GitMetadataObservationV1>,
) -> AdapterResult<ImmutableLaneCaptureV1<LegacyNamespaceClusterObservationV1>> {
    let derived_state = aggregate_inventory_states(
        "derived-legacy-namespaces",
        &[
            legacy.owner_state.clone(),
            git_metadata.evidence.source_state.clone(),
        ],
    );
    let mut rows = Vec::new();
    if matches!(derived_state, InventorySourceStateV1::Present { .. }) {
        let mut projects_by_namespace = BTreeMap::<String, BTreeSet<ProjectId>>::new();
        for git in &git_metadata.rows {
            for namespace in &git.materialized_commit_namespaces {
                projects_by_namespace
                    .entry(namespace.clone())
                    .or_default()
                    .insert(git.project_id.clone());
            }
        }
        for (namespace, project_ids) in projects_by_namespace {
            if project_ids.len() < 2 {
                continue;
            }
            let cluster_id =
                stable_observation_id_v1("namespace-cluster", &[namespace.as_bytes()])?;
            rows.push(LegacyNamespaceClusterObservationV1 {
                observation_id: stable_observation_id_v1(
                    "legacy-namespace",
                    &[cluster_id.as_bytes()],
                )?,
                cluster_id,
                materialized_namespace: namespace,
                project_ids,
            });
        }
    }
    let row_ids = rows
        .iter()
        .map(|row| row.observation_id.clone())
        .collect::<BTreeSet<_>>();
    lane_capture(
        ImmutableInventoryLaneKindV1::LegacyNamespaceClusters,
        "legacy-namespace-clusters",
        vec![(
            ImmutableInventoryOwnerKindV1::DerivedLegacyNamespaceClusters,
            "derived-legacy-namespaces".to_string(),
            derived_state,
            row_ids,
        )],
        rows,
    )
}

fn source_evidence(
    source_id: &str,
    source_kind: MutableInventorySourceKindV1,
    source_locator: MutableInventorySourceLocatorV1,
    state: InventorySourceStateV1,
    row_observation_ids: BTreeSet<String>,
) -> MutableInventorySourceEvidenceV1 {
    let row_set_sha256 = mutable_source_row_set_hash(&row_observation_ids);
    MutableInventorySourceEvidenceV1 {
        source_id: source_id.to_string(),
        source_kind,
        source_locator,
        state,
        row_observation_ids,
        row_set_sha256,
    }
}

fn exact_source_evidence<T>(
    source_id: &str,
    source_kind: MutableInventorySourceKindV1,
    source_locator: MutableInventorySourceLocatorV1,
    source: &ExactDecodedSourceV1<T>,
    row_observation_ids: BTreeSet<String>,
) -> MutableInventorySourceEvidenceV1 {
    source_evidence(
        source_id,
        source_kind,
        source_locator,
        if source.was_missing {
            InventorySourceStateV1::Missing {
                fingerprint: missing_source_fingerprint(source_id),
            }
        } else {
            present_source_state(&source.source)
        },
        row_observation_ids,
    )
}

fn present_source_state(source: &ExactSourceBytesV1) -> InventorySourceStateV1 {
    InventorySourceStateV1::Present {
        fingerprint: source.fingerprint.clone(),
        content_hash: source.content_hash.clone(),
        byte_len: source.bytes.len() as u64,
    }
}

fn present_bytes_state(bytes: &[u8]) -> InventorySourceStateV1 {
    present_source_state(&ExactSourceBytesV1::new(bytes.to_vec()))
}

fn file_observation_state(
    source: &AuthorizedFileObservationV1,
    source_id: &str,
) -> InventorySourceStateV1 {
    match source {
        AuthorizedFileObservationV1::NotFound => InventorySourceStateV1::Missing {
            fingerprint: missing_source_fingerprint(source_id),
        },
        AuthorizedFileObservationV1::Present(source) => present_source_state(source),
        AuthorizedFileObservationV1::Invalid { diagnostic_code } => {
            InventorySourceStateV1::Corrupt {
                fingerprint: missing_source_fingerprint(source_id),
                content_hash: None,
                diagnostic_code: diagnostic_code.clone(),
            }
        }
    }
}

fn missing_source_fingerprint(source_id: &str) -> Sha256ValueV1 {
    let mut bytes = b"blackbox.project-catalog.missing-source.v1\0".to_vec();
    bytes.extend_from_slice(source_id.as_bytes());
    Sha256ValueV1::digest(&bytes)
}

fn decode_source<T>(
    source: AuthorizedFileObservationV1,
    decode: impl FnOnce(&[u8]) -> Result<T, ()>,
    invalid_code: &'static str,
) -> DecodedSourceObservationV1<T> {
    match source {
        AuthorizedFileObservationV1::NotFound => DecodedSourceObservationV1::NotFound,
        AuthorizedFileObservationV1::Invalid { diagnostic_code } => {
            DecodedSourceObservationV1::Invalid {
                source: None,
                diagnostic_code,
            }
        }
        AuthorizedFileObservationV1::Present(source) => match decode(&source.bytes) {
            Ok(value) => DecodedSourceObservationV1::Valid(ExactDecodedSourceV1 {
                source,
                value,
                was_missing: false,
            }),
            Err(()) => DecodedSourceObservationV1::Invalid {
                source: Some(source),
                diagnostic_code: invalid_code.to_string(),
            },
        },
    }
}

fn stable_observation_id_v1(kind: &str, parts: &[&[u8]]) -> AdapterResult<String> {
    if kind.is_empty()
        || kind.len() > 32
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
    {
        return Err(invalid_input("observation id kind is invalid"));
    }
    let mut digest = Sha256::new();
    digest.update(b"blackbox.project-catalog.inventory-observation.v1\0");
    digest.update((kind.len() as u64).to_be_bytes());
    digest.update(kind.as_bytes());
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    Ok(format!("{kind}-{}", hex::encode(digest.finalize())))
}

fn validate_lane_kinds(lanes: &ImmutableInventoryLanesV1) -> AdapterResult<()> {
    for (actual, expected) in [
        (
            lanes.project_scoped_refs.evidence.lane_kind,
            ImmutableInventoryLaneKindV1::ProjectScopedRefs,
        ),
        (
            lanes.edge_workspaces.evidence.lane_kind,
            ImmutableInventoryLaneKindV1::EdgeWorkspaces,
        ),
        (
            lanes.git_metadata.evidence.lane_kind,
            ImmutableInventoryLaneKindV1::GitMetadata,
        ),
        (
            lanes.checkouts.evidence.lane_kind,
            ImmutableInventoryLaneKindV1::Checkouts,
        ),
        (
            lanes.attachment_candidates.evidence.lane_kind,
            ImmutableInventoryLaneKindV1::AttachmentCandidates,
        ),
        (
            lanes.inventory_targets.evidence.lane_kind,
            ImmutableInventoryLaneKindV1::InventoryTargets,
        ),
        (
            lanes.materialized_aliases.evidence.lane_kind,
            ImmutableInventoryLaneKindV1::MaterializedAliases,
        ),
        (
            lanes.legacy_path_observations.evidence.lane_kind,
            ImmutableInventoryLaneKindV1::LegacyPathObservations,
        ),
        (
            lanes.repo_grouping_proofs.evidence.lane_kind,
            ImmutableInventoryLaneKindV1::RepoGroupingProofs,
        ),
        (
            lanes.legacy_namespace_clusters.evidence.lane_kind,
            ImmutableInventoryLaneKindV1::LegacyNamespaceClusters,
        ),
    ] {
        if actual != expected {
            return Err(invalid_input("immutable lane kind does not match field"));
        }
    }
    Ok(())
}

fn lane_evidence(lanes: &ImmutableInventoryLanesV1) -> Vec<ImmutableInventoryLaneEvidenceV1> {
    vec![
        lanes.project_scoped_refs.evidence.clone(),
        lanes.edge_workspaces.evidence.clone(),
        lanes.git_metadata.evidence.clone(),
        lanes.checkouts.evidence.clone(),
        lanes.attachment_candidates.evidence.clone(),
        lanes.inventory_targets.evidence.clone(),
        lanes.materialized_aliases.evidence.clone(),
        lanes.legacy_path_observations.evidence.clone(),
        lanes.repo_grouping_proofs.evidence.clone(),
        lanes.legacy_namespace_clusters.evidence.clone(),
    ]
}

fn sort_lane_rows(lanes: &mut ImmutableInventoryLanesV1) {
    lanes
        .project_scoped_refs
        .rows
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    lanes
        .edge_workspaces
        .rows
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    lanes
        .git_metadata
        .rows
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    lanes
        .checkouts
        .rows
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    lanes
        .attachment_candidates
        .rows
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    lanes
        .inventory_targets
        .rows
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    lanes
        .materialized_aliases
        .rows
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    lanes
        .legacy_path_observations
        .rows
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    lanes
        .repo_grouping_proofs
        .rows
        .sort_by(|left, right| left.proof_id().cmp(right.proof_id()));
    lanes
        .legacy_namespace_clusters
        .rows
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
}

fn invalid_input(detail: impl Into<String>) -> InventoryAdapterError {
    InventoryAdapterError::new("error.project_catalog_inventory_adapter_input", detail)
}

fn invalid_source(detail: impl Into<String>) -> InventoryAdapterError {
    InventoryAdapterError::new("error.project_catalog_inventory_adapter_source", detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::process::Command;

    use bbox_code_source::{
        GenerationDescriptor, ManifestEntry, SCHEMA_VERSION, WALKER_POLICY_VERSION,
        dirty_fingerprint, generation_id, manifest_sha256, source_selector,
    };
    use bbox_code_source_store::{
        CodeSourceStorePaths, CollisionRetirementEntryV1, CollisionRetirementLifecycleV1,
        MigrationEffectiveSourceManifestV1, MigrationEffectiveSourceSelectionV1, StoredGeneration,
        encode_collision_retirement_pending_for_migration,
        encode_migration_effective_source_manifest_v1,
    };

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn run_git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Inventory Test")
            .env("GIT_AUTHOR_EMAIL", "inventory@example.invalid")
            .env("GIT_COMMITTER_NAME", "Inventory Test")
            .env("GIT_COMMITTER_EMAIL", "inventory@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn publisher_adapter_uses_the_owner_codec() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("publisher-refs.json");
        write(&path, br#"{"version":1}"#);
        let store = PublisherRefStore::open(&path).unwrap();
        let captured = capture_publisher_ref_source(&store).unwrap();
        assert!(captured.source.value.rows.is_empty());

        write(&path, br#"{"version":1,"refs":[],"invented":true}"#);
        assert!(PublisherRefStore::open(&path).is_err());
    }

    #[test]
    fn committed_authority_is_bound_to_the_verified_commit() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        run_git(&root, &["init", "-q"]);
        write(
            &root.join(".bbox/config.toml"),
            b"[project]\nrepo_id = \"family-one\"\n",
        );
        run_git(&root, &["add", ".bbox/config.toml"]);
        run_git(&root, &["commit", "-qm", "record authority"]);
        let commit = run_git(&root, &["rev-parse", "HEAD"]);
        write(
            &root.join(".bbox/config.toml"),
            b"[project]\nrepo_id = \"family-two\"\n",
        );

        let project_id = ProjectId::parse("project-a").unwrap();
        let root = AuthorizedInventoryPath::new(&root).unwrap();
        let repository = open_stable_git_repository(&root.authority)
            .unwrap()
            .unwrap();
        let verified_commit = repository.verify_commit_oid(&commit).unwrap();
        let probe = observe_committed_authority_probe(
            "committed-config-project-a",
            &project_id,
            &root,
            Some(&CommittedConfigSourceV1 {
                repository_root: root.as_path().to_path_buf(),
                commit: verified_commit,
            }),
        )
        .unwrap();
        assert_eq!(probe.authority.unwrap().as_str(), "family-one");
        assert!(matches!(
            probe.source_evidence.source_locator,
            MutableInventorySourceLocatorV1::CommittedProjectConfig {
                commit_oid,
                ..
            } if commit_oid == commit
        ));
        assert!(verified_commit_declares_scope(
            &repository,
            &commit,
            ".",
            &PublishedScope::try_new("family-one", ".").unwrap(),
        ));
        assert!(!verified_commit_declares_scope(
            &repository,
            &commit,
            ".",
            &PublishedScope::try_new("family-two", ".").unwrap(),
        ));
    }

    #[test]
    fn quarantined_generation_keeps_exact_metadata_and_manifest_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store =
            CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap();
        let paths = CodeSourceStorePaths::new(store.root()).unwrap();
        let project_id = ProjectId::parse("project-a").unwrap();
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let content = b"fn main() {}\n";
        let entries = vec![ManifestEntry {
            relative_path: "src/main.rs".to_string(),
            content_sha256: hex::encode(Sha256::digest(content)),
            size: content.len() as u64,
        }];
        let head = "b".repeat(40);
        let descriptor = GenerationDescriptor {
            schema_version: SCHEMA_VERSION,
            walker_policy_version: WALKER_POLICY_VERSION.to_string(),
            scope: scope.clone(),
            head_commit: head.clone(),
            dirty_fingerprint: dirty_fingerprint(&head, &entries),
            manifest_sha256: manifest_sha256(&entries),
            file_count: 1,
            logical_bytes: content.len() as u64,
        };
        let active_generation_id = generation_id("host-a", &descriptor);
        let descriptor_manifest_sha256 = descriptor.manifest_sha256.clone();
        let stored = StoredGeneration {
            version: 1,
            generation_id: active_generation_id.clone(),
            producer_id: "host-a".to_string(),
            ordinal: 1,
            descriptor: descriptor.clone(),
            state: GenerationState::Active,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(1),
            entity_inventory_sha256: Some("c".repeat(64)),
        };
        let metadata = serde_json::to_vec(&stored).unwrap();
        let mut manifest = Vec::new();
        serde_json::to_writer(&mut manifest, &entries[0]).unwrap();
        manifest.push(b'\n');
        write(
            &paths
                .generation_metadata(&scope, &active_generation_id)
                .unwrap(),
            &metadata,
        );
        write(
            &paths
                .generation_manifest(&scope, &active_generation_id)
                .unwrap(),
            &manifest,
        );
        let retained_generation_id = generation_id("host-retained", &descriptor);
        write(
            &paths
                .generation_metadata(&scope, &retained_generation_id)
                .unwrap(),
            &serde_json::to_vec(&StoredGeneration {
                version: 1,
                generation_id: retained_generation_id.clone(),
                producer_id: "host-retained".to_string(),
                ordinal: 0,
                descriptor: descriptor.clone(),
                state: GenerationState::Superseded,
                diagnostic: None,
                created_unix_secs: 0,
                materialized_doc_count: Some(1),
                entity_inventory_sha256: Some("c".repeat(64)),
            })
            .unwrap(),
        );
        write(
            &paths
                .generation_manifest(&scope, &retained_generation_id)
                .unwrap(),
            &manifest,
        );
        let selector = format!(
            "{}:m0123456789abcdef",
            source_selector(project_id.as_str(), &active_generation_id)
        );
        write(
            &paths.activation(&project_id),
            &serde_json::to_vec(&ActivationRecord {
                version: 1,
                project_id: project_id.to_string(),
                generation_id: active_generation_id.clone(),
                selector: selector.clone(),
                snapshot_id: format!("collected-{}", "e".repeat(32)),
                document_count: 1,
                entity_inventory_sha256: "c".repeat(64),
                current_chunk_targets: BTreeMap::new(),
                activated_unix_secs: 1,
                cutback_pending: false,
                diagnostic: None,
            })
            .unwrap(),
        );
        write(
            &paths.anchor(),
            &encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
                version: 1,
                selections: vec![MigrationEffectiveSourceSelectionV1 {
                    project_id: project_id.clone(),
                    published_scope: scope.clone(),
                    generation_id: active_generation_id.clone(),
                    selector: selector.clone(),
                }],
            })
            .unwrap(),
        );
        let mut collision = CollisionRetirementLifecycleV1 {
            version: 1,
            project_id: project_id.clone(),
            entries: BTreeMap::from([
                (
                    active_generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: scope.clone(),
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                            selector,
                        ),
                        snapshot_id: format!("collected-{}", "e".repeat(32)),
                        manifest_sha256: descriptor_manifest_sha256.clone(),
                        inventory_hash: "d".repeat(64),
                        plan_hash: "f".repeat(64),
                    },
                ),
                (
                    retained_generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: scope.clone(),
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::NoDurableSelector,
                        snapshot_id: format!("collected-{}", "f".repeat(32)),
                        manifest_sha256: descriptor_manifest_sha256,
                        inventory_hash: "d".repeat(64),
                        plan_hash: "f".repeat(64),
                    },
                ),
            ]),
        };
        write(
            &paths.collision_retirement_pending(&project_id),
            &encode_collision_retirement_pending_for_migration(&collision).unwrap(),
        );
        let snapshot =
            capture_code_source_inventory(&store, &BTreeSet::from([scope.clone()])).unwrap();
        let mismatched_scope = PublishedScope::try_new("different-repo", ".").unwrap();
        assert_eq!(
            observe_code_sources(
                &snapshot,
                &BTreeMap::from([(project_id.clone(), mismatched_scope)]),
                &BTreeSet::new(),
            )
            .unwrap_err()
            .to_string(),
            "error.project_catalog_inventory_adapter_source: active_descriptor_committed_scope_mismatch"
        );
        let capture = observe_code_sources(
            &snapshot,
            &BTreeMap::new(),
            &BTreeSet::from([project_id.clone()]),
        )
        .unwrap();
        let quarantined = capture.sources[0]
            .observation
            .quarantine
            .iter()
            .find(|generation| generation.generation_id == active_generation_id)
            .unwrap();
        assert_eq!(quarantined.generation_id, active_generation_id);
        assert!(matches!(
            quarantined.descriptor,
            ImmutableCollectedDescriptorV1::Valid { .. }
        ));
        assert!(matches!(
            quarantined.manifest,
            ImmutableArtifactObservationV1::Valid { .. }
        ));
        assert_eq!(
            quarantined.collision_lifecycle.selector_evidence,
            DurableSelectorEvidenceV1::ExactMaterialized {
                selector_hash: Sha256ValueV1::digest(
                    collision
                        .entry(&active_generation_id)
                        .unwrap()
                        .exact_selector()
                        .unwrap()
                        .as_bytes(),
                ),
            }
        );
        assert!(capture.sources[0].source_evidence.iter().any(|source| {
            source.source_kind == MutableInventorySourceKindV1::CodeSourceCollisionLifecycle
                && source
                    .row_observation_ids
                    .contains(&quarantined.observation_id)
        }));
        let active_generation_sources = capture.sources[0]
            .source_evidence
            .iter()
            .filter(|source| {
                matches!(
                    &source.source_locator,
                    MutableInventorySourceLocatorV1::CodeSourceGenerationMetadata {
                        generation_id,
                        ..
                    } | MutableInventorySourceLocatorV1::CodeSourceGenerationManifest {
                        generation_id,
                        ..
                    } if generation_id == &active_generation_id
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(active_generation_sources.len(), 2);
        assert!(active_generation_sources.iter().all(|source| {
            source
                .row_observation_ids
                .contains(&quarantined.observation_id)
        }));
        let retained = capture.sources[0]
            .observation
            .quarantine
            .iter()
            .find(|generation| generation.generation_id == retained_generation_id)
            .unwrap();
        assert_eq!(retained.generation_id, retained_generation_id);
        assert_eq!(
            retained.collision_lifecycle.selector_evidence,
            DurableSelectorEvidenceV1::NoDurableSelector
        );

        drop(capture);
        drop(snapshot);
        let mut incomplete_collision = collision.clone();
        incomplete_collision.entries.remove(&retained_generation_id);
        write(
            &paths.collision_retirement_pending(&project_id),
            &encode_collision_retirement_pending_for_migration(&incomplete_collision).unwrap(),
        );
        let snapshot =
            capture_code_source_inventory(&store, &BTreeSet::from([scope.clone()])).unwrap();
        assert_eq!(
            observe_code_sources(
                &snapshot,
                &BTreeMap::new(),
                &BTreeSet::from([project_id.clone()]),
            )
            .unwrap_err()
            .to_string(),
            "error.project_catalog_inventory_adapter_source: collision_lifecycle_owner_generation_set_mismatch"
        );
        drop(snapshot);
        write(
            &paths.collision_retirement_pending(&project_id),
            &encode_collision_retirement_pending_for_migration(&collision).unwrap(),
        );
        fs::remove_file(paths.activation(&project_id)).unwrap();
        fs::write(
            paths.anchor(),
            encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
                version: 1,
                selections: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        let snapshot =
            capture_code_source_inventory(&store, &BTreeSet::from([scope.clone()])).unwrap();
        let capture = observe_code_sources(
            &snapshot,
            &BTreeMap::from([(project_id.clone(), scope.clone())]),
            &BTreeSet::new(),
        )
        .unwrap();
        let active_quarantine = capture.sources[0]
            .observation
            .quarantine
            .iter()
            .find(|generation| generation.generation_id == active_generation_id)
            .unwrap();
        assert!(matches!(
            active_quarantine.collision_lifecycle.selector_evidence,
            DurableSelectorEvidenceV1::ExactMaterialized { .. }
        ));

        drop(capture);
        drop(snapshot);
        fs::remove_file(paths.collision_retirement_pending(&project_id)).unwrap();
        let retained_project_id = ProjectId::parse("project-retained").unwrap();
        let mut retained_origin = stored.clone();
        retained_origin.state = GenerationState::Superseded;
        write(
            &paths
                .generation_metadata(&scope, &active_generation_id)
                .unwrap(),
            &serde_json::to_vec(&retained_origin).unwrap(),
        );
        collision.project_id = retained_project_id.clone();
        collision
            .entries
            .get_mut(&active_generation_id)
            .unwrap()
            .selector_evidence = CollisionRetirementSelectorEvidenceV1::NoDurableSelector;
        write(
            &paths.collision_retirement_pending(&retained_project_id),
            &encode_collision_retirement_pending_for_migration(&collision).unwrap(),
        );
        let snapshot =
            capture_code_source_inventory(&store, &BTreeSet::from([scope.clone()])).unwrap();
        let capture = observe_code_sources(
            &snapshot,
            &BTreeMap::from([(project_id, scope)]),
            &BTreeSet::new(),
        )
        .unwrap();
        let retained_collision_source = capture
            .sources
            .iter()
            .find(|source| source.observation.project_id == retained_project_id)
            .unwrap();
        let retained_collision = retained_collision_source
            .observation
            .quarantine
            .iter()
            .find(|generation| generation.generation_id == active_generation_id)
            .unwrap();
        assert_eq!(
            retained_collision.collision_lifecycle.selector_evidence,
            DurableSelectorEvidenceV1::NoDurableSelector
        );
    }

    #[test]
    fn ambiguous_retained_owner_emits_bounded_resolution_candidates() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store =
            CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap();
        let paths = CodeSourceStorePaths::new(store.root()).unwrap();
        let scope = PublishedScope::try_new("repo-family", "services/shared").unwrap();
        let content = b"fn retained() {}\n";
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".to_string(),
            content_sha256: hex::encode(Sha256::digest(content)),
            size: content.len() as u64,
        }];
        let head = "b".repeat(40);
        let descriptor = GenerationDescriptor {
            schema_version: SCHEMA_VERSION,
            walker_policy_version: WALKER_POLICY_VERSION.to_string(),
            scope: scope.clone(),
            head_commit: head.clone(),
            dirty_fingerprint: dirty_fingerprint(&head, &entries),
            manifest_sha256: manifest_sha256(&entries),
            file_count: 1,
            logical_bytes: content.len() as u64,
        };
        let generation_id = generation_id("host-retained", &descriptor);
        let stored = StoredGeneration {
            version: 1,
            generation_id: generation_id.clone(),
            producer_id: "host-retained".to_string(),
            ordinal: 1,
            descriptor,
            state: GenerationState::Superseded,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(1),
            entity_inventory_sha256: Some("c".repeat(64)),
        };
        write(
            &paths.generation_metadata(&scope, &generation_id).unwrap(),
            &serde_json::to_vec(&stored).unwrap(),
        );
        let mut manifest = Vec::new();
        serde_json::to_writer(&mut manifest, &entries[0]).unwrap();
        manifest.push(b'\n');
        write(
            &paths.generation_manifest(&scope, &generation_id).unwrap(),
            &manifest,
        );

        let snapshot =
            capture_code_source_inventory(&store, &BTreeSet::from([scope.clone()])).unwrap();
        let project_a = ProjectId::parse("project-a").unwrap();
        let project_b = ProjectId::parse("project-b").unwrap();
        let capture = observe_code_sources(
            &snapshot,
            &BTreeMap::from([
                (project_a.clone(), scope.clone()),
                (project_b.clone(), scope.clone()),
            ]),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(capture.sources.is_empty());
        assert_eq!(capture.retained_owner_resolutions.len(), 1);
        let retained = &capture.retained_owner_resolutions[0];
        assert_eq!(
            retained.candidate_project_ids,
            BTreeSet::from([project_a, project_b])
        );
        assert_eq!(
            retained.selector_evidence,
            DurableSelectorEvidenceV1::NoDurableSelector
        );
    }

    #[cfg(unix)]
    #[test]
    fn authorized_path_rejects_symlink_and_target_swap() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = root.join("source.json");
        let replacement = root.join("replacement.json");
        write(&source, b"one");
        write(&replacement, b"two");
        let authorized = AuthorizedInventoryPath::new(&source).unwrap();
        fs::remove_file(&source).unwrap();
        symlink(&replacement, &source).unwrap();
        assert_eq!(
            read_authorized_file(&authorized, 32).unwrap_err().code(),
            "error.project_catalog_inventory_adapter_source"
        );
        assert!(AuthorizedInventoryPath::new(&source).is_err());

        fs::remove_file(&source).unwrap();
        write(&source, b"three");
        let authorized = AuthorizedInventoryPath::new(&source).unwrap();
        let observed = read_authorized_file_with_hook(&authorized, 32, || {
            fs::remove_file(&source).unwrap();
        })
        .unwrap();
        assert!(matches!(
            observed,
            AuthorizedFileObservationV1::Invalid { diagnostic_code }
                if diagnostic_code == "source_path_changed"
        ));
    }

    #[test]
    fn corrupt_legacy_source_is_retained_as_refusal_evidence() {
        let raw = ExactSourceBytesV1::new(b"{not-json".to_vec());
        let captured =
            accept_legacy_projects_source_for_inventory(DecodedSourceObservationV1::Invalid {
                source: Some(raw.clone()),
                diagnostic_code: "legacy_projects_invalid".to_string(),
            });

        assert_eq!(captured.exact.source, raw);
        assert!(!captured.exact.was_missing);
        assert!(captured.exact.value.projects.is_empty());
        assert!(matches!(
            captured.state,
            InventorySourceStateV1::Corrupt {
                diagnostic_code,
                ..
            } if diagnostic_code == "legacy_projects_invalid"
        ));
    }

    #[test]
    fn accepted_missing_legacy_store_is_complete_empty_owner_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("projects.json");
        let authorized = AuthorizedInventoryPath::new(&path).unwrap();
        let source = accept_missing_legacy_projects_source(
            capture_legacy_projects_source(&authorized).unwrap(),
        )
        .unwrap();
        assert!(source.was_missing);
        let captured = observe_legacy_projects(&source, Vec::new()).unwrap();
        assert!(captured.observations.is_empty());
        assert!(matches!(
            captured.owner_state,
            InventorySourceStateV1::Present { byte_len: 0, .. }
        ));
        assert!(!path.exists());
    }

    #[test]
    fn checkout_root_fingerprint_ignores_directory_mtime_and_entry_order() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let authorized = AuthorizedInventoryPath::new(&root).unwrap();
        let first = observe_checkout(&authorized, None).unwrap();
        write(&root.join("temporary"), b"x");
        fs::remove_file(root.join("temporary")).unwrap();
        let second = observe_checkout(&authorized, None).unwrap();
        assert_eq!(
            first.root_source_evidence.state,
            second.root_source_evidence.state
        );
    }

    fn present_owner_state(seed: &str) -> InventorySourceStateV1 {
        InventorySourceStateV1::Present {
            fingerprint: Sha256ValueV1::digest(seed.as_bytes()),
            content_hash: Sha256ValueV1::digest(seed.as_bytes()),
            byte_len: seed.len() as u64,
        }
    }

    #[test]
    fn owner_lane_completeness_is_exact_and_cannot_omit_a_subsource() {
        let complete = lane_capture::<ProjectScopedRefObservationV1>(
            ImmutableInventoryLaneKindV1::ProjectScopedRefs,
            "project-scoped-refs",
            vec![
                (
                    ImmutableInventoryOwnerKindV1::Tantivy,
                    "tantivy".to_string(),
                    present_owner_state("tantivy"),
                    BTreeSet::new(),
                ),
                (
                    ImmutableInventoryOwnerKindV1::VectorMetadata,
                    "vector-metadata".to_string(),
                    present_owner_state("vectors"),
                    BTreeSet::new(),
                ),
            ],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            complete.evidence.completeness,
            crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Complete
        );

        let missing = lane_capture::<ProjectScopedRefObservationV1>(
            ImmutableInventoryLaneKindV1::ProjectScopedRefs,
            "project-scoped-refs",
            vec![
                (
                    ImmutableInventoryOwnerKindV1::Tantivy,
                    "tantivy".to_string(),
                    InventorySourceStateV1::Missing {
                        fingerprint: Sha256ValueV1::digest(b"missing"),
                    },
                    BTreeSet::new(),
                ),
                (
                    ImmutableInventoryOwnerKindV1::VectorMetadata,
                    "vector-metadata".to_string(),
                    present_owner_state("vectors"),
                    BTreeSet::new(),
                ),
            ],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            missing.evidence.completeness,
            crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Missing
        );

        let corrupt = lane_capture::<ProjectScopedRefObservationV1>(
            ImmutableInventoryLaneKindV1::ProjectScopedRefs,
            "project-scoped-refs",
            vec![
                (
                    ImmutableInventoryOwnerKindV1::Tantivy,
                    "tantivy".to_string(),
                    InventorySourceStateV1::Corrupt {
                        fingerprint: Sha256ValueV1::digest(b"corrupt"),
                        content_hash: None,
                        diagnostic_code: "owner_decode_failed".to_string(),
                    },
                    BTreeSet::new(),
                ),
                (
                    ImmutableInventoryOwnerKindV1::VectorMetadata,
                    "vector-metadata".to_string(),
                    present_owner_state("vectors"),
                    BTreeSet::new(),
                ),
            ],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            corrupt.evidence.completeness,
            crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Corrupt
        );

        let omitted = lane_capture::<ProjectScopedRefObservationV1>(
            ImmutableInventoryLaneKindV1::ProjectScopedRefs,
            "project-scoped-refs",
            vec![(
                ImmutableInventoryOwnerKindV1::Tantivy,
                "tantivy".to_string(),
                present_owner_state("tantivy"),
                BTreeSet::new(),
            )],
            Vec::new(),
        )
        .unwrap_err();
        assert_eq!(
            omitted.code(),
            "error.project_catalog_inventory_adapter_source"
        );
    }

    fn namespace_legacy_capture() -> LegacyProjectsCaptureV1 {
        let project_id = ProjectId::parse("project-a").unwrap();
        LegacyProjectsCaptureV1 {
            observations: vec![LegacyProjectObservationV1 {
                observation_id: "legacy-project-a".to_string(),
                record: LegacyProjectRecordInventoryV1 {
                    project_id: project_id.to_string(),
                    repo_id: Some("repo-one".to_string()),
                    canonical_path_digest: digest_path("/tmp/project-a"),
                    registered_at: "2026-01-01T00:00:00Z".to_string(),
                    is_git_repo: true,
                    languages: BTreeSet::new(),
                    aliases: BTreeSet::new(),
                },
                path_status: LegacyProjectPathStatusV1::Missing,
                committed_authority: Some(
                    crate::project_catalog_inventory::CommittedAuthorityObservationV1 {
                        observation_id: "authority-project-a".to_string(),
                        authority: RecordedRepoAuthority::parse("repo-one").unwrap(),
                    },
                ),
                committed_scope: None,
            }],
            source_evidence: Vec::new(),
            owner_state: present_owner_state("legacy"),
            published_scopes: BTreeMap::new(),
            project_roots: BTreeMap::new(),
            repositories: BTreeMap::new(),
            runtime_project_paths: BTreeMap::new(),
        }
    }

    fn namespace_corpus_snapshot() -> CorpusOwnerMigrationSnapshotV1 {
        use bbox_corpus_index::index::migration_inventory::{
            CodeIndexMetadataMigrationSnapshotV1, CorpusCommitNamespaceV1,
            CorpusIndexMigrationSnapshotV1, GitCursorMigrationSnapshotV1,
        };

        CorpusOwnerMigrationSnapshotV1 {
            index: CorpusIndexMigrationSnapshotV1 {
                version: 1,
                state: CorpusMigrationSourceStateV1::Present,
                schema_version: Some("schema".to_string()),
                schema_fingerprint_sha256: Some("1".repeat(64)),
                source_fingerprint_sha256: Some("2".repeat(64)),
                document_count: 2,
                project_scoped_ref_count: 0,
                project_scoped_ref_commitment_sha256: "3".repeat(64),
                project_scoped_refs: Vec::new(),
                commit_namespaces: vec![CorpusCommitNamespaceV1 {
                    namespace: "repo-one".to_string(),
                    commit_document_count: 2,
                    commit_document_commitment_sha256: "4".repeat(64),
                }],
            },
            code_metadata: CodeIndexMetadataMigrationSnapshotV1 {
                version: 1,
                state: CorpusMigrationSourceStateV1::Present,
                schema_fingerprint_sha256: "5".repeat(64),
                source_fingerprint_sha256: Some("6".repeat(64)),
                row_count: 0,
                project_scoped_row_count: 0,
                row_commitment_sha256: "7".repeat(64),
                rows: Vec::new(),
            },
            git_cursors: GitCursorMigrationSnapshotV1 {
                version: 1,
                state: CorpusMigrationSourceStateV1::Present,
                schema_fingerprint_sha256: "8".repeat(64),
                source_fingerprint_sha256: Some("9".repeat(64)),
                row_count: 0,
                row_commitment_sha256: "a".repeat(64),
                rows: Vec::new(),
            },
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_probe_rejects_checkout_swap_before_any_repository_read() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let rehearsal = root.join("rehearsal");
        let project = rehearsal.join("project");
        let outside = root.join("protected");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&outside).unwrap();
        run_git(&project, &["init", "-q"]);
        run_git(&outside, &["init", "-q"]);
        write(
            &project.join(".bbox/config.toml"),
            b"[project]\nrepo_id = \"inside-authority\"\n",
        );
        run_git(&project, &["add", ".bbox/config.toml"]);
        run_git(&project, &["commit", "-qm", "inside"]);
        write(
            &outside.join(".bbox/config.toml"),
            b"[project]\nrepo_id = \"outside-sentinel\"\n",
        );
        run_git(&outside, &["add", ".bbox/config.toml"]);
        run_git(&outside, &["commit", "-qm", "outside"]);
        let projects_path = rehearsal.join("projects.json");
        write(
            &projects_path,
            &serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "projects": [{
                    "project_id": "project-a",
                    "canonical_path": project,
                    "registered_at": "2026-01-01T00:00:00Z",
                    "is_git_repo": true
                }]
            }))
            .unwrap(),
        );
        let projects_path = AuthorizedInventoryPath::new(&projects_path).unwrap();
        let source = accept_missing_legacy_projects_source(
            capture_legacy_projects_source(&projects_path).unwrap(),
        )
        .unwrap();
        let held = rehearsal.join("held-project");
        let error =
            derive_legacy_project_probes_with_hook(&source, Some(&rehearsal), |repository| {
                fs::rename(&project, &held).unwrap();
                symlink(&outside, &project).unwrap();
                let head = repository.verified_head().unwrap().unwrap();
                let committed = read_verified_committed_file_bytes_optional_bounded(
                    &head,
                    ".bbox/config.toml",
                    MAX_COMMITTED_CONFIG_BYTES,
                )
                .unwrap()
                .unwrap();
                assert_eq!(committed, b"[project]\nrepo_id = \"inside-authority\"\n");
                assert!(
                    !committed
                        .windows(b"outside-sentinel".len())
                        .any(|window| window == b"outside-sentinel")
                );
            })
            .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_inventory_adapter_source"
        );
        assert!(!error.to_string().contains("outside-sentinel"));
    }

    fn namespace_vector_snapshot() -> VectorMigrationSnapshotV1 {
        use bbox_vectors::migration_inventory::VectorCommitNamespaceV1;

        VectorMigrationSnapshotV1 {
            version: 1,
            state: VectorMigrationSourceStateV1::Present,
            schema_version: "schema".to_string(),
            schema_fingerprint_sha256: "b".repeat(64),
            source_fingerprint_sha256: Some("c".repeat(64)),
            partition_count: 0,
            active_key_count: 2,
            project_scoped_ref_count: 0,
            project_scoped_ref_commitment_sha256: "d".repeat(64),
            partitions: Vec::new(),
            project_scoped_refs: Vec::new(),
            commit_namespaces: vec![VectorCommitNamespaceV1 {
                namespace: "repo-one".to_string(),
                vector_key_count: 2,
                vector_key_commitment_sha256: "e".repeat(64),
            }],
        }
    }

    fn empty_edge_snapshot() -> EdgeMigrationSnapshotV1 {
        EdgeMigrationSnapshotV1 {
            version: 1,
            state: EdgeMigrationSourceStateV1::Present,
            schema_version: 1,
            schema_fingerprint_sha256: "f".repeat(64),
            source_fingerprint_sha256: Some("1".repeat(64)),
            workspace_count: 0,
            active_selector_count: 0,
            row_commitment_sha256: "2".repeat(64),
            workspaces: Vec::new(),
        }
    }

    #[test]
    fn unavailable_owner_capture_aborts_before_lane_projection() {
        let mut corpus = namespace_corpus_snapshot();
        let vectors = namespace_vector_snapshot();
        let edges = empty_edge_snapshot();
        corpus.git_cursors.state = CorpusMigrationSourceStateV1::Unavailable {
            diagnostic_code: "git_cursor_read_unavailable",
        };
        assert_eq!(
            ensure_owner_inventory_available(&corpus, &vectors, &edges)
                .unwrap_err()
                .code(),
            "error.project_catalog_inventory_adapter_source"
        );

        let corpus = namespace_corpus_snapshot();
        let mut vectors = namespace_vector_snapshot();
        vectors.state = VectorMigrationSourceStateV1::Unavailable {
            diagnostic_code: "vector_wal_read_unavailable",
        };
        assert_eq!(
            ensure_owner_inventory_available(&corpus, &vectors, &edges)
                .unwrap_err()
                .code(),
            "error.project_catalog_inventory_adapter_source"
        );

        let vectors = namespace_vector_snapshot();
        let mut edges = empty_edge_snapshot();
        edges.state = EdgeMigrationSourceStateV1::Unavailable {
            diagnostic_code: "edge_manifest_read_unavailable",
        };
        assert_eq!(
            ensure_owner_inventory_available(&corpus, &vectors, &edges)
                .unwrap_err()
                .code(),
            "error.project_catalog_inventory_adapter_source"
        );
    }

    fn empty_test_lane<T>(
        lane_kind: ImmutableInventoryLaneKindV1,
        completeness: crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1,
    ) -> ImmutableLaneCaptureV1<T> {
        ImmutableLaneCaptureV1 {
            evidence: ImmutableInventoryLaneEvidenceV1 {
                lane_kind,
                source_id: format!("test-{lane_kind:?}"),
                source_state: match completeness {
                    crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Complete => {
                        present_owner_state("test-lane")
                    }
                    crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Missing => {
                        InventorySourceStateV1::Missing {
                            fingerprint: missing_source_fingerprint("test-lane"),
                        }
                    }
                    crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Corrupt => {
                        direct_owner_state(
                            "test-lane",
                            "corrupt",
                            None,
                            Some("test_lane_corrupt"),
                        )
                    }
                },
                completeness,
                row_count: 0,
                owner_subsources: Vec::new(),
            },
            rows: Vec::new(),
        }
    }

    fn empty_test_lanes(
        git_completeness: crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1,
    ) -> ImmutableInventoryLanesV1 {
        use crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1 as Completeness;

        ImmutableInventoryLanesV1 {
            project_scoped_refs: empty_test_lane(
                ImmutableInventoryLaneKindV1::ProjectScopedRefs,
                Completeness::Complete,
            ),
            edge_workspaces: empty_test_lane(
                ImmutableInventoryLaneKindV1::EdgeWorkspaces,
                Completeness::Complete,
            ),
            git_metadata: empty_test_lane(
                ImmutableInventoryLaneKindV1::GitMetadata,
                git_completeness,
            ),
            checkouts: empty_test_lane(
                ImmutableInventoryLaneKindV1::Checkouts,
                Completeness::Complete,
            ),
            attachment_candidates: empty_test_lane(
                ImmutableInventoryLaneKindV1::AttachmentCandidates,
                Completeness::Complete,
            ),
            inventory_targets: empty_test_lane(
                ImmutableInventoryLaneKindV1::InventoryTargets,
                Completeness::Complete,
            ),
            materialized_aliases: empty_test_lane(
                ImmutableInventoryLaneKindV1::MaterializedAliases,
                Completeness::Complete,
            ),
            legacy_path_observations: empty_test_lane(
                ImmutableInventoryLaneKindV1::LegacyPathObservations,
                Completeness::Complete,
            ),
            repo_grouping_proofs: empty_test_lane(
                ImmutableInventoryLaneKindV1::RepoGroupingProofs,
                Completeness::Complete,
            ),
            legacy_namespace_clusters: empty_test_lane(
                ImmutableInventoryLaneKindV1::LegacyNamespaceClusters,
                Completeness::Complete,
            ),
        }
    }

    #[test]
    fn publisher_pins_preserve_duplicate_and_ownerless_scope_authority() {
        let expected_scope = PublishedScope::try_new("repo-one", ".").unwrap();
        let source = ExactDecodedSourceV1 {
            source: ExactSourceBytesV1::new(Vec::new()),
            value: PublisherRefInventoryV1 {
                rows: vec![PublisherRefRow {
                    scope: expected_scope.clone(),
                    branch_ref: "refs/heads/main".to_string(),
                }],
            },
            was_missing: false,
        };
        let legacy = namespace_legacy_capture();
        let lanes = empty_test_lanes(
            crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Complete,
        );
        let duplicate = derive_publisher_pins(
            &source,
            &legacy,
            &BTreeMap::from([
                (
                    ProjectId::parse("project-a").unwrap(),
                    expected_scope.clone(),
                ),
                (
                    ProjectId::parse("project-b").unwrap(),
                    expected_scope.clone(),
                ),
            ]),
            &lanes,
        )
        .unwrap();
        assert!(duplicate.bound.is_empty());
        assert_eq!(duplicate.unbound.len(), 1);
        assert_eq!(
            duplicate.unbound[0].reason,
            UnboundPublisherPinReasonV1::DuplicateScopeOwners
        );
        assert_eq!(duplicate.unbound[0].candidate_project_ids.len(), 2);

        let ownerless = derive_publisher_pins(&source, &legacy, &BTreeMap::new(), &lanes).unwrap();
        assert!(ownerless.bound.is_empty());
        assert_eq!(ownerless.unbound.len(), 1);
        assert_eq!(
            ownerless.unbound[0].reason,
            UnboundPublisherPinReasonV1::OwnerlessScope
        );
        assert!(ownerless.unbound[0].candidate_project_ids.is_empty());
    }

    #[test]
    fn resolvable_pin_survives_an_incomplete_git_lane_for_typed_refusal() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        // Pin the initial branch: this fixture later creates `main`
        // explicitly, and a host git whose init default is already `main`
        // (Apple Git) would otherwise collide.
        run_git(&root, &["init", "-q", "--initial-branch", "master"]);
        write(
            &root.join(".bbox/config.toml"),
            b"[project]\nrepo_id = \"repo-one\"\n",
        );
        run_git(&root, &["add", ".bbox/config.toml"]);
        run_git(&root, &["commit", "-qm", "record authority"]);
        run_git(&root, &["branch", "main"]);

        let project_id = ProjectId::parse("project-a").unwrap();
        let expected_scope = PublishedScope::try_new("repo-one", ".").unwrap();
        let authorized_root = AuthorizedInventoryPath::new(&root).unwrap();
        let repository = open_stable_git_repository(&authorized_root.authority)
            .unwrap()
            .unwrap();
        let mut legacy = namespace_legacy_capture();
        legacy.observations[0].path_status = LegacyProjectPathStatusV1::Present;
        legacy.observations[0].committed_scope = Some(expected_scope.clone());
        legacy
            .published_scopes
            .insert(project_id.clone(), expected_scope.clone());
        legacy
            .project_roots
            .insert(project_id.clone(), authorized_root);
        legacy.repositories.insert(project_id.clone(), repository);
        let source = ExactDecodedSourceV1 {
            source: ExactSourceBytesV1::new(Vec::new()),
            value: PublisherRefInventoryV1 {
                rows: vec![PublisherRefRow {
                    scope: expected_scope,
                    branch_ref: "refs/heads/main".to_string(),
                }],
            },
            was_missing: false,
        };
        let captured = derive_publisher_pins(
            &source,
            &legacy,
            &legacy.published_scopes,
            &empty_test_lanes(
                crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Corrupt,
            ),
        )
        .unwrap();
        assert!(captured.unbound.is_empty());
        assert_eq!(captured.bound.len(), 1);
        assert!(captured.bound[0].resolved_commit.is_some());
        assert_eq!(
            captured.bound[0].resolved_scope.as_ref(),
            Some(&captured.bound[0].expected_scope)
        );
        assert!(
            captured.bound[0]
                .source_observation_ids
                .iter()
                .all(|observation_id| !observation_id.starts_with("git-"))
        );
    }

    #[test]
    fn unborn_repository_preserves_git_lane_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        run_git(&root, &["init", "-q"]);
        let authorized_root = AuthorizedInventoryPath::new(&root).unwrap();
        let checkout = observe_checkout(&authorized_root, None).unwrap();
        let project_id = ProjectId::parse("project-a").unwrap();
        let mut legacy = namespace_legacy_capture();
        legacy.observations[0].path_status = LegacyProjectPathStatusV1::Present;
        legacy.observations[0].committed_authority = None;
        legacy.observations[0].committed_scope = None;
        legacy
            .project_roots
            .insert(project_id.clone(), authorized_root);
        legacy.repositories.insert(
            project_id.clone(),
            checkout.repository.as_ref().unwrap().clone(),
        );
        let attachment = AttachmentCandidateObservationV1 {
            observation_id: "attachment-unborn".to_string(),
            attachment_id: AttachmentId::parse(format!("att_{}", "1".repeat(32))).unwrap(),
            project_id,
            checkout_observation_id: checkout.observation.observation_id.clone(),
            base_relpath: ".".to_string(),
            observed_scope: None,
        };
        let mut corpus = namespace_corpus_snapshot();
        corpus.index.commit_namespaces.clear();
        let mut vectors = namespace_vector_snapshot();
        vectors.commit_namespaces.clear();
        let publisher = ExactDecodedSourceV1 {
            source: ExactSourceBytesV1::new(Vec::new()),
            value: PublisherRefInventoryV1 { rows: Vec::new() },
            was_missing: true,
        };
        let (lane, namespaces, _) = capture_git_metadata_lane(
            &corpus,
            &vectors,
            &legacy,
            &publisher,
            &[checkout],
            &[attachment],
        )
        .unwrap();
        assert_eq!(
            lane.evidence.completeness,
            crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Complete
        );
        assert_eq!(lane.rows.len(), 1);
        assert!(lane.rows[0].common_directory_digest.is_some());
        assert!(lane.rows[0].full_first_commit.is_none());
        assert!(lane.rows[0].resolved_refs.is_empty());
        assert!(namespaces.is_empty());
    }

    #[test]
    fn namespace_join_detects_changed_and_omitted_owner_rows() {
        let legacy = namespace_legacy_capture();
        let corpus = namespace_corpus_snapshot();
        let vectors = namespace_vector_snapshot();
        let joined = capture_commit_namespaces(&corpus, &vectors, &legacy).unwrap();
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].commit_document_count, 2);
        assert_eq!(joined[0].vector_key_count, 2);
        assert!(matches!(
            joined[0].attribution,
            LegacyCommitNamespaceAttributionV1::Proved { .. }
        ));

        let mut changed_corpus = corpus.clone();
        changed_corpus.index.commit_namespaces[0].commit_document_count = 3;
        changed_corpus.index.commit_namespaces[0].commit_document_commitment_sha256 =
            "f".repeat(64);
        let changed = capture_commit_namespaces(&changed_corpus, &vectors, &legacy).unwrap();
        assert_ne!(changed, joined);

        let mut omitted_vector = vectors.clone();
        omitted_vector.commit_namespaces.clear();
        let omitted = capture_commit_namespaces(&corpus, &omitted_vector, &legacy).unwrap();
        assert_eq!(omitted.len(), 1);
        assert_eq!(omitted[0].vector_key_count, 0);
        assert_ne!(
            omitted[0].vector_key_set_sha256,
            joined[0].vector_key_set_sha256
        );

        let mut omitted_corpus = corpus;
        omitted_corpus.index.commit_namespaces.clear();
        omitted_vector.commit_namespaces.clear();
        assert!(
            capture_commit_namespaces(&omitted_corpus, &omitted_vector, &legacy)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unrepresented_git_cursor_fails_the_git_lane_closed() {
        use bbox_corpus_index::index::migration_inventory::GitCursorMigrationRowV1;

        let legacy = namespace_legacy_capture();
        let mut corpus = namespace_corpus_snapshot();
        corpus.git_cursors.row_count = 1;
        corpus.git_cursors.rows = vec![GitCursorMigrationRowV1 {
            project_id: "project-a".to_string(),
            last_ingested_sha: Some("1".repeat(40)),
        }];
        let vectors = namespace_vector_snapshot();
        let publisher = ExactDecodedSourceV1 {
            source: ExactSourceBytesV1::new(Vec::new()),
            value: PublisherRefInventoryV1 { rows: Vec::new() },
            was_missing: true,
        };
        let (lane, namespaces, runtime_common_dirs) =
            capture_git_metadata_lane(&corpus, &vectors, &legacy, &publisher, &[], &[]).unwrap();
        assert_eq!(
            lane.evidence.completeness,
            crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Corrupt
        );
        assert!(lane.rows.is_empty());
        assert!(namespaces.is_empty());
        assert!(runtime_common_dirs.is_empty());
        assert!(lane.evidence.owner_subsources.iter().any(|owner| {
            matches!(
                &owner.source_state,
                InventorySourceStateV1::Corrupt {
                    diagnostic_code,
                    ..
                } if diagnostic_code == "git_cursor_checkout_evidence_missing"
            )
        }));
    }

    #[test]
    fn attachment_discovery_is_stable_and_duplicate_roots_refuse() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let projects = root.join("projects.json");
        write(
            &projects,
            &serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "projects": [{
                    "project_id": "project-a",
                    "canonical_path": root,
                    "registered_at": "2026-01-01T00:00:00Z",
                    "is_git_repo": false
                }]
            }))
            .unwrap(),
        );
        let discovered =
            ProjectCatalogMigrationInventoryFacadeV1::discover_attachment_candidate_keys(
                ProjectCatalogAttachmentCandidateDiscoveryRequestV1 {
                    rehearsal_root: None,
                    legacy_project_store_path: projects.clone(),
                    checkout_roots: vec![root.clone()],
                },
            )
            .unwrap();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].base_relpath, ".");
        let second = ProjectCatalogMigrationInventoryFacadeV1::discover_attachment_candidate_keys(
            ProjectCatalogAttachmentCandidateDiscoveryRequestV1 {
                rehearsal_root: None,
                legacy_project_store_path: projects.clone(),
                checkout_roots: vec![root.clone()],
            },
        )
        .unwrap();
        assert_eq!(discovered, second);

        let error = ProjectCatalogMigrationInventoryFacadeV1::discover_attachment_candidate_keys(
            ProjectCatalogAttachmentCandidateDiscoveryRequestV1 {
                rehearsal_root: None,
                legacy_project_store_path: projects,
                checkout_roots: vec![root.clone(), root],
            },
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_inventory_adapter_input"
        );
    }
}
