//! Durable resumable intake for knowledge publication candidates.
//!
//! Directories and journals an older store kept for provisional workspace
//! snapshots are never read; [`retire_provisional_state`] removes them.

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
    BeginSourceUploadResponseV1, FinalizeSourceUploadResponseV1, KnowledgeSourceLimits,
    MissingSourceBlobsPageV1, PublicationCandidateDescriptorV1, PublicationCandidateStatusV1,
    SourceFileManifestEntryV1, SourceGenerationStateV1, SourceLaneV1, SourceManifestDescriptorV1,
    SourceManifestPageV1, publication_candidate_generation_id, publication_generation_id_matches,
    validate_manifest_page, validate_publication_candidate, validate_publication_generation_id,
    validate_source_blob,
};
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
const PUBLICATION_JOURNAL_PREFIX: &str = "publication-";
/// Store member and journal-name prefix an older store used for provisional
/// workspace snapshots. Scans skip both; only [`retire_provisional_state`]
/// touches them.
const RETIRED_PROVISIONAL_MEMBER: &str = "provisional";
const RETIRED_PROVISIONAL_JOURNAL_PREFIX: &str = "provisional-";

/// What one [`retire_provisional_state`] pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProvisionalRetirementReport {
    /// The legacy `provisional` store member existed and was removed.
    pub removed_directory: bool,
    /// Legacy `journals/provisional-*.json` files removed.
    pub removed_journals: u64,
}

/// Remove the state an older store kept for provisional workspace snapshots:
/// the `provisional` member under `root` and every regular
/// `journals/provisional-*.json`. Nothing else is touched, so publications,
/// blobs, and publication journals survive; blobs only those snapshots
/// referenced become unreferenced and the maintenance grace sweep reclaims
/// them.
///
/// A symlink at `provisional` is unlinked and never followed, and the
/// recursive removal of a directory does not follow symlinks inside it. The
/// pass is idempotent: an absent root or absent members report nothing
/// removed. It runs before [`KnowledgeSourceStore::open`].
// Startup migration path; runs before the listener binds, off any tokio
// worker.
#[allow(clippy::disallowed_methods)]
pub fn retire_provisional_state(root: &Path) -> Result<ProvisionalRetirementReport> {
    let mut report = ProvisionalRetirementReport::default();
    let member = root.join(RETIRED_PROVISIONAL_MEMBER);
    match fs::symlink_metadata(&member) {
        Ok(metadata) => {
            if metadata.is_dir() {
                fs::remove_dir_all(&member)
                    .with_context(|| format!("removing {}", member.display()))?;
            } else {
                fs::remove_file(&member)
                    .with_context(|| format!("removing {}", member.display()))?;
            }
            sync_parent(&member)?;
            report.removed_directory = true;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", member.display()));
        }
    }
    let journals = root.join("journals");
    let entries = match fs::read_dir(&journals) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", journals.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(RETIRED_PROVISIONAL_JOURNAL_PREFIX) || !name.ends_with(".json") {
            continue;
        }
        let path = entry.path();
        if !fs::symlink_metadata(&path)?.is_file() {
            continue;
        }
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        report.removed_journals += 1;
    }
    if report.removed_journals > 0 {
        fs::File::open(&journals)?.sync_all()?;
    }
    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreLimits {
    pub contract: KnowledgeSourceLimits,
    pub max_open_uploads_per_authority: usize,
    pub upload_idle_ttl_secs: u64,
    pub retained_publication_generations: usize,
    pub unreferenced_blob_grace_secs: u64,
}

fn store_directories() -> &'static [&'static str] {
    &[
        "",
        "publications",
        "publications/uploads",
        "publications/generations",
        "publications/generation-index",
        "blobs",
        "blobs/sha256",
        "journals",
    ]
}

fn validate_store_limits(limits: StoreLimits) -> Result<()> {
    limits.contract.validate()?;
    if limits.max_open_uploads_per_authority == 0
        || limits.upload_idle_ttl_secs == 0
        || limits.retained_publication_generations == 0
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

/// Admit a stored upload record and normalize its page cursors.
///
/// State written before a lane existed carries neither that lane's descriptor
/// nor its page cursor, and its generation identity was minted without the
/// lane. That holds for the graphs lane and, one rung up, for the evidence
/// lane. Both are legacy vintage, not corruption: the absent lane cursors are
/// backfilled empty in memory, and an older identity stays admissible for
/// exactly the shape that could have produced it. Everything else remains a
/// refusal.
fn validate_publication_upload(record: &mut PublicationUploadV1) -> Result<()> {
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
    if !publication_generation_id_matches(
        &record.producer_id,
        &record.descriptor,
        &record.source_generation_id,
    )? {
        bail!(StoreRequestError::InvalidState);
    }
    backfill_absent_page_cursors(
        &mut record.next_pages,
        publication_page_cursors(&record.descriptor),
    );
    Ok(())
}

/// Give a lane the store now knows about an empty cursor when the record
/// predates it. Existing cursors are never touched, so a resumable upload
/// keeps its exact durable position.
fn backfill_absent_page_cursors(
    cursors: &mut BTreeMap<String, u64>,
    expected: BTreeMap<String, u64>,
) {
    for (slot, empty) in expected {
        cursors.entry(slot).or_insert(empty);
    }
}

fn is_open(state: SourceGenerationStateV1) -> bool {
    matches!(
        state,
        SourceGenerationStateV1::ReceivingManifest | SourceGenerationStateV1::MissingBlobs
    )
}

/// Page cursors for one publication upload. The configuration lane is
/// explicitly optional, so its slot exists exactly when the descriptor
/// carries the lane: a page for an absent lane has no cursor to advance.
fn publication_page_cursors(
    descriptor: &PublicationCandidateDescriptorV1,
) -> BTreeMap<String, u64> {
    let mut cursors: BTreeMap<String, u64> = [
        (lane_name(SourceLaneV1::Knowledge).to_string(), 0),
        (lane_name(SourceLaneV1::Gaps).to_string(), 0),
        (lane_name(SourceLaneV1::Graphs).to_string(), 0),
        (lane_name(SourceLaneV1::Evidence).to_string(), 0),
    ]
    .into_iter()
    .collect();
    if descriptor.config.is_some() {
        cursors.insert(lane_name(SourceLaneV1::Config).to_string(), 0);
    }
    cursors
}

fn lane_name(lane: SourceLaneV1) -> &'static str {
    match lane {
        SourceLaneV1::Knowledge => "knowledge",
        SourceLaneV1::Gaps => "gaps",
        SourceLaneV1::Graphs => "graphs",
        SourceLaneV1::Evidence => "evidence",
        SourceLaneV1::Config => "config",
    }
}

fn publication_manifest_descriptor(
    descriptor: &PublicationCandidateDescriptorV1,
    lane: SourceLaneV1,
) -> Result<&SourceManifestDescriptorV1> {
    Ok(match lane {
        SourceLaneV1::Knowledge => &descriptor.knowledge,
        SourceLaneV1::Gaps => &descriptor.gaps,
        SourceLaneV1::Graphs => &descriptor.graphs,
        SourceLaneV1::Evidence => &descriptor.evidence,
        SourceLaneV1::Config => descriptor
            .config
            .as_ref()
            .ok_or(StoreRequestError::InvalidInput)?,
    })
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
    // An upload begun before this lane existed has no page directory for it.
    // Create it on the first write rather than assuming the vintage that laid
    // the upload out also knew about every lane.
    let page_dir = NofollowDirectory::open_or_create(&upload_path.join("pages").join(slot))?;
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

fn page_filename(page_index: u64) -> String {
    format!("{page_index:020}.json")
}

/// Read one lane manifest that state written before that lane existed never
/// wrote. An absent file is legacy vintage only when the descriptor also
/// carries an absent lane; a descriptor that claims content for the lane with
/// no manifest on disk is malformed and stays a refusal. Every lane added
/// after the original knowledge/gaps pair reads through here.
fn read_optional_lane_manifest(
    path: &Path,
    name: &str,
    label: &str,
    descriptor: &SourceManifestDescriptorV1,
) -> Result<Vec<SourceFileManifestEntryV1>> {
    match read_json::<Vec<SourceFileManifestEntryV1>>(path, name, MAX_MANIFEST_BYTES, label)? {
        Some(manifest) => Ok(manifest),
        None if descriptor.is_absent_lane() => Ok(Vec::new()),
        None => bail!(StoreRequestError::InvalidState),
    }
}

type PublicationManifests = (
    Vec<SourceFileManifestEntryV1>,
    Vec<SourceFileManifestEntryV1>,
    Vec<SourceFileManifestEntryV1>,
    Vec<SourceFileManifestEntryV1>,
    Option<Vec<SourceFileManifestEntryV1>>,
);

fn load_publication_manifests(
    path: &Path,
    descriptor: &PublicationCandidateDescriptorV1,
) -> Result<PublicationManifests> {
    Ok((
        read_required_json(path, "manifest-knowledge.json", "knowledge manifest")?,
        read_required_json(path, "manifest-gaps.json", "gap manifest")?,
        read_optional_lane_manifest(
            path,
            "manifest-graphs.json",
            "graph manifest",
            &descriptor.graphs,
        )?,
        read_optional_lane_manifest(
            path,
            "manifest-evidence.json",
            "evidence manifest",
            &descriptor.evidence,
        )?,
        // The configuration lane's presence is explicit, so a present lane
        // with no manifest on disk is malformed rather than legacy.
        descriptor
            .config
            .as_ref()
            .map(|_| read_required_json(path, "manifest-config.json", "configuration manifest"))
            .transpose()?,
    ))
}

fn load_expected_blobs(path: &Path) -> Result<BTreeMap<String, u64>> {
    let mut expected = BTreeMap::new();
    for name in [
        "manifest-knowledge.json",
        "manifest-gaps.json",
        "manifest-graphs.json",
        "manifest-evidence.json",
        "manifest-config.json",
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

fn finalize_response(source_generation_id: String) -> FinalizeSourceUploadResponseV1 {
    FinalizeSourceUploadResponseV1 {
        status_url: format!(
            "/internal/knowledge-source/v1/publication/generations/{source_generation_id}/status"
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
        graph_manifest_sha256: source.descriptor.graphs.manifest_sha256.clone(),
        evidence_manifest_sha256: source.descriptor.evidence.manifest_sha256.clone(),
        knowledge_files: source.descriptor.knowledge.file_count,
        gap_files: source.descriptor.gaps.file_count,
        graph_files: source.descriptor.graphs.file_count,
        evidence_files: source.descriptor.evidence.file_count,
        config_manifest_sha256: source
            .descriptor
            .config
            .as_ref()
            .map(|config| config.manifest_sha256.clone()),
        config_files: source
            .descriptor
            .config
            .as_ref()
            .map(|config| config.file_count),
        logical_bytes: source
            .descriptor
            .knowledge
            .logical_bytes
            .checked_add(source.descriptor.gaps.logical_bytes)
            .and_then(|bytes| bytes.checked_add(source.descriptor.graphs.logical_bytes))
            .and_then(|bytes| bytes.checked_add(source.descriptor.evidence.logical_bytes))
            .and_then(|bytes| {
                bytes.checked_add(
                    source
                        .descriptor
                        .config
                        .as_ref()
                        .map_or(0, |config| config.logical_bytes),
                )
            })
            .ok_or(StoreRequestError::LimitExceeded)?,
        diagnostic: source.diagnostic.clone(),
    })
}

fn journal_filename(generation_id: &str) -> String {
    format!("{PUBLICATION_JOURNAL_PREFIX}{generation_id}.json")
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

/// Install an immutable record whose serialized shape has grown since it may
/// have been written. Byte equality still short-circuits; a byte difference is
/// admitted only when the stored bytes decode to exactly the value being
/// installed, which is what a pre-graphs-lane encoding of the same record
/// does. Genuine content drift still conflicts, and the durable bytes are
/// never rewritten.
fn install_immutable_record<T: Serialize + DeserializeOwned + PartialEq>(
    directory: &NofollowDirectory,
    name: &str,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let Some(existing) = directory.read_regular(name, MAX_MANIFEST_BYTES, "immutable record")?
    else {
        return directory.atomic_replace(name, &bytes);
    };
    if existing == bytes {
        return Ok(());
    }
    let decoded: T = serde_json::from_slice(&existing)
        .with_context(|| format!("decoding stored immutable record {name}"))?;
    if &decoded != value {
        bail!(StoreRequestError::Conflict);
    }
    Ok(())
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

/// Name prefix of the temporary member an atomic replace stages in its target
/// directory before the rename. A scan that takes no mutation lock can see
/// one and must pass over it.
const ATOMIC_REPLACE_TEMP_PREFIX: &str = ".bbox-store-";

/// Regular JSON members of `path`, sorted. Members whose name starts with
/// `skipped_prefix` are passed over without inspection; any other non-regular
/// or non-JSON member is malformed state.
fn read_regular_json_files_except(
    path: &Path,
    skipped_prefix: Option<&str>,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("store member name is not UTF-8"))?;
        if skipped_prefix.is_some_and(|prefix| name.starts_with(prefix)) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
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

fn remove_upload_directory(path: &Path) -> Result<()> {
    remove_page_tree(&path.join("pages"), true)?;
    for name in [
        "upload.json",
        "manifest-knowledge.json",
        "manifest-gaps.json",
        "manifest-graphs.json",
        "manifest-evidence.json",
        "manifest-config.json",
    ] {
        remove_regular_file(&path.join(name))?;
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

fn remove_generation_directory(path: &Path) -> Result<()> {
    for name in [
        "descriptor.json",
        "manifest-knowledge.json",
        "manifest-gaps.json",
        "manifest-graphs.json",
        "manifest-evidence.json",
        "manifest-config.json",
        "source.json",
    ] {
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
    graphs: &[SourceFileManifestEntryV1],
    evidence: &[SourceFileManifestEntryV1],
    config: Option<&[SourceFileManifestEntryV1]>,
) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-knowledge-publication-source-evidence-v1\0");
    // A candidate without the configuration lane keeps the exact pre-lane
    // preimage; a present lane (even empty) extends the tuple.
    match config {
        None => hasher.update(serde_json::to_vec(&(
            source, knowledge, gaps, graphs, evidence,
        ))?),
        Some(config) => hasher.update(serde_json::to_vec(&(
            source, knowledge, gaps, graphs, evidence, config,
        ))?),
    }
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
            retained_publication_generations: 8,
            unreferenced_blob_grace_secs: 7 * 24 * 60 * 60,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub expired_uploads: u64,
    pub retired_publication_generations: u64,
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

/// What [`KnowledgeSourceStore::latest_publication_candidate`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LatestPublicationCandidateV1 {
    /// `None` when the project has no stored candidate.
    pub candidate: Option<StoredPublicationCandidateV1>,
    /// File names of generation index entries that could not be decoded,
    /// sorted. Their project is unknown, so a candidate behind one is
    /// missing from every project's answer.
    pub unreadable_index_entries: Vec<String>,
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
struct PublicationGenerationIndexV1 {
    version: u32,
    source_generation_id: String,
    producer_id: String,
    project_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum FinalizeKindV1 {
    Publication,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FinalizeStageV1 {
    Prepared,
    GenerationInstalled,
    Committed,
    Retiring,
}

impl FinalizeStageV1 {
    fn rank(self) -> u8 {
        match self {
            Self::Prepared => 0,
            Self::GenerationInstalled => 1,
            Self::Committed => 2,
            Self::Retiring => 3,
        }
    }
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
        {
            bail!(StoreRequestError::InvalidState);
        }
        match self.kind {
            FinalizeKindV1::Publication => {
                validate_publication_generation_id(&self.source_generation_id)?
            }
        }
        let mut projection = self.clone();
        let checksum = std::mem::take(&mut projection.checksum_sha256);
        if checksum != sha256(&serde_json::to_vec(&projection)?) {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(())
    }

    fn has_same_identity(&self, other: &Self) -> bool {
        self.version == other.version
            && self.kind == other.kind
            && self.upload_id == other.upload_id
            && self.source_generation_id == other.source_generation_id
            && self.authority_key == other.authority_key
            && self.project_id == other.project_id
            && self.created_unix_secs == other.created_unix_secs
            && self.created_unix_nanos == other.created_unix_nanos
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
    pub graphs: Vec<ReadyPublicationFile>,
    pub evidence: Vec<ReadyPublicationFile>,
    /// `None` exactly when the candidate carries no configuration lane
    /// (its producer predates the lane); `Some(empty)` is a verified empty
    /// configuration.
    pub config: Option<Vec<ReadyPublicationFile>>,
}

/// Lock-consistent source state used by the offline strict-cutover preflight.
///
/// The store reports source facts only. Authority decisions remain with the
/// indexing/runtime layer.
#[derive(Debug, Clone)]
pub struct KnowledgeSourceProjectCutoverReadiness {
    pub prepared_upload_count: u64,
    pub unfinished_finalize_journal_count: u64,
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
            let Some(mut record) = read_json::<PublicationUploadV1>(
                &path,
                "upload.json",
                MAX_UPLOAD_RECORD_BYTES,
                "publication upload",
            )?
            else {
                continue;
            };
            validate_publication_upload(&mut record)?;
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
        for lane in [
            SourceLaneV1::Knowledge,
            SourceLaneV1::Gaps,
            SourceLaneV1::Graphs,
            SourceLaneV1::Evidence,
        ]
        .into_iter()
        .chain(descriptor.config.is_some().then_some(SourceLaneV1::Config))
        {
            NofollowDirectory::open_or_create(&upload_path.join("pages").join(lane_name(lane)))?;
        }
        let next_pages = publication_page_cursors(&descriptor);
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
                next_pages,
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
        let mut record = read_json::<PublicationUploadV1>(
            &path,
            "upload.json",
            MAX_UPLOAD_RECORD_BYTES,
            "publication upload",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        validate_publication_upload(&mut record)?;
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
        let descriptor = publication_manifest_descriptor(&record.descriptor, lane)?;
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
        let (knowledge_manifest, gap_manifest, graph_manifest, evidence_manifest, config_manifest) =
            load_publication_manifests(&generation_path, &source.descriptor)?;
        validate_publication_candidate(
            &source.descriptor,
            &knowledge_manifest,
            &gap_manifest,
            &graph_manifest,
            &evidence_manifest,
            config_manifest.as_deref(),
            self.current_limits()?.contract,
        )?;
        let knowledge = self.materialize_ready_publication_files(&knowledge_manifest)?;
        let gaps = self.materialize_ready_publication_files(&gap_manifest)?;
        let graphs = self.materialize_ready_publication_files(&graph_manifest)?;
        let evidence = self.materialize_ready_publication_files(&evidence_manifest)?;
        let config = config_manifest
            .as_deref()
            .map(|manifest| self.materialize_ready_publication_files(manifest))
            .transpose()?;
        let source_generation_sha256 = publication_source_generation_sha256(
            &source,
            &knowledge_manifest,
            &gap_manifest,
            &graph_manifest,
            &evidence_manifest,
            config_manifest.as_deref(),
        )?;
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
                graphs,
                evidence,
                config,
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
        // Several Ready candidates can share one commit: a producer upgraded
        // to capture the configuration lane re-uploads the commit it already
        // published. The lane-bearing candidate is the current one, so a
        // lane-less match is only the fallback.
        let mut lane_less = None;
        for path in read_regular_json_files_except(
            &self.root.join("publications/generation-index"),
            Some(ATOMIC_REPLACE_TEMP_PREFIX),
        )? {
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
                if source.descriptor.config.is_some() {
                    return publication_status(&source).map(Some);
                }
                if lane_less.is_none() {
                    lane_less = Some(publication_status(&source)?);
                }
            }
        }
        Ok(lane_less)
    }

    /// The candidate stored most recently for one project, by its creation
    /// time, whatever its state and whichever producer uploaded it.
    ///
    /// Observational: it takes no mutation lock. A staged atomic-replace
    /// member is passed over, and so is a generation retired between the
    /// index read and the record read. An index entry that cannot be decoded
    /// belongs to an unknown project, so it is named in the result instead
    /// of failing the read for every project.
    pub fn latest_publication_candidate(
        &self,
        project_id: &str,
    ) -> Result<LatestPublicationCandidateV1> {
        validate_project_id(project_id)?;
        let index_dir = self.root.join("publications/generation-index");
        let mut latest = LatestPublicationCandidateV1::default();
        for path in read_regular_json_files_except(&index_dir, Some(ATOMIC_REPLACE_TEMP_PREFIX))? {
            let name = file_name(&path)?;
            let index = match read_json::<PublicationGenerationIndexV1>(
                &index_dir,
                &name,
                MAX_GENERATION_RECORD_BYTES,
                "publication generation index",
            ) {
                Ok(Some(index)) => index,
                Ok(None) => continue,
                Err(_) => {
                    latest.unreadable_index_entries.push(name);
                    continue;
                }
            };
            if index.version != STORE_VERSION || index.project_id != project_id {
                continue;
            }
            let source = match self
                .load_publication_generation(&index.project_id, &index.source_generation_id)
            {
                Ok(source) => source,
                Err(error)
                    if error.downcast_ref::<StoreRequestError>()
                        == Some(&StoreRequestError::NotFound) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let newer = latest.candidate.as_ref().is_none_or(|held| {
                (source.created_unix_secs, source.created_unix_nanos)
                    > (held.created_unix_secs, held.created_unix_nanos)
            });
            if newer {
                latest.candidate = Some(source);
            }
        }
        Ok(latest)
    }

    /// Capture the source-store facts that must be quiet and replayable before
    /// one Published project can cross the strict knowledge-transport boundary.
    /// The mutation lock makes uploads and journals one coherent observation.
    pub fn project_cutover_readiness(
        &self,
        project_id: &str,
    ) -> Result<KnowledgeSourceProjectCutoverReadiness> {
        validate_project_id(project_id)?;
        let _guard = self.lock_mutation()?;

        let mut prepared_upload_count = 0_u64;
        for producer in read_child_directories(&self.root.join("publications/uploads"), &[])? {
            validate_producer_id(&file_name(&producer)?)?;
            for upload_path in read_child_directories(&producer, &[])? {
                let mut upload = read_json::<PublicationUploadV1>(
                    &upload_path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "publication upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                validate_publication_upload(&mut upload)?;
                if upload.project_id == project_id && is_open(upload.state) {
                    prepared_upload_count = prepared_upload_count.saturating_add(1);
                }
            }
        }

        let mut unfinished_finalize_journal_count = 0_u64;
        for path in self.publication_journal_paths()? {
            let journal = read_json::<FinalizeJournalV1>(
                &self.root.join("journals"),
                &file_name(&path)?,
                MAX_JOURNAL_BYTES,
                "knowledge-source finalize journal",
            )?
            .ok_or(StoreRequestError::InvalidState)?;
            journal.validate()?;
            if journal.project_id == project_id && journal.stage != FinalizeStageV1::Committed {
                unfinished_finalize_journal_count =
                    unfinished_finalize_journal_count.saturating_add(1);
            }
        }

        Ok(KnowledgeSourceProjectCutoverReadiness {
            prepared_upload_count,
            unfinished_finalize_journal_count,
        })
    }

    pub fn recover(&self) -> Result<()> {
        let _guard = self.lock_mutation()?;
        for journal_path in self.publication_journal_paths()? {
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
        let graphs = load_manifest_pages(
            path,
            lane_name(SourceLaneV1::Graphs),
            record.next_pages[lane_name(SourceLaneV1::Graphs)],
            record.descriptor.graphs.page_count,
        )?;
        let evidence = load_manifest_pages(
            path,
            lane_name(SourceLaneV1::Evidence),
            record.next_pages[lane_name(SourceLaneV1::Evidence)],
            record.descriptor.evidence.page_count,
        )?;
        let config = record
            .descriptor
            .config
            .as_ref()
            .map(|config| {
                load_manifest_pages(
                    path,
                    lane_name(SourceLaneV1::Config),
                    *record
                        .next_pages
                        .get(lane_name(SourceLaneV1::Config))
                        .ok_or(StoreRequestError::InvalidState)?,
                    config.page_count,
                )
            })
            .transpose()?;
        validate_publication_candidate(
            &record.descriptor,
            &knowledge,
            &gaps,
            &graphs,
            &evidence,
            config.as_deref(),
            self.current_limits()?.contract,
        )?;
        let directory = existing_directory(path)?;
        install_immutable_json(&directory, "manifest-knowledge.json", &knowledge)?;
        install_immutable_json(&directory, "manifest-gaps.json", &gaps)?;
        install_immutable_json(&directory, "manifest-graphs.json", &graphs)?;
        install_immutable_json(&directory, "manifest-evidence.json", &evidence)?;
        if let Some(config) = &config {
            install_immutable_json(&directory, "manifest-config.json", config)?;
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
        // The manifests already admitted this entry, including whether a
        // zero-byte body is legal for its path (graph vertices/edges files
        // may be empty). Validate the body against a name that carries the
        // same emptiness rule rather than a synthetic knowledge-record name,
        // which would refuse every legitimately empty graph blob.
        let entry = SourceFileManifestEntryV1 {
            repository_relative_filename: if expected_size == 0 {
                ".bbox/graphs/blob-validation/vertices.jsonl".to_string()
            } else {
                "blob-validation.json".to_string()
            },
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
            return Ok(finalize_response(upload.source_generation_id));
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
                checksum_sha256: String::new(),
            })?,
        };
        let manifests = load_publication_manifests(&path, &upload.descriptor)?;
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
        install_immutable_record(&directory, "descriptor.json", &upload.descriptor)?;
        install_immutable_json(&directory, "manifest-knowledge.json", &manifests.0)?;
        install_immutable_json(&directory, "manifest-gaps.json", &manifests.1)?;
        install_immutable_json(&directory, "manifest-graphs.json", &manifests.2)?;
        install_immutable_json(&directory, "manifest-evidence.json", &manifests.3)?;
        if let Some(config) = &manifests.4 {
            install_immutable_json(&directory, "manifest-config.json", config)?;
        }
        install_immutable_record(&directory, "source.json", &generation)?;
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
        Ok(finalize_response(upload.source_generation_id))
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
        let manifests = load_publication_manifests(&generation_path, &source.descriptor)?;
        validate_publication_candidate(
            &source.descriptor,
            &manifests.0,
            &manifests.1,
            &manifests.2,
            &manifests.3,
            manifests.4.as_deref(),
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
        let root = self.root.join("journals");
        let name = journal_filename(&journal.source_generation_id);
        if let Some(existing) = read_json::<FinalizeJournalV1>(
            &root,
            &name,
            MAX_JOURNAL_BYTES,
            "knowledge-source finalize journal",
        )? {
            existing.validate()?;
            if !journal.has_same_identity(&existing) || journal.stage.rank() < existing.stage.rank()
            {
                bail!(StoreRequestError::Conflict);
            }
            if journal.stage == existing.stage {
                if journal != existing {
                    bail!(StoreRequestError::Conflict);
                }
                return Ok(existing);
            }
        }
        write_json(&existing_directory(&root)?, &name, &journal)?;
        Ok(journal)
    }

    fn load_publication_upload(
        &self,
        path: &Path,
        authority: &PublicationAuthorityV1,
        upload_id: &str,
    ) -> Result<PublicationUploadV1> {
        let mut record = read_json::<PublicationUploadV1>(
            path,
            "upload.json",
            MAX_UPLOAD_RECORD_BYTES,
            "publication upload",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        validate_publication_upload(&mut record)?;
        if record.upload_id != upload_id
            || record.producer_id != authority.producer_id
            || record.project_id != authority.project_id
            || record.descriptor.scope != authority.scope
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

    /// Every publication finalize journal, sorted. A legacy provisional
    /// journal is skipped by name and never parsed; any other unexpected
    /// member stops the scan as before.
    fn publication_journal_paths(&self) -> Result<Vec<PathBuf>> {
        read_regular_json_files_except(
            &self.root.join("journals"),
            Some(RETIRED_PROVISIONAL_JOURNAL_PREFIX),
        )
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
                let mut upload = read_json::<PublicationUploadV1>(
                    &upload_path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "publication upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                validate_publication_upload(&mut upload)?;
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
        let resumed_publication_retirements = self.resume_retiring_generations()?;
        let expired_uploads = self.expire_uploads(now)?;
        let new_publication_retirements =
            self.retire_old_generations(&protected_publication_generations)?;
        let retired_publication_generations =
            resumed_publication_retirements.saturating_add(new_publication_retirements);
        let referenced = self.referenced_blob_hashes()?;
        let (deleted_blobs, deleted_blob_bytes) =
            self.sweep_unreferenced_blobs(&referenced, now)?;
        Ok(MaintenanceReport {
            expired_uploads,
            retired_publication_generations,
            deleted_blobs,
            deleted_blob_bytes,
        })
    }

    fn resume_retiring_generations(&self) -> Result<u64> {
        let mut publications = 0_u64;
        for path in self.publication_journal_paths()? {
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
            publications = publications.saturating_add(1);
        }
        Ok(publications)
    }

    fn expire_uploads(&self, now: u64) -> Result<u64> {
        let limits = self.current_limits()?;
        let protected = self.unfinished_journal_uploads()?;
        let mut expired = 0_u64;
        for producer in read_child_directories(&self.root.join("publications/uploads"), &[])? {
            let producer_name = file_name(&producer)?;
            validate_producer_id(&producer_name)?;
            for upload_path in read_child_directories(&producer, &[])? {
                let mut upload = read_json::<PublicationUploadV1>(
                    &upload_path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "publication upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                validate_publication_upload(&mut upload)?;
                if is_open(upload.state)
                    && now.saturating_sub(upload.updated_unix_secs) >= limits.upload_idle_ttl_secs
                    && !protected.contains(&(FinalizeKindV1::Publication, upload.upload_id.clone()))
                {
                    remove_upload_directory(&upload_path)?;
                    expired += 1;
                }
            }
        }
        Ok(expired)
    }

    fn unfinished_journal_uploads(&self) -> Result<BTreeSet<(FinalizeKindV1, String)>> {
        let mut protected = BTreeSet::new();
        for path in self.publication_journal_paths()? {
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

    fn retire_old_generations(
        &self,
        protected_publication_generations: &BTreeSet<String>,
    ) -> Result<u64> {
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
                self.retire_finalized_generation(&generation_id)?;
                retired_publication += 1;
            }
        }

        Ok(retired_publication)
    }

    fn retire_finalized_generation(&self, generation_id: &str) -> Result<()> {
        let journal_name = journal_filename(generation_id);
        let mut journal = read_json::<FinalizeJournalV1>(
            &self.root.join("journals"),
            &journal_name,
            MAX_JOURNAL_BYTES,
            "knowledge-source finalize journal",
        )?
        .ok_or(StoreRequestError::InvalidState)?;
        journal.validate()?;
        if journal.stage != FinalizeStageV1::Committed
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
                    remove_generation_directory(&generation)?;
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
                    remove_upload_directory(&upload)?;
                }
            }
        }
        remove_regular_file(
            &self
                .root
                .join("journals")
                .join(journal_filename(&journal.source_generation_id)),
        )
    }

    fn journal_generation_roots(&self) -> Result<BTreeSet<String>> {
        let mut roots = BTreeSet::new();
        for path in self.publication_journal_paths()? {
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
        collect_manifest_hashes(&self.root.join("publications/generations"), &mut hashes)?;
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
        GitObjectFormatV1, SCHEMA_VERSION, legacy_publication_candidate_generation_id,
        pre_evidence_publication_candidate_generation_id, source_file_blob_sha256,
        source_manifest_sha256,
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
            graphs: SourceManifestDescriptorV1::default(),
            evidence: SourceManifestDescriptorV1::default(),
            config: None,
        };
        (descriptor, knowledge, gaps)
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
    fn finalize_journals_refuse_identity_changes_and_stage_regressions() {
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
        let generation = store
            .finalize_publication_upload(&authority, &begin.upload_id)
            .unwrap()
            .source_generation_id;
        let journal = read_json::<FinalizeJournalV1>(
            &root.join("journals"),
            &journal_filename(&generation),
            MAX_JOURNAL_BYTES,
            "test journal",
        )
        .unwrap()
        .unwrap();

        let mut regressed = journal.clone();
        regressed.stage = FinalizeStageV1::Prepared;
        assert_store_error(
            store.write_finalize_journal(regressed),
            StoreRequestError::Conflict,
        );
        let mut changed_identity = journal;
        changed_identity.stage = FinalizeStageV1::Retiring;
        changed_identity.upload_id = "f".repeat(32);
        assert_store_error(
            store.write_finalize_journal(changed_identity),
            StoreRequestError::Conflict,
        );
    }

    #[test]
    fn cutover_readiness_is_lock_consistent_and_refuses_unquiet_source_state() {
        let (_temporary, _root, store) = test_store(StoreLimits::default());
        let authority = publication_authority();
        let (descriptor, knowledge, gaps) = publication_fixture();
        let begin = store
            .begin_publication_upload(&authority, descriptor)
            .unwrap();

        let prepared = store
            .project_cutover_readiness(&authority.project_id)
            .unwrap();
        assert_eq!(prepared.prepared_upload_count, 1);
        assert_eq!(prepared.unfinished_finalize_journal_count, 0);

        put_publication_pages(&store, &authority, &begin.upload_id, &knowledge, &gaps);
        store
            .missing_publication_blobs(&authority, &begin.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &authority, &begin.upload_id);
        store
            .finalize_publication_upload(&authority, &begin.upload_id)
            .unwrap();
        let ready = store
            .project_cutover_readiness(&authority.project_id)
            .unwrap();
        assert_eq!(ready.prepared_upload_count, 0);
        assert_eq!(ready.unfinished_finalize_journal_count, 0);

        store
            .write_finalize_journal(
                FinalizeJournalV1 {
                    version: STORE_VERSION,
                    kind: FinalizeKindV1::Publication,
                    stage: FinalizeStageV1::Prepared,
                    upload_id: "f".repeat(32),
                    source_generation_id: format!("kps_{}", "e".repeat(64)),
                    authority_key: authority.producer_id.clone(),
                    project_id: authority.project_id.clone(),
                    created_unix_secs: 1,
                    created_unix_nanos: 1,
                    checksum_sha256: String::new(),
                }
                .seal()
                .unwrap(),
            )
            .unwrap();
        let unquiet = store
            .project_cutover_readiness(&authority.project_id)
            .unwrap();
        assert_eq!(unquiet.unfinished_finalize_journal_count, 1);
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
                checksum_sha256: String::new(),
            })
            .unwrap();
        let generation_path = store
            .publication_generation_path(&authority.project_id, &upload.source_generation_id)
            .unwrap();
        let directory = NofollowDirectory::open_or_create(&generation_path).unwrap();
        let manifests = load_publication_manifests(&upload_path, &upload.descriptor).unwrap();
        install_immutable_json(&directory, "descriptor.json", &upload.descriptor).unwrap();
        install_immutable_json(&directory, "manifest-knowledge.json", &manifests.0).unwrap();
        install_immutable_json(&directory, "manifest-gaps.json", &manifests.1).unwrap();
        install_immutable_json(&directory, "manifest-graphs.json", &manifests.2).unwrap();
        install_immutable_json(&directory, "manifest-evidence.json", &manifests.3).unwrap();
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
            &journal_filename(&upload.source_generation_id),
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
            graphs: SourceManifestDescriptorV1::default(),
            evidence: SourceManifestDescriptorV1::default(),
            config: None,
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
                .join(journal_filename(&first_generation))
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

        let journal_name = journal_filename(&first_generation);
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
        remove_generation_directory(&first_generation_path).unwrap();
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

        let journal_name = journal_filename(&generation);
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
        remove_generation_directory(&generation_path).unwrap();

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

    // ---- pre-graphs-lane state ----
    //
    // The records below are the shapes the binary that predates the graphs
    // lane wrote: the current records minus every graph field, in the same
    // field order, so a downgraded fixture is byte-faithful to what a
    // production state volume actually holds.

    #[derive(Serialize, Deserialize)]
    struct LegacyPublicationDescriptor {
        schema_version: u32,
        scope: PublishedScope,
        full_ref: String,
        publisher_commit: String,
        object_format: bbox_knowledge_source::GitObjectFormatV1,
        knowledge: SourceManifestDescriptorV1,
        gaps: SourceManifestDescriptorV1,
    }

    impl From<PublicationCandidateDescriptorV1> for LegacyPublicationDescriptor {
        fn from(descriptor: PublicationCandidateDescriptorV1) -> Self {
            assert!(
                descriptor.graphs.is_absent_lane() && descriptor.evidence.is_absent_lane(),
                "only empty graph and evidence lanes have a pre-graphs vintage"
            );
            Self {
                schema_version: descriptor.schema_version,
                scope: descriptor.scope,
                full_ref: descriptor.full_ref,
                publisher_commit: descriptor.publisher_commit,
                object_format: descriptor.object_format,
                knowledge: descriptor.knowledge,
                gaps: descriptor.gaps,
            }
        }
    }

    #[derive(Serialize, Deserialize)]
    struct LegacyPublicationUpload {
        version: u32,
        upload_id: String,
        producer_id: String,
        project_id: String,
        descriptor: LegacyPublicationDescriptor,
        source_generation_id: String,
        state: SourceGenerationStateV1,
        next_pages: BTreeMap<String, u64>,
        page_digests: BTreeMap<String, String>,
        updated_unix_secs: u64,
    }

    #[derive(Serialize, Deserialize)]
    struct LegacyStoredPublication {
        version: u32,
        source_generation_id: String,
        producer_id: String,
        project_id: String,
        descriptor: LegacyPublicationDescriptor,
        state: SourceGenerationStateV1,
        created_unix_secs: u64,
        created_unix_nanos: u128,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<String>,
    }

    /// Every lane the pre-graphs binary never had. Both must leave the store
    /// for a downgraded fixture to be byte-faithful, because the current binary
    /// writes cursors, page directories, and manifests for both.
    const PRE_GRAPHS_ABSENT_LANES: &[SourceLaneV1] =
        &[SourceLaneV1::Graphs, SourceLaneV1::Evidence];

    /// The one lane the pre-evidence binary never had.
    const PRE_EVIDENCE_ABSENT_LANES: &[SourceLaneV1] = &[SourceLaneV1::Evidence];

    fn drop_lane_slots(
        slots: BTreeMap<String, u64>,
        lanes: &[SourceLaneV1],
    ) -> BTreeMap<String, u64> {
        slots
            .into_iter()
            .filter(|(slot, _)| !mentions_any_lane(slot, lanes))
            .collect()
    }

    fn drop_lane_digests(
        digests: BTreeMap<String, String>,
        lanes: &[SourceLaneV1],
    ) -> BTreeMap<String, String> {
        digests
            .into_iter()
            .filter(|(slot, _)| !mentions_any_lane(slot, lanes))
            .collect()
    }

    /// Page-cursor keys are `<lane>` and a digest key appends `/<page>`, so a
    /// lane match is one segment of the split key.
    fn mentions_any_lane(key: &str, lanes: &[SourceLaneV1]) -> bool {
        key.split(['/', '_'])
            .any(|segment| lanes.iter().any(|lane| segment == lane_name(*lane)))
    }

    /// Rewrite a store the current binary produced into the on-disk shape the
    /// pre-graphs-lane binary produced: descriptors with no graph lanes, page
    /// cursors with no graph slots, no graph page directories, no graph
    /// manifests, and the pre-graphs generation identity wherever it appears.
    fn downgrade_store_to_pre_graphs(root: &Path) {
        let mut substitutions = BTreeMap::new();
        collect_pre_graphs_substitutions(root, &mut substitutions);
        assert!(
            !substitutions.is_empty(),
            "the fixture must hold at least one downgradable resource"
        );
        rewrite_records_to_pre_graphs(root);
        substitute_generation_strings(root, &substitutions);
        for lane in PRE_GRAPHS_ABSENT_LANES {
            remove_lane_members(root, *lane);
        }
        reseal_journals(root);
        rename_to_substituted_ids(root, &substitutions);
    }

    fn json_children(path: &Path) -> Vec<PathBuf> {
        let mut children: Vec<PathBuf> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        children.sort();
        children
    }

    fn collect_pre_graphs_substitutions(path: &Path, substitutions: &mut BTreeMap<String, String>) {
        for child in json_children(path) {
            if child.is_dir() {
                collect_pre_graphs_substitutions(&child, substitutions);
                continue;
            }
            if child.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(&child).unwrap()).unwrap();
            let Some(record) = value.as_object() else {
                continue;
            };
            let (Some(stored_id), Some(descriptor)) = (
                record
                    .get("source_generation_id")
                    .and_then(|id| id.as_str()),
                record.get("descriptor"),
            ) else {
                continue;
            };
            if descriptor.get("knowledge").is_some() {
                let descriptor: PublicationCandidateDescriptorV1 =
                    serde_json::from_value(descriptor.clone()).unwrap();
                let producer_id = record
                    .get("producer_id")
                    .and_then(|id| id.as_str())
                    .expect("a publication record names its producer");
                substitutions.insert(
                    stored_id.to_string(),
                    legacy_publication_candidate_generation_id(producer_id, &descriptor),
                );
            }
        }
    }

    fn rewrite_records_to_pre_graphs(path: &Path) {
        for child in json_children(path) {
            if child.is_dir() {
                rewrite_records_to_pre_graphs(&child);
                continue;
            }
            let name = file_name(&child).unwrap();
            let bytes = match name.as_str() {
                "upload.json" | "source.json" | "descriptor.json" => fs::read(&child).unwrap(),
                _ => continue,
            };
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let publication = match name.as_str() {
                "descriptor.json" => value.get("knowledge").is_some(),
                _ => value
                    .get("descriptor")
                    .and_then(|descriptor| descriptor.get("knowledge"))
                    .is_some(),
            };
            let legacy = match (name.as_str(), publication) {
                ("descriptor.json", true) => {
                    serde_json::to_vec_pretty(&LegacyPublicationDescriptor::from(
                        serde_json::from_value::<PublicationCandidateDescriptorV1>(value).unwrap(),
                    ))
                }
                ("upload.json", true) => {
                    let record: PublicationUploadV1 = serde_json::from_value(value).unwrap();
                    serde_json::to_vec_pretty(&LegacyPublicationUpload {
                        version: record.version,
                        upload_id: record.upload_id,
                        producer_id: record.producer_id,
                        project_id: record.project_id,
                        descriptor: record.descriptor.into(),
                        source_generation_id: record.source_generation_id,
                        state: record.state,
                        next_pages: drop_lane_slots(record.next_pages, PRE_GRAPHS_ABSENT_LANES),
                        page_digests: drop_lane_digests(
                            record.page_digests,
                            PRE_GRAPHS_ABSENT_LANES,
                        ),
                        updated_unix_secs: record.updated_unix_secs,
                    })
                }
                ("source.json", true) => {
                    let record: StoredPublicationCandidateV1 =
                        serde_json::from_value(value).unwrap();
                    serde_json::to_vec_pretty(&LegacyStoredPublication {
                        version: record.version,
                        source_generation_id: record.source_generation_id,
                        producer_id: record.producer_id,
                        project_id: record.project_id,
                        descriptor: record.descriptor.into(),
                        state: record.state,
                        created_unix_secs: record.created_unix_secs,
                        created_unix_nanos: record.created_unix_nanos,
                        diagnostic: record.diagnostic,
                    })
                }
                _ => unreachable!("every legacy record shape is handled"),
            };
            fs::write(&child, legacy.unwrap()).unwrap();
        }
    }

    /// Rewrite every stored generation id and working-pair commitment to its
    /// older-vintage substitute. Lane-neutral: both downgraders use it.
    fn substitute_generation_strings(path: &Path, substitutions: &BTreeMap<String, String>) {
        for child in json_children(path) {
            if child.is_dir() {
                substitute_generation_strings(&child, substitutions);
                continue;
            }
            if child.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let mut text = String::from_utf8(fs::read(&child).unwrap()).unwrap();
            for (current, legacy) in substitutions {
                text = text.replace(current.as_str(), legacy.as_str());
            }
            fs::write(&child, text.as_bytes()).unwrap();
        }
    }

    /// Strip one lane from a store: its page directories and all three of its
    /// manifest filenames, exactly as a binary predating the lane left things.
    fn remove_lane_members(path: &Path, lane: SourceLaneV1) {
        let directory = lane_name(lane);
        let manifests = [
            format!("manifest-{directory}.json"),
            format!("manifest-baseline-{directory}.json"),
            format!("manifest-working-{directory}.json"),
        ];
        for child in json_children(path) {
            let name = file_name(&child).unwrap();
            if child.is_dir() {
                if name == directory {
                    fs::remove_dir_all(&child).unwrap();
                    continue;
                }
                remove_lane_members(&child, lane);
            } else if manifests.iter().any(|manifest| manifest == &name) {
                fs::remove_file(&child).unwrap();
            }
        }
    }

    fn reseal_journals(root: &Path) {
        let journals = root.join("journals");
        for child in json_children(&journals) {
            let journal: FinalizeJournalV1 =
                serde_json::from_slice(&fs::read(&child).unwrap()).unwrap();
            let resealed = journal.seal().unwrap();
            fs::write(&child, serde_json::to_vec_pretty(&resealed).unwrap()).unwrap();
        }
    }

    /// Rename every id-named member onto its older-vintage id. Lane-neutral.
    fn rename_to_substituted_ids(path: &Path, substitutions: &BTreeMap<String, String>) {
        for child in json_children(path) {
            if child.is_dir() {
                rename_to_substituted_ids(&child, substitutions);
            }
            let name = file_name(&child).unwrap();
            let mut renamed = name.clone();
            for (current, legacy) in substitutions {
                renamed = renamed.replace(current.as_str(), legacy.as_str());
            }
            if renamed != name {
                fs::rename(&child, child.with_file_name(renamed)).unwrap();
            }
        }
    }

    /// A production-shaped store the current binary writes, which either
    /// downgrader then rewrites into an older vintage: one accepted publication
    /// generation with its committed finalize journal, one publication upload
    /// interrupted after its generation was installed, one publication upload
    /// still mid-manifest.
    /// The fixture keeps several uploads open at once, which the default
    /// per-authority ceiling of two would refuse.
    fn vintage_fixture_limits() -> StoreLimits {
        StoreLimits {
            max_open_uploads_per_authority: 4,
            ..StoreLimits::default()
        }
    }

    fn vintage_fixture_state(root: &Path) -> VintageFixtureState {
        let store = KnowledgeSourceStore::open(root, vintage_fixture_limits()).unwrap();
        let publisher = publication_authority();
        let (descriptor, knowledge, gaps) = publication_fixture();
        let accepted = store
            .begin_publication_upload(&publisher, descriptor)
            .unwrap();
        put_publication_pages(&store, &publisher, &accepted.upload_id, &knowledge, &gaps);
        store
            .missing_publication_blobs(&publisher, &accepted.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &publisher, &accepted.upload_id);
        let accepted_generation = store
            .finalize_publication_upload(&publisher, &accepted.upload_id)
            .unwrap()
            .source_generation_id;

        let (mut interrupted_descriptor, _, _) = publication_fixture();
        interrupted_descriptor.publisher_commit = "2".repeat(40);
        let interrupted = store
            .begin_publication_upload(&publisher, interrupted_descriptor)
            .unwrap();
        put_publication_pages(
            &store,
            &publisher,
            &interrupted.upload_id,
            &knowledge,
            &gaps,
        );
        store
            .missing_publication_blobs(&publisher, &interrupted.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &publisher, &interrupted.upload_id);
        install_publication_generation_without_index(&store, &publisher, &interrupted.upload_id);

        // An upload the producer never finished sending manifest pages for.
        // Its page cursors are the pre-graphs set, so resuming it after the
        // graphs lane landed has to tolerate the absent lane cursor.
        let (mut resumable_descriptor, _, _) = publication_fixture();
        resumable_descriptor.publisher_commit = "4".repeat(40);
        let resumable = store
            .begin_publication_upload(&publisher, resumable_descriptor)
            .unwrap();
        store
            .put_publication_manifest_page(
                &publisher,
                &resumable.upload_id,
                SourceLaneV1::Knowledge,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: knowledge.clone(),
                },
            )
            .unwrap();

        drop(store);
        VintageFixtureState {
            accepted_generation,
            interrupted_upload: interrupted.upload_id,
            resumable_upload: resumable.upload_id,
        }
    }

    struct VintageFixtureState {
        accepted_generation: String,
        interrupted_upload: String,
        resumable_upload: String,
    }

    /// The identity a resource carries once the store is downgraded: the
    /// pre-graphs mint of the same descriptor.
    fn legacy_publication_generation(descriptor: &PublicationCandidateDescriptorV1) -> String {
        legacy_publication_candidate_generation_id(&publication_authority().producer_id, descriptor)
    }

    /// Replay the exact crash window between generation install and generation
    /// index install, leaving a `GenerationInstalled` journal behind.
    fn install_publication_generation_without_index(
        store: &KnowledgeSourceStore,
        authority: &PublicationAuthorityV1,
        upload_id: &str,
    ) {
        let upload_path = store
            .publication_upload_path(&authority.producer_id, upload_id)
            .unwrap();
        let upload = store
            .load_publication_upload(&upload_path, authority, upload_id)
            .unwrap();
        let mut journal = store
            .write_finalize_journal(FinalizeJournalV1 {
                version: STORE_VERSION,
                kind: FinalizeKindV1::Publication,
                stage: FinalizeStageV1::Prepared,
                upload_id: upload_id.to_string(),
                source_generation_id: upload.source_generation_id.clone(),
                authority_key: authority.producer_id.clone(),
                project_id: authority.project_id.clone(),
                created_unix_secs: 1,
                created_unix_nanos: 1,
                checksum_sha256: String::new(),
            })
            .unwrap();
        let generation_path = store
            .publication_generation_path(&authority.project_id, &upload.source_generation_id)
            .unwrap();
        let directory = NofollowDirectory::open_or_create(&generation_path).unwrap();
        let manifests = load_publication_manifests(&upload_path, &upload.descriptor).unwrap();
        install_immutable_json(&directory, "descriptor.json", &upload.descriptor).unwrap();
        install_immutable_json(&directory, "manifest-knowledge.json", &manifests.0).unwrap();
        install_immutable_json(&directory, "manifest-gaps.json", &manifests.1).unwrap();
        install_immutable_json(&directory, "manifest-graphs.json", &manifests.2).unwrap();
        install_immutable_json(&directory, "manifest-evidence.json", &manifests.3).unwrap();
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
    }

    #[test]
    fn pre_graphs_state_opens_and_recovers_every_resource() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("source-store");
        let state = vintage_fixture_state(&root);
        downgrade_store_to_pre_graphs(&root);

        // This is the production failure: opening a state volume that a
        // binary predating the graphs lane wrote.
        let store = KnowledgeSourceStore::open(&root, StoreLimits::default()).unwrap();

        // Every legacy resource kept the identity its own binary minted.
        let publisher = publication_authority();
        let accepted_generation = legacy_publication_generation(&publication_fixture().0);
        assert_ne!(accepted_generation, state.accepted_generation);
        let accepted = store
            .publication_status(&publisher.producer_id, &accepted_generation)
            .unwrap();
        assert_eq!(accepted.state, SourceGenerationStateV1::Ready);
        assert_eq!(accepted.knowledge_files, 1);
        assert_eq!(accepted.graph_files, 0);
        assert_eq!(accepted.graph_manifest_sha256, empty_lane_sha256());
        let pinned = store
            .pin_ready_publication_candidate(&accepted_generation)
            .unwrap();
        assert_eq!(pinned.candidate().knowledge.len(), 1);
        assert!(pinned.candidate().graphs.is_empty());
        drop(pinned);

        // The publication interrupted between generation install and index
        // install was replayed by recovery onto its own legacy identity.
        let mut interrupted_descriptor = publication_fixture().0;
        interrupted_descriptor.publisher_commit = "2".repeat(40);
        let interrupted_generation = legacy_publication_generation(&interrupted_descriptor);
        assert_eq!(
            store
                .publication_status(&publisher.producer_id, &interrupted_generation)
                .unwrap()
                .state,
            SourceGenerationStateV1::Ready
        );
        assert_eq!(
            store
                .finalize_publication_upload(&publisher, &state.interrupted_upload)
                .unwrap()
                .source_generation_id,
            interrupted_generation
        );

        // The half-sent upload resumes on the same durable upload id and
        // finishes through a lane its own binary never knew about.
        let mut resumable_descriptor = publication_fixture().0;
        resumable_descriptor.publisher_commit = "4".repeat(40);
        assert_eq!(
            store
                .begin_publication_upload(&publisher, resumable_descriptor.clone())
                .unwrap()
                .upload_id,
            state.resumable_upload
        );
        let (_, _, gaps) = publication_fixture();
        store
            .put_publication_manifest_page(
                &publisher,
                &state.resumable_upload,
                SourceLaneV1::Gaps,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: gaps,
                },
            )
            .unwrap();
        store
            .missing_publication_blobs(&publisher, &state.resumable_upload, None)
            .unwrap();
        assert_eq!(
            store
                .finalize_publication_upload(&publisher, &state.resumable_upload)
                .unwrap()
                .source_generation_id,
            legacy_publication_generation(&resumable_descriptor)
        );
    }

    /// Legacy tolerance is keyed on an absent lane, so state that claims
    /// graph content and cannot produce it stays a refusal, and an immutable
    /// record whose stored bytes decode to different content still conflicts.
    #[test]
    fn malformed_graph_lane_state_is_still_refused() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let publisher = publication_authority();
        let (mut descriptor, knowledge, gaps) = publication_fixture();
        let graphs = graph_fixture_entries();
        descriptor.graphs = manifest(SourceLaneV1::Graphs, &graphs);
        let begin = store
            .begin_publication_upload(&publisher, descriptor.clone())
            .unwrap();
        put_publication_pages(&store, &publisher, &begin.upload_id, &knowledge, &gaps);
        store
            .put_publication_manifest_page(
                &publisher,
                &begin.upload_id,
                SourceLaneV1::Graphs,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: graphs,
                },
            )
            .unwrap();
        store
            .missing_publication_blobs(&publisher, &begin.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &publisher, &begin.upload_id);
        for bytes in GRAPH_FIXTURE_BYTES {
            store
                .install_publication_blob(
                    &publisher,
                    &begin.upload_id,
                    &source_file_blob_sha256(bytes),
                    bytes.len() as u64,
                    Cursor::new(bytes),
                )
                .unwrap();
        }
        let generation = store
            .finalize_publication_upload(&publisher, &begin.upload_id)
            .unwrap()
            .source_generation_id;

        let generation_path = store
            .publication_generation_path(&publisher.project_id, &generation)
            .unwrap();
        fs::remove_file(generation_path.join("manifest-graphs.json")).unwrap();
        assert_store_error(
            store.pin_ready_publication_candidate(&generation),
            StoreRequestError::InvalidState,
        );

        let directory = existing_directory(&generation_path).unwrap();
        let mut drifted = descriptor;
        drifted.full_ref = "refs/heads/other".to_string();
        assert_store_error(
            install_immutable_record(&directory, "descriptor.json", &drifted),
            StoreRequestError::Conflict,
        );
        assert!(root.join("blobs/sha256").is_dir());
    }

    #[test]
    fn absent_lane_page_directory_is_created_on_first_write() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let publisher = publication_authority();
        let (mut descriptor, knowledge, gaps) = publication_fixture();
        let graphs = graph_fixture_entries();
        descriptor.graphs = manifest(SourceLaneV1::Graphs, &graphs);
        let begin = store
            .begin_publication_upload(&publisher, descriptor)
            .unwrap();
        put_publication_pages(&store, &publisher, &begin.upload_id, &knowledge, &gaps);

        // A store laid out by a binary that did not know this lane has no
        // page directory for it. The first page write creates it.
        let upload_path = store
            .publication_upload_path(&publisher.producer_id, &begin.upload_id)
            .unwrap();
        let lane_pages = upload_path
            .join("pages")
            .join(lane_name(SourceLaneV1::Graphs));
        fs::remove_dir(&lane_pages).unwrap();
        store
            .put_publication_manifest_page(
                &publisher,
                &begin.upload_id,
                SourceLaneV1::Graphs,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: graphs,
                },
            )
            .unwrap();
        assert!(lane_pages.is_dir());
        assert!(root.join("publications").is_dir());
    }

    fn empty_lane_sha256() -> String {
        SourceManifestDescriptorV1::default().manifest_sha256
    }

    #[test]
    fn pre_graphs_accepted_publication_alone_opens() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("source-store");
        let store = KnowledgeSourceStore::open(&root, StoreLimits::default()).unwrap();
        let publisher = publication_authority();
        let (descriptor, knowledge, gaps) = publication_fixture();
        let begin = store
            .begin_publication_upload(&publisher, descriptor)
            .unwrap();
        put_publication_pages(&store, &publisher, &begin.upload_id, &knowledge, &gaps);
        store
            .missing_publication_blobs(&publisher, &begin.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &publisher, &begin.upload_id);
        store
            .finalize_publication_upload(&publisher, &begin.upload_id)
            .unwrap();
        let (descriptor, ..) = publication_fixture();
        let generation =
            legacy_publication_candidate_generation_id(&publisher.producer_id, &descriptor);
        drop(store);
        downgrade_store_to_pre_graphs(&root);

        let store = KnowledgeSourceStore::open(&root, StoreLimits::default()).unwrap();
        assert_eq!(
            store
                .publication_status(&publisher.producer_id, &generation)
                .unwrap()
                .state,
            SourceGenerationStateV1::Ready
        );
    }

    #[test]
    fn pre_graphs_state_accepts_graph_content_after_opening() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("source-store");
        vintage_fixture_state(&root);
        downgrade_store_to_pre_graphs(&root);
        let store = KnowledgeSourceStore::open(&root, vintage_fixture_limits()).unwrap();

        let publisher = publication_authority();
        let (mut descriptor, knowledge, gaps) = publication_fixture();
        let graphs = graph_fixture_entries();
        descriptor.publisher_commit = "3".repeat(40);
        descriptor.graphs = manifest(SourceLaneV1::Graphs, &graphs);
        let begin = store
            .begin_publication_upload(&publisher, descriptor)
            .unwrap();
        put_publication_pages(&store, &publisher, &begin.upload_id, &knowledge, &gaps);
        store
            .put_publication_manifest_page(
                &publisher,
                &begin.upload_id,
                SourceLaneV1::Graphs,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: graphs.clone(),
                },
            )
            .unwrap();
        store
            .missing_publication_blobs(&publisher, &begin.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &publisher, &begin.upload_id);
        for bytes in GRAPH_FIXTURE_BYTES {
            store
                .install_publication_blob(
                    &publisher,
                    &begin.upload_id,
                    &source_file_blob_sha256(bytes),
                    bytes.len() as u64,
                    Cursor::new(bytes),
                )
                .unwrap();
        }
        let generation = store
            .finalize_publication_upload(&publisher, &begin.upload_id)
            .unwrap()
            .source_generation_id;
        assert_eq!(
            store
                .publication_status(&publisher.producer_id, &generation)
                .unwrap()
                .graph_files,
            3
        );
        let pinned = store.pin_ready_publication_candidate(&generation).unwrap();
        assert_eq!(pinned.candidate().graphs.len(), 3);
    }

    const GRAPH_FIXTURE_BYTES: [&[u8]; 3] = [
        br#"{"version":1}"#,
        br#"{"id":"one"}"#,
        br#"{"from":"one"}"#,
    ];

    fn graph_fixture_entries() -> Vec<SourceFileManifestEntryV1> {
        let mut entries = ["schema.json", "vertices.jsonl", "edges.jsonl"]
            .into_iter()
            .zip(GRAPH_FIXTURE_BYTES)
            .map(|(name, bytes)| entry(&format!(".bbox/graphs/records/{name}"), bytes))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.repository_relative_filename
                .cmp(&right.repository_relative_filename)
        });
        entries
    }

    // ---- pre-evidence-lane state ----
    //
    // One rung up the ladder from the pre-graphs shapes above: the records the
    // binary that knew knowledge, gaps, and graphs but not evidence wrote. Its
    // generation identities and working-pair commitments hash the graph
    // descriptor and stop there, so they are distinct from BOTH the current and
    // the pre-graphs mints of the same descriptor.

    #[derive(Serialize, Deserialize)]
    struct PreEvidencePublicationDescriptor {
        schema_version: u32,
        scope: PublishedScope,
        full_ref: String,
        publisher_commit: String,
        object_format: bbox_knowledge_source::GitObjectFormatV1,
        knowledge: SourceManifestDescriptorV1,
        gaps: SourceManifestDescriptorV1,
        graphs: SourceManifestDescriptorV1,
    }

    impl From<PublicationCandidateDescriptorV1> for PreEvidencePublicationDescriptor {
        fn from(descriptor: PublicationCandidateDescriptorV1) -> Self {
            assert!(
                descriptor.evidence.is_absent_lane(),
                "only an empty evidence lane has a pre-evidence vintage"
            );
            Self {
                schema_version: descriptor.schema_version,
                scope: descriptor.scope,
                full_ref: descriptor.full_ref,
                publisher_commit: descriptor.publisher_commit,
                object_format: descriptor.object_format,
                knowledge: descriptor.knowledge,
                gaps: descriptor.gaps,
                graphs: descriptor.graphs,
            }
        }
    }

    #[derive(Serialize, Deserialize)]
    struct PreEvidencePublicationUpload {
        version: u32,
        upload_id: String,
        producer_id: String,
        project_id: String,
        descriptor: PreEvidencePublicationDescriptor,
        source_generation_id: String,
        state: SourceGenerationStateV1,
        next_pages: BTreeMap<String, u64>,
        page_digests: BTreeMap<String, String>,
        updated_unix_secs: u64,
    }

    #[derive(Serialize, Deserialize)]
    struct PreEvidenceStoredPublication {
        version: u32,
        source_generation_id: String,
        producer_id: String,
        project_id: String,
        descriptor: PreEvidencePublicationDescriptor,
        state: SourceGenerationStateV1,
        created_unix_secs: u64,
        created_unix_nanos: u128,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<String>,
    }

    /// Rewrite a store the current binary produced into the on-disk shape the
    /// pre-evidence binary produced: descriptors with no evidence lane, page
    /// cursors with no evidence slots, no evidence page directories, no
    /// evidence manifests, working-pair commitments over knowledge, gaps, and
    /// graphs, and the pre-evidence generation identity wherever it appears.
    fn downgrade_store_to_pre_evidence(root: &Path) {
        let mut substitutions = BTreeMap::new();
        collect_pre_evidence_substitutions(root, &mut substitutions);
        assert!(
            !substitutions.is_empty(),
            "the fixture must hold at least one downgradable resource"
        );
        rewrite_records_to_pre_evidence(root);
        substitute_generation_strings(root, &substitutions);
        for lane in PRE_EVIDENCE_ABSENT_LANES {
            remove_lane_members(root, *lane);
        }
        reseal_journals(root);
        rename_to_substituted_ids(root, &substitutions);
    }

    fn collect_pre_evidence_substitutions(
        path: &Path,
        substitutions: &mut BTreeMap<String, String>,
    ) {
        for child in json_children(path) {
            if child.is_dir() {
                collect_pre_evidence_substitutions(&child, substitutions);
                continue;
            }
            if child.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(&child).unwrap()).unwrap();
            let Some(record) = value.as_object() else {
                continue;
            };
            let (Some(stored_id), Some(descriptor)) = (
                record
                    .get("source_generation_id")
                    .and_then(|id| id.as_str()),
                record.get("descriptor"),
            ) else {
                continue;
            };
            if descriptor.get("knowledge").is_some() {
                let descriptor: PublicationCandidateDescriptorV1 =
                    serde_json::from_value(descriptor.clone()).unwrap();
                let producer_id = record
                    .get("producer_id")
                    .and_then(|id| id.as_str())
                    .expect("a publication record names its producer");
                substitutions.insert(
                    stored_id.to_string(),
                    pre_evidence_publication_candidate_generation_id(producer_id, &descriptor),
                );
            }
        }
    }

    fn rewrite_records_to_pre_evidence(path: &Path) {
        for child in json_children(path) {
            if child.is_dir() {
                rewrite_records_to_pre_evidence(&child);
                continue;
            }
            let name = file_name(&child).unwrap();
            let bytes = match name.as_str() {
                "upload.json" | "source.json" | "descriptor.json" => fs::read(&child).unwrap(),
                _ => continue,
            };
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let publication = match name.as_str() {
                "descriptor.json" => value.get("knowledge").is_some(),
                _ => value
                    .get("descriptor")
                    .and_then(|descriptor| descriptor.get("knowledge"))
                    .is_some(),
            };
            let legacy = match (name.as_str(), publication) {
                ("descriptor.json", true) => {
                    serde_json::to_vec_pretty(&PreEvidencePublicationDescriptor::from(
                        serde_json::from_value::<PublicationCandidateDescriptorV1>(value).unwrap(),
                    ))
                }
                ("upload.json", true) => {
                    let record: PublicationUploadV1 = serde_json::from_value(value).unwrap();
                    serde_json::to_vec_pretty(&PreEvidencePublicationUpload {
                        version: record.version,
                        upload_id: record.upload_id,
                        producer_id: record.producer_id,
                        project_id: record.project_id,
                        descriptor: record.descriptor.into(),
                        source_generation_id: record.source_generation_id,
                        state: record.state,
                        next_pages: drop_lane_slots(record.next_pages, PRE_EVIDENCE_ABSENT_LANES),
                        page_digests: drop_lane_digests(
                            record.page_digests,
                            PRE_EVIDENCE_ABSENT_LANES,
                        ),
                        updated_unix_secs: record.updated_unix_secs,
                    })
                }
                ("source.json", true) => {
                    let record: StoredPublicationCandidateV1 =
                        serde_json::from_value(value).unwrap();
                    serde_json::to_vec_pretty(&PreEvidenceStoredPublication {
                        version: record.version,
                        source_generation_id: record.source_generation_id,
                        producer_id: record.producer_id,
                        project_id: record.project_id,
                        descriptor: record.descriptor.into(),
                        state: record.state,
                        created_unix_secs: record.created_unix_secs,
                        created_unix_nanos: record.created_unix_nanos,
                        diagnostic: record.diagnostic,
                    })
                }
                _ => unreachable!("every pre-evidence record shape is handled"),
            };
            fs::write(&child, legacy.unwrap()).unwrap();
        }
    }

    /// The identity a resource carries once the store is downgraded one rung:
    /// the pre-evidence mint of the same descriptor.
    fn pre_evidence_publication_generation(
        descriptor: &PublicationCandidateDescriptorV1,
    ) -> String {
        pre_evidence_publication_candidate_generation_id(
            &publication_authority().producer_id,
            descriptor,
        )
    }

    #[test]
    fn pre_evidence_state_opens_and_recovers_every_resource() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("source-store");
        let state = vintage_fixture_state(&root);
        downgrade_store_to_pre_evidence(&root);

        // This is the production failure: opening a state volume that a binary
        // predating the evidence lane wrote.
        let store = KnowledgeSourceStore::open(&root, StoreLimits::default()).unwrap();

        // Every pre-evidence resource kept the identity its own binary minted,
        // which is neither the current nor the pre-graphs mint.
        let publisher = publication_authority();
        let accepted_generation = pre_evidence_publication_generation(&publication_fixture().0);
        assert_ne!(accepted_generation, state.accepted_generation);
        assert_ne!(
            accepted_generation,
            legacy_publication_generation(&publication_fixture().0)
        );
        let accepted = store
            .publication_status(&publisher.producer_id, &accepted_generation)
            .unwrap();
        assert_eq!(accepted.state, SourceGenerationStateV1::Ready);
        assert_eq!(accepted.knowledge_files, 1);
        assert_eq!(accepted.evidence_files, 0);
        assert_eq!(accepted.evidence_manifest_sha256, empty_lane_sha256());
        let pinned = store
            .pin_ready_publication_candidate(&accepted_generation)
            .unwrap();
        assert_eq!(pinned.candidate().knowledge.len(), 1);
        assert!(pinned.candidate().evidence.is_empty());
        drop(pinned);

        // The publication interrupted between generation install and index
        // install was replayed by recovery onto its own pre-evidence identity.
        let mut interrupted_descriptor = publication_fixture().0;
        interrupted_descriptor.publisher_commit = "2".repeat(40);
        let interrupted_generation = pre_evidence_publication_generation(&interrupted_descriptor);
        assert_eq!(
            store
                .publication_status(&publisher.producer_id, &interrupted_generation)
                .unwrap()
                .state,
            SourceGenerationStateV1::Ready
        );
        assert_eq!(
            store
                .finalize_publication_upload(&publisher, &state.interrupted_upload)
                .unwrap()
                .source_generation_id,
            interrupted_generation
        );

        // The half-sent upload resumes on the same durable upload id and
        // finishes through a lane its own binary never knew about.
        let mut resumable_descriptor = publication_fixture().0;
        resumable_descriptor.publisher_commit = "4".repeat(40);
        assert_eq!(
            store
                .begin_publication_upload(&publisher, resumable_descriptor.clone())
                .unwrap()
                .upload_id,
            state.resumable_upload
        );
        let (_, _, gaps) = publication_fixture();
        store
            .put_publication_manifest_page(
                &publisher,
                &state.resumable_upload,
                SourceLaneV1::Gaps,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: gaps,
                },
            )
            .unwrap();
        store
            .missing_publication_blobs(&publisher, &state.resumable_upload, None)
            .unwrap();
        assert_eq!(
            store
                .finalize_publication_upload(&publisher, &state.resumable_upload)
                .unwrap()
                .source_generation_id,
            pre_evidence_publication_generation(&resumable_descriptor)
        );
    }

    #[test]
    fn pre_evidence_state_accepts_evidence_content_after_opening() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("source-store");
        vintage_fixture_state(&root);
        downgrade_store_to_pre_evidence(&root);
        let store = KnowledgeSourceStore::open(&root, vintage_fixture_limits()).unwrap();

        let publisher = publication_authority();
        let (mut descriptor, knowledge, gaps) = publication_fixture();
        let evidence = evidence_fixture_entries();
        descriptor.publisher_commit = "3".repeat(40);
        descriptor.evidence = manifest(SourceLaneV1::Evidence, &evidence);
        let begin = store
            .begin_publication_upload(&publisher, descriptor)
            .unwrap();
        put_publication_pages(&store, &publisher, &begin.upload_id, &knowledge, &gaps);
        store
            .put_publication_manifest_page(
                &publisher,
                &begin.upload_id,
                SourceLaneV1::Evidence,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: evidence,
                },
            )
            .unwrap();
        store
            .missing_publication_blobs(&publisher, &begin.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &publisher, &begin.upload_id);
        store
            .install_publication_blob(
                &publisher,
                &begin.upload_id,
                &source_file_blob_sha256(EVIDENCE_FIXTURE_BYTES),
                EVIDENCE_FIXTURE_BYTES.len() as u64,
                Cursor::new(EVIDENCE_FIXTURE_BYTES),
            )
            .unwrap();
        let generation = store
            .finalize_publication_upload(&publisher, &begin.upload_id)
            .unwrap()
            .source_generation_id;
        assert_eq!(
            store
                .publication_status(&publisher.producer_id, &generation)
                .unwrap()
                .evidence_files,
            1
        );
        let pinned = store.pin_ready_publication_candidate(&generation).unwrap();
        assert_eq!(pinned.candidate().evidence.len(), 1);
        assert_eq!(
            pinned.candidate().evidence[0]
                .manifest
                .repository_relative_filename,
            ".bbox/evidence/bindings.json"
        );
    }

    /// Legacy tolerance is keyed on an absent lane, so state that claims
    /// evidence content and cannot produce it stays a refusal.
    #[test]
    fn malformed_evidence_lane_state_is_still_refused() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let publisher = publication_authority();
        let (mut descriptor, knowledge, gaps) = publication_fixture();
        let evidence = evidence_fixture_entries();
        descriptor.evidence = manifest(SourceLaneV1::Evidence, &evidence);
        let begin = store
            .begin_publication_upload(&publisher, descriptor)
            .unwrap();
        put_publication_pages(&store, &publisher, &begin.upload_id, &knowledge, &gaps);
        store
            .put_publication_manifest_page(
                &publisher,
                &begin.upload_id,
                SourceLaneV1::Evidence,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: evidence,
                },
            )
            .unwrap();
        store
            .missing_publication_blobs(&publisher, &begin.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(&store, &publisher, &begin.upload_id);
        store
            .install_publication_blob(
                &publisher,
                &begin.upload_id,
                &source_file_blob_sha256(EVIDENCE_FIXTURE_BYTES),
                EVIDENCE_FIXTURE_BYTES.len() as u64,
                Cursor::new(EVIDENCE_FIXTURE_BYTES),
            )
            .unwrap();
        let generation = store
            .finalize_publication_upload(&publisher, &begin.upload_id)
            .unwrap()
            .source_generation_id;

        let generation_path = store
            .publication_generation_path(&publisher.project_id, &generation)
            .unwrap();
        fs::remove_file(generation_path.join("manifest-evidence.json")).unwrap();
        assert_store_error(
            store.pin_ready_publication_candidate(&generation),
            StoreRequestError::InvalidState,
        );
        assert!(root.join("blobs/sha256").is_dir());
    }

    const EVIDENCE_FIXTURE_BYTES: &[u8] = br#"{"schema_version":1,"bindings":[]}"#;

    fn evidence_fixture_entries() -> Vec<SourceFileManifestEntryV1> {
        vec![entry(
            ".bbox/evidence/bindings.json",
            EVIDENCE_FIXTURE_BYTES,
        )]
    }

    // ---- configuration lane ----
    //
    // The lane is explicitly optional: a producer that predates it sends no
    // descriptor field and the store writes exactly the records it always
    // wrote, while a present lane (even an empty one) is its own shape.

    const CONFIG_TOML_BYTES: &[u8] = b"[mcp]\nenabled = true\n";
    const MCP_STORE_BYTES: &[u8] = br#"{"servers":{}}"#;
    const BROFILE_BYTES: &[u8] = br#"{"name":"reviewer"}"#;

    fn config_files() -> Vec<(SourceFileManifestEntryV1, &'static [u8])> {
        let mut files = vec![
            (
                entry(".bbox/config.toml", CONFIG_TOML_BYTES),
                CONFIG_TOML_BYTES,
            ),
            (entry(".bbox/mcp.json", MCP_STORE_BYTES), MCP_STORE_BYTES),
            (
                entry(".bro/brofiles/reviewer.json", BROFILE_BYTES),
                BROFILE_BYTES,
            ),
        ];
        files.sort_by(|left, right| {
            left.0
                .repository_relative_filename
                .cmp(&right.0.repository_relative_filename)
        });
        files
    }

    fn with_config_lane(
        mut descriptor: PublicationCandidateDescriptorV1,
        entries: &[SourceFileManifestEntryV1],
    ) -> PublicationCandidateDescriptorV1 {
        descriptor.config = Some(manifest(SourceLaneV1::Config, entries));
        descriptor
    }

    /// Upload, complete and finalize one candidate, returning its generation.
    fn upload_candidate(
        store: &KnowledgeSourceStore,
        descriptor: PublicationCandidateDescriptorV1,
        config: &[(SourceFileManifestEntryV1, &[u8])],
    ) -> String {
        let authority = publication_authority();
        let (_, knowledge, gaps) = publication_fixture();
        let begin = store
            .begin_publication_upload(&authority, descriptor)
            .unwrap();
        put_publication_pages(store, &authority, &begin.upload_id, &knowledge, &gaps);
        if !config.is_empty() {
            store
                .put_publication_manifest_page(
                    &authority,
                    &begin.upload_id,
                    SourceLaneV1::Config,
                    0,
                    &SourceManifestPageV1 {
                        page_index: 0,
                        entries: config.iter().map(|(entry, _)| entry.clone()).collect(),
                    },
                )
                .unwrap();
        }
        store
            .missing_publication_blobs(&authority, &begin.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(store, &authority, &begin.upload_id);
        for (entry, bytes) in config {
            store
                .install_publication_blob(
                    &authority,
                    &begin.upload_id,
                    &entry.content_sha256,
                    bytes.len() as u64,
                    Cursor::new(*bytes),
                )
                .unwrap();
        }
        store
            .finalize_publication_upload(&authority, &begin.upload_id)
            .unwrap()
            .source_generation_id
    }

    fn config_manifest_entries() -> Vec<SourceFileManifestEntryV1> {
        config_files().into_iter().map(|(entry, _)| entry).collect()
    }

    #[test]
    fn the_latest_candidate_is_the_newest_stored_for_that_project() {
        let (_temporary, _root, store) = test_store(StoreLimits::default());
        let authority = publication_authority();
        assert_eq!(
            store
                .latest_publication_candidate(&authority.project_id)
                .unwrap(),
            LatestPublicationCandidateV1::default()
        );

        let (first, _, _) = publication_fixture();
        let first_generation = upload_candidate(&store, first.clone(), &[]);
        let latest = store
            .latest_publication_candidate(&authority.project_id)
            .unwrap()
            .candidate
            .unwrap();
        assert_eq!(latest.source_generation_id, first_generation);
        assert_eq!(latest.state, SourceGenerationStateV1::Ready);

        let mut second = first;
        second.publisher_commit = "2".repeat(40);
        let second_generation = upload_candidate(&store, second.clone(), &[]);
        assert_ne!(second_generation, first_generation);
        let latest = store
            .latest_publication_candidate(&authority.project_id)
            .unwrap()
            .candidate
            .unwrap();
        assert_eq!(latest.source_generation_id, second_generation);
        assert_eq!(latest.descriptor.publisher_commit, second.publisher_commit);
        assert_eq!(latest.producer_id, authority.producer_id);
        assert!(latest.created_unix_secs > 0);

        // Candidates are per project.
        assert_eq!(
            store
                .latest_publication_candidate("project-b")
                .unwrap()
                .candidate,
            None
        );
        assert_store_error(
            store.latest_publication_candidate("../project-a"),
            StoreRequestError::InvalidInput,
        );
    }

    /// Neither unlocked scan of the generation index fails on a member an
    /// atomic replace has staged there, and one undecodable entry is named
    /// without hiding the candidates beside it.
    #[test]
    fn index_scans_pass_over_staged_members_and_name_undecodable_entries() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let authority = publication_authority();
        let (descriptor, _, _) = publication_fixture();
        let generation = upload_candidate(&store, descriptor.clone(), &[]);
        let index_dir = root.join("publications/generation-index");

        fs::write(index_dir.join(".bbox-store-4242-7.tmp"), b"{\"partial\":").unwrap();
        let latest = store
            .latest_publication_candidate(&authority.project_id)
            .unwrap();
        assert_eq!(latest.candidate.unwrap().source_generation_id, generation);
        assert!(latest.unreadable_index_entries.is_empty());
        let probed = store
            .probe_publication(
                &authority,
                &descriptor.full_ref,
                &descriptor.publisher_commit,
                descriptor.object_format,
            )
            .unwrap()
            .unwrap();
        assert_eq!(probed.source_generation_id, generation);

        let undecodable = format!("kps_{}.json", "f".repeat(64));
        fs::write(index_dir.join(&undecodable), b"{not json").unwrap();
        let latest = store
            .latest_publication_candidate(&authority.project_id)
            .unwrap();
        assert_eq!(latest.candidate.unwrap().source_generation_id, generation);
        assert_eq!(latest.unreadable_index_entries, vec![undecodable.clone()]);
        // A project with no candidate still learns that an entry is unreadable.
        let other = store.latest_publication_candidate("project-b").unwrap();
        assert_eq!(other.candidate, None);
        assert_eq!(other.unreadable_index_entries, vec![undecodable]);
    }

    #[test]
    fn config_bearing_candidate_round_trips_exact_bytes_and_status() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let files = config_files();
        let descriptor = with_config_lane(publication_fixture().0, &config_manifest_entries());
        let generation = upload_candidate(&store, descriptor.clone(), &files);

        let status = store
            .publication_status(&publication_authority().producer_id, &generation)
            .unwrap();
        assert_eq!(status.config_files, Some(3));
        assert_eq!(
            status.config_manifest_sha256.as_deref(),
            Some(descriptor.config.as_ref().unwrap().manifest_sha256.as_str())
        );
        let config_bytes = files
            .iter()
            .map(|(_, bytes)| bytes.len() as u64)
            .sum::<u64>();
        assert_eq!(
            status.logical_bytes,
            (KNOWLEDGE_BYTES.len() + GAP_BYTES.len()) as u64 + config_bytes
        );
        let generation_path = root
            .join("publications/generations/project-a")
            .join(&generation);
        assert!(generation_path.join("manifest-config.json").is_file());

        let pinned = store.pin_ready_publication_candidate(&generation).unwrap();
        let config = pinned.candidate().config.as_ref().unwrap();
        assert_eq!(config.len(), 3);
        for (file, (entry, bytes)) in config.iter().zip(&files) {
            assert_eq!(&file.manifest, entry);
            assert_eq!(file.source_bytes, *bytes);
        }
    }

    #[test]
    fn empty_present_config_lane_is_distinct_from_a_pre_lane_candidate() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let pre_lane = publication_fixture().0;
        let empty = with_config_lane(publication_fixture().0, &[]);
        let authority = publication_authority();
        let pre_lane_generation = upload_candidate(&store, pre_lane.clone(), &[]);
        assert_eq!(
            pre_lane_generation,
            publication_candidate_generation_id(&authority.producer_id, &pre_lane).unwrap(),
        );
        // A pre-lane candidate is exactly the record every earlier binary
        // wrote: no descriptor field, no page slot, no manifest file.
        let generation_path = root
            .join("publications/generations/project-a")
            .join(&pre_lane_generation);
        let stored_descriptor: serde_json::Value =
            serde_json::from_slice(&fs::read(generation_path.join("descriptor.json")).unwrap())
                .unwrap();
        assert!(stored_descriptor.get("config").is_none());
        assert!(!generation_path.join("manifest-config.json").exists());
        let upload_root = root.join("publications/uploads/producer-a");
        for upload in json_children(&upload_root) {
            let record: serde_json::Value =
                serde_json::from_slice(&fs::read(upload.join("upload.json")).unwrap()).unwrap();
            assert!(record["next_pages"].get("config").is_none());
            assert!(!upload.join("pages/config").exists());
        }
        let pre_lane_status = store
            .publication_status(&authority.producer_id, &pre_lane_generation)
            .unwrap();
        assert_eq!(pre_lane_status.config_files, None);
        assert_eq!(pre_lane_status.config_manifest_sha256, None);
        let probed = store
            .probe_publication(
                &authority,
                &pre_lane.full_ref,
                &pre_lane.publisher_commit,
                pre_lane.object_format,
            )
            .unwrap()
            .unwrap();
        assert_eq!(probed.source_generation_id, pre_lane_generation);
        assert!(
            store
                .pin_ready_publication_candidate(&pre_lane_generation)
                .unwrap()
                .candidate()
                .config
                .is_none()
        );

        // The upgraded producer re-uploads the same commit with the lane.
        let empty_generation = upload_candidate(&store, empty.clone(), &[]);
        assert_ne!(empty_generation, pre_lane_generation);
        let empty_status = store
            .publication_status(&authority.producer_id, &empty_generation)
            .unwrap();
        assert_eq!(empty_status.config_files, Some(0));
        assert_eq!(
            empty_status.config_manifest_sha256.as_deref(),
            Some(source_manifest_sha256(SourceLaneV1::Config, &[]).as_str())
        );
        assert_eq!(
            store
                .pin_ready_publication_candidate(&empty_generation)
                .unwrap()
                .candidate()
                .config
                .as_deref()
                .map(<[ReadyPublicationFile]>::len),
            Some(0)
        );
        // The lane-bearing candidate at the shared commit is the current one.
        let probed = store
            .probe_publication(
                &authority,
                &empty.full_ref,
                &empty.publisher_commit,
                empty.object_format,
            )
            .unwrap()
            .unwrap();
        assert_eq!(probed.source_generation_id, empty_generation);
        assert_eq!(probed.config_files, Some(0));
    }

    #[test]
    fn config_page_for_a_lane_less_descriptor_is_refused() {
        let (_temporary, _root, store) = test_store(StoreLimits::default());
        let authority = publication_authority();
        let begin = store
            .begin_publication_upload(&authority, publication_fixture().0)
            .unwrap();
        assert_store_error(
            store.put_publication_manifest_page(
                &authority,
                &begin.upload_id,
                SourceLaneV1::Config,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: config_manifest_entries(),
                },
            ),
            StoreRequestError::InvalidInput,
        );
    }

    #[test]
    fn oversized_or_foreign_config_lanes_are_refused_before_a_generation_exists() {
        let (_temporary, _root, store) = test_store(StoreLimits::default());
        let authority = publication_authority();

        let mut too_many = publication_fixture().0;
        too_many.config = Some(SourceManifestDescriptorV1 {
            manifest_sha256: "a".repeat(64),
            file_count: bbox_knowledge_source::MAX_CONFIG_SOURCE_FILES + 1,
            logical_bytes: bbox_knowledge_source::MAX_CONFIG_SOURCE_FILES + 1,
            page_count: 2,
        });
        let error = store
            .begin_publication_upload(&authority, too_many)
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<bbox_knowledge_source::ContractError>(),
            Some(&bbox_knowledge_source::ContractError::ConfigLimitExceeded)
        );

        for (entries, expected) in [
            (
                vec![SourceFileManifestEntryV1 {
                    repository_relative_filename: ".bbox/mcp.json".into(),
                    encoded_bytes: bbox_knowledge_source::MAX_CONFIG_SOURCE_FILE_BYTES + 1,
                    content_sha256: "b".repeat(64),
                }],
                bbox_knowledge_source::ContractError::ConfigLimitExceeded,
            ),
            (
                vec![entry(".bro/brofiles/nested/reviewer.json", BROFILE_BYTES)],
                bbox_knowledge_source::ContractError::InvalidConfigSourcePath,
            ),
        ] {
            let mut descriptor = with_config_lane(publication_fixture().0, &entries);
            // Distinct commits keep each attempt a fresh upload.
            descriptor.publisher_commit = entries[0].content_sha256[..40].to_string();
            let begin = store
                .begin_publication_upload(&authority, descriptor)
                .unwrap();
            let (_, knowledge, gaps) = publication_fixture();
            put_publication_pages(&store, &authority, &begin.upload_id, &knowledge, &gaps);
            store
                .put_publication_manifest_page(
                    &authority,
                    &begin.upload_id,
                    SourceLaneV1::Config,
                    0,
                    &SourceManifestPageV1 {
                        page_index: 0,
                        entries,
                    },
                )
                .unwrap();
            let error = store
                .missing_publication_blobs(&authority, &begin.upload_id, None)
                .unwrap_err();
            assert_eq!(
                error.downcast_ref::<bbox_knowledge_source::ContractError>(),
                Some(&expected)
            );
        }
    }

    #[test]
    fn config_blobs_are_roots_while_referenced_and_reclaimed_once_orphaned() {
        let limits = StoreLimits {
            retained_publication_generations: 1,
            unreferenced_blob_grace_secs: 1,
            ..StoreLimits::default()
        };
        let (_temporary, _root, store) = test_store(limits);
        let files = config_files();
        let descriptor = with_config_lane(publication_fixture().0, &config_manifest_entries());
        upload_candidate(&store, descriptor, &files);
        let held = store.maintain_at(&BTreeSet::new(), u64::MAX).unwrap();
        assert_eq!(held.deleted_blobs, 0);
        for (entry, bytes) in &files {
            assert_eq!(
                store
                    .read_blob(&entry.content_sha256, bytes.len())
                    .unwrap()
                    .as_deref(),
                Some(*bytes)
            );
        }

        let mut successor = publication_fixture().0;
        successor.publisher_commit = "2".repeat(40);
        successor.knowledge = manifest(SourceLaneV1::Knowledge, &[]);
        successor.gaps = manifest(SourceLaneV1::Gaps, &[]);
        let successor = with_config_lane(successor, &[]);
        let authority = publication_authority();
        let begin = store
            .begin_publication_upload(&authority, successor)
            .unwrap();
        store
            .missing_publication_blobs(&authority, &begin.upload_id, None)
            .unwrap();
        store
            .finalize_publication_upload(&authority, &begin.upload_id)
            .unwrap();
        let reclaimed = store.maintain_at(&BTreeSet::new(), u64::MAX).unwrap();
        assert_eq!(reclaimed.retired_publication_generations, 1);
        assert_eq!(reclaimed.deleted_blobs, 2 + files.len() as u64);
        for (entry, bytes) in &files {
            assert!(
                store
                    .read_blob(&entry.content_sha256, bytes.len())
                    .unwrap()
                    .is_none()
            );
        }
    }

    // ---- retired provisional state ----

    /// A finalized publication: its generation, committed journal, and the
    /// blobs its manifests reference.
    fn finalized_publication(store: &KnowledgeSourceStore) -> String {
        let authority = publication_authority();
        let (descriptor, knowledge, gaps) = publication_fixture();
        let begin = store
            .begin_publication_upload(&authority, descriptor)
            .unwrap();
        put_publication_pages(&store, &authority, &begin.upload_id, &knowledge, &gaps);
        store
            .missing_publication_blobs(&authority, &begin.upload_id, None)
            .unwrap();
        install_fixture_blobs_publication(store, &authority, &begin.upload_id);
        store
            .finalize_publication_upload(&authority, &begin.upload_id)
            .unwrap()
            .source_generation_id
    }

    /// Lay down the shape an older store left for provisional workspace
    /// snapshots: uploads, generations with manifests, a generation index,
    /// a symlink pointing outside the store, and journals. Contents are
    /// deliberately not valid current records, so any read of them fails.
    /// Returns the hash of a blob only the legacy manifest references.
    fn write_legacy_provisional_state(root: &Path, outside: &Path) -> String {
        let workspace = "0123456789abcdef0123456789abcdef";
        let generation = format!("kws_{}", "a".repeat(64));
        let upload = root
            .join("provisional/uploads")
            .join(workspace)
            .join("b".repeat(32));
        fs::create_dir_all(upload.join("pages/working/knowledge")).unwrap();
        fs::create_dir_all(upload.join("ancestry")).unwrap();
        fs::write(upload.join("upload.json"), b"{\"version\":0}").unwrap();
        fs::write(
            upload.join("pages/working/knowledge/00000000000000000000.json"),
            b"{}",
        )
        .unwrap();
        let generation_dir = root
            .join("provisional/generations/project-a")
            .join(workspace)
            .join(&generation);
        fs::create_dir_all(generation_dir.join("../sequences")).unwrap();
        let only_provisional = b"only referenced by a provisional manifest";
        let hash = source_file_blob_sha256(only_provisional);
        let shard = root.join("blobs/sha256").join(&hash[..2]);
        fs::create_dir_all(&shard).unwrap();
        fs::write(shard.join(&hash[2..]), only_provisional).unwrap();
        fs::write(
            generation_dir.join("manifest-working-knowledge.json"),
            serde_json::to_vec(&vec![entry(
                ".bbox/knowledge/legacy.json",
                only_provisional,
            )])
            .unwrap(),
        )
        .unwrap();
        fs::write(generation_dir.join("source.json"), b"not json").unwrap();
        fs::write(generation_dir.join("../current.json"), b"{\"stale\":true}").unwrap();
        fs::create_dir_all(root.join("provisional/generation-index")).unwrap();
        fs::write(
            root.join("provisional/generation-index")
                .join(format!("{generation}.json")),
            b"{}",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside, root.join("provisional/escape")).unwrap();
        fs::write(
            root.join("journals")
                .join(format!("provisional-{generation}.json")),
            b"{\"kind\":\"provisional\"}",
        )
        .unwrap();
        fs::write(
            root.join("journals")
                .join(format!("provisional-kws_{}.json", "c".repeat(64))),
            b"truncated",
        )
        .unwrap();
        hash
    }

    fn outside_target(temporary: &TempDir) -> PathBuf {
        let outside = temporary.path().canonicalize().unwrap().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.json"), b"{}").unwrap();
        outside
    }

    #[test]
    fn retiring_provisional_state_on_an_absent_root_removes_nothing() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("never-created");
        assert_eq!(
            retire_provisional_state(&root).unwrap(),
            ProvisionalRetirementReport::default()
        );
        assert!(!root.exists());
    }

    #[test]
    fn retiring_provisional_state_on_a_never_provisioned_store_removes_nothing() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let generation = finalized_publication(&store);
        drop(store);
        assert!(!root.join("provisional").exists());
        assert_eq!(
            retire_provisional_state(&root).unwrap(),
            ProvisionalRetirementReport::default()
        );
        let store = KnowledgeSourceStore::open(&root, StoreLimits::default()).unwrap();
        assert_eq!(
            store
                .publication_status(&publication_authority().producer_id, &generation)
                .unwrap()
                .state,
            SourceGenerationStateV1::Ready
        );
    }

    #[test]
    fn retiring_legacy_provisional_state_is_exact_and_idempotent() {
        let (temporary, root, store) = test_store(StoreLimits::default());
        let generation = finalized_publication(&store);
        drop(store);
        let outside = outside_target(&temporary);
        write_legacy_provisional_state(&root, &outside);
        // A non-regular journal under the retired prefix is left alone.
        fs::create_dir_all(root.join("journals/provisional-directory.json")).unwrap();
        let publication_journal = root.join("journals").join(journal_filename(&generation));
        assert!(publication_journal.exists());

        let report = retire_provisional_state(&root).unwrap();
        assert_eq!(
            report,
            ProvisionalRetirementReport {
                removed_directory: true,
                removed_journals: 2,
            }
        );
        assert!(!root.join("provisional").exists());
        assert!(
            outside.join("keep.json").exists(),
            "symlink target survives"
        );
        assert!(publication_journal.exists());
        assert!(root.join("journals/provisional-directory.json").is_dir());
        assert!(root.join("publications/generations").is_dir());
        assert_eq!(
            retire_provisional_state(&root).unwrap(),
            ProvisionalRetirementReport::default()
        );

        let store = KnowledgeSourceStore::open(&root, StoreLimits::default()).unwrap();
        assert_eq!(
            store
                .publication_status(&publication_authority().producer_id, &generation)
                .unwrap()
                .state,
            SourceGenerationStateV1::Ready
        );
    }

    #[test]
    fn a_symlinked_provisional_member_is_unlinked_not_followed() {
        let (temporary, root, store) = test_store(StoreLimits::default());
        drop(store);
        let outside = outside_target(&temporary);
        std::os::unix::fs::symlink(&outside, root.join("provisional")).unwrap();
        let report = retire_provisional_state(&root).unwrap();
        assert!(report.removed_directory);
        assert!(fs::symlink_metadata(root.join("provisional")).is_err());
        assert!(outside.join("keep.json").exists());
    }

    /// Before cleanup runs (or when it failed), legacy provisional state is
    /// present but never read: open, recovery, readiness, and maintenance
    /// all succeed over malformed legacy records, and maintenance reclaims a
    /// blob only a legacy provisional manifest referenced while keeping
    /// every publication-referenced blob.
    #[test]
    fn legacy_provisional_state_is_never_read_and_its_blobs_are_reclaimed() {
        let limits = StoreLimits {
            unreferenced_blob_grace_secs: 1,
            ..StoreLimits::default()
        };
        let (temporary, root, store) = test_store(limits);
        let generation = finalized_publication(&store);
        drop(store);
        let outside = outside_target(&temporary);
        let orphan = write_legacy_provisional_state(&root, &outside);

        let store = KnowledgeSourceStore::open(&root, limits).unwrap();
        store.recover().unwrap();
        let readiness = store.project_cutover_readiness("project-a").unwrap();
        assert_eq!(readiness.prepared_upload_count, 0);
        assert_eq!(readiness.unfinished_finalize_journal_count, 0);

        let report = store.maintain_at(&BTreeSet::new(), u64::MAX).unwrap();
        assert_eq!(report.deleted_blobs, 1);
        assert!(store.read_blob(&orphan, 64).unwrap().is_none());
        assert!(
            store
                .read_blob(
                    &source_file_blob_sha256(KNOWLEDGE_BYTES),
                    KNOWLEDGE_BYTES.len()
                )
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .read_blob(&source_file_blob_sha256(GAP_BYTES), GAP_BYTES.len())
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store
                .publication_status(&publication_authority().producer_id, &generation)
                .unwrap()
                .state,
            SourceGenerationStateV1::Ready
        );
        // The legacy tree itself is untouched by every scan.
        assert!(root.join("provisional/generation-index").is_dir());
    }
}
