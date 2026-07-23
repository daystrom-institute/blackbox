//! Side-effect-free live inventory adapters for project-catalog migration.
//!
//! Every filesystem read starts from an explicitly supplied, lexically
//! validated absolute path. Adapters preserve exact source bytes, classify
//! absence separately from invalid state, and never create directories,
//! canonicalize paths, consult process configuration, or repair a store.
//! Host paths remain confined to the private inventory types that require
//! them. Errors and debug output do not echo path bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use bbox_code_source::{generation_id, source_selector};
use bbox_code_source_store::{
    ActivationRecord, CodeSourceStorePaths, StoreLimits, StoredGeneration,
    decode_activation_v1_for_migration, decode_stored_generation_v1_for_migration,
    verify_generation_manifest_for_migration,
};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::NofollowDirectory;
use bbox_corpus_core::project_catalog::{
    AttachmentId, LegacyProjectStoreV1, MAX_PROJECT_CATALOG_BYTES, MAX_PROJECT_CATALOG_ENTRIES,
    ProjectId, RecordedRepoAuthority, decode_legacy_project_store,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project_catalog_inventory::{
    AttachmentCandidateObservationV1, CheckoutMarkerStateV1, CheckoutObservationV1,
    CodeSourceObservationV1, CollectedGenerationObservationV1, CollectedGenerationRoleV1,
    EdgeWorkspaceObservationV1, GitMetadataObservationV1, ImmutableArtifactObservationV1,
    ImmutableCollectedDescriptorV1, InventoryTargetObservationV1,
    LegacyNamespaceClusterObservationV1, LegacyPathObservationV1, LegacyProjectObservationV1,
    LegacyProjectPathStatusV1, LegacyProjectRecordInventoryV1, MaterializedAliasObservationV1,
    PROJECT_CATALOG_INVENTORY_VERSION_V1, ProjectScopedRefObservationV1, PublisherPinObservationV1,
    QuarantinedGenerationObservationV1, RepoGroupingProofV1, Sha256ValueV1,
    V1ProjectCatalogInventory, digest_path,
};

const MAX_AUTHORIZED_PATH_BYTES: usize = 4_096;
const MAX_PUBLISHER_REF_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CODE_SOURCE_ACTIVATION_BYTES: usize = 512 * 1024 * 1024;
const MAX_CODE_SOURCE_METADATA_BYTES: usize = 64 * 1024;
const MAX_COLLECTED_MANIFEST_BYTES: usize = 512 * 1024 * 1024;
const MAX_CHECKOUT_MARKER_BYTES: usize = 128;
const MAX_FULL_REF_BYTES: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryAdapterError {
    code: &'static str,
    detail: String,
}

impl InventoryAdapterError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: redact_detail(detail.into()),
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

fn redact_detail(detail: String) -> String {
    detail
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(384)
        .collect()
}

/// A caller-authorized absolute path.
///
/// Construction is lexical and side-effect-free. Debug output is deliberately
/// redacted because these paths are host-local migration inputs.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthorizedInventoryPath {
    path: PathBuf,
}

impl AuthorizedInventoryPath {
    pub fn new(path: impl AsRef<Path>) -> AdapterResult<Self> {
        let path = path.as_ref().to_path_buf();
        let path_bytes = path
            .to_str()
            .ok_or_else(|| invalid_input("authorized inventory path is not utf8"))?
            .len();
        if !path.is_absolute()
            || path.file_name().is_none()
            || path_bytes > MAX_AUTHORIZED_PATH_BYTES
            || path
                .components()
                .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        {
            return Err(invalid_input("authorized inventory path is unsafe"));
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
            return Err(invalid_input(
                "authorized relative inventory path is unsafe",
            ));
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
}

impl fmt::Debug for ExactSourceBytesV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExactSourceBytesV1")
            .field("byte_len", &self.bytes.len())
            .field("content_hash", &self.content_hash)
            .finish()
    }
}

impl ExactSourceBytesV1 {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            content_hash: Sha256ValueV1::digest(&bytes),
            bytes,
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
}

impl<T> fmt::Debug for ExactDecodedSourceV1<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExactDecodedSourceV1")
            .field("source", &self.source)
            .field("value", &"<redacted>")
            .finish()
    }
}

impl<T> ExactDecodedSourceV1<T> {
    pub fn source(&self) -> &ExactSourceBytesV1 {
        &self.source
    }

    pub fn value(&self) -> &T {
        &self.value
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum DecodedSourceObservationV1<T> {
    NotFound,
    Valid(ExactDecodedSourceV1<T>),
    Invalid {
        source: Option<ExactSourceBytesV1>,
        diagnostic_code: String,
    },
}

impl<T> fmt::Debug for DecodedSourceObservationV1<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("NotFound"),
            Self::Valid(source) => formatter.debug_tuple("Valid").field(source).finish(),
            Self::Invalid {
                source,
                diagnostic_code,
            } => formatter
                .debug_struct("Invalid")
                .field("source", source)
                .field("diagnostic_code", diagnostic_code)
                .finish(),
        }
    }
}

impl<T> DecodedSourceObservationV1<T> {
    pub fn require_valid(self, label: &'static str) -> AdapterResult<ExactDecodedSourceV1<T>> {
        match self {
            Self::Valid(source) => Ok(source),
            Self::NotFound => Err(InventoryAdapterError::new(
                "error.project_catalog_inventory_source_not_found",
                format!("{label} was not found"),
            )),
            Self::Invalid {
                diagnostic_code, ..
            } => Err(InventoryAdapterError::new(
                "error.project_catalog_inventory_source_invalid",
                format!("{label} is invalid: {diagnostic_code}"),
            )),
        }
    }
}

/// Read one explicitly authorized regular file without following symlinks.
///
/// Absence and invalid state are values. Invalid state includes unsafe path
/// components, non-regular files, oversized content, unreadable content, and a
/// path replaced during the read.
pub fn read_authorized_file(
    path: &AuthorizedInventoryPath,
    max_bytes: usize,
) -> AdapterResult<AuthorizedFileObservationV1> {
    match inspect_path(path.as_path()) {
        InspectedPath::Missing => return Ok(AuthorizedFileObservationV1::NotFound),
        InspectedPath::Symlinked => {
            return Ok(invalid_file_observation("source_path_symlinked"));
        }
        InspectedPath::Unreadable => {
            return Ok(invalid_file_observation("source_path_unreadable"));
        }
        InspectedPath::NonRegular => {
            return Ok(invalid_file_observation("source_path_non_regular"));
        }
        InspectedPath::Regular { len } if len > max_bytes as u64 => {
            return Ok(invalid_file_observation("source_byte_limit_exceeded"));
        }
        InspectedPath::Directory => {
            return Ok(invalid_file_observation("source_path_non_regular"));
        }
        InspectedPath::Regular { .. } => {}
    }
    let Some(parent) = path.as_path().parent() else {
        return Err(invalid_input("authorized file has no parent"));
    };
    let Some(name) = path.as_path().file_name().and_then(|value| value.to_str()) else {
        return Err(invalid_input("authorized file has an invalid basename"));
    };
    let directory = match NofollowDirectory::open_existing(parent) {
        Ok(Some(directory)) => directory,
        Ok(None) => return Ok(AuthorizedFileObservationV1::NotFound),
        Err(_) => return Ok(invalid_file_observation("source_path_unreadable")),
    };
    let bytes = match directory.read_regular(name, max_bytes, "inventory source") {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(AuthorizedFileObservationV1::NotFound),
        Err(_) => return Ok(invalid_file_observation("source_read_invalid")),
    };
    if directory.ensure_still_current().is_err() {
        return Ok(invalid_file_observation("source_path_changed"));
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
        let is_last = index + 1 == components.len();
        if !is_last && !metadata.is_dir() {
            return InspectedPath::NonRegular;
        }
        if is_last {
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

fn invalid_file_observation(code: &str) -> AuthorizedFileObservationV1 {
    AuthorizedFileObservationV1::Invalid {
        diagnostic_code: code.to_string(),
    }
}

pub fn capture_legacy_projects_source(
    path: &AuthorizedInventoryPath,
) -> AdapterResult<DecodedSourceObservationV1<LegacyProjectStoreV1>> {
    let source = read_authorized_file(path, MAX_PROJECT_CATALOG_BYTES)?;
    Ok(decode_source(
        source,
        |bytes| decode_legacy_project_store(bytes).map_err(|_| ()),
        "legacy_projects_invalid",
    ))
}

/// Explicitly materialize the bridge's documented missing-file default after
/// the caller has observed absence. The exact source byte image remains empty,
/// so apply can separately recheck that the authorized path is still absent.
pub fn accept_missing_legacy_projects_source(
    observed: DecodedSourceObservationV1<LegacyProjectStoreV1>,
) -> AdapterResult<ExactDecodedSourceV1<LegacyProjectStoreV1>> {
    match observed {
        DecodedSourceObservationV1::NotFound => Ok(ExactDecodedSourceV1 {
            source: ExactSourceBytesV1::new(Vec::new()),
            value: LegacyProjectStoreV1::default(),
        }),
        DecodedSourceObservationV1::Valid(_) => Err(invalid_input(
            "legacy project source exists and cannot use the missing default",
        )),
        DecodedSourceObservationV1::Invalid { .. } => {
            Err(invalid_source("legacy_projects_invalid_cannot_default"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherRefSourceRowV1 {
    pub scope: PublishedScope,
    pub branch_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherRefSourceV1 {
    pub version: u32,
    pub refs: Vec<PublisherRefSourceRowV1>,
}

impl PublisherRefSourceV1 {
    fn validate(&self) -> AdapterResult<()> {
        if self.version != 1 {
            return Err(invalid_source("publisher_refs_version_invalid"));
        }
        if self.refs.len() > MAX_PROJECT_CATALOG_ENTRIES {
            return Err(invalid_source("publisher_refs_count_exceeded"));
        }
        let mut scopes = BTreeSet::new();
        for row in &self.refs {
            row.scope
                .validate()
                .map_err(|_| invalid_source("publisher_ref_scope_invalid"))?;
            validate_full_publisher_ref(&row.branch_ref)?;
            if !scopes.insert(row.scope.clone()) {
                return Err(invalid_source("publisher_ref_scope_duplicate"));
            }
        }
        Ok(())
    }
}

pub fn capture_publisher_ref_source(
    path: &AuthorizedInventoryPath,
) -> AdapterResult<DecodedSourceObservationV1<PublisherRefSourceV1>> {
    let source = read_authorized_file(path, MAX_PUBLISHER_REF_SOURCE_BYTES)?;
    Ok(match source {
        AuthorizedFileObservationV1::NotFound => DecodedSourceObservationV1::NotFound,
        AuthorizedFileObservationV1::Invalid { diagnostic_code } => {
            DecodedSourceObservationV1::Invalid {
                source: None,
                diagnostic_code,
            }
        }
        AuthorizedFileObservationV1::Present(source) => {
            match serde_json::from_slice::<PublisherRefSourceV1>(&source.bytes) {
                Ok(value) if value.validate().is_ok() => {
                    DecodedSourceObservationV1::Valid(ExactDecodedSourceV1 { source, value })
                }
                _ => DecodedSourceObservationV1::Invalid {
                    source: Some(source),
                    diagnostic_code: "publisher_refs_invalid".to_string(),
                },
            }
        }
    })
}

/// Explicitly materialize the publisher store's documented missing-file
/// default after the caller has observed absence.
pub fn accept_missing_publisher_ref_source(
    observed: DecodedSourceObservationV1<PublisherRefSourceV1>,
) -> AdapterResult<ExactDecodedSourceV1<PublisherRefSourceV1>> {
    match observed {
        DecodedSourceObservationV1::NotFound => Ok(ExactDecodedSourceV1 {
            source: ExactSourceBytesV1::new(Vec::new()),
            value: PublisherRefSourceV1 {
                version: 1,
                refs: Vec::new(),
            },
        }),
        DecodedSourceObservationV1::Valid(_) => Err(invalid_input(
            "publisher ref source exists and cannot use the missing default",
        )),
        DecodedSourceObservationV1::Invalid { .. } => {
            Err(invalid_source("publisher_refs_invalid_cannot_default"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyProjectProbeInputV1 {
    pub project_id: ProjectId,
    pub authorized_canonical_path: AuthorizedInventoryPath,
    pub committed_authority: Option<RecordedRepoAuthority>,
}

pub fn observe_legacy_projects(
    source: &ExactDecodedSourceV1<LegacyProjectStoreV1>,
    probes: Vec<LegacyProjectProbeInputV1>,
) -> AdapterResult<Vec<LegacyProjectObservationV1>> {
    let probe_count = probes.len();
    let mut probes = probes
        .into_iter()
        .map(|probe| (probe.project_id.clone(), probe))
        .collect::<BTreeMap<_, _>>();
    if probes.len() != probe_count || probes.len() != source.value.projects.len() {
        return Err(invalid_input("legacy project probes are not exact"));
    }
    let mut observations = Vec::with_capacity(source.value.projects.len());
    for record in &source.value.projects {
        let project_id = ProjectId::parse(record.project_id.clone())
            .map_err(|_| invalid_source("legacy_project_id_invalid"))?;
        let probe = probes
            .remove(&project_id)
            .ok_or_else(|| invalid_input("legacy project probe is missing"))?;
        if probe.authorized_canonical_path.as_path() != Path::new(&record.canonical_path) {
            return Err(invalid_input(
                "legacy project probe does not authorize the recorded path",
            ));
        }
        let path_status = match inspect_path(probe.authorized_canonical_path.as_path()) {
            InspectedPath::Missing => LegacyProjectPathStatusV1::Missing,
            InspectedPath::Directory => LegacyProjectPathStatusV1::Present,
            InspectedPath::Symlinked
            | InspectedPath::Unreadable
            | InspectedPath::NonRegular
            | InspectedPath::Regular { .. } => {
                return Err(InventoryAdapterError::new(
                    "error.project_catalog_inventory_project_path_invalid",
                    "authorized legacy project path is invalid",
                ));
            }
        };
        let observation_id =
            stable_observation_id_v1("legacy-project", &[project_id.as_str().as_bytes()])?;
        let committed_authority = probe.committed_authority.map(|authority| {
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
        observations.push(LegacyProjectObservationV1 {
            observation_id,
            record: LegacyProjectRecordInventoryV1::from(record.clone()),
            path_status,
            committed_authority,
        });
    }
    if !probes.is_empty() {
        return Err(invalid_input(
            "legacy project probes contain unknown projects",
        ));
    }
    observations.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    Ok(observations)
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

pub fn bind_publisher_pins(
    source: &ExactDecodedSourceV1<PublisherRefSourceV1>,
    bindings: Vec<PublisherPinBindingInputV1>,
) -> AdapterResult<Vec<PublisherPinObservationV1>> {
    let binding_count = bindings.len();
    let mut bindings = bindings
        .into_iter()
        .map(|binding| (binding.expected_scope.clone(), binding))
        .collect::<BTreeMap<_, _>>();
    if bindings.len() != binding_count || bindings.len() != source.value.refs.len() {
        return Err(invalid_input("publisher pin bindings are not exact"));
    }
    let mut observations = Vec::with_capacity(source.value.refs.len());
    let source_observation_id = stable_observation_id_v1(
        "publisher-ref-source",
        &[source.source.content_hash.as_str().as_bytes()],
    )?;
    for row in &source.value.refs {
        let binding = bindings
            .remove(&row.scope)
            .ok_or_else(|| invalid_input("publisher pin binding is missing"))?;
        let mut source_observation_ids = binding.source_observation_ids;
        source_observation_ids.insert(source_observation_id.clone());
        if source_observation_ids
            .iter()
            .any(|observation_id| !valid_stable_id(observation_id))
        {
            return Err(invalid_input("publisher source observation id is invalid"));
        }
        let scope_bytes =
            serde_json::to_vec(&row.scope).map_err(|_| invalid_source("publisher_scope_encode"))?;
        let observation_id = stable_observation_id_v1(
            "publisher-pin",
            &[
                binding.project_id.as_str().as_bytes(),
                &scope_bytes,
                row.branch_ref.as_bytes(),
            ],
        )?;
        observations.push(PublisherPinObservationV1 {
            observation_id,
            project_id: binding.project_id,
            expected_scope: row.scope.clone(),
            full_ref: row.branch_ref.clone(),
            candidate_attachment_ids: binding.candidate_attachment_ids,
            resolved_commit: binding.resolved_commit,
            resolved_scope: binding.resolved_scope,
            source_observation_ids,
        });
    }
    if !bindings.is_empty() {
        return Err(invalid_input("publisher bindings contain unknown scopes"));
    }
    observations.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    Ok(observations)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeSourceGenerationReadInputV1 {
    pub role: CollectedGenerationRoleV1,
    pub generation_id: String,
    pub published_scope: PublishedScope,
    pub activation_scope: Option<PublishedScope>,
    pub literal_selector: String,
    pub checkout_missing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeSourceReadInputV1 {
    pub project_id: ProjectId,
    pub generations: Vec<CodeSourceGenerationReadInputV1>,
    pub quarantine: Vec<QuarantinedGenerationObservationV1>,
}

#[derive(Debug, Clone)]
pub struct CodeSourceGenerationCaptureV1 {
    pub observation: CollectedGenerationObservationV1,
    pub literal_selector: String,
    pub selector_matches_generation: bool,
    pub metadata_source: DecodedSourceObservationV1<StoredGeneration>,
    pub manifest_source: AuthorizedFileObservationV1,
}

#[derive(Debug, Clone)]
pub struct CodeSourceCaptureV1 {
    pub observation: CodeSourceObservationV1,
    pub activation_source: DecodedSourceObservationV1<ActivationRecord>,
    pub generations: Vec<CodeSourceGenerationCaptureV1>,
}

pub fn observe_code_source(
    paths: &CodeSourceStorePaths,
    input: CodeSourceReadInputV1,
    limits: &StoreLimits,
) -> AdapterResult<CodeSourceCaptureV1> {
    let activation_path = AuthorizedInventoryPath::new(paths.activation(&input.project_id))?;
    let activation_source = decode_source(
        read_authorized_file(&activation_path, MAX_CODE_SOURCE_ACTIVATION_BYTES)?,
        |bytes| decode_activation_v1_for_migration(bytes).map_err(|_| ()),
        "code_source_activation_invalid",
    );
    let active_generation_count = input
        .generations
        .iter()
        .filter(|row| row.role == CollectedGenerationRoleV1::Active)
        .count();
    if active_generation_count > 1 {
        return Err(invalid_input(
            "code-source input contains multiple active generations",
        ));
    }
    let mut seen_generations = BTreeSet::new();
    let mut captures = Vec::with_capacity(input.generations.len());
    for generation in input.generations {
        if !seen_generations.insert(generation.generation_id.clone()) {
            return Err(invalid_input("code-source input repeats a generation id"));
        }
        generation
            .published_scope
            .validate()
            .map_err(|_| invalid_input("code-source scope is invalid"))?;
        if let Some(scope) = &generation.activation_scope {
            scope
                .validate()
                .map_err(|_| invalid_input("activation scope is invalid"))?;
        }
        validate_literal_selector(&generation.literal_selector)?;

        let metadata_path = AuthorizedInventoryPath::new(
            paths
                .generation_metadata(&generation.published_scope, &generation.generation_id)
                .map_err(|_| invalid_input("code-source metadata path input is invalid"))?,
        )?;
        let metadata_source = decode_source(
            read_authorized_file(&metadata_path, MAX_CODE_SOURCE_METADATA_BYTES)?,
            |bytes| decode_stored_generation_v1_for_migration(bytes).map_err(|_| ()),
            "stored_generation_invalid",
        );
        let manifest_path = AuthorizedInventoryPath::new(
            paths
                .generation_manifest(&generation.published_scope, &generation.generation_id)
                .map_err(|_| invalid_input("code-source manifest path input is invalid"))?,
        )?;
        let manifest_source = read_authorized_file(&manifest_path, MAX_COLLECTED_MANIFEST_BYTES)?;

        let (descriptor, manifest) =
            describe_generation_sources(&metadata_source, &manifest_source, &generation, limits)?;
        let selector_matches_generation = generation.literal_selector
            == source_selector(input.project_id.as_str(), &generation.generation_id);
        if !selector_matches_generation {
            return Err(InventoryAdapterError::new(
                "error.project_catalog_inventory_code_selector_invalid",
                "literal code-source selector does not match its project and generation",
            ));
        }
        let activation_agrees = match &activation_source {
            DecodedSourceObservationV1::Valid(source) => {
                source.value.project_id == input.project_id.as_str()
                    && source.value.generation_id == generation.generation_id
                    && source.value.selector == generation.literal_selector
            }
            DecodedSourceObservationV1::NotFound | DecodedSourceObservationV1::Invalid { .. } => {
                false
            }
        };
        let activation_scope =
            if generation.role == CollectedGenerationRoleV1::Active && !activation_agrees {
                None
            } else {
                generation.activation_scope.clone()
            };
        let observation_id = stable_observation_id_v1(
            "collected-generation",
            &[
                input.project_id.as_str().as_bytes(),
                generation.generation_id.as_bytes(),
            ],
        )?;
        captures.push(CodeSourceGenerationCaptureV1 {
            observation: CollectedGenerationObservationV1 {
                observation_id,
                project_id: input.project_id.clone(),
                role: generation.role,
                generation_id: generation.generation_id,
                activation_scope,
                descriptor,
                manifest,
                selector_hash: Sha256ValueV1::digest(generation.literal_selector.as_bytes()),
                checkout_missing: generation.checkout_missing,
            },
            literal_selector: generation.literal_selector,
            selector_matches_generation,
            metadata_source,
            manifest_source,
        });
    }
    captures.sort_by(|left, right| {
        left.observation
            .observation_id
            .cmp(&right.observation.observation_id)
    });
    let mut quarantine = input.quarantine;
    quarantine.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    let observation = CodeSourceObservationV1 {
        observation_id: stable_observation_id_v1(
            "code-source",
            &[input.project_id.as_str().as_bytes()],
        )?,
        project_id: input.project_id,
        generations: captures
            .iter()
            .map(|capture| capture.observation.clone())
            .collect(),
        quarantine,
    };
    Ok(CodeSourceCaptureV1 {
        observation,
        activation_source,
        generations: captures,
    })
}

fn describe_generation_sources(
    metadata: &DecodedSourceObservationV1<StoredGeneration>,
    manifest: &AuthorizedFileObservationV1,
    input: &CodeSourceGenerationReadInputV1,
    limits: &StoreLimits,
) -> AdapterResult<(
    ImmutableCollectedDescriptorV1,
    ImmutableArtifactObservationV1,
)> {
    let stored = match metadata {
        DecodedSourceObservationV1::NotFound => {
            return Ok((
                ImmutableCollectedDescriptorV1::Missing,
                artifact_without_descriptor(manifest),
            ));
        }
        DecodedSourceObservationV1::Invalid { .. } => {
            return Ok((
                ImmutableCollectedDescriptorV1::Corrupt {
                    diagnostic_code: "stored_generation_invalid".to_string(),
                },
                artifact_without_descriptor(manifest),
            ));
        }
        DecodedSourceObservationV1::Valid(source) => &source.value,
    };
    if stored.generation_id != input.generation_id
        || stored.descriptor.scope != input.published_scope
        || generation_id(&stored.producer_id, &stored.descriptor) != input.generation_id
    {
        return Ok((
            ImmutableCollectedDescriptorV1::Corrupt {
                diagnostic_code: "stored_generation_identity_mismatch".to_string(),
            },
            artifact_without_descriptor(manifest),
        ));
    }
    let descriptor_bytes = serde_json::to_vec(&stored.descriptor)
        .map_err(|_| invalid_source("stored_generation_descriptor_encode"))?;
    let descriptor = ImmutableCollectedDescriptorV1::Valid {
        descriptor_hash: Sha256ValueV1::digest(&descriptor_bytes),
        published_scope: stored.descriptor.scope.clone(),
    };
    let manifest = match manifest {
        AuthorizedFileObservationV1::NotFound => ImmutableArtifactObservationV1::Missing,
        AuthorizedFileObservationV1::Invalid { .. } => ImmutableArtifactObservationV1::Corrupt {
            diagnostic_code: "collected_manifest_read_invalid".to_string(),
        },
        AuthorizedFileObservationV1::Present(source) => {
            match verify_generation_manifest_for_migration(
                &source.bytes,
                &stored.descriptor,
                &stored.producer_id,
                &stored.generation_id,
                limits,
            ) {
                Ok(_) => ImmutableArtifactObservationV1::Valid {
                    content_hash: source.content_hash.clone(),
                },
                Err(_) => ImmutableArtifactObservationV1::Corrupt {
                    diagnostic_code: "collected_manifest_invalid".to_string(),
                },
            }
        }
    };
    Ok((descriptor, manifest))
}

fn artifact_without_descriptor(
    manifest: &AuthorizedFileObservationV1,
) -> ImmutableArtifactObservationV1 {
    match manifest {
        AuthorizedFileObservationV1::NotFound => ImmutableArtifactObservationV1::Missing,
        AuthorizedFileObservationV1::Present(_) | AuthorizedFileObservationV1::Invalid { .. } => {
            ImmutableArtifactObservationV1::Corrupt {
                diagnostic_code: "collected_manifest_unverifiable".to_string(),
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct CheckoutCaptureV1 {
    pub observation: CheckoutObservationV1,
    pub marker_source: AuthorizedFileObservationV1,
}

pub fn observe_checkout(
    canonical_checkout_root: &AuthorizedInventoryPath,
) -> AdapterResult<CheckoutCaptureV1> {
    match inspect_path(canonical_checkout_root.as_path()) {
        InspectedPath::Directory => {}
        InspectedPath::Missing => {
            return Err(InventoryAdapterError::new(
                "error.project_catalog_inventory_checkout_not_found",
                "authorized checkout root was not found",
            ));
        }
        InspectedPath::Symlinked
        | InspectedPath::Unreadable
        | InspectedPath::NonRegular
        | InspectedPath::Regular { .. } => {
            return Err(InventoryAdapterError::new(
                "error.project_catalog_inventory_checkout_invalid",
                "authorized checkout root is invalid",
            ));
        }
    }
    let marker_path = canonical_checkout_root.join(".bbox/local/checkout-id")?;
    let marker_source = read_authorized_file(&marker_path, MAX_CHECKOUT_MARKER_BYTES)?;
    let marker_state = marker_state(&marker_source);
    let literal_root = canonical_checkout_root
        .as_path()
        .to_str()
        .ok_or_else(|| invalid_input("checkout root is not utf8"))?
        .to_string();
    let root_digest = digest_path(&literal_root);
    let observation_id = stable_observation_id_v1("checkout", &[root_digest.as_str().as_bytes()])?;
    Ok(CheckoutCaptureV1 {
        observation: CheckoutObservationV1 {
            observation_id,
            canonical_checkout_root: literal_root,
            canonical_root_digest: root_digest,
            marker_state,
        },
        marker_source,
    })
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
            } else if valid_checkout_id(value) {
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

/// Rows captured by caller-owned immutable snapshots.
///
/// These inputs exist for stores whose current live APIs cannot safely provide
/// a complete, side-effect-free enumeration. The adapter does not open those
/// stores or infer missing fields. Final inventory validation checks every row
/// against the captured project set.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct InjectedInventoryRowsV1 {
    pub project_scoped_refs: Vec<ProjectScopedRefObservationV1>,
    pub edge_workspaces: Vec<EdgeWorkspaceObservationV1>,
    pub git_metadata: Vec<GitMetadataObservationV1>,
    pub checkouts: Vec<CheckoutObservationV1>,
    pub attachment_candidates: Vec<AttachmentCandidateObservationV1>,
    pub inventory_targets: Vec<InventoryTargetObservationV1>,
    pub materialized_aliases: Vec<MaterializedAliasObservationV1>,
    pub legacy_path_observations: Vec<LegacyPathObservationV1>,
    pub repo_grouping_proofs: Vec<RepoGroupingProofV1>,
    pub legacy_namespace_clusters: Vec<LegacyNamespaceClusterObservationV1>,
}

pub struct LiveInventoryAssemblyV1 {
    pub legacy_source: ExactDecodedSourceV1<LegacyProjectStoreV1>,
    pub publisher_ref_source: ExactDecodedSourceV1<PublisherRefSourceV1>,
    pub legacy_projects: Vec<LegacyProjectObservationV1>,
    pub code_sources: Vec<CodeSourceCaptureV1>,
    pub publisher_pins: Vec<PublisherPinObservationV1>,
    pub injected: InjectedInventoryRowsV1,
}

impl LiveInventoryAssemblyV1 {
    pub fn build(mut self) -> AdapterResult<V1ProjectCatalogInventory> {
        self.legacy_projects
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.code_sources.sort_by(|left, right| {
            left.observation
                .observation_id
                .cmp(&right.observation.observation_id)
        });
        self.publisher_pins
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        sort_injected_rows(&mut self.injected);
        for capture in &self.code_sources {
            validate_code_source_capture_for_assembly(capture)?;
        }
        let inventory = V1ProjectCatalogInventory {
            version: PROJECT_CATALOG_INVENTORY_VERSION_V1,
            source_store_hash: self.legacy_source.source.content_hash,
            source_store_bytes: self.legacy_source.source.bytes,
            publisher_ref_source_hash: self.publisher_ref_source.source.content_hash,
            publisher_ref_source_bytes: self.publisher_ref_source.source.bytes,
            legacy_projects: self.legacy_projects,
            code_sources: self
                .code_sources
                .into_iter()
                .map(|capture| capture.observation)
                .collect(),
            publisher_pins: self.publisher_pins,
            project_scoped_refs: self.injected.project_scoped_refs,
            edge_workspaces: self.injected.edge_workspaces,
            git_metadata: self.injected.git_metadata,
            checkouts: self.injected.checkouts,
            attachment_candidates: self.injected.attachment_candidates,
            inventory_targets: self.injected.inventory_targets,
            materialized_aliases: self.injected.materialized_aliases,
            legacy_path_observations: self.injected.legacy_path_observations,
            repo_grouping_proofs: self.injected.repo_grouping_proofs,
            legacy_namespace_clusters: self.injected.legacy_namespace_clusters,
        };
        inventory.validate().map_err(|error| {
            InventoryAdapterError::new(
                "error.project_catalog_inventory_adapter_validation",
                error.to_string(),
            )
        })?;
        Ok(inventory)
    }
}

fn validate_code_source_capture_for_assembly(capture: &CodeSourceCaptureV1) -> AdapterResult<()> {
    let active_count = capture
        .observation
        .generations
        .iter()
        .filter(|generation| generation.role == CollectedGenerationRoleV1::Active)
        .count();
    match &capture.activation_source {
        DecodedSourceObservationV1::Invalid { .. } => Err(InventoryAdapterError::new(
            "error.project_catalog_inventory_code_activation_invalid",
            "code-source activation bytes are invalid",
        )),
        DecodedSourceObservationV1::Valid(_) if active_count != 1 => {
            Err(InventoryAdapterError::new(
                "error.project_catalog_inventory_code_activation_orphaned",
                "code-source activation has no unique active generation",
            ))
        }
        DecodedSourceObservationV1::NotFound | DecodedSourceObservationV1::Valid(_) => Ok(()),
    }
}

fn sort_injected_rows(rows: &mut InjectedInventoryRowsV1) {
    rows.project_scoped_refs
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    rows.edge_workspaces
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    rows.git_metadata
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    rows.checkouts
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    rows.attachment_candidates
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    rows.inventory_targets
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    rows.materialized_aliases
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    rows.legacy_path_observations
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    rows.repo_grouping_proofs
        .sort_by(|left, right| left.proof_id().cmp(right.proof_id()));
    rows.legacy_namespace_clusters
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
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
            Ok(value) => DecodedSourceObservationV1::Valid(ExactDecodedSourceV1 { source, value }),
            Err(()) => DecodedSourceObservationV1::Invalid {
                source: Some(source),
                diagnostic_code: invalid_code.to_string(),
            },
        },
    }
}

fn validate_full_publisher_ref(value: &str) -> AdapterResult<()> {
    let Some(relative) = value.strip_prefix("refs/heads/") else {
        return Err(invalid_source("publisher_ref_not_full_branch"));
    };
    if value.len() > MAX_FULL_REF_BYTES
        || relative.is_empty()
        || relative.starts_with('/')
        || relative.ends_with('/')
        || relative.contains("//")
        || relative.contains("..")
        || relative.contains("@{")
        || relative.contains('\\')
        || relative
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || relative
            .chars()
            .any(|ch| matches!(ch, '~' | '^' | ':' | '?' | '*' | '['))
        || relative.split('/').any(|component| {
            component.is_empty()
                || matches!(component, "." | "..")
                || component.starts_with('.')
                || component.ends_with('.')
                || component.ends_with(".lock")
        })
    {
        return Err(invalid_source("publisher_ref_invalid"));
    }
    Ok(())
}

fn validate_literal_selector(value: &str) -> AdapterResult<()> {
    if value.trim().is_empty()
        || value.len() > MAX_FULL_REF_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(invalid_input("literal selector is invalid"));
    }
    Ok(())
}

fn valid_checkout_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_stable_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn invalid_input(detail: impl Into<String>) -> InventoryAdapterError {
    InventoryAdapterError::new("error.project_catalog_inventory_adapter_input", detail)
}

fn invalid_source(code: &'static str) -> InventoryAdapterError {
    InventoryAdapterError::new(
        "error.project_catalog_inventory_adapter_source",
        code.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use bbox_code_source::{
        GenerationDescriptor, GenerationState, ManifestEntry, SCHEMA_VERSION,
        WALKER_POLICY_VERSION, dirty_fingerprint, manifest_sha256,
    };

    fn write_file(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn publisher_source(
        path: &Path,
        rows: &[(&PublishedScope, &str)],
    ) -> ExactDecodedSourceV1<PublisherRefSourceV1> {
        let value = PublisherRefSourceV1 {
            version: 1,
            refs: rows
                .iter()
                .map(|(scope, branch_ref)| PublisherRefSourceRowV1 {
                    scope: (*scope).clone(),
                    branch_ref: (*branch_ref).to_string(),
                })
                .collect(),
        };
        write_file(path, &serde_json::to_vec_pretty(&value).unwrap());
        capture_publisher_ref_source(&AuthorizedInventoryPath::new(path).unwrap())
            .unwrap()
            .require_valid("publisher refs")
            .unwrap()
    }

    #[test]
    fn exact_legacy_source_distinguishes_missing_invalid_and_valid() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("projects.json");
        let authorized = AuthorizedInventoryPath::new(&path).unwrap();
        assert_eq!(
            capture_legacy_projects_source(&authorized).unwrap(),
            DecodedSourceObservationV1::NotFound
        );
        let empty = accept_missing_legacy_projects_source(
            capture_legacy_projects_source(&authorized).unwrap(),
        )
        .unwrap();
        assert!(empty.source.bytes().is_empty());
        assert!(empty.value.projects.is_empty());

        write_file(&path, b"{invalid");
        let invalid = capture_legacy_projects_source(&authorized).unwrap();
        assert!(matches!(
            invalid,
            DecodedSourceObservationV1::Invalid {
                source: Some(_),
                ..
            }
        ));

        let raw = format!(
            r#"{{
  "version": 1,
  "projects": [{{
    "project_id": "project-a",
    "canonical_path": {},
    "registered_at": "2026-01-01T00:00:00Z",
    "is_git_repo": true,
    "legacy_extra": true
  }}],
  "legacy_store_extra": true
}}"#,
            serde_json::to_string(root.to_str().unwrap()).unwrap()
        )
        .into_bytes();
        write_file(&path, &raw);
        let valid = capture_legacy_projects_source(&authorized)
            .unwrap()
            .require_valid("legacy projects")
            .unwrap();
        assert_eq!(valid.source.bytes, raw);
        assert_eq!(valid.source.content_hash, Sha256ValueV1::digest(&raw));

        let debug = format!("{authorized:?}");
        assert!(!debug.contains(root.to_str().unwrap()));
    }

    #[test]
    fn legacy_path_probe_requires_exact_authorization_and_preserves_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source_path = root.join("projects.json");
        let raw = serde_json::json!({
            "version": 1,
            "projects": [{
                "project_id": "project-a",
                "canonical_path": root,
                "registered_at": "2026-01-01T00:00:00Z",
                "is_git_repo": true
            }]
        });
        write_file(&source_path, &serde_json::to_vec(&raw).unwrap());
        let source =
            capture_legacy_projects_source(&AuthorizedInventoryPath::new(&source_path).unwrap())
                .unwrap()
                .require_valid("legacy projects")
                .unwrap();
        let observations = observe_legacy_projects(
            &source,
            vec![LegacyProjectProbeInputV1 {
                project_id: ProjectId::parse("project-a").unwrap(),
                authorized_canonical_path: AuthorizedInventoryPath::new(&root).unwrap(),
                committed_authority: Some(RecordedRepoAuthority::parse("repo-family").unwrap()),
            }],
        )
        .unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].path_status,
            LegacyProjectPathStatusV1::Present
        );
        assert_eq!(
            observations[0]
                .committed_authority
                .as_ref()
                .unwrap()
                .authority
                .as_str(),
            "repo-family"
        );
    }

    #[test]
    fn publisher_capture_is_strict_and_binding_keeps_literal_ref() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("publisher-refs.json");
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let source = publisher_source(&path, &[(&scope, "refs/heads/release")]);
        let source_observation_ids = BTreeSet::from(["publisher-source-synthetic".to_string()]);
        let pins = bind_publisher_pins(
            &source,
            vec![PublisherPinBindingInputV1 {
                project_id: ProjectId::parse("project-a").unwrap(),
                expected_scope: scope,
                candidate_attachment_ids: BTreeSet::new(),
                resolved_commit: Some("a".repeat(40)),
                resolved_scope: None,
                source_observation_ids: source_observation_ids.clone(),
            }],
        )
        .unwrap();
        assert_eq!(pins[0].full_ref, "refs/heads/release");
        assert!(
            source_observation_ids
                .iter()
                .all(|id| pins[0].source_observation_ids.contains(id))
        );
        assert_eq!(pins[0].source_observation_ids.len(), 2);

        write_file(
            &path,
            br#"{"version":1,"refs":[{"scope":{"repo_id":"repo-family","bbox_root_relpath":"."},"branch_ref":"main"}]}"#,
        );
        assert!(matches!(
            capture_publisher_ref_source(&AuthorizedInventoryPath::new(&path).unwrap()).unwrap(),
            DecodedSourceObservationV1::Invalid { .. }
        ));
    }

    #[test]
    fn code_source_observation_preserves_exact_sources_and_selector() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store_root = root.join("code-sources");
        let paths = CodeSourceStorePaths::new(&store_root).unwrap();
        let project_id = ProjectId::parse("project-a").unwrap();
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let source_bytes = b"fn main() {}\n";
        let entries = vec![ManifestEntry {
            relative_path: "src/main.rs".to_string(),
            content_sha256: hex::encode(Sha256::digest(source_bytes)),
            size: source_bytes.len() as u64,
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
            logical_bytes: source_bytes.len() as u64,
        };
        let generation = generation_id("host-a", &descriptor);
        let selector = source_selector(project_id.as_str(), &generation);
        let stored = StoredGeneration {
            version: 1,
            generation_id: generation.clone(),
            producer_id: "host-a".to_string(),
            ordinal: 1,
            descriptor: descriptor.clone(),
            state: GenerationState::Active,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(1),
            entity_inventory_sha256: Some("c".repeat(64)),
        };
        let activation = ActivationRecord {
            version: 1,
            project_id: project_id.to_string(),
            generation_id: generation.clone(),
            selector: selector.clone(),
            snapshot_id: format!("collected-{}", "d".repeat(32)),
            document_count: 1,
            entity_inventory_sha256: "c".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 1,
            cutback_pending: false,
            diagnostic: None,
        };
        let mut manifest_bytes = Vec::new();
        serde_json::to_writer(&mut manifest_bytes, &entries[0]).unwrap();
        manifest_bytes.push(b'\n');
        write_file(
            &paths.activation(&project_id),
            &serde_json::to_vec_pretty(&activation).unwrap(),
        );
        write_file(
            &paths.generation_metadata(&scope, &generation).unwrap(),
            &serde_json::to_vec_pretty(&stored).unwrap(),
        );
        write_file(
            &paths.generation_manifest(&scope, &generation).unwrap(),
            &manifest_bytes,
        );

        let capture = observe_code_source(
            &paths,
            CodeSourceReadInputV1 {
                project_id,
                generations: vec![CodeSourceGenerationReadInputV1 {
                    role: CollectedGenerationRoleV1::Active,
                    generation_id: generation,
                    published_scope: scope.clone(),
                    activation_scope: Some(scope),
                    literal_selector: selector.clone(),
                    checkout_missing: false,
                }],
                quarantine: Vec::new(),
            },
            &StoreLimits::default(),
        )
        .unwrap();
        assert_eq!(capture.generations[0].literal_selector, selector);
        assert!(capture.generations[0].selector_matches_generation);
        assert!(matches!(
            capture.generations[0].observation.descriptor,
            ImmutableCollectedDescriptorV1::Valid { .. }
        ));
        assert!(matches!(
            capture.generations[0].observation.manifest,
            ImmutableArtifactObservationV1::Valid { .. }
        ));
        assert!(matches!(
            capture.activation_source,
            DecodedSourceObservationV1::Valid(_)
        ));
    }

    #[test]
    fn checkout_marker_observation_never_creates_and_classifies_content() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let authorized = AuthorizedInventoryPath::new(&root).unwrap();
        let marker = root.join(".bbox/local/checkout-id");
        let missing = observe_checkout(&authorized).unwrap();
        assert_eq!(
            missing.observation.marker_state,
            CheckoutMarkerStateV1::MissingOrEmpty
        );
        assert!(!marker.exists());

        write_file(&marker, b"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\n");
        let valid = observe_checkout(&authorized).unwrap();
        assert_eq!(
            valid.observation.marker_state,
            CheckoutMarkerStateV1::Valid {
                checkout_id: "e".repeat(32)
            }
        );

        write_file(&marker, b"not-an-id");
        let malformed = observe_checkout(&authorized).unwrap();
        assert!(matches!(
            malformed.observation.marker_state,
            CheckoutMarkerStateV1::Malformed { .. }
        ));
    }

    #[test]
    fn assembly_uses_exact_sources_and_is_deterministic() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let projects_path = root.join("projects.json");
        let project_id = ProjectId::parse("project-a").unwrap();
        let projects_json = serde_json::json!({
            "version": 1,
            "projects": [{
                "project_id": project_id,
                "canonical_path": root,
                "registered_at": "2026-01-01T00:00:00Z",
                "is_git_repo": false
            }]
        });
        write_file(&projects_path, &serde_json::to_vec(&projects_json).unwrap());
        let legacy_source =
            capture_legacy_projects_source(&AuthorizedInventoryPath::new(&projects_path).unwrap())
                .unwrap()
                .require_valid("legacy projects")
                .unwrap();
        let legacy_projects = observe_legacy_projects(
            &legacy_source,
            vec![LegacyProjectProbeInputV1 {
                project_id: ProjectId::parse("project-a").unwrap(),
                authorized_canonical_path: AuthorizedInventoryPath::new(&root).unwrap(),
                committed_authority: None,
            }],
        )
        .unwrap();
        let publisher_path = root.join("publisher-refs.json");
        let publisher_ref_source = publisher_source(&publisher_path, &[]);
        let source_bytes = legacy_source.source.bytes.clone();
        let inventory = LiveInventoryAssemblyV1 {
            legacy_source,
            publisher_ref_source,
            legacy_projects,
            code_sources: Vec::new(),
            publisher_pins: Vec::new(),
            injected: InjectedInventoryRowsV1::default(),
        }
        .build()
        .unwrap();
        assert_eq!(inventory.source_store_bytes, source_bytes);
        assert_eq!(
            inventory.inventory_hash().unwrap(),
            inventory.inventory_hash().unwrap()
        );
    }
}
