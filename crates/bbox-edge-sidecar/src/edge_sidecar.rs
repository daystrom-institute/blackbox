//! Edge sidecar persistence layer: the on-disk JSONL edge lanes
//! (observed / explicit / managed-derived), dir layout helpers, dedup
//! append/replace/merge/purge primitives, and legacy-sidecar compaction.
//! Store-agnostic by design — the store->edge emitters live in
//! `edge_index`.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::EntityRef;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    pub source: EntityRef,
    pub kind: String,
    pub target: EntityRef,
    pub provenance: EdgeProvenance,
    pub confidence: EdgeConfidence,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EdgeKey {
    source: EntityRef,
    kind: String,
    target: EntityRef,
    provenance: EdgeProvenance,
    confidence: EdgeConfidence,
}

/// Capture edge rows whose metadata retains a literal execution directory.
///
/// Every JSONL file is read by an exact no-follow file descriptor and the
/// complete tree is accepted only after two identical scans. This makes the
/// read-only capture coherent with atomic lane replacement without creating
/// an edge store or coordination file.
pub fn capture_project_catalog_owner_snapshot(
    edges_dir: &Path,
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

    match std::fs::symlink_metadata(edges_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return missing_owner_snapshot("transcript_edge", "transcript_edge:root", limits);
        }
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => {
            return corrupt_owner_snapshot(
                "transcript_edge",
                "transcript_edge:root",
                "owner_tree_unsafe",
                limits,
            );
        }
    }
    let captures = match capture_stable_regular_tree_nofollow(
        edges_dir,
        "transcript_edge",
        limits,
        |relative| {
            relative
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("jsonl")
        },
    ) {
        Ok(captures) => captures,
        Err(error) => {
            return corrupt_owner_snapshot(
                "transcript_edge",
                "transcript_edge:root",
                error.code,
                limits,
            );
        }
    };
    if captures.is_empty() {
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(b""),
            byte_len: 0,
        };
        return build_owner_snapshot(
            "transcript_edge",
            vec![owner_subsource("transcript_edge:root", state, &[])],
            Vec::new(),
            limits,
        );
    }
    let mut rows = Vec::new();
    let mut subsources = Vec::new();
    for (relative, captured) in captures {
        let subsource_id = stable_subsource_id("transcript_edge", &relative);
        let Some(bytes) = captured.bytes else {
            return corrupt_owner_snapshot(
                "transcript_edge",
                &subsource_id,
                "owner_source_unreadable",
                limits,
            );
        };
        let body = match std::str::from_utf8(&bytes) {
            Ok(body) => body,
            Err(_) => {
                return corrupt_owner_snapshot(
                    "transcript_edge",
                    &subsource_id,
                    "transcript_edge_invalid",
                    limits,
                );
            }
        };
        let mut occurrence_by_hash = BTreeMap::<String, usize>::new();
        let mut subsource_rows = Vec::new();
        for line in body.lines().filter(|line| !line.trim().is_empty()) {
            let edge: Edge = match serde_json::from_str(line) {
                Ok(edge) => edge,
                Err(_) => {
                    return corrupt_owner_snapshot(
                        "transcript_edge",
                        &subsource_id,
                        "transcript_edge_invalid",
                        limits,
                    );
                }
            };
            let Some(cwd) = edge
                .metadata
                .get("cwd")
                .map(|cwd| cwd.trim())
                .filter(|cwd| !cwd.is_empty())
            else {
                continue;
            };
            let row_hash = sha256_hex(line.as_bytes());
            let occurrence = occurrence_by_hash.entry(row_hash.clone()).or_default();
            let stable_row_id = format!("{subsource_id}:{row_hash}:{}", *occurrence);
            *occurrence += 1;
            subsource_rows.push(OwnerSnapshotRowV1::legacy_selector(
                stable_row_id,
                LegacyProjectSelectorKindV1::AbsolutePath,
                cwd,
            ));
        }
        subsources.push(owner_subsource(
            subsource_id,
            captured.state,
            &subsource_rows,
        ));
        rows.extend(subsource_rows);
    }
    finalize_owner_snapshot(
        "transcript_edge",
        "transcript_edge:root",
        subsources,
        rows,
        limits,
    )
}

impl Edge {
    pub fn dedup_key(&self) -> EdgeKey {
        EdgeKey {
            source: self.source.clone(),
            kind: self.kind.clone(),
            target: self.target.clone(),
            provenance: self.provenance,
            confidence: self.confidence,
        }
    }
}

pub fn count_materialized_jsonl_files(edges_dir: &Path) -> usize {
    let mat_dir = crate::manifest::materialized_dir(edges_dir);
    if !mat_dir.is_dir() {
        return 0;
    }
    fn count_jsonl_recursive(dir: &Path) -> usize {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    count += count_jsonl_recursive(&path);
                } else if path.extension().is_some_and(|e| e == "jsonl") {
                    count += 1;
                }
            }
        }
        count
    }
    count_jsonl_recursive(&mat_dir)
}

pub fn scan_lane_project_ids(lane_dir: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let Ok(entries) = fs::read_dir(lane_dir) else {
        return ids;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(stem) = sidecar_file_stem(&path) {
            ids.insert(stem.to_string());
        }
    }
    ids
}

pub fn sidecar_project_id_is_registered(
    project_id: &str,
    registered: Option<&HashSet<String>>,
) -> bool {
    let Some(registered) = registered else {
        return true;
    };
    registered.contains(project_id)
}

pub fn edges_dir_from_bro_store(store_dir: &Path) -> PathBuf {
    store_dir
        .parent()
        .map(|parent| parent.join("edges"))
        .unwrap_or_else(|| store_dir.join("edges"))
}

pub fn edges_dir_from_projects_path(projects_path: &Path) -> PathBuf {
    projects_path
        .parent()
        .map(|parent| parent.join("edges"))
        .unwrap_or_else(|| PathBuf::from("edges"))
}

pub fn managed_derived_edges_dir(edges_dir: &Path) -> PathBuf {
    edges_dir.join("derived")
}

pub fn sidecar_file_stem(path: &Path) -> Option<&str> {
    path.file_stem().and_then(|s| s.to_str())
}

pub fn scan_managed_derived_project_ids(managed_dir: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let Ok(namespace_entries) = fs::read_dir(managed_dir) else {
        return ids;
    };
    for ns_entry in namespace_entries.filter_map(Result::ok) {
        let ns_path = ns_entry.path();
        if !ns_path.is_dir() {
            continue;
        }
        let Ok(project_entries) = fs::read_dir(&ns_path) else {
            continue;
        };
        for proj_entry in project_entries.filter_map(Result::ok) {
            let proj_path = proj_entry.path();
            if proj_path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = sidecar_file_stem(&proj_path) {
                ids.insert(stem.to_string());
            }
        }
    }
    ids
}

pub fn sidecar_project_is_registered(
    path: &Path,
    registered_project_ids: Option<&HashSet<String>>,
) -> bool {
    let Some(registered_project_ids) = registered_project_ids else {
        return true;
    };
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    if matches!(stem, "agents") {
        return true;
    }
    registered_project_ids.contains(stem)
}

/// Test-fixture helper: append raw chunker edges to a project's JSONL lane.
/// Deliberately un-gated (no `#[cfg(test)]`) so consumer-crate tests can use
/// it — `cfg(test)` does not cross crate boundaries.
pub fn append_project_edges(
    edges_dir: &Path,
    project_id: &str,
    edges: &[bbox_chunker::Edge],
) -> Result<()> {
    if edges.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(edges_dir)?;
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut writer = BufWriter::new(file);
    for edge in edges {
        let persisted = Edge {
            source: edge.source.clone(),
            kind: edge.kind.clone(),
            target: edge.target.clone(),
            provenance: edge.provenance,
            confidence: edge.confidence,
            metadata: BTreeMap::new(),
        };
        serde_json::to_writer(&mut writer, &persisted)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

// edge sidecar writes run on the reindex/writer-actor thread.
#[allow(clippy::disallowed_methods)]
pub fn replace_project_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    edges: &[bbox_chunker::Edge],
) -> Result<()> {
    let dir = managed_derived_edges_dir(edges_dir).join(namespace);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{project_id}.jsonl"));
    if edges.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        return Ok(());
    }

    let tmp_path = path.with_extension("jsonl.tmp");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)?;
    // Buffered: one syscall per ~8KiB instead of one per serialized fragment
    // (the unbuffered loop dominated reindex project phases; thread-935b467d).
    let mut writer = BufWriter::new(file);
    for edge in edges {
        let persisted = Edge {
            source: edge.source.clone(),
            kind: edge.kind.clone(),
            target: edge.target.clone(),
            provenance: edge.provenance,
            confidence: edge.confidence,
            metadata: BTreeMap::new(),
        };
        serde_json::to_writer(&mut writer, &persisted)?;
        writer.write_all(b"\n")?;
    }
    let file = writer.into_inner().map_err(|err| err.into_error())?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp_path, path)?;
    Ok(())
}

pub fn append_edges(edges_dir: &Path, project_id: &str, edges: &[Edge]) -> Result<()> {
    if edges.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(edges_dir)?;
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut writer = BufWriter::new(file);
    for edge in edges {
        serde_json::to_writer(&mut writer, edge)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

pub fn append_edges_dedup(edges_dir: &Path, project_id: &str, edges: &[Edge]) -> Result<usize> {
    if edges.is_empty() {
        return Ok(0);
    }
    fs::create_dir_all(edges_dir)?;
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let mut seen = HashSet::new();
    if let Ok(file) = fs::File::open(&path) {
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(edge) = serde_json::from_str::<Edge>(&line) {
                seen.insert(edge_import_key(&edge));
            }
        }
    }
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut writer = BufWriter::new(file);
    let mut written = 0usize;
    for edge in edges {
        if !seen.insert(edge_import_key(edge)) {
            continue;
        }
        serde_json::to_writer(&mut writer, edge)?;
        writer.write_all(b"\n")?;
        written += 1;
    }
    writer.flush()?;
    Ok(written)
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeSidecarCompactionStats {
    pub project_id: String,
    pub applied: bool,
    pub existed: bool,
    pub legacy_path: String,
    pub backup_path: Option<String>,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub lines_seen: u64,
    pub retained_lines: u64,
    pub derived_edges_removed: u64,
    pub explicit_edges_retained: u64,
    pub malformed_lines_retained: u64,
    pub blank_lines_dropped: u64,
}

// invoked from bbox_edge_compact's run_blocking closure.
#[allow(clippy::disallowed_methods)]
pub fn compact_legacy_sidecar(
    edges_dir: &Path,
    project_id: &str,
    apply: bool,
) -> Result<EdgeSidecarCompactionStats> {
    let project_id = bbox_corpus_core::project_catalog::ProjectId::parse(project_id.to_owned())
        .context("validating edge sidecar project id")?;
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let mut stats = EdgeSidecarCompactionStats {
        project_id: project_id.to_string(),
        applied: false,
        existed: path.exists(),
        legacy_path: path.display().to_string(),
        backup_path: None,
        bytes_before: 0,
        bytes_after: 0,
        lines_seen: 0,
        retained_lines: 0,
        derived_edges_removed: 0,
        explicit_edges_retained: 0,
        malformed_lines_retained: 0,
        blank_lines_dropped: 0,
    };
    if !path.exists() {
        return Ok(stats);
    }

    stats.bytes_before = fs::metadata(&path)?.len();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let tmp_path = path.with_file_name(format!(
        "{project_id}.jsonl.compact-{stamp}-{}.tmp",
        std::process::id()
    ));
    let mut writer = if apply {
        Some(BufWriter::new(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp_path)?,
        ))
    } else {
        None
    };

    let file = fs::File::open(&path)?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        stats.lines_seen += 1;
        if line.trim().is_empty() {
            stats.blank_lines_dropped += 1;
            continue;
        }
        match serde_json::from_str::<Edge>(&line) {
            Ok(edge) if edge.provenance == EdgeProvenance::Derived => {
                stats.derived_edges_removed += 1;
            }
            Ok(_) => {
                stats.explicit_edges_retained += 1;
                stats.retained_lines += 1;
                stats.bytes_after += line.len() as u64 + 1;
                if let Some(writer) = writer.as_mut() {
                    writer.write_all(line.as_bytes())?;
                    writer.write_all(b"\n")?;
                }
            }
            Err(_) => {
                stats.malformed_lines_retained += 1;
                stats.retained_lines += 1;
                stats.bytes_after += line.len() as u64 + 1;
                if let Some(writer) = writer.as_mut() {
                    writer.write_all(line.as_bytes())?;
                    writer.write_all(b"\n")?;
                }
            }
        }
    }

    if !apply || stats.derived_edges_removed == 0 && stats.blank_lines_dropped == 0 {
        if let Some(mut writer) = writer {
            writer.flush()?;
            drop(writer);
            let _ = fs::remove_file(&tmp_path);
        }
        return Ok(stats);
    }

    let backup_path = path.with_file_name(format!("{project_id}.jsonl.bak-{stamp}"));
    if let Some(mut writer) = writer {
        writer.flush()?;
        writer.get_ref().sync_all()?;
    } else {
        anyhow::bail!("internal error: compaction apply requested without writer");
    }
    fs::rename(&path, &backup_path)?;
    match fs::rename(&tmp_path, &path) {
        Ok(()) => {}
        Err(err) => {
            let _ = fs::rename(&backup_path, &path);
            let _ = fs::remove_file(&tmp_path);
            return Err(err.into());
        }
    }
    stats.applied = true;
    stats.backup_path = Some(backup_path.display().to_string());
    Ok(stats)
}

pub fn edge_import_key(edge: &Edge) -> String {
    let mut hasher = Sha256::new();
    hasher.update(edge.source.to_string());
    hasher.update(b"\0");
    hasher.update(&edge.kind);
    hasher.update(b"\0");
    hasher.update(edge.target.to_string());
    hasher.update(b"\0");
    if let Some(commit) = edge.metadata.get("anchor.commit_sha_at_edit") {
        hasher.update(commit);
    }
    hex::encode(hasher.finalize())
}

pub fn derived_tool_projection(edge: &Edge) -> Option<Edge> {
    if edge.kind != "EDITED_FILE" {
        return None;
    }
    let EntityRef::Transcript {
        provider,
        session_id,
        ..
    } = &edge.source
    else {
        return None;
    };
    Some(Edge {
        source: edge.target.clone(),
        kind: "EDITED_BY_SESSION".to_string(),
        target: EntityRef::Session {
            provider: provider.clone(),
            session_id: session_id.clone(),
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
        metadata: edge.metadata.clone(),
    })
}

pub fn exact_edge(
    source: EntityRef,
    kind: &str,
    target: EntityRef,
    provenance: EdgeProvenance,
) -> Edge {
    Edge {
        source,
        kind: kind.to_string(),
        target,
        provenance,
        confidence: EdgeConfidence::Exact,
        metadata: BTreeMap::new(),
    }
}

pub fn line_provenance_is_derived(line: &str) -> bool {
    let Some(pos) = line.find("\"provenance\"") else {
        return false;
    };
    let rest = &line[pos + "\"provenance\"".len()..];
    let rest = rest.trim_start();
    if !rest.starts_with(':') {
        return false;
    }
    let after_colon = rest[1..].trim_start();
    after_colon.starts_with("\"derived\"")
}

// ---------------------------------------------------------------------------
// Phase 2: Lifecycle-specific write APIs
// ---------------------------------------------------------------------------
//
// Caller audit (recorded here so it does not rot):
//
//   materialized  = computed current workspace/repo view (Derived provenance)
//   observed      = event/provenance history, usually Tool provenance (Explicit)
//   explicit      = user/agent-authored durable fact (Explicit)
//   global        = non-project graph support (Explicit)
//
// append_project_edges callers (legacy append path):
//   (none — all production callers moved to lifecycle APIs)
//
// append_edges callers (full Edge with metadata):
//   (none — all production callers moved to lifecycle APIs)
//
// append_edges_dedup callers:
//   (none — all production callers moved to lifecycle APIs)
//
// replace_project_edges callers (managed derived replacement):
//   (none directly — wrapped by lifecycle APIs below)
//
// Lifecycle API routing:
//   project_files.rs  → replace_materialized_edges_incremental ("project")
//   git_history.rs    → replace_materialized_edges (full) or merge_materialized_edges (incremental) ("git")
//   tool_edges.rs     → append_observed_edges
//   provenance.rs     → append_explicit_edges
//   routes.rs         → append_explicit_edges (global agents.jsonl)
//   workflow/ops.rs   → append_explicit_edges
// ---------------------------------------------------------------------------

pub fn append_explicit_edges(edges_dir: &Path, project_id: &str, edges: &[Edge]) -> Result<usize> {
    for e in edges {
        debug_assert!(
            e.provenance != EdgeProvenance::Derived,
            "append_explicit_edges: rejected Derived edge kind={} source={:?}",
            e.kind,
            e.source,
        );
    }
    append_edges_dedup(edges_dir, project_id, edges)
}

pub fn append_observed_edges(edges_dir: &Path, project_id: &str, edges: &[Edge]) -> Result<()> {
    for e in edges {
        debug_assert!(
            e.provenance != EdgeProvenance::Derived,
            "append_observed_edges: rejected Derived edge kind={} source={:?}",
            e.kind,
            e.source,
        );
    }
    append_edges(edges_dir, project_id, edges)
}

pub fn replace_materialized_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    edges: &[bbox_chunker::Edge],
) -> Result<()> {
    for e in edges {
        debug_assert!(
            e.provenance == EdgeProvenance::Derived,
            "replace_materialized_edges: rejected non-Derived edge kind={} provenance={:?}",
            e.kind,
            e.provenance,
        );
    }
    replace_project_edges(edges_dir, namespace, project_id, edges)
}

pub fn read_managed_derived_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
) -> Result<Vec<Edge>> {
    let path = managed_derived_edges_dir(edges_dir)
        .join(namespace)
        .join(format!("{project_id}.jsonl"));
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let reader = std::io::BufReader::new(file);
    let mut edges = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(edge) = serde_json::from_str::<Edge>(trimmed) {
            edges.push(edge);
        }
    }
    Ok(edges)
}

pub fn rel_path_hashes_of(edges: &[bbox_chunker::Edge]) -> HashSet<String> {
    let mut hashes = HashSet::new();
    for e in edges {
        if let EntityRef::ProjectFile { rel_path_hash, .. }
        | EntityRef::ProjectFileV2 { rel_path_hash, .. } = &e.source
        {
            hashes.insert(rel_path_hash.clone());
        }
        if let EntityRef::ProjectFile { rel_path_hash, .. }
        | EntityRef::ProjectFileV2 { rel_path_hash, .. } = &e.target
        {
            hashes.insert(rel_path_hash.clone());
        }
    }
    hashes
}

pub fn edge_touches_any_path_hash(edge: &Edge, stale_hashes: &HashSet<String>) -> bool {
    match (&edge.source, &edge.target) {
        (EntityRef::ProjectFile { rel_path_hash, .. }, _)
        | (EntityRef::ProjectFileV2 { rel_path_hash, .. }, _)
        | (_, EntityRef::ProjectFile { rel_path_hash, .. }) => stale_hashes.contains(rel_path_hash),
        (_, EntityRef::ProjectFileV2 { rel_path_hash, .. }) => stale_hashes.contains(rel_path_hash),
        _ => false,
    }
}

pub fn replace_materialized_edges_incremental(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    new_edges: &[bbox_chunker::Edge],
) -> Result<()> {
    for e in new_edges {
        debug_assert!(
            e.provenance == EdgeProvenance::Derived,
            "replace_materialized_edges_incremental: rejected non-Derived edge kind={} provenance={:?}",
            e.kind,
            e.provenance,
        );
    }
    if new_edges.is_empty() {
        return Ok(());
    }
    let stale_hashes = rel_path_hashes_of(new_edges);
    let existing = read_managed_derived_edges(edges_dir, namespace, project_id)?;
    let preserved: Vec<bbox_chunker::Edge> = existing
        .into_iter()
        .filter(|e| !edge_touches_any_path_hash(e, &stale_hashes))
        .map(|e| bbox_chunker::Edge {
            source: e.source,
            kind: e.kind,
            target: e.target,
            provenance: e.provenance,
            confidence: e.confidence,
        })
        .collect();
    let mut merged = preserved;
    merged.extend_from_slice(new_edges);
    replace_project_edges(edges_dir, namespace, project_id, &merged)
}

/// Drop managed derived edges whose source or target is a project file in
/// `stale_hashes` (rel_path_hash). Used to purge a deleted file's file-anchored
/// edges, which the mtime/size incremental path never revisits once the file is
/// gone from disk. Returns the number of edges removed. Granularity matches
/// `edge_touches_any_path_hash` (the incremental-replace key), so symbol→symbol
/// edges carrying no project-file ref are not removed here.
pub fn purge_managed_edges_for_path_hashes(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    stale_hashes: &HashSet<String>,
) -> Result<usize> {
    if stale_hashes.is_empty() {
        return Ok(0);
    }
    let existing = read_managed_derived_edges(edges_dir, namespace, project_id)?;
    let before = existing.len();
    let retained: Vec<bbox_chunker::Edge> = existing
        .into_iter()
        .filter(|e| !edge_touches_any_path_hash(e, stale_hashes))
        .map(|e| bbox_chunker::Edge {
            source: e.source,
            kind: e.kind,
            target: e.target,
            provenance: e.provenance,
            confidence: e.confidence,
        })
        .collect();
    let purged = before.saturating_sub(retained.len());
    if purged > 0 {
        replace_project_edges(edges_dir, namespace, project_id, &retained)?;
    }
    Ok(purged)
}

pub fn merge_materialized_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    new_edges: &[bbox_chunker::Edge],
) -> Result<()> {
    for e in new_edges {
        debug_assert!(
            e.provenance == EdgeProvenance::Derived,
            "merge_materialized_edges: rejected non-Derived edge kind={} provenance={:?}",
            e.kind,
            e.provenance,
        );
    }
    if new_edges.is_empty() {
        return Ok(());
    }
    let existing = read_managed_derived_edges(edges_dir, namespace, project_id)?;
    let mut seen: HashSet<String> = existing.iter().map(edge_import_key).collect();
    let mut merged: Vec<bbox_chunker::Edge> = existing
        .into_iter()
        .map(|e| bbox_chunker::Edge {
            source: e.source,
            kind: e.kind,
            target: e.target,
            provenance: e.provenance,
            confidence: e.confidence,
        })
        .collect();
    for e in new_edges {
        let key = edge_import_key(&Edge {
            source: e.source.clone(),
            kind: e.kind.clone(),
            target: e.target.clone(),
            provenance: e.provenance,
            confidence: e.confidence,
            metadata: BTreeMap::new(),
        });
        if seen.insert(key) {
            merged.push(e.clone());
        }
    }
    replace_project_edges(edges_dir, namespace, project_id, &merged)
}

// ---------------------------------------------------------------------------
// Phase 2: Legacy edge extraction dry-run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyExtractionPlan {
    pub project_id: String,
    pub legacy_path: String,
    pub total_lines: u64,
    pub derived_lines: u64,
    pub tool_lines: u64,
    pub explicit_lines: u64,
    pub malformed_lines: u64,
    pub blank_lines: u64,
    pub managed_replacement_exists: bool,
    pub extractable: bool,
}

pub fn plan_legacy_edge_extraction(
    edges_dir: &Path,
    project_id: &str,
) -> Result<LegacyExtractionPlan> {
    let legacy_path = edges_dir.join(format!("{project_id}.jsonl"));
    let mut plan = LegacyExtractionPlan {
        project_id: project_id.to_string(),
        legacy_path: legacy_path.display().to_string(),
        ..Default::default()
    };

    let managed = managed_derived_edges_dir(edges_dir);
    plan.managed_replacement_exists = managed
        .join("project")
        .join(format!("{project_id}.jsonl"))
        .exists()
        || managed
            .join("git")
            .join(format!("{project_id}.jsonl"))
            .exists();

    let file = match fs::File::open(&legacy_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(plan),
        Err(e) => return Err(e.into()),
    };

    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        plan.total_lines += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            plan.blank_lines += 1;
            continue;
        }
        match serde_json::from_str::<Edge>(trimmed) {
            Ok(edge) => match edge.provenance {
                EdgeProvenance::Derived => plan.derived_lines += 1,
                EdgeProvenance::Explicit => {
                    let is_tool = edge.kind == "READ_FILE"
                        || edge.kind == "EDITED_FILE"
                        || edge.kind == "RAN_BASH";
                    if is_tool {
                        plan.tool_lines += 1;
                    } else {
                        plan.explicit_lines += 1;
                    }
                }
                EdgeProvenance::Implicit => plan.explicit_lines += 1,
            },
            Err(_) => plan.malformed_lines += 1,
        }
    }

    plan.extractable = plan.managed_replacement_exists && plan.derived_lines > 0;
    Ok(plan)
}

#[cfg(test)]
mod project_catalog_snapshot_tests {
    use super::*;
    use bbox_corpus_core::project_catalog_snapshot::{
        OwnerSnapshotLimitsV1, OwnerSnapshotRowValueV1, OwnerSnapshotStateV1,
    };

    #[test]
    fn migration_snapshot_is_no_create_and_captures_only_literal_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        let missing =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert!(matches!(
            missing.state,
            OwnerSnapshotStateV1::Missing { .. }
        ));
        assert!(!root.exists());

        std::fs::create_dir(&root).unwrap();
        let mut with_cwd = Edge {
            source: EntityRef::parse("task:one").unwrap(),
            kind: "RAN_BASH".into(),
            target: EntityRef::parse("task:two").unwrap(),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
        };
        with_cwd
            .metadata
            .insert("cwd".into(), "/repo/worktree".into());
        let without_cwd = Edge {
            source: EntityRef::parse("task:two").unwrap(),
            kind: "RELATED_TO".into(),
            target: EntityRef::parse("task:one").unwrap(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
        };
        std::fs::write(
            root.join("tool.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&with_cwd).unwrap(),
                serde_json::to_string(&without_cwd).unwrap()
            ),
        )
        .unwrap();

        let snapshot =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert_eq!(snapshot.row_count, 1);
        assert!(matches!(
            &snapshot.rows[0].value,
            OwnerSnapshotRowValueV1::LegacyProjectSelector {
                literal_selector,
                ..
            } if literal_selector == "/repo/worktree"
        ));
    }

    #[test]
    fn legacy_compaction_rejects_project_id_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let edges = root.join("edges");
        std::fs::create_dir(&edges).unwrap();
        let outside = root.join("escape.jsonl");
        std::fs::write(&outside, b"sentinel\n").unwrap();

        let error = compact_legacy_sidecar(&edges, "../escape", true)
            .unwrap_err()
            .to_string();

        assert!(error.contains("validating edge sidecar project id"));
        assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel\n");
    }
}
