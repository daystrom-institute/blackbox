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
    SourceManifestDescriptorV1, SourceManifestPageV1, provisional_generation_id_matches,
    provisional_workspace_generation_id, publication_candidate_generation_id,
    publication_generation_id_matches, validate_ancestry_page, validate_manifest_page,
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

/// Admit a stored upload record and normalize its page cursors.
///
/// State written before the graphs lane existed carries neither a graph
/// descriptor nor a graph page cursor, and its generation identity was minted
/// without the graph descriptor. Both are legacy vintage, not corruption: the
/// absent lane cursors are backfilled empty in memory, and the pre-graphs
/// identity stays admissible for exactly the shape that could have produced
/// it. Everything else remains a refusal.
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
    backfill_absent_page_cursors(&mut record.next_pages, publication_page_cursors());
    Ok(())
}

fn validate_provisional_upload(record: &mut ProvisionalUploadV1) -> Result<()> {
    if record.version != STORE_VERSION {
        bail!(StoreRequestError::InvalidState);
    }
    validate_upload_id(&record.upload_id)?;
    validate_project_id(&record.project_id)?;
    record
        .descriptor
        .validate_header(KnowledgeSourceLimits::default())?;
    validate_provisional_generation_id(&record.source_generation_id)?;
    if !provisional_generation_id_matches(&record.descriptor, &record.source_generation_id)? {
        bail!(StoreRequestError::InvalidState);
    }
    backfill_absent_page_cursors(&mut record.next_pages, provisional_page_cursors());
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

fn publication_page_cursors() -> BTreeMap<String, u64> {
    [
        (lane_name(SourceLaneV1::Knowledge).to_string(), 0),
        (lane_name(SourceLaneV1::Gaps).to_string(), 0),
        (lane_name(SourceLaneV1::Graphs).to_string(), 0),
    ]
    .into_iter()
    .collect()
}

fn provisional_page_cursors() -> BTreeMap<String, u64> {
    let mut cursors = BTreeMap::new();
    for class in [SnapshotClassV1::Baseline, SnapshotClassV1::Working] {
        for lane in [
            SourceLaneV1::Knowledge,
            SourceLaneV1::Gaps,
            SourceLaneV1::Graphs,
        ] {
            cursors.insert(provisional_slot_key(class, lane), 0);
        }
    }
    cursors
}

fn lane_name(lane: SourceLaneV1) -> &'static str {
    match lane {
        SourceLaneV1::Knowledge => "knowledge",
        SourceLaneV1::Gaps => "gaps",
        SourceLaneV1::Graphs => "graphs",
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
        SourceLaneV1::Graphs => &descriptor.graphs,
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
        (SnapshotClassV1::Baseline, SourceLaneV1::Graphs) => &descriptor.baseline_graphs,
        (SnapshotClassV1::Working, SourceLaneV1::Knowledge) => &descriptor.working_knowledge,
        (SnapshotClassV1::Working, SourceLaneV1::Gaps) => &descriptor.working_gaps,
        (SnapshotClassV1::Working, SourceLaneV1::Graphs) => &descriptor.working_graphs,
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

/// Read a graph manifest that state written before the graphs lane existed
/// never wrote. An absent file is legacy only when the descriptor also carries
/// an absent graph lane; a descriptor that claims graph content with no
/// manifest on disk is malformed and stays a refusal.
fn read_graph_manifest(
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

fn load_publication_manifests(
    path: &Path,
    descriptor: &PublicationCandidateDescriptorV1,
) -> Result<(
    Vec<SourceFileManifestEntryV1>,
    Vec<SourceFileManifestEntryV1>,
    Vec<SourceFileManifestEntryV1>,
)> {
    Ok((
        read_required_json(path, "manifest-knowledge.json", "knowledge manifest")?,
        read_required_json(path, "manifest-gaps.json", "gap manifest")?,
        read_graph_manifest(
            path,
            "manifest-graphs.json",
            "graph manifest",
            &descriptor.graphs,
        )?,
    ))
}

fn load_provisional_manifests(
    path: &Path,
    descriptor: &ProvisionalWorkspaceDescriptorV1,
) -> Result<(Vec<AncestryCommitV1>, [Vec<SourceFileManifestEntryV1>; 6])> {
    Ok((
        read_required_json(path, "ancestry.json", "ancestry witness")?,
        [
            read_required_json(
                path,
                "manifest-baseline-knowledge.json",
                "baseline knowledge manifest",
            )?,
            read_required_json(path, "manifest-baseline-gaps.json", "baseline gap manifest")?,
            read_graph_manifest(
                path,
                "manifest-baseline-graphs.json",
                "baseline graph manifest",
                &descriptor.baseline_graphs,
            )?,
            read_required_json(
                path,
                "manifest-working-knowledge.json",
                "working knowledge manifest",
            )?,
            read_required_json(path, "manifest-working-gaps.json", "working gap manifest")?,
            read_graph_manifest(
                path,
                "manifest-working-graphs.json",
                "working graph manifest",
                &descriptor.working_graphs,
            )?,
        ],
    ))
}

fn load_expected_blobs(path: &Path) -> Result<BTreeMap<String, u64>> {
    let mut expected = BTreeMap::new();
    for name in [
        "manifest-knowledge.json",
        "manifest-gaps.json",
        "manifest-graphs.json",
        "manifest-baseline-knowledge.json",
        "manifest-baseline-gaps.json",
        "manifest-baseline-graphs.json",
        "manifest-working-knowledge.json",
        "manifest-working-gaps.json",
        "manifest-working-graphs.json",
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
        graph_manifest_sha256: source.descriptor.graphs.manifest_sha256.clone(),
        knowledge_files: source.descriptor.knowledge.file_count,
        gap_files: source.descriptor.gaps.file_count,
        graph_files: source.descriptor.graphs.file_count,
        logical_bytes: source
            .descriptor
            .knowledge
            .logical_bytes
            .checked_add(source.descriptor.gaps.logical_bytes)
            .and_then(|bytes| bytes.checked_add(source.descriptor.graphs.logical_bytes))
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
        baseline_graph_manifest_sha256: source.descriptor.baseline_graphs.manifest_sha256.clone(),
        working_knowledge_manifest_sha256: source
            .descriptor
            .working_knowledge
            .manifest_sha256
            .clone(),
        working_gap_manifest_sha256: source.descriptor.working_gaps.manifest_sha256.clone(),
        working_graph_manifest_sha256: source.descriptor.working_graphs.manifest_sha256.clone(),
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

fn read_child_directories_if_present(path: &Path, allowed_files: &[&str]) -> Result<Vec<PathBuf>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            read_child_directories(path, allowed_files)
        }
        Ok(_) => bail!(StoreRequestError::InvalidState),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
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
            "manifest-baseline-graphs.json",
            "manifest-working-knowledge.json",
            "manifest-working-gaps.json",
            "manifest-working-graphs.json",
        ] {
            remove_regular_file(&path.join(name))?;
        }
    } else {
        remove_page_tree(&path.join("pages"), true)?;
        for name in [
            "upload.json",
            "manifest-knowledge.json",
            "manifest-gaps.json",
            "manifest-graphs.json",
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
            "manifest-baseline-graphs.json",
            "manifest-working-knowledge.json",
            "manifest-working-gaps.json",
            "manifest-working-graphs.json",
            "source.json",
        ]
    } else {
        &[
            "descriptor.json",
            "manifest-knowledge.json",
            "manifest-gaps.json",
            "manifest-graphs.json",
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
    graphs: &[SourceFileManifestEntryV1],
) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-knowledge-publication-source-evidence-v1\0");
    hasher.update(serde_json::to_vec(&(source, knowledge, gaps, graphs))?);
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

    fn has_same_identity(&self, other: &Self) -> bool {
        self.version == other.version
            && self.kind == other.kind
            && self.upload_id == other.upload_id
            && self.source_generation_id == other.source_generation_id
            && self.authority_key == other.authority_key
            && self.project_id == other.project_id
            && self.created_unix_secs == other.created_unix_secs
            && self.created_unix_nanos == other.created_unix_nanos
            && self.lease_expires_unix_secs == other.lease_expires_unix_secs
            && self.prior_generation_id == other.prior_generation_id
            && self.provisional_sequence == other.provisional_sequence
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
}

/// Fully detached bytes for the exact live provisional pointer selected under
/// the store mutation lock. Once returned, retirement or maintenance cannot
/// change the caller's point-in-time view.
#[derive(Debug, Clone)]
pub struct ReadyProvisionalWorkspace {
    pub source_generation_id: String,
    pub project_id: String,
    pub descriptor: ProvisionalWorkspaceDescriptorV1,
    pub lease_expires_unix_secs: u64,
    pub ancestry: Vec<AncestryCommitV1>,
    pub baseline_knowledge: Vec<ReadyPublicationFile>,
    pub baseline_gaps: Vec<ReadyPublicationFile>,
    pub baseline_graphs: Vec<ReadyPublicationFile>,
    pub working_knowledge: Vec<ReadyPublicationFile>,
    pub working_gaps: Vec<ReadyPublicationFile>,
    pub working_graphs: Vec<ReadyPublicationFile>,
}

#[derive(Debug, Clone)]
pub struct ProvisionalProbe {
    pub current: Option<ProvisionalWorkspaceStatusV1>,
    pub next_sequence: u64,
}

/// Lock-consistent source state used by the offline strict-cutover preflight.
///
/// The store reports source facts only. Overlay computation and authority
/// decisions remain with the indexing/runtime layer.
#[derive(Debug, Clone)]
pub struct KnowledgeSourceProjectCutoverReadiness {
    pub prepared_upload_count: u64,
    pub unfinished_finalize_journal_count: u64,
    pub expired_workspace_ids: Vec<WorkspaceId>,
    pub selected_workspaces: Vec<ReadyProvisionalWorkspace>,
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
        ] {
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
        let (knowledge_manifest, gap_manifest, graph_manifest) =
            load_publication_manifests(&generation_path, &source.descriptor)?;
        validate_publication_candidate(
            &source.descriptor,
            &knowledge_manifest,
            &gap_manifest,
            &graph_manifest,
            self.current_limits()?.contract,
        )?;
        let knowledge = self.materialize_ready_publication_files(&knowledge_manifest)?;
        let gaps = self.materialize_ready_publication_files(&gap_manifest)?;
        let graphs = self.materialize_ready_publication_files(&graph_manifest)?;
        let source_generation_sha256 = publication_source_generation_sha256(
            &source,
            &knowledge_manifest,
            &gap_manifest,
            &graph_manifest,
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
            let Some(mut record) = read_json::<ProvisionalUploadV1>(
                &path,
                "upload.json",
                MAX_UPLOAD_RECORD_BYTES,
                "provisional upload",
            )?
            else {
                continue;
            };
            validate_provisional_upload(&mut record)?;
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
        if self
            .load_provisional_sequence(authority, descriptor.sequence)?
            .is_some()
            || self
                .load_provisional_pointer(authority)?
                .is_some_and(|pointer| {
                    pointer.sequence == descriptor.sequence
                        && pointer.source_generation_id == generation_id
                })
            || NofollowDirectory::open_existing(&self.provisional_generation_path(
                &authority.project_id,
                &authority.workspace_id,
                &generation_id,
            )?)?
            .is_some()
        {
            bail!(StoreRequestError::InvalidState);
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
            for lane in [
                SourceLaneV1::Knowledge,
                SourceLaneV1::Gaps,
                SourceLaneV1::Graphs,
            ] {
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

    /// Return the immutable descriptor pinned when an authenticated workspace
    /// began an upload. The server uses this immediately before finalize to
    /// reject a capture whose accepted publication advanced during transfer.
    pub fn provisional_upload_descriptor(
        &self,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
    ) -> Result<ProvisionalWorkspaceDescriptorV1> {
        validate_provisional_authority(authority)?;
        let _guard = self.lock_mutation()?;
        let path = self.provisional_upload_path(&authority.workspace_id, upload_id)?;
        Ok(self
            .load_provisional_upload(&path, authority, upload_id)?
            .descriptor)
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

    /// Return the immutable descriptor of one authenticated provisional
    /// generation. Renewal callers use this to prove the generation still
    /// targets the daemon's current accepted publication before extending it.
    pub fn provisional_generation_descriptor(
        &self,
        authority: &ProvisionalAuthorityV1,
        generation_id: &str,
    ) -> Result<ProvisionalWorkspaceDescriptorV1> {
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
        Ok(self
            .load_provisional_generation(
                &authority.project_id,
                &authority.workspace_id,
                generation_id,
            )?
            .descriptor)
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

    /// Select and materialize one workspace's current live generation as one
    /// atomic, hash-verified snapshot. No descriptor-only selection escapes
    /// this boundary.
    pub fn materialize_selected_provisional(
        &self,
        authority: &ProvisionalAuthorityV1,
        now: u64,
    ) -> Result<Option<ReadyProvisionalWorkspace>> {
        validate_provisional_authority(authority)?;
        let _guard = self.lock_mutation()?;
        self.materialize_selected_provisional_locked(authority, now)
    }

    fn materialize_selected_provisional_locked(
        &self,
        authority: &ProvisionalAuthorityV1,
        now: u64,
    ) -> Result<Option<ReadyProvisionalWorkspace>> {
        let Some(pointer) = self.load_provisional_pointer(authority)? else {
            return Ok(None);
        };
        if pointer.lease_expires_unix_secs <= now {
            return Ok(None);
        }
        let source = self.load_provisional_generation(
            &authority.project_id,
            &authority.workspace_id,
            &pointer.source_generation_id,
        )?;
        if source.state != SourceGenerationStateV1::Ready
            || source.source_generation_id != pointer.source_generation_id
            || source.project_id != pointer.project_id
            || source.descriptor.workspace_id != pointer.workspace_id
            || source.descriptor.sequence != pointer.sequence
        {
            bail!(StoreRequestError::InvalidState);
        }
        let generation_path = self.provisional_generation_path(
            &authority.project_id,
            &authority.workspace_id,
            &source.source_generation_id,
        )?;
        let (ancestry, manifests) =
            load_provisional_manifests(&generation_path, &source.descriptor)?;
        validate_provisional_workspace(
            &source.descriptor,
            &ancestry,
            &manifests[0],
            &manifests[1],
            &manifests[2],
            &manifests[3],
            &manifests[4],
            &manifests[5],
            self.current_limits()?.contract,
        )?;
        Ok(Some(ReadyProvisionalWorkspace {
            source_generation_id: source.source_generation_id,
            project_id: source.project_id,
            descriptor: source.descriptor,
            lease_expires_unix_secs: pointer.lease_expires_unix_secs,
            ancestry,
            baseline_knowledge: self.materialize_ready_publication_files(&manifests[0])?,
            baseline_gaps: self.materialize_ready_publication_files(&manifests[1])?,
            baseline_graphs: self.materialize_ready_publication_files(&manifests[2])?,
            working_knowledge: self.materialize_ready_publication_files(&manifests[3])?,
            working_gaps: self.materialize_ready_publication_files(&manifests[4])?,
            working_graphs: self.materialize_ready_publication_files(&manifests[5])?,
        }))
    }

    /// Capture the source-store facts that must be quiet and replayable before
    /// one Published project can cross the strict knowledge-transport boundary.
    /// The mutation lock makes uploads, journals, pointers, and materialized
    /// generations one coherent observation.
    pub fn project_cutover_readiness(
        &self,
        project_id: &str,
        now: u64,
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
        for workspace in read_child_directories(&self.root.join("provisional/uploads"), &[])? {
            WorkspaceId::parse(file_name(&workspace)?)?;
            for upload_path in read_child_directories(&workspace, &[])? {
                let mut upload = read_json::<ProvisionalUploadV1>(
                    &upload_path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "provisional upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                validate_provisional_upload(&mut upload)?;
                if upload.project_id == project_id && is_open(upload.state) {
                    prepared_upload_count = prepared_upload_count.saturating_add(1);
                }
            }
        }

        let mut unfinished_finalize_journal_count = 0_u64;
        for path in read_regular_json_files(&self.root.join("journals"))? {
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

        let (expired_workspace_ids, selected_workspaces) =
            self.materialize_selected_provisionals_for_project_locked(project_id, now)?;

        Ok(KnowledgeSourceProjectCutoverReadiness {
            prepared_upload_count,
            unfinished_finalize_journal_count,
            expired_workspace_ids,
            selected_workspaces,
        })
    }

    /// Materialize every live atomic workspace pointer for one project. This
    /// is the restart-safe `all` source: visibility comes from the durable
    /// generation lease, not an in-memory session-token cache.
    pub fn materialize_selected_provisionals_for_project(
        &self,
        project_id: &str,
        now: u64,
    ) -> Result<Vec<ReadyProvisionalWorkspace>> {
        validate_project_id(project_id)?;
        let _guard = self.lock_mutation()?;
        self.materialize_selected_provisionals_for_project_locked(project_id, now)
            .map(|(_, selected)| selected)
    }

    /// Enumerate durable workspace pointer owners without materializing any
    /// peer. Callers then materialize each id independently so one corrupt or
    /// expired peer degrades only that peer in an `all` view.
    pub fn selected_provisional_workspace_ids_for_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<WorkspaceId>> {
        validate_project_id(project_id)?;
        let _guard = self.lock_mutation()?;
        let project_root = self.root.join("provisional/generations").join(project_id);
        let mut workspace_ids = Vec::new();
        for workspace_root in read_child_directories_if_present(&project_root, &["current.json"])? {
            let workspace_id = WorkspaceId::parse(file_name(&workspace_root)?)?;
            let directory = existing_directory(&workspace_root)?;
            if directory
                .read_regular(
                    "current.json",
                    MAX_GENERATION_RECORD_BYTES,
                    "provisional pointer",
                )?
                .is_some()
            {
                workspace_ids.push(workspace_id);
            }
        }
        Ok(workspace_ids)
    }

    fn materialize_selected_provisionals_for_project_locked(
        &self,
        project_id: &str,
        now: u64,
    ) -> Result<(Vec<WorkspaceId>, Vec<ReadyProvisionalWorkspace>)> {
        let mut expired_workspace_ids = Vec::new();
        let mut selected_workspaces = Vec::new();
        let project_root = self.root.join("provisional/generations").join(project_id);
        for workspace_root in read_child_directories_if_present(&project_root, &["current.json"])? {
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
                || pointer.sequence == 0
            {
                bail!(StoreRequestError::InvalidState);
            }
            validate_provisional_generation_id(&pointer.source_generation_id)?;
            if pointer.lease_expires_unix_secs <= now {
                expired_workspace_ids.push(workspace_id);
                continue;
            }
            let source = self.load_provisional_generation(
                project_id,
                &workspace_id,
                &pointer.source_generation_id,
            )?;
            let authority = ProvisionalAuthorityV1 {
                project_id: project_id.to_string(),
                scope: source.descriptor.scope.clone(),
                workspace_id,
            };
            let selected = self
                .materialize_selected_provisional_locked(&authority, now)?
                .ok_or(StoreRequestError::InvalidState)?;
            selected_workspaces.push(selected);
        }
        Ok((expired_workspace_ids, selected_workspaces))
    }

    pub fn probe_provisional(
        &self,
        authority: &ProvisionalAuthorityV1,
        now: u64,
    ) -> Result<ProvisionalProbe> {
        validate_provisional_authority(authority)?;
        let _guard = self.lock_mutation()?;
        let current = self
            .selected_provisional(authority, now)?
            .as_ref()
            .map(provisional_status)
            .transpose()?;
        let next_sequence = self.next_provisional_sequence_locked(authority)?;
        Ok(ProvisionalProbe {
            current,
            next_sequence,
        })
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

    /// Renew the exact live pointer at the configured maximum lease without a
    /// descriptor/select race. Expired pointers are never resurrected.
    pub fn renew_selected_provisional(
        &self,
        authority: &ProvisionalAuthorityV1,
    ) -> Result<Option<ProvisionalWorkspaceStatusV1>> {
        validate_provisional_authority(authority)?;
        let limits = self.current_limits()?;
        let now = now_unix_secs();
        let expires = now
            .checked_add(limits.max_provisional_lease_secs)
            .ok_or(StoreRequestError::LimitExceeded)?;
        let _guard = self.lock_mutation()?;
        let Some(pointer) = self.load_provisional_pointer(authority)? else {
            return Ok(None);
        };
        if pointer.lease_expires_unix_secs <= now {
            return Ok(None);
        }
        let source = self.load_provisional_generation(
            &authority.project_id,
            &authority.workspace_id,
            &pointer.source_generation_id,
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
        .map(Some)
    }

    pub fn provisional_renew_interval_secs(&self) -> Result<u64> {
        Ok(self
            .current_limits()?
            .max_provisional_lease_secs
            .saturating_div(2)
            .max(1))
    }

    /// Maximum lease the current server configuration will accept. Checkout
    /// owners use this daemon-authored value instead of guessing a TTL that a
    /// stricter deployment might reject.
    pub fn max_provisional_lease_secs(&self) -> Result<u64> {
        Ok(self.current_limits()?.max_provisional_lease_secs)
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
                    if self.repair_duplicate_provisional_finalize_journal(
                        &authority, &journal, &upload,
                    )? {
                        continue;
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
        let graphs = load_manifest_pages(
            path,
            lane_name(SourceLaneV1::Graphs),
            record.next_pages[lane_name(SourceLaneV1::Graphs)],
            record.descriptor.graphs.page_count,
        )?;
        validate_publication_candidate(
            &record.descriptor,
            &knowledge,
            &gaps,
            &graphs,
            self.current_limits()?.contract,
        )?;
        let directory = existing_directory(path)?;
        install_immutable_json(&directory, "manifest-knowledge.json", &knowledge)?;
        install_immutable_json(&directory, "manifest-gaps.json", &gaps)?;
        install_immutable_json(&directory, "manifest-graphs.json", &graphs)?;
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
        let baseline_graphs = load_manifest_pages(
            path,
            &provisional_slot_key(SnapshotClassV1::Baseline, SourceLaneV1::Graphs),
            record.next_pages
                [&provisional_slot_key(SnapshotClassV1::Baseline, SourceLaneV1::Graphs)],
            record.descriptor.baseline_graphs.page_count,
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
        let working_graphs = load_manifest_pages(
            path,
            &provisional_slot_key(SnapshotClassV1::Working, SourceLaneV1::Graphs),
            record.next_pages
                [&provisional_slot_key(SnapshotClassV1::Working, SourceLaneV1::Graphs)],
            record.descriptor.working_graphs.page_count,
        )?;
        validate_provisional_workspace(
            &record.descriptor,
            &ancestry,
            &baseline_knowledge,
            &baseline_gaps,
            &baseline_graphs,
            &working_knowledge,
            &working_gaps,
            &working_graphs,
            self.current_limits()?.contract,
        )?;
        let directory = existing_directory(path)?;
        install_immutable_json(&directory, "ancestry.json", &ancestry)?;
        for (name, manifest) in [
            ("manifest-baseline-knowledge.json", baseline_knowledge),
            ("manifest-baseline-gaps.json", baseline_gaps),
            ("manifest-baseline-graphs.json", baseline_graphs),
            ("manifest-working-knowledge.json", working_knowledge),
            ("manifest-working-gaps.json", working_gaps),
            ("manifest-working-graphs.json", working_graphs),
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
        let (ancestry, manifests) = load_provisional_manifests(&path, &upload.descriptor)?;
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
        install_immutable_record(&directory, "descriptor.json", &upload.descriptor)?;
        install_immutable_json(&directory, "ancestry.json", &ancestry)?;
        for (name, manifest) in [
            ("manifest-baseline-knowledge.json", &manifests[0]),
            ("manifest-baseline-gaps.json", &manifests[1]),
            ("manifest-baseline-graphs.json", &manifests[2]),
            ("manifest-working-knowledge.json", &manifests[3]),
            ("manifest-working-gaps.json", &manifests[4]),
            ("manifest-working-graphs.json", &manifests[5]),
        ] {
            install_immutable_json(&directory, name, manifest)?;
        }
        install_immutable_record(&directory, "source.json", &generation)?;
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
        if let Some(sequence) = self.load_provisional_sequence(authority, descriptor.sequence)?
            && sequence.source_generation_id != generation_id
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
        let manifests = load_publication_manifests(&generation_path, &source.descriptor)?;
        validate_publication_candidate(
            &source.descriptor,
            &manifests.0,
            &manifests.1,
            &manifests.2,
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
        let (ancestry, manifests) =
            load_provisional_manifests(&generation_path, &source.descriptor)?;
        validate_provisional_workspace(
            &source.descriptor,
            &ancestry,
            &manifests[0],
            &manifests[1],
            &manifests[2],
            &manifests[3],
            &manifests[4],
            &manifests[5],
            self.current_limits()?.contract,
        )?;
        self.verify_all_upload_blobs(&generation_path)
    }

    /// Repair the exact legacy failure where a second upload reused an
    /// already-finalized sequence and replaced its committed journal with a
    /// fresh Prepared journal. The immutable generation, sequence assignment,
    /// generation index, and original Ready upload must all agree before the
    /// committed journal is reconstructed.
    fn repair_duplicate_provisional_finalize_journal(
        &self,
        authority: &ProvisionalAuthorityV1,
        journal: &FinalizeJournalV1,
        duplicate: &ProvisionalUploadV1,
    ) -> Result<bool> {
        if journal.stage != FinalizeStageV1::Prepared
            || duplicate.state != SourceGenerationStateV1::MissingBlobs
        {
            return Ok(false);
        }
        let generation_path = self.provisional_generation_path(
            &authority.project_id,
            &authority.workspace_id,
            &journal.source_generation_id,
        )?;
        if NofollowDirectory::open_existing(&generation_path)?.is_none() {
            return Ok(false);
        }
        let source = self.load_provisional_generation(
            &authority.project_id,
            &authority.workspace_id,
            &journal.source_generation_id,
        )?;
        if source.descriptor != duplicate.descriptor
            || matches!(
                source.state,
                SourceGenerationStateV1::ReceivingManifest
                    | SourceGenerationStateV1::MissingBlobs
                    | SourceGenerationStateV1::Failed
            )
        {
            return Ok(false);
        }
        let sequence = self
            .load_provisional_sequence(authority, duplicate.descriptor.sequence)?
            .ok_or(StoreRequestError::InvalidState)?;
        if sequence.source_generation_id != journal.source_generation_id {
            bail!(StoreRequestError::InvalidState);
        }

        let mut originals = Vec::new();
        for path in read_child_directories(
            &self.provisional_upload_authority_root(&authority.workspace_id)?,
            &[],
        )? {
            let upload_id = file_name(&path)?;
            let candidate = self.load_provisional_upload(&path, authority, &upload_id)?;
            if candidate.upload_id != duplicate.upload_id
                && candidate.state == SourceGenerationStateV1::Ready
                && candidate.source_generation_id == journal.source_generation_id
                && candidate.descriptor == source.descriptor
            {
                originals.push(candidate);
            }
        }
        if originals.len() != 1 {
            bail!(StoreRequestError::InvalidState);
        }
        let original = originals.pop().expect("length checked");
        self.verify_finalized_provisional(authority, &original)?;

        let restored = FinalizeJournalV1 {
            version: STORE_VERSION,
            kind: FinalizeKindV1::Provisional,
            stage: FinalizeStageV1::Committed,
            upload_id: original.upload_id,
            source_generation_id: source.source_generation_id,
            authority_key: authority.workspace_id.to_string(),
            project_id: authority.project_id.clone(),
            created_unix_secs: source.created_unix_secs,
            created_unix_nanos: source.created_unix_nanos,
            lease_expires_unix_secs: Some(source.lease_expires_unix_secs),
            prior_generation_id: None,
            provisional_sequence: Some(source.descriptor.sequence),
            checksum_sha256: String::new(),
        }
        .seal()?;
        restored.validate()?;

        // This is the sole identity-changing journal write: it repairs an
        // on-disk journal produced before identity monotonicity was enforced.
        write_json(
            &existing_directory(&self.root.join("journals"))?,
            &journal_filename(restored.kind, &restored.source_generation_id),
            &restored,
        )?;
        remove_upload_directory(
            &self.provisional_upload_path(&authority.workspace_id, &duplicate.upload_id)?,
            true,
        )?;
        Ok(true)
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
        let name = journal_filename(journal.kind, &journal.source_generation_id);
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

    fn load_provisional_upload(
        &self,
        path: &Path,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
    ) -> Result<ProvisionalUploadV1> {
        let mut record = read_json::<ProvisionalUploadV1>(
            path,
            "upload.json",
            MAX_UPLOAD_RECORD_BYTES,
            "provisional upload",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        validate_provisional_upload(&mut record)?;
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

    fn load_provisional_sequence(
        &self,
        authority: &ProvisionalAuthorityV1,
        sequence: u64,
    ) -> Result<Option<ProvisionalPointerV1>> {
        if sequence == 0 {
            bail!(StoreRequestError::InvalidInput);
        }
        let root = self
            .provisional_workspace_root(&authority.project_id, &authority.workspace_id)?
            .join("sequences");
        let Some(pointer) = read_json::<ProvisionalPointerV1>(
            &root,
            &format!("{sequence:020}.json"),
            MAX_GENERATION_RECORD_BYTES,
            "provisional sequence",
        )?
        else {
            return Ok(None);
        };
        if pointer.version != STORE_VERSION
            || pointer.project_id != authority.project_id
            || pointer.workspace_id != authority.workspace_id
            || pointer.sequence != sequence
        {
            bail!(StoreRequestError::InvalidState);
        }
        validate_provisional_generation_id(&pointer.source_generation_id)?;
        Ok(Some(pointer))
    }

    fn next_provisional_sequence_locked(&self, authority: &ProvisionalAuthorityV1) -> Result<u64> {
        let mut highest = self
            .load_provisional_pointer(authority)?
            .map_or(0, |pointer| pointer.sequence);
        let sequences = self
            .provisional_workspace_root(&authority.project_id, &authority.workspace_id)?
            .join("sequences");
        match fs::symlink_metadata(&sequences) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                for path in read_regular_json_files(&sequences)? {
                    let name = file_name(&path)?;
                    let encoded = name
                        .strip_suffix(".json")
                        .ok_or(StoreRequestError::InvalidState)?;
                    if encoded.len() != 20 || !encoded.bytes().all(|byte| byte.is_ascii_digit()) {
                        bail!(StoreRequestError::InvalidState);
                    }
                    let sequence = encoded
                        .parse::<u64>()
                        .map_err(|_| anyhow!(StoreRequestError::InvalidState))?;
                    self.load_provisional_sequence(authority, sequence)?
                        .ok_or(StoreRequestError::InvalidState)?;
                    highest = highest.max(sequence);
                }
            }
            Ok(_) => bail!(StoreRequestError::InvalidState),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        highest
            .checked_add(1)
            .ok_or_else(|| anyhow!(StoreRequestError::LimitExceeded))
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
        for workspace in read_child_directories(&self.root.join("provisional/uploads"), &[])? {
            WorkspaceId::parse(file_name(&workspace)?)?;
            for upload_path in read_child_directories(&workspace, &[])? {
                let mut upload = read_json::<ProvisionalUploadV1>(
                    &upload_path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "provisional upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                validate_provisional_upload(&mut upload)?;
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
                    remove_upload_directory(&upload_path, false)?;
                    expired += 1;
                }
            }
        }
        for workspace in read_child_directories(&self.root.join("provisional/uploads"), &[])? {
            WorkspaceId::parse(file_name(&workspace)?)?;
            for upload_path in read_child_directories(&workspace, &[])? {
                let mut upload = read_json::<ProvisionalUploadV1>(
                    &upload_path,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "provisional upload",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                validate_provisional_upload(&mut upload)?;
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
        legacy_provisional_workspace_generation_id, legacy_publication_candidate_generation_id,
        legacy_working_pair_sha256, source_file_blob_sha256, source_manifest_sha256,
        working_pair_sha256,
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
            graphs: SourceManifestDescriptorV1::default(),
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
        let empty_graphs = SourceManifestDescriptorV1::default();
        let working_pair = working_pair_sha256(&working_knowledge, &working_gaps, &empty_graphs);
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
                baseline_graphs: empty_graphs.clone(),
                working_knowledge,
                working_gaps,
                working_graphs: empty_graphs,
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
        assert_eq!(
            store
                .provisional_upload_descriptor(&authority, &first.upload_id)
                .unwrap(),
            descriptor
        );
        let mut other_authority = authority.clone();
        other_authority.workspace_id =
            WorkspaceId::parse("fedcba9876543210fedcba9876543210").unwrap();
        assert!(
            store
                .provisional_upload_descriptor(&other_authority, &first.upload_id)
                .is_err()
        );
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
                .provisional_generation_descriptor(&authority, &first_generation)
                .unwrap(),
            descriptor
        );
        assert!(
            store
                .provisional_generation_descriptor(&other_authority, &first_generation)
                .is_err()
        );
        assert_eq!(
            store
                .selected_provisional(&authority, now_unix_secs())
                .unwrap()
                .unwrap()
                .source_generation_id,
            first_generation
        );
        let first_probe = store
            .probe_provisional(&authority, now_unix_secs())
            .unwrap();
        assert_eq!(
            first_probe.current.unwrap().source_generation_id,
            first_generation
        );
        assert_eq!(first_probe.next_sequence, 8);
        let materialized = store
            .materialize_selected_provisional(&authority, now_unix_secs())
            .unwrap()
            .unwrap();
        assert_eq!(materialized.source_generation_id, first_generation);
        assert_eq!(materialized.project_id, authority.project_id);
        assert_eq!(materialized.ancestry, nodes);
        assert_eq!(
            materialized.baseline_knowledge[0].source_bytes,
            KNOWLEDGE_BYTES
        );
        assert_eq!(
            materialized.working_knowledge[0].source_bytes,
            KNOWLEDGE_BYTES
        );
        assert_eq!(materialized.baseline_gaps[0].source_bytes, GAP_BYTES);
        assert_eq!(materialized.working_gaps[0].source_bytes, GAP_BYTES);
        let selected_renewal = store
            .renew_selected_provisional(&authority)
            .unwrap()
            .unwrap();
        assert!(
            selected_renewal.lease_expires_unix_secs.unwrap()
                >= materialized.lease_expires_unix_secs
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
        let retired_probe = store
            .probe_provisional(&authority, now_unix_secs())
            .unwrap();
        assert!(retired_probe.current.is_none());
        assert_eq!(retired_probe.next_sequence, 9);
        assert_store_error(
            store.begin_provisional_upload(&authority, provisional_fixture(8).0),
            StoreRequestError::InvalidState,
        );
    }

    #[test]
    fn finalize_journals_refuse_identity_changes_and_stage_regressions() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let authority = provisional_authority();
        let (descriptor, nodes, knowledge, gaps) = provisional_fixture(7);
        let upload = store
            .begin_provisional_upload(&authority, descriptor)
            .unwrap();
        put_provisional_pages(
            &store,
            &authority,
            &upload.upload_id,
            &nodes,
            &knowledge,
            &gaps,
        );
        store
            .missing_provisional_blobs(&authority, &upload.upload_id, None)
            .unwrap();
        install_fixture_blobs_provisional(&store, &authority, &upload.upload_id);
        let generation = store
            .finalize_provisional_upload(&authority, &upload.upload_id, 60)
            .unwrap()
            .source_generation_id;
        let journal = read_json::<FinalizeJournalV1>(
            &root.join("journals"),
            &journal_filename(FinalizeKindV1::Provisional, &generation),
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
    fn recovery_repairs_legacy_duplicate_provisional_finalize_journal() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let authority = provisional_authority();
        let (descriptor, nodes, knowledge, gaps) = provisional_fixture(7);
        let original = store
            .begin_provisional_upload(&authority, descriptor)
            .unwrap();
        put_provisional_pages(
            &store,
            &authority,
            &original.upload_id,
            &nodes,
            &knowledge,
            &gaps,
        );
        store
            .missing_provisional_blobs(&authority, &original.upload_id, None)
            .unwrap();
        install_fixture_blobs_provisional(&store, &authority, &original.upload_id);
        let generation = store
            .finalize_provisional_upload(&authority, &original.upload_id, 60)
            .unwrap()
            .source_generation_id;
        store.retire_provisional(&authority, &generation).unwrap();

        let original_path = store
            .provisional_upload_path(&authority.workspace_id, &original.upload_id)
            .unwrap();
        let mut duplicate = store
            .load_provisional_upload(&original_path, &authority, &original.upload_id)
            .unwrap();
        duplicate.upload_id = "f".repeat(32);
        duplicate.state = SourceGenerationStateV1::MissingBlobs;
        duplicate.updated_unix_secs = now_unix_secs();
        let duplicate_path = store
            .provisional_upload_path(&authority.workspace_id, &duplicate.upload_id)
            .unwrap();
        write_json(
            &NofollowDirectory::open_or_create(&duplicate_path).unwrap(),
            "upload.json",
            &duplicate,
        )
        .unwrap();

        let legacy = FinalizeJournalV1 {
            version: STORE_VERSION,
            kind: FinalizeKindV1::Provisional,
            stage: FinalizeStageV1::Prepared,
            upload_id: duplicate.upload_id.clone(),
            source_generation_id: generation.clone(),
            authority_key: authority.workspace_id.to_string(),
            project_id: authority.project_id.clone(),
            created_unix_secs: now_unix_secs(),
            created_unix_nanos: now_unix_nanos(),
            lease_expires_unix_secs: Some(now_unix_secs() + 60),
            prior_generation_id: None,
            provisional_sequence: Some(7),
            checksum_sha256: String::new(),
        }
        .seal()
        .unwrap();
        write_json(
            &existing_directory(&root.join("journals")).unwrap(),
            &journal_filename(FinalizeKindV1::Provisional, &generation),
            &legacy,
        )
        .unwrap();
        drop(store);

        let recovered = KnowledgeSourceStore::open(&root, StoreLimits::default()).unwrap();
        assert!(!duplicate_path.exists());
        let restored = read_json::<FinalizeJournalV1>(
            &root.join("journals"),
            &journal_filename(FinalizeKindV1::Provisional, &generation),
            MAX_JOURNAL_BYTES,
            "test journal",
        )
        .unwrap()
        .unwrap();
        assert_eq!(restored.stage, FinalizeStageV1::Committed);
        assert_eq!(restored.upload_id, original.upload_id);
        let probe = recovered
            .probe_provisional(&authority, now_unix_secs())
            .unwrap();
        assert!(probe.current.is_none());
        assert_eq!(probe.next_sequence, 8);
        assert_eq!(
            recovered
                .project_cutover_readiness(&authority.project_id, now_unix_secs())
                .unwrap()
                .unfinished_finalize_journal_count,
            0
        );
    }

    #[test]
    fn cutover_readiness_is_lock_consistent_and_refuses_unquiet_source_state() {
        let (_temporary, _root, store) = test_store(StoreLimits::default());
        let authority = provisional_authority();
        let (descriptor, nodes, knowledge, gaps) = provisional_fixture(7);
        let upload = store
            .begin_provisional_upload(&authority, descriptor)
            .unwrap();

        let prepared = store
            .project_cutover_readiness(&authority.project_id, now_unix_secs())
            .unwrap();
        assert_eq!(prepared.prepared_upload_count, 1);
        assert_eq!(prepared.unfinished_finalize_journal_count, 0);
        assert!(prepared.selected_workspaces.is_empty());

        put_provisional_pages(
            &store,
            &authority,
            &upload.upload_id,
            &nodes,
            &knowledge,
            &gaps,
        );
        store
            .missing_provisional_blobs(&authority, &upload.upload_id, None)
            .unwrap();
        install_fixture_blobs_provisional(&store, &authority, &upload.upload_id);
        let generation = store
            .finalize_provisional_upload(&authority, &upload.upload_id, 60)
            .unwrap()
            .source_generation_id;

        let ready = store
            .project_cutover_readiness(&authority.project_id, now_unix_secs())
            .unwrap();
        assert_eq!(ready.prepared_upload_count, 0);
        assert_eq!(ready.unfinished_finalize_journal_count, 0);
        assert!(ready.expired_workspace_ids.is_empty());
        assert_eq!(ready.selected_workspaces.len(), 1);
        assert_eq!(
            ready.selected_workspaces[0].source_generation_id,
            generation
        );
        assert_eq!(
            store
                .selected_provisional_workspace_ids_for_project(&authority.project_id)
                .unwrap(),
            vec![authority.workspace_id.clone()]
        );
        assert_eq!(
            ready.selected_workspaces[0].working_knowledge[0].source_bytes,
            KNOWLEDGE_BYTES
        );
        assert_eq!(
            ready.selected_workspaces[0].working_gaps[0].source_bytes,
            GAP_BYTES
        );

        store
            .write_finalize_journal(
                FinalizeJournalV1 {
                    version: STORE_VERSION,
                    kind: FinalizeKindV1::Provisional,
                    stage: FinalizeStageV1::Prepared,
                    upload_id: "f".repeat(32),
                    source_generation_id: format!("kws_{}", "e".repeat(64)),
                    authority_key: authority.workspace_id.as_str().to_string(),
                    project_id: authority.project_id.clone(),
                    created_unix_secs: 1,
                    created_unix_nanos: 1,
                    lease_expires_unix_secs: Some(2),
                    prior_generation_id: None,
                    provisional_sequence: Some(8),
                    checksum_sha256: String::new(),
                }
                .seal()
                .unwrap(),
            )
            .unwrap();
        let unquiet = store
            .project_cutover_readiness(&authority.project_id, u64::MAX)
            .unwrap();
        assert_eq!(unquiet.unfinished_finalize_journal_count, 1);
        assert_eq!(unquiet.expired_workspace_ids, vec![authority.workspace_id]);
        assert!(unquiet.selected_workspaces.is_empty());
    }

    #[test]
    fn cutover_readiness_fails_closed_on_selected_remote_blob_corruption() {
        let (_temporary, root, store) = test_store(StoreLimits::default());
        let authority = provisional_authority();
        let (descriptor, nodes, knowledge, gaps) = provisional_fixture(7);
        let upload = store
            .begin_provisional_upload(&authority, descriptor)
            .unwrap();
        put_provisional_pages(
            &store,
            &authority,
            &upload.upload_id,
            &nodes,
            &knowledge,
            &gaps,
        );
        store
            .missing_provisional_blobs(&authority, &upload.upload_id, None)
            .unwrap();
        install_fixture_blobs_provisional(&store, &authority, &upload.upload_id);
        store
            .finalize_provisional_upload(&authority, &upload.upload_id, 60)
            .unwrap();

        let hash = source_file_blob_sha256(KNOWLEDGE_BYTES);
        std::fs::write(
            root.join("blobs/sha256").join(&hash[..2]).join(&hash[2..]),
            b"corrupt",
        )
        .unwrap();

        assert_store_error(
            store.project_cutover_readiness(&authority.project_id, now_unix_secs()),
            StoreRequestError::InvalidState,
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
        let manifests = load_publication_manifests(&upload_path, &upload.descriptor).unwrap();
        install_immutable_json(&directory, "descriptor.json", &upload.descriptor).unwrap();
        install_immutable_json(&directory, "manifest-knowledge.json", &manifests.0).unwrap();
        install_immutable_json(&directory, "manifest-gaps.json", &manifests.1).unwrap();
        install_immutable_json(&directory, "manifest-graphs.json", &manifests.2).unwrap();
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
            graphs: SourceManifestDescriptorV1::default(),
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
                descriptor.graphs.is_absent_lane(),
                "only an empty graph lane has a pre-graphs vintage"
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
    struct LegacyProvisionalDescriptor {
        schema_version: u32,
        scope: PublishedScope,
        workspace_id: WorkspaceId,
        sequence: u64,
        accepted_generation: String,
        accepted_commit: String,
        checkout_head: String,
        merge_base: String,
        object_format: bbox_knowledge_source::GitObjectFormatV1,
        ancestry: AncestryDescriptorV1,
        capture: StableCaptureV1,
        baseline_knowledge: SourceManifestDescriptorV1,
        baseline_gaps: SourceManifestDescriptorV1,
        working_knowledge: SourceManifestDescriptorV1,
        working_gaps: SourceManifestDescriptorV1,
    }

    impl From<ProvisionalWorkspaceDescriptorV1> for LegacyProvisionalDescriptor {
        fn from(descriptor: ProvisionalWorkspaceDescriptorV1) -> Self {
            assert!(
                descriptor.baseline_graphs.is_absent_lane()
                    && descriptor.working_graphs.is_absent_lane(),
                "only an empty graph lane has a pre-graphs vintage"
            );
            Self {
                schema_version: descriptor.schema_version,
                scope: descriptor.scope,
                workspace_id: descriptor.workspace_id,
                sequence: descriptor.sequence,
                accepted_generation: descriptor.accepted_generation,
                accepted_commit: descriptor.accepted_commit,
                checkout_head: descriptor.checkout_head,
                merge_base: descriptor.merge_base,
                object_format: descriptor.object_format,
                ancestry: descriptor.ancestry,
                capture: descriptor.capture,
                baseline_knowledge: descriptor.baseline_knowledge,
                baseline_gaps: descriptor.baseline_gaps,
                working_knowledge: descriptor.working_knowledge,
                working_gaps: descriptor.working_gaps,
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
    struct LegacyProvisionalUpload {
        version: u32,
        upload_id: String,
        project_id: String,
        descriptor: LegacyProvisionalDescriptor,
        source_generation_id: String,
        state: SourceGenerationStateV1,
        next_ancestry_page: u64,
        ancestry_page_digests: BTreeMap<u64, String>,
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

    #[derive(Serialize, Deserialize)]
    struct LegacyStoredProvisional {
        version: u32,
        source_generation_id: String,
        project_id: String,
        descriptor: LegacyProvisionalDescriptor,
        state: SourceGenerationStateV1,
        created_unix_secs: u64,
        created_unix_nanos: u128,
        lease_expires_unix_secs: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<String>,
    }

    fn drop_graph_slots(slots: BTreeMap<String, u64>) -> BTreeMap<String, u64> {
        slots
            .into_iter()
            .filter(|(slot, _)| !mentions_graphs(slot))
            .collect()
    }

    fn drop_graph_digests(digests: BTreeMap<String, String>) -> BTreeMap<String, String> {
        digests
            .into_iter()
            .filter(|(slot, _)| !mentions_graphs(slot))
            .collect()
    }

    fn mentions_graphs(key: &str) -> bool {
        key.split(['/', '_']).any(|segment| segment == "graphs")
    }

    /// Rewrite a store the current binary produced into the on-disk shape the
    /// pre-graphs-lane binary produced: descriptors with no graph lanes, page
    /// cursors with no graph slots, no graph page directories, no graph
    /// manifests, working-pair commitments over knowledge and gaps only, and
    /// the pre-graphs generation identity wherever it appears.
    fn downgrade_store_to_pre_graphs(root: &Path) {
        let mut substitutions = BTreeMap::new();
        collect_pre_graphs_substitutions(root, &mut substitutions);
        assert!(
            !substitutions.is_empty(),
            "the fixture must hold at least one downgradable resource"
        );
        rewrite_records_to_pre_graphs(root);
        substitute_pre_graphs_strings(root, &substitutions);
        remove_graph_lane_members(root);
        reseal_journals(root);
        rename_to_pre_graphs_ids(root, &substitutions);
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
            } else if descriptor.get("baseline_knowledge").is_some() {
                let mut descriptor: ProvisionalWorkspaceDescriptorV1 =
                    serde_json::from_value(descriptor.clone()).unwrap();
                let legacy_pair = legacy_working_pair_sha256(
                    &descriptor.working_knowledge,
                    &descriptor.working_gaps,
                );
                substitutions.insert(
                    descriptor.capture.first_working_pair_sha256.clone(),
                    legacy_pair.clone(),
                );
                descriptor.capture.first_working_pair_sha256 = legacy_pair.clone();
                descriptor.capture.second_working_pair_sha256 = legacy_pair;
                substitutions.insert(
                    stored_id.to_string(),
                    legacy_provisional_workspace_generation_id(&descriptor),
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
                ("descriptor.json", false) => {
                    serde_json::to_vec_pretty(&LegacyProvisionalDescriptor::from(
                        serde_json::from_value::<ProvisionalWorkspaceDescriptorV1>(value).unwrap(),
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
                        next_pages: drop_graph_slots(record.next_pages),
                        page_digests: drop_graph_digests(record.page_digests),
                        updated_unix_secs: record.updated_unix_secs,
                    })
                }
                ("upload.json", false) => {
                    let record: ProvisionalUploadV1 = serde_json::from_value(value).unwrap();
                    serde_json::to_vec_pretty(&LegacyProvisionalUpload {
                        version: record.version,
                        upload_id: record.upload_id,
                        project_id: record.project_id,
                        descriptor: record.descriptor.into(),
                        source_generation_id: record.source_generation_id,
                        state: record.state,
                        next_ancestry_page: record.next_ancestry_page,
                        ancestry_page_digests: record.ancestry_page_digests,
                        next_pages: drop_graph_slots(record.next_pages),
                        page_digests: drop_graph_digests(record.page_digests),
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
                ("source.json", false) => {
                    let record: StoredProvisionalWorkspaceV1 =
                        serde_json::from_value(value).unwrap();
                    serde_json::to_vec_pretty(&LegacyStoredProvisional {
                        version: record.version,
                        source_generation_id: record.source_generation_id,
                        project_id: record.project_id,
                        descriptor: record.descriptor.into(),
                        state: record.state,
                        created_unix_secs: record.created_unix_secs,
                        created_unix_nanos: record.created_unix_nanos,
                        lease_expires_unix_secs: record.lease_expires_unix_secs,
                        diagnostic: record.diagnostic,
                    })
                }
                _ => unreachable!("every legacy record shape is handled"),
            };
            fs::write(&child, legacy.unwrap()).unwrap();
        }
    }

    fn substitute_pre_graphs_strings(path: &Path, substitutions: &BTreeMap<String, String>) {
        for child in json_children(path) {
            if child.is_dir() {
                substitute_pre_graphs_strings(&child, substitutions);
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

    fn remove_graph_lane_members(path: &Path) {
        for child in json_children(path) {
            let name = file_name(&child).unwrap();
            if child.is_dir() {
                if name == "graphs" {
                    fs::remove_dir_all(&child).unwrap();
                    continue;
                }
                remove_graph_lane_members(&child);
            } else if matches!(
                name.as_str(),
                "manifest-graphs.json"
                    | "manifest-baseline-graphs.json"
                    | "manifest-working-graphs.json"
            ) {
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

    fn rename_to_pre_graphs_ids(path: &Path, substitutions: &BTreeMap<String, String>) {
        for child in json_children(path) {
            if child.is_dir() {
                rename_to_pre_graphs_ids(&child, substitutions);
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

    /// A production-shaped store as the pre-graphs binary left it: one accepted
    /// publication generation with its committed finalize journal, one
    /// publication upload interrupted after its generation was installed, one
    /// accepted provisional workspace, and one provisional upload interrupted
    /// before its generation was installed.
    /// The fixture keeps several uploads open at once, which the default
    /// per-authority ceiling of two would refuse.
    fn pre_graphs_limits() -> StoreLimits {
        StoreLimits {
            max_open_uploads_per_authority: 4,
            ..StoreLimits::default()
        }
    }

    fn pre_graphs_state(root: &Path) -> PreGraphsState {
        let store = KnowledgeSourceStore::open(root, pre_graphs_limits()).unwrap();
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

        let workspace = provisional_authority();
        let (workspace_descriptor, nodes, workspace_knowledge, workspace_gaps) =
            provisional_fixture(7);
        let ready = store
            .begin_provisional_upload(&workspace, workspace_descriptor)
            .unwrap();
        put_provisional_pages(
            &store,
            &workspace,
            &ready.upload_id,
            &nodes,
            &workspace_knowledge,
            &workspace_gaps,
        );
        store
            .missing_provisional_blobs(&workspace, &ready.upload_id, None)
            .unwrap();
        install_fixture_blobs_provisional(&store, &workspace, &ready.upload_id);
        let ready_workspace_generation = store
            .finalize_provisional_upload(&workspace, &ready.upload_id, 3_600)
            .unwrap()
            .source_generation_id;

        let (next_descriptor, ..) = provisional_fixture(8);
        let pending = store
            .begin_provisional_upload(&workspace, next_descriptor)
            .unwrap();
        put_provisional_pages(
            &store,
            &workspace,
            &pending.upload_id,
            &nodes,
            &workspace_knowledge,
            &workspace_gaps,
        );
        store
            .missing_provisional_blobs(&workspace, &pending.upload_id, None)
            .unwrap();
        install_fixture_blobs_provisional(&store, &workspace, &pending.upload_id);
        write_prepared_provisional_journal(&store, &workspace, &pending.upload_id);

        drop(store);
        PreGraphsState {
            accepted_generation,
            interrupted_upload: interrupted.upload_id,
            resumable_upload: resumable.upload_id,
            ready_workspace_generation,
            pending_upload: pending.upload_id,
        }
    }

    struct PreGraphsState {
        accepted_generation: String,
        interrupted_upload: String,
        resumable_upload: String,
        ready_workspace_generation: String,
        pending_upload: String,
    }

    /// The identity a resource carries once the store is downgraded: the
    /// pre-graphs mint of the same descriptor.
    fn legacy_publication_generation(descriptor: &PublicationCandidateDescriptorV1) -> String {
        legacy_publication_candidate_generation_id(&publication_authority().producer_id, descriptor)
    }

    fn legacy_provisional_generation(sequence: u64) -> String {
        let (mut descriptor, ..) = provisional_fixture(sequence);
        let legacy_pair =
            legacy_working_pair_sha256(&descriptor.working_knowledge, &descriptor.working_gaps);
        descriptor.capture.first_working_pair_sha256 = legacy_pair.clone();
        descriptor.capture.second_working_pair_sha256 = legacy_pair;
        legacy_provisional_workspace_generation_id(&descriptor)
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
        let manifests = load_publication_manifests(&upload_path, &upload.descriptor).unwrap();
        install_immutable_json(&directory, "descriptor.json", &upload.descriptor).unwrap();
        install_immutable_json(&directory, "manifest-knowledge.json", &manifests.0).unwrap();
        install_immutable_json(&directory, "manifest-gaps.json", &manifests.1).unwrap();
        install_immutable_json(&directory, "manifest-graphs.json", &manifests.2).unwrap();
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

    fn write_prepared_provisional_journal(
        store: &KnowledgeSourceStore,
        authority: &ProvisionalAuthorityV1,
        upload_id: &str,
    ) {
        let upload_path = store
            .provisional_upload_path(&authority.workspace_id, upload_id)
            .unwrap();
        let upload = store
            .load_provisional_upload(&upload_path, authority, upload_id)
            .unwrap();
        store
            .write_finalize_journal(FinalizeJournalV1 {
                version: STORE_VERSION,
                kind: FinalizeKindV1::Provisional,
                stage: FinalizeStageV1::Prepared,
                upload_id: upload_id.to_string(),
                source_generation_id: upload.source_generation_id.clone(),
                authority_key: authority.workspace_id.to_string(),
                project_id: authority.project_id.clone(),
                created_unix_secs: 2,
                created_unix_nanos: 2,
                lease_expires_unix_secs: Some(u64::MAX / 2),
                prior_generation_id: None,
                provisional_sequence: Some(upload.descriptor.sequence),
                checksum_sha256: String::new(),
            })
            .unwrap();
    }

    #[test]
    fn pre_graphs_state_opens_and_recovers_every_resource() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("source-store");
        let state = pre_graphs_state(&root);
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
        assert_eq!(accepted.graph_manifest_sha256, empty_graph_lane_sha256());
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

        let workspace = provisional_authority();
        let workspace_generation = legacy_provisional_generation(7);
        assert_ne!(workspace_generation, state.ready_workspace_generation);
        let ready = store
            .provisional_status(&workspace, &workspace_generation)
            .unwrap();
        assert_eq!(ready.sequence, 7);
        assert_eq!(
            ready.working_graph_manifest_sha256,
            empty_graph_lane_sha256()
        );

        // The workspace upload holding a Prepared journal was finalized by
        // recovery and now owns the live pointer.
        let pending_generation = legacy_provisional_generation(8);
        assert_eq!(
            store
                .provisional_status(&workspace, &pending_generation)
                .unwrap()
                .sequence,
            8
        );
        assert_eq!(
            store
                .finalize_provisional_upload(&workspace, &state.pending_upload, 3_600)
                .unwrap()
                .source_generation_id,
            pending_generation
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

        let selected = store
            .materialize_selected_provisionals_for_project(&workspace.project_id, now_unix_secs())
            .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].source_generation_id, pending_generation);
        assert!(selected[0].working_graphs.is_empty());
        assert!(selected[0].baseline_graphs.is_empty());
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

    fn empty_graph_lane_sha256() -> String {
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
        pre_graphs_state(&root);
        downgrade_store_to_pre_graphs(&root);
        let store = KnowledgeSourceStore::open(&root, pre_graphs_limits()).unwrap();

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
}
