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
    /// The durable project this row belongs to, stamped by the Phase 6
    /// catalog backfill (plan section 3.3, adjudication Q-E1).
    ///
    /// A TYPED top-level field rather than a `metadata` key: project ownership
    /// is authority, and burying it in the free-form string map would make it
    /// indistinguishable from the incidental annotations already living there
    /// (including the `cwd` this stamp is meant to supersede).
    ///
    /// `skip_serializing_if` keeps an unstamped edge byte-identical to what
    /// every pre-Phase-6 writer produced, so adding this field does not rewrite
    /// the corpus and does not disturb [`transcript_edge_row_identity`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
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
        // The SAME walk the stamper uses, so the two halves cannot disagree
        // about which rows exist or what they are called.
        let lane_rows = match transcript_edge_lane_rows(body) {
            Ok(lane_rows) => lane_rows,
            Err(code) => {
                return corrupt_owner_snapshot("transcript_edge", &subsource_id, code, limits);
            }
        };
        let subsource_rows = lane_rows
            .iter()
            .map(|row| {
                OwnerSnapshotRowV1::legacy_selector(
                    row.stable_row_id(&subsource_id),
                    LegacyProjectSelectorKindV1::AbsolutePath,
                    &row.cwd,
                )
            })
            .collect::<Vec<_>>();
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

/// The durable field the Phase 6 backfill stamps onto an edge row.
///
/// Declared here, beside [`Edge::project_id`] and the identity function that
/// must exclude it, so the three cannot drift apart.
pub const EDGE_PROJECT_ID_FIELD: &str = "project_id";

/// One catalog-visible row of one edge lane file.
struct TranscriptEdgeLaneRow {
    /// Index into the file's `split_inclusive('\n')` segments, so the stamper
    /// can replace exactly this line and copy every other byte through.
    segment_index: usize,
    /// The project-id-independent content hash (see
    /// [`transcript_edge_row_identity`]).
    identity: String,
    /// Which same-identity row within this lane this is, in file order.
    occurrence: usize,
    cwd: String,
}

impl TranscriptEdgeLaneRow {
    fn stable_row_id(&self, subsource_id: &str) -> String {
        format!("{subsource_id}:{}:{}", self.identity, self.occurrence)
    }
}

/// The ONE transcript-edge row identity, shared by capture and by stamping.
///
/// BINDING (plan section 3.3, adjudication Q-E1): derived from the complete
/// JSON value with `project_id` REMOVED. Hashing the raw line - which is what
/// capture used to do - would make a row's identity change the instant the
/// backfill stamped it, so a crash-retry could never recognise its own
/// already-stamped work and would re-stamp or refuse it as absent. Excluding
/// the field makes the identity invariant across the write that adds it.
///
/// The COMPLETE value is hashed, not the typed [`Edge`] projection, so a field
/// a newer binary wrote still participates in identity instead of being
/// silently dropped by a round-trip through this binary's struct.
///
/// Object keys are recursively sorted before hashing. `serde_json::Map`
/// iterates in insertion order or sorted order depending on whether the
/// `preserve_order` feature happens to be unified in from elsewhere in the
/// dependency graph; canonicalising makes every stable row id in the corpus
/// independent of that, rather than silently dependent on an unrelated crate's
/// feature selection.
pub fn transcript_edge_row_identity(line: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(line).ok()?;
    value.as_object_mut()?.remove(EDGE_PROJECT_ID_FIELD);
    let canonical = canonicalize_json_value(&value);
    let bytes = serde_json::to_vec(&canonical).ok()?;
    Some(bbox_corpus_core::project_catalog_snapshot::sha256_hex(
        &bytes,
    ))
}

/// Recursively sort object keys so a hash is key-order-independent.
fn canonicalize_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<String, serde_json::Value> = map
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize_json_value(value)))
                .collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize_json_value).collect())
        }
        other => other.clone(),
    }
}

/// Split one lane file body into its content lines, keeping each line's exact
/// terminator so a rewrite can reproduce the file byte for byte.
fn transcript_edge_segments(body: &str) -> impl Iterator<Item = (&str, &str)> {
    body.split_inclusive('\n').map(|segment| {
        let content = segment
            .strip_suffix('\n')
            .map_or(segment, |rest| rest.strip_suffix('\r').unwrap_or(rest));
        (content, &segment[content.len()..])
    })
}

/// Walk one lane file's catalog-visible rows.
///
/// The single definition of "which rows does this owner have, and what is each
/// one called". Capture builds its snapshot rows from this and the stamper
/// locates its target with it, so the read and write halves cannot drift on the
/// filter (blank lines skipped, rows without a nonempty `cwd` skipped), on the
/// identity, or on occurrence numbering.
fn transcript_edge_lane_rows(body: &str) -> Result<Vec<TranscriptEdgeLaneRow>, &'static str> {
    let mut occurrence_by_identity = BTreeMap::<String, usize>::new();
    let mut rows = Vec::new();
    for (segment_index, (content, _)) in transcript_edge_segments(body).enumerate() {
        if content.trim().is_empty() {
            continue;
        }
        // Parsed as a typed Edge purely to keep capture's existing validity
        // contract: a lane line that is not an edge is a corrupt owner, not a
        // row to skip.
        let edge: Edge = serde_json::from_str(content).map_err(|_| "transcript_edge_invalid")?;
        let Some(cwd) = edge
            .metadata
            .get("cwd")
            .map(|cwd| cwd.trim())
            .filter(|cwd| !cwd.is_empty())
        else {
            continue;
        };
        let identity = transcript_edge_row_identity(content).ok_or("transcript_edge_invalid")?;
        let occurrence = occurrence_by_identity.entry(identity.clone()).or_default();
        rows.push(TranscriptEdgeLaneRow {
            segment_index,
            identity,
            occurrence: *occurrence,
            cwd: cwd.to_string(),
        });
        *occurrence += 1;
    }
    Ok(rows)
}

/// Stamp one transcript-edge row with its durable project id.
///
/// The physical write is an atomic WHOLE-FILE replacement (plan section 3.3,
/// Q-E1): the one matching row is transformed through `serde_json::Value`, the
/// complete lane is streamed to a unique sibling temporary preserving every
/// unrelated line and every unknown field, the temporary is fsynced, atomically
/// renamed over the lane, and the parent directory fsynced. Never an in-place
/// overwrite, which would expose a torn line to a concurrent reader, and never
/// an appended superseding duplicate, which would give the lane two rows with
/// the same identity and break occurrence numbering for every later row.
///
/// Edge lane writes normally run on the reindex writer actor, which the backfill
/// is not. The plan's sanctioned alternative is taken instead: a
/// descriptor-confined source-identity recheck immediately before the
/// replacement, so a lane that changed between the read and the rename refuses
/// rather than clobbering the concurrent writer's work.
pub fn stamp_project_catalog_owner_row(
    edges_dir: &Path,
    source_row_id: &str,
    project_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerRowStampError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        OWNER_PROJECT_ID_INVALID, OWNER_ROW_ABSENT, OWNER_SOURCE_UNWRITABLE, OwnerRowStampError,
        OwnerRowStampOutcomeV1, RowStampDecisionV1, capture_stable_regular_tree_nofollow,
        stable_subsource_id, stamp_row_object,
    };

    if project_id.trim().is_empty() {
        return Err(OwnerRowStampError::new(OWNER_PROJECT_ID_INVALID));
    }
    let captures =
        capture_stable_regular_tree_nofollow(edges_dir, "transcript_edge", limits, |relative| {
            relative.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        })
        .map_err(|error| OwnerRowStampError::new(error.code))?;

    // Locate the row by rebuilding every lane's row ids through the shared
    // walk, rather than by parsing the id: subsource ids are opaque hashes and
    // splitting on ':' would be a second, weaker identity implementation.
    for (relative, captured) in captures {
        let subsource_id = stable_subsource_id("transcript_edge", &relative);
        let Some(bytes) = captured.bytes else {
            continue;
        };
        let Ok(body) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let Ok(lane_rows) = transcript_edge_lane_rows(body) else {
            continue;
        };
        let Some(row) = lane_rows
            .iter()
            .find(|row| row.stable_row_id(&subsource_id) == source_row_id)
        else {
            continue;
        };

        let segments: Vec<(&str, &str)> = transcript_edge_segments(body).collect();
        let (content, terminator) = segments[row.segment_index];
        let mut value: serde_json::Value = serde_json::from_str(content)
            .map_err(|_| OwnerRowStampError::new("transcript_edge_invalid"))?;
        // The shared three-way rule: unstamped writes, same-project elides,
        // different-project refuses. Never re-implemented here.
        if stamp_row_object(&mut value, project_id)? == RowStampDecisionV1::AlreadyStamped {
            return Ok(OwnerRowStampOutcomeV1::AlreadyStamped);
        }
        let stamped = serde_json::to_string(&value)
            .map_err(|_| OwnerRowStampError::new(OWNER_SOURCE_UNWRITABLE))?;

        let mut rewritten = Vec::with_capacity(bytes.len() + stamped.len());
        for (index, (segment_content, segment_terminator)) in segments.iter().enumerate() {
            if index == row.segment_index {
                rewritten.extend_from_slice(stamped.as_bytes());
                rewritten.extend_from_slice(terminator.as_bytes());
            } else {
                rewritten.extend_from_slice(segment_content.as_bytes());
                rewritten.extend_from_slice(segment_terminator.as_bytes());
            }
        }

        let path = edges_dir.join(&relative);
        return commit_stamped_lane(&path, &bytes, &rewritten, limits.max_source_bytes)
            .map(|()| OwnerRowStampOutcomeV1::Stamped)
            .map_err(OwnerRowStampError::new);
    }
    Err(OwnerRowStampError::new(OWNER_ROW_ABSENT))
}

/// Write `rewritten` over `path` atomically, refusing if the lane changed since
/// `expected` was read.
// The backfill stamper is an offline admin path, not a tool handler.
#[allow(clippy::disallowed_methods)]
fn commit_stamped_lane(
    path: &Path,
    expected: &[u8],
    rewritten: &[u8],
    max_bytes: usize,
) -> std::result::Result<(), &'static str> {
    use bbox_corpus_core::project_catalog_snapshot::{
        OWNER_SOURCE_MOVED, OWNER_SOURCE_UNWRITABLE, capture_regular_file_nofollow,
    };

    let parent = path.parent().ok_or(OWNER_SOURCE_UNWRITABLE)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(OWNER_SOURCE_UNWRITABLE)?;
    let tmp_path = parent.join(format!(
        "{name}.stamp.tmp.{pid}.{seq}",
        pid = std::process::id(),
        seq = writer_temp_sequence()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp_path)
        .map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
    let committed = (|| -> std::result::Result<(), &'static str> {
        file.write_all(rewritten)
            .map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
        file.sync_all().map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
        // The recheck, as late as it can be: everything above is reversible by
        // unlinking the temporary, and nothing below can observe a change.
        let current = capture_regular_file_nofollow(path, "transcript_edge", name, max_bytes);
        if current.bytes.as_deref() != Some(expected) {
            // The lane moved under us. Abandon rather than clobber the
            // concurrent writer; nothing has been committed at this point, so
            // the caller can retry against the new state.
            return Err(OWNER_SOURCE_MOVED);
        }
        fs::rename(&tmp_path, path).map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
        // Durability of the rename itself, not of the bytes: without this the
        // directory entry can be lost even though the temporary was fsynced.
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| OWNER_SOURCE_UNWRITABLE)?;
        Ok(())
    })();
    drop(file);
    if committed.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    committed
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
            // Chunker-derived edges carry no catalog authority; only the
            // Phase 6 backfill stamps a project onto an existing row.
            project_id: None,
        };
        serde_json::to_writer(&mut writer, &persisted)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

// Unique per-process sequence counter for writer temp files. Using
// create_new (O_EXCL) with pid+seq guarantees a fresh inode every time,
// so GC cannot unlink a temp the writer is actively using via a
// deterministic name (R16F2).
static WRITER_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn writer_temp_sequence() -> u64 {
    WRITER_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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

    let tmp_path = dir.join(format!(
        "{project_id}.jsonl.tmp.{pid}.{seq}",
        pid = std::process::id(),
        seq = writer_temp_sequence()
    ));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
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
            // Chunker-derived edges carry no catalog authority; only the
            // Phase 6 backfill stamps a project onto an existing row.
            project_id: None,
        };
        serde_json::to_writer(&mut writer, &persisted)?;
        writer.write_all(b"\n")?;
    }
    let file = writer.into_inner().map_err(|err| err.into_error())?;
    file.sync_all()?;
    drop(file);
    if let Err(err) = fs::rename(&tmp_path, &path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err.into());
    }
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
        project_id: edge.project_id.clone(),
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
        project_id: None,
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
            project_id: None,
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
            project_id: None,
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
            project_id: None,
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

    /// One lane holding a stampable row, an unrelated row that must survive
    /// untouched, and a row carrying a field this binary's `Edge` does not know.
    fn lane_fixture(root: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let path = root.join("tool.jsonl");
        std::fs::write(
            &path,
            // Hand-written rather than serialized so the unknown field
            // `future_field` and the key order are exactly what a NEWER binary
            // would have written.
            concat!(
                r#"{"source":{"type":"task","task_id":"one"},"kind":"RAN_BASH","target":{"type":"task","task_id":"two"},"provenance":"explicit","confidence":"exact","metadata":{"cwd":"/repo/one"},"future_field":{"written_by":"a newer binary"}}"#,
                "\n",
                r#"{"source":{"type":"task","task_id":"three"},"kind":"RELATED_TO","target":{"type":"task","task_id":"four"},"provenance":"derived","confidence":"exact"}"#,
                "\n",
                r#"{"source":{"type":"task","task_id":"five"},"kind":"RAN_BASH","target":{"type":"task","task_id":"six"},"provenance":"explicit","confidence":"exact","metadata":{"cwd":"/repo/two"}}"#,
                "\n",
            ),
        )
        .unwrap();
        path
    }

    fn only_row_id(root: &Path, cwd: &str) -> String {
        let snapshot =
            capture_project_catalog_owner_snapshot(root, OwnerSnapshotLimitsV1::default()).unwrap();
        snapshot
            .rows
            .iter()
            .find(|row| {
                matches!(&row.value, OwnerSnapshotRowValueV1::LegacyProjectSelector {
                    literal_selector, ..
                } if literal_selector == cwd)
            })
            .unwrap_or_else(|| panic!("no row for {cwd}"))
            .stable_row_id
            .clone()
    }

    /// THE Q-E1 INVARIANT. A row's identity is the same before and after it is
    /// stamped, so a crash-retry recognises its own completed work.
    ///
    /// Without this the retry would compute a different id, fail to find the
    /// row, and report `owner_row_absent` on a row it had just written.
    #[test]
    fn stamping_leaves_the_row_identity_unchanged_and_retry_sees_already_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        lane_fixture(&root);
        let row_id = only_row_id(&root, "/repo/one");

        assert_eq!(
            stamp_project_catalog_owner_row(
                &root,
                &row_id,
                "a1b2c3d4",
                OwnerSnapshotLimitsV1::default()
            )
            .unwrap(),
            bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1::Stamped
        );

        // Re-derived from the POST-stamp file: byte-identical to the id the
        // pre-stamp capture produced.
        assert_eq!(only_row_id(&root, "/repo/one"), row_id);

        // The crash-retry: the exact same call the torn backfill would repeat.
        assert_eq!(
            stamp_project_catalog_owner_row(
                &root,
                &row_id,
                "a1b2c3d4",
                OwnerSnapshotLimitsV1::default()
            )
            .unwrap(),
            bbox_corpus_core::project_catalog_snapshot::OwnerRowStampOutcomeV1::AlreadyStamped
        );
    }

    /// The rewrite is a whole-file replacement that preserves every unrelated
    /// line and every field this binary does not know about, and it NEVER
    /// appends a superseding duplicate.
    #[test]
    fn stamping_preserves_unrelated_lines_and_unknown_fields_without_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        let path = lane_fixture(&root);
        let before = std::fs::read_to_string(&path).unwrap();
        let row_id = only_row_id(&root, "/repo/one");

        stamp_project_catalog_owner_row(
            &root,
            &row_id,
            "a1b2c3d4",
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        let before_lines: Vec<&str> = before.lines().collect();
        let after_lines: Vec<&str> = after.lines().collect();
        // No append-duplicate: same line count, same order.
        assert_eq!(after_lines.len(), before_lines.len());
        // The two rows this stamp did not name are byte-identical.
        assert_eq!(after_lines[1], before_lines[1]);
        assert_eq!(after_lines[2], before_lines[2]);

        let stamped: serde_json::Value = serde_json::from_str(after_lines[0]).unwrap();
        assert_eq!(stamped["project_id"], "a1b2c3d4");
        // The field this binary's Edge struct has never heard of survived the
        // Value round-trip rather than being dropped.
        assert_eq!(stamped["future_field"]["written_by"], "a newer binary");
        assert_eq!(stamped["metadata"]["cwd"], "/repo/one");
    }

    /// A row already bound to a DIFFERENT project is refused, not overwritten,
    /// and the lane is left exactly as it was.
    #[test]
    fn stamping_a_conflicting_row_refuses_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        let path = lane_fixture(&root);
        let row_id = only_row_id(&root, "/repo/one");
        stamp_project_catalog_owner_row(
            &root,
            &row_id,
            "a1b2c3d4",
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap();
        let after_first = std::fs::read(&path).unwrap();

        let error = stamp_project_catalog_owner_row(
            &root,
            &row_id,
            "99999999",
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap_err();

        assert_eq!(
            error.code,
            bbox_corpus_core::project_catalog_snapshot::OWNER_ROW_PROJECT_ID_CONFLICT
        );
        assert_eq!(std::fs::read(&path).unwrap(), after_first);
    }

    /// A row id no lane produces is a typed absence, and nothing is written.
    #[test]
    fn stamping_an_unknown_row_refuses_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        let path = lane_fixture(&root);
        let before = std::fs::read(&path).unwrap();

        let error = stamp_project_catalog_owner_row(
            &root,
            "transcript_edge:nope:deadbeef:0",
            "a1b2c3d4",
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap_err();

        assert_eq!(
            error.code,
            bbox_corpus_core::project_catalog_snapshot::OWNER_ROW_ABSENT
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// A lane that MOVED between the stamper's read and its replacement is its
    /// own diagnostic, distinct from a row that was never there.
    ///
    /// The two demand opposite operator responses - re-run preflight against
    /// the moved state, versus investigate an artifact naming a row the store
    /// does not have - so collapsing them onto one token would lose the only
    /// information that distinguishes them. Both are staleness at the backfill
    /// level; only the diagnostic tells them apart.
    #[test]
    fn a_lane_that_moves_mid_stamp_is_distinct_from_an_absent_row() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        let path = lane_fixture(&root);
        let row_id = only_row_id(&root, "/repo/one");

        // The absent case, for contrast on the same lane.
        let absent = stamp_project_catalog_owner_row(
            &root,
            "transcript_edge:nope:deadbeef:0",
            "a1b2c3d4",
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap_err();
        assert_eq!(
            absent.code,
            bbox_corpus_core::project_catalog_snapshot::OWNER_ROW_ABSENT
        );

        // The moved case: commit_stamped_lane rechecks the source against the
        // bytes the locate step read, so bytes that no longer match abandon
        // the write. Driven directly because racing a real concurrent writer
        // would make the test timing-dependent.
        let stale = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"{\"moved\":true}\n").unwrap();
        let moved =
            commit_stamped_lane(&path, &stale, b"irrelevant", 16 * 1024 * 1024).unwrap_err();

        assert_eq!(
            moved,
            bbox_corpus_core::project_catalog_snapshot::OWNER_SOURCE_MOVED
        );
        assert_ne!(
            moved,
            bbox_corpus_core::project_catalog_snapshot::OWNER_ROW_ABSENT
        );
        // Abandoned, not clobbered: the concurrent writer's bytes survive.
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"moved\":true}\n");
        let _ = row_id;
    }

    /// Identity ignores `project_id` and ignores key ORDER, but not content.
    /// The key-order half is what makes every stable row id in the corpus
    /// independent of whether `serde_json`'s `preserve_order` feature happens to
    /// be unified in from elsewhere in the dependency graph.
    #[test]
    fn row_identity_excludes_project_id_and_key_order_but_not_content() {
        let plain = r#"{"source":"task:one","kind":"K","target":"task:two"}"#;
        let stamped =
            r#"{"source":"task:one","kind":"K","target":"task:two","project_id":"a1b2c3d4"}"#;
        let reordered = r#"{"target":"task:two","kind":"K","source":"task:one"}"#;
        let different = r#"{"source":"task:one","kind":"OTHER","target":"task:two"}"#;

        let identity = transcript_edge_row_identity(plain).unwrap();
        assert_eq!(transcript_edge_row_identity(stamped).unwrap(), identity);
        assert_eq!(transcript_edge_row_identity(reordered).unwrap(), identity);
        assert_ne!(transcript_edge_row_identity(different).unwrap(), identity);
    }

    /// Two rows identical except for their project stamp still get distinct
    /// ids, and stamping the unstamped one does not renumber the other.
    #[test]
    fn same_identity_rows_keep_distinct_occurrences_across_a_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("edges");
        std::fs::create_dir_all(&root).unwrap();
        let row = r#"{"source":{"type":"task","task_id":"one"},"kind":"RAN_BASH","target":{"type":"task","task_id":"two"},"provenance":"explicit","confidence":"exact","metadata":{"cwd":"/repo/one"}}"#;
        std::fs::write(root.join("tool.jsonl"), format!("{row}\n{row}\n")).unwrap();

        let snapshot =
            capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
                .unwrap();
        assert_eq!(snapshot.row_count, 2);
        let first = snapshot.rows[0].stable_row_id.clone();
        let second = snapshot.rows[1].stable_row_id.clone();
        assert_ne!(first, second);
        assert!(first.ends_with(":0") && second.ends_with(":1"));

        stamp_project_catalog_owner_row(
            &root,
            &first,
            "a1b2c3d4",
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap();

        // Both ids still resolve after the stamp: occurrence numbering did not
        // shift under the row that was not touched.
        let after = capture_project_catalog_owner_snapshot(&root, OwnerSnapshotLimitsV1::default())
            .unwrap();
        let ids: Vec<&str> = after
            .rows
            .iter()
            .map(|row| row.stable_row_id.as_str())
            .collect();
        assert!(ids.contains(&first.as_str()) && ids.contains(&second.as_str()));
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
