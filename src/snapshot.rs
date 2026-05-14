use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::chunker::EdgeProvenance;
use crate::edge_index::Edge;
use crate::entity_ref::EntityRef;
use crate::manifest::{
    ManifestIndex, OverlayManifest, WorkspaceIndexEntry, WorkspaceManifest, materialized_dir,
};

const INDEXER_VERSION: &str = "project-index-v1";
const CHUNKER_VERSION: &str = "chunker-v1";
const DIRTY_OVERLAY_DIRNAME: &str = "dirty-current";

pub fn clean_snapshot_id(repo_id: &str, project_id: &str, head_sha: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_id.as_bytes());
    hasher.update(project_id.as_bytes());
    hasher.update(head_sha.as_bytes());
    hasher.update(INDEXER_VERSION.as_bytes());
    hasher.update(CHUNKER_VERSION.as_bytes());
    let hash = hasher.finalize();
    let sha_prefix = &head_sha[..head_sha.len().min(12)];
    format!("head-{}-{}", sha_prefix, hex::encode(&hash[..8]))
}

pub fn nongit_snapshot_id(project_id: &str, source_tree_fingerprint: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(project_id.as_bytes());
    hasher.update(source_tree_fingerprint.as_bytes());
    hasher.update(INDEXER_VERSION.as_bytes());
    hasher.update(CHUNKER_VERSION.as_bytes());
    let hash = hasher.finalize();
    format!("nongit-{}", hex::encode(&hash[..16]))
}

pub fn snapshot_dir(edges_dir: &Path, project_id: &str, snapshot_id: &str) -> PathBuf {
    materialized_dir(edges_dir)
        .join("workspace")
        .join(project_id)
        .join("snapshots")
        .join(snapshot_id)
}

pub fn dirty_overlay_dir(edges_dir: &Path, project_id: &str) -> PathBuf {
    materialized_dir(edges_dir)
        .join("workspace")
        .join(project_id)
        .join(DIRTY_OVERLAY_DIRNAME)
}

pub fn write_snapshot_files(
    edges_dir: &Path,
    project_id: &str,
    snapshot_id: &str,
    files: &[(&str, &[Edge])],
) -> Result<()> {
    let snap_dir = snapshot_dir(edges_dir, project_id, snapshot_id);
    let tmp_dir = snap_dir.with_extension("write-tmp");

    if tmp_dir.is_dir() {
        let _ = fs::remove_dir_all(&tmp_dir);
    }
    fs::create_dir_all(&tmp_dir)?;
    for (filename, edges) in files {
        let path = tmp_dir.join(*filename);
        let mut file = fs::File::create(&path)?;
        for edge in *edges {
            serde_json::to_writer(&mut file, edge)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
    }
    if snap_dir.is_dir() {
        let _ = fs::remove_dir_all(&snap_dir);
    }
    fs::rename(&tmp_dir, &snap_dir)?;
    Ok(())
}

pub fn write_dirty_overlay(
    edges_dir: &Path,
    project_id: &str,
    files: &[(&str, &[Edge])],
) -> Result<()> {
    for (filename, edges) in files {
        for e in *edges {
            if e.provenance != EdgeProvenance::Derived {
                anyhow::bail!(
                    "dirty overlay rejected non-Derived edge in {}: kind={} provenance={:?} source={:?}",
                    filename,
                    e.kind,
                    e.provenance,
                    e.source,
                );
            }
        }
    }

    let overlay_dir = dirty_overlay_dir(edges_dir, project_id);
    let tmp_dir = overlay_dir.with_extension("write-tmp");

    if tmp_dir.is_dir() {
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    let all_empty = files.iter().all(|(_, edges)| edges.is_empty());
    if all_empty {
        if overlay_dir.is_dir() {
            for entry in fs::read_dir(&overlay_dir)? {
                let entry = entry?;
                if entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        return Ok(());
    }

    fs::create_dir_all(&tmp_dir)?;

    // Collect covered rel_path_hashes from all overlay edges so the loader
    // can merge snapshot + overlay at per-file granularity.
    let mut covered_hashes = std::collections::HashSet::new();
    for (_filename, edges) in files {
        for edge in *edges {
            if let EntityRef::ProjectFile { rel_path_hash, .. } = &edge.source {
                covered_hashes.insert(rel_path_hash.clone());
            }
            if let EntityRef::ProjectFile { rel_path_hash, .. } = &edge.target {
                covered_hashes.insert(rel_path_hash.clone());
            }
        }
    }

    for (filename, edges) in files {
        if edges.is_empty() {
            continue;
        }
        let path = tmp_dir.join(*filename);
        let mut file = fs::File::create(&path)?;
        for edge in *edges {
            serde_json::to_writer(&mut file, edge)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
    }

    // Write overlay_manifest.json so the loader knows which hashes are covered.
    OverlayManifest::write_to(&tmp_dir, &covered_hashes)?;

    if overlay_dir.is_dir() {
        let _ = fs::remove_dir_all(&overlay_dir);
    }
    fs::rename(&tmp_dir, &overlay_dir)?;
    Ok(())
}

pub fn clear_dirty_overlay(edges_dir: &Path, project_id: &str) -> Result<bool> {
    let overlay_dir = dirty_overlay_dir(edges_dir, project_id);
    if !overlay_dir.is_dir() {
        return Ok(false);
    }

    let validation = validate_overlay_provenance(&overlay_dir);
    if let Err(bad_files) = validation {
        quarantine_dirty_overlay(edges_dir, project_id, &bad_files)?;
        tracing::warn!(
            project_id,
            ?bad_files,
            "quarantined dirty overlay containing non-Derived provenance"
        );
        return Ok(false);
    }

    fs::remove_dir_all(&overlay_dir)?;
    Ok(true)
}

fn validate_overlay_provenance(overlay_dir: &Path) -> std::result::Result<(), Vec<PathBuf>> {
    let mut bad = Vec::new();
    if let Ok(entries) = fs::read_dir(overlay_dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(file) = fs::File::open(&path) {
                let reader = std::io::BufReader::new(file);
                use std::io::BufRead;
                for line in reader.lines().flatten() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(edge) = serde_json::from_str::<Edge>(trimmed) {
                        if edge.provenance != EdgeProvenance::Derived {
                            bad.push(path.clone());
                            break;
                        }
                    }
                }
            }
        }
    }
    if bad.is_empty() { Ok(()) } else { Err(bad) }
}

fn quarantine_dirty_overlay(
    edges_dir: &Path,
    project_id: &str,
    bad_files: &[PathBuf],
) -> Result<()> {
    let quarantine_dir = materialized_dir(edges_dir)
        .join("workspace")
        .join(project_id)
        .join("dirty-quarantine");

    fs::create_dir_all(&quarantine_dir)?;
    for src in bad_files {
        let filename = src.file_name().unwrap_or_default();
        let dest = quarantine_dir.join(filename);
        if src.exists() {
            fs::rename(src, &dest)?;
        }
    }
    Ok(())
}

pub struct SnapshotSwitch {
    pub snapshot_id: String,
    pub dirty: bool,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub edges: Vec<Edge>,
}

pub fn switch_to_clean_snapshot(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    branch: Option<&str>,
    head_sha: &str,
    project_edges: Vec<Edge>,
    symbol_edges: Vec<Edge>,
    git_current_edges: Vec<Edge>,
) -> Result<()> {
    let snap_id = clean_snapshot_id(repo_id, project_id, head_sha);
    let snap_path = snapshot_dir(edges_dir, project_id, &snap_id);

    if !snap_path.is_dir() {
        let mut files: Vec<(&str, &[Edge])> = Vec::new();
        let empty: Vec<Edge> = Vec::new();
        files.push(("project.jsonl", &project_edges));
        files.push((
            "symbols.jsonl",
            if symbol_edges.is_empty() {
                &empty
            } else {
                &symbol_edges
            },
        ));
        files.push((
            "git-current.jsonl",
            if git_current_edges.is_empty() {
                &empty
            } else {
                &git_current_edges
            },
        ));
        write_snapshot_files(edges_dir, project_id, &snap_id, &files)?;
    }

    let had_overlay = clear_dirty_overlay(edges_dir, project_id)?;
    if had_overlay {
        tracing::info!(project_id, "cleared dirty overlay on clean checkout");
    }

    update_manifest_for_snapshot(
        edges_dir,
        project_id,
        repo_id,
        branch,
        Some(head_sha),
        false,
        None,
        &snap_id,
        None,
    )?;

    Ok(())
}

pub fn switch_to_dirty_overlay(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    branch: Option<&str>,
    head_sha: &str,
    dirty_fingerprint: &str,
    project_edges: Vec<Edge>,
    symbol_edges: Vec<Edge>,
    git_current_edges: Vec<Edge>,
) -> Result<()> {
    let snap_id = clean_snapshot_id(repo_id, project_id, head_sha);
    let snap_path = snapshot_dir(edges_dir, project_id, &snap_id);

    if !snap_path.is_dir() {
        let empty: Vec<Edge> = Vec::new();
        let files: Vec<(&str, &[Edge])> = vec![("project.jsonl", &empty)];
        write_snapshot_files(edges_dir, project_id, &snap_id, &files)?;
    }

    let overlay_files: Vec<(&str, &[Edge])> = vec![
        ("project.jsonl", &project_edges),
        ("symbols.jsonl", &symbol_edges),
        ("git-current.jsonl", &git_current_edges),
    ];
    write_dirty_overlay(edges_dir, project_id, &overlay_files)?;

    update_manifest_for_snapshot(
        edges_dir,
        project_id,
        repo_id,
        branch,
        Some(head_sha),
        true,
        Some(dirty_fingerprint),
        &snap_id,
        Some(&format!(
            "workspace/{}/{}",
            project_id, DIRTY_OVERLAY_DIRNAME
        )),
    )?;

    Ok(())
}

fn update_manifest_for_snapshot(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    branch: Option<&str>,
    head_sha: Option<&str>,
    dirty: bool,
    dirty_fingerprint: Option<&str>,
    snapshot_id: &str,
    dirty_overlay_rel: Option<&str>,
) -> Result<()> {
    let manifest = WorkspaceManifest {
        version: 1,
        project_id: project_id.to_string(),
        repo_id: Some(repo_id.to_string()),
        canonical_path: None,
        git_common_dir: None,
        git_worktree_dir: None,
        branch: branch.map(|b| b.to_string()),
        head_sha: head_sha.map(|s| s.to_string()),
        dirty,
        dirty_fingerprint: dirty_fingerprint.map(|f| f.to_string()),
        active_snapshot_id: Some(snapshot_id.to_string()),
        active_dirty_overlay_id: dirty_overlay_rel.map(|r| r.to_string()),
        updated_at: None,
    };
    WorkspaceManifest::write_to(edges_dir, &manifest)?;

    let mut idx = ManifestIndex::load(edges_dir).unwrap_or_else(|_| ManifestIndex::new());

    let snap_rel = format!("workspace/{}/snapshots/{}", project_id, snapshot_id);
    idx.upsert_workspace(
        project_id,
        WorkspaceIndexEntry {
            manifest: format!("workspace/{}/manifest.json", project_id),
            active_snapshot: Some(snap_rel),
            dirty_overlay: dirty_overlay_rel.map(|r| r.to_string()),
            repo_materialization: None,
        },
    );
    idx.write_atomic(edges_dir)?;

    Ok(())
}

pub fn worktree_identity(project_path: &Path) -> (String, Option<String>) {
    let project_id = crate::entity_ref::project_id_for_path(project_path)
        .unwrap_or_else(|_| hash_path_fallback(project_path));
    let repo_id = discover_repo_id(project_path);
    (project_id, repo_id)
}

fn hash_path_fallback(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hex::encode(&hasher.finalize()[..8])
}

fn discover_repo_id(project_path: &Path) -> Option<String> {
    let git_dir = project_path.join(".git");
    if !git_dir.exists() {
        return None;
    }
    crate::entity_ref::repo_id_for_path(project_path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_ref::EntityRef;

    fn make_edge(id: &str, kind: &str, target: &str, prov: EdgeProvenance) -> Edge {
        Edge {
            source: EntityRef::Knowledge { id: id.into() },
            kind: kind.into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: prov,
            confidence: crate::chunker::EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
        }
    }

    fn derived_edge(id: &str, kind: &str, target: &str) -> Edge {
        make_edge(id, kind, target, EdgeProvenance::Derived)
    }

    fn explicit_edge(id: &str, kind: &str, target: &str) -> Edge {
        make_edge(id, kind, target, EdgeProvenance::Explicit)
    }

    #[test]
    fn clean_snapshot_id_differs_across_branches() {
        let a = clean_snapshot_id("repo1", "proj1", "aaa111222333");
        let b = clean_snapshot_id("repo1", "proj1", "bbb444555666");
        assert_ne!(
            a, b,
            "different head_sha must produce different snapshot ids"
        );
    }

    #[test]
    fn clean_snapshot_id_is_deterministic() {
        let id1 = clean_snapshot_id("repo1", "proj1", "abc123");
        let id2 = clean_snapshot_id("repo1", "proj1", "abc123");
        assert_eq!(id1, id2);
    }

    #[test]
    fn clean_snapshot_id_excludes_dirty_fingerprint() {
        let id = clean_snapshot_id("repo1", "proj1", "abc123def456");
        assert!(
            id.starts_with("head-abc123def456-"),
            "snapshot id should start with head sha prefix, got: {id}"
        );
    }

    #[test]
    fn nongit_snapshot_id_differs_by_fingerprint() {
        let a = nongit_snapshot_id("proj1", "fp-a");
        let b = nongit_snapshot_id("proj1", "fp-b");
        assert_ne!(a, b);
    }

    #[test]
    fn write_snapshot_creates_files_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![derived_edge("k1", "DESCRIBES", "k2")];
        write_snapshot_files(edges_dir, "p1", "snap-001", &[("project.jsonl", &edges)]).unwrap();

        let snap = snapshot_dir(edges_dir, "p1", "snap-001");
        assert!(snap.is_dir(), "snapshot dir must exist");
        let project_jsonl = snap.join("project.jsonl");
        assert!(project_jsonl.exists(), "project.jsonl must exist");

        let content = fs::read_to_string(&project_jsonl).unwrap();
        assert!(
            content.contains("k1"),
            "project.jsonl must contain edge data"
        );
    }

    #[test]
    fn write_snapshot_atomic_on_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let snap_dir = snapshot_dir(edges_dir, "p1", "snap-conflict");
        fs::create_dir_all(&snap_dir).unwrap();
        fs::write(snap_dir.join("project.jsonl"), "stale").unwrap();

        let edges = vec![derived_edge("k_new", "DESCRIBES", "k_target")];
        write_snapshot_files(
            edges_dir,
            "p1",
            "snap-conflict",
            &[("project.jsonl", &edges)],
        )
        .unwrap();

        let content = fs::read_to_string(snap_dir.join("project.jsonl")).unwrap();
        assert!(
            content.contains("k_new"),
            "overwritten snapshot must have new edge"
        );
        assert!(
            !content.contains("stale"),
            "overwritten snapshot must not have old content"
        );
    }

    #[test]
    fn dirty_overlay_writes_derived_edges() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![derived_edge("k_dirty", "DESCRIBES", "k_target")];
        write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges)]).unwrap();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        assert!(overlay.is_dir());
        let content = fs::read_to_string(overlay.join("project.jsonl")).unwrap();
        assert!(content.contains("k_dirty"));
    }

    #[test]
    fn dirty_overlay_rejects_explicit_edges() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![explicit_edge("k_exp", "DESCRIBES", "k_target")];
        let result = write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges)]);
        assert!(result.is_err(), "must reject explicit edges");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("non-Derived"),
            "error must mention non-Derived: {err}"
        );
    }

    #[test]
    fn dirty_overlay_replaces_previous() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges_v1 = vec![derived_edge("k_v1", "DESCRIBES", "k_target")];
        write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges_v1)]).unwrap();

        let edges_v2 = vec![derived_edge("k_v2", "DESCRIBES", "k_target")];
        write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges_v2)]).unwrap();

        let content =
            fs::read_to_string(dirty_overlay_dir(edges_dir, "p1").join("project.jsonl")).unwrap();
        assert!(content.contains("k_v2"), "overlay must contain new edge");
        assert!(
            !content.contains("k_v1"),
            "overlay must not contain old edge"
        );
    }

    #[test]
    fn clear_dirty_overlay_removes_dir() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![derived_edge("k1", "DESCRIBES", "k2")];
        write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges)]).unwrap();

        let cleared = clear_dirty_overlay(edges_dir, "p1").unwrap();
        assert!(cleared, "should report overlay was cleared");
        assert!(
            !dirty_overlay_dir(edges_dir, "p1").exists(),
            "overlay dir must be gone"
        );
    }

    #[test]
    fn clear_dirty_overlay_returns_false_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cleared = clear_dirty_overlay(dir.path(), "p1").unwrap();
        assert!(!cleared, "should report nothing to clear");
    }

    #[test]
    fn clear_dirty_overlay_quarantines_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        fs::create_dir_all(&overlay).unwrap();
        let explicit_edge_line =
            serde_json::to_string(&explicit_edge("k_exp", "DESCRIBES", "k2")).unwrap();
        fs::write(overlay.join("project.jsonl"), explicit_edge_line).unwrap();

        let cleared = clear_dirty_overlay(edges_dir, "p1").unwrap();
        assert!(
            !cleared,
            "should not report success for quarantined overlay"
        );

        let quarantine = materialized_dir(edges_dir)
            .join("workspace")
            .join("p1")
            .join("dirty-quarantine")
            .join("project.jsonl");
        assert!(quarantine.exists(), "quarantined file must exist");
    }

    #[test]
    fn switch_to_clean_creates_snapshot_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![derived_edge("k1", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "abc123def456",
            edges.clone(),
            vec![],
            vec![],
        )
        .unwrap();

        let snap_id = clean_snapshot_id("repo1", "p1", "abc123def456");
        let snap = snapshot_dir(edges_dir, "p1", &snap_id);
        assert!(snap.is_dir(), "snapshot dir must exist");

        let manifest_path = crate::manifest::WorkspaceManifest::manifest_path(edges_dir, "p1");
        let manifest = WorkspaceManifest::read_from(&manifest_path).unwrap();
        assert_eq!(
            manifest.active_snapshot_id.as_deref(),
            Some(snap_id.as_str())
        );
        assert!(!manifest.dirty);
        assert!(manifest.active_dirty_overlay_id.is_none());
    }

    #[test]
    fn switch_to_clean_reuses_existing_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges_a = vec![derived_edge("k_branch_a", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa1111",
            edges_a.clone(),
            vec![],
            vec![],
        )
        .unwrap();

        let edges_b = vec![derived_edge("k_branch_b", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-b"),
            "sha_bbbb2222",
            edges_b.clone(),
            vec![],
            vec![],
        )
        .unwrap();

        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa1111",
            edges_a.clone(),
            vec![],
            vec![],
        )
        .unwrap();

        let snap_a = clean_snapshot_id("repo1", "p1", "sha_aaaa1111");
        let snap_dir = snapshot_dir(edges_dir, "p1", &snap_a);
        let content = fs::read_to_string(snap_dir.join("project.jsonl")).unwrap();
        assert!(
            content.contains("k_branch_a"),
            "reused snapshot must have original edge"
        );
    }

    #[test]
    fn switch_to_dirty_creates_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let dirty_edges = vec![derived_edge("k_dirty", "DESCRIBES", "k2")];
        switch_to_dirty_overlay(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "abc123def456",
            "fp-dirty-v1",
            dirty_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        assert!(overlay.is_dir(), "dirty overlay dir must exist");
        let content = fs::read_to_string(overlay.join("project.jsonl")).unwrap();
        assert!(content.contains("k_dirty"));

        let manifest_path = WorkspaceManifest::manifest_path(edges_dir, "p1");
        let manifest = WorkspaceManifest::read_from(&manifest_path).unwrap();
        assert!(manifest.dirty);
        assert_eq!(manifest.dirty_fingerprint.as_deref(), Some("fp-dirty-v1"));
        assert!(manifest.active_dirty_overlay_id.is_some());
    }

    #[test]
    fn clean_checkout_clears_dirty_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let dirty_edges = vec![derived_edge("k_dirty", "DESCRIBES", "k2")];
        switch_to_dirty_overlay(
            edges_dir,
            "p1",
            "repo1",
            Some("feature"),
            "sha111222333",
            "fp-dirty",
            dirty_edges,
            vec![],
            vec![],
        )
        .unwrap();
        assert!(dirty_overlay_dir(edges_dir, "p1").is_dir());

        let clean_edges = vec![derived_edge("k_clean", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha444555666",
            clean_edges,
            vec![],
            vec![],
        )
        .unwrap();

        assert!(
            !dirty_overlay_dir(edges_dir, "p1").exists(),
            "dirty overlay must be cleared on clean checkout"
        );

        let manifest =
            WorkspaceManifest::read_from(&WorkspaceManifest::manifest_path(edges_dir, "p1"))
                .unwrap();
        assert!(!manifest.dirty);
        assert!(manifest.active_dirty_overlay_id.is_none());
    }

    #[test]
    fn inactive_snapshot_does_not_affect_manifest_index() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges_a = vec![derived_edge("k_a", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa",
            edges_a,
            vec![],
            vec![],
        )
        .unwrap();

        let edges_b = vec![derived_edge("k_b", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-b"),
            "sha_bbbb",
            edges_b,
            vec![],
            vec![],
        )
        .unwrap();

        let snap_a_id = clean_snapshot_id("repo1", "p1", "sha_aaaa");
        let snap_a_dir = snapshot_dir(edges_dir, "p1", &snap_a_id);
        assert!(
            snap_a_dir.is_dir(),
            "inactive snapshot must still exist on disk"
        );

        let idx = ManifestIndex::load(edges_dir).unwrap();
        let entry = &idx.workspaces["p1"];
        let active_snap = entry.active_snapshot.as_ref().unwrap();
        assert!(
            active_snap.contains("sha_bbbb"),
            "active snapshot must be branch-b, got: {active_snap}"
        );
    }

    #[test]
    fn switching_back_reuses_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges_a = vec![derived_edge("k_a", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa",
            edges_a.clone(),
            vec![],
            vec![],
        )
        .unwrap();

        let snap_a_content_before = {
            let snap_a_id = clean_snapshot_id("repo1", "p1", "sha_aaaa");
            let snap_a = snapshot_dir(edges_dir, "p1", &snap_a_id);
            fs::read_to_string(snap_a.join("project.jsonl")).unwrap()
        };

        let edges_b = vec![derived_edge("k_b", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-b"),
            "sha_bbbb",
            edges_b,
            vec![],
            vec![],
        )
        .unwrap();

        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa",
            edges_a,
            vec![],
            vec![],
        )
        .unwrap();

        let snap_a_id = clean_snapshot_id("repo1", "p1", "sha_aaaa");
        let snap_a_content_after = {
            let snap_a = snapshot_dir(edges_dir, "p1", &snap_a_id);
            fs::read_to_string(snap_a.join("project.jsonl")).unwrap()
        };
        assert_eq!(
            snap_a_content_before, snap_a_content_after,
            "switching back must reuse identical snapshot content"
        );
    }

    #[test]
    fn worktree_identity_without_git() {
        let dir = tempfile::tempdir().unwrap();
        let (project_id, repo_id) = worktree_identity(dir.path());
        assert!(!project_id.is_empty());
        assert!(repo_id.is_none(), "non-git dir should have no repo_id");
    }

    #[test]
    fn worktree_identity_with_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let (project_id, repo_id) = worktree_identity(dir.path());
        assert!(!project_id.is_empty());
        assert!(repo_id.is_some(), "git dir should yield repo_id");
    }

    #[test]
    fn dirty_overlay_empty_edges_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![derived_edge("k1", "DESCRIBES", "k2")];
        write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges)]).unwrap();
        assert!(dirty_overlay_dir(edges_dir, "p1").is_dir());

        write_dirty_overlay(edges_dir, "p1", &[]).unwrap();
        let overlay = dirty_overlay_dir(edges_dir, "p1");
        if overlay.is_dir() {
            let has_jsonl = fs::read_dir(&overlay)
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| e.path().extension().and_then(|e| e.to_str()) == Some("jsonl"));
            assert!(!has_jsonl, "empty overlay write must remove jsonl files");
        }
    }

    #[test]
    fn stale_temp_dir_does_not_leak_into_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let snap_dir = snapshot_dir(edges_dir, "p1", "snap-stale");
        let tmp_dir = snap_dir.with_extension("write-tmp");
        fs::create_dir_all(&tmp_dir).unwrap();
        fs::write(tmp_dir.join("stale.jsonl"), "should not persist").unwrap();

        let edges = vec![derived_edge("k_clean", "DESCRIBES", "k2")];
        write_snapshot_files(edges_dir, "p1", "snap-stale", &[("project.jsonl", &edges)]).unwrap();

        assert!(
            !snap_dir.join("stale.jsonl").exists(),
            "stale temp file must not leak into snapshot"
        );
        assert!(
            snap_dir.join("project.jsonl").exists(),
            "new snapshot files must exist"
        );
    }

    #[test]
    fn dirty_overlay_writes_multiple_lanes() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let proj = vec![derived_edge("k_proj", "DESCRIBES", "k2")];
        let sym = vec![derived_edge("k_sym", "HAS_SYMBOL", "k2")];
        let git = vec![derived_edge("k_git", "EDITED_FILE", "k2")];

        write_dirty_overlay(
            edges_dir,
            "p1",
            &[
                ("project.jsonl", &proj),
                ("symbols.jsonl", &sym),
                ("git-current.jsonl", &git),
            ],
        )
        .unwrap();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        let proj_content = fs::read_to_string(overlay.join("project.jsonl")).unwrap();
        let sym_content = fs::read_to_string(overlay.join("symbols.jsonl")).unwrap();
        let git_content = fs::read_to_string(overlay.join("git-current.jsonl")).unwrap();

        assert!(proj_content.contains("k_proj"));
        assert!(sym_content.contains("k_sym"));
        assert!(git_content.contains("k_git"));
    }

    #[test]
    fn switch_to_dirty_writes_all_lanes_to_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let proj = vec![derived_edge("k_proj", "DESCRIBES", "k2")];
        let sym = vec![derived_edge("k_sym", "HAS_SYMBOL", "k2")];
        let git = vec![derived_edge("k_git", "EDITED_FILE", "k2")];

        switch_to_dirty_overlay(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "abc123def456",
            "fp-dirty",
            proj,
            sym,
            git,
        )
        .unwrap();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        assert!(overlay.join("project.jsonl").exists());
        assert!(overlay.join("symbols.jsonl").exists());
        assert!(overlay.join("git-current.jsonl").exists());

        let proj_content = fs::read_to_string(overlay.join("project.jsonl")).unwrap();
        assert!(proj_content.contains("k_proj"));
        let sym_content = fs::read_to_string(overlay.join("symbols.jsonl")).unwrap();
        assert!(sym_content.contains("k_sym"));
    }

    #[test]
    fn active_loader_overlay_suppresses_clean_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let snap_edges = vec![derived_edge("k_snap_only", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha_aaaa",
            snap_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let dirty_edges = vec![derived_edge("k_dirty", "DESCRIBES", "k2")];
        switch_to_dirty_overlay(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha_aaaa",
            "fp-dirty",
            dirty_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let idx = ManifestIndex::load(edges_dir).unwrap();
        let paths = idx.active_materialized_paths(edges_dir);

        let has_overlay = paths
            .iter()
            .any(|p| p.to_str().unwrap_or_default().contains("dirty-current"));
        let has_snap = paths.iter().any(|p| {
            p.to_str()
                .unwrap_or_default()
                .contains("snapshots/head-sha_aaaa")
        });
        let snap_edge_in_paths = paths.iter().any(|p| {
            fs::read_to_string(p)
                .unwrap_or_default()
                .contains("k_snap_only")
        });
        assert!(has_overlay, "active loader must include dirty overlay");
        assert!(
            !has_snap,
            "whole-workspace dirty overlay must suppress clean snapshot paths"
        );
        assert!(
            !snap_edge_in_paths,
            "clean-snapshot-only edge must not appear in active paths"
        );
    }

    #[test]
    fn clean_checkout_restores_clean_snapshot_in_active_paths() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let snap_edges = vec![derived_edge("k_snap_only", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha_aaaa",
            snap_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let dirty_edges = vec![derived_edge("k_dirty", "DESCRIBES", "k2")];
        switch_to_dirty_overlay(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha_aaaa",
            "fp-dirty",
            dirty_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let clean_edges = vec![derived_edge("k_snap_only", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha_aaaa",
            clean_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let idx = ManifestIndex::load(edges_dir).unwrap();
        let paths = idx.active_materialized_paths(edges_dir);

        let has_overlay = paths
            .iter()
            .any(|p| p.to_str().unwrap_or_default().contains("dirty-current"));
        let has_snap = paths.iter().any(|p| {
            p.to_str()
                .unwrap_or_default()
                .contains("snapshots/head-sha_aaaa")
        });
        assert!(
            !has_overlay,
            "clean checkout must remove dirty overlay from active paths"
        );
        assert!(
            has_snap,
            "clean checkout must restore clean snapshot in active paths"
        );
    }

    #[test]
    fn branch_switch_changes_active_graph_via_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges_a = vec![derived_edge("k_branch_a", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa",
            edges_a,
            vec![],
            vec![],
        )
        .unwrap();

        let idx_a = ManifestIndex::load(edges_dir).unwrap();
        let paths_a = idx_a.active_materialized_paths(edges_dir);
        let has_a = paths_a.iter().any(|p| {
            let content = fs::read_to_string(p).unwrap_or_default();
            content.contains("k_branch_a")
        });
        assert!(has_a, "branch-a active graph must have branch-a edges");

        let edges_b = vec![derived_edge("k_branch_b", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-b"),
            "sha_bbbb",
            edges_b,
            vec![],
            vec![],
        )
        .unwrap();

        let idx_b = ManifestIndex::load(edges_dir).unwrap();
        let paths_b = idx_b.active_materialized_paths(edges_dir);
        let has_b = paths_b.iter().any(|p| {
            let content = fs::read_to_string(p).unwrap_or_default();
            content.contains("k_branch_b")
        });
        let has_a_in_b = paths_b.iter().any(|p| {
            let content = fs::read_to_string(p).unwrap_or_default();
            content.contains("k_branch_a")
        });
        assert!(has_b, "branch-b active graph must have branch-b edges");
        assert!(
            !has_a_in_b,
            "branch-b active graph must NOT have branch-a edges"
        );
    }
}
