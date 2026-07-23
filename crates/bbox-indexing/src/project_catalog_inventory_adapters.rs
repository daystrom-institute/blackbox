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

use bbox_code_source::{
    GenerationState, source_selector, validate_collected_materialization_selector,
};
use bbox_code_source_store::{
    ActivationRecord, ActivationRecordV2, CodeSourceStorePaths, MigrationLegacyAnchorEvidenceV1,
    MigrationLegacyGenerationEvidenceV1, MigrationLegacyInventoryV1, StoreLimits,
    StoredGenerationV2, decode_migration_effective_source_manifest_v1,
    encode_activation_v2_for_migration, encode_stored_generation_v2_for_migration,
    verify_generation_manifest_for_migration,
};
use bbox_corpus_core::git::{
    read_verified_committed_file_bytes_optional_bounded, verify_commit_oid_with_alternate,
};
use bbox_corpus_core::identity::{PublishedScope, resolve_recorded_repo_id};
use bbox_corpus_core::json_store::NofollowDirectory;
use bbox_corpus_core::project_catalog::{
    AttachmentId, LegacyProjectStoreV1, MAX_PROJECT_CATALOG_BYTES, MAX_PROJECT_CATALOG_ENTRIES,
    ProjectId, RecordedRepoAuthority, decode_legacy_project_store,
};
use sha2::{Digest, Sha256};

use crate::project_catalog_inventory::{
    AttachmentCandidateObservationV1, CheckoutMarkerStateV1, CheckoutObservationV1,
    CodeSourceObservationV1, CollectedGenerationObservationV1, CollectedGenerationRoleV1,
    EdgeWorkspaceObservationV1, GitMetadataObservationV1, ImmutableArtifactObservationV1,
    ImmutableCollectedDescriptorV1, ImmutableInventoryLaneCompletenessV1,
    ImmutableInventoryLaneEvidenceV1, ImmutableInventoryLaneKindV1, InventorySourceStateV1,
    InventoryTargetObservationV1, LegacyNamespaceClusterObservationV1, LegacyPathObservationV1,
    LegacyProjectObservationV1, LegacyProjectPathStatusV1, LegacyProjectRecordInventoryV1,
    MaterializedAliasObservationV1, MutableInventorySourceEvidenceV1, MutableInventorySourceKindV1,
    MutableInventorySourceLocatorV1, PROJECT_CATALOG_INVENTORY_VERSION_V1,
    ProjectScopedRefObservationV1, PublisherPinObservationV1, QuarantinedGenerationObservationV1,
    RepoGroupingProofV1, Sha256ValueV1, V1ProjectCatalogInventory, digest_path,
    mutable_source_row_set_hash,
};
use crate::publisher::{PublisherRefRow, decode_publisher_ref_source_v1};

const MAX_AUTHORIZED_PATH_BYTES: usize = 4_096;
const MAX_PUBLISHER_REF_SOURCE_BYTES: usize = 8 * 1024 * 1024;
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

pub type AdapterResult<T> = Result<T, InventoryAdapterError>;

#[derive(Clone, PartialEq, Eq)]
pub struct AuthorizedInventoryPath {
    path: PathBuf,
}

impl AuthorizedInventoryPath {
    pub fn new(path: impl AsRef<Path>) -> AdapterResult<Self> {
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
        Ok(Self { path })
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
}

impl fmt::Debug for AuthorizedInventoryPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthorizedInventoryPath(<redacted>)")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ExactSourceBytesV1 {
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

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn content_hash(&self) -> &Sha256ValueV1 {
        &self.content_hash
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizedFileObservationV1 {
    NotFound,
    Present(ExactSourceBytesV1),
    Invalid { diagnostic_code: String },
}

#[derive(Clone, PartialEq, Eq)]
pub struct ExactDecodedSourceV1<T> {
    source: ExactSourceBytesV1,
    value: T,
    was_missing: bool,
}

impl<T> ExactDecodedSourceV1<T> {
    pub fn source(&self) -> &ExactSourceBytesV1 {
        &self.source
    }

    pub fn value(&self) -> &T {
        &self.value
    }
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
pub enum DecodedSourceObservationV1<T> {
    NotFound,
    Valid(ExactDecodedSourceV1<T>),
    Invalid {
        source: Option<ExactSourceBytesV1>,
        diagnostic_code: String,
    },
}

impl<T> DecodedSourceObservationV1<T> {
    pub fn require_valid(self, label: &'static str) -> AdapterResult<ExactDecodedSourceV1<T>> {
        match self {
            Self::Valid(source) => Ok(source),
            Self::NotFound => Err(invalid_source(format!("{label}_not_found"))),
            Self::Invalid {
                diagnostic_code, ..
            } => Err(invalid_source(format!("{label}_{diagnostic_code}"))),
        }
    }
}

pub fn read_authorized_file(
    path: &AuthorizedInventoryPath,
    max_bytes: usize,
) -> AdapterResult<AuthorizedFileObservationV1> {
    match inspect_path(path.as_path()) {
        InspectedPath::Missing => return Ok(AuthorizedFileObservationV1::NotFound),
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
    let parent = path
        .as_path()
        .parent()
        .ok_or_else(|| invalid_input("authorized file has no parent"))?;
    let name = path
        .as_path()
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_input("authorized file has an invalid basename"))?;
    let directory = match NofollowDirectory::open_existing(parent) {
        Ok(Some(directory)) => directory,
        Ok(None) => return Ok(AuthorizedFileObservationV1::NotFound),
        Err(_) => return Ok(invalid_file("source_path_unreadable")),
    };
    let bytes = match directory.read_regular(name, max_bytes, "migration inventory source") {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(AuthorizedFileObservationV1::NotFound),
        Err(_) => return Ok(invalid_file("source_read_invalid")),
    };
    if directory.ensure_still_current().is_err() {
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

pub(crate) fn capture_legacy_projects_source(
    path: &AuthorizedInventoryPath,
) -> AdapterResult<DecodedSourceObservationV1<LegacyProjectStoreV1>> {
    Ok(decode_source(
        read_authorized_file(path, MAX_PROJECT_CATALOG_BYTES)?,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublisherRefInventoryV1 {
    pub rows: Vec<PublisherRefRow>,
}

pub(crate) fn capture_publisher_ref_source(
    path: &AuthorizedInventoryPath,
) -> AdapterResult<DecodedSourceObservationV1<PublisherRefInventoryV1>> {
    Ok(decode_source(
        read_authorized_file(path, MAX_PUBLISHER_REF_SOURCE_BYTES)?,
        |bytes| {
            decode_publisher_ref_source_v1(bytes)
                .map(|rows| PublisherRefInventoryV1 { rows })
                .map_err(|_| ())
        },
        "publisher_refs_invalid",
    ))
}

fn accept_missing_publisher_ref_source(
    observed: DecodedSourceObservationV1<PublisherRefInventoryV1>,
) -> AdapterResult<ExactDecodedSourceV1<PublisherRefInventoryV1>> {
    match observed {
        DecodedSourceObservationV1::NotFound => Ok(ExactDecodedSourceV1 {
            source: ExactSourceBytesV1::new(Vec::new()),
            value: PublisherRefInventoryV1 { rows: Vec::new() },
            was_missing: true,
        }),
        DecodedSourceObservationV1::Valid(source) => Ok(source),
        DecodedSourceObservationV1::Invalid { .. } => Err(invalid_source("publisher_refs_invalid")),
    }
}

#[derive(Debug, Clone)]
pub struct CommittedConfigSourceV1 {
    pub repository_root: AuthorizedInventoryPath,
    pub commit_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedAuthorityProbeV1 {
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
        .strip_prefix(source.repository_root.as_path())
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
    let verified = verify_commit_oid_with_alternate(
        source.repository_root.as_path(),
        &source.commit_oid,
        None,
    )
    .map_err(|_| invalid_source("committed_config_commit_invalid"))?;
    let bytes = read_verified_committed_file_bytes_optional_bounded(
        &verified,
        &repo_relative_path,
        MAX_COMMITTED_CONFIG_BYTES,
    )
    .map_err(|_| invalid_source("committed_config_read_invalid"))?;
    let locator = MutableInventorySourceLocatorV1::CommittedProjectConfig {
        project_id: project_id.clone(),
        commit_oid: verified.oid().to_string(),
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
pub struct LegacyProjectProbeInputV1 {
    pub project_id: ProjectId,
    pub authorized_canonical_path: AuthorizedInventoryPath,
    pub committed_config: Option<CommittedConfigSourceV1>,
}

#[derive(Debug, Clone)]
struct LegacyProjectsCaptureV1 {
    observations: Vec<LegacyProjectObservationV1>,
    source_evidence: Vec<MutableInventorySourceEvidenceV1>,
    published_scopes: BTreeMap<ProjectId, PublishedScope>,
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
        if let Some(scope) = authority_probe.published_scope {
            published_scopes.insert(project_id.clone(), scope);
        }
        source_evidence.push(authority_probe.source_evidence);
        observations.push(LegacyProjectObservationV1 {
            observation_id,
            record: LegacyProjectRecordInventoryV1::from(record.clone()),
            path_status,
            committed_authority,
        });
    }
    observations.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    source_evidence.sort_by(|left, right| left.source_id.cmp(&right.source_id));
    Ok(LegacyProjectsCaptureV1 {
        observations,
        source_evidence,
        published_scopes,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublisherPinBindingInputV1 {
    pub project_id: ProjectId,
    pub expected_scope: PublishedScope,
    pub candidate_attachment_ids: BTreeSet<AttachmentId>,
    pub resolved_commit: Option<String>,
    pub resolved_scope: Option<PublishedScope>,
    pub source_observation_ids: BTreeSet<String>,
}

fn bind_publisher_pins(
    source: &ExactDecodedSourceV1<PublisherRefInventoryV1>,
    bindings: Vec<PublisherPinBindingInputV1>,
) -> AdapterResult<Vec<PublisherPinObservationV1>> {
    let binding_count = bindings.len();
    let mut bindings = bindings
        .into_iter()
        .map(|binding| (binding.expected_scope.clone(), binding))
        .collect::<BTreeMap<_, _>>();
    if binding_count != bindings.len() || bindings.len() != source.value.rows.len() {
        return Err(invalid_input("publisher pin bindings are not exact"));
    }
    let publisher_source_id = stable_observation_id_v1(
        "publisher-ref-source",
        &[source.source.content_hash.as_str().as_bytes()],
    )?;
    let mut rows = Vec::new();
    for publisher in &source.value.rows {
        let binding = bindings
            .remove(&publisher.scope)
            .ok_or_else(|| invalid_input("publisher pin binding is missing"))?;
        let mut source_observation_ids = binding.source_observation_ids;
        source_observation_ids.insert(publisher_source_id.clone());
        rows.push(PublisherPinObservationV1 {
            observation_id: stable_observation_id_v1(
                "publisher-pin",
                &[
                    binding.project_id.as_str().as_bytes(),
                    publisher.scope.repo_id().as_bytes(),
                    publisher.scope.bbox_root_relpath().as_bytes(),
                    publisher.branch_ref.as_bytes(),
                ],
            )?,
            project_id: binding.project_id,
            expected_scope: publisher.scope.clone(),
            full_ref: publisher.branch_ref.clone(),
            candidate_attachment_ids: binding.candidate_attachment_ids,
            resolved_commit: binding.resolved_commit,
            resolved_scope: binding.resolved_scope,
            source_observation_ids,
        });
    }
    rows.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    Ok(rows)
}

#[derive(Debug, Clone)]
struct CodeSourceInventorySnapshotV1 {
    anchor_source: ExactSourceBytesV1,
    anchor_missing: bool,
    inventory: MigrationLegacyInventoryV1,
}

fn capture_code_source_inventory(
    paths: &CodeSourceStorePaths,
    limits: &StoreLimits,
    catalog_scopes: &BTreeSet<PublishedScope>,
) -> AdapterResult<CodeSourceInventorySnapshotV1> {
    let guard = paths
        .lock_migration_inventory()
        .map_err(|_| invalid_source("code_source_lock_invalid"))?;
    let inventory = guard
        .snapshot_legacy_v1_for_scopes(limits, catalog_scopes)
        .map_err(|_| invalid_source("code_source_inventory_invalid"))?;
    inventory
        .validate_evidence()
        .map_err(|_| invalid_source("code_source_inventory_evidence_invalid"))?;
    let (anchor_source, anchor_missing) = match &inventory.anchor {
        MigrationLegacyAnchorEvidenceV1::Missing => (ExactSourceBytesV1::new(Vec::new()), true),
        MigrationLegacyAnchorEvidenceV1::Present { bytes, .. } => {
            (ExactSourceBytesV1::new(bytes.clone()), false)
        }
    };
    Ok(CodeSourceInventorySnapshotV1 {
        anchor_source,
        anchor_missing,
        inventory,
    })
}

#[derive(Debug, Clone)]
struct CodeSourceCaptureV1 {
    observation: CodeSourceObservationV1,
    source_evidence: Vec<MutableInventorySourceEvidenceV1>,
}

fn observe_code_sources(
    snapshot: &CodeSourceInventorySnapshotV1,
    project_scopes: &BTreeMap<ProjectId, PublishedScope>,
    missing_checkout_projects: &BTreeSet<ProjectId>,
    limits: &StoreLimits,
) -> AdapterResult<Vec<CodeSourceCaptureV1>> {
    let generations_by_id = snapshot
        .inventory
        .generations
        .iter()
        .map(|row| (row.generation_id.clone(), row))
        .collect::<BTreeMap<_, _>>();
    let mut owner_by_generation = BTreeMap::<String, ProjectId>::new();
    if let MigrationLegacyAnchorEvidenceV1::Present { bytes, .. } = &snapshot.inventory.anchor {
        let manifest = decode_migration_effective_source_manifest_v1(bytes)
            .map_err(|_| invalid_source("effective_source_manifest_invalid"))?;
        for selection in manifest.selections {
            insert_generation_owner(
                &mut owner_by_generation,
                &selection.generation_id,
                &selection.project_id,
            )?;
        }
    }
    for activation in &snapshot.inventory.activations {
        insert_generation_owner(
            &mut owner_by_generation,
            &activation.record.generation_id,
            &activation.project_id,
        )?;
    }
    for collision in &snapshot.inventory.collision_pending {
        insert_generation_owner(
            &mut owner_by_generation,
            &collision.record.generation_id,
            &collision.project_id,
        )?;
    }
    for generation in &snapshot.inventory.generations {
        if owner_by_generation.contains_key(&generation.generation_id) {
            continue;
        }
        let candidates = project_scopes
            .iter()
            .filter(|(_, scope)| **scope == generation.published_scope)
            .map(|(project_id, _)| project_id)
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return Err(invalid_source("retained_generation_owner_ambiguous"));
        }
        owner_by_generation.insert(generation.generation_id.clone(), candidates[0].clone());
    }
    let quarantined = snapshot
        .inventory
        .collision_pending
        .iter()
        .map(|row| (row.project_id.clone(), row.record.generation_id.clone()))
        .collect::<BTreeSet<_>>();
    let mut grouped = BTreeMap::<ProjectId, Vec<&MigrationLegacyGenerationEvidenceV1>>::new();
    for generation in &snapshot.inventory.generations {
        let project_id = owner_by_generation
            .get(&generation.generation_id)
            .ok_or_else(|| invalid_source("protected_generation_owner_missing"))?;
        grouped
            .entry(project_id.clone())
            .or_default()
            .push(generation);
    }
    for activation in &snapshot.inventory.activations {
        grouped.entry(activation.project_id.clone()).or_default();
    }
    let mut captures = Vec::new();
    for (project_id, mut generation_rows) in grouped {
        generation_rows.sort_by(|left, right| left.generation_id.cmp(&right.generation_id));
        let activation = snapshot
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
                if quarantined.contains(&(project_id.clone(), generation.generation_id.clone())) {
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
                describe_generation(generation, limits)?;
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
            if quarantined.contains(&(project_id.clone(), generation.generation_id.clone())) {
                quarantine.push(QuarantinedGenerationObservationV1 {
                    observation_id,
                    project_id: project_id.clone(),
                    generation_id: generation.generation_id.clone(),
                    descriptor,
                    manifest,
                    manifest_hash: Sha256ValueV1::parse(generation.manifest_sha256.clone())
                        .map_err(|_| invalid_source("generation_manifest_hash_invalid"))?,
                    planned_metadata_v2_hash,
                });
            } else {
                let active = active_generation_id == Some(generation.generation_id.as_str());
                let literal_selector = if active {
                    activation
                        .expect("active generation has activation")
                        .record
                        .selector
                        .clone()
                } else {
                    source_selector(project_id.as_str(), &generation.generation_id)
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
                    activation_scope: Some(generation.published_scope.clone()),
                    descriptor,
                    manifest,
                    selector_hash: Sha256ValueV1::digest(literal_selector.as_bytes()),
                    checkout_missing: missing_checkout_projects.contains(&project_id),
                    planned_metadata_v2_hash,
                });
            }
        }
        let planned_activation_v2_hash = match activation {
            Some(activation) if active_generation_id.is_some() => {
                let generation = generations_by_id
                    .get(&activation.record.generation_id)
                    .ok_or_else(|| invalid_source("activation_generation_missing"))?;
                if quarantined.contains(&(project_id.clone(), generation.generation_id.clone())) {
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
    Ok(captures)
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
    root_source_evidence: MutableInventorySourceEvidenceV1,
    marker_source_evidence: MutableInventorySourceEvidenceV1,
}

fn observe_checkout(root: &AuthorizedInventoryPath) -> AdapterResult<CheckoutCaptureV1> {
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
    let observation_id = stable_observation_id_v1("checkout", &[root_digest.as_str().as_bytes()])?;
    let marker_source_id =
        stable_observation_id_v1("checkout-marker-source", &[root_digest.as_str().as_bytes()])?;
    let row_ids = BTreeSet::from([observation_id.clone()]);
    Ok(CheckoutCaptureV1 {
        observation: CheckoutObservationV1 {
            observation_id,
            canonical_checkout_root: literal_root.to_string(),
            canonical_root_digest: root_digest.clone(),
            marker_state: marker_state(&marker),
        },
        runtime_root: root.clone(),
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

#[derive(Clone, PartialEq, Eq)]
pub struct ImmutableLaneCaptureV1<T> {
    pub evidence: ImmutableInventoryLaneEvidenceV1,
    pub rows: Vec<T>,
}

impl<T> ImmutableLaneCaptureV1<T> {
    pub fn complete(
        lane_kind: ImmutableInventoryLaneKindV1,
        source_id: String,
        fingerprint: Sha256ValueV1,
        content_hash: Sha256ValueV1,
        byte_len: u64,
        rows: Vec<T>,
    ) -> AdapterResult<Self> {
        if rows.len() > MAX_PROJECT_CATALOG_ENTRIES {
            return Err(invalid_input("immutable lane row count exceeds limit"));
        }
        Ok(Self {
            evidence: ImmutableInventoryLaneEvidenceV1 {
                lane_kind,
                source_id,
                source_state: InventorySourceStateV1::Present {
                    fingerprint,
                    content_hash,
                    byte_len,
                },
                completeness: ImmutableInventoryLaneCompletenessV1::Complete,
                row_count: rows.len() as u64,
            },
            rows,
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ImmutableInventoryLanesV1 {
    pub project_scoped_refs: ImmutableLaneCaptureV1<ProjectScopedRefObservationV1>,
    pub edge_workspaces: ImmutableLaneCaptureV1<EdgeWorkspaceObservationV1>,
    pub git_metadata: ImmutableLaneCaptureV1<GitMetadataObservationV1>,
    pub checkouts: ImmutableLaneCaptureV1<CheckoutObservationV1>,
    pub attachment_candidates: ImmutableLaneCaptureV1<AttachmentCandidateObservationV1>,
    pub inventory_targets: ImmutableLaneCaptureV1<InventoryTargetObservationV1>,
    pub materialized_aliases: ImmutableLaneCaptureV1<MaterializedAliasObservationV1>,
    pub legacy_path_observations: ImmutableLaneCaptureV1<LegacyPathObservationV1>,
    pub repo_grouping_proofs: ImmutableLaneCaptureV1<RepoGroupingProofV1>,
    pub legacy_namespace_clusters: ImmutableLaneCaptureV1<LegacyNamespaceClusterObservationV1>,
}

#[derive(Clone)]
pub struct ProjectCatalogMigrationInventoryRequestV1 {
    pub legacy_project_store_path: AuthorizedInventoryPath,
    pub publisher_ref_store_path: AuthorizedInventoryPath,
    pub code_source_store_paths: CodeSourceStorePaths,
    pub code_source_limits: StoreLimits,
    pub legacy_project_probes: Vec<LegacyProjectProbeInputV1>,
    pub publisher_bindings: Vec<PublisherPinBindingInputV1>,
    pub checkout_roots: Vec<AuthorizedInventoryPath>,
    pub immutable_lanes: ImmutableInventoryLanesV1,
}

#[derive(Debug, Clone)]
pub struct ProjectCatalogMigrationInventoryResultV1 {
    pub inventory: V1ProjectCatalogInventory,
    /// Host-local authorities are retained outside canonical source locators.
    pub checkout_path_bindings: BTreeMap<String, AuthorizedInventoryPath>,
    pub code_source_canonical_sha256: Sha256ValueV1,
    pub code_source_generation_count: u64,
    pub code_source_generation_set_sha256: Sha256ValueV1,
}

pub struct ProjectCatalogMigrationInventoryFacadeV1;

impl ProjectCatalogMigrationInventoryFacadeV1 {
    pub fn capture(
        request: ProjectCatalogMigrationInventoryRequestV1,
    ) -> AdapterResult<ProjectCatalogMigrationInventoryResultV1> {
        let legacy_source = accept_missing_legacy_projects_source(capture_legacy_projects_source(
            &request.legacy_project_store_path,
        )?)?;
        let mut legacy = observe_legacy_projects(&legacy_source, request.legacy_project_probes)?;
        let catalog_scopes = legacy
            .published_scopes
            .values()
            .cloned()
            .collect::<BTreeSet<_>>();
        let code_snapshot = capture_code_source_inventory(
            &request.code_source_store_paths,
            &request.code_source_limits,
            &catalog_scopes,
        )?;
        let missing_checkout_projects = legacy
            .observations
            .iter()
            .filter(|row| row.path_status == LegacyProjectPathStatusV1::Missing)
            .map(|row| {
                ProjectId::parse(row.record.project_id.clone())
                    .expect("validated legacy project id remains valid")
            })
            .collect::<BTreeSet<_>>();
        let mut code_sources = observe_code_sources(
            &code_snapshot,
            &legacy.published_scopes,
            &missing_checkout_projects,
            &request.code_source_limits,
        )?;
        let publisher_source = accept_missing_publisher_ref_source(capture_publisher_ref_source(
            &request.publisher_ref_store_path,
        )?)?;
        let publisher_pins = bind_publisher_pins(&publisher_source, request.publisher_bindings)?;
        let mut checkout_captures = request
            .checkout_roots
            .iter()
            .map(observe_checkout)
            .collect::<AdapterResult<Vec<_>>>()?;
        checkout_captures.sort_by(|left, right| {
            left.observation
                .observation_id
                .cmp(&right.observation.observation_id)
        });
        let legacy_row_ids = legacy
            .observations
            .iter()
            .map(|row| row.observation_id.clone())
            .collect();
        let mut mutable_source_evidence = vec![exact_source_evidence(
            "legacy-project-store",
            MutableInventorySourceKindV1::LegacyProjectStore,
            MutableInventorySourceLocatorV1::LegacyProjectStore,
            &legacy_source,
            legacy_row_ids,
        )];
        mutable_source_evidence.append(&mut legacy.source_evidence);
        let publisher_row_ids = publisher_pins
            .iter()
            .map(|row| row.observation_id.clone())
            .collect();
        mutable_source_evidence.push(exact_source_evidence(
            "publisher-ref-store",
            MutableInventorySourceKindV1::PublisherRefStore,
            MutableInventorySourceLocatorV1::PublisherRefStore,
            &publisher_source,
            publisher_row_ids,
        ));
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
        let checkout_observations = checkout_captures
            .iter()
            .map(|capture| capture.observation.clone())
            .collect::<Vec<_>>();
        let mut checkout_path_bindings = BTreeMap::new();
        for capture in checkout_captures {
            checkout_path_bindings.insert(
                capture.observation.observation_id.clone(),
                capture.runtime_root,
            );
            mutable_source_evidence.push(capture.root_source_evidence);
            mutable_source_evidence.push(capture.marker_source_evidence);
        }
        mutable_source_evidence.sort_by(|left, right| left.source_id.cmp(&right.source_id));
        let mut lanes = request.immutable_lanes;
        if lanes.checkouts.evidence.completeness != ImmutableInventoryLaneCompletenessV1::Complete
            || !matches!(
                &lanes.checkouts.evidence.source_state,
                InventorySourceStateV1::Present { .. }
            )
        {
            return Err(invalid_input(
                "runtime checkout composition requires a complete checkout lane",
            ));
        }
        lanes.checkouts.rows = checkout_observations;
        lanes.checkouts.evidence.row_count = lanes.checkouts.rows.len() as u64;
        sort_lane_rows(&mut lanes);
        validate_lane_kinds(&lanes)?;
        let code_source_canonical_sha256 =
            Sha256ValueV1::parse(code_snapshot.inventory.canonical_sha256.clone())
                .map_err(|_| invalid_source("code_source_canonical_hash_invalid"))?;
        let code_source_generation_set_sha256 =
            Sha256ValueV1::parse(code_snapshot.inventory.generation_set_sha256.clone())
                .map_err(|_| invalid_source("code_source_generation_set_hash_invalid"))?;
        let inventory = V1ProjectCatalogInventory {
            version: PROJECT_CATALOG_INVENTORY_VERSION_V1,
            source_store_hash: legacy_source.source.content_hash.clone(),
            source_store_bytes: legacy_source.source.bytes,
            publisher_ref_source_hash: publisher_source.source.content_hash.clone(),
            publisher_ref_source_bytes: publisher_source.source.bytes,
            code_source_inventory_hash: code_source_canonical_sha256.clone(),
            code_source_generation_count: code_snapshot.inventory.generation_count,
            code_source_generation_set_sha256: code_source_generation_set_sha256.clone(),
            mutable_source_evidence,
            immutable_lane_evidence: lane_evidence(&lanes),
            legacy_projects: legacy.observations,
            code_sources: code_sources
                .into_iter()
                .map(|capture| capture.observation)
                .collect(),
            publisher_pins,
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
        };
        inventory
            .validate()
            .map_err(|error| invalid_source(error.to_string()))?;
        Ok(ProjectCatalogMigrationInventoryResultV1 {
            code_source_canonical_sha256,
            code_source_generation_count: code_snapshot.inventory.generation_count,
            code_source_generation_set_sha256,
            inventory,
            checkout_path_bindings,
        })
    }
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

pub fn stable_observation_id_v1(kind: &str, parts: &[&[u8]]) -> AdapterResult<String> {
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
        dirty_fingerprint, generation_id, manifest_sha256,
    };
    use bbox_code_source_store::{
        CollisionRetirementLifecycleStateV1, CollisionRetirementLifecycleV1, StoredGeneration,
        encode_collision_retirement_pending_for_migration,
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

    fn empty_lane<T>(
        kind: ImmutableInventoryLaneKindV1,
        source_id: &str,
        rows: Vec<T>,
    ) -> ImmutableLaneCaptureV1<T> {
        ImmutableLaneCaptureV1::complete(
            kind,
            source_id.to_string(),
            Sha256ValueV1::digest(format!("{source_id}:fingerprint").as_bytes()),
            Sha256ValueV1::digest(format!("{source_id}:content").as_bytes()),
            0,
            rows,
        )
        .unwrap()
    }

    fn empty_lanes() -> ImmutableInventoryLanesV1 {
        ImmutableInventoryLanesV1 {
            project_scoped_refs: empty_lane(
                ImmutableInventoryLaneKindV1::ProjectScopedRefs,
                "lane-project-refs",
                Vec::new(),
            ),
            edge_workspaces: empty_lane(
                ImmutableInventoryLaneKindV1::EdgeWorkspaces,
                "lane-edge-workspaces",
                Vec::new(),
            ),
            git_metadata: empty_lane(
                ImmutableInventoryLaneKindV1::GitMetadata,
                "lane-git-metadata",
                Vec::new(),
            ),
            checkouts: empty_lane(
                ImmutableInventoryLaneKindV1::Checkouts,
                "lane-checkouts",
                Vec::new(),
            ),
            attachment_candidates: empty_lane(
                ImmutableInventoryLaneKindV1::AttachmentCandidates,
                "lane-attachments",
                Vec::new(),
            ),
            inventory_targets: empty_lane(
                ImmutableInventoryLaneKindV1::InventoryTargets,
                "lane-targets",
                Vec::new(),
            ),
            materialized_aliases: empty_lane(
                ImmutableInventoryLaneKindV1::MaterializedAliases,
                "lane-aliases",
                Vec::new(),
            ),
            legacy_path_observations: empty_lane(
                ImmutableInventoryLaneKindV1::LegacyPathObservations,
                "lane-legacy-paths",
                Vec::new(),
            ),
            repo_grouping_proofs: empty_lane(
                ImmutableInventoryLaneKindV1::RepoGroupingProofs,
                "lane-repo-proofs",
                Vec::new(),
            ),
            legacy_namespace_clusters: empty_lane(
                ImmutableInventoryLaneKindV1::LegacyNamespaceClusters,
                "lane-namespace-clusters",
                Vec::new(),
            ),
        }
    }

    #[test]
    fn publisher_adapter_uses_the_owner_codec() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("publisher-refs.json");
        write(&path, br#"{"version":1}"#);
        let captured = capture_publisher_ref_source(&AuthorizedInventoryPath::new(&path).unwrap())
            .unwrap()
            .require_valid("publisher")
            .unwrap();
        assert!(captured.value.rows.is_empty());

        write(&path, br#"{"version":1,"refs":[],"invented":true}"#);
        assert!(matches!(
            capture_publisher_ref_source(&AuthorizedInventoryPath::new(&path).unwrap()).unwrap(),
            DecodedSourceObservationV1::Invalid { .. }
        ));
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
        let probe = observe_committed_authority_probe(
            "committed-config-project-a",
            &project_id,
            &root,
            Some(&CommittedConfigSourceV1 {
                repository_root: root.clone(),
                commit_oid: commit.clone(),
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
    }

    #[test]
    fn quarantined_generation_keeps_exact_metadata_and_manifest_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let paths = CodeSourceStorePaths::new(root.join("code-sources")).unwrap();
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
        let generation_id = generation_id("host-a", &descriptor);
        let stored = StoredGeneration {
            version: 1,
            generation_id: generation_id.clone(),
            producer_id: "host-a".to_string(),
            ordinal: 1,
            descriptor,
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
            &paths.generation_metadata(&scope, &generation_id).unwrap(),
            &metadata,
        );
        write(
            &paths.generation_manifest(&scope, &generation_id).unwrap(),
            &manifest,
        );
        let collision = CollisionRetirementLifecycleV1 {
            version: 1,
            state: CollisionRetirementLifecycleStateV1::Pending,
            project_id: project_id.clone(),
            former_scope: scope.clone(),
            generation_id: generation_id.clone(),
            selector: format!(
                "{}:m0123456789abcdef",
                source_selector(project_id.as_str(), &generation_id)
            ),
            snapshot_id: format!("collected-{}", "e".repeat(32)),
            manifest_sha256: hex::encode(Sha256::digest(&manifest)),
            inventory_hash: "d".repeat(64),
            plan_hash: "f".repeat(64),
        };
        write(
            &paths.collision_retirement_pending(&project_id),
            &encode_collision_retirement_pending_for_migration(&collision).unwrap(),
        );
        let snapshot =
            capture_code_source_inventory(&paths, &StoreLimits::default(), &BTreeSet::new())
                .unwrap();
        let captures = observe_code_sources(
            &snapshot,
            &BTreeMap::new(),
            &BTreeSet::new(),
            &StoreLimits::default(),
        )
        .unwrap();
        let quarantined = &captures[0].observation.quarantine[0];
        assert_eq!(quarantined.generation_id, generation_id);
        assert!(matches!(
            quarantined.descriptor,
            ImmutableCollectedDescriptorV1::Valid { .. }
        ));
        assert!(matches!(
            quarantined.manifest,
            ImmutableArtifactObservationV1::Valid { .. }
        ));
        let bound = captures[0]
            .source_evidence
            .iter()
            .filter(|source| {
                matches!(
                    source.source_kind,
                    MutableInventorySourceKindV1::CodeSourceGenerationMetadata
                        | MutableInventorySourceKindV1::CodeSourceGenerationManifest
                )
            })
            .all(|source| {
                source
                    .row_observation_ids
                    .contains(&quarantined.observation_id)
            });
        assert!(bound);
    }

    #[test]
    fn checkout_root_fingerprint_ignores_directory_mtime_and_entry_order() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let authorized = AuthorizedInventoryPath::new(&root).unwrap();
        let first = observe_checkout(&authorized).unwrap();
        write(&root.join("temporary"), b"x");
        fs::remove_file(root.join("temporary")).unwrap();
        let second = observe_checkout(&authorized).unwrap();
        assert_eq!(
            first.root_source_evidence.state,
            second.root_source_evidence.state
        );
    }

    #[test]
    fn mutable_source_row_sets_reject_omission_and_substitution() {
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
        let result = ProjectCatalogMigrationInventoryFacadeV1::capture(
            ProjectCatalogMigrationInventoryRequestV1 {
                legacy_project_store_path: AuthorizedInventoryPath::new(&projects).unwrap(),
                publisher_ref_store_path: AuthorizedInventoryPath::new(
                    root.join("publisher-refs.json"),
                )
                .unwrap(),
                code_source_store_paths: CodeSourceStorePaths::new(root.join("code-sources"))
                    .unwrap(),
                code_source_limits: StoreLimits::default(),
                legacy_project_probes: vec![LegacyProjectProbeInputV1 {
                    project_id: ProjectId::parse("project-a").unwrap(),
                    authorized_canonical_path: AuthorizedInventoryPath::new(&root).unwrap(),
                    committed_config: None,
                }],
                publisher_bindings: Vec::new(),
                checkout_roots: Vec::new(),
                immutable_lanes: empty_lanes(),
            },
        )
        .unwrap();
        let mut omitted = result.inventory.clone();
        let legacy = omitted
            .mutable_source_evidence
            .iter_mut()
            .find(|source| source.source_kind == MutableInventorySourceKindV1::LegacyProjectStore)
            .unwrap();
        legacy.row_observation_ids.clear();
        legacy.row_set_sha256 = mutable_source_row_set_hash(&legacy.row_observation_ids);
        assert!(omitted.validate().is_err());

        let mut substituted = result.inventory;
        let legacy = substituted
            .mutable_source_evidence
            .iter_mut()
            .find(|source| source.source_kind == MutableInventorySourceKindV1::LegacyProjectStore)
            .unwrap();
        legacy.row_observation_ids = BTreeSet::from(["lane-project-refs".to_string()]);
        legacy.row_set_sha256 = mutable_source_row_set_hash(&legacy.row_observation_ids);
        assert!(substituted.validate().is_err());
    }
}
