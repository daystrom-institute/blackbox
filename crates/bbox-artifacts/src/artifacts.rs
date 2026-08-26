use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use anyhow::{Context, Result, anyhow, bail};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest as _;

use bbox_util::util;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Workflow,
    Packet,
    Brofile,
    Agent,
    Atom,
    Team,
    Cron,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Packet => "packet",
            Self::Brofile => "brofile",
            Self::Agent => "agent",
            Self::Atom => "atom",
            Self::Team => "team",
            Self::Cron => "cron",
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArtifactInstallParams {
    pub kind: ArtifactKind,
    /// Local JSON file path or http(s) URL.
    pub source: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub supersedes: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArtifactListParams {
    #[serde(default)]
    pub kind: Option<ArtifactKind>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub include_superseded: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArtifactSupersedeParams {
    pub kind: ArtifactKind,
    pub name: String,
    pub superseded_by: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArtifactRemoveParams {
    pub kind: ArtifactKind,
    pub name: String,
    /// Show the exact catalog paths that would be removed without deleting.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Required when `dry_run=false`.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactMetadata {
    pub kind: ArtifactKind,
    pub name: String,
    pub version: String,
    pub source: String,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    #[serde(default)]
    pub local: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default)]
    pub supersedes_chain: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default = "default_active")]
    pub active: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub install_warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactListEntry {
    pub kind: ArtifactKind,
    pub name: String,
    pub version: String,
    pub source: String,
    pub installed_at: String,
    pub active: bool,
    pub supersedes_chain: Vec<String>,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredArtifact {
    pub kind: ArtifactKind,
    pub path: String,
    pub local: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ArtifactRemoveResult {
    pub kind: ArtifactKind,
    pub name: String,
    pub dry_run: bool,
    pub removed: bool,
    pub paths: Vec<String>,
}

/// Discriminates between a globally-stored artifact and one scoped to a
/// specific project.  Project artifacts live under
/// `artifacts/projects/<project_id>/<local|committed>/<kind>/`.
#[derive(Debug, Clone)]
pub enum ArtifactScope<'a> {
    Global,
    Project { project_id: &'a str, local: bool },
}

impl<'a> ArtifactScope<'a> {
    /// Returns (project_id, project_path, local) for metadata fields.
    /// Global scope returns (None, None, false).
    fn id_path_local(&self) -> (Option<String>, Option<String>, bool) {
        match self {
            Self::Global => (None, None, false),
            Self::Project { project_id, local } => (Some((*project_id).to_string()), None, *local),
        }
    }

    fn subdir(&self) -> Option<PathBuf> {
        match self {
            Self::Global => None,
            Self::Project { project_id, local } => {
                let layer = if *local { "local" } else { "committed" };
                Some(PathBuf::from("projects").join(project_id).join(layer))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactCatalog {
    root: PathBuf,
}

/// Capture durable artifact targets and legacy project-path selectors without
/// opening or creating an [`ArtifactCatalog`].
pub fn capture_project_catalog_owner_snapshot(
    root: &Path,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        LegacyProjectSelectorKindV1, OwnerSnapshotRowV1, OwnerSnapshotStateV1,
        build_owner_snapshot, capture_stable_regular_tree_nofollow, corrupt_owner_snapshot,
        finalize_owner_snapshot, missing_owner_snapshot, owner_subsource, sha256_hex,
        stable_subsource_id,
    };

    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return missing_owner_snapshot("artifact", "artifact:root", limits);
        }
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => {
            return corrupt_owner_snapshot(
                "artifact",
                "artifact:root",
                "owner_tree_unsafe",
                limits,
            );
        }
    }
    let captures =
        match capture_stable_regular_tree_nofollow(root, "artifact", limits, |relative| {
            !relative.components().any(|component| {
                matches!(
                    component,
                    Component::Normal(value)
                        if value.to_string_lossy().starts_with(".retiring-")
                )
            }) && (relative.file_name().and_then(|name| name.to_str()) == Some("metadata.json")
                || relative
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".metadata.json")))
        }) {
            Ok(captures) => captures,
            Err(error) => {
                return corrupt_owner_snapshot("artifact", "artifact:root", error.code, limits);
            }
        };
    if captures.is_empty() {
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(b""),
            byte_len: 0,
        };
        return build_owner_snapshot(
            "artifact",
            vec![owner_subsource("artifact:root", state, &[])],
            Vec::new(),
            limits,
        );
    }
    let mut rows = Vec::new();
    let mut subsources = Vec::new();
    for (relative, captured) in captures {
        let subsource_id = stable_subsource_id("artifact", &relative);
        let Some(bytes) = captured.bytes else {
            return corrupt_owner_snapshot(
                "artifact",
                &subsource_id,
                "owner_source_unreadable",
                limits,
            );
        };
        let metadata: ArtifactMetadata = match serde_json::from_slice(&bytes) {
            Ok(metadata) => metadata,
            Err(_) => {
                return corrupt_owner_snapshot(
                    "artifact",
                    &subsource_id,
                    "owner_source_invalid",
                    limits,
                );
            }
        };
        let mut subsource_rows = Vec::new();
        if let Some(project_id) = metadata
            .project_id
            .as_deref()
            .map(str::trim)
            .filter(|project_id| !project_id.is_empty())
        {
            subsource_rows.push(OwnerSnapshotRowV1::inventory_target(
                format!("{subsource_id}:target"),
                project_id,
                sha256_hex(&bytes),
            ));
        }
        if metadata.project_id.is_none() {
            if let Some(project_path) = metadata
                .project_path
                .as_deref()
                .map(str::trim)
                .filter(|project_path| !project_path.is_empty())
            {
                subsource_rows.push(OwnerSnapshotRowV1::legacy_selector(
                    format!("{subsource_id}:legacy-path"),
                    LegacyProjectSelectorKindV1::Project,
                    project_path,
                ));
            }
        }
        subsources.push(owner_subsource(
            subsource_id,
            captured.state,
            &subsource_rows,
        ));
        rows.extend(subsource_rows);
    }
    finalize_owner_snapshot("artifact", "artifact:root", subsources, rows, limits)
}

/// Stamp one artifact metadata record with its stable project id, the
/// write-back inverse of [`capture_project_catalog_owner_snapshot`].
///
/// The artifact row id is derived from the record's PATH rather than a field
/// inside the document, so `row_id_of` reconstructs it from the subsource id
/// exactly as capture does.
pub fn stamp_project_catalog_owner_row(
    root: &Path,
    source_row_id: &str,
    expected_members: &bbox_corpus_core::project_catalog_snapshot::LegacySelectorMembersV1,
    project_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence(
        source_row_id,
        expected_members,
    )?;
    use bbox_corpus_core::project_catalog_snapshot::stamp_json_tree_row;

    stamp_json_tree_row(
        root,
        "artifact",
        limits,
        |relative| {
            !relative.components().any(|component| {
                matches!(
                    component,
                    Component::Normal(value)
                        if value.to_string_lossy().starts_with(".retiring-")
                )
            }) && (relative.file_name().and_then(|name| name.to_str()) == Some("metadata.json")
                || relative
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".metadata.json")))
        },
        |subsource_id, _document| Some(format!("{subsource_id}:legacy-path")),
        source_row_id,
        project_id,
    )
}

/// Read the stable project ids of MANY artifact metadata rows, the VERIFY half
/// of [`stamp_project_catalog_owner_row`]. Locates the records exactly as the
/// stamper does, so the two agree on row identity by construction.
///
/// Batched over the whole requested set because this owner is a TREE: a per-row
/// caller walks every metadata file once per row.
pub fn read_project_catalog_owner_rows(
    root: &Path,
    rows: &bbox_corpus_core::project_catalog_snapshot::OwnerRowRequestV1,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowBatchV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    bbox_corpus_core::project_catalog_snapshot::ensure_singleton_member_evidence_batch(rows)?;
    let source_row_ids = &rows
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    use bbox_corpus_core::project_catalog_snapshot::read_json_tree_rows_project_id;

    read_json_tree_rows_project_id(
        root,
        "artifact",
        limits,
        |relative| {
            !relative.components().any(|component| {
                matches!(
                    component,
                    Component::Normal(value)
                        if value.to_string_lossy().starts_with(".retiring-")
                )
            }) && (relative.file_name().and_then(|name| name.to_str()) == Some("metadata.json")
                || relative
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".metadata.json")))
        },
        |subsource_id, _document| Some(format!("{subsource_id}:legacy-path")),
        source_row_ids,
    )
}

/// Remove every artifact row owned by one project using the catalog metadata
/// format. Directories are first renamed out of the live tree, then removed.
pub fn discharge_project_catalog_rows(
    root: &Path,
    project_id: &str,
    selectors: &[String],
) -> Result<usize> {
    let targets = capture_project_catalog_retirement_targets(root, project_id, selectors)?;
    discharge_project_catalog_targets(root, &targets)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRetirementTarget {
    pub owner_project_id: String,
    pub legacy_project_path: Option<String>,
    pub artifact_directory: String,
    pub metadata_path: String,
    pub payload_path: String,
    pub metadata_sha256: String,
    pub version_metadata: Vec<ArtifactMetadataCommitment>,
    pub payload_sha256: String,
    pub tree_manifest: Vec<ArtifactMetadataCommitment>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactMetadataCommitment {
    pub path: String,
    pub sha256: String,
}

const MAX_RETIREMENT_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_RETIREMENT_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RETIREMENT_AGGREGATE_BYTES: u64 = 256 * 1024 * 1024;

impl ArtifactRetirementTarget {
    pub fn validate(&self) -> Result<()> {
        if self.owner_project_id.is_empty() {
            bail!("artifact retirement owner project id is empty");
        }
        for (label, value) in [
            ("artifact directory", self.artifact_directory.as_str()),
            ("metadata path", self.metadata_path.as_str()),
            ("payload path", self.payload_path.as_str()),
        ] {
            if !strict_retirement_relative_path(Path::new(value)) {
                bail!("artifact retirement {label} is not a strict relative path");
            }
        }
        if !strict_sha256(&self.metadata_sha256) || !strict_sha256(&self.payload_sha256) {
            bail!("artifact retirement target contains a malformed sha256");
        }
        let directory = Path::new(&self.artifact_directory);
        if !Path::new(&self.metadata_path).starts_with(directory) {
            bail!("artifact retirement metadata is outside its artifact directory");
        }
        if Path::new(&self.payload_path).parent() != directory.parent() {
            bail!("artifact retirement payload is outside its artifact parent");
        }
        let mut metadata_paths = std::collections::BTreeSet::new();
        for commitment in &self.version_metadata {
            let path = Path::new(&commitment.path);
            if !strict_retirement_relative_path(path)
                || !path.starts_with(directory)
                || !strict_sha256(&commitment.sha256)
                || !metadata_paths.insert(&commitment.path)
            {
                bail!("artifact retirement version metadata commitment is invalid");
            }
        }
        if self.tree_manifest.is_empty()
            || self
                .tree_manifest
                .iter()
                .any(|entry| !strict_sha256(&entry.sha256))
        {
            bail!("artifact retirement tree manifest is invalid");
        }
        Ok(())
    }
}

pub fn capture_project_catalog_retirement_targets(
    root: &Path,
    project_id: &str,
    selectors: &[String],
) -> Result<Vec<ArtifactRetirementTarget>> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("artifact store root is not a safe directory");
    }
    let mut targets = Vec::new();
    let mut aggregate_bytes = 0_u64;
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .path()
            .strip_prefix(root)
            .context("artifact entry escaped its store root")?
            .components()
            .any(|component| {
                matches!(
                    component,
                    Component::Normal(value)
                        if value.to_string_lossy().starts_with(".retiring-")
                )
            })
        {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        if file_name != "metadata.json" {
            continue;
        }
        let metadata_bytes = read_bounded_regular_nofollow(
            entry.path(),
            MAX_RETIREMENT_METADATA_BYTES,
            &mut aggregate_bytes,
        )?;
        let metadata: ArtifactMetadata = serde_json::from_slice(&metadata_bytes)?;
        let directory = entry
            .path()
            .parent()
            .ok_or_else(|| anyhow!("artifact metadata has no owning directory"))?;
        let relative = directory
            .strip_prefix(root)
            .context("artifact directory escaped its store root")?;
        let metadata_relative = entry
            .path()
            .strip_prefix(root)
            .context("artifact metadata escaped its store root")?;
        let payload = directory.with_extension("json");
        let payload_relative = payload
            .strip_prefix(root)
            .context("artifact payload escaped its store root")?;
        let Some(canonical_owner) = artifact_owner_identity(&metadata)? else {
            // Global artifact: no project can own it, so no retirement
            // inventories it.
            continue;
        };
        let mut version_metadata = Vec::new();
        let versions = directory.join(".versions");
        if versions.is_dir() {
            for version in WalkDir::new(&versions).follow_links(false) {
                let version = version?;
                if !version.file_type().is_file()
                    || !version
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.ends_with(".metadata.json"))
                {
                    continue;
                }
                let bytes = read_bounded_regular_nofollow(
                    version.path(),
                    MAX_RETIREMENT_METADATA_BYTES,
                    &mut aggregate_bytes,
                )?;
                let version_record: ArtifactMetadata = serde_json::from_slice(&bytes)?;
                if artifact_owner_identity(&version_record)?.as_ref() != Some(&canonical_owner) {
                    bail!("artifact version metadata owner disagrees with canonical metadata");
                }
                version_metadata.push(ArtifactMetadataCommitment {
                    path: version
                        .path()
                        .strip_prefix(root)?
                        .to_str()
                        .context("artifact version metadata path is not UTF-8")?
                        .to_string(),
                    sha256: hex::encode(sha2::Sha256::digest(&bytes)),
                });
            }
        }
        version_metadata.sort();
        let expected_owner = match metadata.project_id.as_deref() {
            Some(owner) => owner == project_id,
            None => metadata
                .project_path
                .as_ref()
                .is_some_and(|path| selectors.iter().any(|selector| selector == path)),
        };
        if !expected_owner {
            continue;
        }
        let payload_tombstone = artifact_payload_tombstone(directory)?;
        let payload_path = if payload.is_file() {
            payload.clone()
        } else if payload_tombstone.is_file() {
            payload_tombstone
        } else {
            bail!("owned artifact metadata has no payload");
        };
        let payload_hash = hash_bounded_regular_nofollow(
            &payload_path,
            MAX_RETIREMENT_PAYLOAD_BYTES,
            &mut aggregate_bytes,
        )?;
        let mut tree_manifest = Vec::new();
        for tree_entry in WalkDir::new(directory).follow_links(false) {
            let tree_entry = tree_entry?;
            if tree_entry.file_type().is_symlink() {
                bail!("artifact retirement tree contains a symlink");
            }
            if !tree_entry.file_type().is_file() {
                continue;
            }
            tree_manifest.push(ArtifactMetadataCommitment {
                path: tree_entry
                    .path()
                    .strip_prefix(root)?
                    .to_str()
                    .context("artifact tree path is not UTF-8")?
                    .to_string(),
                sha256: hash_bounded_regular_nofollow(
                    tree_entry.path(),
                    MAX_RETIREMENT_PAYLOAD_BYTES,
                    &mut aggregate_bytes,
                )?,
            });
        }
        tree_manifest.push(ArtifactMetadataCommitment {
            path: payload_relative
                .to_str()
                .context("artifact payload path is not UTF-8")?
                .to_string(),
            sha256: payload_hash.clone(),
        });
        tree_manifest.sort();
        let target = ArtifactRetirementTarget {
            owner_project_id: project_id.to_string(),
            legacy_project_path: metadata.project_id.is_none().then(|| {
                metadata
                    .project_path
                    .clone()
                    .expect("legacy artifact owner has a project path")
            }),
            artifact_directory: relative
                .to_str()
                .context("artifact directory is not UTF-8")?
                .to_string(),
            metadata_path: metadata_relative
                .to_str()
                .context("artifact metadata path is not UTF-8")?
                .to_string(),
            payload_path: payload_relative
                .to_str()
                .context("artifact payload path is not UTF-8")?
                .to_string(),
            metadata_sha256: hex::encode(sha2::Sha256::digest(&metadata_bytes)),
            version_metadata,
            payload_sha256: payload_hash,
            tree_manifest,
        };
        target.validate()?;
        targets.push(target);
    }
    targets.sort();
    targets.dedup();
    Ok(targets)
}

pub fn discharge_project_catalog_targets(
    root: &Path,
    targets: &[ArtifactRetirementTarget],
) -> Result<usize> {
    if targets.is_empty() {
        return Ok(0);
    }
    with_artifact_mutation_lock(root, || {
        let anchored = AnchoredArtifactRoot::open(root)?;
        let mut removed = 0usize;
        for target in targets {
            target.validate()?;
            anchored.discharge(target)?;
            removed += 1;
        }
        Ok(removed)
    })
}

fn with_artifact_mutation_lock<T>(root: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    bbox_corpus_core::json_store::with_store_lock(&root.join(".artifact-root-mutation"), operation)
}

fn strict_retirement_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn strict_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArtifactOwnerIdentity {
    ProjectId(String),
    LegacyProjectPath(String),
}

/// `None` is a global (project-less) artifact: a legitimate store resident
/// that no project retirement can own, not an error. Refusing here made one
/// global artifact fail every retire on the host before the per-project
/// ownership filter could skip it.
fn artifact_owner_identity(metadata: &ArtifactMetadata) -> Result<Option<ArtifactOwnerIdentity>> {
    if let Some(project_id) = metadata
        .project_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(ArtifactOwnerIdentity::ProjectId(
            project_id.to_string(),
        )));
    }
    Ok(metadata
        .project_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| ArtifactOwnerIdentity::LegacyProjectPath(value.to_string())))
}

fn validate_artifact_target_owner(
    target: &ArtifactRetirementTarget,
    metadata: &ArtifactMetadata,
) -> Result<()> {
    let expected = match &target.legacy_project_path {
        Some(path) => ArtifactOwnerIdentity::LegacyProjectPath(path.clone()),
        None => ArtifactOwnerIdentity::ProjectId(target.owner_project_id.clone()),
    };
    if artifact_owner_identity(metadata)? != Some(expected) {
        bail!("artifact metadata owner drifted after Prepared");
    }
    Ok(())
}

fn checked_retirement_file_len(path: &Path, limit: u64, aggregate: &mut u64) -> Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("artifact retirement input is not a regular nofollow file");
    }
    if metadata.len() > limit {
        bail!("artifact retirement input exceeds its per-file byte limit");
    }
    *aggregate = aggregate
        .checked_add(metadata.len())
        .context("artifact retirement aggregate byte count overflowed")?;
    if *aggregate > MAX_RETIREMENT_AGGREGATE_BYTES {
        bail!("artifact retirement inputs exceed their aggregate byte limit");
    }
    Ok(metadata.len())
}

fn read_bounded_regular_nofollow(path: &Path, limit: u64, aggregate: &mut u64) -> Result<Vec<u8>> {
    use std::io::Read;

    let length = checked_retirement_file_len(path, limit, aggregate)?;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path)?;
    let capacity = usize::try_from(length).context("artifact retirement file is too large")?;
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != length {
        bail!("artifact retirement input changed while being read");
    }
    Ok(bytes)
}

fn hash_bounded_regular_nofollow(path: &Path, limit: u64, aggregate: &mut u64) -> Result<String> {
    use std::io::Read;

    let length = checked_retirement_file_len(path, limit, aggregate)?;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path)?;
    let mut remaining = length;
    let mut buffer = [0_u8; 64 * 1024];
    let mut hash = sha2::Sha256::new();
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
        let read = file.read(&mut buffer[..chunk])?;
        if read == 0 {
            bail!("artifact retirement input changed while being hashed");
        }
        hash.update(&buffer[..read]);
        remaining -= read as u64;
    }
    let mut extra = [0_u8; 1];
    if file.read(&mut extra)? != 0 {
        bail!("artifact retirement input changed while being hashed");
    }
    Ok(hex::encode(hash.finalize()))
}

fn artifact_payload_tombstone(directory: &Path) -> Result<PathBuf> {
    let payload = directory.with_extension("json");
    let parent = directory
        .parent()
        .ok_or_else(|| anyhow!("artifact directory has no parent"))?;
    let name = payload
        .file_name()
        .and_then(|name| name.to_str())
        .context("artifact payload name is not UTF-8")?;
    Ok(parent.join(format!(".retiring-payload-{name}")))
}

#[cfg(unix)]
struct AnchoredArtifactRoot {
    directory: fs::File,
}

#[cfg(unix)]
impl AnchoredArtifactRoot {
    fn open(root: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;

        let directory = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(root)
            .context("failed to open anchored artifact root")?;
        Ok(Self { directory })
    }

    fn discharge(&self, target: &ArtifactRetirementTarget) -> Result<()> {
        let directory = Path::new(&target.artifact_directory);
        let payload = Path::new(&target.payload_path);
        let parent = directory
            .parent()
            .ok_or_else(|| anyhow!("artifact directory has no parent"))?;
        let parent_components = strict_artifact_components(parent)?;
        let parent_fd = self.open_directory_chain(&parent_components)?;
        let directory_name = directory
            .file_name()
            .ok_or_else(|| anyhow!("artifact directory has no final component"))?;
        let payload_name = payload
            .file_name()
            .ok_or_else(|| anyhow!("artifact payload has no final component"))?;
        let metadata_tombstone = std::ffi::OsString::from(format!(
            ".retiring-metadata-{}",
            directory_name.to_string_lossy()
        ));
        let payload_tombstone = std::ffi::OsString::from(format!(
            ".retiring-payload-{}",
            payload_name.to_string_lossy()
        ));

        self.validate_target_state(
            target,
            &parent_fd,
            parent,
            directory_name,
            &metadata_tombstone,
            payload_name,
            &payload_tombstone,
        )?;
        artifact_retirement_fault("before_payload_hide")?;
        if entry_kind_at(&parent_fd, payload_name)? == Some(ArtifactEntryKind::Regular) {
            if entry_kind_at(&parent_fd, &payload_tombstone)?.is_some() {
                bail!("artifact payload retirement tombstone already exists");
            }
            let live = open_artifact_at(parent_fd.as_raw_fd(), payload_name, false)?;
            let live_identity = artifact_file_identity(&live)?;
            if artifact_entry_identity_at(&parent_fd, payload_name)? != Some(live_identity) {
                bail!("artifact payload inode changed before being hidden");
            }
            rename_entry_at(&parent_fd, payload_name, &payload_tombstone)?;
            let hidden = open_artifact_at(parent_fd.as_raw_fd(), &payload_tombstone, false)?;
            if artifact_file_identity(&hidden)? != live_identity {
                bail!("artifact payload inode changed while being hidden");
            }
            artifact_retirement_fault("payload_hidden")?;
        }
        if entry_kind_at(&parent_fd, directory_name)? == Some(ArtifactEntryKind::Directory) {
            if entry_kind_at(&parent_fd, &metadata_tombstone)?.is_some() {
                bail!("artifact metadata retirement tombstone already exists");
            }
            let live = open_artifact_at(parent_fd.as_raw_fd(), directory_name, true)?;
            let live_identity = artifact_file_identity(&live)?;
            if artifact_entry_identity_at(&parent_fd, directory_name)? != Some(live_identity) {
                bail!("artifact metadata inode changed before being hidden");
            }
            rename_entry_at(&parent_fd, directory_name, &metadata_tombstone)?;
            let hidden = open_artifact_at(parent_fd.as_raw_fd(), &metadata_tombstone, true)?;
            if artifact_file_identity(&hidden)? != live_identity {
                bail!("artifact metadata inode changed while being hidden");
            }
            artifact_retirement_fault("metadata_hidden")?;
        }
        artifact_retirement_fault("before_tombstone_validation")?;
        self.validate_target_state(
            target,
            &parent_fd,
            parent,
            directory_name,
            &metadata_tombstone,
            payload_name,
            &payload_tombstone,
        )?;
        artifact_retirement_fault("after_tombstone_validation")?;
        self.validate_target_state(
            target,
            &parent_fd,
            parent,
            directory_name,
            &metadata_tombstone,
            payload_name,
            &payload_tombstone,
        )?;
        remove_committed_artifact_directory(&parent_fd, &metadata_tombstone, target)?;
        if entry_kind_at(&parent_fd, &payload_tombstone)? == Some(ArtifactEntryKind::Regular) {
            let payload = open_artifact_at(parent_fd.as_raw_fd(), &payload_tombstone, false)?;
            let identity = artifact_file_identity(&payload)?;
            let mut aggregate = 0_u64;
            if hash_open_artifact_file(payload, MAX_RETIREMENT_PAYLOAD_BYTES, &mut aggregate)?
                != target.payload_sha256
            {
                bail!("artifact retirement payload changed before deletion");
            }
            if artifact_entry_identity_at(&parent_fd, &payload_tombstone)? != Some(identity) {
                bail!("artifact retirement payload inode changed before deletion");
            }
            remove_artifact_entry_at(&parent_fd, &payload_tombstone)?;
            artifact_retirement_fault("payload_tombstone_removed")?;
        }
        Ok(())
    }

    fn validate_target_state(
        &self,
        target: &ArtifactRetirementTarget,
        parent_fd: &fs::File,
        parent: &Path,
        directory_name: &std::ffi::OsStr,
        metadata_tombstone: &std::ffi::OsStr,
        payload_name: &std::ffi::OsStr,
        payload_tombstone: &std::ffi::OsStr,
    ) -> Result<()> {
        let directory = Path::new(&target.artifact_directory);
        let metadata = Path::new(&target.metadata_path);
        let metadata_suffix = metadata
            .strip_prefix(directory)
            .context("artifact metadata is outside its artifact directory")?;
        let live_metadata = metadata.to_path_buf();
        let tombstone_metadata = parent.join(metadata_tombstone).join(metadata_suffix);
        let live_payload = Path::new(&target.payload_path).to_path_buf();
        let tombstone_payload = parent.join(payload_tombstone);

        let live_directory_kind = self.entry_kind(parent.join(directory_name))?;
        let tombstone_directory_kind = self.entry_kind(parent.join(metadata_tombstone))?;
        let live_payload_kind = self.entry_kind(parent.join(payload_name))?;
        let tombstone_payload_kind = self.entry_kind(parent.join(payload_tombstone))?;

        reject_wrong_artifact_kind("metadata directory", live_directory_kind, true)?;
        reject_wrong_artifact_kind("metadata tombstone", tombstone_directory_kind, true)?;
        reject_wrong_artifact_kind("payload", live_payload_kind, false)?;
        reject_wrong_artifact_kind("payload tombstone", tombstone_payload_kind, false)?;
        if live_directory_kind.is_some() && tombstone_directory_kind.is_some() {
            bail!("artifact has both live and tombstoned metadata");
        }
        if live_payload_kind.is_some() && tombstone_payload_kind.is_some() {
            bail!("artifact has both live and tombstoned payloads");
        }

        let metadata_candidate = if live_directory_kind.is_some() {
            Some(live_metadata)
        } else if tombstone_directory_kind.is_some() {
            Some(tombstone_metadata)
        } else {
            None
        };
        let metadata_path = match metadata_candidate {
            Some(path) => match self.entry_kind(path.clone())? {
                Some(ArtifactEntryKind::Regular) => Some(path),
                Some(_) => bail!("artifact canonical metadata is not a regular file"),
                None => None,
            },
            None => None,
        };
        let payload_path = if live_payload_kind.is_some() {
            Some(live_payload)
        } else if tombstone_payload_kind.is_some() {
            Some(tombstone_payload)
        } else {
            None
        };
        if metadata_path.is_none() && payload_path.is_none() {
            return Ok(());
        }
        if metadata_path.is_some() && payload_path.is_none() {
            bail!("artifact metadata remains after payload disappeared");
        }
        let mut aggregate = 0_u64;
        let mut current_tree = Vec::new();
        if metadata_path.is_some() {
            collect_artifact_tree_at(
                parent_fd,
                if live_directory_kind.is_some() {
                    directory_name
                } else {
                    metadata_tombstone
                },
                directory,
                &mut aggregate,
                &mut current_tree,
            )?;
        }
        if payload_path.is_some() {
            let payload_file = open_artifact_at(
                parent_fd.as_raw_fd(),
                if live_payload_kind.is_some() {
                    payload_name
                } else {
                    payload_tombstone
                },
                false,
            )?;
            current_tree.push(ArtifactMetadataCommitment {
                path: target.payload_path.clone(),
                sha256: hash_open_artifact_file(
                    payload_file,
                    MAX_RETIREMENT_PAYLOAD_BYTES,
                    &mut aggregate,
                )?,
            });
        }
        current_tree.sort();
        let expected = target
            .tree_manifest
            .iter()
            .map(|entry| (&entry.path, &entry.sha256))
            .collect::<std::collections::BTreeMap<_, _>>();
        for current in &current_tree {
            if expected.get(&current.path) != Some(&&current.sha256) {
                bail!("artifact retirement tree drifted after Prepared");
            }
        }
        if let Some(metadata_path) = &metadata_path {
            let metadata_bytes = self.read_bounded_file(
                metadata_path,
                MAX_RETIREMENT_METADATA_BYTES,
                &mut aggregate,
            )?;
            let metadata: ArtifactMetadata = serde_json::from_slice(&metadata_bytes)?;
            validate_artifact_target_owner(target, &metadata)?;
            if hex::encode(sha2::Sha256::digest(&metadata_bytes)) != target.metadata_sha256 {
                bail!("artifact retirement target fingerprint drifted after Prepared");
            }
        }
        for commitment in &target.version_metadata {
            if !current_tree
                .iter()
                .any(|current| current.path == commitment.path)
            {
                continue;
            }
            let relative = Path::new(&commitment.path);
            let suffix = relative
                .strip_prefix(directory)
                .context("artifact version metadata escaped its directory")?;
            let state_path = if live_directory_kind.is_some() {
                relative.to_path_buf()
            } else {
                parent.join(metadata_tombstone).join(suffix)
            };
            let bytes =
                self.read_bounded_file(&state_path, MAX_RETIREMENT_METADATA_BYTES, &mut aggregate)?;
            let version: ArtifactMetadata = serde_json::from_slice(&bytes)?;
            validate_artifact_target_owner(target, &version)?;
            if hex::encode(sha2::Sha256::digest(&bytes)) != commitment.sha256 {
                bail!("artifact retirement version metadata drifted after Prepared");
            }
        }
        if let Some(payload_path) = &payload_path {
            if self.sha256_file(payload_path, MAX_RETIREMENT_PAYLOAD_BYTES, &mut aggregate)?
                != target.payload_sha256
            {
                bail!("artifact retirement payload drifted after Prepared");
            }
        }
        Ok(())
    }

    fn entry_kind(&self, relative: PathBuf) -> Result<Option<ArtifactEntryKind>> {
        let components = strict_artifact_components(&relative)?;
        let Some((name, parents)) = components.split_last() else {
            bail!("artifact retirement path is empty");
        };
        let parent = match self.open_directory_chain(parents) {
            Ok(parent) => parent,
            Err(error) if is_not_found_error(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        entry_kind_at(&parent, name)
    }

    fn read_bounded_file(
        &self,
        relative: &Path,
        limit: u64,
        aggregate: &mut u64,
    ) -> Result<Vec<u8>> {
        use std::io::Read;

        let components = strict_artifact_components(relative)?;
        let Some((name, parents)) = components.split_last() else {
            bail!("artifact retirement file path is empty");
        };
        let parent = self.open_directory_chain(parents)?;
        let mut file = open_artifact_at(parent.as_raw_fd(), name, false)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("artifact retirement fingerprint target is not a regular file");
        }
        if metadata.len() > limit {
            bail!("artifact retirement input exceeds its per-file byte limit");
        }
        *aggregate = aggregate
            .checked_add(metadata.len())
            .context("artifact retirement aggregate byte count overflowed")?;
        if *aggregate > MAX_RETIREMENT_AGGREGATE_BYTES {
            bail!("artifact retirement inputs exceed their aggregate byte limit");
        }
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len()).context("artifact retirement file is too large")?,
        );
        file.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 != metadata.len() {
            bail!("artifact retirement input changed while being read");
        }
        Ok(bytes)
    }

    fn sha256_file(&self, relative: &Path, limit: u64, aggregate: &mut u64) -> Result<String> {
        use std::io::Read;

        let components = strict_artifact_components(relative)?;
        let Some((name, parents)) = components.split_last() else {
            bail!("artifact retirement file path is empty");
        };
        let parent = self.open_directory_chain(parents)?;
        let mut file = open_artifact_at(parent.as_raw_fd(), name, false)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > limit {
            bail!("artifact retirement fingerprint target exceeds its file bound");
        }
        *aggregate = aggregate
            .checked_add(metadata.len())
            .context("artifact retirement aggregate byte count overflowed")?;
        if *aggregate > MAX_RETIREMENT_AGGREGATE_BYTES {
            bail!("artifact retirement inputs exceed their aggregate byte limit");
        }
        let mut remaining = metadata.len();
        let mut buffer = [0_u8; 64 * 1024];
        let mut hash = sha2::Sha256::new();
        while remaining > 0 {
            let chunk = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
            let read = file.read(&mut buffer[..chunk])?;
            if read == 0 {
                bail!("artifact retirement input changed while being hashed");
            }
            hash.update(&buffer[..read]);
            remaining -= read as u64;
        }
        let mut extra = [0_u8; 1];
        if file.read(&mut extra)? != 0 {
            bail!("artifact retirement input changed while being hashed");
        }
        Ok(hex::encode(hash.finalize()))
    }

    fn open_directory_chain(&self, components: &[std::ffi::OsString]) -> Result<fs::File> {
        let mut current = self.directory.try_clone()?;
        for component in components {
            current =
                open_artifact_at(current.as_raw_fd(), component, true).with_context(|| {
                    format!("artifact path component {component:?} is not confined")
                })?;
        }
        Ok(current)
    }
}

fn artifact_retirement_fault(boundary: &str) -> Result<()> {
    if cfg!(debug_assertions)
        && std::env::var("BLACKBOX_TEST_ARTIFACT_RETIRE_FAULT")
            .is_ok_and(|requested| requested == boundary)
    {
        bail!("injected artifact retirement fault after {boundary}");
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArtifactEntryKind {
    Regular,
    Directory,
}

#[cfg(unix)]
fn reject_wrong_artifact_kind(
    label: &str,
    kind: Option<ArtifactEntryKind>,
    directory: bool,
) -> Result<()> {
    let expected = if directory {
        ArtifactEntryKind::Directory
    } else {
        ArtifactEntryKind::Regular
    };
    if kind.is_some_and(|kind| kind != expected) {
        bail!("artifact retirement {label} has an unsupported file type");
    }
    Ok(())
}

#[cfg(unix)]
fn strict_artifact_components(path: &Path) -> Result<Vec<std::ffi::OsString>> {
    if path.as_os_str().is_empty() {
        return Ok(Vec::new());
    }
    if !strict_retirement_relative_path(path) {
        bail!("artifact retirement path is unsafe");
    }
    Ok(path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value.to_os_string(),
            _ => unreachable!("strict path validation accepted a non-normal component"),
        })
        .collect())
}

#[cfg(unix)]
fn open_artifact_at(
    parent_fd: std::os::fd::RawFd,
    name: &std::ffi::OsStr,
    directory: bool,
) -> std::io::Result<fs::File> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent_fd is owned by a live File and name is NUL-terminated.
    let fd = unsafe { libc::openat(parent_fd, name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn entry_kind_at(parent: &fs::File, name: &std::ffi::OsStr) -> Result<Option<ArtifactEntryKind>> {
    use std::os::unix::ffi::OsStrExt;

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| anyhow!("artifact path contains NUL"))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat is writable and name is NUL-terminated.
    let status = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error.into())
        };
    }
    // SAFETY: fstatat initialized stat.
    let stat = unsafe { stat.assume_init() };
    match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => Ok(Some(ArtifactEntryKind::Regular)),
        libc::S_IFDIR => Ok(Some(ArtifactEntryKind::Directory)),
        libc::S_IFLNK => bail!("artifact retirement target is symlinked"),
        _ => bail!("artifact retirement target has an unsupported file type"),
    }
}

#[cfg(unix)]
fn rename_entry_at(parent: &fs::File, from: &std::ffi::OsStr, to: &std::ffi::OsStr) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let from = std::ffi::CString::new(from.as_bytes())
        .map_err(|_| anyhow!("artifact path contains NUL"))?;
    let to =
        std::ffi::CString::new(to.as_bytes()).map_err(|_| anyhow!("artifact path contains NUL"))?;
    // SAFETY: parent is live and both names are NUL-terminated.
    if unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    parent.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn artifact_file_identity(file: &fs::File) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn artifact_entry_identity_at(
    parent: &fs::File,
    name: &std::ffi::OsStr,
) -> Result<Option<(u64, u64)>> {
    use std::os::unix::ffi::OsStrExt;

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| anyhow!("artifact path contains NUL"))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe {
        libc::fstatat(
            std::os::fd::AsRawFd::as_raw_fd(parent),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(Some((stat.st_dev as u64, stat.st_ino as u64)))
}

#[cfg(unix)]
fn remove_committed_artifact_directory(
    parent: &fs::File,
    tombstone: &std::ffi::OsStr,
    target: &ArtifactRetirementTarget,
) -> Result<bool> {
    use std::os::unix::ffi::OsStrExt;

    let kind = match entry_kind_at(parent, tombstone)? {
        Some(kind) => kind,
        None => return Ok(false),
    };
    if kind != ArtifactEntryKind::Directory {
        bail!("artifact metadata tombstone is not a directory");
    }
    let logical_root = Path::new(&target.artifact_directory);
    let expected = target
        .tree_manifest
        .iter()
        .filter(|commitment| commitment.path != target.payload_path)
        .map(|commitment| (PathBuf::from(&commitment.path), commitment.sha256.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let directory = open_artifact_at(parent.as_raw_fd(), tombstone, true)?;
    artifact_retirement_fault("before_committed_tree_delete")?;
    let mut aggregate = 0_u64;
    remove_committed_artifact_children(&directory, logical_root, &expected, &mut aggregate)?;
    let directory_identity = artifact_file_identity(&directory)?;
    if artifact_entry_identity_at(parent, tombstone)? != Some(directory_identity) {
        bail!("artifact retirement metadata tombstone inode changed before deletion");
    }
    let name = std::ffi::CString::new(tombstone.as_bytes())
        .map_err(|_| anyhow!("artifact path contains NUL"))?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    parent.sync_all()?;
    artifact_retirement_fault("metadata_tombstone_removed")?;
    Ok(true)
}

#[cfg(unix)]
fn remove_committed_artifact_children(
    directory: &fs::File,
    logical_directory: &Path,
    expected: &std::collections::BTreeMap<PathBuf, String>,
    aggregate: &mut u64,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    for name in list_artifact_directory(directory)? {
        let logical_path = logical_directory.join(&name);
        match entry_kind_at(directory, &name)? {
            Some(ArtifactEntryKind::Regular) => {
                let expected_hash = expected
                    .get(&logical_path)
                    .context("artifact retirement tombstone contains an uncommitted file")?;
                let file = open_artifact_at(directory.as_raw_fd(), &name, false)?;
                let identity = artifact_file_identity(&file)?;
                let hash = hash_open_artifact_file(file, MAX_RETIREMENT_PAYLOAD_BYTES, aggregate)?;
                if &hash != expected_hash {
                    bail!("artifact retirement committed file changed after Prepared");
                }
                if artifact_entry_identity_at(directory, &name)? != Some(identity) {
                    bail!("artifact retirement committed file inode changed before deletion");
                }
                let name_c = std::ffi::CString::new(name.as_bytes())
                    .map_err(|_| anyhow!("artifact path contains NUL"))?;
                if unsafe { libc::unlinkat(directory.as_raw_fd(), name_c.as_ptr(), 0) } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                directory.sync_all()?;
                artifact_retirement_fault("committed_file_unlinked")?;
            }
            Some(ArtifactEntryKind::Directory) => {
                let child = open_artifact_at(directory.as_raw_fd(), &name, true)?;
                remove_committed_artifact_children(&child, &logical_path, expected, aggregate)?;
                let identity = artifact_file_identity(&child)?;
                if artifact_entry_identity_at(directory, &name)? != Some(identity) {
                    bail!("artifact retirement directory inode changed before deletion");
                }
                let name_c = std::ffi::CString::new(name.as_bytes())
                    .map_err(|_| anyhow!("artifact path contains NUL"))?;
                if unsafe {
                    libc::unlinkat(directory.as_raw_fd(), name_c.as_ptr(), libc::AT_REMOVEDIR)
                } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                directory.sync_all()?;
                artifact_retirement_fault("committed_directory_removed")?;
            }
            None => bail!("artifact retirement tombstone changed during deletion"),
        }
    }
    directory.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn remove_artifact_entry_at(parent: &fs::File, name: &std::ffi::OsStr) -> Result<bool> {
    use std::os::unix::ffi::OsStrExt;

    let kind = match entry_kind_at(parent, name)? {
        Some(kind) => kind,
        None => return Ok(false),
    };
    let name_c = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| anyhow!("artifact path contains NUL"))?;
    match kind {
        ArtifactEntryKind::Directory => {
            let child = open_artifact_at(parent.as_raw_fd(), name, true)
                .context("artifact directory changed during confinement check")?;
            for child_name in list_artifact_directory(&child)? {
                remove_artifact_entry_at(&child, &child_name)?;
            }
            // SAFETY: parent and name identify the drained directory.
            if unsafe { libc::unlinkat(parent.as_raw_fd(), name_c.as_ptr(), libc::AT_REMOVEDIR) }
                != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        ArtifactEntryKind::Regular => {
            // SAFETY: parent and name identify the regular file checked above.
            if unsafe { libc::unlinkat(parent.as_raw_fd(), name_c.as_ptr(), 0) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    parent.sync_all()?;
    Ok(true)
}

#[cfg(unix)]
fn collect_artifact_tree_at(
    parent: &fs::File,
    name: &std::ffi::OsStr,
    logical_path: &Path,
    aggregate: &mut u64,
    entries: &mut Vec<ArtifactMetadataCommitment>,
) -> Result<()> {
    let directory = open_artifact_at(parent.as_raw_fd(), name, true)?;
    for child_name in list_artifact_directory(&directory)? {
        let child_logical = logical_path.join(&child_name);
        match entry_kind_at(&directory, &child_name)? {
            Some(ArtifactEntryKind::Directory) => collect_artifact_tree_at(
                &directory,
                &child_name,
                &child_logical,
                aggregate,
                entries,
            )?,
            Some(ArtifactEntryKind::Regular) => {
                let file = open_artifact_at(directory.as_raw_fd(), &child_name, false)?;
                entries.push(ArtifactMetadataCommitment {
                    path: child_logical
                        .to_str()
                        .context("artifact tree path is not UTF-8")?
                        .to_string(),
                    sha256: hash_open_artifact_file(file, MAX_RETIREMENT_PAYLOAD_BYTES, aggregate)?,
                });
            }
            None => bail!("artifact tree changed during validation"),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn hash_open_artifact_file(mut file: fs::File, limit: u64, aggregate: &mut u64) -> Result<String> {
    use std::io::Read;

    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        bail!("artifact tree file exceeds its bound");
    }
    *aggregate = aggregate
        .checked_add(metadata.len())
        .context("artifact aggregate byte count overflowed")?;
    if *aggregate > MAX_RETIREMENT_AGGREGATE_BYTES {
        bail!("artifact tree exceeds its aggregate bound");
    }
    let mut remaining = metadata.len();
    let mut hash = sha2::Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
        let read = file.read(&mut buffer[..chunk])?;
        if read == 0 {
            bail!("artifact tree file changed while hashing");
        }
        hash.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(hex::encode(hash.finalize()))
}

#[cfg(unix)]
fn list_artifact_directory(directory: &fs::File) -> Result<Vec<std::ffi::OsString>> {
    use std::os::unix::ffi::OsStringExt;

    // SAFETY: dup creates an independent descriptor for fdopendir.
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: duplicate is a valid directory descriptor.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        // SAFETY: fdopendir did not consume duplicate on failure.
        unsafe { libc::close(duplicate) };
        return Err(std::io::Error::last_os_error().into());
    }
    let mut names = Vec::new();
    #[cfg(test)]
    let mut entries_seen = 0_isize;
    loop {
        set_artifact_readdir_errno(0);
        // SAFETY: stream remains valid until closed below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let errno = artifact_readdir_errno();
            if errno == 0 {
                break;
            }
            unsafe { libc::closedir(stream) };
            return Err(std::io::Error::from_raw_os_error(errno).into());
        }
        // SAFETY: d_name is NUL-terminated by readdir.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(std::ffi::OsString::from_vec(name.to_vec()));
            #[cfg(test)]
            {
                entries_seen += 1;
                if TEST_ARTIFACT_READDIR_FAIL_AFTER.load(std::sync::atomic::Ordering::SeqCst)
                    == entries_seen
                {
                    unsafe { libc::closedir(stream) };
                    return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
                }
            }
        }
    }
    // SAFETY: stream was returned by fdopendir and is closed once.
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
static TEST_ARTIFACT_READDIR_FAIL_AFTER: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(-1);

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn artifact_readdir_errno_location() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn artifact_readdir_errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

fn set_artifact_readdir_errno(value: libc::c_int) {
    unsafe { *artifact_readdir_errno_location() = value };
}

fn artifact_readdir_errno() -> libc::c_int {
    unsafe { *artifact_readdir_errno_location() }
}

#[cfg(unix)]
fn is_not_found_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

#[cfg(not(unix))]
struct AnchoredArtifactRoot {
    root: PathBuf,
}

#[cfg(not(unix))]
impl AnchoredArtifactRoot {
    fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn discharge(&self, target: &ArtifactRetirementTarget) -> Result<()> {
        target.validate()?;
        let directory = self.validate(Path::new(&target.artifact_directory))?;
        let payload = self.validate(Path::new(&target.payload_path))?;
        let parent = directory
            .parent()
            .ok_or_else(|| anyhow!("artifact directory has no parent"))?;
        let metadata_tombstone = parent.join(format!(
            ".retiring-metadata-{}",
            directory
                .file_name()
                .and_then(|name| name.to_str())
                .context("artifact directory name is not UTF-8")?
        ));
        let payload_tombstone = artifact_payload_tombstone(&directory)?;
        if payload.is_file() {
            fs::rename(&payload, &payload_tombstone)?;
            fs::File::open(parent)?.sync_all()?;
        }
        if directory.is_dir() {
            fs::rename(&directory, &metadata_tombstone)?;
            fs::File::open(parent)?.sync_all()?;
        }
        if metadata_tombstone.is_dir() {
            fs::remove_dir_all(&metadata_tombstone)?;
        }
        if payload_tombstone.is_file() {
            fs::remove_file(&payload_tombstone)?;
        }
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    fn validate(&self, relative: &Path) -> Result<PathBuf> {
        if !strict_retirement_relative_path(relative) {
            bail!("artifact retirement path is unsafe");
        }
        let mut current = self.root.clone();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                bail!("artifact retirement path is unsafe");
            };
            current.push(component);
            if current.exists() && fs::symlink_metadata(&current)?.file_type().is_symlink() {
                bail!("artifact retirement path is symlinked");
            }
        }
        Ok(current)
    }
}

impl ArtifactCatalog {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn install_value(
        &self,
        kind: ArtifactKind,
        source: String,
        value: &Value,
        name_override: Option<String>,
        version_override: Option<String>,
        supersedes_override: Option<String>,
    ) -> Result<ArtifactMetadata> {
        self.install_value_scoped(
            ArtifactScope::Global,
            kind,
            source,
            value,
            name_override,
            version_override,
            supersedes_override,
        )
    }

    pub fn install_value_scoped(
        &self,
        scope: ArtifactScope<'_>,
        kind: ArtifactKind,
        source: String,
        value: &Value,
        name_override: Option<String>,
        version_override: Option<String>,
        supersedes_override: Option<String>,
    ) -> Result<ArtifactMetadata> {
        let name = name_override
            .clone()
            .or_else(|| artifact_name(kind, value))
            .ok_or_else(|| {
                anyhow!("artifact name required (via value.name/domain or name_override)")
            })?;
        let meta_path = self.metadata_path_scoped(&scope, kind, &name)?;

        with_artifact_mutation_lock(&self.root, || {
            bbox_corpus_core::json_store::with_store_lock(&meta_path, || {
                self.install_value_locked_scoped(
                    scope,
                    kind,
                    source,
                    value,
                    name_override,
                    version_override,
                    supersedes_override,
                )
            })
        })
    }

    #[allow(dead_code)] // used by tests in this file and src/watcher.rs
    pub fn load_artifact_value_scoped(
        &self,
        project_id: Option<&str>,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<Option<Value>> {
        // Lookup order: project local → project committed → global
        if let Some(pid) = project_id {
            let local_scope = ArtifactScope::Project {
                project_id: pid,
                local: true,
            };
            let path = self.artifact_path_scoped(&local_scope, kind, name)?;
            let metadata = self.metadata_path_scoped(&local_scope, kind, name)?;
            if metadata.is_file() && path.exists() {
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                return Ok(Some(
                    serde_json::from_str(&raw)
                        .with_context(|| format!("parsing {}", path.display()))?,
                ));
            }

            let committed_scope = ArtifactScope::Project {
                project_id: pid,
                local: false,
            };
            let path = self.artifact_path_scoped(&committed_scope, kind, name)?;
            let metadata = self.metadata_path_scoped(&committed_scope, kind, name)?;
            if metadata.is_file() && path.exists() {
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                return Ok(Some(
                    serde_json::from_str(&raw)
                        .with_context(|| format!("parsing {}", path.display()))?,
                ));
            }
        }
        // Fall back to global.
        self.load_artifact_value(kind, name)
    }

    fn install_value_locked_scoped(
        &self,
        scope: ArtifactScope<'_>,
        kind: ArtifactKind,
        source: String,
        value: &Value,
        name_override: Option<String>,
        version_override: Option<String>,
        supersedes_override: Option<String>,
    ) -> Result<ArtifactMetadata> {
        let name = name_override
            .or_else(|| artifact_name(kind, value))
            .ok_or_else(|| anyhow!("artifact name missing for {}", kind.as_str()))?;
        validate_artifact_name(&name)?;
        let version = version_override
            .or_else(|| artifact_version(value))
            .ok_or_else(|| anyhow!("artifact `{name}` missing required version"))?;

        // Compute hash before supersession logic so idempotency check can use it.
        let hash = artifact_content_sha256(value)?;

        // Idempotency: if active metadata for same scope/kind/name/hash exists, no-op.
        let (project_id, project_path, local) = scope.id_path_local();
        let existing = self.load_metadata_scoped(&scope, kind, &name);
        if let Ok(existing_meta) = existing {
            if existing_meta.active
                && existing_meta.content_sha256.as_deref() == Some(&hash)
                && existing_meta.project_id == project_id
            {
                return Ok(existing_meta);
            }
        }

        let supersedes = supersedes_override.or_else(|| artifact_supersedes(value));
        let mut chain = Vec::new();
        if let Some(prev) = supersedes.as_deref() {
            if let Ok(prev_meta) = self.load_metadata(kind, prev) {
                chain.extend(prev_meta.supersedes_chain.clone());
            }
            chain.push(prev.to_string());
            let _ = self.mark_superseded(kind, prev, &name);
        }

        let artifact_path = self.artifact_path_scoped(&scope, kind, &name)?;
        if let Some(parent) = artifact_path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&artifact_path, value)?;

        let metadata = ArtifactMetadata {
            kind,
            name,
            version,
            source,
            installed_at: util::now_iso(),
            content_sha256: Some(hash),
            project_id,
            project_path,
            local,
            supersedes,
            supersedes_chain: chain,
            superseded_by: None,
            active: true,
            install_warnings: Vec::new(),
        };
        self.save_metadata_scoped(&scope, &metadata)?;
        self.save_version_snapshot_scoped(&scope, &metadata, value)?;
        Ok(metadata)
    }

    pub fn list(&self, p: &ArtifactListParams) -> Result<Vec<ArtifactListEntry>> {
        let mut out = Vec::new();
        let kinds: Vec<ArtifactKind> = match p.kind {
            Some(k) => vec![k],
            None => vec![
                ArtifactKind::Workflow,
                ArtifactKind::Packet,
                ArtifactKind::Brofile,
                ArtifactKind::Agent,
                ArtifactKind::Atom,
                ArtifactKind::Team,
                ArtifactKind::Cron,
            ],
        };
        for kind in kinds {
            let dir = self.root.join(kind.as_str());
            if !dir.exists() {
                continue;
            }
            for entry in WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.file_name().and_then(|s| s.to_str()) != Some("metadata.json") {
                    continue;
                }
                let raw = fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let meta: ArtifactMetadata = serde_json::from_str(&raw)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if let Some(name) = p.name.as_deref() {
                    if meta.name != name {
                        continue;
                    }
                }
                if !p.include_superseded && meta.kind == ArtifactKind::Agent && !meta.active {
                    continue;
                }
                let artifact_path = self.artifact_path(meta.kind, &meta.name)?;
                let description = if meta.kind == ArtifactKind::Agent {
                    extract_agent_description(&artifact_path)
                } else {
                    None
                };
                let current_meta = meta.clone();
                out.push(ArtifactListEntry {
                    kind: current_meta.kind,
                    name: current_meta.name,
                    version: current_meta.version,
                    source: current_meta.source,
                    installed_at: current_meta.installed_at,
                    active: current_meta.active,
                    supersedes_chain: current_meta.supersedes_chain,
                    path: artifact_path.to_string_lossy().into_owned(),
                    superseded_by: current_meta.superseded_by,
                    description,
                });
                if p.include_superseded && meta.kind == ArtifactKind::Agent {
                    let version_dir = self.version_dir_path(meta.kind, &meta.name)?;
                    if version_dir.exists() {
                        for version_entry in WalkDir::new(&version_dir)
                            .max_depth(1)
                            .into_iter()
                            .filter_map(|e| e.ok())
                        {
                            let version_path = version_entry.path();
                            let Some(file_name) = version_path.file_name().and_then(|s| s.to_str())
                            else {
                                continue;
                            };
                            let Some(version) = file_name
                                .strip_prefix('v')
                                .and_then(|s| s.strip_suffix(".metadata.json"))
                            else {
                                continue;
                            };
                            if version == meta.version {
                                continue;
                            }
                            let raw = fs::read_to_string(version_path)
                                .with_context(|| format!("reading {}", version_path.display()))?;
                            let version_meta: ArtifactMetadata = serde_json::from_str(&raw)
                                .with_context(|| format!("parsing {}", version_path.display()))?;
                            let artifact_path =
                                self.version_artifact_path(meta.kind, &meta.name, version)?;
                            let description = extract_agent_description(&artifact_path);
                            out.push(ArtifactListEntry {
                                kind: version_meta.kind,
                                name: version_meta.name,
                                version: version_meta.version,
                                source: version_meta.source,
                                installed_at: version_meta.installed_at,
                                active: version_meta.active,
                                supersedes_chain: version_meta.supersedes_chain,
                                path: artifact_path.to_string_lossy().into_owned(),
                                superseded_by: version_meta.superseded_by,
                                description,
                            });
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            a.kind
                .as_str()
                .cmp(b.kind.as_str())
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| b.installed_at.cmp(&a.installed_at))
        });
        Ok(out)
    }

    pub fn supersede(
        &self,
        kind: ArtifactKind,
        name: &str,
        superseded_by: &str,
    ) -> Result<ArtifactMetadata> {
        let meta_path = self.metadata_path(kind, name)?;
        with_artifact_mutation_lock(&self.root, || {
            bbox_corpus_core::json_store::with_store_lock(&meta_path, || {
                self.supersede_locked(kind, name, superseded_by)
            })
        })
    }

    fn supersede_locked(
        &self,
        kind: ArtifactKind,
        name: &str,
        superseded_by: &str,
    ) -> Result<ArtifactMetadata> {
        validate_artifact_name(name)?;
        validate_artifact_name(superseded_by)?;
        self.load_metadata(kind, superseded_by)
            .with_context(|| format!("superseding artifact `{superseded_by}` must exist"))?;
        self.mark_superseded(kind, name, superseded_by)
    }

    pub fn remove_hard(
        &self,
        kind: ArtifactKind,
        name: &str,
        dry_run: bool,
        confirm: bool,
    ) -> Result<ArtifactRemoveResult> {
        validate_artifact_name(name)?;
        if !dry_run && !confirm {
            bail!("hard artifact removal requires confirm=true");
        }

        let meta_path = self.metadata_path(kind, name)?;
        with_artifact_mutation_lock(&self.root, || {
            bbox_corpus_core::json_store::with_store_lock(&meta_path, || {
                self.remove_hard_locked(kind, name, dry_run)
            })
        })
    }

    fn remove_hard_locked(
        &self,
        kind: ArtifactKind,
        name: &str,
        dry_run: bool,
    ) -> Result<ArtifactRemoveResult> {
        let artifact_path = self.artifact_path(kind, name)?;
        let metadata_dir = self.root.join(kind.as_str()).join(name_dir_path(name)?);
        if !artifact_path.exists() && !metadata_dir.exists() {
            bail!("artifact `{}` `{}` not found", kind.as_str(), name);
        }

        let paths = vec![
            artifact_path.to_string_lossy().into_owned(),
            metadata_dir.to_string_lossy().into_owned(),
        ];

        if dry_run {
            return Ok(ArtifactRemoveResult {
                kind,
                name: name.to_string(),
                dry_run,
                removed: false,
                paths,
            });
        }

        remove_file_if_exists(&artifact_path)?;
        remove_dir_if_exists(&metadata_dir)?;
        Ok(ArtifactRemoveResult {
            kind,
            name: name.to_string(),
            dry_run,
            removed: true,
            paths,
        })
    }

    fn mark_superseded(
        &self,
        kind: ArtifactKind,
        name: &str,
        superseded_by: &str,
    ) -> Result<ArtifactMetadata> {
        let mut meta = self.load_metadata(kind, name)?;
        meta.active = false;
        meta.superseded_by = Some(superseded_by.to_string());
        self.save_metadata(&meta)?;
        self.save_version_metadata(&meta)?;
        Ok(meta)
    }

    pub fn load_artifact_value(&self, kind: ArtifactKind, name: &str) -> Result<Option<Value>> {
        let path = self.artifact_path(kind, name)?;
        if !path.exists() {
            return Ok(None);
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let value: Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(value))
    }

    pub fn load_artifact_value_version(
        &self,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<Option<Value>> {
        validate_artifact_name(name)?;
        validate_version_component(version)?;
        let path = self.version_artifact_path(kind, name, version)?;
        if !path.exists() {
            return Ok(None);
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let value: Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(value))
    }

    pub fn metadata_for(&self, kind: ArtifactKind, name: &str) -> Result<Option<ArtifactMetadata>> {
        let path = self.metadata_path(kind, name)?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(self.load_metadata(kind, name)?))
    }

    pub fn metadata_for_version(
        &self,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<Option<ArtifactMetadata>> {
        validate_artifact_name(name)?;
        validate_version_component(version)?;
        let path = self.version_metadata_path(kind, name, version)?;
        if !path.exists() {
            return Ok(None);
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))
            .map(Some)
    }

    pub fn update_install_warnings(
        &self,
        kind: ArtifactKind,
        name: &str,
        warnings: Vec<String>,
    ) -> Result<ArtifactMetadata> {
        let meta_path = self.metadata_path(kind, name)?;
        with_artifact_mutation_lock(&self.root, || {
            bbox_corpus_core::json_store::with_store_lock(&meta_path, || {
                let mut meta = self.load_metadata(kind, name)?;
                meta.install_warnings = warnings;
                self.save_metadata(&meta)?;
                self.save_version_metadata(&meta)?;
                Ok(meta)
            })
        })
    }

    fn load_metadata(&self, kind: ArtifactKind, name: &str) -> Result<ArtifactMetadata> {
        let path = self.metadata_path(kind, name)?;
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    fn save_metadata(&self, meta: &ArtifactMetadata) -> Result<()> {
        let path = self.metadata_path(meta.kind, &meta.name)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&path, meta)
    }

    fn save_version_metadata(&self, meta: &ArtifactMetadata) -> Result<()> {
        let path = self.version_metadata_path(meta.kind, &meta.name, &meta.version)?;
        atomic_write_json(&path, meta)
    }

    fn artifact_path(&self, kind: ArtifactKind, name: &str) -> Result<PathBuf> {
        Ok(self.root.join(kind.as_str()).join(name_path(name, "json")?))
    }

    fn metadata_path(&self, kind: ArtifactKind, name: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join(kind.as_str())
            .join(name_dir_path(name)?)
            .join("metadata.json"))
    }

    fn version_artifact_path(
        &self,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .version_dir_path(kind, name)?
            .join(format!("v{version}.json")))
    }

    fn version_metadata_path(
        &self,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .version_dir_path(kind, name)?
            .join(format!("v{version}.metadata.json")))
    }

    fn version_dir_path(&self, kind: ArtifactKind, name: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join(kind.as_str())
            .join(name_dir_path(name)?)
            .join(".versions"))
    }

    // ── Scoped path helpers ────────────────────────────────────────────

    fn scoped_root(&self, scope: &ArtifactScope<'_>) -> PathBuf {
        match scope.subdir() {
            None => self.root.clone(),
            Some(sub) => self.root.join(sub),
        }
    }

    fn artifact_path_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .scoped_root(scope)
            .join(kind.as_str())
            .join(name_path(name, "json")?))
    }

    fn metadata_path_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .scoped_root(scope)
            .join(kind.as_str())
            .join(name_dir_path(name)?)
            .join("metadata.json"))
    }

    fn version_dir_path_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .scoped_root(scope)
            .join(kind.as_str())
            .join(name_dir_path(name)?)
            .join(".versions"))
    }

    fn version_artifact_path_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .version_dir_path_scoped(scope, kind, name)?
            .join(format!("v{version}.json")))
    }

    fn version_metadata_path_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .version_dir_path_scoped(scope, kind, name)?
            .join(format!("v{version}.metadata.json")))
    }

    fn load_metadata_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<ArtifactMetadata> {
        let path = self.metadata_path_scoped(scope, kind, name)?;
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    fn save_metadata_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        meta: &ArtifactMetadata,
    ) -> Result<()> {
        let path = self.metadata_path_scoped(scope, meta.kind, &meta.name)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&path, meta)
    }

    fn save_version_snapshot_scoped(
        &self,
        scope: &ArtifactScope<'_>,
        meta: &ArtifactMetadata,
        value: &Value,
    ) -> Result<()> {
        let artifact_path =
            self.version_artifact_path_scoped(scope, meta.kind, &meta.name, &meta.version)?;
        if let Some(parent) = artifact_path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&artifact_path, value)?;
        let version_meta_path =
            self.version_metadata_path_scoped(scope, meta.kind, &meta.name, &meta.version)?;
        if let Some(parent) = version_meta_path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_json(&version_meta_path, meta)
    }

    /// Mark the artifact whose `source` field matches `source_path` as removed.
    ///
    /// Sets `active=false` and `superseded_by=Some("file_removed")`.  Does not
    /// delete the artifact JSON or version snapshots (audit trail preserved).
    /// Returns `Ok(None)` when no matching active artifact is found.
    pub fn mark_removed_by_source(
        &self,
        scope: ArtifactScope<'_>,
        kind: ArtifactKind,
        source_path: &Path,
    ) -> anyhow::Result<Option<ArtifactMetadata>> {
        with_artifact_mutation_lock(&self.root, || {
            self.mark_removed_by_source_locked(scope, kind, source_path)
        })
    }

    pub fn active_artifact_by_source(
        &self,
        scope: ArtifactScope<'_>,
        kind: ArtifactKind,
        source_path: &Path,
    ) -> anyhow::Result<Option<ArtifactMetadata>> {
        with_artifact_mutation_lock(&self.root, || {
            self.active_artifact_by_source_locked(&scope, kind, source_path)
        })
    }

    pub fn mark_removed_by_source_if_identity(
        &self,
        scope: ArtifactScope<'_>,
        kind: ArtifactKind,
        source_path: &Path,
        expected_name: &str,
        expected_version: &str,
        expected_content_sha256: Option<&str>,
    ) -> anyhow::Result<Option<ArtifactMetadata>> {
        with_artifact_mutation_lock(&self.root, || {
            let Some(meta) = self.active_artifact_by_source_locked(&scope, kind, source_path)?
            else {
                return Ok(None);
            };
            // R16F4: bind complete metadata identity. Name and version
            // alone are not sufficient: a reinstall with the same name
            // and version but different content must NOT be deactivated
            // by a stale removal prepared against the old content.
            let Some(expected_content_sha256) = expected_content_sha256 else {
                return Ok(None);
            };
            if meta.name != expected_name
                || meta.version != expected_version
                || meta.content_sha256.as_deref() != Some(expected_content_sha256)
            {
                return Ok(None);
            }
            self.mark_removed_metadata_locked(scope, meta)
        })
    }

    fn mark_removed_by_source_locked(
        &self,
        scope: ArtifactScope<'_>,
        kind: ArtifactKind,
        source_path: &Path,
    ) -> anyhow::Result<Option<ArtifactMetadata>> {
        let Some(meta) = self.active_artifact_by_source_locked(&scope, kind, source_path)? else {
            return Ok(None);
        };
        self.mark_removed_metadata_locked(scope, meta)
    }

    fn active_artifact_by_source_locked(
        &self,
        scope: &ArtifactScope<'_>,
        kind: ArtifactKind,
        source_path: &Path,
    ) -> anyhow::Result<Option<ArtifactMetadata>> {
        let source_str = source_path.to_string_lossy();
        let kind_dir = self.scoped_root(scope).join(kind.as_str());
        match fs::symlink_metadata(&kind_dir) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => bail!("artifact kind directory is not a safe directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        for entry in WalkDir::new(&kind_dir).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_symlink() {
                bail!("artifact watcher traversal encountered a symlink");
            }
            let path = entry.path();
            if path.file_name().and_then(|s| s.to_str()) != Some("metadata.json") {
                continue;
            }
            let raw = fs::read_to_string(path)?;
            let meta: ArtifactMetadata = serde_json::from_str(&raw)?;
            if !meta.active || meta.source != source_str {
                continue;
            }
            return Ok(Some(meta));
        }
        Ok(None)
    }

    fn mark_removed_metadata_locked(
        &self,
        scope: ArtifactScope<'_>,
        mut meta: ArtifactMetadata,
    ) -> anyhow::Result<Option<ArtifactMetadata>> {
        meta.active = false;
        meta.superseded_by = Some("file_removed".to_string());
        self.save_metadata_scoped(&scope, &meta)?;
        let version_meta_path =
            self.version_metadata_path_scoped(&scope, meta.kind, &meta.name, &meta.version)?;
        if version_meta_path.exists() {
            atomic_write_json(&version_meta_path, &meta)?;
        }
        Ok(Some(meta))
    }
}

/// Report from `backfill_content_hashes`.
pub struct BackfillReport {
    pub active_updated: usize,
    pub version_updated: usize,
    pub missing_artifacts: usize,
}

impl ArtifactCatalog {
    /// Back-fill `content_sha256` into metadata files that lack it.
    ///
    /// Walks all active `metadata.json` files and all version snapshot metadata
    /// files under the catalog root.  For each entry with `content_sha256:
    /// None`, computes the hash from the corresponding artifact JSON file and
    /// writes back the patched metadata.  Missing artifact JSON is logged and
    /// counted but does not abort startup.
    pub fn backfill_content_hashes(&self) -> anyhow::Result<BackfillReport> {
        let mut report = BackfillReport {
            active_updated: 0,
            version_updated: 0,
            missing_artifacts: 0,
        };
        self.backfill_in_dir(&self.root, &mut report)?;
        Ok(report)
    }

    fn backfill_in_dir(&self, root: &Path, report: &mut BackfillReport) -> anyhow::Result<()> {
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let file_name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n,
                None => continue,
            };

            let is_active = file_name == "metadata.json";
            let is_version = file_name.starts_with('v') && file_name.ends_with(".metadata.json");
            if !is_active && !is_version {
                continue;
            }

            let raw = match fs::read_to_string(path) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("backfill: reading {}: {e}", path.display());
                    continue;
                }
            };
            let mut meta: ArtifactMetadata = match serde_json::from_str(&raw) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("backfill: parsing {}: {e}", path.display());
                    continue;
                }
            };

            if meta.content_sha256.is_some() {
                continue;
            }

            // Compute artifact path from the metadata file location.
            let artifact_path = if is_active {
                // active metadata: <root>/<kind>/<name>/metadata.json
                // artifact: <root>/<kind>/<name>.json
                let dir = path.parent().unwrap();
                let artifact_name_os = dir.file_name().unwrap();
                let parent = dir.parent().unwrap();
                parent.join(format!("{}.json", artifact_name_os.to_string_lossy()))
            } else {
                // version metadata: .../.versions/v<ver>.metadata.json
                // artifact: .../.versions/v<ver>.json
                let version_stem = file_name.strip_suffix(".metadata.json").unwrap();
                path.parent().unwrap().join(format!("{version_stem}.json"))
            };

            if !artifact_path.exists() {
                tracing::warn!("backfill: artifact json missing for {}", path.display());
                report.missing_artifacts += 1;
                continue;
            }

            let artifact_raw = match fs::read_to_string(&artifact_path) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        "backfill: reading artifact {}: {e}",
                        artifact_path.display()
                    );
                    report.missing_artifacts += 1;
                    continue;
                }
            };
            let artifact_value: Value = match serde_json::from_str(&artifact_raw) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "backfill: parsing artifact {}: {e}",
                        artifact_path.display()
                    );
                    report.missing_artifacts += 1;
                    continue;
                }
            };

            let hash = match artifact_content_sha256(&artifact_value) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("backfill: hashing {}: {e}", artifact_path.display());
                    continue;
                }
            };

            meta.content_sha256 = Some(hash);
            if let Err(e) = atomic_write_json(path, &meta) {
                tracing::warn!("backfill: writing {}: {e}", path.display());
                continue;
            }

            if is_active {
                report.active_updated += 1;
            } else {
                report.version_updated += 1;
            }
        }
        Ok(())
    }
}

pub fn discover_project_artifacts(project_dir: &Path) -> Result<Vec<DiscoveredArtifact>> {
    let bbox_root = project_dir.join(".bbox");
    if !bbox_root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();

    // Scan committed artifacts: .bbox/<kind>/*.json
    scan_artifact_dir(&bbox_root, &bbox_root, false, &mut out);

    // Scan local artifacts: .bbox/local/<kind>/*.json
    let local_root = bbox_root.join("local");
    if local_root.exists() {
        scan_artifact_dir(&local_root, &bbox_root, true, &mut out);
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Discover and install all `.bbox/` artifacts for a project into the scoped catalog.
///
/// Each JSON file under `.bbox/{kind}/*.json` is installed as a committed project
/// artifact; files under `.bbox/local/{kind}/*.json` are installed as local artifacts.
/// Content-hash idempotency makes this safe to call on every register.
pub fn discover_and_install_project_artifacts(
    project_dir: &Path,
    project_id: &str,
    catalog: &ArtifactCatalog,
) -> anyhow::Result<Vec<ArtifactMetadata>> {
    let discovered = discover_project_artifacts(project_dir)?;
    let mut results = Vec::new();
    for artifact in discovered {
        let raw = match fs::read_to_string(&artifact.path) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("discover_and_install: reading {}: {e}", artifact.path);
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("discover_and_install: parsing {}: {e}", artifact.path);
                continue;
            }
        };
        let scope = ArtifactScope::Project {
            project_id,
            local: artifact.local,
        };
        match catalog.install_value_scoped(
            scope,
            artifact.kind,
            artifact.path.clone(),
            &value,
            None,
            None,
            None,
        ) {
            Ok(meta) => results.push(meta),
            Err(e) => tracing::warn!("discover_and_install: installing {}: {e}", artifact.path),
        }
    }
    Ok(results)
}

fn scan_artifact_dir(
    scan_root: &Path,
    _bbox_root: &Path,
    local: bool,
    out: &mut Vec<DiscoveredArtifact>,
) {
    for entry in WalkDir::new(scan_root)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Skip .gitignore and non-JSON files
        if !file_name.ends_with(".json") {
            continue;
        }
        // Determine kind from the immediate parent directory name under scan_root.
        let Some(kind) = path
            .strip_prefix(scan_root)
            .ok()
            .and_then(|p| p.components().next())
            .and_then(|c| c.as_os_str().to_str())
            .and_then(artifact_kind_from_dir)
        else {
            continue;
        };
        out.push(DiscoveredArtifact {
            kind,
            path: path.to_string_lossy().into_owned(),
            local,
        });
    }
}

fn artifact_kind_from_dir(component: &str) -> Option<ArtifactKind> {
    artifact_kind_from_dir_pub(component)
}

pub fn artifact_kind_from_dir_pub(component: &str) -> Option<ArtifactKind> {
    match component {
        "workflows" => Some(ArtifactKind::Workflow),
        "packets" => Some(ArtifactKind::Packet),
        "brofiles" => Some(ArtifactKind::Brofile),
        "agents" => Some(ArtifactKind::Agent),
        "atoms" => Some(ArtifactKind::Atom),
        "teams" => Some(ArtifactKind::Team),
        "crons" => Some(ArtifactKind::Cron),
        _ => None,
    }
}

fn artifact_name(kind: ArtifactKind, value: &Value) -> Option<String> {
    match kind {
        ArtifactKind::Workflow
        | ArtifactKind::Brofile
        | ArtifactKind::Agent
        | ArtifactKind::Atom
        | ArtifactKind::Team
        | ArtifactKind::Cron => value.get("name")?.as_str().map(str::to_string),
        ArtifactKind::Packet => value.get("domain")?.as_str().map(str::to_string),
    }
}

fn artifact_version(value: &Value) -> Option<String> {
    match value.get("version")? {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn artifact_supersedes(value: &Value) -> Option<String> {
    value
        .get("supersedes")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn validate_artifact_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.starts_with('/')
        || name.contains('\\')
        || name.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.ends_with(".json")
                || part.ends_with(".metadata")
        })
    {
        bail!("invalid artifact name `{name}`");
    }
    Ok(())
}

fn validate_version_component(version: &str) -> Result<()> {
    if version.trim().is_empty()
        || version.contains('/')
        || version.contains('\\')
        || version == "."
        || version == ".."
    {
        bail!("invalid artifact version `{version}`");
    }
    Ok(())
}

fn name_path(name: &str, suffix: &str) -> Result<PathBuf> {
    validate_artifact_name(name)?;
    let mut path = PathBuf::new();
    let mut parts = name.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_some() {
            path.push(part);
        } else {
            path.push(format!("{part}.{suffix}"));
        }
    }
    Ok(path)
}

fn name_dir_path(name: &str) -> Result<PathBuf> {
    validate_artifact_name(name)?;
    let mut path = PathBuf::new();
    for part in name.split('/') {
        path.push(part);
    }
    Ok(path)
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    bbox_corpus_core::json_store::atomic_write_json_locked(path, value)
}

fn default_active() -> bool {
    true
}

fn default_true() -> bool {
    true
}

// artifact mutations run via run_blocking handlers (wave 13).
#[allow(clippy::disallowed_methods)]
fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

// artifact mutations run via run_blocking handlers (wave 13).
#[allow(clippy::disallowed_methods)]
fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Compute a stable SHA-256 hash of a JSON value with sorted object keys.
///
/// Key ordering is normalised recursively (BTreeMap), then the canonical
/// compact JSON bytes are hashed.  This means `{"b":1,"a":2}` and
/// `{"a":2,"b":1}` produce the same hash.
pub fn artifact_content_sha256(value: &Value) -> anyhow::Result<String> {
    let canonical = canonicalize_value(value);
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|e| anyhow::anyhow!("json serialization for hash: {e}"))?;
    let digest = sha2::Sha256::digest(&bytes);
    Ok(hex::encode(digest))
}

/// Recursively sort object keys so the hash is key-order-independent.
fn canonicalize_value(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize_value(v)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize_value).collect()),
        other => other.clone(),
    }
}

fn extract_agent_description(artifact_path: &Path) -> Option<String> {
    let raw = fs::read_to_string(artifact_path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("description")
        .or_else(|| value.get("manifest")?.get("description"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_list_and_supersede_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let first = serde_json::json!({
            "name": "sample-arc",
            "version": 1,
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });
        let second = serde_json::json!({
            "name": "sample-arc-v2",
            "version": 2,
            "supersedes": "sample-arc",
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });

        catalog
            .install_value(
                ArtifactKind::Workflow,
                "first.json".into(),
                &first,
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value(
                ArtifactKind::Workflow,
                "second.json".into(),
                &second,
                None,
                None,
                None,
            )
            .unwrap();

        let rows = catalog
            .list(&ArtifactListParams {
                kind: Some(ArtifactKind::Workflow),
                name: None,
                include_superseded: false,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        let old = rows.iter().find(|row| row.name == "sample-arc").unwrap();
        let new = rows.iter().find(|row| row.name == "sample-arc-v2").unwrap();
        assert!(!old.active);
        assert_eq!(old.superseded_by.as_deref(), Some("sample-arc-v2"));
        assert!(new.active);
        assert_eq!(new.supersedes_chain, vec!["sample-arc"]);

        let meta = catalog
            .supersede(ArtifactKind::Workflow, "sample-arc-v2", "sample-arc")
            .unwrap();
        assert!(!meta.active);
        assert_eq!(meta.superseded_by.as_deref(), Some("sample-arc"));
    }

    #[test]
    fn supersedes_chain_accumulates_across_three_versions() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let first = serde_json::json!({
            "name": "chain-v1",
            "version": 1,
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });
        let second = serde_json::json!({
            "name": "chain-v2",
            "version": 2,
            "supersedes": "chain-v1",
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });
        let third = serde_json::json!({
            "name": "chain-v3",
            "version": 3,
            "supersedes": "chain-v2",
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });

        catalog
            .install_value(
                ArtifactKind::Workflow,
                "first.json".into(),
                &first,
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value(
                ArtifactKind::Workflow,
                "second.json".into(),
                &second,
                None,
                None,
                None,
            )
            .unwrap();
        let meta = catalog
            .install_value(
                ArtifactKind::Workflow,
                "third.json".into(),
                &third,
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(meta.supersedes_chain, vec!["chain-v1", "chain-v2"]);
    }

    #[test]
    fn discovers_project_bbox_artifacts_without_installing() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir
            .path()
            .join(".bbox")
            .join("workflows")
            .join("custom.json");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "{}").unwrap();

        let found = discover_project_artifacts(dir.path()).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, ArtifactKind::Workflow);
    }

    #[test]
    fn project_artifact_discovery_skips_unknown_directories() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join(".bbox").join("data").join("custom.json");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "{}").unwrap();

        let found = discover_project_artifacts(dir.path()).unwrap();

        assert!(found.is_empty());
    }

    #[test]
    fn agent_install_list_and_supersede_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let agent_v1 = serde_json::json!({
            "name": "code-reviewer",
            "version": 1,
            "description": "Reviews code for bugs",
            "brofile": "sonnet-standard"
        });
        let agent_v2 = serde_json::json!({
            "name": "code-reviewer-v2",
            "version": 2,
            "supersedes": "code-reviewer",
            "description": "Reviews code for bugs and style",
            "brofile": "sonnet-standard"
        });

        catalog
            .install_value(
                ArtifactKind::Agent,
                "agent-v1.json".into(),
                &agent_v1,
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "agent-v2.json".into(),
                &agent_v2,
                None,
                None,
                None,
            )
            .unwrap();

        let rows = catalog
            .list(&ArtifactListParams {
                kind: Some(ArtifactKind::Agent),
                name: None,
                include_superseded: false,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "code-reviewer-v2");
        assert_eq!(
            rows[0].description.as_deref(),
            Some("Reviews code for bugs and style")
        );

        let all_rows = catalog
            .list(&ArtifactListParams {
                kind: Some(ArtifactKind::Agent),
                name: None,
                include_superseded: true,
            })
            .unwrap();
        assert_eq!(all_rows.len(), 2);
        let old = all_rows.iter().find(|r| r.name == "code-reviewer").unwrap();
        let new = all_rows
            .iter()
            .find(|r| r.name == "code-reviewer-v2")
            .unwrap();
        assert!(!old.active);
        assert_eq!(old.superseded_by.as_deref(), Some("code-reviewer-v2"));
        assert!(new.active);
        assert_eq!(new.supersedes_chain, vec!["code-reviewer"]);

        let meta = catalog
            .supersede(ArtifactKind::Agent, "code-reviewer-v2", "code-reviewer")
            .unwrap();
        assert!(!meta.active);
        assert_eq!(meta.superseded_by.as_deref(), Some("code-reviewer"));
    }

    #[test]
    fn agent_install_requires_name_field() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let result = catalog.install_value(
            ArtifactKind::Agent,
            "bad.json".into(),
            &serde_json::json!("not an object"),
            None,
            None,
            None,
        );
        // artifacts.rs doesn't validate object-ness; that happens in main.rs dispatch.
        // But name extraction should still fail for a bare string.
        assert!(result.is_err());
    }

    #[test]
    fn discovers_project_agent_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir
            .path()
            .join(".bbox")
            .join("agents")
            .join("reviewer.json");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "{}").unwrap();

        let found = discover_project_artifacts(dir.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, ArtifactKind::Agent);
    }

    #[test]
    fn agent_list_filters_by_kind_only() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        catalog
            .install_value(
                ArtifactKind::Agent,
                "agent.json".into(),
                &serde_json::json!({"name": "my-agent", "version": 1}),
                None,
                None,
                None,
            )
            .unwrap();

        let agent_rows = catalog
            .list(&ArtifactListParams {
                kind: Some(ArtifactKind::Agent),
                name: None,
                include_superseded: false,
            })
            .unwrap();
        assert_eq!(agent_rows.len(), 1);

        let workflow_rows = catalog
            .list(&ArtifactListParams {
                kind: Some(ArtifactKind::Workflow),
                name: None,
                include_superseded: false,
            })
            .unwrap();
        assert!(workflow_rows.is_empty());
    }

    #[test]
    fn load_artifact_value_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let agent = serde_json::json!({
            "name": "test-agent",
            "version": 2,
            "description": "A test agent.",
            "brofile_ref": "reviewer-persona"
        });
        catalog
            .install_value(
                ArtifactKind::Agent,
                "test.json".into(),
                &agent,
                None,
                None,
                None,
            )
            .unwrap();

        let value = catalog
            .load_artifact_value(ArtifactKind::Agent, "test-agent")
            .unwrap()
            .unwrap();
        assert_eq!(value["description"], "A test agent.");
        assert_eq!(value["brofile_ref"], "reviewer-persona");

        let meta = catalog
            .metadata_for(ArtifactKind::Agent, "test-agent")
            .unwrap()
            .unwrap();
        assert_eq!(meta.name, "test-agent");
        assert_eq!(meta.version, "2");
        assert!(meta.active);

        assert!(
            catalog
                .load_artifact_value(ArtifactKind::Agent, "nonexistent")
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .metadata_for(ArtifactKind::Agent, "nonexistent")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn discovery_scans_all_kind_dirs_including_local() {
        let dir = tempfile::tempdir().unwrap();
        let bbox = dir.path().join(".bbox");

        // committed
        for subdir in &[
            "brofiles",
            "workflows",
            "packets",
            "agents",
            "teams",
            "crons",
        ] {
            let d = bbox.join(subdir);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("artifact.json"), "{}").unwrap();
        }

        // local
        for subdir in &["brofiles", "agents"] {
            let d = bbox.join("local").join(subdir);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("local_artifact.json"), "{}").unwrap();
        }

        let discovered = discover_project_artifacts(dir.path()).unwrap();

        let committed_count = discovered.iter().filter(|d| !d.local).count();
        let local_count = discovered.iter().filter(|d| d.local).count();

        assert_eq!(
            committed_count, 6,
            "should find one committed artifact per kind dir (brofiles/workflows/packets/agents/teams/crons)"
        );
        assert_eq!(local_count, 2, "should find 2 local artifacts");

        // Make sure Team kind is included
        assert!(
            discovered
                .iter()
                .any(|d| d.kind == ArtifactKind::Team && !d.local)
        );
        assert!(
            discovered
                .iter()
                .any(|d| d.kind == ArtifactKind::Cron && !d.local)
        );
    }

    #[test]
    fn project_local_artifact_shadows_committed() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let committed = serde_json::json!({"name": "shadow-arc", "version": "1", "tier": "committed", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});
        let local = serde_json::json!({"name": "shadow-arc", "version": "2", "tier": "local", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                ArtifactKind::Workflow,
                "committed.json".into(),
                &committed,
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: true,
                },
                ArtifactKind::Workflow,
                "local.json".into(),
                &local,
                None,
                None,
                None,
            )
            .unwrap();

        let v = catalog
            .load_artifact_value_scoped(Some("p1"), ArtifactKind::Workflow, "shadow-arc")
            .unwrap()
            .unwrap();
        assert_eq!(v["tier"], "local", "local must shadow committed");
    }

    #[test]
    fn project_committed_artifact_shadows_global() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let global = serde_json::json!({"name": "shadow-global", "version": "1", "tier": "global", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});
        let committed = serde_json::json!({"name": "shadow-global", "version": "2", "tier": "committed", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        catalog
            .install_value(
                ArtifactKind::Workflow,
                "global.json".into(),
                &global,
                None,
                None,
                None,
            )
            .unwrap();
        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p2",
                    local: false,
                },
                ArtifactKind::Workflow,
                "committed.json".into(),
                &committed,
                None,
                None,
                None,
            )
            .unwrap();

        let v = catalog
            .load_artifact_value_scoped(Some("p2"), ArtifactKind::Workflow, "shadow-global")
            .unwrap()
            .unwrap();
        assert_eq!(v["tier"], "committed", "committed must shadow global");
    }

    #[test]
    fn global_lookup_unchanged_without_project() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let global = serde_json::json!({"name": "global-only", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});
        catalog
            .install_value(
                ArtifactKind::Workflow,
                "global.json".into(),
                &global,
                None,
                None,
                None,
            )
            .unwrap();

        // No project_id → falls through to global
        let v = catalog
            .load_artifact_value_scoped(None, ArtifactKind::Workflow, "global-only")
            .unwrap()
            .unwrap();
        assert_eq!(
            v["name"].as_str(),
            Some("global-only"),
            "global artifact must be returned"
        );
    }

    #[test]
    fn project_scoped_install_does_not_touch_global() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({"name": "proj-arc", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        let scope = ArtifactScope::Project {
            project_id: "test-project",
            local: false,
        };
        let meta = catalog
            .install_value_scoped(
                scope,
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(meta.project_id.as_deref(), Some("test-project"));
        assert!(!meta.local);

        // Global artifact must not exist.
        let global_artifact = dir
            .path()
            .join("artifacts")
            .join("workflow")
            .join("proj-arc.json");
        assert!(
            !global_artifact.exists(),
            "project install must not write global artifact"
        );

        // Scoped artifact exists.
        let scoped_artifact = dir
            .path()
            .join("artifacts")
            .join("projects")
            .join("test-project")
            .join("committed")
            .join("workflow")
            .join("proj-arc.json");
        assert!(
            scoped_artifact.exists(),
            "project artifact must be written to scoped path"
        );

        // Global lookup returns None.
        assert!(
            catalog
                .load_artifact_value(ArtifactKind::Workflow, "proj-arc")
                .unwrap()
                .is_none()
        );

        // Scoped lookup returns the value.
        let v = catalog
            .load_artifact_value_scoped(Some("test-project"), ArtifactKind::Workflow, "proj-arc")
            .unwrap();
        assert!(v.is_some());
    }

    #[test]
    fn backfill_updates_active_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({"name": "bf-arc", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        // Install, then manually strip the hash from active metadata.
        catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();
        let meta_path = dir
            .path()
            .join("artifacts")
            .join("workflow")
            .join("bf-arc")
            .join("metadata.json");
        let mut meta: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta.content_sha256 = None;
        fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

        let report = catalog.backfill_content_hashes().unwrap();
        assert_eq!(report.active_updated, 1);
        assert_eq!(report.missing_artifacts, 0);

        let after: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        assert!(after.content_sha256.is_some());
    }

    #[test]
    fn identity_removal_refuses_hashless_legacy_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({
            "name": "legacy-hashless",
            "version": "1",
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });
        catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();
        let meta_path = dir
            .path()
            .join("artifacts/workflow/legacy-hashless/metadata.json");
        let mut meta: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta.content_sha256 = None;
        fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

        let removed = catalog
            .mark_removed_by_source_if_identity(
                ArtifactScope::Global,
                ArtifactKind::Workflow,
                Path::new("src.json"),
                "legacy-hashless",
                "1",
                None,
            )
            .unwrap();

        assert!(removed.is_none());
        assert!(
            catalog
                .active_artifact_by_source(
                    ArtifactScope::Global,
                    ArtifactKind::Workflow,
                    Path::new("src.json"),
                )
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn backfill_updates_version_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({"name": "bf-ver", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();
        let version_meta_path = dir
            .path()
            .join("artifacts")
            .join("workflow")
            .join("bf-ver")
            .join(".versions")
            .join("v1.metadata.json");
        let mut meta: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&version_meta_path).unwrap()).unwrap();
        meta.content_sha256 = None;
        fs::write(
            &version_meta_path,
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();

        let report = catalog.backfill_content_hashes().unwrap();
        assert_eq!(report.version_updated, 1);

        let after: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&version_meta_path).unwrap()).unwrap();
        assert!(after.content_sha256.is_some());
    }

    #[test]
    fn backfill_skips_missing_version_payload_and_logs() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({"name": "bf-miss", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();
        let versions_dir = dir
            .path()
            .join("artifacts")
            .join("workflow")
            .join("bf-miss")
            .join(".versions");

        // Strip hash from version metadata, then delete the version artifact JSON.
        let version_meta = versions_dir.join("v1.metadata.json");
        let mut meta: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&version_meta).unwrap()).unwrap();
        meta.content_sha256 = None;
        fs::write(&version_meta, serde_json::to_string_pretty(&meta).unwrap()).unwrap();
        fs::remove_file(versions_dir.join("v1.json")).unwrap();

        let report = catalog.backfill_content_hashes().unwrap();
        assert_eq!(report.missing_artifacts, 1);
        // Metadata should still have no hash (not backfilled since payload missing).
        let after: ArtifactMetadata =
            serde_json::from_str(&fs::read_to_string(&version_meta).unwrap()).unwrap();
        assert!(after.content_sha256.is_none());
    }

    #[test]
    fn artifact_hash_is_stable_under_key_reordering() {
        let a = serde_json::json!({"b": 1, "a": 2, "c": {"z": 9, "y": 8}});
        let b = serde_json::json!({"a": 2, "c": {"y": 8, "z": 9}, "b": 1});
        let ha = artifact_content_sha256(&a).unwrap();
        let hb = artifact_content_sha256(&b).unwrap();
        assert_eq!(ha, hb, "hash must be key-order independent");
        assert_eq!(ha.len(), 64, "sha256 hex is 64 chars");
    }

    #[test]
    fn artifact_hash_changes_on_value_change() {
        let a = serde_json::json!({"name": "foo", "version": 1});
        let b = serde_json::json!({"name": "foo", "version": 2});
        let ha = artifact_content_sha256(&a).unwrap();
        let hb = artifact_content_sha256(&b).unwrap();
        assert_ne!(ha, hb);
    }

    #[test]
    fn artifact_metadata_old_json_deserializes_without_hash() {
        // JSON that has none of the new fields — must deserialize successfully
        // with Option fields as None and bool fields as false.
        let json = r#"{
            "kind": "workflow",
            "name": "old-artifact",
            "version": "1",
            "source": "file.json",
            "installed_at": "2024-01-01T00:00:00Z",
            "supersedes_chain": [],
            "active": true
        }"#;
        let meta: ArtifactMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.name, "old-artifact");
        assert!(meta.content_sha256.is_none());
        assert!(meta.project_id.is_none());
        assert!(meta.project_path.is_none());
        assert!(!meta.local);
        assert!(meta.active);
    }

    #[test]
    fn artifact_metadata_round_trip_with_new_fields() {
        let meta = ArtifactMetadata {
            kind: ArtifactKind::Workflow,
            name: "test-arc".to_string(),
            version: "2".to_string(),
            source: "src.json".to_string(),
            installed_at: "2024-01-01T00:00:00Z".to_string(),
            content_sha256: Some("abcdef1234567890".to_string()),
            project_id: Some("proj-123".to_string()),
            project_path: Some("/home/user/myproject".to_string()),
            local: true,
            supersedes: None,
            supersedes_chain: vec![],
            superseded_by: None,
            active: true,
            install_warnings: vec![],
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: ArtifactMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content_sha256.as_deref(), Some("abcdef1234567890"));
        assert_eq!(back.project_id.as_deref(), Some("proj-123"));
        assert_eq!(back.project_path.as_deref(), Some("/home/user/myproject"));
        assert!(back.local);
    }

    #[test]
    fn install_identical_artifact_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({
            "name": "idempotent-arc",
            "version": "1",
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });

        let first = catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();
        assert!(first.content_sha256.is_some());

        // Second install with same content must be a no-op: installed_at unchanged.
        let second = catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            first.installed_at, second.installed_at,
            "no-op install must not change installed_at"
        );
        assert_eq!(first.content_sha256, second.content_sha256);

        // Only one version artifact file should exist (not counting .metadata.json).
        let version_dir = dir
            .path()
            .join("artifacts")
            .join("workflow")
            .join("idempotent-arc")
            .join(".versions");
        let files: Vec<_> = std::fs::read_dir(&version_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let artifact_count = files
            .iter()
            .filter(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                s.ends_with(".json") && !s.ends_with(".metadata.json")
            })
            .count();
        assert_eq!(
            artifact_count, 1,
            "only one version artifact file after idempotent install"
        );
    }

    #[test]
    fn install_changed_artifact_preserves_supersession_chain() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let v1 = serde_json::json!({"name": "chain-arc", "version": "1", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});
        let v2 = serde_json::json!({"name": "chain-arc", "version": "2", "extra": "data", "actors": {}, "start": "Done", "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}});

        let m1 = catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &v1,
                None,
                None,
                None,
            )
            .unwrap();
        let m2 = catalog
            .install_value(
                ArtifactKind::Workflow,
                "src.json".into(),
                &v2,
                None,
                None,
                None,
            )
            .unwrap();

        // Different content → different hash
        assert_ne!(
            m1.content_sha256, m2.content_sha256,
            "hash must differ after content change"
        );
        // New install has content_sha256 set
        assert!(m2.content_sha256.is_some());
        // Version reflects the new value
        assert_eq!(m2.version, "2");
    }

    #[test]
    fn artifact_remove_marks_superseded_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        let wf_dir = project_dir.join(".bbox").join("workflows");
        std::fs::create_dir_all(&wf_dir).unwrap();
        let source_path = wf_dir.join("rem-flow.json");
        let value = serde_json::json!({"name": "rem-flow", "version": "1", "steps": []});
        std::fs::write(&source_path, serde_json::to_string(&value).unwrap()).unwrap();

        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let scope = ArtifactScope::Project {
            project_id: "proj-rem",
            local: false,
        };
        let meta = catalog
            .install_value_scoped(
                scope.clone(),
                ArtifactKind::Workflow,
                source_path.to_string_lossy().into_owned(),
                &value,
                None,
                None,
                None,
            )
            .unwrap();
        assert!(meta.active);
        assert!(meta.superseded_by.is_none());

        // Simulate file removal.
        std::fs::remove_file(&source_path).unwrap();
        let removed = catalog
            .mark_removed_by_source(scope.clone(), ArtifactKind::Workflow, &source_path)
            .unwrap();
        let removed_meta = removed.expect("should return removed metadata");
        assert!(!removed_meta.active);
        assert_eq!(removed_meta.superseded_by.as_deref(), Some("file_removed"));
        assert_eq!(removed_meta.name, "rem-flow");

        // Artifact JSON in catalog must still exist (audit trail preserved).
        let artifact_in_catalog = catalog
            .load_artifact_value_scoped(Some("proj-rem"), ArtifactKind::Workflow, "rem-flow")
            .unwrap();
        assert!(
            artifact_in_catalog.is_some(),
            "artifact JSON must not be deleted on removal"
        );

        // Calling again on the already-removed artifact returns None (not active).
        let second_remove = catalog
            .mark_removed_by_source(scope, ArtifactKind::Workflow, &source_path)
            .unwrap();
        assert!(second_remove.is_none(), "second remove should be no-op");
    }

    #[test]
    fn hard_remove_dry_run_lists_exact_paths_without_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({
            "name": "obsolete-agent",
            "version": 1,
            "description": "old",
            "brofile": "sonnet-standard"
        });
        catalog
            .install_value(
                ArtifactKind::Agent,
                "agent.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();

        let result = catalog
            .remove_hard(ArtifactKind::Agent, "obsolete-agent", true, false)
            .unwrap();

        assert!(!result.removed);
        assert!(result.dry_run);
        assert_eq!(result.paths.len(), 2);
        assert!(
            result
                .paths
                .iter()
                .any(|p| p.ends_with("agent/obsolete-agent.json"))
        );
        assert!(
            result
                .paths
                .iter()
                .any(|p| p.ends_with("agent/obsolete-agent"))
        );
        assert!(
            catalog
                .load_artifact_value(ArtifactKind::Agent, "obsolete-agent")
                .unwrap()
                .is_some()
        );
        assert!(
            catalog
                .metadata_for(ArtifactKind::Agent, "obsolete-agent")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn hard_remove_requires_confirmation_and_prunes_catalog_files() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ArtifactCatalog::open(dir.path().join("artifacts")).unwrap();
        let artifact = serde_json::json!({
            "name": "obsolete-agent",
            "version": 1,
            "description": "old",
            "brofile": "sonnet-standard"
        });
        catalog
            .install_value(
                ArtifactKind::Agent,
                "agent.json".into(),
                &artifact,
                None,
                None,
                None,
            )
            .unwrap();

        let unconfirmed = catalog.remove_hard(ArtifactKind::Agent, "obsolete-agent", false, false);
        assert!(unconfirmed.is_err());

        let result = catalog
            .remove_hard(ArtifactKind::Agent, "obsolete-agent", false, true)
            .unwrap();

        assert!(result.removed);
        assert!(!result.dry_run);
        assert!(
            catalog
                .load_artifact_value(ArtifactKind::Agent, "obsolete-agent")
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .metadata_for(ArtifactKind::Agent, "obsolete-agent")
                .unwrap()
                .is_none()
        );
        for path in result.paths {
            assert!(!std::path::Path::new(&path).exists());
        }
    }

    #[test]
    fn migration_snapshot_captures_targets_and_legacy_paths_without_creating_root() {
        use bbox_corpus_core::project_catalog_snapshot::{
            OwnerSnapshotLimitsV1, OwnerSnapshotRowValueV1, OwnerSnapshotStateV1,
        };

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("artifacts");
        let missing =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert!(matches!(
            missing.state,
            OwnerSnapshotStateV1::Missing { .. }
        ));
        assert!(!root.exists());

        let metadata_dir = root
            .join("projects")
            .join("project1")
            .join("local")
            .join("agent")
            .join("owner-test");
        std::fs::create_dir_all(&metadata_dir).unwrap();
        let metadata = ArtifactMetadata {
            kind: ArtifactKind::Agent,
            name: "owner-test".into(),
            version: "1".into(),
            source: "fixture".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            content_sha256: Some("a".repeat(64)),
            project_id: Some("project1".into()),
            project_path: Some("/repo/legacy".into()),
            local: true,
            supersedes: None,
            supersedes_chain: Vec::new(),
            superseded_by: None,
            active: true,
            install_warnings: Vec::new(),
        };
        std::fs::write(
            metadata_dir.join("metadata.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        std::fs::write(
            metadata_dir.with_extension("json"),
            serde_json::to_vec(&serde_json::json!({"name": "owner-test"})).unwrap(),
        )
        .unwrap();

        let snapshot =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert_eq!(snapshot.row_count, 1);
        assert!(snapshot.rows.iter().any(|row| matches!(
            &row.value,
            OwnerSnapshotRowValueV1::InventoryTarget { project_id, .. }
                if project_id == "project1"
        )));
        assert!(!snapshot.rows.iter().any(|row| matches!(
            &row.value,
            OwnerSnapshotRowValueV1::LegacyProjectSelector { .. }
        )));
        let retained_dir = root
            .join("projects")
            .join("project2")
            .join("local")
            .join("agent")
            .join("retained");
        std::fs::create_dir_all(&retained_dir).unwrap();
        let mut retained = metadata.clone();
        retained.name = "retained".into();
        retained.project_id = Some("project2".into());
        std::fs::write(
            retained_dir.join("metadata.json"),
            serde_json::to_vec(&retained).unwrap(),
        )
        .unwrap();

        assert_eq!(
            discharge_project_catalog_rows(&root, "project1", &["/repo/legacy".into()]).unwrap(),
            1
        );
        assert_eq!(
            discharge_project_catalog_rows(&root, "project1", &["/repo/legacy".into()]).unwrap(),
            0
        );
        let discharged =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert_eq!(discharged.row_count, 1);
        assert!(discharged.rows.iter().any(|row| matches!(
            &row.value,
            OwnerSnapshotRowValueV1::InventoryTarget { project_id, .. }
                if project_id == "project2"
        )));
    }

    #[test]
    fn artifact_retirement_recovers_payload_and_metadata_tombstone_boundaries() {
        for metadata_hidden in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let metadata_dir = root
                .join("projects")
                .join("project1")
                .join("local")
                .join("agent")
                .join("owner-test");
            fs::create_dir_all(&metadata_dir).unwrap();
            let metadata = ArtifactMetadata {
                kind: ArtifactKind::Agent,
                name: "owner-test".into(),
                version: "1".into(),
                source: "fixture".into(),
                installed_at: "2026-01-01T00:00:00Z".into(),
                content_sha256: Some("a".repeat(64)),
                project_id: Some("project1".into()),
                project_path: None,
                local: true,
                supersedes: None,
                supersedes_chain: Vec::new(),
                superseded_by: None,
                active: true,
                install_warnings: Vec::new(),
            };
            fs::write(
                metadata_dir.join("metadata.json"),
                serde_json::to_vec(&metadata).unwrap(),
            )
            .unwrap();
            let payload = metadata_dir.with_extension("json");
            fs::write(
                &payload,
                serde_json::to_vec(&serde_json::json!({"name": "owner-test"})).unwrap(),
            )
            .unwrap();
            let targets =
                capture_project_catalog_retirement_targets(&root, "project1", &[]).unwrap();
            let parent = metadata_dir.parent().unwrap();
            let payload_tombstone = parent.join(".retiring-payload-owner-test.json");
            fs::rename(&payload, &payload_tombstone).unwrap();
            if metadata_hidden {
                fs::rename(&metadata_dir, parent.join(".retiring-metadata-owner-test")).unwrap();
                assert!(
                    capture_project_catalog_retirement_targets(&root, "project1", &[])
                        .unwrap()
                        .is_empty()
                );
            } else {
                assert_eq!(
                    capture_project_catalog_retirement_targets(&root, "project1", &[]).unwrap(),
                    targets
                );
            }

            let catalog = ArtifactCatalog::open(&root).unwrap();
            assert!(
                catalog
                    .load_artifact_value_scoped(Some("project1"), ArtifactKind::Agent, "owner-test")
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                discharge_project_catalog_targets(&root, &targets).unwrap(),
                1
            );
            assert!(!payload_tombstone.exists());
            assert!(!metadata_dir.exists());
        }
    }

    #[test]
    fn artifact_retirement_resumes_after_partial_committed_tree_unlinks() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let metadata_dir = root
            .join("projects")
            .join("project1")
            .join("local")
            .join("agent")
            .join("partial");
        let versions = metadata_dir.join(".versions");
        fs::create_dir_all(&versions).unwrap();
        let metadata = ArtifactMetadata {
            kind: ArtifactKind::Agent,
            name: "partial".into(),
            version: "1".into(),
            source: "fixture".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            content_sha256: Some("a".repeat(64)),
            project_id: Some("project1".into()),
            project_path: None,
            local: true,
            supersedes: None,
            supersedes_chain: Vec::new(),
            superseded_by: None,
            active: true,
            install_warnings: Vec::new(),
        };
        let metadata_bytes = serde_json::to_vec(&metadata).unwrap();
        fs::write(metadata_dir.join("metadata.json"), &metadata_bytes).unwrap();
        fs::write(versions.join("1.metadata.json"), &metadata_bytes).unwrap();
        let payload = metadata_dir.with_extension("json");
        fs::write(&payload, br#"{"name":"partial"}"#).unwrap();
        let targets = capture_project_catalog_retirement_targets(&root, "project1", &[]).unwrap();

        let parent = metadata_dir.parent().unwrap();
        let metadata_tombstone = parent.join(".retiring-metadata-partial");
        let payload_tombstone = parent.join(".retiring-payload-partial.json");
        fs::rename(&metadata_dir, &metadata_tombstone).unwrap();
        fs::rename(&payload, &payload_tombstone).unwrap();
        fs::remove_file(metadata_tombstone.join("metadata.json")).unwrap();

        assert_eq!(
            discharge_project_catalog_targets(&root, &targets).unwrap(),
            1
        );
        assert!(!metadata_tombstone.exists());
        assert!(!payload_tombstone.exists());
        assert_eq!(
            discharge_project_catalog_targets(&root, &targets).unwrap(),
            1
        );
    }

    #[test]
    fn artifact_retirement_partial_progress_rejects_changed_survivors_and_additions() {
        for mutation in ["changed", "addition"] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let metadata_dir = root
                .join("projects")
                .join("project1")
                .join("local")
                .join("agent")
                .join("drift");
            fs::create_dir_all(&metadata_dir).unwrap();
            let metadata = ArtifactMetadata {
                kind: ArtifactKind::Agent,
                name: "drift".into(),
                version: "1".into(),
                source: "fixture".into(),
                installed_at: "2026-01-01T00:00:00Z".into(),
                content_sha256: Some("a".repeat(64)),
                project_id: Some("project1".into()),
                project_path: None,
                local: true,
                supersedes: None,
                supersedes_chain: Vec::new(),
                superseded_by: None,
                active: true,
                install_warnings: Vec::new(),
            };
            fs::write(
                metadata_dir.join("metadata.json"),
                serde_json::to_vec(&metadata).unwrap(),
            )
            .unwrap();
            let payload = metadata_dir.with_extension("json");
            fs::write(&payload, br#"{"name":"drift"}"#).unwrap();
            let targets =
                capture_project_catalog_retirement_targets(&root, "project1", &[]).unwrap();
            let parent = metadata_dir.parent().unwrap();
            let metadata_tombstone = parent.join(".retiring-metadata-drift");
            let payload_tombstone = parent.join(".retiring-payload-drift.json");
            fs::rename(&metadata_dir, &metadata_tombstone).unwrap();
            fs::rename(&payload, &payload_tombstone).unwrap();
            if mutation == "changed" {
                fs::write(metadata_tombstone.join("metadata.json"), b"changed").unwrap();
            } else {
                fs::write(metadata_tombstone.join("foreign"), b"foreign").unwrap();
            }
            assert!(discharge_project_catalog_targets(&root, &targets).is_err());
        }
    }

    #[test]
    fn artifact_writers_and_retirement_share_the_root_mutation_lock() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let catalog = ArtifactCatalog::open(&root).unwrap();
        let guard = bbox_corpus_core::json_store::acquire_store_lock_nofollow(
            &root.join(".artifact-root-mutation"),
        )
        .unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let writer = catalog.clone();
        let handle = std::thread::spawn(move || {
            let result = writer.install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: true,
                },
                ArtifactKind::Workflow,
                "fixture".into(),
                &serde_json::json!({
                    "name": "locked",
                    "version": "1",
                    "actors": {},
                    "start": "Done",
                    "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
                }),
                None,
                None,
                None,
            );
            sent.send(result.is_ok()).unwrap();
        });
        assert!(
            received
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
        drop(guard);
        assert!(
            received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
        handle.join().unwrap();

        let guard = bbox_corpus_core::json_store::acquire_store_lock_nofollow(
            &root.join(".artifact-root-mutation"),
        )
        .unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let watcher_catalog = catalog.clone();
        let handle = std::thread::spawn(move || {
            let result = watcher_catalog.mark_removed_by_source(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: true,
                },
                ArtifactKind::Workflow,
                Path::new("fixture"),
            );
            sent.send(result.unwrap().is_some()).unwrap();
        });
        assert!(
            received
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
        drop(guard);
        assert!(
            received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
        handle.join().unwrap();

        let targets = capture_project_catalog_retirement_targets(&root, "p1", &[]).unwrap();
        let guard = bbox_corpus_core::json_store::acquire_store_lock_nofollow(
            &root.join(".artifact-root-mutation"),
        )
        .unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let root_for_thread = root.clone();
        let handle = std::thread::spawn(move || {
            sent.send(discharge_project_catalog_targets(&root_for_thread, &targets).is_ok())
                .unwrap();
        });
        assert!(
            received
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
        drop(guard);
        assert!(
            received
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
        handle.join().unwrap();
    }

    #[test]
    fn artifact_retirement_target_rejects_unsafe_or_malformed_identity() {
        let valid = ArtifactRetirementTarget {
            owner_project_id: "project1".into(),
            legacy_project_path: None,
            artifact_directory: "projects/project1/local/agent/a".into(),
            metadata_path: "projects/project1/local/agent/a/metadata.json".into(),
            payload_path: "projects/project1/local/agent/a.json".into(),
            metadata_sha256: "a".repeat(64),
            version_metadata: Vec::new(),
            payload_sha256: "b".repeat(64),
            tree_manifest: vec![ArtifactMetadataCommitment {
                path: "projects/project1/local/agent/a/metadata.json".into(),
                sha256: "a".repeat(64),
            }],
        };
        assert!(valid.validate().is_ok());
        for invalid in [
            ArtifactRetirementTarget {
                artifact_directory: String::new(),
                ..valid.clone()
            },
            ArtifactRetirementTarget {
                artifact_directory: "/tmp/victim".into(),
                ..valid.clone()
            },
            ArtifactRetirementTarget {
                artifact_directory: "../victim".into(),
                ..valid.clone()
            },
            ArtifactRetirementTarget {
                metadata_sha256: "not-a-hash".into(),
                ..valid.clone()
            },
            ArtifactRetirementTarget {
                metadata_path: "other/metadata.json".into(),
                ..valid.clone()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn artifact_retirement_refuses_symlinked_intermediate_component() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("artifacts");
        let outside = directory.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(outside.join("project1/local/agent/a")).unwrap();
        fs::write(
            outside.join("project1/local/agent/a/metadata.json"),
            b"metadata",
        )
        .unwrap();
        fs::write(outside.join("project1/local/agent/a.json"), b"payload").unwrap();
        symlink(&outside, root.join("projects")).unwrap();
        let target = ArtifactRetirementTarget {
            owner_project_id: "project1".into(),
            legacy_project_path: None,
            artifact_directory: "projects/project1/local/agent/a".into(),
            metadata_path: "projects/project1/local/agent/a/metadata.json".into(),
            payload_path: "projects/project1/local/agent/a.json".into(),
            metadata_sha256: hex::encode(sha2::Sha256::digest(b"metadata")),
            version_metadata: Vec::new(),
            payload_sha256: hex::encode(sha2::Sha256::digest(b"payload")),
            tree_manifest: vec![ArtifactMetadataCommitment {
                path: "projects/project1/local/agent/a/metadata.json".into(),
                sha256: hex::encode(sha2::Sha256::digest(b"metadata")),
            }],
        };
        assert!(discharge_project_catalog_targets(&root, &[target]).is_err());
        assert!(outside.join("project1/local/agent/a.json").exists());
    }

    #[test]
    fn artifact_retirement_refuses_cross_owner_version_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let artifact = root.join("projects/project-b/local/agent/shared");
        fs::create_dir_all(artifact.join(".versions")).unwrap();
        let metadata = |owner: &str| ArtifactMetadata {
            kind: ArtifactKind::Agent,
            name: "shared".into(),
            version: "1".into(),
            source: "fixture".into(),
            installed_at: "2026-07-27T00:00:00Z".into(),
            content_sha256: Some("a".repeat(64)),
            project_id: Some(owner.into()),
            project_path: None,
            local: true,
            supersedes: None,
            supersedes_chain: Vec::new(),
            superseded_by: None,
            active: true,
            install_warnings: Vec::new(),
        };
        fs::write(
            artifact.join("metadata.json"),
            serde_json::to_vec(&metadata("project-b")).unwrap(),
        )
        .unwrap();
        fs::write(
            artifact.join(".versions/v1.metadata.json"),
            serde_json::to_vec(&metadata("project-a")).unwrap(),
        )
        .unwrap();
        fs::write(artifact.with_extension("json"), b"payload").unwrap();

        assert!(capture_project_catalog_retirement_targets(&root, "project-a", &[]).is_err());
        assert!(artifact.exists());
        assert!(artifact.with_extension("json").exists());
    }

    #[test]
    fn artifact_retirement_refuses_oversized_payload_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let artifact = root.join("projects/project-a/local/agent/large");
        fs::create_dir_all(&artifact).unwrap();
        let metadata = ArtifactMetadata {
            kind: ArtifactKind::Agent,
            name: "large".into(),
            version: "1".into(),
            source: "fixture".into(),
            installed_at: "2026-07-27T00:00:00Z".into(),
            content_sha256: Some("a".repeat(64)),
            project_id: Some("project-a".into()),
            project_path: None,
            local: true,
            supersedes: None,
            supersedes_chain: Vec::new(),
            superseded_by: None,
            active: true,
            install_warnings: Vec::new(),
        };
        fs::write(
            artifact.join("metadata.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        fs::File::create(artifact.with_extension("json"))
            .unwrap()
            .set_len(MAX_RETIREMENT_PAYLOAD_BYTES + 1)
            .unwrap();
        let error =
            capture_project_catalog_retirement_targets(&root, "project-a", &[]).unwrap_err();
        assert!(error.to_string().contains("per-file byte limit"));
    }

    #[cfg(unix)]
    #[test]
    fn artifact_recursive_delete_propagates_mid_enumeration_error() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a"), b"a").unwrap();
        fs::write(directory.path().join("b"), b"b").unwrap();
        let handle = fs::File::open(directory.path()).unwrap();
        TEST_ARTIFACT_READDIR_FAIL_AFTER.store(1, std::sync::atomic::Ordering::SeqCst);
        let result = list_artifact_directory(&handle);
        TEST_ARTIFACT_READDIR_FAIL_AFTER.store(-1, std::sync::atomic::Ordering::SeqCst);
        assert!(result.is_err());
    }

    #[test]
    fn artifact_retirement_tree_manifest_refuses_added_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let artifact = root.join("projects/project-a/local/agent/tree");
        fs::create_dir_all(&artifact).unwrap();
        let metadata = ArtifactMetadata {
            kind: ArtifactKind::Agent,
            name: "tree".into(),
            version: "1".into(),
            source: "fixture".into(),
            installed_at: "2026-07-27T00:00:00Z".into(),
            content_sha256: Some("a".repeat(64)),
            project_id: Some("project-a".into()),
            project_path: None,
            local: true,
            supersedes: None,
            supersedes_chain: Vec::new(),
            superseded_by: None,
            active: true,
            install_warnings: Vec::new(),
        };
        fs::write(
            artifact.join("metadata.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        fs::write(artifact.with_extension("json"), b"payload").unwrap();
        let targets = capture_project_catalog_retirement_targets(&root, "project-a", &[]).unwrap();
        fs::write(artifact.join("late-version.json"), b"late").unwrap();
        assert!(discharge_project_catalog_targets(&root, &targets).is_err());
        assert!(artifact.exists());
        assert!(
            !artifact
                .parent()
                .unwrap()
                .join(".retiring-metadata-tree")
                .exists()
        );
        assert!(
            !artifact
                .parent()
                .unwrap()
                .join(".retiring-payload-tree.json")
                .exists()
        );
        assert!(artifact.with_extension("json").exists());
    }
}

// ── Project-catalog row stamping (P6-B) ─────────────────────────

#[cfg(test)]
mod owner_row_stamping {
    use super::*;
    use bbox_corpus_core::project_catalog_snapshot::{
        OWNER_ROW_ABSENT, OWNER_ROW_PROJECT_ID_CONFLICT, OwnerRowStampOutcomeV1,
        OwnerSnapshotLimitsV1, stable_subsource_id,
    };

    struct Fixture {
        root: std::path::PathBuf,
        row_a: String,
        row_b: String,
        path_a: std::path::PathBuf,
        path_b: std::path::PathBuf,
    }

    fn document(selector: &str, extra: bool) -> Vec<u8> {
        let future = if extra {
            r#", "future_field": {"kept": true}"#
        } else {
            ""
        };
        format!(
            r#"{{"kind": "agent", "name": "n", "version": "1", "source": "fixture", "installed_at": "2026-01-01T00:00:00Z", "project_path": "{selector}"{future}}}
"#
        )
        .into_bytes()
    }

    /// The artifact row id is derived from the record's PATH, so the test
    /// reconstructs it exactly as capture does.
    fn row_id(relative: &str) -> String {
        format!(
            "{}:legacy-path",
            stable_subsource_id("artifact", std::path::Path::new(relative))
        )
    }

    fn write_fixture(dir: &tempfile::TempDir) -> Fixture {
        let root = dir.path().canonicalize().unwrap().join("artifacts");
        let dir_a = root.join("one");
        let dir_b = root.join("two");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let path_a = dir_a.join("metadata.json");
        let path_b = dir_b.join("metadata.json");
        std::fs::write(&path_a, document("/legacy/path/one", true)).unwrap();
        std::fs::write(&path_b, document("/legacy/path/two", false)).unwrap();
        Fixture {
            root,
            row_a: row_id("one/metadata.json"),
            row_b: row_id("two/metadata.json"),
            path_a,
            path_b,
        }
    }

    fn path_of(fixture: &Fixture, row: &str) -> std::path::PathBuf {
        if row == fixture.row_a {
            fixture.path_a.clone()
        } else {
            fixture.path_b.clone()
        }
    }

    fn read_bytes(fixture: &Fixture, row: &str) -> Vec<u8> {
        std::fs::read(path_of(fixture, row)).unwrap()
    }

    fn read_row(fixture: &Fixture, row: &str) -> serde_json::Value {
        serde_json::from_slice(&read_bytes(fixture, row)).unwrap()
    }

    fn stamp(
        fixture: &Fixture,
        row: &str,
        project_id: &str,
    ) -> std::result::Result<
        OwnerRowStampOutcomeV1,
        bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
    > {
        stamp_project_catalog_owner_row(
            &fixture.root,
            row,
            &bbox_corpus_core::project_catalog_snapshot::singleton_selector_members(row),
            project_id,
            OwnerSnapshotLimitsV1::default(),
        )
    }

    #[test]
    fn a_fresh_row_takes_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir);

        assert_eq!(
            stamp(&fixture, &fixture.row_a, "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::Stamped
        );

        let row = read_row(&fixture, &fixture.row_a);
        assert_eq!(row["project_id"], "a1b2c3d4");
        // The legacy selector is RETAINED for dual-read.
        assert_eq!(row["project_path"], "/legacy/path/one");
        // A field this binary does not model survives the write-back.
        assert_eq!(row["future_field"]["kept"], true);
        // Stamping one record must not touch its neighbours.
        assert!(
            read_row(&fixture, &fixture.row_b)
                .get("project_id")
                .is_none()
        );
    }

    /// Re-applying a torn backfill must complete, not double-write.
    #[test]
    fn restamping_the_same_id_is_an_idempotent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir);

        stamp(&fixture, &fixture.row_a, "a1b2c3d4").unwrap();
        let after_first = read_bytes(&fixture, &fixture.row_a);

        assert_eq!(
            stamp(&fixture, &fixture.row_a, "a1b2c3d4").unwrap(),
            OwnerRowStampOutcomeV1::AlreadyStamped
        );
        assert_eq!(read_bytes(&fixture, &fixture.row_a), after_first);
    }

    /// Never a silent overwrite.
    #[test]
    fn a_conflicting_id_refuses_and_leaves_the_row_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir);

        stamp(&fixture, &fixture.row_a, "a1b2c3d4").unwrap();
        let before = read_bytes(&fixture, &fixture.row_a);

        let error = stamp(&fixture, &fixture.row_a, "99998888").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_PROJECT_ID_CONFLICT);
        assert_eq!(read_row(&fixture, &fixture.row_a)["project_id"], "a1b2c3d4");
        assert_eq!(read_bytes(&fixture, &fixture.row_a), before);
    }

    /// Absence is a refusal, never a success.
    #[test]
    fn an_absent_row_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = write_fixture(&dir);

        let error = stamp(&fixture, "artifact:deadbeef:legacy-path", "a1b2c3d4").unwrap_err();
        assert_eq!(error.code, OWNER_ROW_ABSENT);
    }

    /// An absent SOURCE is likewise a refusal, and must not create it.
    #[test]
    fn an_absent_source_refuses_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("artifacts");
        let fixture = Fixture {
            row_a: row_id("one/metadata.json"),
            row_b: row_id("two/metadata.json"),
            path_a: root.join("one/metadata.json"),
            path_b: root.join("two/metadata.json"),
            root,
        };

        assert!(stamp(&fixture, &fixture.row_a, "a1b2c3d4").is_err());
        assert!(!fixture.root.exists());
    }
}
