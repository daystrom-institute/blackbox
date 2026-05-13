// ---------------------------------------------------------------------------
// Phase 3: Workspace Manifests and Active Loader
// ---------------------------------------------------------------------------
//
// Observed retention gate (phase_3_policy_gate):
//   DECISION: retain observed history indefinitely for now. Observed lanes
//   (tool edges, provenance) are append-only and do not participate in
//   snapshot/branch mechanics. Health warnings for excessive observed bytes
//   per project should be added in a follow-up. Cap, archive, and prune
//   options remain available for future policy without schema changes.
//
// P1 observed backfill consideration:
//   `bbox_project_register` post-step will retroactively walk transcripts
//   and emit EDITED_FILE/READ_FILE/RAN_BASH edges for the newly registered
//   project (see design/proposed/agentic-corpus-followups.md §P1). This
//   will grow observed history for every project that gets registered after
//   initial transcript indexing. Storage health must surface observed bytes
//   per project before that lands so operators can see growth. The observed
//   byte accounting in storage_health.rs (tool_bytes in StorageHealthTotals)
//   is the foundation; per-project breakdown is a follow-up.
// ---------------------------------------------------------------------------

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

const MANIFEST_VERSION: u32 = 1;
const MANIFEST_INDEX_FILENAME: &str = "manifest-index.json";

pub fn materialized_dir(edges_dir: &Path) -> PathBuf {
    edges_dir.join("materialized")
}

pub fn workspace_manifest_dir(edges_dir: &Path, project_id: &str) -> PathBuf {
    materialized_dir(edges_dir)
        .join("workspace")
        .join(project_id)
}

pub fn manifest_index_path(edges_dir: &Path) -> PathBuf {
    materialized_dir(edges_dir).join(MANIFEST_INDEX_FILENAME)
}

// ---------------------------------------------------------------------------
// Workspace manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceManifest {
    pub version: u32,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_common_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_worktree_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub dirty: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_snapshot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_dirty_overlay_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl WorkspaceManifest {
    pub fn manifest_path(edges_dir: &Path, project_id: &str) -> PathBuf {
        workspace_manifest_dir(edges_dir, project_id).join("manifest.json")
    }

    pub fn write_to(edges_dir: &Path, manifest: &Self) -> Result<()> {
        let dir = workspace_manifest_dir(edges_dir, &manifest.project_id);
        fs::create_dir_all(&dir)?;
        let path = dir.join("manifest.json");
        let tmp_path = path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp_path)?;
        serde_json::to_writer_pretty(&mut file, manifest)?;
        file.sync_all()?;
        drop(file);
        fs::rename(tmp_path, path)?;
        Ok(())
    }

    pub fn read_from(path: &Path) -> Result<Self> {
        let data = fs::read_to_string(path)?;
        let manifest: Self = serde_json::from_str(&data)?;
        if manifest.version != MANIFEST_VERSION {
            anyhow::bail!(
                "manifest version {} != expected {}",
                manifest.version,
                MANIFEST_VERSION
            );
        }
        Ok(manifest)
    }
}

// ---------------------------------------------------------------------------
// Manifest index
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceIndexEntry {
    pub manifest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_overlay: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_materialization: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestIndex {
    pub version: u32,
    pub workspaces: std::collections::BTreeMap<String, WorkspaceIndexEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl ManifestIndex {
    pub fn new() -> Self {
        Self {
            version: MANIFEST_VERSION,
            workspaces: std::collections::BTreeMap::new(),
            updated_at: None,
        }
    }

    pub fn load(edges_dir: &Path) -> Result<Self> {
        let path = manifest_index_path(edges_dir);
        let data = fs::read_to_string(&path)?;
        let idx: Self = serde_json::from_str(&data)?;
        if idx.version != MANIFEST_VERSION {
            anyhow::bail!(
                "manifest-index version {} != expected {}",
                idx.version,
                MANIFEST_VERSION
            );
        }
        Ok(idx)
    }

    pub fn write_atomic(&self, edges_dir: &Path) -> Result<()> {
        let dir = materialized_dir(edges_dir);
        fs::create_dir_all(&dir)?;
        let path = manifest_index_path(edges_dir);
        let tmp_path = path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp_path)?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.sync_all()?;
        drop(file);
        fs::rename(tmp_path, path)?;
        Ok(())
    }

    pub fn upsert_workspace(&mut self, project_id: &str, entry: WorkspaceIndexEntry) {
        self.updated_at = Some(chrono_now_rfc3339());
        self.workspaces.insert(project_id.to_string(), entry);
    }

    pub fn remove_workspace(&mut self, project_id: &str) {
        self.workspaces.remove(project_id);
        self.updated_at = Some(chrono_now_rfc3339());
    }

    pub fn active_materialized_paths(&self, edges_dir: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for (project_id, entry) in &self.workspaces {
            let has_overlay = entry.dirty_overlay.is_some()
                && materialized_dir(edges_dir)
                    .join(entry.dirty_overlay.as_ref().unwrap())
                    .is_dir();

            if has_overlay {
                let overlay_dir =
                    materialized_dir(edges_dir).join(entry.dirty_overlay.as_ref().unwrap());
                append_jsonl_files(&overlay_dir, &mut paths);
            } else if let Some(ref snapshot) = entry.active_snapshot {
                let snapshot_dir = materialized_dir(edges_dir).join(snapshot);
                if snapshot_dir.is_dir() {
                    append_jsonl_files(&snapshot_dir, &mut paths);
                }
            }

            if let Some(ref repo_mat) = entry.repo_materialization {
                let repo_dir = materialized_dir(edges_dir).join(repo_mat);
                if repo_dir.is_dir() {
                    append_jsonl_files(&repo_dir, &mut paths);
                }
            }
            if entry.active_snapshot.is_none() && !has_overlay {
                let managed_dir = edges_dir
                    .join("derived")
                    .join("project")
                    .join(format!("{}.jsonl", project_id));
                if managed_dir.exists() {
                    paths.push(managed_dir);
                }
            }
        }
        paths
    }

    pub fn protected_materialized_paths(&self, edges_dir: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for (project_id, entry) in &self.workspaces {
            let has_overlay = entry.dirty_overlay.is_some()
                && materialized_dir(edges_dir)
                    .join(entry.dirty_overlay.as_ref().unwrap())
                    .is_dir();

            if has_overlay {
                let overlay_dir =
                    materialized_dir(edges_dir).join(entry.dirty_overlay.as_ref().unwrap());
                append_jsonl_files(&overlay_dir, &mut paths);
            }

            if let Some(ref snapshot) = entry.active_snapshot {
                let snapshot_dir = materialized_dir(edges_dir).join(snapshot);
                if snapshot_dir.is_dir() {
                    append_jsonl_files(&snapshot_dir, &mut paths);
                }
            }

            if let Some(ref repo_mat) = entry.repo_materialization {
                let repo_dir = materialized_dir(edges_dir).join(repo_mat);
                if repo_dir.is_dir() {
                    append_jsonl_files(&repo_dir, &mut paths);
                }
            }
            if entry.active_snapshot.is_none() && !has_overlay {
                let managed_dir = edges_dir
                    .join("derived")
                    .join("project")
                    .join(format!("{}.jsonl", project_id));
                if managed_dir.exists() {
                    paths.push(managed_dir);
                }
            }
        }
        paths
    }

    pub fn validate(&self, edges_dir: &Path) -> ManifestValidation {
        let mut missing_manifests = Vec::new();
        let mut missing_paths = Vec::new();
        for (project_id, entry) in &self.workspaces {
            let manifest_path = materialized_dir(edges_dir).join(&entry.manifest);
            if !manifest_path.exists() {
                missing_manifests.push(project_id.clone());
                continue;
            }
            if let Some(ref snapshot) = entry.active_snapshot {
                let snap_dir = materialized_dir(edges_dir).join(snapshot);
                if !snap_dir.is_dir() {
                    missing_paths.push(snapshot.clone());
                }
            }
            if let Some(ref overlay) = entry.dirty_overlay {
                let overlay_dir = materialized_dir(edges_dir).join(overlay);
                if !overlay_dir.is_dir() {
                    missing_paths.push(overlay.clone());
                }
            }
            if let Some(ref repo_mat) = entry.repo_materialization {
                let repo_dir = materialized_dir(edges_dir).join(repo_mat);
                if !repo_dir.is_dir() {
                    missing_paths.push(repo_mat.clone());
                }
            }
        }
        if missing_manifests.is_empty() && missing_paths.is_empty() {
            ManifestValidation::Valid
        } else {
            ManifestValidation::Invalid {
                missing_manifests,
                missing_paths,
            }
        }
    }
}

fn append_jsonl_files(dir: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            paths.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Fallback reason for active loader
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestFallbackReason {
    MissingNotMigrated,
    Corrupt {
        error: String,
    },
    Stale {
        missing_manifests: Vec<String>,
        missing_paths: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// Manifest validation result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ManifestValidation {
    Valid,
    Invalid {
        missing_manifests: Vec<String>,
        missing_paths: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// Try-load with fallback classification
// ---------------------------------------------------------------------------

pub fn try_load_manifest_index(edges_dir: &Path) -> Result<ManifestIndex, ManifestFallbackReason> {
    let path = manifest_index_path(edges_dir);
    if !path.exists() {
        return Err(ManifestFallbackReason::MissingNotMigrated);
    }
    match ManifestIndex::load(edges_dir) {
        Ok(idx) => match idx.validate(edges_dir) {
            ManifestValidation::Valid => Ok(idx),
            ManifestValidation::Invalid {
                missing_manifests,
                missing_paths,
            } => Err(ManifestFallbackReason::Stale {
                missing_manifests,
                missing_paths,
            }),
        },
        Err(e) => Err(ManifestFallbackReason::Corrupt {
            error: e.to_string(),
        }),
    }
}

fn chrono_now_rfc3339() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let mut idx = ManifestIndex::new();
        idx.upsert_workspace(
            "proj1234",
            WorkspaceIndexEntry {
                manifest: "workspace/proj1234/manifest.json".into(),
                active_snapshot: Some("workspace/proj1234/snapshots/head-abc".into()),
                dirty_overlay: None,
                repo_materialization: None,
            },
        );
        idx.write_atomic(edges_dir).unwrap();

        let loaded = ManifestIndex::load(edges_dir).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.workspaces.len(), 1);
        assert_eq!(
            loaded.workspaces["proj1234"].manifest,
            "workspace/proj1234/manifest.json"
        );
    }

    #[test]
    fn manifest_index_missing_is_missing_not_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let result = try_load_manifest_index(dir.path());
        assert_eq!(
            result.unwrap_err(),
            ManifestFallbackReason::MissingNotMigrated
        );
    }

    #[test]
    fn manifest_index_corrupt_json_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mat_dir = materialized_dir(dir.path());
        fs::create_dir_all(&mat_dir).unwrap();
        fs::write(manifest_index_path(dir.path()), b"not json{{{").unwrap();

        let result = try_load_manifest_index(dir.path());
        match result.unwrap_err() {
            ManifestFallbackReason::Corrupt { .. } => {}
            other => panic!("expected Corrupt, got {:?}", other),
        }
    }

    #[test]
    fn manifest_index_stale_missing_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let mut idx = ManifestIndex::new();
        idx.upsert_workspace(
            "proj_missing",
            WorkspaceIndexEntry {
                manifest: "workspace/proj_missing/manifest.json".into(),
                active_snapshot: None,
                dirty_overlay: None,
                repo_materialization: None,
            },
        );
        idx.write_atomic(edges_dir).unwrap();

        let result = try_load_manifest_index(edges_dir);
        match result.unwrap_err() {
            ManifestFallbackReason::Stale {
                missing_manifests, ..
            } => {
                assert!(missing_manifests.contains(&"proj_missing".to_string()));
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn workspace_manifest_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = WorkspaceManifest {
            version: 1,
            project_id: "proj1234".into(),
            repo_id: Some("repo_abcd".into()),
            canonical_path: Some("/home/me/repo".into()),
            git_common_dir: None,
            git_worktree_dir: None,
            branch: Some("main".into()),
            head_sha: Some("abc123".into()),
            dirty: false,
            dirty_fingerprint: None,
            active_snapshot_id: Some("head-abc123".into()),
            active_dirty_overlay_id: None,
            updated_at: None,
        };
        WorkspaceManifest::write_to(dir.path(), &manifest).unwrap();

        let path = WorkspaceManifest::manifest_path(dir.path(), "proj1234");
        let loaded = WorkspaceManifest::read_from(&path).unwrap();
        assert_eq!(loaded, manifest);
    }

    #[test]
    fn active_materialized_paths_includes_managed_project_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let managed_dir = edges_dir.join("derived").join("project");
        fs::create_dir_all(&managed_dir).unwrap();
        fs::write(managed_dir.join("proj1234.jsonl"), b"{}").unwrap();

        let mut idx = ManifestIndex::new();
        idx.upsert_workspace(
            "proj1234",
            WorkspaceIndexEntry {
                manifest: "workspace/proj1234/manifest.json".into(),
                active_snapshot: None,
                dirty_overlay: None,
                repo_materialization: None,
            },
        );

        let paths = idx.active_materialized_paths(edges_dir);
        assert!(
            paths
                .iter()
                .any(|p| p.to_str().unwrap().contains("proj1234.jsonl")),
            "managed project sidecar must be in active paths"
        );
    }

    #[test]
    fn active_loader_skips_inactive_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let active_snap = materialized_dir(edges_dir)
            .join("workspace")
            .join("p1")
            .join("snapshots")
            .join("head-active");
        fs::create_dir_all(&active_snap).unwrap();
        let active_edge = r#"{"source":"knowledge:k1","kind":"DESCRIBES","target":"knowledge:k2","provenance":"explicit","confidence":"exact","metadata":{}}"#;
        fs::write(active_snap.join("project.jsonl"), active_edge).unwrap();

        let inactive_snap = materialized_dir(edges_dir)
            .join("workspace")
            .join("p1")
            .join("snapshots")
            .join("head-old-branch");
        fs::create_dir_all(&inactive_snap).unwrap();
        let inactive_edge = r#"{"source":"knowledge:stale","kind":"DESCRIBES","target":"knowledge:k3","provenance":"explicit","confidence":"exact","metadata":{}}"#;
        fs::write(inactive_snap.join("project.jsonl"), inactive_edge).unwrap();

        let mut idx = ManifestIndex::new();
        idx.upsert_workspace(
            "p1",
            WorkspaceIndexEntry {
                manifest: "workspace/p1/manifest.json".into(),
                active_snapshot: Some("workspace/p1/snapshots/head-active".into()),
                dirty_overlay: None,
                repo_materialization: None,
            },
        );

        let paths = idx.active_materialized_paths(edges_dir);
        assert!(
            paths
                .iter()
                .any(|p| p.to_str().unwrap().contains("head-active")),
            "active snapshot must be in paths"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p.to_str().unwrap().contains("head-old-branch")),
            "inactive snapshot must NOT be in paths"
        );
    }

    #[test]
    fn active_paths_excludes_managed_sidecar_when_snapshot_exists() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let managed_dir = edges_dir.join("derived").join("project");
        fs::create_dir_all(&managed_dir).unwrap();
        fs::write(managed_dir.join("p1.jsonl"), b"{}").unwrap();

        let snap_dir = materialized_dir(edges_dir)
            .join("workspace")
            .join("p1")
            .join("snapshots")
            .join("head-abc");
        fs::create_dir_all(&snap_dir).unwrap();
        fs::write(snap_dir.join("project.jsonl"), b"{}").unwrap();

        let mut idx = ManifestIndex::new();
        idx.upsert_workspace(
            "p1",
            WorkspaceIndexEntry {
                manifest: "workspace/p1/manifest.json".into(),
                active_snapshot: Some("workspace/p1/snapshots/head-abc".into()),
                dirty_overlay: None,
                repo_materialization: None,
            },
        );

        let paths = idx.active_materialized_paths(edges_dir);
        assert!(
            paths
                .iter()
                .any(|p| p.to_str().unwrap().contains("head-abc")),
            "active snapshot must be in paths"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p.to_str().unwrap().contains("derived/project/p1.jsonl")),
            "managed sidecar must NOT be in paths when snapshot exists"
        );
    }

    #[test]
    fn manifest_version_mismatch_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mat_dir = materialized_dir(dir.path());
        fs::create_dir_all(&mat_dir).unwrap();
        let bad = r#"{"version":99,"workspaces":{},"updated_at":null}"#;
        fs::write(manifest_index_path(dir.path()), bad).unwrap();

        let result = try_load_manifest_index(dir.path());
        match result.unwrap_err() {
            ManifestFallbackReason::Corrupt { error } => {
                assert!(error.contains("version"));
            }
            other => panic!("expected Corrupt, got {:?}", other),
        }
    }
}
