//! Durable resumable intake for knowledge publication candidates and
//! provisional workspace source snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::ops::Bound::{Excluded, Unbounded};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{
    NofollowDirectory, StoreLockGuard, acquire_store_lock_nofollow,
};
use bbox_corpus_core::project_catalog::ProjectId;
use bbox_knowledge_source::{
    AncestryCommitV1, AncestryPageV1, BeginSourceUploadResponseV1, FinalizeSourceUploadResponseV1,
    KnowledgeSourceLimits, MissingSourceBlobsPageV1, ProvisionalWorkspaceDescriptorV1,
    ProvisionalWorkspaceStatusV1, PublicationCandidateDescriptorV1, PublicationCandidateStatusV1,
    SnapshotClassV1, SourceFileManifestEntryV1, SourceGenerationStateV1, SourceLaneV1,
    SourceManifestDescriptorV1, SourceManifestPageV1, provisional_workspace_generation_id,
    publication_candidate_generation_id, validate_ancestry_page, validate_manifest_page,
    validate_provisional_generation_id, validate_provisional_workspace,
    validate_publication_candidate, validate_publication_generation_id, validate_source_blob,
};
use bro_core::WorkspaceId;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const STORE_VERSION: u32 = 1;
const MAX_UPLOAD_RECORD_BYTES: usize = 512 * 1024;
const MAX_GENERATION_RECORD_BYTES: usize = 512 * 1024;
const MAX_MANIFEST_BYTES: usize = 512 * 1024 * 1024;
const MAX_JOURNAL_BYTES: usize = 128 * 1024;
const MISSING_PAGE_SIZE: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreLimits {
    pub contract: KnowledgeSourceLimits,
    pub max_open_uploads_per_authority: usize,
    pub upload_idle_ttl_secs: u64,
    pub max_provisional_lease_secs: u64,
    pub retained_publication_generations: usize,
    pub retained_provisional_generations: usize,
    pub unreferenced_blob_grace_secs: u64,
}

fn store_directories() -> &'static [&'static str] {
    &[
        "",
        "publications",
        "publications/uploads",
        "publications/generations",
        "publications/generation-index",
        "provisional",
        "provisional/uploads",
        "provisional/generations",
        "provisional/generation-index",
        "blobs",
        "blobs/sha256",
        "journals",
    ]
}

fn validate_store_limits(limits: StoreLimits) -> Result<()> {
    limits.contract.validate()?;
    if limits.max_open_uploads_per_authority == 0
        || limits.upload_idle_ttl_secs == 0
        || limits.max_provisional_lease_secs == 0
        || limits.retained_publication_generations == 0
        || limits.retained_provisional_generations == 0
    {
        bail!(StoreRequestError::LimitExceeded);
    }
    Ok(())
}

fn validate_publication_authority(authority: &PublicationAuthorityV1) -> Result<()> {
    validate_producer_id(&authority.producer_id)?;
    validate_project_id(&authority.project_id)?;
    authority.scope.validate()?;
    Ok(())
}

fn validate_provisional_authority(authority: &ProvisionalAuthorityV1) -> Result<()> {
    validate_project_id(&authority.project_id)?;
    authority.scope.validate()?;
    WorkspaceId::parse(authority.workspace_id.as_str())?;
    Ok(())
}

fn validate_project_id(value: &str) -> Result<()> {
    ProjectId::parse(value.to_string()).map_err(|_| anyhow!(StoreRequestError::InvalidInput))?;
    Ok(())
}

fn validate_producer_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!(StoreRequestError::InvalidInput);
    }
    Ok(())
}

fn validate_upload_id(value: &str) -> Result<()> {
    if value.len() != 32 || !is_lower_hex(value) {
        bail!(StoreRequestError::InvalidInput);
    }
    Ok(())
}

fn validate_blob_hash(value: &str) -> Result<()> {
    if value.len() != 64 || !is_lower_hex(value) {
        bail!(StoreRequestError::InvalidInput);
    }
    Ok(())
}

fn validate_publication_upload(record: &PublicationUploadV1) -> Result<()> {
    if record.version != STORE_VERSION {
        bail!(StoreRequestError::InvalidState);
    }
    validate_upload_id(&record.upload_id)?;
    validate_producer_id(&record.producer_id)?;
    validate_project_id(&record.project_id)?;
    record
        .descriptor
        .validate_header(KnowledgeSourceLimits::default())?;
    validate_publication_generation_id(&record.source_generation_id)?;
    if publication_candidate_generation_id(&record.producer_id, &record.descriptor)?
        != record.source_generation_id
    {
        bail!(StoreRequestError::InvalidState);
    }
    Ok(())
}

fn validate_provisional_upload(record: &ProvisionalUploadV1) -> Result<()> {
    if record.version != STORE_VERSION {
        bail!(StoreRequestError::InvalidState);
    }
    validate_upload_id(&record.upload_id)?;
    validate_project_id(&record.project_id)?;
    record
        .descriptor
        .validate_header(KnowledgeSourceLimits::default())?;
    validate_provisional_generation_id(&record.source_generation_id)?;
    if provisional_workspace_generation_id(&record.descriptor)? != record.source_generation_id {
        bail!(StoreRequestError::InvalidState);
    }
    Ok(())
}

fn is_open(state: SourceGenerationStateV1) -> bool {
    matches!(
        state,
        SourceGenerationStateV1::ReceivingManifest | SourceGenerationStateV1::MissingBlobs
    )
}

fn publication_page_cursors() -> BTreeMap<String, u64> {
    [
        (lane_name(SourceLaneV1::Knowledge).to_string(), 0),
        (lane_name(SourceLaneV1::Gaps).to_string(), 0),
    ]
    .into_iter()
    .collect()
}

fn provisional_page_cursors() -> BTreeMap<String, u64> {
    let mut cursors = BTreeMap::new();
    for class in [SnapshotClassV1::Baseline, SnapshotClassV1::Working] {
        for lane in [SourceLaneV1::Knowledge, SourceLaneV1::Gaps] {
            cursors.insert(provisional_slot_key(class, lane), 0);
        }
    }
    cursors
}

fn lane_name(lane: SourceLaneV1) -> &'static str {
    match lane {
        SourceLaneV1::Knowledge => "knowledge",
        SourceLaneV1::Gaps => "gaps",
    }
}

fn class_name(class: SnapshotClassV1) -> &'static str {
    match class {
        SnapshotClassV1::Baseline => "baseline",
        SnapshotClassV1::Working => "working",
    }
}

fn provisional_slot_key(class: SnapshotClassV1, lane: SourceLaneV1) -> String {
    format!("{}/{}", class_name(class), lane_name(lane))
}

fn publication_manifest_descriptor(
    descriptor: &PublicationCandidateDescriptorV1,
    lane: SourceLaneV1,
) -> &SourceManifestDescriptorV1 {
    match lane {
        SourceLaneV1::Knowledge => &descriptor.knowledge,
        SourceLaneV1::Gaps => &descriptor.gaps,
    }
}

fn provisional_manifest_descriptor(
    descriptor: &ProvisionalWorkspaceDescriptorV1,
    class: SnapshotClassV1,
    lane: SourceLaneV1,
) -> &SourceManifestDescriptorV1 {
    match (class, lane) {
        (SnapshotClassV1::Baseline, SourceLaneV1::Knowledge) => &descriptor.baseline_knowledge,
        (SnapshotClassV1::Baseline, SourceLaneV1::Gaps) => &descriptor.baseline_gaps,
        (SnapshotClassV1::Working, SourceLaneV1::Knowledge) => &descriptor.working_knowledge,
        (SnapshotClassV1::Working, SourceLaneV1::Gaps) => &descriptor.working_gaps,
    }
}

#[allow(clippy::too_many_arguments)]
fn put_manifest_page_locked(
    upload_path: &Path,
    next_pages: &mut BTreeMap<String, u64>,
    page_digests: &mut BTreeMap<String, String>,
    slot: &str,
    descriptor: &SourceManifestDescriptorV1,
    page_index: u64,
    page: &SourceManifestPageV1,
    raw: &[u8],
    limits: KnowledgeSourceLimits,
) -> Result<()> {
    if page.page_index != page_index {
        bail!(StoreRequestError::InvalidInput);
    }
    validate_manifest_page(descriptor, page, raw.len() as u64, limits)?;
    let next = next_pages
        .get_mut(slot)
        .ok_or(StoreRequestError::InvalidState)?;
    let digest = sha256(raw);
    let digest_key = format!("{slot}/{page_index:020}");
    if page_index < *next {
        if page_digests.get(&digest_key) == Some(&digest) {
            return Ok(());
        }
        bail!(StoreRequestError::Conflict);
    }
    if page_index != *next {
        bail!(StoreRequestError::InvalidInput);
    }
    let page_dir = existing_directory(&upload_path.join("pages").join(slot))?;
    page_dir.atomic_replace(&page_filename(page_index), raw)?;
    page_digests.insert(digest_key, digest);
    *next = next
        .checked_add(1)
        .ok_or(StoreRequestError::LimitExceeded)?;
    Ok(())
}

fn load_manifest_pages(
    upload_path: &Path,
    slot: &str,
    observed_pages: u64,
    expected_pages: u64,
) -> Result<Vec<SourceFileManifestEntryV1>> {
    if observed_pages != expected_pages {
        bail!(StoreRequestError::InvalidState);
    }
    let mut manifest = Vec::new();
    for page_index in 0..expected_pages {
        let page = read_json::<SourceManifestPageV1>(
            &upload_path.join("pages").join(slot),
            &page_filename(page_index),
            bbox_knowledge_source::MAX_MANIFEST_PAGE_BYTES as usize,
            "knowledge-source manifest page",
        )?
        .ok_or(StoreRequestError::InvalidState)?;
        if page.page_index != page_index {
            bail!(StoreRequestError::InvalidState);
        }
        manifest.extend(page.entries);
        if manifest.len() as u64 > bbox_knowledge_source::MAX_SOURCE_FILES_PER_LANE {
            bail!(StoreRequestError::LimitExceeded);
        }
    }
    Ok(manifest)
}

fn load_ancestry_pages(upload_path: &Path, page_count: u64) -> Result<Vec<AncestryCommitV1>> {
    let mut nodes = Vec::new();
    for page_index in 0..page_count {
        let page = read_json::<AncestryPageV1>(
            &upload_path.join("ancestry"),
            &page_filename(page_index),
            bbox_knowledge_source::MAX_ANCESTRY_PAGE_BYTES as usize,
            "knowledge-source ancestry page",
        )?
        .ok_or(StoreRequestError::InvalidState)?;
        if page.page_index != page_index {
            bail!(StoreRequestError::InvalidState);
        }
        nodes.extend(page.nodes);
        if nodes.len() as u64 > bbox_knowledge_source::MAX_ANCESTRY_NODES {
            bail!(StoreRequestError::LimitExceeded);
        }
    }
    Ok(nodes)
}

fn page_filename(page_index: u64) -> String {
    format!("{page_index:020}.json")
}

fn load_publication_manifests(
    path: &Path,
) -> Result<(
    Vec<SourceFileManifestEntryV1>,
    Vec<SourceFileManifestEntryV1>,
)> {
    Ok((
        read_required_json(path, "manifest-knowledge.json", "knowledge manifest")?,
        read_required_json(path, "manifest-gaps.json", "gap manifest")?,
    ))
}

fn load_provisional_manifests(
    path: &Path,
) -> Result<(Vec<AncestryCommitV1>, [Vec<SourceFileManifestEntryV1>; 4])> {
    Ok((
        read_required_json(path, "ancestry.json", "ancestry witness")?,
        [
            read_required_json(
                path,
                "manifest-baseline-knowledge.json",
                "baseline knowledge manifest",
            )?,
            read_required_json(path, "manifest-baseline-gaps.json", "baseline gap manifest")?,
            read_required_json(
                path,
                "manifest-working-knowledge.json",
                "working knowledge manifest",
            )?,
            read_required_json(path, "manifest-working-gaps.json", "working gap manifest")?,
        ],
    ))
}

fn load_expected_blobs(path: &Path) -> Result<BTreeMap<String, u64>> {
    let mut expected = BTreeMap::new();
    for name in [
        "manifest-knowledge.json",
        "manifest-gaps.json",
        "manifest-baseline-knowledge.json",
        "manifest-baseline-gaps.json",
        "manifest-working-knowledge.json",
        "manifest-working-gaps.json",
    ] {
        let Some(manifest) = read_json::<Vec<SourceFileManifestEntryV1>>(
            path,
            name,
            MAX_MANIFEST_BYTES,
            "knowledge-source manifest",
        )?
        else {
            continue;
        };
        for entry in manifest {
            validate_blob_hash(&entry.content_sha256)?;
            if expected
                .insert(entry.content_sha256, entry.encoded_bytes)
                .is_some_and(|prior| prior != entry.encoded_bytes)
            {
                bail!(StoreRequestError::Conflict);
            }
        }
    }
    Ok(expected)
}

fn begin_response(upload_id: String, limits: KnowledgeSourceLimits) -> BeginSourceUploadResponseV1 {
    BeginSourceUploadResponseV1 {
        upload_id,
        max_manifest_page_entries: limits.max_manifest_page_entries,
        max_manifest_page_bytes: limits.max_manifest_page_bytes,
        max_ancestry_page_nodes: limits.max_ancestry_page_nodes,
        max_ancestry_page_bytes: limits.max_ancestry_page_bytes,
        max_blob_bytes: limits.max_file_bytes,
    }
}

fn finalize_response(
    kind: FinalizeKindV1,
    source_generation_id: String,
) -> FinalizeSourceUploadResponseV1 {
    let lane = match kind {
        FinalizeKindV1::Publication => "publication",
        FinalizeKindV1::Provisional => "provisional",
    };
    FinalizeSourceUploadResponseV1 {
        status_url: format!(
            "/internal/knowledge-source/v1/{lane}/generations/{source_generation_id}/status"
        ),
        source_generation_id,
    }
}

fn publication_status(
    source: &StoredPublicationCandidateV1,
) -> Result<PublicationCandidateStatusV1> {
    source
        .descriptor
        .validate_header(KnowledgeSourceLimits::default())?;
    Ok(PublicationCandidateStatusV1 {
        source_generation_id: source.source_generation_id.clone(),
        state: source.state,
        producer_id: source.producer_id.clone(),
        full_ref: source.descriptor.full_ref.clone(),
        publisher_commit: source.descriptor.publisher_commit.clone(),
        object_format: source.descriptor.object_format,
        observed_at_unix_secs: source.created_unix_secs,
        knowledge_manifest_sha256: source.descriptor.knowledge.manifest_sha256.clone(),
        gap_manifest_sha256: source.descriptor.gaps.manifest_sha256.clone(),
        knowledge_files: source.descriptor.knowledge.file_count,
        gap_files: source.descriptor.gaps.file_count,
        logical_bytes: source
            .descriptor
            .knowledge
            .logical_bytes
            .checked_add(source.descriptor.gaps.logical_bytes)
            .ok_or(StoreRequestError::LimitExceeded)?,
        diagnostic: source.diagnostic.clone(),
    })
}

fn provisional_status(
    source: &StoredProvisionalWorkspaceV1,
) -> Result<ProvisionalWorkspaceStatusV1> {
    source
        .descriptor
        .validate_header(KnowledgeSourceLimits::default())?;
    Ok(ProvisionalWorkspaceStatusV1 {
        source_generation_id: source.source_generation_id.clone(),
        state: source.state,
        workspace_id: source.descriptor.workspace_id.clone(),
        sequence: source.descriptor.sequence,
        accepted_generation: source.descriptor.accepted_generation.clone(),
        checkout_head: source.descriptor.checkout_head.clone(),
        observed_at_unix_secs: source.created_unix_secs,
        baseline_knowledge_manifest_sha256: source
            .descriptor
            .baseline_knowledge
            .manifest_sha256
            .clone(),
        baseline_gap_manifest_sha256: source.descriptor.baseline_gaps.manifest_sha256.clone(),
        working_knowledge_manifest_sha256: source
            .descriptor
            .working_knowledge
            .manifest_sha256
            .clone(),
        working_gap_manifest_sha256: source.descriptor.working_gaps.manifest_sha256.clone(),
        lease_expires_unix_secs: Some(source.lease_expires_unix_secs),
        diagnostic: source.diagnostic.clone(),
    })
}

fn journal_filename(kind: FinalizeKindV1, generation_id: &str) -> String {
    let prefix = match kind {
        FinalizeKindV1::Publication => "publication",
        FinalizeKindV1::Provisional => "provisional",
    };
    format!("{prefix}-{generation_id}.json")
}

fn existing_directory(path: &Path) -> Result<NofollowDirectory> {
    NofollowDirectory::open_existing(path)?.ok_or_else(|| anyhow!(StoreRequestError::NotFound))
}

fn write_json<T: Serialize>(directory: &NofollowDirectory, name: &str, value: &T) -> Result<()> {
    directory.atomic_replace(name, &serde_json::to_vec_pretty(value)?)
}

fn install_immutable_json<T: Serialize>(
    directory: &NofollowDirectory,
    name: &str,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    if let Some(existing) = directory.read_regular(name, MAX_MANIFEST_BYTES, "immutable record")? {
        if existing != bytes {
            bail!(StoreRequestError::Conflict);
        }
        return Ok(());
    }
    directory.atomic_replace(name, &bytes)
}

fn read_json<T: DeserializeOwned>(
    directory: &Path,
    name: &str,
    maximum: usize,
    label: &str,
) -> Result<Option<T>> {
    let Some(directory) = NofollowDirectory::open_existing(directory)? else {
        return Ok(None);
    };
    let Some(bytes) = directory.read_regular(name, maximum, label)? else {
        return Ok(None);
    };
    Ok(Some(
        serde_json::from_slice(&bytes).with_context(|| format!("decoding {label}"))?,
    ))
}

fn read_required_json<T: DeserializeOwned>(directory: &Path, name: &str, label: &str) -> Result<T> {
    read_json(directory, name, MAX_MANIFEST_BYTES, label)?
        .ok_or_else(|| anyhow!(StoreRequestError::InvalidState))
}

fn read_child_directories(path: &Path, allowed_files: &[&str]) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!(StoreRequestError::InvalidState);
        }
        if metadata.is_dir() {
            directories.push(entry.path());
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("store member name is not UTF-8"))?;
        if !metadata.is_file() || !allowed_files.contains(&name.as_str()) {
            bail!(StoreRequestError::InvalidState);
        }
    }
    directories.sort();
    Ok(directories)
}

fn read_regular_json_files(path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("store member name is not UTF-8"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() || !name.ends_with(".json") {
            bail!(StoreRequestError::InvalidState);
        }
        files.push(entry.path());
    }
    files.sort();
    Ok(files)
}

fn file_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("store path has no UTF-8 filename"))
}

fn remove_regular_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(StoreRequestError::InvalidState);
            }
            fs::remove_file(path)?;
            sync_parent(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_upload_directory(path: &Path, provisional: bool) -> Result<()> {
    if provisional {
        remove_page_tree(&path.join("ancestry"), false)?;
        remove_page_tree(&path.join("pages"), true)?;
        for name in [
            "upload.json",
            "ancestry.json",
            "manifest-baseline-knowledge.json",
            "manifest-baseline-gaps.json",
            "manifest-working-knowledge.json",
            "manifest-working-gaps.json",
        ] {
            remove_regular_file(&path.join(name))?;
        }
    } else {
        remove_page_tree(&path.join("pages"), true)?;
        for name in [
            "upload.json",
            "manifest-knowledge.json",
            "manifest-gaps.json",
        ] {
            remove_regular_file(&path.join(name))?;
        }
    }
    remove_empty_directory(path)
}

fn remove_page_tree(path: &Path, nested: bool) -> Result<()> {
    let Some(_) = NofollowDirectory::open_existing(path)? else {
        return Ok(());
    };
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!(StoreRequestError::InvalidState);
        }
        if metadata.is_dir() && nested {
            remove_page_tree(&entry.path(), true)?;
        } else if metadata.is_file() && file_name(&entry.path())?.ends_with(".json") {
            fs::remove_file(entry.path())?;
        } else {
            bail!(StoreRequestError::InvalidState);
        }
    }
    fs::File::open(path)?.sync_all()?;
    remove_empty_directory(path)
}

fn remove_generation_directory(path: &Path, provisional: bool) -> Result<()> {
    let names: &[&str] = if provisional {
        &[
            "descriptor.json",
            "ancestry.json",
            "manifest-baseline-knowledge.json",
            "manifest-baseline-gaps.json",
            "manifest-working-knowledge.json",
            "manifest-working-gaps.json",
            "source.json",
        ]
    } else {
        &[
            "descriptor.json",
            "manifest-knowledge.json",
            "manifest-gaps.json",
            "source.json",
        ]
    };
    for name in names {
        remove_regular_file(&path.join(name))?;
    }
    remove_empty_directory(path)
}

fn remove_empty_directory(path: &Path) -> Result<()> {
    if fs::read_dir(path)?.next().transpose()?.is_some() {
        bail!(StoreRequestError::InvalidState);
    }
    fs::remove_dir(path)?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn collect_manifest_hashes(path: &Path, hashes: &mut BTreeSet<String>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry?;
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(&entry_path)?;
        if metadata.file_type().is_symlink() {
            bail!(StoreRequestError::InvalidState);
        }
        if metadata.is_dir() {
            collect_manifest_hashes(&entry_path, hashes)?;
            continue;
        }
        if !metadata.is_file() {
            bail!(StoreRequestError::InvalidState);
        }
        let name = file_name(&entry_path)?;
        if name.starts_with("manifest-") && name.ends_with(".json") {
            let bytes = fs::read(&entry_path)?;
            if bytes.len() > MAX_MANIFEST_BYTES {
                bail!(StoreRequestError::LimitExceeded);
            }
            let manifest: Vec<SourceFileManifestEntryV1> =
                serde_json::from_slice(&bytes).context("decoding stored source manifest")?;
            for record in manifest {
                validate_blob_hash(&record.content_sha256)?;
                hashes.insert(record.content_sha256);
            }
        } else if name.ends_with(".json")
            && path
                .components()
                .any(|component| component.as_os_str() == "pages")
            && !path
                .components()
                .any(|component| component.as_os_str() == "ancestry")
        {
            let bytes = fs::read(&entry_path)?;
            if bytes.len() > bbox_knowledge_source::MAX_MANIFEST_PAGE_BYTES as usize {
                bail!(StoreRequestError::LimitExceeded);
            }
            let page: SourceManifestPageV1 =
                serde_json::from_slice(&bytes).context("decoding source manifest page")?;
            for record in page.entries {
                validate_blob_hash(&record.content_sha256)?;
                hashes.insert(record.content_sha256);
            }
        }
    }
    Ok(())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn publication_source_generation_sha256(
    source: &StoredPublicationCandidateV1,
    knowledge: &[SourceFileManifestEntryV1],
    gaps: &[SourceFileManifestEntryV1],
) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-knowledge-publication-source-evidence-v1\0");
    hasher.update(serde_json::to_vec(&(source, knowledge, gaps))?);
    Ok(hex::encode(hasher.finalize()))
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            contract: KnowledgeSourceLimits::default(),
            max_open_uploads_per_authority: 2,
            upload_idle_ttl_secs: 24 * 60 * 60,
            max_provisional_lease_secs: 60 * 60,
            retained_publication_generations: 8,
            retained_provisional_generations: 2,
            unreferenced_blob_grace_secs: 7 * 24 * 60 * 60,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub expired_uploads: u64,
    pub expired_provisional_leases: u64,
    pub retired_publication_generations: u64,
    pub retired_provisional_generations: u64,
    pub deleted_blobs: u64,
    pub deleted_blob_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreRequestError {
    LimitExceeded,
    TooManyOpenUploads,
    InvalidState,
    InvalidInput,
    Conflict,
    NotFound,
}

impl std::fmt::Display for StoreRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::LimitExceeded => "knowledge-source input exceeds an enforced limit",
            Self::TooManyOpenUploads => "authority has too many open knowledge-source uploads",
            Self::InvalidState => "knowledge-source resource is not in the required state",
            Self::InvalidInput => "knowledge-source input is invalid",
            Self::Conflict => "knowledge-source evidence conflicts with durable state",
            Self::NotFound => "knowledge-source resource was not found",
        })
    }
}

impl std::error::Error for StoreRequestError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationAuthorityV1 {
    pub producer_id: String,
    pub project_id: String,
    pub scope: PublishedScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionalAuthorityV1 {
    pub project_id: String,
    pub scope: PublishedScope,
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredPublicationCandidateV1 {
    pub version: u32,
    pub source_generation_id: String,
    pub producer_id: String,
    pub project_id: String,
    pub descriptor: PublicationCandidateDescriptorV1,
    pub state: SourceGenerationStateV1,
    pub created_unix_secs: u64,
    pub created_unix_nanos: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredProvisionalWorkspaceV1 {
    pub version: u32,
    pub source_generation_id: String,
    pub project_id: String,
    pub descriptor: ProvisionalWorkspaceDescriptorV1,
    pub state: SourceGenerationStateV1,
    pub created_unix_secs: u64,
    pub created_unix_nanos: u128,
    pub lease_expires_unix_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PublicationUploadV1 {
    version: u32,
    upload_id: String,
    producer_id: String,
    project_id: String,
    descriptor: PublicationCandidateDescriptorV1,
    source_generation_id: String,
    state: SourceGenerationStateV1,
    next_pages: BTreeMap<String, u64>,
    page_digests: BTreeMap<String, String>,
    updated_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProvisionalUploadV1 {
    version: u32,
    upload_id: String,
    project_id: String,
    descriptor: ProvisionalWorkspaceDescriptorV1,
    source_generation_id: String,
    state: SourceGenerationStateV1,
    next_ancestry_page: u64,
    ancestry_page_digests: BTreeMap<u64, String>,
    next_pages: BTreeMap<String, u64>,
    page_digests: BTreeMap<String, String>,
    updated_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PublicationGenerationIndexV1 {
    version: u32,
    source_generation_id: String,
    producer_id: String,
    project_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProvisionalGenerationIndexV1 {
    version: u32,
    source_generation_id: String,
    project_id: String,
    workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProvisionalPointerV1 {
    version: u32,
    project_id: String,
    workspace_id: WorkspaceId,
    sequence: u64,
    source_generation_id: String,
    lease_expires_unix_secs: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum FinalizeKindV1 {
    Publication,
    Provisional,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FinalizeStageV1 {
    Prepared,
    GenerationInstalled,
    Committed,
    Retiring,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FinalizeJournalV1 {
    version: u32,
    kind: FinalizeKindV1,
    stage: FinalizeStageV1,
    upload_id: String,
    source_generation_id: String,
    authority_key: String,
    project_id: String,
    created_unix_secs: u64,
    created_unix_nanos: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lease_expires_unix_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prior_generation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provisional_sequence: Option<u64>,
    checksum_sha256: String,
}

impl FinalizeJournalV1 {
    fn seal(mut self) -> Result<Self> {
        self.checksum_sha256.clear();
        self.checksum_sha256 = sha256(&serde_json::to_vec(&self)?);
        Ok(self)
    }

    fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION
            || self.upload_id.is_empty()
            || self.authority_key.is_empty()
            || validate_project_id(&self.project_id).is_err()
            || self.created_unix_secs == 0
            || self.created_unix_nanos == 0
            || (self.kind == FinalizeKindV1::Publication && self.lease_expires_unix_secs.is_some())
            || (self.kind == FinalizeKindV1::Publication && self.prior_generation_id.is_some())
            || (self.kind == FinalizeKindV1::Publication && self.provisional_sequence.is_some())
            || (self.kind == FinalizeKindV1::Provisional && self.lease_expires_unix_secs.is_none())
            || (self.kind == FinalizeKindV1::Provisional && self.provisional_sequence.is_none())
        {
            bail!(StoreRequestError::InvalidState);
        }
        match self.kind {
            FinalizeKindV1::Publication => {
                validate_publication_generation_id(&self.source_generation_id)?
            }
            FinalizeKindV1::Provisional => {
                validate_provisional_generation_id(&self.source_generation_id)?
            }
        }
        if let Some(prior_generation_id) = &self.prior_generation_id {
            validate_provisional_generation_id(prior_generation_id)?;
            if prior_generation_id == &self.source_generation_id {
                bail!(StoreRequestError::InvalidState);
            }
        }
        let mut projection = self.clone();
        let checksum = std::mem::take(&mut projection.checksum_sha256);
        if checksum != sha256(&serde_json::to_vec(&projection)?) {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(())
    }
}

struct MutationGuard<'a> {
    _anchor: StoreLockGuard,
    _in_process: MutexGuard<'a, ()>,
}

pub struct KnowledgeSourceStore {
    root: PathBuf,
    limits: RwLock<StoreLimits>,
    mutation: Mutex<()>,
    publication_pins: Arc<Mutex<BTreeMap<String, usize>>>,
}

#[derive(Debug, Clone)]
pub struct ReadyPublicationFile {
    pub manifest: SourceFileManifestEntryV1,
    pub source_bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ReadyPublicationCandidate {
    pub source_generation_id: String,
    pub source_generation_sha256: String,
    pub producer_id: String,
    pub project_id: String,
    pub descriptor: PublicationCandidateDescriptorV1,
    pub observed_at_unix_secs: u64,
    pub knowledge: Vec<ReadyPublicationFile>,
    pub gaps: Vec<ReadyPublicationFile>,
}

#[derive(Debug)]
pub struct PinnedReadyPublicationCandidate {
    candidate: ReadyPublicationCandidate,
    _pin: PublicationPinGuard,
}

impl PinnedReadyPublicationCandidate {
    pub fn candidate(&self) -> &ReadyPublicationCandidate {
        &self.candidate
    }
}

#[derive(Debug)]
struct PublicationPinGuard {
    generation_id: String,
    pins: Arc<Mutex<BTreeMap<String, usize>>>,
}

impl Drop for PublicationPinGuard {
    fn drop(&mut self) {
        let Ok(mut pins) = self.pins.lock() else {
            return;
        };
        let Some(count) = pins.get_mut(&self.generation_id) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            pins.remove(&self.generation_id);
        }
    }
}

impl KnowledgeSourceStore {
    pub fn open(root: impl Into<PathBuf>, limits: StoreLimits) -> Result<Self> {
        validate_store_limits(limits)?;
        let root = root.into();
        for relative in store_directories() {
            NofollowDirectory::open_or_create(&root.join(relative))?;
        }
        let store = Self {
            root,
            limits: RwLock::new(limits),
            mutation: Mutex::new(()),
            publication_pins: Arc::new(Mutex::new(BTreeMap::new())),
        };
        store.recover()?;
        Ok(store)
    }

    pub fn open_existing(root: impl Into<PathBuf>, limits: StoreLimits) -> Result<Self> {
        validate_store_limits(limits)?;
        let root = root.into();
        for relative in store_directories() {
            NofollowDirectory::open_existing(&root.join(relative))?
                .ok_or_else(|| anyhow!("knowledge-source store member {relative} is missing"))?;
        }
        Ok(Self {
            root,
            limits: RwLock::new(limits),
            mutation: Mutex::new(()),
            publication_pins: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn update_limits(&self, limits: StoreLimits) -> Result<()> {
        validate_store_limits(limits)?;
        *self
            .limits
            .write()
            .map_err(|_| anyhow!("knowledge-source limit lock is poisoned"))? = limits;
        Ok(())
    }

    pub fn begin_publication_upload(
        &self,
        authority: &PublicationAuthorityV1,
        descriptor: PublicationCandidateDescriptorV1,
    ) -> Result<BeginSourceUploadResponseV1> {
        validate_publication_authority(authority)?;
        let limits = self.current_limits()?;
        descriptor.validate_header(limits.contract)?;
        if descriptor.scope != authority.scope {
            bail!(StoreRequestError::InvalidInput);
        }
        let generation_id =
            publication_candidate_generation_id(&authority.producer_id, &descriptor)?;
        let _guard = self.lock_mutation()?;
        let producer_root = self.publication_upload_authority_root(&authority.producer_id)?;
        let mut open = 0_usize;
        for path in read_child_directories(&producer_root, &[])? {
            let Some(record) = read_json::<PublicationUploadV1>(
                &path,
                "upload.json",
                MAX_UPLOAD_RECORD_BYTES,
                "publication upload",
            )?
            else {
                continue;
            };
            validate_publication_upload(&record)?;
            if record.producer_id == authority.producer_id
                && record.project_id == authority.project_id
                && record.descriptor == descriptor
                && is_open(record.state)
            {
                return Ok(begin_response(record.upload_id, limits.contract));
            }
            if record.producer_id == authority.producer_id && is_open(record.state) {
                open += 1;
            }
        }
        if open >= limits.max_open_uploads_per_authority {
            bail!(StoreRequestError::TooManyOpenUploads);
        }
        if self.count_open_uploads()? >= limits.contract.max_open_uploads {
            bail!(StoreRequestError::TooManyOpenUploads);
        }
        let upload_id = Uuid::new_v4().simple().to_string();
        let upload_path = producer_root.join(&upload_id);
        let upload_dir = NofollowDirectory::open_or_create(&upload_path)?;
        for lane in [SourceLaneV1::Knowledge, SourceLaneV1::Gaps] {
            NofollowDirectory::open_or_create(&upload_path.join("pages").join(lane_name(lane)))?;
        }
        write_json(
            &upload_dir,
            "upload.json",
            &PublicationUploadV1 {
                version: STORE_VERSION,
                upload_id: upload_id.clone(),
                producer_id: authority.producer_id.clone(),
                project_id: authority.project_id.clone(),
                descriptor,
                source_generation_id: generation_id,
                state: SourceGenerationStateV1::ReceivingManifest,
                next_pages: publication_page_cursors(),
                page_digests: BTreeMap::new(),
                updated_unix_secs: now_unix_secs(),
            },
        )?;
        Ok(begin_response(upload_id, limits.contract))
    }

    pub fn publication_upload_authority(
        &self,
        producer_id: &str,
        upload_id: &str,
    ) -> Result<PublicationAuthorityV1> {
        validate_producer_id(producer_id)?;
        let path = self.publication_upload_path(producer_id, upload_id)?;
        let record = read_json::<PublicationUploadV1>(
            &path,
            "upload.json",
            MAX_UPLOAD_RECORD_BYTES,
            "publication upload",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        validate_publication_upload(&record)?;
        if record.producer_id != producer_id || record.upload_id != upload_id {
            bail!(StoreRequestError::NotFound);
        }
        Ok(PublicationAuthorityV1 {
            producer_id: record.producer_id,
            project_id: record.project_id,
            scope: record.descriptor.scope,
        })
    }

    pub fn put_publication_manifest_page(
        &self,
        authority: &PublicationAuthorityV1,
        upload_id: &str,
        lane: SourceLaneV1,
        page_index: u64,
        page: &SourceManifestPageV1,
    ) -> Result<()> {
        validate_publication_authority(authority)?;
        let limits = self.current_limits()?;
        let raw = serde_json::to_vec(page)?;
        let _guard = self.lock_mutation()?;
        let path = self.publication_upload_path(&authority.producer_id, upload_id)?;
        let mut record = self.load_publication_upload(&path, authority, upload_id)?;
        if record.state != SourceGenerationStateV1::ReceivingManifest {
            bail!(StoreRequestError::InvalidState);
        }
        let descriptor = publication_manifest_descriptor(&record.descriptor, lane);
        put_manifest_page_locked(
            &path,
            &mut record.next_pages,
            &mut record.page_digests,
            lane_name(lane),
            descriptor,
            page_index,
            page,
            &raw,
            limits.contract,
        )?;
        record.updated_unix_secs = now_unix_secs();
        let directory = existing_directory(&path)?;
        write_json(&directory, "upload.json", &record)
    }

    pub fn missing_publication_blobs(
        &self,
        authority: &PublicationAuthorityV1,
        upload_id: &str,
        cursor: Option<&str>,
    ) -> Result<MissingSourceBlobsPageV1> {
        validate_publication_authority(authority)?;
        let _guard = self.lock_mutation()?;
        let path = self.publication_upload_path(&authority.producer_id, upload_id)?;
        let mut record = self.load_publication_upload(&path, authority, upload_id)?;
        if record.state == SourceGenerationStateV1::ReceivingManifest {
            self.complete_publication_manifest_locked(&path, &mut record)?;
        }
        if record.state != SourceGenerationStateV1::MissingBlobs {
            bail!(StoreRequestError::InvalidState);
        }
        self.missing_blobs_for_upload(&path, &record.source_generation_id, cursor)
    }

    pub fn install_publication_blob(
        &self,
        authority: &PublicationAuthorityV1,
        upload_id: &str,
        hash: &str,
        expected_size: u64,
        reader: impl Read,
    ) -> Result<()> {
        validate_publication_authority(authority)?;
        let _guard = self.lock_mutation()?;
        let path = self.publication_upload_path(&authority.producer_id, upload_id)?;
        let mut record = self.load_publication_upload(&path, authority, upload_id)?;
        if record.state != SourceGenerationStateV1::MissingBlobs {
            bail!(StoreRequestError::InvalidState);
        }
        self.install_upload_blob(&path, hash, expected_size, reader)?;
        record.updated_unix_secs = now_unix_secs();
        write_json(&existing_directory(&path)?, "upload.json", &record)
    }

    pub fn expected_publication_blob_size(
        &self,
        authority: &PublicationAuthorityV1,
        upload_id: &str,
        hash: &str,
    ) -> Result<u64> {
        validate_publication_authority(authority)?;
        validate_blob_hash(hash)?;
        let path = self.publication_upload_path(&authority.producer_id, upload_id)?;
        let record = self.load_publication_upload(&path, authority, upload_id)?;
        if record.state != SourceGenerationStateV1::MissingBlobs {
            bail!(StoreRequestError::InvalidState);
        }
        load_expected_blobs(&path)?
            .get(hash)
            .copied()
            .ok_or_else(|| anyhow!(StoreRequestError::NotFound))
    }

    pub fn finalize_publication_upload(
        &self,
        authority: &PublicationAuthorityV1,
        upload_id: &str,
    ) -> Result<FinalizeSourceUploadResponseV1> {
        validate_publication_authority(authority)?;
        let _guard = self.lock_mutation()?;
        self.finalize_publication_locked(authority, upload_id, None)
    }

    pub fn publication_status(
        &self,
        producer_id: &str,
        generation_id: &str,
    ) -> Result<PublicationCandidateStatusV1> {
        validate_producer_id(producer_id)?;
        validate_publication_generation_id(generation_id)?;
        let index = read_json::<PublicationGenerationIndexV1>(
            &self.root.join("publications/generation-index"),
            &format!("{generation_id}.json"),
            MAX_GENERATION_RECORD_BYTES,
            "publication generation index",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        if index.version != STORE_VERSION
            || index.source_generation_id != generation_id
            || index.producer_id != producer_id
        {
            bail!(StoreRequestError::NotFound);
        }
        let source = self.load_publication_generation(&index.project_id, generation_id)?;
        publication_status(&source)
    }

    pub fn publication_generation_authority(
        &self,
        producer_id: &str,
        generation_id: &str,
    ) -> Result<PublicationAuthorityV1> {
        validate_producer_id(producer_id)?;
        validate_publication_generation_id(generation_id)?;
        let index = read_json::<PublicationGenerationIndexV1>(
            &self.root.join("publications/generation-index"),
            &format!("{generation_id}.json"),
            MAX_GENERATION_RECORD_BYTES,
            "publication generation index",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        if index.version != STORE_VERSION
            || index.source_generation_id != generation_id
            || index.producer_id != producer_id
        {
            bail!(StoreRequestError::NotFound);
        }
        let source = self.load_publication_generation(&index.project_id, generation_id)?;
        if source.producer_id != producer_id {
            bail!(StoreRequestError::NotFound);
        }
        Ok(PublicationAuthorityV1 {
            producer_id: producer_id.to_string(),
            project_id: index.project_id,
            scope: source.descriptor.scope,
        })
    }

    pub fn pin_ready_publication_candidate(
        &self,
        generation_id: &str,
    ) -> Result<PinnedReadyPublicationCandidate> {
        validate_publication_generation_id(generation_id)?;
        let _guard = self.lock_mutation()?;
        let index = read_json::<PublicationGenerationIndexV1>(
            &self.root.join("publications/generation-index"),
            &format!("{generation_id}.json"),
            MAX_GENERATION_RECORD_BYTES,
            "publication generation index",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        if index.version != STORE_VERSION || index.source_generation_id != generation_id {
            bail!(StoreRequestError::InvalidState);
        }
        let source = self.load_publication_generation(&index.project_id, generation_id)?;
        if source.state != SourceGenerationStateV1::Ready
            || source.source_generation_id != generation_id
            || source.producer_id != index.producer_id
            || source.project_id != index.project_id
        {
            bail!(StoreRequestError::InvalidState);
        }
        let generation_path = self.publication_generation_path(&index.project_id, generation_id)?;
        let (knowledge_manifest, gap_manifest) = load_publication_manifests(&generation_path)?;
        validate_publication_candidate(
            &source.descriptor,
            &knowledge_manifest,
            &gap_manifest,
            self.current_limits()?.contract,
        )?;
        let knowledge = self.materialize_ready_publication_files(&knowledge_manifest)?;
        let gaps = self.materialize_ready_publication_files(&gap_manifest)?;
        let source_generation_sha256 =
            publication_source_generation_sha256(&source, &knowledge_manifest, &gap_manifest)?;
        let mut pins = self
            .publication_pins
            .lock()
            .map_err(|_| anyhow!(StoreRequestError::InvalidState))?;
        *pins.entry(generation_id.to_string()).or_insert(0) += 1;
        drop(pins);
        Ok(PinnedReadyPublicationCandidate {
            candidate: ReadyPublicationCandidate {
                source_generation_id: generation_id.to_string(),
                source_generation_sha256,
                producer_id: source.producer_id,
                project_id: source.project_id,
                descriptor: source.descriptor,
                observed_at_unix_secs: source.created_unix_secs,
                knowledge,
                gaps,
            },
            _pin: PublicationPinGuard {
                generation_id: generation_id.to_string(),
                pins: Arc::clone(&self.publication_pins),
            },
        })
    }

    pub fn probe_publication(
        &self,
        authority: &PublicationAuthorityV1,
        full_ref: &str,
        publisher_commit: &str,
        object_format: bbox_knowledge_source::GitObjectFormatV1,
    ) -> Result<Option<PublicationCandidateStatusV1>> {
        validate_publication_authority(authority)?;
        for path in read_regular_json_files(&self.root.join("publications/generation-index"))? {
            let index = read_json::<PublicationGenerationIndexV1>(
                &self.root.join("publications/generation-index"),
                &file_name(&path)?,
                MAX_GENERATION_RECORD_BYTES,
                "publication generation index",
            )?
            .ok_or(StoreRequestError::InvalidState)?;
            if index.version != STORE_VERSION
                || index.producer_id != authority.producer_id
                || index.project_id != authority.project_id
            {
                continue;
            }
            let source =
                self.load_publication_generation(&index.project_id, &index.source_generation_id)?;
            if source.state == SourceGenerationStateV1::Ready
                && source.descriptor.scope == authority.scope
                && source.descriptor.full_ref == full_ref
                && source.descriptor.publisher_commit == publisher_commit
                && source.descriptor.object_format == object_format
            {
                return publication_status(&source).map(Some);
            }
        }
        Ok(None)
    }

    pub fn begin_provisional_upload(
        &self,
        authority: &ProvisionalAuthorityV1,
        descriptor: ProvisionalWorkspaceDescriptorV1,
    ) -> Result<BeginSourceUploadResponseV1> {
        validate_provisional_authority(authority)?;
        let limits = self.current_limits()?;
        descriptor.validate_header(limits.contract)?;
        if descriptor.scope != authority.scope || descriptor.workspace_id != authority.workspace_id
        {
            bail!(StoreRequestError::InvalidInput);
        }
        let generation_id = provisional_workspace_generation_id(&descriptor)?;
        let _guard = self.lock_mutation()?;
        self.refuse_stale_or_conflicting_sequence(authority, &descriptor, &generation_id)?;
        let workspace_root = self.provisional_upload_authority_root(&authority.workspace_id)?;
        let mut open = 0_usize;
        for path in read_child_directories(&workspace_root, &[])? {
            let Some(record) = read_json::<ProvisionalUploadV1>(
                &path,
                "upload.json",
                MAX_UPLOAD_RECORD_BYTES,
                "provisional upload",
            )?
            else {
                continue;
            };
            validate_provisional_upload(&record)?;
            if record.project_id == authority.project_id
                && record.descriptor == descriptor
                && is_open(record.state)
            {
                return Ok(begin_response(record.upload_id, limits.contract));
            }
            if record.descriptor.sequence == descriptor.sequence
                && record.source_generation_id != generation_id
                && is_open(record.state)
            {
                bail!(StoreRequestError::Conflict);
            }
            if is_open(record.state) {
                open += 1;
            }
        }
        if open >= limits.max_open_uploads_per_authority {
            bail!(StoreRequestError::TooManyOpenUploads);
        }
        if self.count_open_uploads()? >= limits.contract.max_open_uploads {
            bail!(StoreRequestError::TooManyOpenUploads);
        }
        let upload_id = Uuid::new_v4().simple().to_string();
        let path = workspace_root.join(&upload_id);
        let directory = NofollowDirectory::open_or_create(&path)?;
        NofollowDirectory::open_or_create(&path.join("ancestry"))?;
        for class in [SnapshotClassV1::Baseline, SnapshotClassV1::Working] {
            for lane in [SourceLaneV1::Knowledge, SourceLaneV1::Gaps] {
                NofollowDirectory::open_or_create(
                    &path
                        .join("pages")
                        .join(class_name(class))
                        .join(lane_name(lane)),
                )?;
            }
        }
        write_json(
            &directory,
            "upload.json",
            &ProvisionalUploadV1 {
                version: STORE_VERSION,
                upload_id: upload_id.clone(),
                project_id: authority.project_id.clone(),
                descriptor,
                source_generation_id: generation_id,
                state: SourceGenerationStateV1::ReceivingManifest,
                next_ancestry_page: 0,
                ancestry_page_digests: BTreeMap::new(),
                next_pages: provisional_page_cursors(),
                page_digests: BTreeMap::new(),
                updated_unix_secs: now_unix_secs(),
            },
        )?;
        Ok(begin_response(upload_id, limits.contract))
    }

    pub fn put_provisional_ancestry_page(
        &self,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
        page_index: u64,
        page: &AncestryPageV1,
    ) -> Result<()> {
        validate_provisional_authority(authority)?;
        let limits = self.current_limits()?;
        let raw = serde_json::to_vec(page)?;
        let digest = sha256(&raw);
        let _guard = self.lock_mutation()?;
        let path = self.provisional_upload_path(&authority.workspace_id, upload_id)?;
        let mut record = self.load_provisional_upload(&path, authority, upload_id)?;
        if record.state != SourceGenerationStateV1::ReceivingManifest {
            bail!(StoreRequestError::InvalidState);
        }
        if page.page_index != page_index {
            bail!(StoreRequestError::InvalidInput);
        }
        validate_ancestry_page(
            &record.descriptor.ancestry,
            page,
            raw.len() as u64,
            limits.contract,
        )?;
        if page_index < record.next_ancestry_page {
            if record.ancestry_page_digests.get(&page_index) == Some(&digest) {
                return Ok(());
            }
            bail!(StoreRequestError::Conflict);
        }
        if page_index != record.next_ancestry_page {
            bail!(StoreRequestError::InvalidInput);
        }
        existing_directory(&path.join("ancestry"))?
            .atomic_replace(&page_filename(page_index), &raw)?;
        record.ancestry_page_digests.insert(page_index, digest);
        record.next_ancestry_page = record
            .next_ancestry_page
            .checked_add(1)
            .ok_or(StoreRequestError::LimitExceeded)?;
        record.updated_unix_secs = now_unix_secs();
        write_json(&existing_directory(&path)?, "upload.json", &record)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn put_provisional_manifest_page(
        &self,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
        class: SnapshotClassV1,
        lane: SourceLaneV1,
        page_index: u64,
        page: &SourceManifestPageV1,
    ) -> Result<()> {
        validate_provisional_authority(authority)?;
        let limits = self.current_limits()?;
        let raw = serde_json::to_vec(page)?;
        let _guard = self.lock_mutation()?;
        let path = self.provisional_upload_path(&authority.workspace_id, upload_id)?;
        let mut record = self.load_provisional_upload(&path, authority, upload_id)?;
        if record.state != SourceGenerationStateV1::ReceivingManifest {
            bail!(StoreRequestError::InvalidState);
        }
        let descriptor = provisional_manifest_descriptor(&record.descriptor, class, lane);
        let key = provisional_slot_key(class, lane);
        put_manifest_page_locked(
            &path,
            &mut record.next_pages,
            &mut record.page_digests,
            &key,
            descriptor,
            page_index,
            page,
            &raw,
            limits.contract,
        )?;
        record.updated_unix_secs = now_unix_secs();
        write_json(&existing_directory(&path)?, "upload.json", &record)
    }

    pub fn missing_provisional_blobs(
        &self,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
        cursor: Option<&str>,
    ) -> Result<MissingSourceBlobsPageV1> {
        validate_provisional_authority(authority)?;
        let _guard = self.lock_mutation()?;
        let path = self.provisional_upload_path(&authority.workspace_id, upload_id)?;
        let mut record = self.load_provisional_upload(&path, authority, upload_id)?;
        if record.state == SourceGenerationStateV1::ReceivingManifest {
            self.complete_provisional_manifest_locked(&path, &mut record)?;
        }
        if record.state != SourceGenerationStateV1::MissingBlobs {
            bail!(StoreRequestError::InvalidState);
        }
        self.missing_blobs_for_upload(&path, &record.source_generation_id, cursor)
    }

    pub fn install_provisional_blob(
        &self,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
        hash: &str,
        expected_size: u64,
        reader: impl Read,
    ) -> Result<()> {
        validate_provisional_authority(authority)?;
        let _guard = self.lock_mutation()?;
        let path = self.provisional_upload_path(&authority.workspace_id, upload_id)?;
        let mut record = self.load_provisional_upload(&path, authority, upload_id)?;
        if record.state != SourceGenerationStateV1::MissingBlobs {
            bail!(StoreRequestError::InvalidState);
        }
        self.install_upload_blob(&path, hash, expected_size, reader)?;
        record.updated_unix_secs = now_unix_secs();
        write_json(&existing_directory(&path)?, "upload.json", &record)
    }

    pub fn expected_provisional_blob_size(
        &self,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
        hash: &str,
    ) -> Result<u64> {
        validate_provisional_authority(authority)?;
        validate_blob_hash(hash)?;
        let path = self.provisional_upload_path(&authority.workspace_id, upload_id)?;
        let record = self.load_provisional_upload(&path, authority, upload_id)?;
        if record.state != SourceGenerationStateV1::MissingBlobs {
            bail!(StoreRequestError::InvalidState);
        }
        load_expected_blobs(&path)?
            .get(hash)
            .copied()
            .ok_or_else(|| anyhow!(StoreRequestError::NotFound))
    }

    pub fn finalize_provisional_upload(
        &self,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
        lease_ttl_secs: u64,
    ) -> Result<FinalizeSourceUploadResponseV1> {
        validate_provisional_authority(authority)?;
        let limits = self.current_limits()?;
        if lease_ttl_secs == 0 || lease_ttl_secs > limits.max_provisional_lease_secs {
            bail!(StoreRequestError::LimitExceeded);
        }
        let lease_expires = now_unix_secs()
            .checked_add(lease_ttl_secs)
            .ok_or(StoreRequestError::LimitExceeded)?;
        let _guard = self.lock_mutation()?;
        self.finalize_provisional_locked(authority, upload_id, lease_expires, None)
    }

    pub fn provisional_status(
        &self,
        authority: &ProvisionalAuthorityV1,
        generation_id: &str,
    ) -> Result<ProvisionalWorkspaceStatusV1> {
        validate_provisional_authority(authority)?;
        validate_provisional_generation_id(generation_id)?;
        let index = read_json::<ProvisionalGenerationIndexV1>(
            &self.root.join("provisional/generation-index"),
            &format!("{generation_id}.json"),
            MAX_GENERATION_RECORD_BYTES,
            "provisional generation index",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        if index.version != STORE_VERSION
            || index.source_generation_id != generation_id
            || index.project_id != authority.project_id
            || index.workspace_id != authority.workspace_id
        {
            bail!(StoreRequestError::NotFound);
        }
        let mut source = self.load_provisional_generation(
            &authority.project_id,
            &authority.workspace_id,
            generation_id,
        )?;
        if source.state == SourceGenerationStateV1::Ready
            && let Some(pointer) = self.load_provisional_pointer(authority)?
            && pointer.source_generation_id == generation_id
        {
            source.lease_expires_unix_secs = pointer.lease_expires_unix_secs;
        }
        provisional_status(&source)
    }

    pub fn selected_provisional(
        &self,
        authority: &ProvisionalAuthorityV1,
        now: u64,
    ) -> Result<Option<StoredProvisionalWorkspaceV1>> {
        validate_provisional_authority(authority)?;
        let Some(pointer) = self.load_provisional_pointer(authority)? else {
            return Ok(None);
        };
        if pointer.lease_expires_unix_secs <= now {
            return Ok(None);
        }
        let mut source = self.load_provisional_generation(
            &authority.project_id,
            &authority.workspace_id,
            &pointer.source_generation_id,
        )?;
        if source.state != SourceGenerationStateV1::Ready {
            bail!(StoreRequestError::InvalidState);
        }
        source.lease_expires_unix_secs = pointer.lease_expires_unix_secs;
        Ok(Some(source))
    }

    pub fn probe_provisional(
        &self,
        authority: &ProvisionalAuthorityV1,
        now: u64,
    ) -> Result<Option<ProvisionalWorkspaceStatusV1>> {
        self.selected_provisional(authority, now)?
            .as_ref()
            .map(provisional_status)
            .transpose()
    }

    pub fn renew_provisional(
        &self,
        authority: &ProvisionalAuthorityV1,
        generation_id: &str,
        lease_ttl_secs: u64,
    ) -> Result<ProvisionalWorkspaceStatusV1> {
        validate_provisional_authority(authority)?;
        validate_provisional_generation_id(generation_id)?;
        let limits = self.current_limits()?;
        if lease_ttl_secs == 0 || lease_ttl_secs > limits.max_provisional_lease_secs {
            bail!(StoreRequestError::LimitExceeded);
        }
        let expires = now_unix_secs()
            .checked_add(lease_ttl_secs)
            .ok_or(StoreRequestError::LimitExceeded)?;
        let _guard = self.lock_mutation()?;
        let pointer = self
            .load_provisional_pointer(authority)?
            .ok_or(StoreRequestError::NotFound)?;
        if pointer.source_generation_id != generation_id {
            bail!(StoreRequestError::Conflict);
        }
        let source = self.load_provisional_generation(
            &authority.project_id,
            &authority.workspace_id,
            generation_id,
        )?;
        if source.state != SourceGenerationStateV1::Ready {
            bail!(StoreRequestError::InvalidState);
        }
        self.write_provisional_pointer(
            authority,
            ProvisionalPointerV1 {
                lease_expires_unix_secs: expires,
                ..pointer
            },
        )?;
        provisional_status(&StoredProvisionalWorkspaceV1 {
            lease_expires_unix_secs: expires,
            ..source
        })
    }

    pub fn retire_provisional(
        &self,
        authority: &ProvisionalAuthorityV1,
        generation_id: &str,
    ) -> Result<()> {
        validate_provisional_authority(authority)?;
        validate_provisional_generation_id(generation_id)?;
        let _guard = self.lock_mutation()?;
        if let Some(pointer) = self.load_provisional_pointer(authority)? {
            if pointer.source_generation_id != generation_id {
                bail!(StoreRequestError::Conflict);
            }
            remove_regular_file(&self.provisional_pointer_path(authority)?)?;
        }
        let mut source = self.load_provisional_generation(
            &authority.project_id,
            &authority.workspace_id,
            generation_id,
        )?;
        match source.state {
            SourceGenerationStateV1::Ready => {
                source.state = SourceGenerationStateV1::Retired;
                self.write_provisional_source(&source)
            }
            SourceGenerationStateV1::Retired => Ok(()),
            _ => bail!(StoreRequestError::InvalidState),
        }
    }

    pub fn recover(&self) -> Result<()> {
        let _guard = self.lock_mutation()?;
        for journal_path in read_regular_json_files(&self.root.join("journals"))? {
            let name = journal_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow!("journal filename is not UTF-8"))?;
            let journal = read_json::<FinalizeJournalV1>(
                &self.root.join("journals"),
                name,
                MAX_JOURNAL_BYTES,
                "knowledge-source finalize journal",
            )?
            .ok_or_else(|| anyhow!("finalize journal disappeared"))?;
            journal.validate()?;
            if journal.stage == FinalizeStageV1::Retiring {
                self.complete_retiring_generation(&journal)?;
                continue;
            }
            match journal.kind {
                FinalizeKindV1::Publication => {
                    let upload_id = journal.upload_id.clone();
                    let path = self.publication_upload_path(&journal.authority_key, &upload_id)?;
                    let upload = read_json::<PublicationUploadV1>(
                        &path,
                        "upload.json",
                        MAX_UPLOAD_RECORD_BYTES,
                        "publication upload",
                    )?
                    .ok_or(StoreRequestError::NotFound)?;
                    let authority = PublicationAuthorityV1 {
                        producer_id: upload.producer_id.clone(),
                        project_id: upload.project_id.clone(),
                        scope: upload.descriptor.scope.clone(),
                    };
                    if journal.authority_key != authority.producer_id
                        || journal.project_id != authority.project_id
                    {
                        bail!(StoreRequestError::InvalidState);
                    }
                    if journal.stage == FinalizeStageV1::Committed {
                        if upload.state != SourceGenerationStateV1::Ready {
                            bail!(StoreRequestError::InvalidState);
                        }
                        self.verify_ready_publication(&authority, &upload)?;
                        continue;
                    }
                    self.finalize_publication_locked(&authority, &upload_id, Some(journal))?;
                }
                FinalizeKindV1::Provisional => {
                    let upload_id = journal.upload_id.clone();
                    let workspace = WorkspaceId::parse(journal.authority_key.clone())?;
                    let path = self.provisional_upload_path(&workspace, &upload_id)?;
                    let upload = read_json::<ProvisionalUploadV1>(
                        &path,
                        "upload.json",
                        MAX_UPLOAD_RECORD_BYTES,
                        "provisional upload",
                    )?
                    .ok_or(StoreRequestError::NotFound)?;
                    let authority = ProvisionalAuthorityV1 {
                        project_id: upload.project_id.clone(),
                        scope: upload.descriptor.scope.clone(),
                        workspace_id: upload.descriptor.workspace_id.clone(),
                    };
                    if journal.authority_key != authority.workspace_id.as_str()
                        || journal.project_id != authority.project_id
                        || journal.provisional_sequence != Some(upload.descriptor.sequence)
                    {
                        bail!(StoreRequestError::InvalidState);
                    }
                    if journal.stage == FinalizeStageV1::Committed {
                        if upload.state != SourceGenerationStateV1::Ready {
                            bail!(StoreRequestError::InvalidState);
                        }
                        self.verify_finalized_provisional(&authority, &upload)?;
                        continue;
                    }
                    self.finalize_provisional_locked(
                        &authority,
                        &upload_id,
                        journal
                            .lease_expires_unix_secs
                            .ok_or(StoreRequestError::InvalidState)?,
                        Some(journal),
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn maintain(
        &self,
        protected_publication_generations: &BTreeSet<String>,
    ) -> Result<MaintenanceReport> {
        self.maintain_at(protected_publication_generations, now_unix_secs())
    }

    pub fn maintain_at(
        &self,
        protected_publication_generations: &BTreeSet<String>,
        now: u64,
    ) -> Result<MaintenanceReport> {
        let _guard = self.lock_mutation()?;
        self.maintain_locked(protected_publication_generations, now)
    }

    fn complete_publication_manifest_locked(
        &self,
        path: &Path,
        record: &mut PublicationUploadV1,
    ) -> Result<()> {
        let knowledge = load_manifest_pages(
            path,
            lane_name(SourceLaneV1::Knowledge),
            record.next_pages[lane_name(SourceLaneV1::Knowledge)],
            record.descriptor.knowledge.page_count,
        )?;
        let gaps = load_manifest_pages(
            path,
            lane_name(SourceLaneV1::Gaps),
            record.next_pages[lane_name(SourceLaneV1::Gaps)],
            record.descriptor.gaps.page_count,
        )?;
        validate_publication_candidate(
            &record.descriptor,
            &knowledge,
            &gaps,
            self.current_limits()?.contract,
        )?;
        let directory = existing_directory(path)?;
        install_immutable_json(&directory, "manifest-knowledge.json", &knowledge)?;
        install_immutable_json(&directory, "manifest-gaps.json", &gaps)?;
        record.state = SourceGenerationStateV1::MissingBlobs;
        record.updated_unix_secs = now_unix_secs();
        write_json(&directory, "upload.json", record)
    }

    fn complete_provisional_manifest_locked(
        &self,
        path: &Path,
        record: &mut ProvisionalUploadV1,
    ) -> Result<()> {
        if record.next_ancestry_page != record.descriptor.ancestry.page_count {
            bail!(StoreRequestError::InvalidState);
        }
        let ancestry = load_ancestry_pages(path, record.next_ancestry_page)?;
        let baseline_knowledge = load_manifest_pages(
            path,
            &provisional_slot_key(SnapshotClassV1::Baseline, SourceLaneV1::Knowledge),
            record.next_pages
                [&provisional_slot_key(SnapshotClassV1::Baseline, SourceLaneV1::Knowledge)],
            record.descriptor.baseline_knowledge.page_count,
        )?;
        let baseline_gaps = load_manifest_pages(
            path,
            &provisional_slot_key(SnapshotClassV1::Baseline, SourceLaneV1::Gaps),
            record.next_pages[&provisional_slot_key(SnapshotClassV1::Baseline, SourceLaneV1::Gaps)],
            record.descriptor.baseline_gaps.page_count,
        )?;
        let working_knowledge = load_manifest_pages(
            path,
            &provisional_slot_key(SnapshotClassV1::Working, SourceLaneV1::Knowledge),
            record.next_pages
                [&provisional_slot_key(SnapshotClassV1::Working, SourceLaneV1::Knowledge)],
            record.descriptor.working_knowledge.page_count,
        )?;
        let working_gaps = load_manifest_pages(
            path,
            &provisional_slot_key(SnapshotClassV1::Working, SourceLaneV1::Gaps),
            record.next_pages[&provisional_slot_key(SnapshotClassV1::Working, SourceLaneV1::Gaps)],
            record.descriptor.working_gaps.page_count,
        )?;
        validate_provisional_workspace(
            &record.descriptor,
            &ancestry,
            &baseline_knowledge,
            &baseline_gaps,
            &working_knowledge,
            &working_gaps,
            self.current_limits()?.contract,
        )?;
        let directory = existing_directory(path)?;
        install_immutable_json(&directory, "ancestry.json", &ancestry)?;
        for (name, manifest) in [
            ("manifest-baseline-knowledge.json", baseline_knowledge),
            ("manifest-baseline-gaps.json", baseline_gaps),
            ("manifest-working-knowledge.json", working_knowledge),
            ("manifest-working-gaps.json", working_gaps),
        ] {
            install_immutable_json(&directory, name, &manifest)?;
        }
        record.state = SourceGenerationStateV1::MissingBlobs;
        record.updated_unix_secs = now_unix_secs();
        write_json(&directory, "upload.json", record)
    }

    fn missing_blobs_for_upload(
        &self,
        upload_path: &Path,
        generation_id: &str,
        cursor: Option<&str>,
    ) -> Result<MissingSourceBlobsPageV1> {
        let expected = load_expected_blobs(upload_path)?;
        let mut missing = Vec::new();
        let range = match cursor {
            Some(cursor) => expected.range::<str, _>((Excluded(cursor), Unbounded)),
            None => expected.range::<str, _>((Unbounded, Unbounded)),
        };
        let mut has_more = false;
        for (hash, size) in range {
            match self.read_blob(hash, *size as usize)? {
                Some(_) => {}
                None => {
                    if missing.len() == MISSING_PAGE_SIZE {
                        has_more = true;
                        break;
                    }
                    missing.push(hash.clone());
                }
            }
        }
        let next_cursor = has_more.then(|| {
            missing
                .last()
                .expect("a full missing-blob page has a final item")
                .clone()
        });
        Ok(MissingSourceBlobsPageV1 {
            source_generation_id: generation_id.to_string(),
            hashes: missing,
            next_cursor,
        })
    }

    fn install_upload_blob(
        &self,
        upload_path: &Path,
        hash: &str,
        expected_size: u64,
        mut reader: impl Read,
    ) -> Result<()> {
        let expected = load_expected_blobs(upload_path)?;
        let size = expected.get(hash).ok_or(StoreRequestError::NotFound)?;
        if *size != expected_size || expected_size > self.current_limits()?.contract.max_file_bytes
        {
            bail!(StoreRequestError::InvalidInput);
        }
        let mut bytes = Vec::with_capacity(expected_size as usize);
        reader
            .by_ref()
            .take(expected_size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        let entry = SourceFileManifestEntryV1 {
            repository_relative_filename: "blob-validation.json".to_string(),
            encoded_bytes: expected_size,
            content_sha256: hash.to_string(),
        };
        validate_source_blob(&entry, &bytes, self.current_limits()?.contract)?;
        self.install_blob_bytes(hash, &bytes)
    }

    fn finalize_publication_locked(
        &self,
        authority: &PublicationAuthorityV1,
        upload_id: &str,
        recovered: Option<FinalizeJournalV1>,
    ) -> Result<FinalizeSourceUploadResponseV1> {
        let path = self.publication_upload_path(&authority.producer_id, upload_id)?;
        let mut upload = self.load_publication_upload(&path, authority, upload_id)?;
        let recovering = recovered.is_some();
        if upload.state == SourceGenerationStateV1::Ready && !recovering {
            self.verify_ready_publication(authority, &upload)?;
            return Ok(finalize_response(
                FinalizeKindV1::Publication,
                upload.source_generation_id,
            ));
        }
        if upload.state != SourceGenerationStateV1::MissingBlobs
            && !(recovering && upload.state == SourceGenerationStateV1::Ready)
        {
            bail!(StoreRequestError::InvalidState);
        }
        self.verify_all_upload_blobs(&path)?;
        let mut journal = match recovered {
            Some(journal) => {
                if journal.kind != FinalizeKindV1::Publication
                    || journal.upload_id != upload_id
                    || journal.source_generation_id != upload.source_generation_id
                    || journal.authority_key != authority.producer_id
                    || journal.project_id != authority.project_id
                {
                    bail!(StoreRequestError::InvalidState);
                }
                journal
            }
            None => self.write_finalize_journal(FinalizeJournalV1 {
                version: STORE_VERSION,
                kind: FinalizeKindV1::Publication,
                stage: FinalizeStageV1::Prepared,
                upload_id: upload_id.to_string(),
                source_generation_id: upload.source_generation_id.clone(),
                authority_key: authority.producer_id.clone(),
                project_id: authority.project_id.clone(),
                created_unix_secs: now_unix_secs(),
                created_unix_nanos: now_unix_nanos(),
                lease_expires_unix_secs: None,
                prior_generation_id: None,
                provisional_sequence: None,
                checksum_sha256: String::new(),
            })?,
        };
        let manifests = load_publication_manifests(&path)?;
        let generation_path =
            self.publication_generation_path(&authority.project_id, &upload.source_generation_id)?;
        let generation = StoredPublicationCandidateV1 {
            version: STORE_VERSION,
            source_generation_id: upload.source_generation_id.clone(),
            producer_id: authority.producer_id.clone(),
            project_id: authority.project_id.clone(),
            descriptor: upload.descriptor.clone(),
            state: SourceGenerationStateV1::Ready,
            created_unix_secs: journal.created_unix_secs,
            created_unix_nanos: journal.created_unix_nanos,
            diagnostic: None,
        };
        let directory = NofollowDirectory::open_or_create(&generation_path)?;
        install_immutable_json(&directory, "descriptor.json", &upload.descriptor)?;
        install_immutable_json(&directory, "manifest-knowledge.json", &manifests.0)?;
        install_immutable_json(&directory, "manifest-gaps.json", &manifests.1)?;
        install_immutable_json(&directory, "source.json", &generation)?;
        journal.stage = FinalizeStageV1::GenerationInstalled;
        journal = self.write_finalize_journal(journal)?;

        let index_dir = existing_directory(&self.root.join("publications/generation-index"))?;
        install_immutable_json(
            &index_dir,
            &format!("{}.json", upload.source_generation_id),
            &PublicationGenerationIndexV1 {
                version: STORE_VERSION,
                source_generation_id: upload.source_generation_id.clone(),
                producer_id: authority.producer_id.clone(),
                project_id: authority.project_id.clone(),
            },
        )?;
        if upload.state != SourceGenerationStateV1::Ready {
            upload.state = SourceGenerationStateV1::Ready;
            upload.updated_unix_secs = now_unix_secs();
            write_json(&existing_directory(&path)?, "upload.json", &upload)?;
        }
        journal.stage = FinalizeStageV1::Committed;
        self.write_finalize_journal(journal)?;
        Ok(finalize_response(
            FinalizeKindV1::Publication,
            upload.source_generation_id,
        ))
    }

    fn finalize_provisional_locked(
        &self,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
        lease_expires: u64,
        recovered: Option<FinalizeJournalV1>,
    ) -> Result<FinalizeSourceUploadResponseV1> {
        let path = self.provisional_upload_path(&authority.workspace_id, upload_id)?;
        let mut upload = self.load_provisional_upload(&path, authority, upload_id)?;
        let recovering = recovered.is_some();
        if upload.state == SourceGenerationStateV1::Ready && !recovering {
            self.verify_finalized_provisional(authority, &upload)?;
            return Ok(finalize_response(
                FinalizeKindV1::Provisional,
                upload.source_generation_id,
            ));
        }
        if upload.state != SourceGenerationStateV1::MissingBlobs
            && !(recovering && upload.state == SourceGenerationStateV1::Ready)
        {
            bail!(StoreRequestError::InvalidState);
        }
        self.verify_all_upload_blobs(&path)?;
        self.refuse_stale_or_conflicting_sequence(
            authority,
            &upload.descriptor,
            &upload.source_generation_id,
        )?;
        let prior_generation_id = self
            .load_provisional_pointer(authority)?
            .filter(|pointer| pointer.source_generation_id != upload.source_generation_id)
            .map(|pointer| pointer.source_generation_id);
        let mut journal = match recovered {
            Some(journal) => {
                if journal.kind != FinalizeKindV1::Provisional
                    || journal.upload_id != upload_id
                    || journal.source_generation_id != upload.source_generation_id
                    || journal.authority_key != authority.workspace_id.as_str()
                    || journal.project_id != authority.project_id
                    || journal.lease_expires_unix_secs != Some(lease_expires)
                    || journal.provisional_sequence != Some(upload.descriptor.sequence)
                {
                    bail!(StoreRequestError::InvalidState);
                }
                journal
            }
            None => self.write_finalize_journal(FinalizeJournalV1 {
                version: STORE_VERSION,
                kind: FinalizeKindV1::Provisional,
                stage: FinalizeStageV1::Prepared,
                upload_id: upload_id.to_string(),
                source_generation_id: upload.source_generation_id.clone(),
                authority_key: authority.workspace_id.to_string(),
                project_id: authority.project_id.clone(),
                created_unix_secs: now_unix_secs(),
                created_unix_nanos: now_unix_nanos(),
                lease_expires_unix_secs: Some(lease_expires),
                prior_generation_id,
                provisional_sequence: Some(upload.descriptor.sequence),
                checksum_sha256: String::new(),
            })?,
        };
        let (ancestry, manifests) = load_provisional_manifests(&path)?;
        let generation_path = self.provisional_generation_path(
            &authority.project_id,
            &authority.workspace_id,
            &upload.source_generation_id,
        )?;
        let generation = StoredProvisionalWorkspaceV1 {
            version: STORE_VERSION,
            source_generation_id: upload.source_generation_id.clone(),
            project_id: authority.project_id.clone(),
            descriptor: upload.descriptor.clone(),
            state: SourceGenerationStateV1::Ready,
            created_unix_secs: journal.created_unix_secs,
            created_unix_nanos: journal.created_unix_nanos,
            lease_expires_unix_secs: lease_expires,
            diagnostic: None,
        };
        let directory = NofollowDirectory::open_or_create(&generation_path)?;
        install_immutable_json(&directory, "descriptor.json", &upload.descriptor)?;
        install_immutable_json(&directory, "ancestry.json", &ancestry)?;
        for (name, manifest) in [
            ("manifest-baseline-knowledge.json", &manifests[0]),
            ("manifest-baseline-gaps.json", &manifests[1]),
            ("manifest-working-knowledge.json", &manifests[2]),
            ("manifest-working-gaps.json", &manifests[3]),
        ] {
            install_immutable_json(&directory, name, manifest)?;
        }
        install_immutable_json(&directory, "source.json", &generation)?;
        journal.stage = FinalizeStageV1::GenerationInstalled;
        journal = self.write_finalize_journal(journal)?;

        let index_dir = existing_directory(&self.root.join("provisional/generation-index"))?;
        install_immutable_json(
            &index_dir,
            &format!("{}.json", upload.source_generation_id),
            &ProvisionalGenerationIndexV1 {
                version: STORE_VERSION,
                source_generation_id: upload.source_generation_id.clone(),
                project_id: authority.project_id.clone(),
                workspace_id: authority.workspace_id.clone(),
            },
        )?;
        let sequences = NofollowDirectory::open_or_create(
            &self
                .provisional_workspace_root(&authority.project_id, &authority.workspace_id)?
                .join("sequences"),
        )?;
        install_immutable_json(
            &sequences,
            &format!("{:020}.json", upload.descriptor.sequence),
            &ProvisionalPointerV1 {
                version: STORE_VERSION,
                project_id: authority.project_id.clone(),
                workspace_id: authority.workspace_id.clone(),
                sequence: upload.descriptor.sequence,
                source_generation_id: upload.source_generation_id.clone(),
                lease_expires_unix_secs: lease_expires,
            },
        )?;
        self.write_provisional_pointer(
            authority,
            ProvisionalPointerV1 {
                version: STORE_VERSION,
                project_id: authority.project_id.clone(),
                workspace_id: authority.workspace_id.clone(),
                sequence: upload.descriptor.sequence,
                source_generation_id: upload.source_generation_id.clone(),
                lease_expires_unix_secs: lease_expires,
            },
        )?;
        if let Some(prior_generation_id) = &journal.prior_generation_id {
            let mut prior = self.load_provisional_generation(
                &authority.project_id,
                &authority.workspace_id,
                prior_generation_id,
            )?;
            prior.state = SourceGenerationStateV1::Superseded;
            self.write_provisional_source(&prior)?;
        }
        if upload.state != SourceGenerationStateV1::Ready {
            upload.state = SourceGenerationStateV1::Ready;
            upload.updated_unix_secs = now_unix_secs();
            write_json(&existing_directory(&path)?, "upload.json", &upload)?;
        }
        journal.stage = FinalizeStageV1::Committed;
        self.write_finalize_journal(journal)?;
        Ok(finalize_response(
            FinalizeKindV1::Provisional,
            upload.source_generation_id,
        ))
    }

    fn refuse_stale_or_conflicting_sequence(
        &self,
        authority: &ProvisionalAuthorityV1,
        descriptor: &ProvisionalWorkspaceDescriptorV1,
        generation_id: &str,
    ) -> Result<()> {
        if let Some(pointer) = self.load_provisional_pointer(authority)? {
            if pointer.sequence > descriptor.sequence {
                bail!(StoreRequestError::InvalidState);
            }
            if pointer.sequence == descriptor.sequence
                && pointer.source_generation_id != generation_id
            {
                bail!(StoreRequestError::Conflict);
            }
        }
        let sequence_path = self
            .provisional_workspace_root(&authority.project_id, &authority.workspace_id)?
            .join("sequences");
        if let Some(sequence) = read_json::<ProvisionalPointerV1>(
            &sequence_path,
            &format!("{:020}.json", descriptor.sequence),
            MAX_GENERATION_RECORD_BYTES,
            "provisional sequence",
        )? && sequence.source_generation_id != generation_id
        {
            bail!(StoreRequestError::Conflict);
        }
        Ok(())
    }

    fn verify_all_upload_blobs(&self, upload_path: &Path) -> Result<()> {
        for (hash, size) in load_expected_blobs(upload_path)? {
            if self.read_blob(&hash, size as usize)?.is_none() {
                bail!(StoreRequestError::InvalidState);
            }
        }
        Ok(())
    }

    fn verify_ready_publication(
        &self,
        authority: &PublicationAuthorityV1,
        upload: &PublicationUploadV1,
    ) -> Result<()> {
        let source =
            self.load_publication_generation(&authority.project_id, &upload.source_generation_id)?;
        if source.state != SourceGenerationStateV1::Ready
            || source.producer_id != authority.producer_id
            || source.descriptor != upload.descriptor
        {
            bail!(StoreRequestError::InvalidState);
        }
        let index = read_json::<PublicationGenerationIndexV1>(
            &self.root.join("publications/generation-index"),
            &format!("{}.json", upload.source_generation_id),
            MAX_GENERATION_RECORD_BYTES,
            "publication generation index",
        )?
        .ok_or(StoreRequestError::InvalidState)?;
        if index.version != STORE_VERSION
            || index.source_generation_id != upload.source_generation_id
            || index.producer_id != authority.producer_id
            || index.project_id != authority.project_id
        {
            bail!(StoreRequestError::InvalidState);
        }
        let generation_path =
            self.publication_generation_path(&authority.project_id, &upload.source_generation_id)?;
        let manifests = load_publication_manifests(&generation_path)?;
        validate_publication_candidate(
            &source.descriptor,
            &manifests.0,
            &manifests.1,
            self.current_limits()?.contract,
        )?;
        self.verify_all_upload_blobs(&generation_path)
    }

    fn verify_finalized_provisional(
        &self,
        authority: &ProvisionalAuthorityV1,
        upload: &ProvisionalUploadV1,
    ) -> Result<()> {
        let source = self.load_provisional_generation(
            &authority.project_id,
            &authority.workspace_id,
            &upload.source_generation_id,
        )?;
        if source.descriptor != upload.descriptor
            || matches!(
                source.state,
                SourceGenerationStateV1::ReceivingManifest
                    | SourceGenerationStateV1::MissingBlobs
                    | SourceGenerationStateV1::Failed
            )
        {
            bail!(StoreRequestError::InvalidState);
        }
        let index = read_json::<ProvisionalGenerationIndexV1>(
            &self.root.join("provisional/generation-index"),
            &format!("{}.json", upload.source_generation_id),
            MAX_GENERATION_RECORD_BYTES,
            "provisional generation index",
        )?
        .ok_or(StoreRequestError::InvalidState)?;
        if index.version != STORE_VERSION
            || index.source_generation_id != upload.source_generation_id
            || index.project_id != authority.project_id
            || index.workspace_id != authority.workspace_id
        {
            bail!(StoreRequestError::InvalidState);
        }
        let generation_path = self.provisional_generation_path(
            &authority.project_id,
            &authority.workspace_id,
            &upload.source_generation_id,
        )?;
        let (ancestry, manifests) = load_provisional_manifests(&generation_path)?;
        validate_provisional_workspace(
            &source.descriptor,
            &ancestry,
            &manifests[0],
            &manifests[1],
            &manifests[2],
            &manifests[3],
            self.current_limits()?.contract,
        )?;
        self.verify_all_upload_blobs(&generation_path)
    }

    fn install_blob_bytes(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        validate_blob_hash(hash)?;
        let directory =
            NofollowDirectory::open_or_create(&self.root.join("blobs/sha256").join(&hash[..2]))?;
        let name = &hash[2..];
        if let Some(existing) = directory.read_regular(
            name,
            self.current_limits()?.contract.max_file_bytes as usize,
            "knowledge-source blob",
        )? {
            if existing != bytes || sha256(&existing) != hash {
                bail!(StoreRequestError::Conflict);
            }
            return Ok(());
        }
        directory.atomic_replace(name, bytes)
    }

    fn materialize_ready_publication_files(
        &self,
        manifest: &[SourceFileManifestEntryV1],
    ) -> Result<Vec<ReadyPublicationFile>> {
        manifest
            .iter()
            .map(|entry| {
                let maximum = usize::try_from(entry.encoded_bytes)
                    .map_err(|_| anyhow!(StoreRequestError::LimitExceeded))?;
                let source_bytes = self
                    .read_blob(&entry.content_sha256, maximum)?
                    .ok_or(StoreRequestError::InvalidState)?;
                if source_bytes.len() != maximum {
                    bail!(StoreRequestError::InvalidState);
                }
                Ok(ReadyPublicationFile {
                    manifest: entry.clone(),
                    source_bytes,
                })
            })
            .collect()
    }

    fn read_blob(&self, hash: &str, maximum: usize) -> Result<Option<Vec<u8>>> {
        validate_blob_hash(hash)?;
        let Some(directory) =
            NofollowDirectory::open_existing(&self.root.join("blobs/sha256").join(&hash[..2]))?
        else {
            return Ok(None);
        };
        let Some(bytes) = directory.read_regular(&hash[2..], maximum, "knowledge-source blob")?
        else {
            return Ok(None);
        };
        if bytes.len() > maximum || sha256(&bytes) != hash {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(Some(bytes))
    }

    fn write_finalize_journal(&self, journal: FinalizeJournalV1) -> Result<FinalizeJournalV1> {
        let journal = journal.seal()?;
        journal.validate()?;
        write_json(
            &existing_directory(&self.root.join("journals"))?,
            &journal_filename(journal.kind, &journal.source_generation_id),
            &journal,
        )?;
        Ok(journal)
    }

    fn load_publication_upload(
        &self,
        path: &Path,
        authority: &PublicationAuthorityV1,
        upload_id: &str,
    ) -> Result<PublicationUploadV1> {
        let record = read_json::<PublicationUploadV1>(
            path,
            "upload.json",
            MAX_UPLOAD_RECORD_BYTES,
            "publication upload",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        validate_publication_upload(&record)?;
        if record.upload_id != upload_id
            || record.producer_id != authority.producer_id
            || record.project_id != authority.project_id
            || record.descriptor.scope != authority.scope
        {
            bail!(StoreRequestError::NotFound);
        }
        Ok(record)
    }

    fn load_provisional_upload(
        &self,
        path: &Path,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
    ) -> Result<ProvisionalUploadV1> {
        let record = read_json::<ProvisionalUploadV1>(
            path,
            "upload.json",
            MAX_UPLOAD_RECORD_BYTES,
            "provisional upload",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        validate_provisional_upload(&record)?;
        if record.upload_id != upload_id
            || record.project_id != authority.project_id
            || record.descriptor.scope != authority.scope
            || record.descriptor.workspace_id != authority.workspace_id
        {
            bail!(StoreRequestError::NotFound);
        }
        Ok(record)
    }

    fn load_publication_generation(
        &self,
        project_id: &str,
        generation_id: &str,
    ) -> Result<StoredPublicationCandidateV1> {
        let path = self.publication_generation_path(project_id, generation_id)?;
        let source = read_json::<StoredPublicationCandidateV1>(
            &path,
            "source.json",
            MAX_GENERATION_RECORD_BYTES,
            "publication generation",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        if source.version != STORE_VERSION
            || source.project_id != project_id
            || source.source_generation_id != generation_id
        {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(source)
    }

    fn load_provisional_generation(
        &self,
        project_id: &str,
        workspace_id: &WorkspaceId,
        generation_id: &str,
    ) -> Result<StoredProvisionalWorkspaceV1> {
        let path = self.provisional_generation_path(project_id, workspace_id, generation_id)?;
        let source = read_json::<StoredProvisionalWorkspaceV1>(
            &path,
            "source.json",
            MAX_GENERATION_RECORD_BYTES,
            "provisional generation",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        if source.version != STORE_VERSION
            || source.project_id != project_id
            || source.descriptor.workspace_id != *workspace_id
            || source.source_generation_id != generation_id
        {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(source)
    }

    fn write_provisional_source(&self, source: &StoredProvisionalWorkspaceV1) -> Result<()> {
        let path = self.provisional_generation_path(
            &source.project_id,
            &source.descriptor.workspace_id,
            &source.source_generation_id,
        )?;
        write_json(&existing_directory(&path)?, "source.json", source)
    }

    fn load_provisional_pointer(
        &self,
        authority: &ProvisionalAuthorityV1,
    ) -> Result<Option<ProvisionalPointerV1>> {
        let path = self.provisional_pointer_path(authority)?;
        let Some(parent) = path.parent() else {
            bail!(StoreRequestError::InvalidState);
        };
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            bail!(StoreRequestError::InvalidState);
        };
        let Some(pointer) = read_json::<ProvisionalPointerV1>(
            parent,
            name,
            MAX_GENERATION_RECORD_BYTES,
            "provisional pointer",
        )?
        else {
            return Ok(None);
        };
        if pointer.version != STORE_VERSION
            || pointer.project_id != authority.project_id
            || pointer.workspace_id != authority.workspace_id
        {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(Some(pointer))
    }

    fn write_provisional_pointer(
        &self,
        authority: &ProvisionalAuthorityV1,
        pointer: ProvisionalPointerV1,
    ) -> Result<()> {
        if pointer.project_id != authority.project_id
            || pointer.workspace_id != authority.workspace_id
        {
            bail!(StoreRequestError::InvalidInput);
        }
        let root =
            self.provisional_workspace_root(&authority.project_id, &authority.workspace_id)?;
        write_json(
            &NofollowDirectory::open_or_create(&root)?,
            "current.json",
            &pointer,
        )
    }

    fn publication_upload_authority_root(&self, producer_id: &str) -> Result<PathBuf> {
        validate_producer_id(producer_id)?;
        let path = self.root.join("publications/uploads").join(producer_id);
        NofollowDirectory::open_or_create(&path)?;
        Ok(path)
    }

    fn publication_upload_path(&self, producer_id: &str, upload_id: &str) -> Result<PathBuf> {
        validate_producer_id(producer_id)?;
        validate_upload_id(upload_id)?;
        Ok(self
            .root
            .join("publications/uploads")
            .join(producer_id)
            .join(upload_id))
    }

    fn provisional_upload_authority_root(&self, workspace_id: &WorkspaceId) -> Result<PathBuf> {
        let path = self
            .root
            .join("provisional/uploads")
            .join(workspace_id.as_str());
        NofollowDirectory::open_or_create(&path)?;
        Ok(path)
    }

    fn provisional_upload_path(
        &self,
        workspace_id: &WorkspaceId,
        upload_id: &str,
    ) -> Result<PathBuf> {
        validate_upload_id(upload_id)?;
        Ok(self
            .root
            .join("provisional/uploads")
            .join(workspace_id.as_str())
            .join(upload_id))
    }

    fn publication_generation_path(
        &self,
        project_id: &str,
        generation_id: &str,
    ) -> Result<PathBuf> {
        validate_project_id(project_id)?;
        validate_publication_generation_id(generation_id)?;
        Ok(self
            .root
            .join("publications/generations")
            .join(project_id)
            .join(generation_id))
    }

    fn provisional_workspace_root(
        &self,
        project_id: &str,
        workspace_id: &WorkspaceId,
    ) -> Result<PathBuf> {
        validate_project_id(project_id)?;
        Ok(self
            .root
            .join("provisional/generations")
            .join(project_id)
            .join(workspace_id.as_str()))
    }

    fn provisional_generation_path(
        &self,
        project_id: &str,
        workspace_id: &WorkspaceId,
        generation_id: &str,
    ) -> Result<PathBuf> {
        validate_provisional_generation_id(generation_id)?;
        Ok(self
            .provisional_workspace_root(project_id, workspace_id)?
            .join(generation_id))
    }

    fn provisional_pointer_path(&self, authority: &ProvisionalAuthorityV1) -> Result<PathBuf> {
        Ok(self
            .provisional_workspace_root(&authority.project_id, &authority.workspace_id)?
            .join("current.json"))
    }

    fn lock_mutation(&self) -> Result<MutationGuard<'_>> {
        let in_process = self
            .mutation
            .lock()
            .map_err(|_| anyhow!("knowledge-source mutation lock is poisoned"))?;
        let anchor = acquire_store_lock_nofollow(&self.root.join("store.json.lock"))?;
        Ok(MutationGuard {
            _anchor: anchor,
            _in_process: in_process,
        })
    }

    fn current_limits(&self) -> Result<StoreLimits> {
        self.limits
            .read()
            .map(|limits| *limits)
            .map_err(|_| anyhow!("knowledge-source limit lock is poisoned"))
    }

    fn count_open_uploads(&self) -> Result<u64> {
        let mut open = 0_u64;
        for producer in read_child_directories(&self.root.join("publications/uploads"), &[])? {
            validate_producer_id(&file_name(&producer)?)?;
            for upload_path in read_child_directories(&producer, &[])? {
                let upload = read_json::<PublicationUploadV1>(
                    &upload_path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "publication upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                validate_publication_upload(&upload)?;
                open = open.saturating_add(is_open(upload.state) as u64);
            }
        }
        for workspace in read_child_directories(&self.root.join("provisional/uploads"), &[])? {
            WorkspaceId::parse(file_name(&workspace)?)?;
            for upload_path in read_child_directories(&workspace, &[])? {
                let upload = read_json::<ProvisionalUploadV1>(
                    &upload_path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "provisional upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                validate_provisional_upload(&upload)?;
                open = open.saturating_add(is_open(upload.state) as u64);
            }
        }
        Ok(open)
    }

    fn maintain_locked(
        &self,
        protected_publication_generations: &BTreeSet<String>,
        now: u64,
    ) -> Result<MaintenanceReport> {
        let mut protected_publication_generations = protected_publication_generations.clone();
        protected_publication_generations.extend(
            self.publication_pins
                .lock()
                .map_err(|_| anyhow!(StoreRequestError::InvalidState))?
                .keys()
                .cloned(),
        );
        for generation in &protected_publication_generations {
            validate_publication_generation_id(generation)?;
        }
        let (resumed_publication_retirements, resumed_provisional_retirements) =
            self.resume_retiring_generations()?;
        let expired_uploads = self.expire_uploads(now)?;
        let expired_provisional_leases = self.expire_provisional_leases(now)?;
        let (new_publication_retirements, new_provisional_retirements) =
            self.retire_old_generations(&protected_publication_generations)?;
        let retired_publication_generations =
            resumed_publication_retirements.saturating_add(new_publication_retirements);
        let retired_provisional_generations =
            resumed_provisional_retirements.saturating_add(new_provisional_retirements);
        let referenced = self.referenced_blob_hashes()?;
        let (deleted_blobs, deleted_blob_bytes) =
            self.sweep_unreferenced_blobs(&referenced, now)?;
        Ok(MaintenanceReport {
            expired_uploads,
            expired_provisional_leases,
            retired_publication_generations,
            retired_provisional_generations,
            deleted_blobs,
            deleted_blob_bytes,
        })
    }

    fn resume_retiring_generations(&self) -> Result<(u64, u64)> {
        let mut publications = 0_u64;
        let mut provisionals = 0_u64;
        for path in read_regular_json_files(&self.root.join("journals"))? {
            let journal = read_json::<FinalizeJournalV1>(
                &self.root.join("journals"),
                &file_name(&path)?,
                MAX_JOURNAL_BYTES,
                "knowledge-source finalize journal",
            )?
            .ok_or(StoreRequestError::InvalidState)?;
            journal.validate()?;
            if journal.stage != FinalizeStageV1::Retiring {
                continue;
            }
            self.complete_retiring_generation(&journal)?;
            match journal.kind {
                FinalizeKindV1::Publication => publications = publications.saturating_add(1),
                FinalizeKindV1::Provisional => provisionals = provisionals.saturating_add(1),
            }
        }
        Ok((publications, provisionals))
    }

    fn expire_uploads(&self, now: u64) -> Result<u64> {
        let limits = self.current_limits()?;
        let protected = self.unfinished_journal_uploads()?;
        let mut expired = 0_u64;
        for producer in read_child_directories(&self.root.join("publications/uploads"), &[])? {
            let producer_name = file_name(&producer)?;
            validate_producer_id(&producer_name)?;
            for upload_path in read_child_directories(&producer, &[])? {
                let upload = read_json::<PublicationUploadV1>(
                    &upload_path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "publication upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                validate_publication_upload(&upload)?;
                if is_open(upload.state)
                    && now.saturating_sub(upload.updated_unix_secs) >= limits.upload_idle_ttl_secs
                    && !protected.contains(&(FinalizeKindV1::Publication, upload.upload_id.clone()))
                {
                    remove_upload_directory(&upload_path, false)?;
                    expired += 1;
                }
            }
        }
        for workspace in read_child_directories(&self.root.join("provisional/uploads"), &[])? {
            WorkspaceId::parse(file_name(&workspace)?)?;
            for upload_path in read_child_directories(&workspace, &[])? {
                let upload = read_json::<ProvisionalUploadV1>(
                    &upload_path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "provisional upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                validate_provisional_upload(&upload)?;
                if is_open(upload.state)
                    && now.saturating_sub(upload.updated_unix_secs) >= limits.upload_idle_ttl_secs
                    && !protected.contains(&(FinalizeKindV1::Provisional, upload.upload_id.clone()))
                {
                    remove_upload_directory(&upload_path, true)?;
                    expired += 1;
                }
            }
        }
        Ok(expired)
    }

    fn unfinished_journal_uploads(&self) -> Result<BTreeSet<(FinalizeKindV1, String)>> {
        let mut protected = BTreeSet::new();
        for path in read_regular_json_files(&self.root.join("journals"))? {
            let name = file_name(&path)?;
            let journal = read_json::<FinalizeJournalV1>(
                &self.root.join("journals"),
                &name,
                MAX_JOURNAL_BYTES,
                "knowledge-source finalize journal",
            )?
            .ok_or(StoreRequestError::InvalidState)?;
            journal.validate()?;
            if journal.stage != FinalizeStageV1::Committed {
                protected.insert((journal.kind, journal.upload_id));
            }
        }
        Ok(protected)
    }

    fn expire_provisional_leases(&self, now: u64) -> Result<u64> {
        let mut expired = 0_u64;
        for project in read_child_directories(&self.root.join("provisional/generations"), &[])? {
            let project_id = file_name(&project)?;
            validate_project_id(&project_id)?;
            for workspace_root in read_child_directories(&project, &[])? {
                let workspace_id = WorkspaceId::parse(file_name(&workspace_root)?)?;
                let Some(pointer) = read_json::<ProvisionalPointerV1>(
                    &workspace_root,
                    "current.json",
                    MAX_GENERATION_RECORD_BYTES,
                    "provisional pointer",
                )?
                else {
                    continue;
                };
                if pointer.version != STORE_VERSION
                    || pointer.project_id != project_id
                    || pointer.workspace_id != workspace_id
                {
                    bail!(StoreRequestError::InvalidState);
                }
                if pointer.lease_expires_unix_secs > now {
                    continue;
                }
                let mut source = self.load_provisional_generation(
                    &project_id,
                    &workspace_id,
                    &pointer.source_generation_id,
                )?;
                remove_regular_file(&workspace_root.join("current.json"))?;
                source.state = SourceGenerationStateV1::Expired;
                self.write_provisional_source(&source)?;
                expired += 1;
            }
        }
        Ok(expired)
    }

    fn retire_old_generations(
        &self,
        protected_publication_generations: &BTreeSet<String>,
    ) -> Result<(u64, u64)> {
        let limits = self.current_limits()?;
        let journal_roots = self.journal_generation_roots()?;
        let mut retired_publication = 0_u64;
        for project in read_child_directories(&self.root.join("publications/generations"), &[])? {
            let project_id = file_name(&project)?;
            validate_project_id(&project_id)?;
            let mut generations = Vec::new();
            for path in read_child_directories(&project, &[])? {
                let generation_id = file_name(&path)?;
                validate_publication_generation_id(&generation_id)?;
                let source = self.load_publication_generation(&project_id, &generation_id)?;
                generations.push((source.created_unix_nanos, generation_id));
            }
            generations
                .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
            for (_, generation_id) in generations
                .into_iter()
                .skip(limits.retained_publication_generations)
            {
                if protected_publication_generations.contains(&generation_id)
                    || journal_roots.contains(&generation_id)
                {
                    continue;
                }
                self.retire_finalized_generation(FinalizeKindV1::Publication, &generation_id)?;
                retired_publication += 1;
            }
        }

        let mut retired_provisional = 0_u64;
        for project in read_child_directories(&self.root.join("provisional/generations"), &[])? {
            let project_id = file_name(&project)?;
            validate_project_id(&project_id)?;
            for workspace_root in read_child_directories(&project, &[])? {
                let workspace_id = WorkspaceId::parse(file_name(&workspace_root)?)?;
                let current = read_json::<ProvisionalPointerV1>(
                    &workspace_root,
                    "current.json",
                    MAX_GENERATION_RECORD_BYTES,
                    "provisional pointer",
                )?
                .map(|pointer| pointer.source_generation_id);
                let mut generations = Vec::new();
                for path in read_child_directories(&workspace_root, &["current.json"])? {
                    let generation_id = file_name(&path)?;
                    if generation_id == "sequences" {
                        continue;
                    }
                    validate_provisional_generation_id(&generation_id)?;
                    let source = self.load_provisional_generation(
                        &project_id,
                        &workspace_id,
                        &generation_id,
                    )?;
                    generations.push((source.created_unix_nanos, generation_id));
                }
                generations
                    .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
                for (_, generation_id) in generations
                    .into_iter()
                    .skip(limits.retained_provisional_generations)
                {
                    if current.as_deref() == Some(&generation_id)
                        || journal_roots.contains(&generation_id)
                    {
                        continue;
                    }
                    self.retire_finalized_generation(FinalizeKindV1::Provisional, &generation_id)?;
                    retired_provisional += 1;
                }
            }
        }
        Ok((retired_publication, retired_provisional))
    }

    fn retire_finalized_generation(&self, kind: FinalizeKindV1, generation_id: &str) -> Result<()> {
        let journal_name = journal_filename(kind, generation_id);
        let mut journal = read_json::<FinalizeJournalV1>(
            &self.root.join("journals"),
            &journal_name,
            MAX_JOURNAL_BYTES,
            "knowledge-source finalize journal",
        )?
        .ok_or(StoreRequestError::InvalidState)?;
        journal.validate()?;
        if journal.kind != kind
            || journal.stage != FinalizeStageV1::Committed
            || journal.source_generation_id != generation_id
        {
            bail!(StoreRequestError::InvalidState);
        }
        match journal.kind {
            FinalizeKindV1::Publication => {
                let path =
                    self.publication_upload_path(&journal.authority_key, &journal.upload_id)?;
                let upload = read_json::<PublicationUploadV1>(
                    &path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "publication upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                if upload.state != SourceGenerationStateV1::Ready
                    || upload.source_generation_id != generation_id
                    || upload.project_id != journal.project_id
                {
                    bail!(StoreRequestError::InvalidState);
                }
            }
            FinalizeKindV1::Provisional => {
                let workspace_id = WorkspaceId::parse(journal.authority_key.clone())?;
                let path = self.provisional_upload_path(&workspace_id, &journal.upload_id)?;
                let upload = read_json::<ProvisionalUploadV1>(
                    &path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "provisional upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                if upload.state != SourceGenerationStateV1::Ready
                    || upload.source_generation_id != generation_id
                    || upload.project_id != journal.project_id
                    || Some(upload.descriptor.sequence) != journal.provisional_sequence
                {
                    bail!(StoreRequestError::InvalidState);
                }
            }
        }
        journal.stage = FinalizeStageV1::Retiring;
        let journal = self.write_finalize_journal(journal)?;
        self.complete_retiring_generation(&journal)
    }

    fn complete_retiring_generation(&self, journal: &FinalizeJournalV1) -> Result<()> {
        if journal.stage != FinalizeStageV1::Retiring {
            bail!(StoreRequestError::InvalidState);
        }
        match journal.kind {
            FinalizeKindV1::Publication => {
                let generation = self.publication_generation_path(
                    &journal.project_id,
                    &journal.source_generation_id,
                )?;
                if NofollowDirectory::open_existing(&generation)?.is_some() {
                    remove_generation_directory(&generation, false)?;
                }
                remove_regular_file(
                    &self
                        .root
                        .join("publications/generation-index")
                        .join(format!("{}.json", journal.source_generation_id)),
                )?;
                let upload =
                    self.publication_upload_path(&journal.authority_key, &journal.upload_id)?;
                if NofollowDirectory::open_existing(&upload)?.is_some() {
                    remove_upload_directory(&upload, false)?;
                }
            }
            FinalizeKindV1::Provisional => {
                let workspace_id = WorkspaceId::parse(journal.authority_key.clone())?;
                let generation = self.provisional_generation_path(
                    &journal.project_id,
                    &workspace_id,
                    &journal.source_generation_id,
                )?;
                if NofollowDirectory::open_existing(&generation)?.is_some() {
                    remove_generation_directory(&generation, true)?;
                }
                remove_regular_file(
                    &self
                        .root
                        .join("provisional/generation-index")
                        .join(format!("{}.json", journal.source_generation_id)),
                )?;
                let workspace_root =
                    self.provisional_workspace_root(&journal.project_id, &workspace_id)?;
                remove_regular_file(&workspace_root.join("sequences").join(format!(
                        "{:020}.json",
                        journal
                            .provisional_sequence
                            .ok_or(StoreRequestError::InvalidState)?
                    )))?;
                let upload = self.provisional_upload_path(&workspace_id, &journal.upload_id)?;
                if NofollowDirectory::open_existing(&upload)?.is_some() {
                    remove_upload_directory(&upload, true)?;
                }
            }
        }
        remove_regular_file(&self.root.join("journals").join(journal_filename(
            journal.kind,
            &journal.source_generation_id,
        )))
    }

    fn journal_generation_roots(&self) -> Result<BTreeSet<String>> {
        let mut roots = BTreeSet::new();
        for path in read_regular_json_files(&self.root.join("journals"))? {
            let journal = read_json::<FinalizeJournalV1>(
                &self.root.join("journals"),
                &file_name(&path)?,
                MAX_JOURNAL_BYTES,
                "knowledge-source finalize journal",
            )?
            .ok_or(StoreRequestError::InvalidState)?;
            journal.validate()?;
            if journal.stage != FinalizeStageV1::Committed {
                roots.insert(journal.source_generation_id);
            }
        }
        Ok(roots)
    }

    fn referenced_blob_hashes(&self) -> Result<BTreeSet<String>> {
        let mut hashes = BTreeSet::new();
        collect_manifest_hashes(&self.root.join("publications/uploads"), &mut hashes)?;
        collect_manifest_hashes(&self.root.join("provisional/uploads"), &mut hashes)?;
        collect_manifest_hashes(&self.root.join("publications/generations"), &mut hashes)?;
        collect_manifest_hashes(&self.root.join("provisional/generations"), &mut hashes)?;
        Ok(hashes)
    }

    fn sweep_unreferenced_blobs(
        &self,
        referenced: &BTreeSet<String>,
        now: u64,
    ) -> Result<(u64, u64)> {
        let grace = self.current_limits()?.unreferenced_blob_grace_secs;
        let root = self.root.join("blobs/sha256");
        let mut deleted = 0_u64;
        let mut deleted_bytes = 0_u64;
        for prefix in read_child_directories(&root, &[])? {
            let prefix_name = file_name(&prefix)?;
            if prefix_name.len() != 2 || !is_lower_hex(&prefix_name) {
                bail!(StoreRequestError::InvalidState);
            }
            for entry in fs::read_dir(&prefix)? {
                let entry = entry?;
                let metadata = fs::symlink_metadata(entry.path())?;
                let suffix = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow!("blob filename is not UTF-8"))?;
                let hash = format!("{prefix_name}{suffix}");
                if metadata.file_type().is_symlink()
                    || !metadata.is_file()
                    || suffix.len() != 62
                    || !is_lower_hex(&suffix)
                {
                    bail!(StoreRequestError::InvalidState);
                }
                if referenced.contains(&hash) {
                    continue;
                }
                let modified = metadata
                    .modified()?
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| anyhow!("blob mtime predates Unix epoch"))?
                    .as_secs();
                if now.saturating_sub(modified) < grace {
                    continue;
                }
                deleted_bytes = deleted_bytes.saturating_add(metadata.len());
                fs::remove_file(entry.path())?;
                deleted += 1;
            }
            fs::File::open(&prefix)?.sync_all()?;
            if fs::read_dir(&prefix)?.next().transpose()?.is_none() {
                fs::remove_dir(&prefix)?;
            }
        }
        fs::File::open(root)?.sync_all()?;
        Ok((deleted, deleted_bytes))
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::io::Cursor;

    use bbox_knowledge_source::{
        AncestryDescriptorV1, GitObjectFormatV1, SCHEMA_VERSION, StableCaptureV1, ancestry_sha256,
        source_file_blob_sha256, source_manifest_sha256, working_pair_sha256,
    };
    use tempfile::TempDir;

    use super::*;

    const KNOWLEDGE_BYTES: &[u8] = br#"{"id":"knowledge-1"}"#;
    const GAP_BYTES: &[u8] = br#"{"id":"gap-11111111"}"#;

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo-family", ".").unwrap()
    }

    fn publication_authority() -> PublicationAuthorityV1 {
        PublicationAuthorityV1 {
            producer_id: "producer-a".to_string(),
            project_id: "project-a".to_string(),
            scope: scope(),
        }
    }

    fn provisional_authority() -> ProvisionalAuthorityV1 {
        ProvisionalAuthorityV1 {
            project_id: "project-a".to_string(),
            scope: scope(),
            workspace_id: WorkspaceId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        }
    }

    fn entry(path: &str, bytes: &[u8]) -> SourceFileManifestEntryV1 {
        SourceFileManifestEntryV1 {
            repository_relative_filename: path.to_string(),
            encoded_bytes: bytes.len() as u64,
            content_sha256: source_file_blob_sha256(bytes),
        }
    }

    fn manifest(
        lane: SourceLaneV1,
        entries: &[SourceFileManifestEntryV1],
    ) -> SourceManifestDescriptorV1 {
        SourceManifestDescriptorV1 {
            manifest_sha256: source_manifest_sha256(lane, entries),
            file_count: entries.len() as u64,
            logical_bytes: entries.iter().map(|entry| entry.encoded_bytes).sum(),
            page_count: (!entries.is_empty()) as u64,
        }
    }

    fn publication_fixture() -> (
        PublicationCandidateDescriptorV1,
        Vec<SourceFileManifestEntryV1>,
        Vec<SourceFileManifestEntryV1>,
    ) {
        let knowledge = vec![entry(".bbox/knowledge/knowledge-1.json", KNOWLEDGE_BYTES)];
        let gaps = vec![entry(".bbox/gaps/gap-11111111.json", GAP_BYTES)];
        let descriptor = PublicationCandidateDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope: scope(),
            full_ref: "refs/heads/main".to_string(),
            publisher_commit: "1".repeat(40),
            object_format: GitObjectFormatV1::Sha1,
            knowledge: manifest(SourceLaneV1::Knowledge, &knowledge),
            gaps: manifest(SourceLaneV1::Gaps, &gaps),
        };
        (descriptor, knowledge, gaps)
    }

    fn ancestry_fixture() -> (AncestryDescriptorV1, Vec<AncestryCommitV1>) {
        let root = "1".repeat(40);
        let nodes = vec![
            AncestryCommitV1 {
                commit_oid: root.clone(),
                parent_oids: Vec::new(),
            },
            AncestryCommitV1 {
                commit_oid: "2".repeat(40),
                parent_oids: vec![root.clone()],
            },
            AncestryCommitV1 {
                commit_oid: "3".repeat(40),
                parent_oids: vec![root],
            },
        ];
        (
            AncestryDescriptorV1 {
                ancestry_sha256: ancestry_sha256(GitObjectFormatV1::Sha1, &nodes),
                node_count: nodes.len() as u64,
                edge_count: 2,
                page_count: 1,
            },
            nodes,
        )
    }

    fn provisional_fixture(
        sequence: u64,
    ) -> (
        ProvisionalWorkspaceDescriptorV1,
        Vec<AncestryCommitV1>,
        Vec<SourceFileManifestEntryV1>,
        Vec<SourceFileManifestEntryV1>,
    ) {
        let authority = provisional_authority();
        let knowledge = vec![entry(".bbox/knowledge/knowledge-1.json", KNOWLEDGE_BYTES)];
        let gaps = vec![entry(".bbox/gaps/gap-11111111.json", GAP_BYTES)];
        let baseline_knowledge = manifest(SourceLaneV1::Knowledge, &knowledge);
        let baseline_gaps = manifest(SourceLaneV1::Gaps, &gaps);
        let working_knowledge = baseline_knowledge.clone();
        let working_gaps = baseline_gaps.clone();
        let working_pair = working_pair_sha256(&working_knowledge, &working_gaps);
        let (ancestry, nodes) = ancestry_fixture();
        (
            ProvisionalWorkspaceDescriptorV1 {
                schema_version: SCHEMA_VERSION,
                scope: scope(),
                workspace_id: authority.workspace_id,
                sequence,
                accepted_generation: "a".repeat(64),
                accepted_commit: "2".repeat(40),
                checkout_head: "3".repeat(40),
                merge_base: "1".repeat(40),
                object_format: GitObjectFormatV1::Sha1,
                ancestry,
                capture: StableCaptureV1 {
                    transaction_pending_before: false,
                    transaction_pending_after: false,
                    first_working_pair_sha256: working_pair.clone(),
                    second_working_pair_sha256: working_pair,
                },
                baseline_knowledge,
                baseline_gaps,
                working_knowledge,
                working_gaps,
            },
            nodes,
            knowledge,
            gaps,
        )
    }

    fn test_store(limits: StoreLimits) -> (TempDir, PathBuf, KnowledgeSourceStore) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("source-store");
        let store = KnowledgeSourceStore::open(&root, limits).unwrap();
        (temporary, root, store)
    }

    fn assert_store_error<T: Debug>(result: Result<T>, expected: StoreRequestError) {
        let error = result.unwrap_err();
        assert_eq!(error.downcast_ref::<StoreRequestError>(), Some(&expected));
    }

    fn put_publication_pages(
        store: &KnowledgeSourceStore,
        authority: &PublicationAuthorityV1,
        upload_id: &str,
        knowledge: &[SourceFileManifestEntryV1],
        gaps: &[SourceFileManifestEntryV1],
    ) {
        for (lane, entries) in [
            (SourceLaneV1::Knowledge, knowledge),
            (SourceLaneV1::Gaps, gaps),
        ] {
            if !entries.is_empty() {
                store
                    .put_publication_manifest_page(
                        authority,
                        upload_id,
                        lane,
                        0,
                        &SourceManifestPageV1 {
                            page_index: 0,
                            entries: entries.to_vec(),
                        },
                    )
                    .unwrap();
            }
        }
    }

    fn put_provisional_pages(
        store: &KnowledgeSourceStore,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
        nodes: &[AncestryCommitV1],
        knowledge: &[SourceFileManifestEntryV1],
        gaps: &[SourceFileManifestEntryV1],
    ) {
        store
            .put_provisional_ancestry_page(
                authority,
                upload_id,
                0,
                &AncestryPageV1 {
                    page_index: 0,
                    nodes: nodes.to_vec(),
                },
            )
            .unwrap();
        for class in [SnapshotClassV1::Baseline, SnapshotClassV1::Working] {
            for (lane, entries) in [
                (SourceLaneV1::Knowledge, knowledge),
                (SourceLaneV1::Gaps, gaps),
            ] {
                store
                    .put_provisional_manifest_page(
                        authority,
                        upload_id,
                        class,
                        lane,
                        0,
                        &SourceManifestPageV1 {
                            page_index: 0,
                            entries: entries.to_vec(),
                        },
                    )
                    .unwrap();
            }
        }
    }

    fn install_fixture_blobs_publication(
        store: &KnowledgeSourceStore,
        authority: &PublicationAuthorityV1,
        upload_id: &str,
    ) {
        for bytes in [KNOWLEDGE_BYTES, GAP_BYTES] {
            store
                .install_publication_blob(
                    authority,
                    upload_id,
                    &source_file_blob_sha256(bytes),
                    bytes.len() as u64,
                    Cursor::new(bytes),
                )
                .unwrap();
        }
    }

    fn install_fixture_blobs_provisional(
        store: &KnowledgeSourceStore,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
    ) {
        for bytes in [KNOWLEDGE_BYTES, GAP_BYTES] {
            store
                .install_provisional_blob(
                    authority,
                    upload_id,
                    &source_file_blob_sha256(bytes),
                    bytes.len() as u64,
                    Cursor::new(bytes),
                )
                .unwrap();
        }
    }

    #[test]
    fn publication_upload_is_resumable_authority_bound_and_durable() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let authority = publication_authority();
        let (descriptor, knowledge, gaps) = publication_fixture();
        let begin = store
            .begin_publication_upload(&authority, descriptor)
            .unwrap();
        let resumed = store
            .begin_publication_upload(&authority, publication_fixture().0)
            .unwrap();
        assert_eq!(resumed.upload_id, begin.upload_id);

        let knowledge_page = SourceManifestPageV1 {
            page_index: 0,
            entries: knowledge.clone(),
        };
        store
            .put_publication_manifest_page(
                &authority,
                &begin.upload_id,
                SourceLaneV1::Knowledge,
                0,
                &knowledge_page,
            )
            .unwrap();
        store
            .put_publication_manifest_page(
                &authority,
                &begin.upload_id,
                SourceLaneV1::Knowledge,
                0,
                &knowledge_page,
            )
            .unwrap();
        let mut conflicting_page = knowledge_page;
        conflicting_page.entries[0].repository_relative_filename =
            ".bbox/knowledge/knowledge-2.json".to_string();
        assert_store_error(
            store.put_publication_manifest_page(
                &authority,
                &begin.upload_id,
                SourceLaneV1::Knowledge,
                0,
                &conflicting_page,
            ),
            StoreRequestError::Conflict,
        );
        store
            .put_publication_manifest_page(
                &authority,
                &begin.upload_id,
                SourceLaneV1::Gaps,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: gaps,
                },
            )
            .unwrap();

        let mut wrong_authority = authority.clone();
        wrong_authority.project_id = "project-b".to_string();
        assert_store_error(
            store.missing_publication_blobs(&wrong_authority, &begin.upload_id, None),
            StoreRequestError::NotFound,
        );
        let missing = store
            .missing_publication_blobs(&authority, &begin.upload_id, None)
            .unwrap();
        assert_eq!(missing.hashes.len(), 2);
        install_fixture_blobs_publication(&store, &authority, &begin.upload_id);
        assert!(
            store
                .missing_publication_blobs(&authority, &begin.upload_id, None)
                .unwrap()
                .hashes
                .is_empty()
        );
        let finalized = store
            .finalize_publication_upload(&authority, &begin.upload_id)
            .unwrap();
        assert_eq!(
            store
                .finalize_publication_upload(&authority, &begin.upload_id)
                .unwrap()
                .source_generation_id,
            finalized.source_generation_id
        );
        assert_eq!(
            store
                .publication_status(&authority.producer_id, &finalized.source_generation_id)
                .unwrap()
                .state,
            SourceGenerationStateV1::Ready
        );
        drop(store);
        let reopened = KnowledgeSourceStore::open(&root, StoreLimits::default()).unwrap();
        assert_eq!(
            reopened
                .publication_status(&authority.producer_id, &finalized.source_generation_id)
                .unwrap()
                .knowledge_files,
            1
        );
    }

    #[test]
    fn provisional_selection_is_monotonic_atomic_and_lease_controlled() {
        let (_temporary, _root, store) = test_store(StoreLimits::default());
        let authority = provisional_authority();
        let (descriptor, nodes, knowledge, gaps) = provisional_fixture(7);
        let first = store
            .begin_provisional_upload(&authority, descriptor.clone())
            .unwrap();
        put_provisional_pages(
            &store,
            &authority,
            &first.upload_id,
            &nodes,
            &knowledge,
            &gaps,
        );
        assert_eq!(
            store
                .missing_provisional_blobs(&authority, &first.upload_id, None)
                .unwrap()
                .hashes
                .len(),
            2
        );
        install_fixture_blobs_provisional(&store, &authority, &first.upload_id);
        let first_generation = store
            .finalize_provisional_upload(&authority, &first.upload_id, 60)
            .unwrap()
            .source_generation_id;
        assert_eq!(
            store
                .selected_provisional(&authority, now_unix_secs())
                .unwrap()
                .unwrap()
                .source_generation_id,
            first_generation
        );
        let renewed = store
            .renew_provisional(&authority, &first_generation, 120)
            .unwrap();
        assert!(renewed.lease_expires_unix_secs.unwrap() > now_unix_secs());

        let mut conflicting = descriptor;
        conflicting.checkout_head = "4".repeat(40);
        assert_store_error(
            store.begin_provisional_upload(&authority, conflicting),
            StoreRequestError::Conflict,
        );

        let (next_descriptor, next_nodes, next_knowledge, next_gaps) = provisional_fixture(8);
        let next = store
            .begin_provisional_upload(&authority, next_descriptor)
            .unwrap();
        put_provisional_pages(
            &store,
            &authority,
            &next.upload_id,
            &next_nodes,
            &next_knowledge,
            &next_gaps,
        );
        assert!(
            store
                .missing_provisional_blobs(&authority, &next.upload_id, None)
                .unwrap()
                .hashes
                .is_empty()
        );
        let next_generation = store
            .finalize_provisional_upload(&authority, &next.upload_id, 60)
            .unwrap()
            .source_generation_id;
        assert_eq!(
            store
                .provisional_status(&authority, &first_generation)
                .unwrap()
                .state,
            SourceGenerationStateV1::Superseded
        );
        assert_eq!(
            store
                .finalize_provisional_upload(&authority, &first.upload_id, 60)
                .unwrap()
                .source_generation_id,
            first_generation
        );
        assert_eq!(
            store
                .selected_provisional(&authority, now_unix_secs())
                .unwrap()
                .unwrap()
                .source_generation_id,
            next_generation
        );
        assert_store_error(
            store.begin_provisional_upload(&authority, provisional_fixture(7).0),
            StoreRequestError::InvalidState,
        );
        store
            .retire_provisional(&authority, &next_generation)
            .unwrap();
        assert!(
            store
                .selected_provisional(&authority, now_unix_secs())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn recovery_replays_generation_install_with_journal_bound_timestamp() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let authority = publication_authority();
        let (descriptor, knowledge, gaps) = publication_fixture();
        let begin = store
            .begin_publication_upload(&authority, descriptor)
            .unwrap();
        put_publication_pages(&store, &authority, &begin.upload_id, &knowledge, &gaps);
        store
            .missing_publication_blobs(&authority, &begin.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &authority, &begin.upload_id);

        let upload_path = store
            .publication_upload_path(&authority.producer_id, &begin.upload_id)
            .unwrap();
        let upload = store
            .load_publication_upload(&upload_path, &authority, &begin.upload_id)
            .unwrap();
        let mut journal = store
            .write_finalize_journal(FinalizeJournalV1 {
                version: STORE_VERSION,
                kind: FinalizeKindV1::Publication,
                stage: FinalizeStageV1::Prepared,
                upload_id: begin.upload_id.clone(),
                source_generation_id: upload.source_generation_id.clone(),
                authority_key: authority.producer_id.clone(),
                project_id: authority.project_id.clone(),
                created_unix_secs: 1,
                created_unix_nanos: 1,
                lease_expires_unix_secs: None,
                prior_generation_id: None,
                provisional_sequence: None,
                checksum_sha256: String::new(),
            })
            .unwrap();
        let generation_path = store
            .publication_generation_path(&authority.project_id, &upload.source_generation_id)
            .unwrap();
        let directory = NofollowDirectory::open_or_create(&generation_path).unwrap();
        let manifests = load_publication_manifests(&upload_path).unwrap();
        install_immutable_json(&directory, "descriptor.json", &upload.descriptor).unwrap();
        install_immutable_json(&directory, "manifest-knowledge.json", &manifests.0).unwrap();
        install_immutable_json(&directory, "manifest-gaps.json", &manifests.1).unwrap();
        install_immutable_json(
            &directory,
            "source.json",
            &StoredPublicationCandidateV1 {
                version: STORE_VERSION,
                source_generation_id: upload.source_generation_id.clone(),
                producer_id: authority.producer_id.clone(),
                project_id: authority.project_id.clone(),
                descriptor: upload.descriptor,
                state: SourceGenerationStateV1::Ready,
                created_unix_secs: 1,
                created_unix_nanos: 1,
                diagnostic: None,
            },
        )
        .unwrap();
        journal.stage = FinalizeStageV1::GenerationInstalled;
        store.write_finalize_journal(journal).unwrap();
        drop(store);

        let recovered = KnowledgeSourceStore::open(&root, StoreLimits::default()).unwrap();
        let source = recovered
            .load_publication_generation(&authority.project_id, &upload.source_generation_id)
            .unwrap();
        assert_eq!(source.created_unix_secs, 1);
        assert_eq!(source.state, SourceGenerationStateV1::Ready);
        let journal = read_json::<FinalizeJournalV1>(
            &root.join("journals"),
            &journal_filename(FinalizeKindV1::Publication, &upload.source_generation_id),
            MAX_JOURNAL_BYTES,
            "test journal",
        )
        .unwrap()
        .unwrap();
        assert_eq!(journal.stage, FinalizeStageV1::Committed);
    }

    #[test]
    fn missing_blob_cursor_is_exclusive_without_skipping_the_overflow_item() {
        let (_temporary, _root, store) = test_store(StoreLimits::default());
        let authority = publication_authority();
        let knowledge = (0..=MISSING_PAGE_SIZE)
            .map(|index| {
                let bytes = format!("blob-{index:04}");
                entry(
                    &format!(".bbox/knowledge/knowledge-{index:04}.json"),
                    bytes.as_bytes(),
                )
            })
            .collect::<Vec<_>>();
        let descriptor = PublicationCandidateDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope: scope(),
            full_ref: "refs/heads/main".to_string(),
            publisher_commit: "1".repeat(40),
            object_format: GitObjectFormatV1::Sha1,
            knowledge: manifest(SourceLaneV1::Knowledge, &knowledge),
            gaps: manifest(SourceLaneV1::Gaps, &[]),
        };
        let begin = store
            .begin_publication_upload(&authority, descriptor)
            .unwrap();
        put_publication_pages(&store, &authority, &begin.upload_id, &knowledge, &[]);
        let first = store
            .missing_publication_blobs(&authority, &begin.upload_id, None)
            .unwrap();
        assert_eq!(first.hashes.len(), MISSING_PAGE_SIZE);
        assert_eq!(first.next_cursor.as_ref(), first.hashes.last());
        let second = store
            .missing_publication_blobs(&authority, &begin.upload_id, first.next_cursor.as_deref())
            .unwrap();
        assert_eq!(second.hashes.len(), 1);
        assert!(second.next_cursor.is_none());
        assert!(!first.hashes.contains(&second.hashes[0]));
    }

    #[test]
    fn maintenance_expires_only_open_uploads_and_collects_unreferenced_blobs() {
        let limits = StoreLimits {
            upload_idle_ttl_secs: 1,
            unreferenced_blob_grace_secs: 1,
            ..StoreLimits::default()
        };
        let (_temporary, _root, store) = test_store(limits);
        let authority = publication_authority();
        let begin = store
            .begin_publication_upload(&authority, publication_fixture().0)
            .unwrap();
        let upload_path = store
            .publication_upload_path(&authority.producer_id, &begin.upload_id)
            .unwrap();
        let orphan = b"orphan-blob";
        let orphan_hash = source_file_blob_sha256(orphan);
        store.install_blob_bytes(&orphan_hash, orphan).unwrap();

        let report = store.maintain_at(&BTreeSet::new(), u64::MAX).unwrap();
        assert_eq!(report.expired_uploads, 1);
        assert_eq!(report.deleted_blobs, 1);
        assert!(!upload_path.exists());
        assert!(
            store
                .read_blob(&orphan_hash, orphan.len())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn retention_reclaims_terminal_upload_journal_and_now_unreferenced_blobs() {
        let limits = StoreLimits {
            retained_publication_generations: 1,
            unreferenced_blob_grace_secs: 1,
            ..StoreLimits::default()
        };
        let (_temporary, root, store) = test_store(limits);
        let authority = publication_authority();
        let (first_descriptor, knowledge, gaps) = publication_fixture();
        let first = store
            .begin_publication_upload(&authority, first_descriptor)
            .unwrap();
        put_publication_pages(&store, &authority, &first.upload_id, &knowledge, &gaps);
        store
            .missing_publication_blobs(&authority, &first.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &authority, &first.upload_id);
        let first_generation = store
            .finalize_publication_upload(&authority, &first.upload_id)
            .unwrap()
            .source_generation_id;

        let mut second_descriptor = publication_fixture().0;
        second_descriptor.publisher_commit = "2".repeat(40);
        second_descriptor.knowledge = manifest(SourceLaneV1::Knowledge, &[]);
        second_descriptor.gaps = manifest(SourceLaneV1::Gaps, &[]);
        let second = store
            .begin_publication_upload(&authority, second_descriptor)
            .unwrap();
        assert!(
            store
                .missing_publication_blobs(&authority, &second.upload_id, None)
                .unwrap()
                .hashes
                .is_empty()
        );
        store
            .finalize_publication_upload(&authority, &second.upload_id)
            .unwrap();

        let report = store.maintain_at(&BTreeSet::new(), u64::MAX).unwrap();
        assert_eq!(report.retired_publication_generations, 1);
        assert_eq!(report.deleted_blobs, 2);
        assert!(
            !store
                .publication_upload_path(&authority.producer_id, &first.upload_id)
                .unwrap()
                .exists()
        );
        assert!(
            !root
                .join("journals")
                .join(journal_filename(
                    FinalizeKindV1::Publication,
                    &first_generation
                ))
                .exists()
        );
    }

    #[test]
    fn recovery_completes_partially_retired_generation() {
        let limits = StoreLimits {
            retained_publication_generations: 1,
            ..StoreLimits::default()
        };
        let (_temporary, root, store) = test_store(limits);
        let authority = publication_authority();
        let (first_descriptor, knowledge, gaps) = publication_fixture();
        let first = store
            .begin_publication_upload(&authority, first_descriptor)
            .unwrap();
        put_publication_pages(&store, &authority, &first.upload_id, &knowledge, &gaps);
        store
            .missing_publication_blobs(&authority, &first.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &authority, &first.upload_id);
        let first_generation = store
            .finalize_publication_upload(&authority, &first.upload_id)
            .unwrap()
            .source_generation_id;

        let mut second_descriptor = publication_fixture().0;
        second_descriptor.publisher_commit = "2".repeat(40);
        second_descriptor.knowledge = manifest(SourceLaneV1::Knowledge, &[]);
        second_descriptor.gaps = manifest(SourceLaneV1::Gaps, &[]);
        let second = store
            .begin_publication_upload(&authority, second_descriptor)
            .unwrap();
        store
            .missing_publication_blobs(&authority, &second.upload_id, None)
            .unwrap();
        let second_generation = store
            .finalize_publication_upload(&authority, &second.upload_id)
            .unwrap()
            .source_generation_id;

        let journal_name = journal_filename(FinalizeKindV1::Publication, &first_generation);
        let mut journal = read_json::<FinalizeJournalV1>(
            &root.join("journals"),
            &journal_name,
            MAX_JOURNAL_BYTES,
            "knowledge-source finalize journal",
        )
        .unwrap()
        .unwrap();
        journal.stage = FinalizeStageV1::Retiring;
        store.write_finalize_journal(journal).unwrap();
        let first_generation_path = store
            .publication_generation_path(&authority.project_id, &first_generation)
            .unwrap();
        remove_generation_directory(&first_generation_path, false).unwrap();
        drop(store);

        let recovered = KnowledgeSourceStore::open(&root, limits).unwrap();
        assert!(!first_generation_path.exists());
        assert!(
            !recovered
                .publication_upload_path(&authority.producer_id, &first.upload_id)
                .unwrap()
                .exists()
        );
        assert!(!root.join("journals").join(journal_name).exists());
        assert_store_error(
            recovered.publication_status(&authority.producer_id, &first_generation),
            StoreRequestError::NotFound,
        );
        assert_eq!(
            recovered
                .publication_status(&authority.producer_id, &second_generation)
                .unwrap()
                .state,
            SourceGenerationStateV1::Ready
        );
    }

    #[test]
    fn maintenance_resumes_partially_retired_generation_without_restart() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let authority = publication_authority();
        let (descriptor, knowledge, gaps) = publication_fixture();
        let upload = store
            .begin_publication_upload(&authority, descriptor)
            .unwrap();
        put_publication_pages(&store, &authority, &upload.upload_id, &knowledge, &gaps);
        store
            .missing_publication_blobs(&authority, &upload.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &authority, &upload.upload_id);
        let generation = store
            .finalize_publication_upload(&authority, &upload.upload_id)
            .unwrap()
            .source_generation_id;

        let journal_name = journal_filename(FinalizeKindV1::Publication, &generation);
        let mut journal = read_json::<FinalizeJournalV1>(
            &root.join("journals"),
            &journal_name,
            MAX_JOURNAL_BYTES,
            "knowledge-source finalize journal",
        )
        .unwrap()
        .unwrap();
        journal.stage = FinalizeStageV1::Retiring;
        store.write_finalize_journal(journal).unwrap();
        let generation_path = store
            .publication_generation_path(&authority.project_id, &generation)
            .unwrap();
        remove_generation_directory(&generation_path, false).unwrap();

        let report = store.maintain_at(&BTreeSet::new(), u64::MAX).unwrap();
        assert_eq!(report.retired_publication_generations, 1);
        assert!(!generation_path.exists());
        assert!(
            !store
                .publication_upload_path(&authority.producer_id, &upload.upload_id)
                .unwrap()
                .exists()
        );
        assert!(!root.join("journals").join(journal_name).exists());
        assert_store_error(
            store.publication_status(&authority.producer_id, &generation),
            StoreRequestError::NotFound,
        );
    }

    #[test]
    fn pinned_ready_candidate_materializes_exact_bytes_and_blocks_retention() {
        let limits = StoreLimits {
            retained_publication_generations: 1,
            ..StoreLimits::default()
        };
        let (_temporary, _root, store) = test_store(limits);
        let authority = publication_authority();
        let (descriptor, knowledge, gaps) = publication_fixture();
        let first = store
            .begin_publication_upload(&authority, descriptor)
            .unwrap();
        put_publication_pages(&store, &authority, &first.upload_id, &knowledge, &gaps);
        store
            .missing_publication_blobs(&authority, &first.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &authority, &first.upload_id);
        let first_generation = store
            .finalize_publication_upload(&authority, &first.upload_id)
            .unwrap()
            .source_generation_id;
        let pinned = store
            .pin_ready_publication_candidate(&first_generation)
            .unwrap();
        assert_eq!(pinned.candidate().knowledge.len(), 1);
        assert_eq!(pinned.candidate().gaps.len(), 1);
        assert_eq!(
            pinned.candidate().knowledge[0].source_bytes,
            KNOWLEDGE_BYTES
        );
        assert_eq!(pinned.candidate().gaps[0].source_bytes, GAP_BYTES);
        assert_eq!(pinned.candidate().source_generation_sha256.len(), 64);

        let mut second_descriptor = publication_fixture().0;
        second_descriptor.publisher_commit = "2".repeat(40);
        second_descriptor.knowledge = manifest(SourceLaneV1::Knowledge, &[]);
        second_descriptor.gaps = manifest(SourceLaneV1::Gaps, &[]);
        let second = store
            .begin_publication_upload(&authority, second_descriptor)
            .unwrap();
        store
            .missing_publication_blobs(&authority, &second.upload_id, None)
            .unwrap();
        store
            .finalize_publication_upload(&authority, &second.upload_id)
            .unwrap();

        let protected = store.maintain_at(&BTreeSet::new(), u64::MAX).unwrap();
        assert_eq!(protected.retired_publication_generations, 0);
        assert!(
            store
                .publication_status(&authority.producer_id, &first_generation)
                .is_ok()
        );
        drop(pinned);
        let reclaimed = store.maintain_at(&BTreeSet::new(), u64::MAX).unwrap();
        assert_eq!(reclaimed.retired_publication_generations, 1);
        assert_store_error(
            store.publication_status(&authority.producer_id, &first_generation),
            StoreRequestError::NotFound,
        );
    }
}
