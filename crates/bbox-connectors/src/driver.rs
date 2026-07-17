//! The shared sync driver: given a mount and a connector, drive
//! `list_changes`/`fetch`, materialize into the root with atomic writes,
//! reconcile renames as moves, record deletions, and produce a
//! [`SyncSummary`]. Connectors only enumerate and fetch (or, for
//! `bulk_materializes()` connectors, materialize their own tree) - every
//! path-safety, policy, and durability concern lives here so it applies
//! uniformly to every connector.
//!
//! Cursor-advance rule: the driver only advances the mount's cursor to the
//! connector-reported `next_cursor` when the batch had zero per-entry
//! errors. A batch with errors returns the ORIGINAL cursor unchanged, so
//! the next pass re-lists from the same point and retries the failed
//! entries - already-materialized entries skip re-fetch via the
//! remote_version match, so this is cheap, not a full re-fetch.

use std::path::Path;

use anyhow::{Context, Result};

use crate::connector::RemoteSourceConnector;
use crate::manifest::{EntryState, Manifest, ManifestEntry, encode_physical_path, join_contained};
use crate::materialize::{
    atomic_write, ensure_ownership_marker, remove_materialized_file, rename_materialized_file,
    sha256_hex,
};
use crate::policy::{PolicyDecision, TotalBytesBudget};
use crate::types::{ChangeCursor, MountConfig, RemoteChangeKind, RemoteEntry};

/// Outcome of one sync pass. Degradations (an expired cursor forcing a full
/// walk, a rename reported with no prior manifest record, ...) are always
/// reported here, never silently absorbed.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncSummary {
    pub fetched: u64,
    pub skipped: u64,
    pub deleted: u64,
    pub renamed: u64,
    /// Per-entry failure messages. Non-empty means the cursor was NOT
    /// advanced this pass (see module docs).
    pub errors: Vec<String>,
    pub degradations: Vec<String>,
    /// The cursor to persist on `MountRecord` after this call returns.
    pub next_cursor: Option<ChangeCursor>,
}

/// Drive one sync pass for `mount` using `connector`, loading and
/// persisting the manifest at `manifest_path`. `cursor` is the mount's
/// last-committed cursor (`None` = full walk).
pub async fn sync_mount(
    connector: &dyn RemoteSourceConnector,
    mount: &MountConfig,
    manifest_path: &Path,
    cursor: Option<ChangeCursor>,
) -> Result<SyncSummary> {
    ensure_ownership_marker(&mount.materialization_root)
        .context("ensuring materialization root ownership marker")?;
    let mut manifest = Manifest::open(manifest_path).context("opening manifest")?;

    let batch = connector
        .list_changes(mount, cursor.clone())
        .await
        .context("listing remote changes")?;

    let mut summary = SyncSummary::default();
    if batch.degraded_to_full_walk {
        let message =
            "connector could not resume from the supplied cursor and degraded to a full walk"
                .to_string();
        tracing::warn!(connector = connector.kind(), "{message}");
        summary.degradations.push(message);
    }

    let mut budget = TotalBytesBudget::new(mount.policy.max_total_bytes);

    if connector.bulk_materializes() {
        let outcome = connector
            .materialize_bulk(mount, &mount.materialization_root, &manifest, &batch)
            .await
            .context("bulk materialize failed")?;

        for remote_id in &outcome.deleted_remote_ids {
            if manifest.remove_by_remote_id(remote_id).is_some() {
                summary.deleted += 1;
            }
        }

        for mut upsert in outcome.upserts {
            let was_rename = manifest
                .get(&upsert.remote_id)
                .is_some_and(|existing| existing.logical_path != upsert.logical_path);

            let physical = join_contained(&mount.materialization_root, &upsert.physical_path)?;
            let on_disk_size = if matches!(upsert.state, EntryState::Materialized) {
                crate::materialize::file_size(&physical).ok()
            } else {
                None
            };

            match mount.policy.evaluate(&upsert.logical_path, on_disk_size) {
                PolicyDecision::Exclude(reason) => {
                    remove_materialized_file(&physical)?;
                    upsert.state = EntryState::Skipped { reason };
                    upsert.content_hash = None;
                    summary.skipped += 1;
                }
                PolicyDecision::Include => {
                    if let Some(size) = on_disk_size {
                        // Loud-abort semantics match the generic path, with
                        // an honest caveat: the connector already wrote
                        // these bytes via its own checkout mechanics before
                        // policy could intervene (documented in
                        // git_connector.rs / AGENTS.md).
                        budget.charge(&upsert.logical_path, size)?;
                    }
                    if was_rename {
                        summary.renamed += 1;
                    } else if matches!(upsert.state, EntryState::Materialized) {
                        summary.fetched += 1;
                    }
                }
            }
            manifest.upsert(upsert);
        }
        manifest.save(manifest_path)?;
    } else {
        for change in &batch.changes {
            match &change.kind {
                RemoteChangeKind::Deleted => {
                    if let Some(existing) = manifest.remove_by_remote_id(&change.entry.remote_id) {
                        let physical =
                            join_contained(&mount.materialization_root, &existing.physical_path)?;
                        remove_materialized_file(&physical)?;
                        summary.deleted += 1;
                    }
                }
                RemoteChangeKind::Renamed { from_path } => {
                    let normalized = reconcile_rename(
                        &mut manifest,
                        mount,
                        from_path,
                        &change.entry,
                        &mut summary,
                    )?;
                    // Reuses the resume-without-refetch check in
                    // `process_content_change_inner`: when the rename
                    // carried no content change (remote_version matches
                    // what reconciliation just wrote), this is a no-op.
                    process_content_change(
                        connector,
                        mount,
                        &mut manifest,
                        &normalized,
                        &mut budget,
                        &mut summary,
                    )
                    .await?;
                }
                RemoteChangeKind::Added | RemoteChangeKind::Modified => {
                    process_content_change(
                        connector,
                        mount,
                        &mut manifest,
                        &change.entry,
                        &mut budget,
                        &mut summary,
                    )
                    .await?;
                }
            }
            // Per-entry durability: a crash after this point resumes
            // without re-fetching everything already committed above.
            manifest.save(manifest_path)?;
        }
        manifest.save(manifest_path)?;
    }

    summary.next_cursor = if summary.errors.is_empty() {
        batch.next_cursor
    } else {
        cursor
    };

    Ok(summary)
}

/// Reconcile a reported rename as a manifest move (same remote_id, new
/// logical/physical path) instead of delete+refetch. When the connector
/// reports a rename with no prior manifest record (unexpected - a
/// degradation, not a hard error), this records that degradation and lets
/// the caller's subsequent `process_content_change` materialize the new
/// path as if it were freshly added.
/// Returns a normalized [`RemoteEntry`] (the connector's reported entry
/// with `remote_id` swapped to the reconciled identity) for the caller to
/// feed into `process_content_change` - that call's resume-without-refetch
/// check then naturally decides whether the rename also carried a content
/// change (remote_version differs) or was a pure move (no fetch needed).
fn reconcile_rename(
    manifest: &mut Manifest,
    mount: &MountConfig,
    from_path: &str,
    new_entry: &RemoteEntry,
    summary: &mut SyncSummary,
) -> Result<RemoteEntry> {
    let Some(existing) = manifest.find_by_logical_path(from_path).cloned() else {
        let message = format!(
            "rename reported from `{from_path}` to `{}` but no manifest entry was found for \
             the prior path; materializing `{}` as new",
            new_entry.path, new_entry.path
        );
        tracing::warn!("{message}");
        summary.degradations.push(message);
        return Ok(new_entry.clone());
    };

    let remote_id = existing.remote_id.clone();
    manifest.remove_by_remote_id(&remote_id);
    let new_physical = encode_physical_path(&new_entry.path, &remote_id, manifest)?;
    let from_physical = join_contained(&mount.materialization_root, &existing.physical_path)?;
    let to_physical = join_contained(&mount.materialization_root, &new_physical)?;

    if matches!(existing.state, EntryState::Materialized) && from_physical.exists() {
        rename_materialized_file(&from_physical, &to_physical)?;
    }

    // remote_version is deliberately left at the OLD value here (not
    // new_entry's) - the caller's resume-without-refetch check compares it
    // against the normalized entry's remote_version to decide whether the
    // rename also carried a content change.
    manifest.upsert(ManifestEntry {
        remote_id: remote_id.clone(),
        logical_path: new_entry.path.clone(),
        physical_path: new_physical,
        ..existing
    });
    summary.renamed += 1;
    Ok(RemoteEntry {
        remote_id,
        path: new_entry.path.clone(),
        size: new_entry.size,
        remote_version: new_entry.remote_version.clone(),
        kind: new_entry.kind,
        media_type: new_entry.media_type.clone(),
    })
}

/// Fetch (subject to resume-without-refetch and policy), write, and record
/// one Added/Modified entry. A [`crate::policy::BudgetExceeded`] cause
/// aborts the whole pass (propagated as `Err`) - every other failure is
/// recorded in `summary.errors` and the loop continues. See module docs for
/// the cursor consequence of a recorded error.
async fn process_content_change(
    connector: &dyn RemoteSourceConnector,
    mount: &MountConfig,
    manifest: &mut Manifest,
    entry: &RemoteEntry,
    budget: &mut TotalBytesBudget,
    summary: &mut SyncSummary,
) -> Result<()> {
    if let Err(err) =
        process_content_change_inner(connector, mount, manifest, entry, budget, summary).await
    {
        if err
            .downcast_ref::<crate::policy::BudgetExceeded>()
            .is_some()
        {
            return Err(err);
        }
        let message = format!("{}: {err:#}", entry.path);
        tracing::warn!(connector = connector.kind(), path = %entry.path, error = %err, "entry fetch failed");
        summary.errors.push(message);
    }
    Ok(())
}

async fn process_content_change_inner(
    connector: &dyn RemoteSourceConnector,
    mount: &MountConfig,
    manifest: &mut Manifest,
    entry: &RemoteEntry,
    budget: &mut TotalBytesBudget,
    summary: &mut SyncSummary,
) -> Result<()> {
    match mount.policy.evaluate(&entry.path, entry.size) {
        PolicyDecision::Exclude(reason) => {
            manifest.upsert(ManifestEntry {
                remote_id: entry.remote_id.clone(),
                remote_version: entry.remote_version.clone(),
                logical_path: entry.path.clone(),
                physical_path: encode_physical_path(&entry.path, &entry.remote_id, manifest)?,
                export_format: None,
                content_hash: None,
                state: EntryState::Skipped { reason },
            });
            summary.skipped += 1;
            return Ok(());
        }
        PolicyDecision::Include => {}
    }

    // Resume-without-refetch: an already-materialized entry whose
    // remote_version has not moved needs no network call at all. Neither
    // fetched nor skipped - a true no-op for this pass.
    if let Some(existing) = manifest.get(&entry.remote_id)
        && matches!(existing.state, EntryState::Materialized)
        && existing.remote_version == entry.remote_version
    {
        return Ok(());
    }

    let fetched = connector.fetch(mount, entry).await.context("fetch")?;

    // v1 approximation of a true streamed hard cap (design's honest
    // wrinkle: native-doc export sizes are unknown before fetching): once
    // bytes are in hand, an oversized result is discarded rather than
    // written, and recorded as skipped instead of materialized.
    if fetched.bytes.len() as u64 > mount.policy.max_file_bytes {
        manifest.upsert(ManifestEntry {
            remote_id: entry.remote_id.clone(),
            remote_version: entry.remote_version.clone(),
            logical_path: entry.path.clone(),
            physical_path: encode_physical_path(&entry.path, &entry.remote_id, manifest)?,
            export_format: fetched.export_format.clone(),
            content_hash: None,
            state: EntryState::Skipped {
                reason: format!(
                    "fetched {} bytes, over the {} byte per-file cap (size was unknown before fetch)",
                    fetched.bytes.len(),
                    mount.policy.max_file_bytes
                ),
            },
        });
        summary.skipped += 1;
        return Ok(());
    }

    budget.charge(&entry.path, fetched.bytes.len() as u64)?;

    let physical_path = encode_physical_path(&entry.path, &entry.remote_id, manifest)?;
    let physical = join_contained(&mount.materialization_root, &physical_path)?;
    atomic_write(&physical, &fetched.bytes)?;
    let content_hash = sha256_hex(&fetched.bytes);

    manifest.upsert(ManifestEntry {
        remote_id: entry.remote_id.clone(),
        remote_version: entry.remote_version.clone(),
        logical_path: entry.path.clone(),
        physical_path,
        export_format: fetched.export_format.clone(),
        content_hash: Some(content_hash),
        state: EntryState::Materialized,
    });
    summary.fetched += 1;
    Ok(())
}

#[cfg(test)]
// Test fixtures assert directly against materialized bytes on disk via
// plain std::fs calls - unrelated to the library's own async/blocking-lane
// discipline (materialize.rs's sync helpers are what production code paths
// go through).
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::connector::BulkMaterializeOutcome;
    use crate::policy::MountPolicy;
    use crate::types::{ChangeBatch, FetchedContent, RemoteChange, RemoteEntryKind, RemoteInfo};
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// In-memory fake connector: a fixed map of path -> bytes plus a
    /// scripted sequence of `ChangeBatch`es, one per `list_changes` call.
    /// `fail_paths` makes `fetch` error for specific paths (once), to
    /// exercise the error/no-cursor-advance path.
    struct FakeConnector {
        batches: Mutex<Vec<ChangeBatch>>,
        content: BTreeMap<String, Vec<u8>>,
        fail_once: Mutex<std::collections::HashSet<String>>,
        fetch_calls: Mutex<u32>,
    }

    #[async_trait]
    impl RemoteSourceConnector for FakeConnector {
        fn kind(&self) -> &'static str {
            "fake"
        }

        async fn validate(&self, _mount: &MountConfig) -> Result<RemoteInfo> {
            Ok(RemoteInfo::default())
        }

        async fn list_changes(
            &self,
            _mount: &MountConfig,
            _cursor: Option<ChangeCursor>,
        ) -> Result<ChangeBatch> {
            let mut batches = self.batches.lock().unwrap();
            if batches.is_empty() {
                return Ok(ChangeBatch::default());
            }
            Ok(batches.remove(0))
        }

        async fn fetch(&self, _mount: &MountConfig, entry: &RemoteEntry) -> Result<FetchedContent> {
            *self.fetch_calls.lock().unwrap() += 1;
            let mut fail_once = self.fail_once.lock().unwrap();
            if fail_once.remove(&entry.path) {
                anyhow::bail!("simulated fetch failure for {}", entry.path);
            }
            let bytes = self
                .content
                .get(&entry.path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no fixture content for {}", entry.path))?;
            Ok(FetchedContent {
                bytes,
                media_type: None,
                export_format: None,
            })
        }
    }

    fn entry(remote_id: &str, path: &str, version: &str, size: u64) -> RemoteEntry {
        RemoteEntry {
            remote_id: remote_id.to_string(),
            path: path.to_string(),
            size: Some(size),
            remote_version: version.to_string(),
            kind: RemoteEntryKind::File,
            media_type: None,
        }
    }

    fn mount(root: &Path, policy: MountPolicy) -> MountConfig {
        MountConfig {
            connector_kind: "fake".to_string(),
            remote_scope: "fake://scope".to_string(),
            materialization_root: root.to_path_buf(),
            policy,
            options: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn full_sync_materializes_entries_and_advances_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("root");
        let manifest_path = dir.path().canonicalize().unwrap().join("manifest.json");

        let mut content = BTreeMap::new();
        content.insert("a.txt".to_string(), b"hello".to_vec());
        let connector = FakeConnector {
            batches: Mutex::new(vec![ChangeBatch {
                changes: vec![RemoteChange {
                    entry: entry("r1", "a.txt", "v1", 5),
                    kind: RemoteChangeKind::Added,
                }],
                next_cursor: Some(ChangeCursor("cursor-1".to_string())),
                degraded_to_full_walk: false,
                ..Default::default()
            }]),
            content,
            fail_once: Mutex::new(Default::default()),
            fetch_calls: Mutex::new(0),
        };

        let mount = mount(&root, MountPolicy::default());
        let summary = sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();

        assert_eq!(summary.fetched, 1);
        assert!(summary.errors.is_empty());
        assert_eq!(
            summary.next_cursor,
            Some(ChangeCursor("cursor-1".to_string()))
        );
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"hello");

        let manifest = Manifest::open(&manifest_path).unwrap();
        let recorded = manifest.get("r1").unwrap();
        assert!(matches!(recorded.state, EntryState::Materialized));
        assert!(recorded.content_hash.is_some());
    }

    #[tokio::test]
    async fn resume_skips_refetch_of_unchanged_materialized_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("root");
        let manifest_path = dir.path().canonicalize().unwrap().join("manifest.json");

        let mut content = BTreeMap::new();
        content.insert("a.txt".to_string(), b"hello".to_vec());
        let batch = ChangeBatch {
            changes: vec![RemoteChange {
                entry: entry("r1", "a.txt", "v1", 5),
                kind: RemoteChangeKind::Added,
            }],
            next_cursor: Some(ChangeCursor("cursor-1".to_string())),
            degraded_to_full_walk: false,
            ..Default::default()
        };
        let connector = FakeConnector {
            batches: Mutex::new(vec![batch.clone(), batch]),
            content,
            fail_once: Mutex::new(Default::default()),
            fetch_calls: Mutex::new(0),
        };
        let mount = mount(&root, MountPolicy::default());

        sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();
        assert_eq!(*connector.fetch_calls.lock().unwrap(), 1);

        // A "fresh driver instance" re-sync with the same remote_version
        // must not call fetch again.
        sync_mount(
            &connector,
            &mount,
            &manifest_path,
            Some(ChangeCursor("cursor-1".to_string())),
        )
        .await
        .unwrap();
        assert_eq!(*connector.fetch_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn fetch_error_does_not_advance_cursor_and_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("root");
        let manifest_path = dir.path().canonicalize().unwrap().join("manifest.json");

        let mut content = BTreeMap::new();
        content.insert("a.txt".to_string(), b"hello".to_vec());
        let mut fail_once = std::collections::HashSet::new();
        fail_once.insert("a.txt".to_string());
        let connector = FakeConnector {
            batches: Mutex::new(vec![ChangeBatch {
                changes: vec![RemoteChange {
                    entry: entry("r1", "a.txt", "v1", 5),
                    kind: RemoteChangeKind::Added,
                }],
                next_cursor: Some(ChangeCursor("cursor-1".to_string())),
                degraded_to_full_walk: false,
                ..Default::default()
            }]),
            content,
            fail_once: Mutex::new(fail_once),
            fetch_calls: Mutex::new(0),
        };
        let mount = mount(&root, MountPolicy::default());

        let old_cursor = Some(ChangeCursor("cursor-0".to_string()));
        let summary = sync_mount(&connector, &mount, &manifest_path, old_cursor.clone())
            .await
            .unwrap();
        assert_eq!(summary.errors.len(), 1);
        assert_eq!(summary.next_cursor, old_cursor);
        assert!(!root.join("a.txt").exists());
    }

    #[tokio::test]
    async fn policy_exclusion_prevents_fetch_and_records_skip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("root");
        let manifest_path = dir.path().canonicalize().unwrap().join("manifest.json");

        let connector = FakeConnector {
            batches: Mutex::new(vec![ChangeBatch {
                changes: vec![RemoteChange {
                    entry: entry("r1", "secret/a.txt", "v1", 5),
                    kind: RemoteChangeKind::Added,
                }],
                next_cursor: None,
                degraded_to_full_walk: false,
                ..Default::default()
            }]),
            content: BTreeMap::new(),
            fail_once: Mutex::new(Default::default()),
            fetch_calls: Mutex::new(0),
        };
        let policy = MountPolicy {
            exclude: vec!["secret/**".to_string()],
            ..Default::default()
        };
        let mount = mount(&root, policy);

        let summary = sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();
        assert_eq!(summary.skipped, 1);
        assert_eq!(*connector.fetch_calls.lock().unwrap(), 0);
        assert!(!root.join("secret").exists());
    }

    #[tokio::test]
    async fn total_bytes_cap_aborts_the_whole_pass() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("root");
        let manifest_path = dir.path().canonicalize().unwrap().join("manifest.json");

        let mut content = BTreeMap::new();
        content.insert("a.txt".to_string(), vec![0u8; 100]);
        let connector = FakeConnector {
            batches: Mutex::new(vec![ChangeBatch {
                changes: vec![RemoteChange {
                    entry: entry("r1", "a.txt", "v1", 100),
                    kind: RemoteChangeKind::Added,
                }],
                next_cursor: Some(ChangeCursor("c1".to_string())),
                degraded_to_full_walk: false,
                ..Default::default()
            }]),
            content,
            fail_once: Mutex::new(Default::default()),
            fetch_calls: Mutex::new(0),
        };
        let policy = MountPolicy {
            max_total_bytes: Some(10),
            ..Default::default()
        };
        let mount = mount(&root, policy);

        let err = sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("mount byte cap exceeded"));
    }

    #[tokio::test]
    async fn deletion_removes_manifest_entry_and_physical_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("root");
        let manifest_path = dir.path().canonicalize().unwrap().join("manifest.json");

        let mut content = BTreeMap::new();
        content.insert("a.txt".to_string(), b"hello".to_vec());
        let connector = FakeConnector {
            batches: Mutex::new(vec![
                ChangeBatch {
                    changes: vec![RemoteChange {
                        entry: entry("r1", "a.txt", "v1", 5),
                        kind: RemoteChangeKind::Added,
                    }],
                    next_cursor: Some(ChangeCursor("c1".to_string())),
                    degraded_to_full_walk: false,
                    ..Default::default()
                },
                ChangeBatch {
                    changes: vec![RemoteChange {
                        entry: entry("r1", "a.txt", "v1", 5),
                        kind: RemoteChangeKind::Deleted,
                    }],
                    next_cursor: Some(ChangeCursor("c2".to_string())),
                    degraded_to_full_walk: false,
                    ..Default::default()
                },
            ]),
            content,
            fail_once: Mutex::new(Default::default()),
            fetch_calls: Mutex::new(0),
        };
        let mount = mount(&root, MountPolicy::default());

        sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();
        assert!(root.join("a.txt").exists());

        let summary = sync_mount(
            &connector,
            &mount,
            &manifest_path,
            Some(ChangeCursor("c1".to_string())),
        )
        .await
        .unwrap();
        assert_eq!(summary.deleted, 1);
        assert!(!root.join("a.txt").exists());
        assert!(Manifest::open(&manifest_path).unwrap().get("r1").is_none());
    }

    #[tokio::test]
    async fn rename_reconciles_as_move_not_delete_plus_refetch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("root");
        let manifest_path = dir.path().canonicalize().unwrap().join("manifest.json");

        let mut content = BTreeMap::new();
        content.insert("old.txt".to_string(), b"hello".to_vec());
        content.insert("new.txt".to_string(), b"hello".to_vec());
        let connector = FakeConnector {
            batches: Mutex::new(vec![
                ChangeBatch {
                    changes: vec![RemoteChange {
                        entry: entry("r1", "old.txt", "v1", 5),
                        kind: RemoteChangeKind::Added,
                    }],
                    next_cursor: Some(ChangeCursor("c1".to_string())),
                    degraded_to_full_walk: false,
                    ..Default::default()
                },
                ChangeBatch {
                    changes: vec![RemoteChange {
                        entry: entry("r1", "new.txt", "v1", 5),
                        kind: RemoteChangeKind::Renamed {
                            from_path: "old.txt".to_string(),
                        },
                    }],
                    next_cursor: Some(ChangeCursor("c2".to_string())),
                    degraded_to_full_walk: false,
                    ..Default::default()
                },
            ]),
            content,
            fail_once: Mutex::new(Default::default()),
            fetch_calls: Mutex::new(0),
        };
        let mount = mount(&root, MountPolicy::default());

        sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();
        let summary = sync_mount(
            &connector,
            &mount,
            &manifest_path,
            Some(ChangeCursor("c1".to_string())),
        )
        .await
        .unwrap();

        assert_eq!(summary.renamed, 1);
        assert!(!root.join("old.txt").exists());
        assert!(root.join("new.txt").exists());
        // Same remote_version - no network fetch was needed for the rename
        // itself (content unchanged), only the earlier initial materialize.
        assert_eq!(*connector.fetch_calls.lock().unwrap(), 1);

        let manifest = Manifest::open(&manifest_path).unwrap();
        let recorded = manifest.get("r1").unwrap();
        assert_eq!(recorded.logical_path, "new.txt");
    }

    #[tokio::test]
    async fn degraded_cursor_is_reported_never_silent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("root");
        let manifest_path = dir.path().canonicalize().unwrap().join("manifest.json");

        let connector = FakeConnector {
            batches: Mutex::new(vec![ChangeBatch {
                changes: vec![],
                next_cursor: Some(ChangeCursor("fresh".to_string())),
                degraded_to_full_walk: true,
                ..Default::default()
            }]),
            content: BTreeMap::new(),
            fail_once: Mutex::new(Default::default()),
            fetch_calls: Mutex::new(0),
        };
        let mount = mount(&root, MountPolicy::default());

        let summary = sync_mount(
            &connector,
            &mount,
            &manifest_path,
            Some(ChangeCursor("stale".to_string())),
        )
        .await
        .unwrap();
        assert_eq!(summary.degradations.len(), 1);
        assert!(summary.degradations[0].contains("full walk"));
    }

    #[tokio::test]
    async fn ownership_marker_is_stamped_on_first_sync() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("root");
        let manifest_path = dir.path().canonicalize().unwrap().join("manifest.json");

        let connector = FakeConnector {
            batches: Mutex::new(vec![ChangeBatch::default()]),
            content: BTreeMap::new(),
            fail_once: Mutex::new(Default::default()),
            fetch_calls: Mutex::new(0),
        };
        let mount = mount(&root, MountPolicy::default());
        sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();

        crate::materialize::verify_ownership_marker(&root).unwrap();
    }

    struct BulkConnector {
        outcome: BulkMaterializeOutcome,
    }

    #[async_trait]
    impl RemoteSourceConnector for BulkConnector {
        fn kind(&self) -> &'static str {
            "bulk-fake"
        }

        async fn validate(&self, _mount: &MountConfig) -> Result<RemoteInfo> {
            Ok(RemoteInfo::default())
        }

        async fn list_changes(
            &self,
            _mount: &MountConfig,
            _cursor: Option<ChangeCursor>,
        ) -> Result<ChangeBatch> {
            Ok(ChangeBatch {
                next_cursor: Some(ChangeCursor("bulk-cursor".to_string())),
                ..Default::default()
            })
        }

        async fn fetch(&self, _mount: &MountConfig, entry: &RemoteEntry) -> Result<FetchedContent> {
            anyhow::bail!(
                "bulk connector does not expect per-entry fetch for {}",
                entry.path
            )
        }

        fn bulk_materializes(&self) -> bool {
            true
        }

        async fn materialize_bulk(
            &self,
            _mount: &MountConfig,
            root: &Path,
            _manifest: &Manifest,
            _batch: &ChangeBatch,
        ) -> Result<BulkMaterializeOutcome> {
            for entry in &self.outcome.upserts {
                if matches!(entry.state, EntryState::Materialized) {
                    let path = root.join(&entry.physical_path);
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).unwrap();
                    }
                    std::fs::write(&path, b"bulk-content").unwrap();
                }
            }
            Ok(self.outcome.clone())
        }
    }

    #[tokio::test]
    async fn bulk_materialize_path_applies_policy_after_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("root");
        let manifest_path = dir.path().canonicalize().unwrap().join("manifest.json");

        let connector = BulkConnector {
            outcome: BulkMaterializeOutcome {
                upserts: vec![
                    ManifestEntry {
                        remote_id: "r1".to_string(),
                        remote_version: "sha1".to_string(),
                        logical_path: "keep.txt".to_string(),
                        physical_path: "keep.txt".to_string(),
                        export_format: None,
                        content_hash: Some("hash".to_string()),
                        state: EntryState::Materialized,
                    },
                    ManifestEntry {
                        remote_id: "r2".to_string(),
                        remote_version: "sha1".to_string(),
                        logical_path: "excluded/big.bin".to_string(),
                        physical_path: "excluded/big.bin".to_string(),
                        export_format: None,
                        content_hash: Some("hash".to_string()),
                        state: EntryState::Materialized,
                    },
                ],
                deleted_remote_ids: vec![],
            },
        };
        let policy = MountPolicy {
            exclude: vec!["excluded/**".to_string()],
            ..Default::default()
        };
        let mount = mount(&root, policy);

        let summary = sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();
        assert_eq!(summary.fetched, 1);
        assert_eq!(summary.skipped, 1);
        assert!(root.join("keep.txt").exists());
        assert!(!root.join("excluded/big.bin").exists());

        let manifest = Manifest::open(&manifest_path).unwrap();
        assert!(matches!(
            manifest.get("r2").unwrap().state,
            EntryState::Skipped { .. }
        ));
        assert!(manifest.get("r2").unwrap().content_hash.is_none());
    }
}
