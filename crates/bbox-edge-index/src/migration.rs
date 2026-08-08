use std::cmp::Reverse;
#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BinaryHeap;
use std::fs;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::edge_index::Edge;
use bbox_chunker::EdgeProvenance;

const MIGRATION_VERSION: u32 = 1;
const SORT_CHUNK_BYTES: usize = 8 * 1024 * 1024;
const MERGE_FAN_IN: usize = 32;

pub fn explicit_lane_path(edges_dir: &Path, project_id: &str) -> PathBuf {
    edges_dir
        .join("explicit")
        .join(format!("{project_id}.jsonl"))
}

pub fn observed_lane_path(edges_dir: &Path, project_id: &str) -> PathBuf {
    edges_dir
        .join("observed")
        .join(format!("{project_id}.jsonl"))
}

pub fn quarantine_dir(edges_dir: &Path, project_id: &str) -> PathBuf {
    edges_dir.join("quarantine").join(project_id)
}

pub fn migrations_dir(edges_dir: &Path) -> PathBuf {
    edges_dir.join("migrations")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MigrationManifest {
    pub version: u32,
    pub migration_id: String,
    pub project_id: String,
    pub source_path: String,
    pub source_hash: String,
    pub status: MigrationStatus,
    pub explicit_count: u64,
    pub observed_count: u64,
    pub derived_dropped: u64,
    pub quarantined_count: u64,
    pub backup_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStatus {
    Pending,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineLine {
    pub source_path: String,
    pub line_number: u64,
    pub raw: String,
    pub error: String,
}

#[cfg(test)]
#[derive(Debug, Default)]
pub struct ExtractionResult {
    pub explicit_edges: Vec<Edge>,
    pub observed_edges: Vec<Edge>,
    pub quarantine: Vec<QuarantineLine>,
    pub derived_dropped: u64,
    pub total_lines: u64,
}

#[cfg(test)]
pub fn extract_legacy_sidecar(edges_dir: &Path, project_id: &str) -> Result<ExtractionResult> {
    let legacy_path = edges_dir.join(format!("{project_id}.jsonl"));
    if !legacy_path.exists() {
        return Ok(ExtractionResult::default());
    }

    let has_replacement = has_managed_replacement(edges_dir, project_id);

    let mut result = ExtractionResult::default();
    let file = fs::File::open(&legacy_path)?;
    let reader = std::io::BufReader::new(file);

    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        result.total_lines += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Edge>(trimmed) {
            Ok(edge) => match edge.provenance {
                EdgeProvenance::Derived => {
                    if has_replacement {
                        result.derived_dropped += 1;
                    } else {
                        result.explicit_edges.push(edge);
                    }
                }
                EdgeProvenance::Explicit => {
                    let is_tool = edge.kind == "READ_FILE"
                        || edge.kind == "EDITED_FILE"
                        || edge.kind == "RAN_BASH";
                    if is_tool {
                        result.observed_edges.push(edge);
                    } else {
                        result.explicit_edges.push(edge);
                    }
                }
                EdgeProvenance::Implicit => {
                    result.explicit_edges.push(edge);
                }
            },
            Err(err) => {
                result.quarantine.push(QuarantineLine {
                    source_path: legacy_path.display().to_string(),
                    line_number: idx as u64 + 1,
                    raw: trimmed.to_string(),
                    error: err.to_string(),
                });
            }
        }
    }
    Ok(result)
}

#[derive(Debug, Default)]
struct StagedExtraction {
    source_hash: String,
    explicit_count: u64,
    observed_count: u64,
    derived_dropped: u64,
    quarantined_count: u64,
}

/// Stream one unbounded legacy lane into bounded staging files. The source
/// digest is folded from the exact bytes consumed so the caller can reject a
/// lane that changed between planning and extraction.
fn stage_legacy_sidecar(
    legacy_path: &Path,
    staging_dir: &Path,
    quarantine_path: &Path,
    has_replacement: bool,
) -> Result<StagedExtraction> {
    fs::create_dir_all(staging_dir)?;
    let mut explicit = None::<std::io::BufWriter<fs::File>>;
    let mut observed = None::<std::io::BufWriter<fs::File>>;
    let mut quarantine = None::<std::io::BufWriter<fs::File>>;
    let mut result = StagedExtraction::default();
    let mut digest = Sha256::new();
    let mut reader = std::io::BufReader::new(fs::File::open(legacy_path)?);
    let mut bytes = Vec::new();
    let mut line_number = 0u64;

    loop {
        bytes.clear();
        let read = reader.read_until(b'\n', &mut bytes)?;
        if read == 0 {
            break;
        }
        digest.update(&bytes);
        line_number += 1;
        let raw = std::str::from_utf8(&bytes)?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Edge>(trimmed) {
            Ok(edge) => {
                let observed_lane = match edge.provenance {
                    EdgeProvenance::Derived if has_replacement => {
                        result.derived_dropped += 1;
                        continue;
                    }
                    EdgeProvenance::Explicit
                        if edge.kind == "READ_FILE"
                            || edge.kind == "EDITED_FILE"
                            || edge.kind == "RAN_BASH" =>
                    {
                        true
                    }
                    _ => false,
                };
                let (destination, name) = if observed_lane {
                    result.observed_count += 1;
                    (&mut observed, "observed.jsonl")
                } else {
                    result.explicit_count += 1;
                    (&mut explicit, "explicit.jsonl")
                };
                if destination.is_none() {
                    *destination = Some(std::io::BufWriter::new(fs::File::create(
                        staging_dir.join(name),
                    )?));
                }
                let writer = destination.as_mut().expect("staging writer initialized");
                serde_json::to_writer(&mut *writer, &edge)?;
                writer.write_all(b"\n")?;
            }
            Err(error) => {
                result.quarantined_count += 1;
                if quarantine.is_none() {
                    if let Some(parent) = quarantine_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    quarantine = Some(std::io::BufWriter::new(fs::File::create(quarantine_path)?));
                }
                let row = QuarantineLine {
                    source_path: legacy_path.display().to_string(),
                    line_number,
                    raw: trimmed.to_string(),
                    error: error.to_string(),
                };
                let writer = quarantine.as_mut().expect("quarantine writer initialized");
                serde_json::to_writer(&mut *writer, &row)?;
                writer.write_all(b"\n")?;
            }
        }
    }

    for writer in [&mut explicit, &mut observed, &mut quarantine]
        .into_iter()
        .flatten()
    {
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    result.source_hash = hex::encode(digest.finalize());
    Ok(result)
}

pub fn compute_source_hash(edges_dir: &Path, project_id: &str) -> Result<String> {
    let legacy_path = edges_dir.join(format!("{project_id}.jsonl"));
    if !legacy_path.exists() {
        return Ok(String::new());
    }
    let mut hasher = Sha256::new();
    let mut file = fs::File::open(&legacy_path)?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn find_committed_migration(
    edges_dir: &Path,
    project_id: &str,
) -> Result<Option<MigrationManifest>> {
    let m_dir = migrations_dir(edges_dir);
    if !m_dir.is_dir() {
        return Ok(None);
    }
    for entry in fs::read_dir(&m_dir)?.filter_map(Result::ok) {
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }
        let data = fs::read_to_string(&manifest_path)?;
        let manifest: MigrationManifest = serde_json::from_str(&data)?;
        if manifest.project_id == project_id && manifest.status == MigrationStatus::Committed {
            return Ok(Some(manifest));
        }
    }
    Ok(None)
}

#[cfg(test)]
pub fn is_project_migrated(edges_dir: &Path, project_id: &str) -> bool {
    find_committed_migration(edges_dir, project_id)
        .ok()
        .flatten()
        .is_some()
}

pub fn generate_migration_id(project_id: &str, source_hash: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut hasher = Sha256::new();
    hasher.update(project_id.as_bytes());
    hasher.update(source_hash.as_bytes());
    hasher.update(ts.to_le_bytes());
    let hash = hasher.finalize();
    format!(
        "migrate-{}-{}-{}",
        project_id,
        &source_hash[..8.min(source_hash.len())],
        hex::encode(&hash[..4])
    )
}

pub fn has_managed_replacement(edges_dir: &Path, project_id: &str) -> bool {
    edges_dir
        .join("derived")
        .join("project")
        .join(format!("{project_id}.jsonl"))
        .exists()
        || edges_dir
            .join("derived")
            .join("git")
            .join(format!("{project_id}.jsonl"))
            .exists()
        || bbox_edge_sidecar::manifest::try_load_manifest_index(edges_dir)
            .ok()
            .and_then(|idx| idx.workspaces.get(project_id).cloned())
            .is_some()
}

struct MigrationLock {
    lock_path: PathBuf,
}

impl MigrationLock {
    fn release(&self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        self.release();
    }
}

fn acquire_migration_lock(edges_dir: &Path, project_id: &str) -> Result<MigrationLock> {
    let lock_dir = migrations_dir(edges_dir).join("locks");
    fs::create_dir_all(&lock_dir)?;
    let lock_path = lock_dir.join(format!("{project_id}.lock"));
    fs::File::options()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(|_| {
            anyhow::anyhow!(
                "migration already in progress for project {} (lock file: {})",
                project_id,
                lock_path.display()
            )
        })?;
    Ok(MigrationLock { lock_path })
}

pub fn apply_migration(edges_dir: &Path, project_id: &str) -> Result<MigrationManifest> {
    let legacy_path = edges_dir.join(format!("{project_id}.jsonl"));
    if !legacy_path.exists() {
        anyhow::bail!("no legacy sidecar for project {}", project_id);
    }

    if !has_managed_replacement(edges_dir, project_id) {
        anyhow::bail!(
            "no managed materialized replacement exists for project {}; run reindex first",
            project_id
        );
    }

    let _lock = acquire_migration_lock(edges_dir, project_id)?;
    let _mutation_lock =
        bbox_edge_sidecar::edge_sidecar::lock_project_edge_mutation(edges_dir, project_id)?;

    let source_hash = compute_source_hash(edges_dir, project_id)?;
    if let Some(existing) = find_committed_migration(edges_dir, project_id)? {
        if existing.source_hash == source_hash {
            tracing::info!(
                project_id,
                migration_id = %existing.migration_id,
                "migration already committed for this source hash"
            );
            return Ok(existing);
        }
    }

    let migration_id = generate_migration_id(project_id, &source_hash);
    let migration_dir = migrations_dir(edges_dir).join(&migration_id);
    let staging_dir = migration_dir.join("staging");
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let extraction = stage_legacy_sidecar(
        &legacy_path,
        &staging_dir,
        &quarantine_dir(edges_dir, project_id).join(format!("{ts}.jsonl")),
        true,
    )?;
    if extraction.source_hash != source_hash {
        let _ = fs::remove_dir_all(&migration_dir);
        anyhow::bail!(
            "legacy sidecar changed during migration staging for project {}; retry after active writers quiesce",
            project_id
        );
    }

    let manifest = MigrationManifest {
        version: MIGRATION_VERSION,
        migration_id: migration_id.clone(),
        project_id: project_id.to_string(),
        source_path: legacy_path.display().to_string(),
        source_hash: source_hash.clone(),
        status: MigrationStatus::Pending,
        explicit_count: extraction.explicit_count,
        observed_count: extraction.observed_count,
        derived_dropped: extraction.derived_dropped,
        quarantined_count: extraction.quarantined_count,
        backup_path: None,
        created_at: Some(epoch_to_rfc3339()),
        committed_at: None,
    };
    write_migration_manifest(&migration_dir, &manifest)?;

    install_lane_outputs(edges_dir, project_id, &staging_dir)?;

    let backup_path = edges_dir.join(format!("{project_id}.jsonl.bak-migrated-{ts}"));
    fs::rename(&legacy_path, &backup_path)?;

    let committed = MigrationManifest {
        status: MigrationStatus::Committed,
        backup_path: Some(backup_path.display().to_string()),
        committed_at: Some(epoch_to_rfc3339()),
        ..manifest
    };
    write_migration_manifest(&migration_dir, &committed)?;

    Ok(committed)
}

fn install_lane_outputs(edges_dir: &Path, project_id: &str, staging_dir: &Path) -> Result<()> {
    merge_staging_into_lane(
        staging_dir.join("explicit.jsonl"),
        explicit_lane_path(edges_dir, project_id),
    )?;
    merge_staging_into_lane(
        staging_dir.join("observed.jsonl"),
        observed_lane_path(edges_dir, project_id),
    )?;
    Ok(())
}

fn merge_staging_into_lane(staging_path: PathBuf, lane_path: PathBuf) -> Result<()> {
    merge_staging_into_lane_with_chunk_bytes(staging_path, lane_path, SORT_CHUNK_BYTES)
}

fn merge_staging_into_lane_with_chunk_bytes(
    staging_path: PathBuf,
    lane_path: PathBuf,
    chunk_bytes: usize,
) -> Result<()> {
    if !staging_path.exists() {
        return Ok(());
    }
    let work_dir = staging_path.with_extension("merge-runs");
    if work_dir.exists() {
        fs::remove_dir_all(&work_dir)?;
    }
    fs::create_dir_all(&work_dir)?;
    let inputs = [(lane_path.as_path(), 0u8), (staging_path.as_path(), 1u8)];
    let mut runs = build_sorted_runs(&inputs, &work_dir, chunk_bytes)?;
    let sorted = merge_sorted_runs(&mut runs, &work_dir)?;

    let Some(sorted) = sorted else {
        if lane_path.exists() {
            fs::remove_file(&lane_path)?;
        }
        fs::remove_dir_all(&work_dir)?;
        return Ok(());
    };

    if let Some(parent) = lane_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = lane_path.with_extension("jsonl.migrate.tmp");
    let mut reader = std::io::BufReader::new(fs::File::open(&sorted)?);
    let mut writer = std::io::BufWriter::new(fs::File::create(&tmp)?);
    let mut record = String::new();
    while reader.read_line(&mut record)? != 0 {
        let trimmed = record.trim_end_matches(['\n', '\r']);
        let json = trimmed
            .splitn(4, '\t')
            .nth(3)
            .ok_or_else(|| anyhow::anyhow!("invalid migration sort record"))?;
        writer.write_all(json.as_bytes())?;
        writer.write_all(b"\n")?;
        record.clear();
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    // POSIX rename replaces an existing regular-file lane atomically. Never
    // remove the destination first: a kill in that gap used to leave the live
    // lane absent, after which recovery discarded the only complete staging
    // copy and silently lost the pre-existing migrated edges.
    fs::rename(&tmp, &lane_path)?;
    #[cfg(unix)]
    if let Some(parent) = lane_path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    fs::remove_dir_all(&work_dir)?;
    Ok(())
}

fn build_sorted_runs(
    inputs: &[(&Path, u8)],
    work_dir: &Path,
    chunk_bytes: usize,
) -> Result<Vec<PathBuf>> {
    let chunk_bytes = chunk_bytes.max(1);
    let mut chunk = Vec::<String>::new();
    let mut buffered_bytes = 0usize;
    let mut run_paths = Vec::new();
    let mut sequence = 0u64;

    for (path, priority) in inputs {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for line in std::io::BufReader::new(file).lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(edge) = serde_json::from_str::<Edge>(trimmed) else {
                continue;
            };
            let key = edge_import_key_no_meta(&edge);
            let json = serde_json::to_string(&edge)?;
            let record = format!("{key}\t{priority}\t{sequence:020}\t{json}");
            sequence = sequence.wrapping_add(1);
            buffered_bytes = buffered_bytes.saturating_add(record.len());
            chunk.push(record);
            if buffered_bytes >= chunk_bytes {
                flush_sorted_run(&mut chunk, work_dir, &mut run_paths)?;
                buffered_bytes = 0;
            }
        }
    }
    flush_sorted_run(&mut chunk, work_dir, &mut run_paths)?;
    Ok(run_paths)
}

fn flush_sorted_run(
    chunk: &mut Vec<String>,
    work_dir: &Path,
    run_paths: &mut Vec<PathBuf>,
) -> Result<()> {
    if chunk.is_empty() {
        return Ok(());
    }
    chunk.sort_unstable();
    let path = work_dir.join(format!("run-{:08}.txt", run_paths.len()));
    let mut writer = std::io::BufWriter::new(fs::File::create(&path)?);
    let mut prior_key = None::<String>;
    for record in chunk.drain(..) {
        let key = sort_record_key(&record);
        if prior_key.as_deref() == Some(key) {
            continue;
        }
        writer.write_all(record.as_bytes())?;
        writer.write_all(b"\n")?;
        prior_key = Some(key.to_string());
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    run_paths.push(path);
    Ok(())
}

fn merge_sorted_runs(runs: &mut Vec<PathBuf>, work_dir: &Path) -> Result<Option<PathBuf>> {
    let mut round = 0usize;
    while runs.len() > 1 {
        let prior = std::mem::take(runs);
        for (group_index, group) in prior.chunks(MERGE_FAN_IN).enumerate() {
            let output = work_dir.join(format!("merge-{round:04}-{group_index:08}.txt"));
            if group.len() == 1 {
                fs::rename(&group[0], &output)?;
            } else {
                merge_run_group(group, &output)?;
                for input in group {
                    fs::remove_file(input)?;
                }
            }
            runs.push(output);
        }
        round += 1;
    }
    Ok(runs.pop())
}

fn merge_run_group(inputs: &[PathBuf], output: &Path) -> Result<()> {
    let mut readers = inputs
        .iter()
        .map(|path| fs::File::open(path).map(std::io::BufReader::new))
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::<Reverse<(String, usize)>>::new();
    for (index, reader) in readers.iter_mut().enumerate() {
        let mut line = String::new();
        if reader.read_line(&mut line)? != 0 {
            heap.push(Reverse((trim_record_line(line), index)));
        }
    }

    let mut writer = std::io::BufWriter::new(fs::File::create(output)?);
    let mut prior_key = None::<String>;
    while let Some(Reverse((record, reader_index))) = heap.pop() {
        let key = sort_record_key(&record);
        if prior_key.as_deref() != Some(key) {
            writer.write_all(record.as_bytes())?;
            writer.write_all(b"\n")?;
            prior_key = Some(key.to_string());
        }
        let mut next = String::new();
        if readers[reader_index].read_line(&mut next)? != 0 {
            heap.push(Reverse((trim_record_line(next), reader_index)));
        }
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn trim_record_line(mut line: String) -> String {
    let trimmed = line.trim_end_matches(['\n', '\r']).len();
    line.truncate(trimmed);
    line
}

fn sort_record_key(record: &str) -> &str {
    record.split_once('\t').map_or(record, |(key, _)| key)
}

fn edge_import_key_no_meta(edge: &Edge) -> String {
    let mut hasher = Sha256::new();
    hasher.update(
        serde_json::to_string(&edge.source)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(edge.kind.as_bytes());
    hasher.update(
        serde_json::to_string(&edge.target)
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(format!("{:?}", edge.provenance).as_bytes());
    hex::encode(hasher.finalize())
}

fn write_migration_manifest(dir: &Path, manifest: &MigrationManifest) -> Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join("manifest.json");
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp)?;
    serde_json::to_writer_pretty(&mut file, manifest)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)?;
    Ok(())
}

fn epoch_to_rfc3339() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

pub fn recover_pending_migrations(edges_dir: &Path) -> Result<Vec<String>> {
    let m_dir = migrations_dir(edges_dir);
    if !m_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut recovered = Vec::new();
    for entry in fs::read_dir(&m_dir)?.filter_map(Result::ok) {
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }
        let data = fs::read_to_string(&manifest_path)?;
        let manifest: MigrationManifest = match serde_json::from_str(&data) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if manifest.status != MigrationStatus::Pending {
            continue;
        }

        let legacy_exists = edges_dir
            .join(format!("{}.jsonl", manifest.project_id))
            .exists();

        if legacy_exists {
            let _ = fs::remove_dir_all(entry.path());
            recovered.push(format!(
                "removed pending migration {} (source still present, retry on next apply)",
                manifest.migration_id
            ));
        } else {
            let explicit_ok = manifest.explicit_count == 0
                || explicit_lane_path(edges_dir, &manifest.project_id).exists();
            let observed_ok = manifest.observed_count == 0
                || observed_lane_path(edges_dir, &manifest.project_id).exists();

            if explicit_ok && observed_ok {
                let committed = MigrationManifest {
                    status: MigrationStatus::Committed,
                    committed_at: Some(epoch_to_rfc3339()),
                    ..manifest.clone()
                };
                write_migration_manifest(&entry.path(), &committed)?;
                recovered.push(format!(
                    "confirmed pending migration {} (lanes installed, source already moved)",
                    manifest.migration_id
                ));
            } else {
                recovered.push(format!(
                    "WARNING: pending migration {} has missing lane outputs and source gone",
                    manifest.migration_id
                ));
            }
        }
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_chunker::EdgeConfidence;
    use bbox_corpus_core::entity_ref::EntityRef;

    fn derived_edge(id: &str, kind: &str, target: &str) -> Edge {
        Edge {
            source: EntityRef::Knowledge { id: id.into() },
            kind: kind.into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
            project_id: None,
        }
    }

    fn explicit_edge(id: &str, kind: &str, target: &str) -> Edge {
        Edge {
            source: EntityRef::Knowledge { id: id.into() },
            kind: kind.into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
            project_id: None,
        }
    }

    fn observed_edge(id: &str, kind: &str, target: &str) -> Edge {
        Edge {
            source: EntityRef::Knowledge { id: id.into() },
            kind: kind.into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
            project_id: None,
        }
    }

    fn write_legacy(edges_dir: &Path, project_id: &str, lines: &[&str]) {
        fs::create_dir_all(edges_dir).unwrap();
        let path = edges_dir.join(format!("{project_id}.jsonl"));
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            file.write_all(line.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
        }
    }

    fn write_managed_replacement(edges_dir: &Path, project_id: &str) {
        let dir = edges_dir.join("derived").join("project");
        fs::create_dir_all(&dir).unwrap();
        let edge =
            serde_json::to_string(&derived_edge("k_managed", "DESCRIBES", "k_target")).unwrap();
        fs::write(dir.join(format!("{project_id}.jsonl")), edge).unwrap();
    }

    #[test]
    fn extraction_preserves_explicit_edges() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k_exp", "SUPERSEDES", "k_old")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp]);

        let result = extract_legacy_sidecar(edges_dir, "p1").unwrap();
        assert_eq!(result.explicit_edges.len(), 1);
        assert_eq!(result.observed_edges.len(), 0);
        assert_eq!(result.derived_dropped, 0);
    }

    #[test]
    fn extraction_preserves_observed_tool_edges() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let obs = serde_json::to_string(&observed_edge("k_obs", "READ_FILE", "k_target")).unwrap();
        write_legacy(edges_dir, "p1", &[&obs]);

        let result = extract_legacy_sidecar(edges_dir, "p1").unwrap();
        assert_eq!(result.observed_edges.len(), 1);
        assert_eq!(result.explicit_edges.len(), 0);
    }

    #[test]
    fn extraction_drops_derived_with_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let der = serde_json::to_string(&derived_edge("k_der", "DESCRIBES", "k_target")).unwrap();
        write_legacy(edges_dir, "p1", &[&der]);
        write_managed_replacement(edges_dir, "p1");

        let result = extract_legacy_sidecar(edges_dir, "p1").unwrap();
        assert_eq!(result.derived_dropped, 1);
        assert_eq!(result.explicit_edges.len(), 0);
    }

    #[test]
    fn extraction_keeps_derived_without_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let der = serde_json::to_string(&derived_edge("k_der", "DESCRIBES", "k_target")).unwrap();
        write_legacy(edges_dir, "p1", &[&der]);

        let result = extract_legacy_sidecar(edges_dir, "p1").unwrap();
        assert_eq!(result.derived_dropped, 0);
        assert_eq!(result.explicit_edges.len(), 1);
    }

    #[test]
    fn extraction_quarantines_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        write_legacy(edges_dir, "p1", &["not valid json{{{"]);

        let result = extract_legacy_sidecar(edges_dir, "p1").unwrap();
        assert_eq!(result.quarantine.len(), 1);
        assert!(
            !result.quarantine[0].error.is_empty(),
            "quarantine error must be non-empty"
        );
    }

    #[test]
    fn apply_migration_moves_lanes_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k_exp", "SUPERSEDES", "k_old")).unwrap();
        let obs =
            serde_json::to_string(&observed_edge("k_obs", "EDITED_FILE", "k_target")).unwrap();
        let der = serde_json::to_string(&derived_edge("k_der", "DESCRIBES", "k_target")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp, &obs, &der, "bad json"]);
        write_managed_replacement(edges_dir, "p1");

        let manifest = apply_migration(edges_dir, "p1").unwrap();

        assert_eq!(manifest.status, MigrationStatus::Committed);
        assert_eq!(manifest.explicit_count, 1);
        assert_eq!(manifest.observed_count, 1);
        assert_eq!(manifest.derived_dropped, 1);
        assert_eq!(manifest.quarantined_count, 1);
        assert!(manifest.backup_path.is_some());

        let explicit_path = explicit_lane_path(edges_dir, "p1");
        assert!(explicit_path.exists(), "explicit lane must exist");
        let explicit_content = fs::read_to_string(&explicit_path).unwrap();
        assert!(explicit_content.contains("k_exp"));

        let observed_path = observed_lane_path(edges_dir, "p1");
        assert!(observed_path.exists(), "observed lane must exist");
        let observed_content = fs::read_to_string(&observed_path).unwrap();
        assert!(observed_content.contains("k_obs"));

        let legacy = edges_dir.join("p1.jsonl");
        assert!(!legacy.exists(), "legacy sidecar must be moved to backup");

        let q_dir = quarantine_dir(edges_dir, "p1");
        assert!(q_dir.is_dir(), "quarantine dir must exist");
    }

    #[test]
    fn apply_idempotent_same_hash() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k_exp", "SUPERSEDES", "k_old")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp]);
        write_managed_replacement(edges_dir, "p1");

        let first = apply_migration(edges_dir, "p1").unwrap();
        let backup = first.backup_path.clone().unwrap();
        fs::rename(&backup, edges_dir.join("p1.jsonl")).unwrap();

        let second = apply_migration(edges_dir, "p1").unwrap();
        assert_eq!(first.source_hash, second.source_hash);
    }

    #[test]
    fn dry_run_does_not_mutate() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k_exp", "SUPERSEDES", "k_old")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp]);

        let extraction = extract_legacy_sidecar(edges_dir, "p1").unwrap();
        assert_eq!(extraction.explicit_edges.len(), 1);

        let legacy = edges_dir.join("p1.jsonl");
        assert!(legacy.exists(), "dry-run must not modify legacy sidecar");
        assert!(
            !explicit_lane_path(edges_dir, "p1").exists(),
            "dry-run must not create lanes"
        );
    }

    #[test]
    fn is_project_migrated_false_before_apply() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        write_legacy(edges_dir, "p1", &["{}"]);

        assert!(!is_project_migrated(edges_dir, "p1"));
    }

    #[test]
    fn is_project_migrated_true_after_apply() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k1", "DESCRIBES", "k2")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp]);
        write_managed_replacement(edges_dir, "p1");

        apply_migration(edges_dir, "p1").unwrap();
        assert!(is_project_migrated(edges_dir, "p1"));
    }

    #[test]
    fn recover_pending_removes_staging_when_source_present() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k1", "DESCRIBES", "k2")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp]);

        let migration_id = generate_migration_id("p1", "fakehash");
        let m_dir = migrations_dir(edges_dir).join(&migration_id);
        fs::create_dir_all(&m_dir).unwrap();

        let manifest = MigrationManifest {
            version: 1,
            migration_id: migration_id.clone(),
            project_id: "p1".into(),
            source_path: "test".into(),
            source_hash: "fakehash".into(),
            status: MigrationStatus::Pending,
            explicit_count: 0,
            observed_count: 0,
            derived_dropped: 0,
            quarantined_count: 0,
            backup_path: None,
            created_at: None,
            committed_at: None,
        };
        write_migration_manifest(&m_dir, &manifest).unwrap();

        let recovered = recover_pending_migrations(edges_dir).unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].contains("removed pending"));
        assert!(!m_dir.exists(), "pending migration dir must be cleaned up");
    }

    #[test]
    fn recover_pending_confirms_when_source_gone_lanes_exist() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k1", "DESCRIBES", "k2")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp]);
        write_managed_replacement(edges_dir, "p1");
        apply_migration(edges_dir, "p1").unwrap();

        let committed = find_committed_migration(edges_dir, "p1").unwrap().unwrap();

        let m_dir = migrations_dir(edges_dir).join(&committed.migration_id);
        let mut pending = committed.clone();
        pending.status = MigrationStatus::Pending;
        pending.committed_at = None;
        write_migration_manifest(&m_dir, &pending).unwrap();

        let recovered = recover_pending_migrations(edges_dir).unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].contains("confirmed pending"));

        let reloaded: MigrationManifest =
            serde_json::from_str(&fs::read_to_string(m_dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(reloaded.status, MigrationStatus::Committed);
    }

    #[test]
    fn active_loader_does_not_double_load_migrated_sidecar() {
        use crate::edge_index::EdgeIndex;

        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k_exp", "SUPERSEDES", "k_old")).unwrap();
        let der = serde_json::to_string(&derived_edge("k_der", "DESCRIBES", "k_target")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp, &der]);
        write_managed_replacement(edges_dir, "p1");

        apply_migration(edges_dir, "p1").unwrap();

        let legacy = edges_dir.join("p1.jsonl");
        assert!(!legacy.exists(), "legacy must be gone after migration");

        let mut index = EdgeIndex::default();
        let mut seen = std::collections::HashSet::new();
        index
            .load_sidecar_edges(edges_dir, None, &mut seen, true)
            .unwrap();

        let source_exp = EntityRef::Knowledge { id: "k_exp".into() };
        let source_der = EntityRef::Knowledge { id: "k_der".into() };

        assert_eq!(
            index.forward_edges(&source_exp).len(),
            1,
            "explicit edge from migrated lane must load"
        );
        assert_eq!(
            index.forward_edges(&source_der).len(),
            0,
            "derived edge must not load (dropped by migration)"
        );
    }

    #[test]
    fn backup_file_has_migrated_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k1", "DESCRIBES", "k2")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp]);
        write_managed_replacement(edges_dir, "p1");

        let manifest = apply_migration(edges_dir, "p1").unwrap();
        let backup_path = PathBuf::from(manifest.backup_path.unwrap());

        assert!(backup_path.exists(), "backup must exist");
        assert!(
            backup_path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("p1.jsonl.bak-migrated-"),
            "backup must have migrated prefix"
        );
    }

    #[test]
    fn apply_refuses_without_managed_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k1", "DESCRIBES", "k2")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp]);

        let result = apply_migration(edges_dir, "p1");
        assert!(result.is_err(), "must refuse without managed replacement");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("managed materialized replacement"),
            "error must mention replacement requirement: {err}"
        );

        let legacy = edges_dir.join("p1.jsonl");
        assert!(legacy.exists(), "legacy must not be touched");
    }

    #[test]
    fn lane_install_merges_with_existing() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let existing =
            serde_json::to_string(&explicit_edge("k_existing", "DESCRIBES", "k2")).unwrap();
        let lane_dir = edges_dir.join("explicit");
        fs::create_dir_all(&lane_dir).unwrap();
        fs::write(lane_dir.join("p1.jsonl"), &existing).unwrap();

        let exp_new = serde_json::to_string(&explicit_edge("k_new", "DESCRIBES", "k3")).unwrap();
        let exp_dup =
            serde_json::to_string(&explicit_edge("k_existing", "DESCRIBES", "k2")).unwrap();
        let der = serde_json::to_string(&derived_edge("k_der", "DESCRIBES", "k_target")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp_new, &exp_dup, &der]);
        write_managed_replacement(edges_dir, "p1");

        apply_migration(edges_dir, "p1").unwrap();

        let lane_content = fs::read_to_string(explicit_lane_path(edges_dir, "p1")).unwrap();
        assert!(
            lane_content.contains("k_existing"),
            "existing lane edge must survive"
        );
        assert!(lane_content.contains("k_new"), "new edge must be added");
        assert!(
            lane_content.matches("k_existing").count() == 1,
            "duplicate edge must be deduped"
        );
    }

    #[test]
    fn recovery_detects_missing_required_lane() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let migration_id = generate_migration_id("p1", "fakehash");
        let m_dir = migrations_dir(edges_dir).join(&migration_id);
        fs::create_dir_all(&m_dir).unwrap();

        let manifest = MigrationManifest {
            version: 1,
            migration_id: migration_id.clone(),
            project_id: "p1".into(),
            source_path: "test".into(),
            source_hash: "fakehash".into(),
            status: MigrationStatus::Pending,
            explicit_count: 5,
            observed_count: 0,
            derived_dropped: 0,
            quarantined_count: 0,
            backup_path: None,
            created_at: None,
            committed_at: None,
        };
        write_migration_manifest(&m_dir, &manifest).unwrap();

        let recovered = recover_pending_migrations(edges_dir).unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(
            recovered[0].contains("WARNING"),
            "must warn about missing lane outputs: {}",
            recovered[0]
        );

        let reloaded: MigrationManifest =
            serde_json::from_str(&fs::read_to_string(m_dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(
            reloaded.status,
            MigrationStatus::Pending,
            "must not confirm migration with missing lanes"
        );
    }

    #[test]
    fn migrated_backup_uses_bak_prefix_for_gc_compatibility() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k1", "DESCRIBES", "k2")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp]);
        write_managed_replacement(edges_dir, "p1");

        let manifest = apply_migration(edges_dir, "p1").unwrap();
        let backup_name = PathBuf::from(manifest.backup_path.unwrap())
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        assert!(
            backup_name.contains(".bak-"),
            "backup name must contain .bak- for GC compatibility: {backup_name}"
        );
        assert!(
            backup_name.contains("migrated"),
            "backup name must contain 'migrated': {backup_name}"
        );
    }

    #[test]
    fn lane_install_external_merge_is_bounded_and_prefers_existing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let lane_path = dir.path().join("explicit/p1.jsonl");
        let staging_path = dir.path().join("migrations/m1/staging/explicit.jsonl");
        fs::create_dir_all(lane_path.parent().unwrap()).unwrap();
        fs::create_dir_all(staging_path.parent().unwrap()).unwrap();

        let mut existing = explicit_edge("k_existing", "DESCRIBES", "k_target");
        existing.metadata.insert("owner".into(), "existing".into());
        let mut duplicate = existing.clone();
        duplicate.metadata.insert("owner".into(), "staged".into());
        let new = explicit_edge("k_new", "DESCRIBES", "k_target");
        fs::write(
            &lane_path,
            format!("{}\n", serde_json::to_string(&existing).unwrap()),
        )
        .unwrap();
        fs::write(
            &staging_path,
            format!(
                "{}\n{}\n{}\n",
                serde_json::to_string(&duplicate).unwrap(),
                serde_json::to_string(&new).unwrap(),
                serde_json::to_string(&new).unwrap(),
            ),
        )
        .unwrap();

        // One byte forces every row into its own initial run, exercising the
        // bounded multi-run merge rather than the in-memory fast path.
        merge_staging_into_lane_with_chunk_bytes(staging_path, lane_path.clone(), 1).unwrap();

        let content = fs::read_to_string(lane_path).unwrap();
        assert_eq!(content.lines().count(), 2);
        assert!(content.contains("existing"));
        assert!(!content.contains("staged"));
        assert_eq!(content.matches("k_new").count(), 1);
    }

    #[test]
    fn migration_lock_prevents_concurrent_apply() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let _lock1 = acquire_migration_lock(edges_dir, "p1").unwrap();
        let lock2 = acquire_migration_lock(edges_dir, "p1");
        assert!(lock2.is_err(), "second lock acquisition must fail");
    }

    #[test]
    fn migration_lock_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        {
            let _lock = acquire_migration_lock(edges_dir, "p1").unwrap();
        }
        let lock2 = acquire_migration_lock(edges_dir, "p1");
        assert!(lock2.is_ok(), "lock must be released on drop");
    }
}
