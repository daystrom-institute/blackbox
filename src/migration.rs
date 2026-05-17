#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chunker::EdgeProvenance;
use crate::edge_index::Edge;

const MIGRATION_VERSION: u32 = 1;

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

#[derive(Debug, Default)]
pub struct ExtractionResult {
    pub explicit_edges: Vec<Edge>,
    pub observed_edges: Vec<Edge>,
    pub quarantine: Vec<QuarantineLine>,
    pub derived_dropped: u64,
    pub total_lines: u64,
}

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

pub fn compute_source_hash(edges_dir: &Path, project_id: &str) -> Result<String> {
    let legacy_path = edges_dir.join(format!("{project_id}.jsonl"));
    if !legacy_path.exists() {
        return Ok(String::new());
    }
    let data = fs::read(&legacy_path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
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
        || crate::manifest::try_load_manifest_index(edges_dir)
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

    let extraction = extract_legacy_sidecar(edges_dir, project_id)?;
    let migration_id = generate_migration_id(project_id, &source_hash);
    let migration_dir = migrations_dir(edges_dir).join(&migration_id);
    let staging_dir = migration_dir.join("staging");

    fs::create_dir_all(&staging_dir)?;
    fs::create_dir_all(edges_dir.join("explicit"))?;
    fs::create_dir_all(edges_dir.join("observed"))?;

    if !extraction.explicit_edges.is_empty() {
        let path = staging_dir.join("explicit.jsonl");
        let mut file = fs::File::create(&path)?;
        for edge in &extraction.explicit_edges {
            serde_json::to_writer(&mut file, edge)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
    }

    if !extraction.observed_edges.is_empty() {
        let path = staging_dir.join("observed.jsonl");
        let mut file = fs::File::create(&path)?;
        for edge in &extraction.observed_edges {
            serde_json::to_writer(&mut file, edge)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
    }

    if !extraction.quarantine.is_empty() {
        let q_dir = quarantine_dir(edges_dir, project_id);
        fs::create_dir_all(&q_dir)?;
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let q_path = q_dir.join(format!("{ts}.jsonl"));
        let mut file = fs::File::create(&q_path)?;
        for ql in &extraction.quarantine {
            serde_json::to_writer(&mut file, ql)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
    }

    let manifest = MigrationManifest {
        version: MIGRATION_VERSION,
        migration_id: migration_id.clone(),
        project_id: project_id.to_string(),
        source_path: legacy_path.display().to_string(),
        source_hash: source_hash.clone(),
        status: MigrationStatus::Pending,
        explicit_count: extraction.explicit_edges.len() as u64,
        observed_count: extraction.observed_edges.len() as u64,
        derived_dropped: extraction.derived_dropped,
        quarantined_count: extraction.quarantine.len() as u64,
        backup_path: None,
        created_at: Some(epoch_to_rfc3339()),
        committed_at: None,
    };
    write_migration_manifest(&migration_dir, &manifest)?;

    install_lane_outputs(edges_dir, project_id, &staging_dir)?;

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
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
    if !staging_path.exists() {
        return Ok(());
    }

    let mut existing_keys: HashSet<String> = HashSet::new();
    let mut merged: Vec<Edge> = Vec::new();

    if lane_path.exists() {
        let file = fs::File::open(&lane_path)?;
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(edge) = serde_json::from_str::<Edge>(trimmed) {
                let key = edge_import_key_no_meta(&edge);
                existing_keys.insert(key);
                merged.push(edge);
            }
        }
    }

    let staging_file = fs::File::open(&staging_path)?;
    let staging_reader = std::io::BufReader::new(staging_file);
    for line in staging_reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(edge) = serde_json::from_str::<Edge>(trimmed) {
            let key = edge_import_key_no_meta(&edge);
            if existing_keys.insert(key) {
                merged.push(edge);
            }
        }
    }

    if merged.is_empty() {
        if lane_path.exists() {
            fs::remove_file(&lane_path)?;
        }
        return Ok(());
    }

    let tmp = lane_path.with_extension("jsonl.tmp");
    {
        let mut file = fs::File::create(&tmp)?;
        for edge in &merged {
            serde_json::to_writer(&mut file, edge)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
    }
    if lane_path.exists() {
        fs::remove_file(&lane_path)?;
    }
    fs::rename(&tmp, &lane_path)?;
    Ok(())
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
    use crate::chunker::EdgeConfidence;
    use crate::entity_ref::EntityRef;

    fn derived_edge(id: &str, kind: &str, target: &str) -> Edge {
        Edge {
            source: EntityRef::Knowledge { id: id.into() },
            kind: kind.into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
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
        index.load_sidecar_edges(edges_dir, None, &mut seen, true);

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
