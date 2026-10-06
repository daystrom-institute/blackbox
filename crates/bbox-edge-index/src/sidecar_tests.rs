//! Persistence tests for the edge sidecar lanes, snapshot GC and storage
//! health. They exercise the writers in `bbox-edge-sidecar` together with
//! the storage-health planner in this crate.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_edge_sidecar::edge_sidecar::*;

#[test]
fn purge_managed_edges_removes_only_deleted_file_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let edges_dir = dir.path();
    let proj = "projpurge";

    let keep_file = EntityRef::ProjectFile {
        project_id: proj.into(),
        rel_path_hash: "keephash".into(),
        chunk_hash: "a".repeat(64),
        occurrence_idx: 0,
    };
    let del_file = EntityRef::ProjectFile {
        project_id: proj.into(),
        rel_path_hash: "delhash".into(),
        chunk_hash: "b".repeat(64),
        occurrence_idx: 0,
    };
    let sym_a = EntityRef::SymbolV2 {
        project_id: proj.into(),
        snapshot_id: "snap".into(),
        qualified_name: "pkg.A".into(),
        defn_hash: "c".repeat(64),
    };
    let sym_b = EntityRef::SymbolV2 {
        project_id: proj.into(),
        snapshot_id: "snap".into(),
        qualified_name: "pkg.B".into(),
        defn_hash: "d".repeat(64),
    };
    let mk = |s: EntityRef, k: &str, t: EntityRef| bbox_chunker::Edge {
        source: s,
        kind: k.into(),
        target: t,
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
    };
    let edges = vec![
        mk(keep_file.clone(), "NEXT_SECTION", keep_file.clone()),
        mk(del_file.clone(), "NEXT_SECTION", del_file.clone()),
        // symbol→symbol edge: carries no project-file ref, so it is retained.
        mk(sym_a.clone(), "CALLS", sym_b.clone()),
    ];
    replace_project_edges(edges_dir, "project", proj, &edges).unwrap();

    let mut stale = HashSet::new();
    stale.insert("delhash".to_string());
    let purged = purge_managed_edges_for_path_hashes(edges_dir, "project", proj, &stale).unwrap();
    assert_eq!(purged, 1, "only the deleted file's edge is removed");

    let remaining = read_managed_derived_edges(edges_dir, "project", proj).unwrap();
    assert_eq!(remaining.len(), 2);
    assert!(
        remaining.iter().any(|e| e.source == keep_file),
        "kept file's edge retained"
    );
    assert!(
        remaining.iter().any(|e| e.source == sym_a),
        "symbol→symbol edge retained (no file ref)"
    );
    assert!(
        !remaining.iter().any(|e| e.source == del_file),
        "deleted file's edge purged"
    );

    // Empty stale set is a no-op.
    assert_eq!(
        purge_managed_edges_for_path_hashes(edges_dir, "project", proj, &HashSet::new()).unwrap(),
        0
    );
}

#[test]
fn line_provenance_is_derived_matches_exact_serialized_forms() {
    assert!(
        line_provenance_is_derived(
            r#"{"source":"k:abc","kind":"DESCRIBES","target":"k:def","provenance":"derived","confidence":"exact"}"#
        ),
        "compact JSON with derived provenance"
    );
    assert!(
        line_provenance_is_derived(
            r#"{"source":"k:abc", "kind":"DESCRIBES", "target":"k:def", "provenance": "derived", "confidence":"exact"}"#
        ),
        "JSON with spaces around colon/value for provenance"
    );
    assert!(
        !line_provenance_is_derived(
            r#"{"source":"k:abc","kind":"DESCRIBES","target":"k:def","provenance":"explicit","confidence":"exact"}"#
        ),
        "explicit provenance must not match"
    );
    assert!(
        !line_provenance_is_derived(
            r#"{"source":"k:abc","kind":"DESCRIBES","target":"k:def","provenance":"implicit","confidence":"exact"}"#
        ),
        "implicit provenance must not match"
    );
    assert!(
        !line_provenance_is_derived("not valid json at all"),
        "malformed line must not match"
    );
    assert!(
        !line_provenance_is_derived("{\"provenance\":\"derivedly_wrong\"}"),
        "substring that is not exact value must not match"
    );
    assert!(
        !line_provenance_is_derived(
            "{\"source\":\"k:abc\",\"kind\":\"DESCRIBES\",\"target\":\"k:def\",\"provenance\":\"explicit\",\"confidence\":\"exact\",\"metadata\":{\"nested\":\"provenance\\\":\\\"derived\\\"\"}"
        ),
        "explicit top-level with derived-like substring in metadata must not false-skip"
    );
    assert!(
        !line_provenance_is_derived("no provenance field at all"),
        "line without provenance key must not match"
    );
}

#[test]
fn compact_legacy_sidecar_removes_only_derived_edges() {
    let dir = tempfile::tempdir().unwrap();
    let source = EntityRef::ProjectFile {
        project_id: "proj1234".into(),
        rel_path_hash: "pathhash".into(),
        chunk_hash: "a".repeat(64),
        occurrence_idx: 0,
    };
    let derived = bbox_chunker::Edge {
        source: source.clone(),
        kind: "NEXT_SECTION".into(),
        target: EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 1,
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
    };
    append_project_edges(dir.path(), "proj1234", &[derived]).unwrap();
    let explicit = Edge {
        source: EntityRef::Transcript {
            provider: "claude".into(),
            session_id: "sess-1".into(),
            line_offset: 42,
            event_idx: 0,
        },
        kind: "RAN_BASH".into(),
        target: source.clone(),
        provenance: EdgeProvenance::Explicit,
        confidence: EdgeConfidence::Heuristic,
        metadata: BTreeMap::new(),
        project_id: None,
    };
    append_edges(dir.path(), "proj1234", &[explicit]).unwrap();

    let dry_run = compact_legacy_sidecar(dir.path(), "proj1234", false).unwrap();
    assert!(!dry_run.applied);
    assert_eq!(dry_run.derived_edges_removed, 1);
    assert_eq!(dry_run.explicit_edges_retained, 1);

    let applied = compact_legacy_sidecar(dir.path(), "proj1234", true).unwrap();
    assert!(applied.applied);
    assert_eq!(applied.derived_edges_removed, 1);
    assert!(applied.backup_path.is_some());

    let compacted = fs::read_to_string(dir.path().join("proj1234.jsonl")).unwrap();
    assert_eq!(compacted.lines().count(), 1);
    assert!(compacted.contains("RAN_BASH"));
    assert!(!compacted.contains("NEXT_SECTION"));
}

#[test]
fn append_edges_dedup_skips_reimported_edges() {
    let dir = tempfile::tempdir().unwrap();
    let edge = Edge {
        source: EntityRef::Transcript {
            provider: "claude".into(),
            session_id: "sess-1".into(),
            line_offset: 42,
            event_idx: 0,
        },
        kind: "RAN_BASH".into(),
        target: EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        },
        provenance: EdgeProvenance::Explicit,
        confidence: EdgeConfidence::Heuristic,
        metadata: BTreeMap::from([("tool.id".into(), "tool-1".into())]),
        project_id: None,
    };

    assert_eq!(
        append_edges_dedup(dir.path(), "proj1234", std::slice::from_ref(&edge)).unwrap(),
        1
    );
    assert_eq!(
        append_edges_dedup(dir.path(), "proj1234", std::slice::from_ref(&edge)).unwrap(),
        0
    );
    let sidecar = fs::read_to_string(dir.path().join("proj1234.jsonl")).unwrap();
    assert_eq!(sidecar.lines().count(), 1);
}

// -----------------------------------------------------------------------
// Phase 2 tests
// -----------------------------------------------------------------------

fn derived_chunker_edge(kind: &str) -> bbox_chunker::Edge {
    bbox_chunker::Edge {
        source: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "h1".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        },
        kind: kind.into(),
        target: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "h2".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 0,
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
    }
}

#[test]
fn replace_materialized_replaces_not_appends() {
    let dir = tempfile::tempdir().unwrap();
    let first = derived_chunker_edge("CALLS");
    let second = derived_chunker_edge("USES_TYPE");

    replace_materialized_edges(dir.path(), "project", "p1", &[first]).unwrap();
    let sidecar_path = dir.path().join("derived").join("project").join("p1.jsonl");
    let content1 = fs::read_to_string(&sidecar_path).unwrap();
    assert_eq!(content1.lines().count(), 1);

    replace_materialized_edges(dir.path(), "project", "p1", &[second.clone(), second]).unwrap();
    let content2 = fs::read_to_string(&sidecar_path).unwrap();
    assert_eq!(
        content2.lines().count(),
        1,
        "managed replacement is a set and must not persist duplicates"
    );
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "rejected non-Derived edge")]
fn replace_materialized_rejects_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = derived_chunker_edge("CALLS");
    e.provenance = EdgeProvenance::Explicit;
    let _ = replace_materialized_edges(dir.path(), "project", "p1", &[e]);
}

#[test]
fn repeated_materialized_replace_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let edge = derived_chunker_edge("CALLS");

    for _ in 0..5 {
        replace_materialized_edges(dir.path(), "project", "p1", std::slice::from_ref(&edge))
            .unwrap();
    }

    let sidecar_path = dir.path().join("derived").join("project").join("p1.jsonl");
    let content = fs::read_to_string(&sidecar_path).unwrap();
    assert_eq!(
        content.lines().count(),
        1,
        "repeated replacement must not grow line count"
    );
}

#[test]
fn incremental_materialized_replace_stream_deduplicates_preserved_rows() {
    let dir = tempfile::tempdir().unwrap();
    let edges_dir = dir.path();
    let managed = managed_derived_edges_dir(edges_dir).join("project");
    fs::create_dir_all(&managed).unwrap();

    let preserved = Edge {
        source: EntityRef::Knowledge { id: "old".into() },
        kind: "RELATES_TO".into(),
        target: EntityRef::Knowledge {
            id: "target".into(),
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
        metadata: Default::default(),
        project_id: None,
    };
    let serialized = serde_json::to_string(&preserved).unwrap();
    fs::write(
        managed.join("p1.jsonl"),
        format!("{serialized}\n{serialized}\n{serialized}\n"),
    )
    .unwrap();

    let new_edge = derived_chunker_edge("CALLS");
    replace_materialized_edges_incremental(edges_dir, "project", "p1", &[new_edge]).unwrap();

    let content = fs::read_to_string(managed.join("p1.jsonl")).unwrap();
    assert_eq!(
        content.lines().count(),
        2,
        "three duplicate preserved rows collapse to one beside the new edge"
    );
}

#[test]
fn incremental_materialized_replace_preserves_unchanged_file_edges() {
    let dir = tempfile::tempdir().unwrap();
    let edges_dir = dir.path();

    let file_a = bbox_chunker::Edge {
        source: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "aaa".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        },
        kind: "NEXT_SECTION".into(),
        target: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "aaa".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 0,
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
    };
    let file_b = bbox_chunker::Edge {
        source: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "bbb".into(),
            chunk_hash: "c".repeat(64),
            occurrence_idx: 0,
        },
        kind: "NEXT_SECTION".into(),
        target: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "bbb".into(),
            chunk_hash: "d".repeat(64),
            occurrence_idx: 0,
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
    };

    replace_materialized_edges(
        edges_dir,
        "project",
        "p1",
        &[file_a.clone(), file_b.clone()],
    )
    .unwrap();

    let after_full = read_managed_derived_edges(edges_dir, "project", "p1").unwrap();
    assert_eq!(after_full.len(), 2);

    let file_a_updated = bbox_chunker::Edge {
        source: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "aaa".into(),
            chunk_hash: "e".repeat(64),
            occurrence_idx: 0,
        },
        kind: "NEXT_SECTION".into(),
        target: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "aaa".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 0,
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
    };

    replace_materialized_edges_incremental(edges_dir, "project", "p1", &[file_a_updated]).unwrap();

    let after_incremental = read_managed_derived_edges(edges_dir, "project", "p1").unwrap();
    assert_eq!(after_incremental.len(), 2, "total edges must stay at 2");

    let b_edges: Vec<_> = after_incremental
        .iter()
        .filter(|e| match &e.source {
            EntityRef::ProjectFile { rel_path_hash, .. } => rel_path_hash == "bbb",
            _ => false,
        })
        .collect();
    assert_eq!(b_edges.len(), 1, "unchanged file-b edge must be preserved");

    let a_edges: Vec<_> = after_incremental
        .iter()
        .filter(|e| match &e.source {
            EntityRef::ProjectFile { rel_path_hash, .. } => rel_path_hash == "aaa",
            _ => false,
        })
        .collect();
    assert_eq!(a_edges.len(), 1, "updated file-a edge must be present");
    assert_eq!(a_edges[0].kind, "NEXT_SECTION");
}

#[test]
fn incremental_materialized_replace_no_duplicates_on_repeat() {
    let dir = tempfile::tempdir().unwrap();
    let edge = bbox_chunker::Edge {
        source: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "xxx".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        },
        kind: "NEXT_SECTION".into(),
        target: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "xxx".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 0,
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
    };

    replace_materialized_edges_incremental(
        dir.path(),
        "project",
        "p1",
        std::slice::from_ref(&edge),
    )
    .unwrap();
    replace_materialized_edges_incremental(dir.path(), "project", "p1", &[edge]).unwrap();

    let after = read_managed_derived_edges(dir.path(), "project", "p1").unwrap();
    assert_eq!(
        after.len(),
        1,
        "re-incremental with same edges must not duplicate"
    );
}

#[test]
fn merge_materialized_git_preserves_old_commits_and_appends_new() {
    let dir = tempfile::tempdir().unwrap();

    let old_commit_edge = bbox_chunker::Edge {
        source: EntityRef::Commit {
            repo_id: "repo1".into(),
            sha: "aaaaaa".into(),
        },
        kind: "COMMIT_EDITED_FILE".into(),
        target: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "fff".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
    };
    let new_commit_edge = bbox_chunker::Edge {
        source: EntityRef::Commit {
            repo_id: "repo1".into(),
            sha: "bbbbbb".into(),
        },
        kind: "COMMIT_EDITED_FILE".into(),
        target: EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "fff".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
    };

    replace_materialized_edges(
        dir.path(),
        "git",
        "p1",
        std::slice::from_ref(&old_commit_edge),
    )
    .unwrap();
    let after_full = read_managed_derived_edges(dir.path(), "git", "p1").unwrap();
    assert_eq!(after_full.len(), 1);

    merge_materialized_edges(
        dir.path(),
        "git",
        "p1",
        std::slice::from_ref(&new_commit_edge),
    )
    .unwrap();
    let after_merge = read_managed_derived_edges(dir.path(), "git", "p1").unwrap();
    assert_eq!(after_merge.len(), 2, "old + new commit edges");

    let has_old = after_merge.iter().any(|e| match &e.source {
        EntityRef::Commit { sha, .. } => sha == "aaaaaa",
        _ => false,
    });
    let has_new = after_merge.iter().any(|e| match &e.source {
        EntityRef::Commit { sha, .. } => sha == "bbbbbb",
        _ => false,
    });
    assert!(has_old, "old commit edge must be preserved");
    assert!(has_new, "new commit edge must be appended");
}

// -----------------------------------------------------------------------
// Snapshot GC and storage health over manifest-selected materializations
// -----------------------------------------------------------------------

mod cross_phase {
    use super::*;
    use crate::storage_health::{
        GcParams, GcPolicy, SnapshotRetentionPolicy, plan_gc_with_policy, scan_storage_health,
    };
    use bbox_edge_sidecar::snapshot::{
        clean_snapshot_id, switch_to_clean_snapshot, switch_to_dirty_overlay,
    };

    fn derived_edge(source: &str, kind: &str, target: &str) -> Edge {
        Edge {
            source: EntityRef::Knowledge { id: source.into() },
            kind: kind.into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
            project_id: None,
        }
    }

    fn setup_branch_snapshot(
        edges_dir: &Path,
        project_id: &str,
        repo_id: &str,
        branch: &str,
        head_sha: &str,
        edges: Vec<Edge>,
    ) {
        let empty: Vec<Edge> = Vec::new();
        switch_to_clean_snapshot(
            edges_dir,
            project_id,
            repo_id,
            Some(branch),
            head_sha,
            edges,
            empty.clone(),
            empty,
        )
        .unwrap();
    }

    #[test]
    fn gc_retains_active_snapshot_and_dirty_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        let project_id = "p1";
        let repo_id = "repo_abc";
        let head_sha = "aaaa1111bbbb";

        let clean_edges = vec![derived_edge("sym_clean", "DESCRIBES", "target_clean")];
        setup_branch_snapshot(
            edges_dir,
            project_id,
            repo_id,
            "main",
            head_sha,
            clean_edges,
        );

        let dirty_edges = vec![derived_edge("sym_dirty", "DESCRIBES", "target_dirty")];
        let empty: Vec<Edge> = Vec::new();
        switch_to_dirty_overlay(
            edges_dir,
            project_id,
            repo_id,
            Some("main"),
            head_sha,
            "fingerprint1",
            dirty_edges,
            empty.clone(),
            empty,
        )
        .unwrap();

        let registered: HashSet<String> = [project_id.to_string()].into_iter().collect();
        let policy = GcPolicy {
            materialized_snapshots: SnapshotRetentionPolicy {
                keep_active: true,
                keep_recent_per_workspace: 0,
                keep_recent_per_repo: 0,
                branch_switch_grace_minutes: 0,
                max_age_days: None,
                max_count_per_workspace: None,
                max_total_bytes_per_workspace: None,
            },
            ..Default::default()
        };

        let candidates = plan_gc_with_policy(
            edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: true,
                prune_orphans: false,
                prune_temps: true,
                prune_inactive_snapshots: true,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
            &policy,
        )
        .unwrap();

        let overlay_path = format!("workspace/{}/dirty-current", project_id);
        let active_snap_id = clean_snapshot_id(repo_id, project_id, head_sha);
        let snap_path = format!("workspace/{}/snapshots/{}", project_id, active_snap_id);

        for candidate in &candidates {
            assert!(
                !candidate.path.contains(&overlay_path),
                "dirty overlay must not be a GC candidate: {}",
                candidate.path
            );
            assert!(
                !candidate.path.contains(&snap_path),
                "active snapshot must not be a GC candidate even when overlay wins: {}",
                candidate.path
            );
        }

        let inactive_candidates: Vec<_> = candidates
            .iter()
            .filter(|c| c.rule == "inactive_snapshot")
            .collect();
        assert!(
            inactive_candidates.is_empty(),
            "active snapshot + dirty overlay must both be protected, no inactive_snapshot candidates"
        );
    }

    #[test]
    fn gc_prunes_inactive_snapshots_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        let project_id = "p1";
        let repo_id = "repo_abc";

        let sha_a = "aaaa1111bbbb";
        let sha_b = "bbbb2222cccc";

        setup_branch_snapshot(
            edges_dir,
            project_id,
            repo_id,
            "main",
            sha_a,
            vec![derived_edge("sym_a", "DESCRIBES", "t")],
        );
        setup_branch_snapshot(
            edges_dir,
            project_id,
            repo_id,
            "feature",
            sha_b,
            vec![derived_edge("sym_b", "DESCRIBES", "t")],
        );

        setup_branch_snapshot(
            edges_dir,
            project_id,
            repo_id,
            "main",
            sha_a,
            vec![derived_edge("sym_a", "DESCRIBES", "t")],
        );

        let registered: HashSet<String> = [project_id.to_string()].into_iter().collect();
        let policy = GcPolicy {
            materialized_snapshots: SnapshotRetentionPolicy {
                keep_active: true,
                keep_recent_per_workspace: 0,
                keep_recent_per_repo: 0,
                branch_switch_grace_minutes: 0,
                max_age_days: None,
                max_count_per_workspace: None,
                max_total_bytes_per_workspace: None,
            },
            ..Default::default()
        };
        let candidates = plan_gc_with_policy(
            edges_dir,
            &registered,
            &GcParams {
                dry_run: true,
                project_filter: None,
                prune_backups: true,
                prune_orphans: false,
                prune_temps: false,
                prune_inactive_snapshots: true,
                max_backup_age_days: None,
                keep_newest_backup_per_source: 1,
            },
            &policy,
        )
        .unwrap();

        let inactive_snap_id = clean_snapshot_id(repo_id, project_id, sha_b);
        let inactive_path = format!("workspace/{}/snapshots/{}", project_id, inactive_snap_id);

        let inactive_candidate = candidates.iter().find(|c| {
            c.path.contains(&inactive_path)
                && c.rule.starts_with("snapshot_prunable")
                && c.deletable
        });
        assert!(
            inactive_candidate.is_some(),
            "inactive branch B snapshot must be a GC candidate: {:?}",
            candidates
                .iter()
                .filter(|c| c.rule.starts_with("snapshot_"))
                .map(|c| (&c.rule, &c.path, c.deletable))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn storage_health_reports_active_inactive_post_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        let project_id = "p1";
        let repo_id = "repo_abc";

        let sha_a = "aaaa1111bbbb";
        let sha_b = "bbbb2222cccc";

        let big_edge = derived_edge("sym_a", "DESCRIBES", "target_a");

        setup_branch_snapshot(
            edges_dir,
            project_id,
            repo_id,
            "main",
            sha_a,
            vec![big_edge.clone()],
        );
        setup_branch_snapshot(
            edges_dir,
            project_id,
            repo_id,
            "feature",
            sha_b,
            vec![derived_edge("sym_b", "DESCRIBES", "target_b")],
        );

        setup_branch_snapshot(
            edges_dir,
            project_id,
            repo_id,
            "main",
            sha_a,
            vec![big_edge],
        );

        let registered: HashSet<String> = [project_id.to_string()].into_iter().collect();
        let report = scan_storage_health(edges_dir, &registered, None, false).unwrap();

        let ms = report.manifest_status.expect("manifest must exist");
        assert!(
            ms.active_materialized_bytes > 0,
            "active bytes must be nonzero: {}",
            ms.active_materialized_bytes
        );
        assert!(
            ms.active_materialized_files > 0,
            "active files must be nonzero: {}",
            ms.active_materialized_files
        );
        assert!(
            ms.inactive_materialized_bytes > 0,
            "inactive bytes must be nonzero (branch B snapshot): {}",
            ms.inactive_materialized_bytes
        );
        assert!(
            ms.inactive_materialized_files > 0,
            "inactive files must be nonzero (branch B snapshot): {}",
            ms.inactive_materialized_files
        );
    }
}

// -------------------------------------------------------------------------
// Focused tests for active/historical mode, per-file overlay, and observed
// -------------------------------------------------------------------------
