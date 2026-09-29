use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

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

fn remove_committed_staging(migration_dir: &Path, manifest: &MigrationManifest) -> Result<bool> {
    if manifest.status != MigrationStatus::Committed {
        anyhow::bail!("refusing to remove staging for an uncommitted migration");
    }
    if migration_dir.file_name().and_then(|name| name.to_str())
        != Some(manifest.migration_id.as_str())
    {
        anyhow::bail!("migration directory does not match its committed manifest id");
    }
    let staging_dir = migration_dir.join("staging");
    let metadata = match fs::symlink_metadata(&staging_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("committed migration staging path is not a nofollow directory");
    }
    fs::remove_dir_all(&staging_dir)?;
    #[cfg(unix)]
    fs::File::open(migration_dir)?.sync_all()?;
    Ok(true)
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
        if manifest.status == MigrationStatus::Committed {
            match remove_committed_staging(&entry.path(), &manifest) {
                Ok(true) => recovered.push(format!(
                    "removed committed migration staging {}",
                    manifest.migration_id
                )),
                Ok(false) => {}
                Err(error) => recovered.push(format!(
                    "WARNING: committed migration staging {} could not be removed: {error}",
                    manifest.migration_id
                )),
            }
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
                if let Err(error) = remove_committed_staging(&entry.path(), &committed) {
                    recovered.push(format!(
                        "WARNING: confirmed migration {} but committed staging cleanup failed: {error}",
                        manifest.migration_id
                    ));
                    continue;
                }
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
    use crate::edge_index::Edge;
    use bbox_chunker::{EdgeConfidence, EdgeProvenance};
    use bbox_corpus_core::entity_ref::EntityRef;
    use std::collections::BTreeMap;
    use std::io::Write;

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

    fn write_legacy(edges_dir: &Path, project_id: &str, lines: &[&str]) {
        fs::create_dir_all(edges_dir).unwrap();
        let path = edges_dir.join(format!("{project_id}.jsonl"));
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            file.write_all(line.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
        }
    }

    /// Write one migration manifest under `migrations/<id>/` and return its
    /// directory.
    fn write_manifest_fixture(
        edges_dir: &Path,
        status: MigrationStatus,
        explicit_count: u64,
    ) -> (MigrationManifest, PathBuf) {
        let migration_id = "migrate-p1-fakehash-00000000".to_string();
        let m_dir = migrations_dir(edges_dir).join(&migration_id);
        fs::create_dir_all(&m_dir).unwrap();
        let manifest = MigrationManifest {
            version: 1,
            migration_id,
            project_id: "p1".into(),
            source_path: "test".into(),
            source_hash: "fakehash".into(),
            status,
            explicit_count,
            observed_count: 0,
            derived_dropped: 0,
            quarantined_count: 0,
            backup_path: None,
            created_at: None,
            committed_at: None,
        };
        write_migration_manifest(&m_dir, &manifest).unwrap();
        (manifest, m_dir)
    }

    fn write_explicit_lane(edges_dir: &Path, project_id: &str) {
        let lane = explicit_lane_path(edges_dir, project_id);
        fs::create_dir_all(lane.parent().unwrap()).unwrap();
        let edge = serde_json::to_string(&explicit_edge("k1", "DESCRIBES", "k2")).unwrap();
        fs::write(lane, format!("{edge}\n")).unwrap();
    }

    #[test]
    fn recover_pending_removes_staging_when_source_present() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let exp = serde_json::to_string(&explicit_edge("k1", "DESCRIBES", "k2")).unwrap();
        write_legacy(edges_dir, "p1", &[&exp]);
        let (_, m_dir) = write_manifest_fixture(edges_dir, MigrationStatus::Pending, 0);

        let recovered = recover_pending_migrations(edges_dir).unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].contains("removed pending"));
        assert!(!m_dir.exists(), "pending migration dir must be cleaned up");
    }

    #[test]
    fn recover_pending_confirms_when_source_gone_lanes_exist() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        write_explicit_lane(edges_dir, "p1");
        let (_, m_dir) = write_manifest_fixture(edges_dir, MigrationStatus::Pending, 1);
        let staging = m_dir.join("staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("observed.jsonl"), b"committed residue").unwrap();

        let recovered = recover_pending_migrations(edges_dir).unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].contains("confirmed pending"));
        assert!(!staging.exists());

        let reloaded: MigrationManifest =
            serde_json::from_str(&fs::read_to_string(m_dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(reloaded.status, MigrationStatus::Committed);
    }

    #[test]
    fn recovery_reclaims_staging_from_already_committed_migration() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        write_explicit_lane(edges_dir, "p1");
        let (committed, migration_dir) =
            write_manifest_fixture(edges_dir, MigrationStatus::Committed, 1);
        let staging = migration_dir.join("staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("observed.jsonl"), vec![b'x'; 1024]).unwrap();

        let recovered = recover_pending_migrations(edges_dir).unwrap();

        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].contains("removed committed migration staging"));
        assert!(!staging.exists());
        let reloaded: MigrationManifest =
            serde_json::from_str(&fs::read_to_string(migration_dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(reloaded, committed);
    }

    #[test]
    fn active_loader_reads_installed_explicit_lane() {
        use crate::edge_index::EdgeIndex;

        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        write_explicit_lane(edges_dir, "p1");

        let mut index = EdgeIndex::default();
        let mut seen = std::collections::HashSet::new();
        index
            .load_sidecar_edges(edges_dir, None, &mut seen, true)
            .unwrap();

        let source = EntityRef::Knowledge { id: "k1".into() };
        assert_eq!(
            index.forward_edges(&source).len(),
            1,
            "an installed explicit lane loads"
        );
    }

    #[test]
    fn recovery_detects_missing_required_lane() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let (_, m_dir) = write_manifest_fixture(edges_dir, MigrationStatus::Pending, 5);

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
}
