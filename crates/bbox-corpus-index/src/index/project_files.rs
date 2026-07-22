use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use ignore::{DirEntry, WalkBuilder};
use sha2::{Digest, Sha256};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::TermQuery;
use tantivy::schema::IndexRecordOption;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

use super::{FieldHandles, FileMeta, FileMetaSource, ReindexConfig};
use bbox_chunker::{self as chunker, Chunk, Edge, EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::{self, EntityRef};
use bbox_corpus_core::project_record::{ProjectRecord, load_project_records};

#[derive(Debug, Default)]
pub struct ProjectIndexStats {
    pub indexed_files: u64,
    pub indexed_docs: u64,
    pub skipped: u64,
    pub emitted_edges: u64,
    pub indexed_commits: u64,
    pub call_edges: u64,
    pub resolved_call_edges: u64,
    pub skipped_symlinks: u64,
    pub skipped_special: u64,
    pub skipped_unsupported: u64,
    pub skipped_oversize: u64,
    pub pending_local_snapshots: Vec<bbox_edge_sidecar::snapshot::PendingLocalSnapshotActivation>,
}

#[derive(Debug)]
pub struct CollectedIndexResult {
    pub snapshot_id: String,
    pub selector: String,
    pub document_count: u64,
    pub entity_inventory_sha256: String,
    pub current_chunk_targets: HashMap<String, EntityRef>,
    pub head_commit: String,
    pub dirty_fingerprint: String,
    pub worktree_dirty: bool,
}

pub fn collected_materialization_selector(project_id: &str, generation_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-collected-selector-materialization-v1");
    hasher.update(bbox_edge_sidecar::snapshot::current_materialization_version().as_bytes());
    format!(
        "{}:m{}",
        bbox_code_source::source_selector(project_id, generation_id),
        hex::encode(&hasher.finalize()[..8])
    )
}

pub fn local_activation_marker(project_id: &str) -> String {
    format!("code-source-activation:{project_id}")
}

pub fn recover_pending_local_snapshot_activations(
    searcher: &tantivy::Searcher,
    fields: FieldHandles,
    edges_dir: &Path,
) -> Result<()> {
    let Some(journal) =
        bbox_edge_sidecar::snapshot::load_pending_local_activation_journal(edges_dir)?
    else {
        return Ok(());
    };
    let mut committed = 0_usize;
    for activation in journal.activations() {
        let query = TermQuery::new(
            Term::from_field_text(
                fields.entity_id,
                &local_activation_marker(activation.project_id()),
            ),
            IndexRecordOption::Basic,
        );
        let count = searcher.search(&query, &Count)?;
        if count > 1 {
            anyhow::bail!("local activation marker is not unique");
        }
        let matches_commit = searcher
            .search(&query, &TopDocs::with_limit(1))?
            .into_iter()
            .next()
            .map(|(_score, address)| searcher.doc::<TantivyDocument>(address))
            .transpose()?
            .and_then(|document| {
                document
                    .get_first(fields.code_source_generation)
                    .and_then(|value| match value {
                        tantivy::schema::OwnedValue::Str(value) => Some(value.clone()),
                        _ => None,
                    })
            })
            .is_some_and(|token| token == journal.commit_token());
        if matches_commit {
            committed += 1;
        }
    }

    if committed == journal.activations().len() {
        bbox_edge_sidecar::snapshot::activate_pending_local_snapshots(
            edges_dir,
            journal.activations(),
        )?;
    } else if committed != 0 {
        anyhow::bail!("local activation commit markers are only partially visible");
    }
    bbox_edge_sidecar::snapshot::clear_pending_local_activation_journal(edges_dir)
}

#[derive(Debug, Default)]
struct ProjectFileScanStats {
    skipped_symlinks: u64,
    skipped_special: u64,
    skipped_unsupported: u64,
    skipped_oversize: u64,
}

#[derive(Debug, Clone)]
pub struct ActiveCollectedSource {
    pub selector: String,
    pub generation_id: String,
}

#[derive(Debug, Default)]
pub struct PreservedCollectedDocuments {
    pub project_ids: BTreeSet<String>,
    pub documents: Vec<TantivyDocument>,
}

pub fn active_collected_sources(
    config: &ReindexConfig,
) -> Result<BTreeMap<String, ActiveCollectedSource>> {
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&config.projects_path);
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
    Ok(manifest
        .workspaces
        .into_iter()
        .filter_map(|(project_id, entry)| {
            let selector = entry.code_source_selector?;
            let generation_id = entry.code_source_generation?;
            selector.starts_with("collected:").then_some((
                project_id,
                ActiveCollectedSource {
                    selector,
                    generation_id,
                },
            ))
        })
        .collect())
}

pub fn collect_preserved_collected_documents(
    index: &Index,
    config: &ReindexConfig,
    f: FieldHandles,
) -> Result<PreservedCollectedDocuments> {
    let active = active_collected_sources(config)?;
    if active.is_empty() {
        return Ok(PreservedCollectedDocuments::default());
    }
    let store = bbox_code_source_store::CodeSourceStore::open(
        &config.code_source_store_path,
        bbox_code_source_store::StoreLimits::default(),
    )?;
    let searcher = index.reader()?.searcher();
    let mut preserved = PreservedCollectedDocuments::default();
    for (project_id, source) in active {
        let Some(activation) = store.load_activation(&project_id)? else {
            let diagnostic = "active collected source has no activation record";
            store.record_health_failure(&project_id, "preservation_failed", diagnostic)?;
            anyhow::bail!(diagnostic);
        };
        if activation.selector != source.selector
            || activation.generation_id != source.generation_id
        {
            let diagnostic = "active collected source disagrees with its activation record";
            store.record_health_failure(&project_id, "preservation_failed", diagnostic)?;
            anyhow::bail!(diagnostic);
        }
        let generation = store.find_generation(&source.generation_id)?;
        if generation.materialized_doc_count != Some(activation.document_count)
            || generation.entity_inventory_sha256.as_deref()
                != Some(activation.entity_inventory_sha256.as_str())
        {
            let diagnostic = "active collected materialization metadata is incomplete";
            store.record_health_failure(&project_id, "preservation_failed", diagnostic)?;
            anyhow::bail!(diagnostic);
        }
        let query = TermQuery::new(
            Term::from_field_text(f.code_source_selector, &source.selector),
            IndexRecordOption::Basic,
        );
        let count = searcher.search(&query, &Count)?;
        if count as u64 != activation.document_count {
            store.record_health_failure(
                &project_id,
                "preservation_failed",
                &format!(
                    "active collected document count mismatch: expected {}, observed {}",
                    activation.document_count, count
                ),
            )?;
            anyhow::bail!(
                "active collected document count mismatch: expected {}, observed {}",
                activation.document_count,
                count
            );
        }
        let mut entity_ids = Vec::with_capacity(count);
        let mut documents = Vec::with_capacity(count);
        for (_score, address) in searcher.search(&query, &TopDocs::with_limit(count))? {
            let document = searcher.doc::<TantivyDocument>(address)?;
            let entity_id = document
                .get_first(f.entity_id)
                .and_then(|value| match value {
                    tantivy::schema::OwnedValue::Str(value) => Some(value.clone()),
                    _ => None,
                })
                .ok_or_else(|| anyhow::anyhow!("preserved collected document has no entity id"))?;
            entity_ids.push(entity_id);
            documents.push(document);
        }
        entity_ids.sort();
        let mut inventory = Sha256::new();
        for entity_id in entity_ids {
            inventory.update((entity_id.len() as u64).to_be_bytes());
            inventory.update(entity_id.as_bytes());
        }
        let observed = hex::encode(inventory.finalize());
        if observed != activation.entity_inventory_sha256 {
            store.record_health_failure(
                &project_id,
                "preservation_failed",
                &format!(
                    "active collected entity inventory mismatch: expected {}, observed {}",
                    activation.entity_inventory_sha256, observed
                ),
            )?;
            anyhow::bail!(
                "active collected entity inventory mismatch: expected {}, observed {}",
                activation.entity_inventory_sha256,
                observed
            );
        }
        store.clear_health_failure(&project_id, "preservation_failed")?;
        preserved.project_ids.insert(project_id);
        preserved.documents.extend(documents);
    }
    Ok(preserved)
}

struct PendingProjectFile {
    path_str: String,
    absolute_path: PathBuf,
    mtime: u64,
    size: u64,
    chunks: Vec<Chunk>,
}

struct ProjectIndexContext<'a> {
    f: FieldHandles,
    writer: &'a mut IndexWriter,
    meta: &'a mut HashMap<String, FileMeta>,
    stats: &'a mut ProjectIndexStats,
    edges_dir: &'a Path,
    git_meta_dir: &'a Path,
    force_git_full: bool,
}

fn project_refs_v2_enabled() -> bool {
    std::env::var("BBOX_PROJECT_REFS_V2")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn ref_snapshot_id(
    project: &ProjectRecord,
    root: &Path,
    files: &[(String, u64, u64)],
    commit_sha: Option<&str>,
) -> Option<String> {
    if !project_refs_v2_enabled() {
        return None;
    }
    if let (Some(repo_id), Some(head_sha)) = (project.repo_id.as_deref(), commit_sha) {
        return Some(bbox_edge_sidecar::snapshot::clean_snapshot_id(
            repo_id,
            &project.project_id,
            head_sha,
        ));
    }

    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    for (path, mtime, size) in files {
        hasher.update(path.as_bytes());
        hasher.update(mtime.to_le_bytes());
        hasher.update(size.to_le_bytes());
    }
    let fingerprint = hex::encode(hasher.finalize());
    Some(bbox_edge_sidecar::snapshot::nongit_snapshot_id(
        &project.project_id,
        &fingerprint,
    ))
}

pub fn scan_registered_project_files(config: &ReindexConfig) -> Result<Vec<(String, u64, u64)>> {
    let mut files = Vec::new();
    let collected = active_collected_sources(config)?;
    for project in load_project_records(&config.projects_path)? {
        let root = PathBuf::from(&project.canonical_path);
        if !collected.contains_key(&project.project_id) {
            let _ = scan_project_files(&root, &mut files)?;
        }
        if project.repo_id.is_some() {
            if let Some(head) = bbox_corpus_core::git::head_fingerprint(&root) {
                files.push((
                    super::git_history::git_source_key(&project.project_id),
                    0,
                    head,
                ));
            }
        }
    }
    Ok(files)
}

pub fn index_registered_projects_standalone(
    config: &ReindexConfig,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    force_git_full: bool,
    preserved_collected: &BTreeSet<String>,
) -> Result<ProjectIndexStats> {
    let mut stats = ProjectIndexStats::default();
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&config.projects_path);
    let git_meta_dir = super::git_history::git_meta_dir_from_projects_path(&config.projects_path);
    let collected = active_collected_sources(config)?;
    let collected_store = (!collected.is_empty()).then(|| {
        bbox_code_source_store::CodeSourceStore::open(
            &config.code_source_store_path,
            bbox_code_source_store::StoreLimits::default(),
        )
    });
    for project in load_project_records(&config.projects_path)? {
        let root = PathBuf::from(&project.canonical_path);
        if let Some(active) = collected.get(&project.project_id) {
            let store = collected_store
                .as_ref()
                .expect("collected store exists when collected sources exist")
                .as_ref()
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            index_active_collected_project(
                &project,
                &root,
                active,
                store,
                f,
                writer,
                meta,
                &edges_dir,
                &git_meta_dir,
                force_git_full,
                preserved_collected.contains(&project.project_id),
                &mut stats,
            )?;
            continue;
        }
        if !root.exists() {
            continue;
        }
        let mut ctx = ProjectIndexContext {
            f,
            writer,
            meta,
            stats: &mut stats,
            edges_dir: &edges_dir,
            git_meta_dir: &git_meta_dir,
            force_git_full,
        };
        index_project(&project, &root, &mut ctx)?;
    }
    Ok(stats)
}

#[allow(clippy::too_many_arguments)]
fn index_active_collected_project(
    project: &ProjectRecord,
    root: &Path,
    active: &ActiveCollectedSource,
    store: &bbox_code_source_store::CodeSourceStore,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    edges_dir: &Path,
    git_meta_dir: &Path,
    force_full: bool,
    preserved_documents_are_staged: bool,
    stats: &mut ProjectIndexStats,
) -> Result<()> {
    let activation = store
        .load_activation(&project.project_id)?
        .ok_or_else(|| anyhow::anyhow!("active collected selector has no activation record"))?;
    if activation.generation_id != active.generation_id || activation.selector != active.selector {
        anyhow::bail!("active collected selector disagrees with its activation record");
    }
    let expected_selector =
        collected_materialization_selector(&project.project_id, &active.generation_id);
    if active.selector != expected_selector {
        anyhow::bail!("active collected selector requires materialization migration");
    }
    let expected_snapshot = bbox_edge_sidecar::snapshot::collected_snapshot_id(
        &project.project_id,
        &active.generation_id,
    );
    if activation.snapshot_id != expected_snapshot {
        anyhow::bail!("active collected materialization version requires an explicit migration");
    }
    let stored = store.find_generation(&active.generation_id)?;
    let entries = store.load_generation_entries(&stored.descriptor.scope, &active.generation_id)?;
    let blobs_available = !force_full
        || entries.iter().all(|entry| {
            store
                .verified_blob_file(&entry.content_sha256, entry.size)
                .is_ok()
        });
    if force_full && !blobs_available {
        store.mark_generation_state(
            &stored.descriptor.scope,
            &active.generation_id,
            bbox_code_source::GenerationState::MissingBlobData,
            Some("one or more active source blobs are missing or corrupt".to_string()),
        )?;
        store.record_health_failure(
            &project.project_id,
            "missing_blob_data",
            "one or more active source blobs are missing or corrupt",
        )?;
    } else if force_full && stored.state == bbox_code_source::GenerationState::MissingBlobData {
        store.mark_generation_state(
            &stored.descriptor.scope,
            &active.generation_id,
            bbox_code_source::GenerationState::Active,
            None,
        )?;
        store.clear_health_failure(&project.project_id, "missing_blob_data")?;
    }
    let staged = if force_full && blobs_available {
        Some(stage_collected_project_generation(
            project,
            &stored.descriptor,
            &active.generation_id,
            &entries,
            f,
            writer,
            edges_dir,
            |entry| {
                let mut file = store.verified_blob_file(&entry.content_sha256, entry.size)?;
                let mut bytes = Vec::with_capacity(entry.size as usize);
                file.read_to_end(&mut bytes)?;
                Ok(bytes)
            },
        )?)
    } else if force_full && !preserved_documents_are_staged {
        anyhow::bail!(
            "active collected generation has unavailable blobs and no verified read-back"
        );
    } else {
        if force_full && !blobs_available {
            tracing::warn!(
                project_id = %project.project_id,
                generation = %active.generation_id,
                "full rebuild preserved active collected documents because source blobs are unavailable"
            );
        }
        None
    };
    let current_chunk_targets = staged
        .as_ref()
        .map(|result| result.current_chunk_targets.clone())
        .unwrap_or_else(|| {
            activation
                .current_chunk_targets
                .clone()
                .into_iter()
                .collect()
        });
    let mut git_ctx = super::git_history::GitIndexContext {
        f,
        writer,
        meta,
        edges_dir,
        git_meta_dir,
        force_full,
    };
    let git_stats = super::git_history::index_git_history_for_project(
        project,
        root,
        &current_chunk_targets,
        &mut git_ctx,
    )?;
    stats.indexed_commits += git_stats.indexed_commits;
    stats.indexed_docs += git_stats.indexed_commits;
    stats.emitted_edges += git_stats.emitted_edges;
    let git_edges = bbox_edge_sidecar::edge_sidecar::read_managed_derived_edges(
        edges_dir,
        "git",
        &project.project_id,
    )?
    .into_iter()
    .map(|edge| bbox_edge_sidecar::edge_sidecar::Edge {
        source: edge.source,
        kind: edge.kind,
        target: edge.target,
        provenance: edge.provenance,
        confidence: edge.confidence,
        metadata: Default::default(),
    })
    .collect::<Vec<_>>();
    if let Some(staged) = staged {
        bbox_edge_sidecar::snapshot::write_snapshot_files(
            edges_dir,
            &project.project_id,
            &staged.snapshot_id,
            &[("git-current.jsonl", git_edges.as_slice())],
        )?;
        stats.indexed_docs += staged.document_count;
        stats.indexed_files += stored.descriptor.file_count;
    } else {
        bbox_edge_sidecar::snapshot::write_snapshot_files(
            edges_dir,
            &project.project_id,
            &activation.snapshot_id,
            &[("git-current.jsonl", git_edges.as_slice())],
        )?;
    }
    Ok(())
}

pub fn build_project_file_doc(
    chunk: &Chunk,
    project: &ProjectRecord,
    absolute_path: &Path,
    commit_sha: Option<&str>,
    snapshot_id: Option<&str>,
    f: FieldHandles,
) -> TantivyDocument {
    let selector = bbox_code_source::local_selector(&project.project_id);
    let relative_path = chunk.file_path.to_string_lossy();
    let entry_key = bbox_code_source::source_entry_key(&selector, &relative_path);
    build_project_file_doc_for_source(
        chunk,
        project,
        absolute_path,
        commit_sha,
        snapshot_id,
        &selector,
        "local",
        &entry_key,
        f,
    )
}

pub fn stage_collected_project_generation<F>(
    project: &ProjectRecord,
    descriptor: &bbox_code_source::GenerationDescriptor,
    generation_id: &str,
    entries: &[bbox_code_source::ManifestEntry],
    f: FieldHandles,
    writer: &mut IndexWriter,
    edges_dir: &Path,
    open_bytes: F,
) -> Result<CollectedIndexResult>
where
    F: FnMut(&bbox_code_source::ManifestEntry) -> Result<Vec<u8>>,
{
    descriptor.validate_manifest(entries, u64::MAX, u64::MAX)?;
    let snapshot_id =
        bbox_edge_sidecar::snapshot::collected_snapshot_id(&project.project_id, generation_id);
    let selector = collected_materialization_selector(&project.project_id, generation_id);
    stage_project_file_generation(
        project,
        descriptor,
        generation_id,
        entries,
        &selector,
        &snapshot_id,
        false,
        f,
        writer,
        edges_dir,
        open_bytes,
    )
}

pub fn stage_local_project_generation(
    project: &ProjectRecord,
    scope: &bbox_corpus_core::identity::PublishedScope,
    f: FieldHandles,
    writer: &mut IndexWriter,
    edges_dir: &Path,
) -> Result<CollectedIndexResult> {
    let root = PathBuf::from(&project.canonical_path)
        .canonicalize()
        .with_context(|| format!("canonicalizing local project {}", project.project_id))?;
    if !root.is_dir() {
        anyhow::bail!("registered local project root is not a directory");
    }
    let head_commit = bbox_corpus_core::git::current_head(&root)
        .ok_or_else(|| anyhow::anyhow!("registered local project has no readable Git HEAD"))?;
    let mut scanned = Vec::new();
    let _scan_stats = scan_project_files(&root, &mut scanned)?;
    let mut entries = Vec::with_capacity(scanned.len());
    for (absolute_path, _mtime, declared_size) in scanned {
        let absolute_path = PathBuf::from(absolute_path);
        let relative_path = absolute_path
            .strip_prefix(&root)
            .context("scanned local source escaped its registered root")?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("local source path is not valid UTF-8"))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let bytes = read_regular_file_confined(&root, Path::new(&relative_path))
            .with_context(|| format!("reading local source {relative_path}"))?;
        if bytes.len() as u64 != declared_size {
            anyhow::bail!("local source changed while preparing cutback");
        }
        entries.push(bbox_code_source::ManifestEntry {
            relative_path,
            content_sha256: full_hash(&bytes),
            size: declared_size,
        });
    }
    entries.sort_by(|left, right| {
        left.relative_path
            .as_bytes()
            .cmp(right.relative_path.as_bytes())
    });
    let logical_bytes = entries.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.size)
            .ok_or_else(|| anyhow::anyhow!("local source byte count overflow"))
    })?;
    let dirty_fingerprint = bbox_code_source::dirty_fingerprint(&head_commit, &entries);
    let descriptor = bbox_code_source::GenerationDescriptor {
        schema_version: bbox_code_source::SCHEMA_VERSION,
        walker_policy_version: bbox_code_source::WALKER_POLICY_VERSION.to_string(),
        scope: scope.clone(),
        head_commit: head_commit.clone(),
        dirty_fingerprint: dirty_fingerprint.clone(),
        manifest_sha256: bbox_code_source::manifest_sha256(&entries),
        file_count: entries.len() as u64,
        logical_bytes,
    };
    descriptor.validate_manifest(&entries, u64::MAX, u64::MAX)?;
    let selector = bbox_code_source::local_selector(&project.project_id);
    let worktree_dirty = bbox_corpus_core::git::is_worktree_dirty(&root);
    let snapshot_id = if worktree_dirty {
        bbox_edge_sidecar::snapshot::nongit_snapshot_id(&project.project_id, &dirty_fingerprint)
    } else {
        bbox_edge_sidecar::snapshot::clean_snapshot_id(
            &scope.repo_id,
            &project.project_id,
            &head_commit,
        )
    };
    stage_project_file_generation(
        project,
        &descriptor,
        "local",
        &entries,
        &selector,
        &snapshot_id,
        worktree_dirty,
        f,
        writer,
        edges_dir,
        |entry| {
            read_regular_file_confined(&root, Path::new(&entry.relative_path))
                .with_context(|| format!("re-reading local source {}", entry.relative_path))
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn stage_project_file_generation<F>(
    project: &ProjectRecord,
    descriptor: &bbox_code_source::GenerationDescriptor,
    generation_id: &str,
    entries: &[bbox_code_source::ManifestEntry],
    selector: &str,
    snapshot_id: &str,
    worktree_dirty: bool,
    f: FieldHandles,
    writer: &mut IndexWriter,
    edges_dir: &Path,
    mut open_bytes: F,
) -> Result<CollectedIndexResult>
where
    F: FnMut(&bbox_code_source::ManifestEntry) -> Result<Vec<u8>>,
{
    const MAX_STAGED_SYMBOLS: usize = 2_000_000;
    const MAX_STAGED_CHUNK_TARGETS: usize = 2_000_000;
    const MAX_STAGED_ENTITY_ID_BYTES: usize = 256 * 1024 * 1024;

    let registry = chunker::default_registry();
    let mut chunk_entry = |entry: &bbox_code_source::ManifestEntry| {
        let relative_path = Path::new(&entry.relative_path);
        let display_path = Path::new(&project.canonical_path).join(relative_path);
        let bytes = open_bytes(entry)
            .with_context(|| format!("opening collected source {}", entry.relative_path))?;
        if bytes.len() as u64 != entry.size || full_hash(&bytes) != entry.content_sha256 {
            anyhow::bail!("collected source blob failed manifest verification");
        }
        if is_binary(relative_path, &bytes) {
            return Ok(None);
        }
        let sniff_len = bytes.len().min(4096);
        let Some(format) = registry
            .iter()
            .find(|chunker| chunker.claims(relative_path, &bytes[..sniff_len]))
        else {
            return Ok(None);
        };
        let (chunks, edges) = format.chunk(relative_path, &bytes).with_context(|| {
            format!(
                "chunking collected source {} as {}",
                entry.relative_path,
                format.format_id()
            )
        })?;
        let chunks = bound_chunks(&finalize_chunks(project, relative_path, chunks));
        Ok(Some((display_path, chunks, edges)))
    };

    // Pass one retains only symbol identities. Chunk bodies and file bytes are
    // released after each immutable blob, so generation size no longer maps
    // directly to peak staging memory.
    let mut symbol_table = HashMap::new();
    for entry in entries {
        let Some((_display_path, chunks, _edges)) = chunk_entry(entry)? else {
            continue;
        };
        extend_symbol_table(&mut symbol_table, &chunks, Some(snapshot_id));
        if symbol_table.len() > MAX_STAGED_SYMBOLS {
            anyhow::bail!("collected source symbol table exceeds the staging safety limit");
        }
    }

    writer.delete_term(Term::from_field_text(f.code_source_selector, selector));

    let mut stats = ProjectIndexStats::default();
    let mut entity_ids = Vec::new();
    let mut entity_id_bytes = 0_usize;
    let mut current_chunk_targets = HashMap::new();
    let mut edge_writer = bbox_edge_sidecar::snapshot::create_snapshot_edge_writer(
        edges_dir,
        &project.project_id,
        snapshot_id,
        "project.jsonl",
    )?;
    for entry in entries {
        let Some((display_path, chunks, parser_edges)) = chunk_entry(entry)? else {
            continue;
        };
        let mut project_edges = derive_edges(&chunks, parser_edges, Some(snapshot_id));
        project_edges.extend(derive_code_edges(
            &chunks,
            &symbol_table,
            &mut stats,
            Some(snapshot_id),
        ));
        current_chunk_targets.extend(git_targets_for_scope(
            &descriptor.scope.bbox_root_relpath,
            &chunks,
            Some(snapshot_id),
        ));
        if current_chunk_targets.len() > MAX_STAGED_CHUNK_TARGETS {
            anyhow::bail!("collected source chunk targets exceed the staging safety limit");
        }
        let sidecar_edges = project_edges
            .into_iter()
            .map(|edge| bbox_edge_sidecar::edge_sidecar::Edge {
                source: edge.source,
                kind: edge.kind,
                target: edge.target,
                provenance: edge.provenance,
                confidence: edge.confidence,
                metadata: Default::default(),
            })
            .collect::<Vec<_>>();
        edge_writer.append(&sidecar_edges)?;

        let entry_key = bbox_code_source::source_entry_key(&selector, &entry.relative_path);
        for chunk in chunks {
            let entity_id =
                super::embed_hook::project_file_entity_id_for_snapshot(&chunk, Some(snapshot_id));
            entity_id_bytes = entity_id_bytes.saturating_add(entity_id.len());
            if entity_id_bytes > MAX_STAGED_ENTITY_ID_BYTES {
                anyhow::bail!("collected source entity inventory exceeds the staging safety limit");
            }
            let doc = build_project_file_doc_for_source(
                &chunk,
                project,
                &display_path,
                Some(&descriptor.head_commit),
                Some(snapshot_id),
                &selector,
                generation_id,
                &entry_key,
                f,
            );
            super::embed_hook::emit_project_file(&chunk, &entity_id);
            writer.add_document(doc)?;
            entity_ids.push(entity_id);
        }
    }
    edge_writer.finish()?;

    entity_ids.sort();
    let mut inventory = Sha256::new();
    for entity_id in &entity_ids {
        inventory.update((entity_id.len() as u64).to_be_bytes());
        inventory.update(entity_id.as_bytes());
    }
    Ok(CollectedIndexResult {
        snapshot_id: snapshot_id.to_string(),
        selector: selector.to_string(),
        document_count: entity_ids.len() as u64,
        entity_inventory_sha256: hex::encode(inventory.finalize()),
        current_chunk_targets,
        head_commit: descriptor.head_commit.clone(),
        dirty_fingerprint: descriptor.dirty_fingerprint.clone(),
        worktree_dirty,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn build_project_file_doc_for_source(
    chunk: &Chunk,
    project: &ProjectRecord,
    display_path: &Path,
    commit_sha: Option<&str>,
    snapshot_id: Option<&str>,
    selector: &str,
    generation: &str,
    entry_key: &str,
    f: FieldHandles,
) -> TantivyDocument {
    let entity_id = super::embed_hook::project_file_entity_id_for_snapshot(chunk, snapshot_id);
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "project_file");
    doc.add_text(f.parser_version, entity_ref::PARSER_VERSION);
    doc.add_text(f.content, &chunk.content);
    if chunk.chunk_kind == "code_block" {
        doc.add_text(f.code_content, &chunk.content);
    }
    doc.add_text(f.session_id, "");
    doc.add_text(f.account, "project_file");
    doc.add_text(f.project, &project.canonical_path);
    doc.add_text(f.role, "file");
    let path_str = display_path.to_string_lossy();
    doc.add_text(f.file_path, &*path_str);
    doc.add_text(f.code_source_selector, selector);
    doc.add_text(f.code_source_generation, generation);
    doc.add_text(f.code_source_entry_key, entry_key);
    // Reuse the same string for the tokenized path field; the code tokenizer
    // splits on `/`, `_`, `.`, etc., so /home/x/src/embed/voyage.rs becomes
    // tokens [home, x, src, embed, voyage, rs] available to BM25 ranking.
    doc.add_text(f.path_tokens, &*path_str);
    if let Some(symbol) = &chunk.symbol {
        // Symbol path also tokenized for BM25 boost — `Witness.Authority` →
        // [Witness, Authority] so symbol-named queries surface correctly.
        doc.add_text(f.path_tokens, symbol.as_str());
    }
    doc.add_u64(f.byte_offset, chunk.byte_start);
    doc.add_u64(f.byte_end, chunk.byte_end);
    if let Some(line_start) = chunk.line_start {
        doc.add_u64(f.line_start, line_start as u64);
    }
    if let Some(line_end) = chunk.line_end {
        doc.add_u64(f.line_end, line_end as u64);
    }
    doc.add_u64(f.is_subagent, 0);
    doc.add_text(f.project_id, &project.project_id);
    doc.add_text(f.chunk_kind, &chunk.chunk_kind);
    doc.add_text(f.chunk_hash, &chunk.chunk_hash);
    doc.add_text(f.entity_id, &entity_id);
    if let Some(language) = &chunk.language {
        doc.add_text(f.language, language);
    }
    if let Some(symbol) = &chunk.symbol {
        doc.add_text(f.symbol, symbol);
    }
    if let Some(symbol_exact) = &chunk.symbol_exact {
        doc.add_text(f.symbol_exact, symbol_exact);
    }
    if let Some(symbol_kind) = &chunk.symbol_kind {
        doc.add_text(f.symbol_kind, symbol_kind);
    }
    if let Some(parent_kind) = &chunk.parent_kind {
        doc.add_text(f.parent_kind, parent_kind);
    }
    if let Some(repo_id) = &project.repo_id {
        doc.add_text(f.repo_id, repo_id);
    }
    if let Some(commit_sha) = commit_sha {
        doc.add_text(f.commit_sha, commit_sha);
    }
    doc
}

pub fn resolve_current_chunk_entity(
    project: &ProjectRecord,
    root: &Path,
    absolute_path: &Path,
    byte_range: Option<(u64, u64)>,
) -> Result<Option<EntityRef>> {
    let relative_path = absolute_path.strip_prefix(root).unwrap_or(absolute_path);
    let bytes = match read_regular_file_confined(root, relative_path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    if is_binary(absolute_path, &bytes) {
        return Ok(None);
    }
    let registry = chunker::default_registry();
    let sniff_len = bytes.len().min(4096);
    let Some(format) = registry
        .iter()
        .find(|chunker| chunker.claims(absolute_path, &bytes[..sniff_len]))
    else {
        return Ok(None);
    };
    let (chunks, _edges) = format.chunk(absolute_path, &bytes)?;
    let chunks = bound_chunks(&finalize_chunks(project, relative_path, chunks));
    let selected = byte_range
        .and_then(|(start, _end)| {
            chunks
                .iter()
                .find(|chunk| chunk.byte_start <= start && start <= chunk.byte_end)
        })
        .or_else(|| chunks.first());
    Ok(selected.map(|chunk| EntityRef::ProjectFile {
        project_id: chunk.project_id.clone(),
        rel_path_hash: chunk.rel_path_hash.clone(),
        chunk_hash: chunk.chunk_hash.clone(),
        occurrence_idx: chunk.occurrence_idx,
    }))
}

#[derive(Debug, PartialEq, Eq)]
enum ProjectFileAction {
    /// mtime+size+materialization version all match — leave as-is.
    Skip,
    /// New file, changed content, or a known-different materialization version
    /// (a real indexer/chunker/parser bump) — must re-chunk.
    Reindex,
}

/// Decide what to do with a scanned project file given its previously indexed
/// metadata. The version dimension forces a re-chunk after an
/// indexer/chunker/parser bump even when the file is byte-for-byte unchanged.
/// An unknown stored version is re-chunked because it cannot prove either the
/// current materialization algorithm or a V2 snapshot identity.
fn classify_project_file(
    prev: Option<&FileMeta>,
    mtime: u64,
    size: u64,
    mat_version: &str,
) -> ProjectFileAction {
    let Some(prev) = prev else {
        return ProjectFileAction::Reindex;
    };
    if prev.mtime != mtime || prev.size != size {
        return ProjectFileAction::Reindex;
    }
    match prev.mat_version.as_deref() {
        Some(v) if v == mat_version => ProjectFileAction::Skip,
        None => ProjectFileAction::Reindex,
        Some(_) => ProjectFileAction::Reindex,
    }
}

fn index_project(
    project: &ProjectRecord,
    root: &Path,
    ctx: &mut ProjectIndexContext<'_>,
) -> Result<()> {
    let registry = chunker::default_registry();
    let commit_sha = bbox_corpus_core::git::current_head(root);
    let mut files = Vec::new();
    let mut pending = Vec::new();
    let mut project_edges = Vec::new();
    let scan_stats = scan_project_files(root, &mut files)?;
    ctx.stats.skipped_symlinks += scan_stats.skipped_symlinks;
    ctx.stats.skipped_special += scan_stats.skipped_special;
    ctx.stats.skipped_unsupported += scan_stats.skipped_unsupported;
    ctx.stats.skipped_oversize += scan_stats.skipped_oversize;
    let snapshot_id = ref_snapshot_id(project, root, &files, commit_sha.as_deref());
    let base_mat_version = bbox_edge_sidecar::snapshot::current_materialization_version();
    let mat_version = snapshot_id
        .as_ref()
        .map_or(base_mat_version.clone(), |snapshot_id| {
            format!("{base_mat_version}+ref-snapshot:{snapshot_id}")
        });
    // On-disk text-file set for this project, captured before `files` is moved.
    // Used to detect tracked-file deletions (in meta, absent on disk) so their
    // derived edges are purged rather than lingering in the materialized graph.
    let current_paths: std::collections::HashSet<String> =
        files.iter().map(|(p, _, _)| p.clone()).collect();
    for (path_str, mtime, size) in files {
        match classify_project_file(ctx.meta.get(path_str.as_str()), mtime, size, &mat_version) {
            ProjectFileAction::Skip => {
                ctx.stats.skipped += 1;
                continue;
            }
            ProjectFileAction::Reindex => {
                let relative_path = Path::new(&path_str)
                    .strip_prefix(root)
                    .unwrap_or_else(|_| Path::new(&path_str));
                let selector = bbox_code_source::local_selector(&project.project_id);
                let entry_key =
                    bbox_code_source::source_entry_key(&selector, &relative_path.to_string_lossy());
                ctx.writer.delete_term(Term::from_field_text(
                    ctx.f.code_source_entry_key,
                    &entry_key,
                ));
            }
        }

        let path = PathBuf::from(&path_str);
        let relative_path = path.strip_prefix(root).unwrap_or(&path);
        let bytes = match read_regular_file_confined(root, relative_path) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "failed to read project file");
                continue;
            }
        };
        if is_binary(&path, &bytes) {
            ctx.stats.skipped += 1;
            continue;
        }
        let sniff_len = bytes.len().min(4096);
        let Some(format) = registry
            .iter()
            .find(|chunker| chunker.claims(&path, &bytes[..sniff_len]))
        else {
            ctx.stats.skipped += 1;
            continue;
        };
        let (chunks, edges) = format
            .chunk(&path, &bytes)
            .with_context(|| format!("chunking {} as {}", path.display(), format.format_id()))?;
        let chunks = finalize_chunks(project, relative_path, chunks);
        let bounded_chunks = bound_chunks(&chunks);
        let edges = derive_edges(&bounded_chunks, edges, snapshot_id.as_deref());
        ctx.stats.emitted_edges += edges.len() as u64;
        project_edges.extend(edges);
        pending.push(PendingProjectFile {
            path_str,
            absolute_path: path,
            mtime,
            size,
            chunks: bounded_chunks,
        });
    }

    // Captured before `pending` is consumed below. Combined with the git
    // commit count after history indexing, this is the per-project signal that
    // lets `snapshot_after_reindex` skip re-materializing byte-identical edges.
    let files_changed = !pending.is_empty();
    let symbol_table = build_symbol_table(&pending, snapshot_id.as_deref());
    let mut current_chunk_targets = HashMap::new();
    let scope_relpath = bbox_corpus_core::git::git_root_for_path(root)
        .and_then(|git_root| bbox_corpus_core::identity::bbox_root_relpath(&git_root, root))
        .unwrap_or_else(|| ".".to_string());
    for file in pending {
        let code_edges = derive_code_edges(
            &file.chunks,
            &symbol_table,
            ctx.stats,
            snapshot_id.as_deref(),
        );
        ctx.stats.emitted_edges += code_edges.len() as u64;
        project_edges.extend(code_edges);
        current_chunk_targets.extend(git_targets_for_scope(
            &scope_relpath,
            &file.chunks,
            snapshot_id.as_deref(),
        ));
        for chunk in file.chunks {
            let doc = build_project_file_doc(
                &chunk,
                project,
                &file.absolute_path,
                commit_sha.as_deref(),
                snapshot_id.as_deref(),
                ctx.f,
            );
            let entity_id = super::embed_hook::project_file_entity_id_for_snapshot(
                &chunk,
                snapshot_id.as_deref(),
            );
            super::embed_hook::emit_project_file(&chunk, &entity_id);
            ctx.writer.add_document(doc)?;
            ctx.stats.indexed_docs += 1;
        }
        ctx.meta.insert(
            file.path_str.clone(),
            local_file_meta(
                project,
                root,
                Path::new(&file.path_str),
                file.mtime,
                file.size,
                Some(mat_version.clone()),
            ),
        );
        ctx.stats.indexed_files += 1;
    }
    bbox_edge_sidecar::edge_sidecar::replace_materialized_edges_incremental(
        ctx.edges_dir,
        "project",
        &project.project_id,
        &project_edges,
    )?;
    let mut git_ctx = super::git_history::GitIndexContext {
        f: ctx.f,
        writer: ctx.writer,
        meta: ctx.meta,
        edges_dir: ctx.edges_dir,
        git_meta_dir: ctx.git_meta_dir,
        force_full: ctx.force_git_full,
    };
    let git_stats = super::git_history::index_git_history_for_project(
        project,
        root,
        &current_chunk_targets,
        &mut git_ctx,
    )?;
    ctx.stats.indexed_commits += git_stats.indexed_commits;
    ctx.stats.indexed_docs += git_stats.indexed_commits;
    if git_stats.indexed_commits > 0 {
        ctx.stats.indexed_files += 1;
    }
    ctx.stats.emitted_edges += git_stats.emitted_edges;
    if ctx.force_git_full {
        match bbox_edge_sidecar::edge_sidecar::compact_legacy_sidecar(
            ctx.edges_dir,
            &project.project_id,
            true,
        ) {
            Ok(stats) if stats.applied => {
                tracing::info!(
                    project_id = %project.project_id,
                    removed = stats.derived_edges_removed,
                    retained = stats.retained_lines,
                    bytes_before = stats.bytes_before,
                    bytes_after = stats.bytes_after,
                    "compacted legacy edge sidecar after full project refresh"
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(
                    project_id = %project.project_id,
                    error = %err,
                    "failed to compact legacy edge sidecar after full project refresh"
                );
            }
        }
    }
    // Purge derived edges for tracked files that were deleted (or are no longer
    // indexable) this pass: present in meta under `root` but absent from the
    // current on-disk scan. The Tantivy docs are purged separately by the
    // reindex deletion sweep; without this, the file's file-anchored edges
    // (NEXT_SECTION / DEFINED_IN / CONTAINS_SYMBOL) survive in the materialized
    // graph. Matched by rel_path_hash, mirroring the incremental-replace
    // granularity; symbol→symbol edges (CALLS/USES_TYPE) carry no file ref and
    // age out with the snapshot id rather than being purged here.
    let deleted_rel_hashes: std::collections::HashSet<String> = ctx
        .meta
        .keys()
        .filter(|key| !current_paths.contains(key.as_str()))
        .filter_map(|key| {
            let rel = Path::new(key).strip_prefix(root).ok()?;
            Some(short_hash(rel.to_string_lossy().as_bytes()))
        })
        .collect();
    let deletions_purged = if deleted_rel_hashes.is_empty() {
        0
    } else {
        bbox_edge_sidecar::edge_sidecar::purge_managed_edges_for_path_hashes(
            ctx.edges_dir,
            "project",
            &project.project_id,
            &deleted_rel_hashes,
        )?
    };

    let materialization_changed =
        files_changed || git_stats.indexed_commits > 0 || deletions_purged > 0;
    if let Some(pending) =
        snapshot_after_reindex(project, root, ctx.edges_dir, materialization_changed)?
    {
        ctx.stats.pending_local_snapshots.push(pending);
    }
    Ok(())
}

fn scan_project_files(
    root: &Path,
    out: &mut Vec<(String, u64, u64)>,
) -> Result<ProjectFileScanStats> {
    let mut stats = ProjectFileScanStats::default();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| entry.depth() == 0 || !is_skipped_entry(entry))
        .build();
    for entry in walker.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            stats.skipped_symlinks += 1;
            continue;
        }
        if !meta.is_file() {
            if path != root {
                stats.skipped_special += 1;
            }
            continue;
        }
        let Some(max_bytes) = bbox_code_source::max_bytes_for_path(path) else {
            stats.skipped_unsupported += 1;
            continue;
        };
        if meta.len() > max_bytes {
            stats.skipped_oversize += 1;
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        let Some(path) = path.to_str() else {
            stats.skipped_unsupported += 1;
            continue;
        };
        out.push((path.to_string(), mtime, meta.len()));
    }
    Ok(stats)
}

fn is_skipped_entry(entry: &DirEntry) -> bool {
    // `.bbox/` is blackbox's own control directory: project config, MCP wiring,
    // catalog-owned artifacts, and (per the repo-owned-project-state design)
    // structured knowledge owned by a dedicated spooler. It must NOT be pulled
    // into the generic project_file corpus, or its JSON/TOML/MD gets indexed as
    // project source — duplicating catalog/knowledge entities with confusing
    // search hits. Skip it like any other dotdir.
    entry
        .file_name()
        .to_str()
        .is_some_and(bbox_code_source::is_skipped_component)
}

#[cfg(test)]
fn is_supported_text_path(path: &Path) -> bool {
    bbox_code_source::is_supported_source_path(path)
}

#[cfg(unix)]
fn read_regular_file_confined(root: &Path, relative_path: &Path) -> Result<Vec<u8>> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    bbox_code_source::validate_relative_path(
        relative_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("source path is not valid UTF-8"))?,
    )?;
    let max_bytes = bbox_code_source::max_bytes_for_path(relative_path)
        .ok_or_else(|| anyhow::anyhow!("unsupported source path"))?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut directory = options.open(root)?;
    let components = relative_path.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            anyhow::bail!("source path has a non-normal component");
        };
        let name = CString::new(name.as_bytes()).context("source path contains NUL")?;
        let last = index + 1 == components.len();
        let flags = if last {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let opened = unsafe { fs::File::from_raw_fd(fd) };
        if last {
            let metadata = opened.metadata()?;
            if !metadata.is_file() || metadata.len() > max_bytes {
                anyhow::bail!("source is not a regular bounded file");
            }
            let mut bytes = Vec::new();
            opened
                .take(max_bytes.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > max_bytes {
                anyhow::bail!("source exceeds its byte cap");
            }
            return Ok(bytes);
        }
        directory = opened;
    }
    anyhow::bail!("source path is empty")
}

#[cfg(not(unix))]
fn read_regular_file_confined(root: &Path, relative_path: &Path) -> Result<Vec<u8>> {
    let max_bytes = bbox_code_source::max_bytes_for_path(relative_path)
        .ok_or_else(|| anyhow::anyhow!("unsupported source path"))?;
    let path = root.join(relative_path);
    let canonical_parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("source path has no parent"))?
        .canonicalize()?;
    if !canonical_parent.starts_with(root) {
        anyhow::bail!("source path escaped configured root");
    }
    let file = fs::OpenOptions::new().read(true).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        anyhow::bail!("source is not a regular bounded file");
    }
    let mut bytes = Vec::with_capacity(metadata.len().min(max_bytes) as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("source exceeds its byte cap");
    }
    Ok(bytes)
}

fn local_file_meta(
    project: &ProjectRecord,
    root: &Path,
    path: &Path,
    mtime: u64,
    size: u64,
    mat_version: Option<String>,
) -> FileMeta {
    let relative_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let selector = bbox_code_source::local_selector(&project.project_id);
    let entry_key = bbox_code_source::source_entry_key(&selector, &relative_path);
    FileMeta {
        mtime,
        size,
        mat_version,
        source: FileMetaSource::LocalProjectFile {
            project_id: project.project_id.clone(),
            selector,
            relative_path,
            entry_key,
        },
    }
}

fn finalize_chunks(project: &ProjectRecord, rel_path: &Path, chunks: Vec<Chunk>) -> Vec<Chunk> {
    let rel_path_hash = short_hash(rel_path.to_string_lossy().as_bytes());
    chunks
        .into_iter()
        .enumerate()
        .map(|(idx, mut chunk)| {
            let chunk_hash = full_hash(chunk.content.as_bytes());
            chunk.project_id = project.project_id.clone();
            chunk.file_path = rel_path.to_path_buf();
            chunk.rel_path_hash.clone_from(&rel_path_hash);
            chunk.chunk_hash = chunk_hash;
            chunk.occurrence_idx = idx as u32;
            chunk
        })
        .collect()
}

fn git_targets_for_scope(
    bbox_root_relpath: &str,
    chunks: &[Chunk],
    snapshot_id: Option<&str>,
) -> HashMap<String, EntityRef> {
    super::git_history::current_chunk_targets(chunks, snapshot_id)
        .into_iter()
        .map(|(relative_path, entity)| {
            let git_path = if bbox_root_relpath == "." {
                relative_path
            } else {
                format!("{bbox_root_relpath}/{relative_path}")
            };
            (git_path, entity)
        })
        .collect()
}

fn split_oversized_chunk(chunk: &Chunk) -> Vec<Chunk> {
    if chunk.content.len() <= chunker::MAX_CHUNK_BYTES {
        return vec![chunk.clone()];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < chunk.content.len() {
        let mut end = (start + chunker::MAX_CHUNK_BYTES).min(chunk.content.len());
        while !chunk.content.is_char_boundary(end) {
            end -= 1;
        }
        let content = chunk.content[start..end].to_string();
        let mut split = chunk.clone();
        split.content = content;
        split.byte_start = chunk.byte_start + start as u64;
        split.byte_end = chunk.byte_start + end as u64;
        split.chunk_hash = full_hash(split.content.as_bytes());
        split.occurrence_idx = out.len() as u32;
        out.push(split);
        start = end;
    }
    out
}

fn bound_chunks(chunks: &[Chunk]) -> Vec<Chunk> {
    chunks
        .iter()
        .flat_map(split_oversized_chunk)
        .enumerate()
        .map(|(idx, mut chunk)| {
            chunk.occurrence_idx = idx as u32;
            chunk
        })
        .collect()
}

fn derive_edges(chunks: &[Chunk], mut edges: Vec<Edge>, snapshot_id: Option<&str>) -> Vec<Edge> {
    for pair in chunks.windows(2) {
        edges.push(Edge {
            source: chunk_ref(&pair[0], snapshot_id),
            kind: "NEXT_SECTION".to_string(),
            target: chunk_ref(&pair[1], snapshot_id),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        });
    }
    // Markdown file/section link extraction is separate from storage lifecycle:
    // this pass indexes current project-file chunks and code-symbol edges.
    edges
}

fn build_symbol_table(
    files: &[PendingProjectFile],
    snapshot_id: Option<&str>,
) -> HashMap<String, EntityRef> {
    let mut symbols = HashMap::new();
    for file in files {
        extend_symbol_table(&mut symbols, &file.chunks, snapshot_id);
    }
    symbols
}

fn extend_symbol_table(
    symbols: &mut HashMap<String, EntityRef>,
    chunks: &[Chunk],
    snapshot_id: Option<&str>,
) {
    for chunk in chunks {
        if chunk.chunk_kind != "code_block" {
            continue;
        }
        let Some(qualified_name) = &chunk.symbol else {
            continue;
        };
        let symbol = symbol_ref(chunk, qualified_name, snapshot_id);
        symbols
            .entry(qualified_name.clone())
            .or_insert(symbol.clone());
        if let Some(bare) = &chunk.symbol_exact {
            symbols.entry(bare.clone()).or_insert(symbol);
        }
    }
}

fn derive_code_edges(
    chunks: &[Chunk],
    symbols: &HashMap<String, EntityRef>,
    stats: &mut ProjectIndexStats,
    snapshot_id: Option<&str>,
) -> Vec<Edge> {
    let mut edges = Vec::new();
    for chunk in chunks
        .iter()
        .filter(|chunk| chunk.chunk_kind == "code_block")
    {
        let file_ref = chunk_ref(chunk, snapshot_id);
        if let Some(qualified_name) = &chunk.symbol {
            let symbol = symbol_ref(chunk, qualified_name, snapshot_id);
            edges.push(edge(
                symbol.clone(),
                "DEFINED_IN",
                file_ref.clone(),
                EdgeConfidence::Exact,
            ));
            edges.push(edge(
                file_ref.clone(),
                "CONTAINS_SYMBOL",
                symbol.clone(),
                EdgeConfidence::Exact,
            ));
            edges.extend(derive_has_field_edges(chunk, &symbol, symbols));
            edges.extend(derive_impl_trait_edges(chunk, &symbol, symbols));
            for callee in call_names(&chunk.content) {
                if let Some(target) = symbols.get(&callee) {
                    edges.push(edge(
                        symbol.clone(),
                        "CALLS",
                        target.clone(),
                        EdgeConfidence::Heuristic,
                    ));
                    stats.call_edges += 1;
                    stats.resolved_call_edges += 1;
                }
            }
            for type_name in type_names(&chunk.content) {
                if let Some(target) = symbols.get(&type_name) {
                    edges.push(edge(
                        symbol.clone(),
                        "USES_TYPE",
                        target.clone(),
                        EdgeConfidence::Heuristic,
                    ));
                }
            }
        }
    }
    edges
}

fn edge(source: EntityRef, kind: &str, target: EntityRef, confidence: EdgeConfidence) -> Edge {
    Edge {
        source,
        kind: kind.to_string(),
        target,
        provenance: EdgeProvenance::Derived,
        confidence,
    }
}

fn symbol_ref(chunk: &Chunk, qualified_name: &str, snapshot_id: Option<&str>) -> EntityRef {
    if let Some(snapshot_id) = snapshot_id {
        return EntityRef::SymbolV2 {
            project_id: chunk.project_id.clone(),
            snapshot_id: snapshot_id.to_string(),
            qualified_name: qualified_name.to_string(),
            defn_hash: chunk.chunk_hash.clone(),
        };
    }
    EntityRef::Symbol {
        project_id: chunk.project_id.clone(),
        qualified_name: qualified_name.to_string(),
        defn_hash: chunk.chunk_hash.clone(),
    }
}

fn resolve_symbol<'a>(
    symbols: &'a HashMap<String, EntityRef>,
    name: &str,
) -> Option<&'a EntityRef> {
    symbols.get(name).or_else(|| {
        name.rsplit_once("::")
            .and_then(|(_, bare)| symbols.get(bare))
            .or_else(|| {
                name.rsplit_once('.')
                    .and_then(|(_, bare)| symbols.get(bare))
            })
    })
}

fn derive_has_field_edges(
    chunk: &Chunk,
    source: &EntityRef,
    symbols: &HashMap<String, EntityRef>,
) -> Vec<Edge> {
    let Some(struct_name) = &chunk.symbol else {
        return Vec::new();
    };
    if !chunk.content.contains("struct ") {
        return Vec::new();
    }
    field_names(&chunk.content)
        .into_iter()
        .filter_map(|field| {
            let target = resolve_symbol(symbols, &format!("{struct_name}::{field}"))?;
            Some(edge(
                source.clone(),
                "HAS_FIELD",
                target.clone(),
                EdgeConfidence::Heuristic,
            ))
        })
        .collect()
}

fn derive_impl_trait_edges(
    chunk: &Chunk,
    source: &EntityRef,
    symbols: &HashMap<String, EntityRef>,
) -> Vec<Edge> {
    let header = chunk.content.split('{').next().unwrap_or_default().trim();
    let Some(rest) = header.strip_prefix("impl ") else {
        return Vec::new();
    };
    let Some((trait_name, _target)) = rest.split_once(" for ") else {
        return Vec::new();
    };
    let Some(target) = resolve_symbol(symbols, trait_name.trim()) else {
        return Vec::new();
    };
    vec![edge(
        source.clone(),
        "IMPLEMENTS_TRAIT",
        target.clone(),
        EdgeConfidence::Heuristic,
    )]
}

fn call_names(content: &str) -> Vec<String> {
    let call_pattern = regex::Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap();
    call_pattern
        .captures_iter(content)
        .filter_map(|capture| capture.get(1).map(|name| name.as_str()))
        .filter(|name| !CALL_KEYWORDS.contains(name))
        .map(str::to_string)
        .collect()
}

fn type_names(content: &str) -> Vec<String> {
    let type_pattern = regex::Regex::new(r"\b([A-Z][A-Za-z0-9_]{2,})\b").unwrap();
    type_pattern
        .captures_iter(content)
        .filter_map(|capture| capture.get(1).map(|name| name.as_str().to_string()))
        .collect()
}

fn field_names(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || !trimmed.contains(':') {
                return None;
            }
            let left = trimmed.split(':').next()?.trim();
            let name = left.split_whitespace().last()?.trim_start_matches("pub ");
            if name
                .chars()
                .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
            {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

const CALL_KEYWORDS: &[&str] = &[
    "as",
    "assert",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "const",
    "continue",
    "def",
    "defer",
    "delete",
    "do",
    "else",
    "finally",
    "fn",
    "for",
    "from",
    "function",
    "go",
    "if",
    "import",
    "in",
    "instanceof",
    "is",
    "lambda",
    "let",
    "loop",
    "match",
    "nameof",
    "new",
    "of",
    "raise",
    "return",
    "select",
    "sizeof",
    "switch",
    "then",
    "throw",
    "try",
    "typeof",
    "unless",
    "using",
    "var",
    "when",
    "where",
    "while",
    "with",
    "yield",
];

fn chunk_ref(chunk: &Chunk, snapshot_id: Option<&str>) -> EntityRef {
    if let Some(snapshot_id) = snapshot_id {
        return EntityRef::ProjectFileV2 {
            project_id: chunk.project_id.clone(),
            snapshot_id: snapshot_id.to_string(),
            rel_path_hash: chunk.rel_path_hash.clone(),
            chunk_hash: chunk.chunk_hash.clone(),
            occurrence_idx: chunk.occurrence_idx,
        };
    }
    EntityRef::ProjectFile {
        project_id: chunk.project_id.clone(),
        rel_path_hash: chunk.rel_path_hash.clone(),
        chunk_hash: chunk.chunk_hash.clone(),
        occurrence_idx: chunk.occurrence_idx,
    }
}

/// PDFs and spreadsheet workbooks are legitimately binary (embedded
/// streams, xref/trailer binary markers, font/image data for PDF; ZIP
/// central-directory/OLE2 CFB structure and compressed part data for
/// xlsx/xlsm/xlam/xlsb/xls/ods) and are expected to contain NUL bytes in
/// their first 4096 bytes; the null-byte heuristic below would otherwise
/// exclude nearly every real-world file of these formats before it ever
/// reaches the chunker registry's own `claims()` (magic-header) check.
/// `PdfChunker::claims` (crates/bbox-chunker/src/pdf.rs) and
/// `XlsxChunker::claims` (crates/bbox-chunker/src/xlsx.rs) are the real
/// gates for whether such a file's content is extractable, so the blanket
/// binary sniff is bypassed by extension here rather than tightened
/// generically. Raster images (X-IMG) are exempted for the same reason:
/// they are byte-diverse compressed binary by construction, and
/// `XImgChunker::claims` (crates/bbox-chunker/src/ximg.rs) is the real
/// gate (extension + magic-byte scan).
fn is_binary(path: &Path, bytes: &[u8]) -> bool {
    if matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "pdf"
                | "xlsx"
                | "xlsm"
                | "xlam"
                | "xlsb"
                | "xls"
                | "ods"
                | "docx"
                | "pptx"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
        )
    ) {
        return false;
    }
    bytes.iter().take(4096).any(|byte| *byte == 0)
}

fn full_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn short_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex::encode(&digest[..4])
}

/// Returns true iff the on-disk materialization for `project_id` already
/// reflects the current HEAD, indexer/chunker version, and worktree dirty state
/// — i.e. re-running `switch_to_*` would reproduce byte-identical edges and only
/// churn mtimes. Any inconsistency (cold start, version bump, branch switch,
/// dirty↔clean drift, GC'd active path) returns false so we materialize as today.
///
/// Gates on the `ManifestIndex` (the loader authority via
/// `active_materialized_paths`), not `WorkspaceManifest`, whose `dirty`/
/// `dirty_fingerprint` fields have no runtime reader and may drift on
/// metadata-only changes (`git add`, same-HEAD branch relabel).
fn materialization_is_current(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    head_sha: &str,
    worktree_dirty: bool,
) -> bool {
    let Ok(idx) = bbox_edge_sidecar::manifest::ManifestIndex::load(edges_dir) else {
        // No manifest-index yet (cold start / never materialized) ⇒ materialize.
        return false;
    };
    let Some(entry) = idx.workspaces.get(project_id) else {
        return false;
    };

    // Version-aware expected snapshot for the current HEAD. `clean_snapshot_id`
    // folds INDEXER_VERSION/CHUNKER_VERSION, so a version bump with unchanged
    // mtimes yields a different id ⇒ mismatch ⇒ not skipped.
    let expected_snap =
        bbox_edge_sidecar::snapshot::clean_snapshot_id(repo_id, project_id, head_sha);
    let expected_snap_rel =
        bbox_edge_sidecar::snapshot::active_snapshot_rel(project_id, &expected_snap);
    if entry.active_snapshot.as_deref() != Some(expected_snap_rel.as_str()) {
        return false;
    }

    // Dirty-state consistency across three sources: current worktree, the
    // ManifestIndex overlay pointer, and the overlay dir on disk. Any
    // disagreement forces re-materialization (e.g. a clean checkout that left a
    // stale overlay, which `switch_to_clean_snapshot` must clear).
    let overlay_rel = bbox_edge_sidecar::snapshot::dirty_overlay_rel(project_id);
    let overlay_in_manifest = entry.dirty_overlay.as_deref() == Some(overlay_rel.as_str());
    let overlay_on_disk =
        bbox_edge_sidecar::snapshot::dirty_overlay_dir(edges_dir, project_id).is_dir();
    if worktree_dirty {
        if !overlay_in_manifest || !overlay_on_disk {
            return false;
        }
    } else {
        if entry.dirty_overlay.is_some() || overlay_on_disk {
            return false;
        }
        // Clean ⇒ the active snapshot dir is what the loader reads; it must
        // exist. `active_materialized_paths` silently drops missing dirs, so a
        // GC between passes would lose this project from the graph if we skipped.
        if !bbox_edge_sidecar::snapshot::snapshot_dir(edges_dir, project_id, &expected_snap)
            .is_dir()
        {
            return false;
        }
    }

    true
}

fn snapshot_after_reindex(
    project: &ProjectRecord,
    root: &Path,
    edges_dir: &Path,
    materialization_changed: bool,
) -> Result<Option<bbox_edge_sidecar::snapshot::PendingLocalSnapshotActivation>> {
    let Some(repo_id) = project.repo_id.as_deref() else {
        return Ok(None);
    };
    let Some(head_sha) = bbox_corpus_core::git::current_head(root) else {
        return Ok(None);
    };
    let branch = bbox_corpus_core::git::current_branch(root);
    let worktree_dirty = bbox_corpus_core::git::is_worktree_dirty(root);

    // Writer-side materialization idempotency. Re-running `switch_to_*` rewrites
    // the dirty overlay via temp-dir + atomic rename, which stamps fresh mtimes
    // on `dirty-current/*.jsonl`. The edge-index rebuild watcher sums sidecar
    // mtimes, so a byte-identical re-materialization still trips a full 18-21s
    // EdgeIndex rebuild. When this pass changed nothing for the project and the
    // on-disk materialization already matches the current head/version/worktree
    // state, skip it. Correctness rests on: derived overlay/snapshot edge content
    // is a deterministic function of (head_sha, changed-file set + contents). No
    // re-chunked file (empty `pending`) and no indexed commit ⇒ identical edges.
    if !materialization_changed
        && materialization_is_current(
            edges_dir,
            &project.project_id,
            repo_id,
            &head_sha,
            worktree_dirty,
        )
    {
        return Ok(None);
    }

    let project_edges = bbox_edge_sidecar::edge_sidecar::read_managed_derived_edges(
        edges_dir,
        "project",
        &project.project_id,
    )?;
    let git_edges = bbox_edge_sidecar::edge_sidecar::read_managed_derived_edges(
        edges_dir,
        "git",
        &project.project_id,
    )?;

    let snapshot_edges: Vec<bbox_edge_sidecar::edge_sidecar::Edge> = project_edges
        .iter()
        .map(|e| bbox_edge_sidecar::edge_sidecar::Edge {
            source: e.source.clone(),
            kind: e.kind.clone(),
            target: e.target.clone(),
            provenance: e.provenance,
            confidence: e.confidence,
            metadata: Default::default(),
        })
        .collect();
    let git_snapshot_edges: Vec<bbox_edge_sidecar::edge_sidecar::Edge> = git_edges
        .iter()
        .map(|e| bbox_edge_sidecar::edge_sidecar::Edge {
            source: e.source.clone(),
            kind: e.kind.clone(),
            target: e.target.clone(),
            provenance: e.provenance,
            confidence: e.confidence,
            metadata: Default::default(),
        })
        .collect();

    let pending = if worktree_dirty {
        let fingerprint = bbox_corpus_core::git::dirty_fingerprint(root).unwrap_or_default();
        let snapshot_id =
            bbox_edge_sidecar::snapshot::nongit_snapshot_id(&project.project_id, &fingerprint);
        bbox_edge_sidecar::snapshot::stage_local_snapshot_activation(
            edges_dir,
            &project.project_id,
            repo_id,
            branch.as_deref(),
            &head_sha,
            true,
            Some(&fingerprint),
            &snapshot_id,
            &snapshot_edges,
            &[],
            &git_snapshot_edges,
        )?
    } else {
        let snapshot_id =
            bbox_edge_sidecar::snapshot::clean_snapshot_id(repo_id, &project.project_id, &head_sha);
        bbox_edge_sidecar::snapshot::stage_local_snapshot_activation(
            edges_dir,
            &project.project_id,
            repo_id,
            branch.as_deref(),
            &head_sha,
            false,
            None,
            &snapshot_id,
            &snapshot_edges,
            &[],
            &git_snapshot_edges,
        )?
    };
    Ok(Some(pending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_schema;
    use bbox_chunker::SourceFormatChunker;
    use tantivy::schema::Field;

    #[test]
    fn document_containers_get_the_larger_byte_budget() {
        use std::path::Path;
        assert_eq!(
            bbox_code_source::max_bytes_for_path(Path::new("deck.pdf")),
            Some(bbox_code_source::MAX_DOCUMENT_FILE_BYTES)
        );
        assert_eq!(
            bbox_code_source::max_bytes_for_path(Path::new("Board.DOCX")),
            Some(bbox_code_source::MAX_DOCUMENT_FILE_BYTES)
        );
        assert_eq!(
            bbox_code_source::max_bytes_for_path(Path::new("main.rs")),
            Some(bbox_code_source::MAX_TEXT_FILE_BYTES)
        );
        assert_eq!(
            bbox_code_source::max_bytes_for_path(Path::new("notes.md")),
            Some(bbox_code_source::MAX_TEXT_FILE_BYTES)
        );
    }

    #[test]
    fn images_get_the_provider_capped_byte_budget() {
        use std::path::Path;
        for name in [
            "figure.png",
            "shot.JPG",
            "photo.jpeg",
            "anim.gif",
            "icon.webp",
        ] {
            assert_eq!(
                bbox_code_source::max_bytes_for_path(Path::new(name)),
                Some(bbox_code_source::MAX_IMAGE_FILE_BYTES),
                "{name}"
            );
        }
    }

    #[test]
    fn images_are_supported_and_exempted_from_the_binary_sniff() {
        use std::path::Path;
        assert!(is_supported_text_path(Path::new("figure.png")));
        assert!(is_supported_text_path(Path::new("shot.jpeg")));
        // Real image bytes are byte-diverse binary (NUL bytes included);
        // the extension bypass must apply even though the content sniff
        // alone would call it binary.
        assert!(!is_binary(Path::new("figure.png"), &[0u8; 16]));
        // A non-image file with the same NUL-heavy content is still binary.
        assert!(is_binary(Path::new("figure.bin"), &[0u8; 16]));
    }

    #[test]
    fn project_file_doc_includes_agentic_fields() {
        let (_schema, fields) = build_schema();
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunk = Chunk {
            project_id: "proj1234".into(),
            file_path: PathBuf::from("design/agentic-corpus.md"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "doc_section".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 0,
            language: Some("md".into()),
            symbol: None,
            symbol_exact: None,
            symbol_kind: None,
            parent_kind: None,
            line_start: None,
            line_end: None,
            content: "agentic-corpus design".into(),
            byte_start: 10,
            byte_end: 32,
            visual_payload: None,
        };

        let commit_sha = "a".repeat(40);
        let doc = build_project_file_doc(
            &chunk,
            &project,
            Path::new("/tmp/repo/design/agentic-corpus.md"),
            Some(commit_sha.as_str()),
            None,
            fields,
        );

        assert_eq!(first_text(&doc, fields.doc_type), "project_file");
        assert_eq!(first_text(&doc, fields.chunk_kind), "doc_section");
        assert_eq!(first_text(&doc, fields.language), "md");
        assert_eq!(first_text(&doc, fields.repo_id), "repo1234");
        assert_eq!(
            first_text(&doc, fields.entity_id),
            format!("project_file:proj1234:abcd1234:{}:0", "f".repeat(64))
        );
    }

    #[test]
    fn project_file_doc_can_emit_snapshot_specific_entity_id() {
        let (_schema, fields) = build_schema();
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunk = Chunk {
            project_id: "proj1234".into(),
            file_path: PathBuf::from("src/lib.rs"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "code_block".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 3,
            language: Some("rust".into()),
            symbol: Some("helper".into()),
            symbol_exact: Some("crate::helper".into()),
            symbol_kind: Some("function".into()),
            parent_kind: None,
            line_start: Some(1),
            line_end: Some(4),
            content: "fn helper() {}".into(),
            byte_start: 0,
            byte_end: 14,
            visual_payload: None,
        };

        let commit_sha = "a".repeat(40);
        let doc = build_project_file_doc(
            &chunk,
            &project,
            Path::new("/tmp/repo/src/lib.rs"),
            Some(commit_sha.as_str()),
            Some("head-repo1234-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            fields,
        );

        assert_eq!(
            first_text(&doc, fields.entity_id),
            format!(
                "project_file_v2:proj1234:head-repo1234-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:abcd1234:{}:3",
                "f".repeat(64)
            )
        );
    }

    #[test]
    fn tier_a_call_edges_resolve_against_symbol_table() {
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunks = finalize_chunks(
            &project,
            Path::new("src/lib.rs"),
            vec![
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "fn helper() {}",
                    0,
                    14,
                    0,
                ),
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "fn caller() { helper(); }",
                    15,
                    39,
                    1,
                ),
            ],
        )
        .into_iter()
        .enumerate()
        .map(|(idx, mut chunk)| {
            if idx == 0 {
                chunk.symbol = Some("helper".into());
                chunk.symbol_exact = Some("helper".into());
            } else {
                chunk.symbol = Some("caller".into());
                chunk.symbol_exact = Some("caller".into());
            }
            chunk
        })
        .collect::<Vec<_>>();
        let pending = vec![PendingProjectFile {
            path_str: "/tmp/repo/src/lib.rs".into(),
            absolute_path: PathBuf::from("/tmp/repo/src/lib.rs"),
            mtime: 1,
            size: 39,
            chunks,
        }];
        let symbols = build_symbol_table(&pending, None);
        let mut stats = ProjectIndexStats::default();
        let edges = derive_code_edges(&pending[0].chunks, &symbols, &mut stats, None);
        assert!(edges.iter().any(|edge| edge.kind == "CALLS"));
        assert!(stats.call_edges >= 1);
        assert_eq!(stats.resolved_call_edges, stats.call_edges);
    }

    #[test]
    fn tier_a_edges_skip_external_symbol_targets() {
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunks = finalize_chunks(
            &project,
            Path::new("src/lib.rs"),
            vec![
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "trait LocalTrait {}",
                    0,
                    19,
                    0,
                ),
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "impl LocalTrait for Thing {}\nuse std::fmt::Display;",
                    20,
                    72,
                    1,
                ),
            ],
        )
        .into_iter()
        .enumerate()
        .map(|(idx, mut chunk)| {
            if idx == 0 {
                chunk.symbol = Some("LocalTrait".into());
                chunk.symbol_exact = Some("LocalTrait".into());
            } else {
                chunk.symbol = Some("Thing::impl".into());
                chunk.symbol_exact = Some("impl".into());
            }
            chunk
        })
        .collect::<Vec<_>>();
        let pending = vec![PendingProjectFile {
            path_str: "/tmp/repo/src/lib.rs".into(),
            absolute_path: PathBuf::from("/tmp/repo/src/lib.rs"),
            mtime: 1,
            size: 72,
            chunks,
        }];
        let symbols = build_symbol_table(&pending, None);
        let mut stats = ProjectIndexStats::default();
        let edges = derive_code_edges(&pending[0].chunks, &symbols, &mut stats, None);

        assert!(edges.iter().any(|edge| edge.kind == "IMPLEMENTS_TRAIT"));
        assert!(!edges.iter().any(|edge| edge.kind == "IMPORTS"));
    }

    #[test]
    fn call_names_skip_flow_control_keywords() {
        let names = call_names("if (cond) { foo(); }");

        assert!(!names.iter().any(|name| name == "if"));
        assert!(names.iter().any(|name| name == "foo"));
    }

    #[test]
    fn json_chunk_hashes_survive_noncanonical_formatting() {
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let left = br#"
        {
          "b": 2,
          "a": { "z": true }
        }
        "#;
        let right = br#"{"a":{"z":true},"b":2}"#;

        let left_chunks = bbox_chunker::config::JsonChunker
            .chunk(Path::new("config.json"), left)
            .unwrap()
            .0;
        let right_chunks = bbox_chunker::config::JsonChunker
            .chunk(Path::new("config.json"), right)
            .unwrap()
            .0;
        let left_chunks = finalize_chunks(&project, Path::new("config.json"), left_chunks);
        let right_chunks = finalize_chunks(&project, Path::new("config.json"), right_chunks);
        let left_hashes = left_chunks
            .iter()
            .map(|chunk| (chunk.content.clone(), chunk.chunk_hash.clone()))
            .collect::<Vec<_>>();
        let right_hashes = right_chunks
            .iter()
            .map(|chunk| (chunk.content.clone(), chunk.chunk_hash.clone()))
            .collect::<Vec<_>>();

        assert_eq!(left_hashes, right_hashes);
    }

    fn first_text(doc: &TantivyDocument, field: Field) -> String {
        doc.get_all(field)
            .next()
            .and_then(|v| match v {
                tantivy::schema::OwnedValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    // --- materialization idempotency guard (issue #2 follow-up) ---
    //
    // These exercise `materialization_is_current`, the decision behind skipping a
    // no-op `snapshot_after_reindex`. Skipping when it returns true is what keeps
    // a byte-identical re-materialization from re-stamping overlay mtimes and
    // tripping the edge-index rebuild watcher; the "force" cases guard against
    // skipping when the on-disk graph would actually go stale.

    const MAT_REPO: &str = "repo-mat";
    const MAT_PROJ: &str = "proj-mat";
    const MAT_HEAD: &str = "abc123def456";

    fn mat_edge(id: &str, target: &str) -> bbox_edge_sidecar::edge_sidecar::Edge {
        bbox_edge_sidecar::edge_sidecar::Edge {
            source: EntityRef::Knowledge { id: id.into() },
            kind: "DESCRIBES".into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: Default::default(),
        }
    }

    fn seed_clean(edges_dir: &Path) {
        bbox_edge_sidecar::snapshot::switch_to_clean_snapshot(
            edges_dir,
            MAT_PROJ,
            MAT_REPO,
            Some("main"),
            MAT_HEAD,
            vec![mat_edge("k1", "k2")],
            vec![],
            vec![],
        )
        .unwrap();
    }

    fn seed_dirty(edges_dir: &Path) {
        bbox_edge_sidecar::snapshot::switch_to_dirty_overlay(
            edges_dir,
            MAT_PROJ,
            MAT_REPO,
            Some("main"),
            MAT_HEAD,
            "fp-dirty",
            vec![mat_edge("k_dirty", "k2")],
            vec![],
            vec![],
        )
        .unwrap();
    }

    #[test]
    fn classify_project_file_covers_skip_and_reindex() {
        let v = bbox_edge_sidecar::snapshot::current_materialization_version();
        let current = FileMeta {
            mtime: 100,
            size: 200,
            mat_version: Some(v.clone()),
            source: FileMetaSource::LegacyFilesystem,
        };
        // Fully current → skip.
        assert_eq!(
            classify_project_file(Some(&current), 100, 200, &v),
            ProjectFileAction::Skip
        );
        // mtime or size drift → reindex.
        assert_eq!(
            classify_project_file(Some(&current), 101, 200, &v),
            ProjectFileAction::Reindex
        );
        assert_eq!(
            classify_project_file(Some(&current), 100, 201, &v),
            ProjectFileAction::Reindex
        );
        // Known-different version with identical content → reindex (real bump).
        let stale = FileMeta {
            mtime: 100,
            size: 200,
            mat_version: Some("older-version".into()),
            source: FileMetaSource::LegacyFilesystem,
        };
        assert_eq!(
            classify_project_file(Some(&stale), 100, 200, &v),
            ProjectFileAction::Reindex
        );
        // Unknown stored version cannot prove the current materialization.
        let legacy = FileMeta {
            mtime: 100,
            size: 200,
            mat_version: None,
            source: FileMetaSource::LegacyFilesystem,
        };
        assert_eq!(
            classify_project_file(Some(&legacy), 100, 200, &v),
            ProjectFileAction::Reindex
        );
        // Unknown version but content drift → reindex (content wins).
        assert_eq!(
            classify_project_file(Some(&legacy), 101, 200, &v),
            ProjectFileAction::Reindex
        );
        // Never-seen file → reindex.
        assert_eq!(
            classify_project_file(None, 100, 200, &v),
            ProjectFileAction::Reindex
        );
    }

    #[test]
    fn materialization_cold_start_forces_rematerialize() {
        let dir = tempfile::tempdir().unwrap();
        // No manifest-index on disk yet (never materialized).
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn materialization_clean_steady_state_skips() {
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        assert!(materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn materialization_dirty_steady_state_skips() {
        let dir = tempfile::tempdir().unwrap();
        seed_dirty(dir.path());
        assert!(materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            true
        ));
    }

    #[test]
    fn materialization_head_or_version_change_forces_rematerialize() {
        // A different HEAD — and equivalently an INDEXER/CHUNKER_VERSION bump,
        // since both feed the hashed snapshot_id — must re-materialize even when
        // file mtimes are unchanged.
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            "deadbeefcafe",
            false
        ));
    }

    #[test]
    fn materialization_clean_with_stale_overlay_forces_rematerialize() {
        // Worktree is clean now but a dirty overlay is still active; the clean
        // switch must run to clear it, so skipping would leave the stale overlay.
        let dir = tempfile::tempdir().unwrap();
        seed_dirty(dir.path());
        assert!(bbox_edge_sidecar::snapshot::dirty_overlay_dir(dir.path(), MAT_PROJ).is_dir());
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn materialization_dirty_without_overlay_forces_rematerialize() {
        // Worktree just went dirty but only a clean snapshot is materialized.
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            true
        ));
    }

    #[test]
    fn materialization_missing_active_snapshot_dir_forces_rematerialize() {
        // Manifest still references a snapshot dir that has been GC'd off disk.
        // active_materialized_paths drops missing dirs silently, so re-materialize.
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        let snap_id = bbox_edge_sidecar::snapshot::clean_snapshot_id(MAT_REPO, MAT_PROJ, MAT_HEAD);
        let snap = bbox_edge_sidecar::snapshot::snapshot_dir(dir.path(), MAT_PROJ, &snap_id);
        std::fs::remove_dir_all(&snap).unwrap();
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn scan_skips_bbox_control_dir() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Normal project source — must be indexed.
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("README.md"), "# project").unwrap();

        // blackbox control dir — must NOT be indexed (config, MCP wiring,
        // catalog-owned artifacts, and future structured knowledge live here,
        // owned by other subsystems).
        fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        fs::write(root.join(".bbox/config.toml"), "x = 1").unwrap();
        fs::write(root.join(".bbox/mcp.json"), "{}").unwrap();
        fs::write(root.join(".bbox/knowledge/entry.json"), "{}").unwrap();

        // Another dotdir — already skipped; sanity anchor.
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "[core]").unwrap();

        let mut out = Vec::new();
        scan_project_files(root, &mut out).unwrap();
        let indexed: Vec<String> = out.iter().map(|(p, _, _)| p.clone()).collect();

        assert!(
            indexed.iter().any(|p| p.ends_with("src/main.rs")),
            "normal source should be indexed: {indexed:?}"
        );
        assert!(
            indexed.iter().any(|p| p.ends_with("README.md")),
            "top-level markdown should be indexed: {indexed:?}"
        );
        assert!(
            indexed.iter().all(|p| !p.contains("/.bbox/")),
            ".bbox control dir must be excluded from project_file indexing: {indexed:?}"
        );
    }

    #[test]
    fn html_and_xhtml_files_are_admitted_and_claimed_by_html_chunker_not_code_chunker() {
        use std::fs;
        // `code::language_for_path` maps .html/.htm to the "html" tree-sitter
        // grammar, so `CodeChunker::claims` also matches those extensions
        // (verified live: `ts_language_for_name("html")` resolves via
        // tree-sitter-language-pack). `HtmlChunker` MUST be registered
        // before `CodeChunker` in `chunker::default_registry()` for the
        // registry's first-match `find()` (see `index_project` /
        // `resolve_current_chunk_entity` above) to route .html/.htm/.xhtml
        // through prose sectioning rather than code-symbol extraction. This
        // guards that ordering at the registry-integration level, not just
        // inside bbox-chunker's own unit tests.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("page.html"),
            "<html><body><h1>Title</h1><p>hello</p></body></html>",
        )
        .unwrap();
        fs::write(root.join("frag.xhtml"), "<p>fragment</p>").unwrap();

        let mut out = Vec::new();
        scan_project_files(root, &mut out).unwrap();
        let indexed: Vec<String> = out.iter().map(|(p, _, _)| p.clone()).collect();
        assert!(
            indexed.iter().any(|p| p.ends_with("page.html")),
            ".html must be admitted by the project-file walker: {indexed:?}"
        );
        assert!(
            indexed.iter().any(|p| p.ends_with("frag.xhtml")),
            ".xhtml must be admitted by the project-file walker: {indexed:?}"
        );

        let registry = chunker::default_registry();
        let bytes = fs::read(root.join("page.html")).unwrap();
        let sniff_len = bytes.len().min(4096);
        let claimed = registry
            .iter()
            .find(|c| c.claims(Path::new("page.html"), &bytes[..sniff_len]))
            .expect("some chunker must claim page.html");
        assert_eq!(
            claimed.format_id(),
            "html",
            "HtmlChunker must win the registry claim over CodeChunker for .html"
        );

        let (chunks, _edges) = claimed.chunk(Path::new("page.html"), &bytes).unwrap();
        assert!(
            chunks.iter().all(|chunk| chunk.chunk_kind == "web_section"),
            "expected web_section chunks, got {chunks:?}"
        );
    }
}
