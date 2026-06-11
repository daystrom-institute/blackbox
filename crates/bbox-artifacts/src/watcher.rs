use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::RecommendedWatcher;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};

use crate::artifacts::{ArtifactCatalog, ArtifactScope};

/// Fired (once per debounced batch) when a committed `.bbox/knowledge/*.json`
/// file is created, modified, or removed. The daemon uses it to reload the
/// in-memory knowledge store and trigger a search reindex. Deliberately a bare
/// `Fn()` so the watcher stays decoupled from `SharedState`.
pub type KnowledgeChangeCallback = Arc<dyn Fn() + Send + Sync>;

/// Handle to the filesystem watcher that monitors `.bbox/` directories for
/// installed artifact changes and committed knowledge changes. Keeps the
/// watcher alive for the daemon lifetime.
pub struct BbxWatcher {
    debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    /// `(project_id, canonical .bbox root)` pairs. The project_id is the one
    /// supplied at registration — never reconstructed from the directory name.
    watched_roots: Arc<Mutex<Vec<(String, PathBuf)>>>,
}

impl BbxWatcher {
    /// Start watching all registered projects.
    ///
    /// `on_knowledge_change`, when set, is invoked once per debounced batch in
    /// which any committed `.bbox/knowledge/*.json` file changed. This is
    /// detected independently of the artifact-routing gate below (which ignores
    /// in-place modifies), because knowledge files are also edited in place.
    pub fn start(
        projects: Vec<(String, PathBuf)>, // (project_id, canonical_project_dir)
        catalog: Arc<ArtifactCatalog>,
        on_knowledge_change: Option<KnowledgeChangeCallback>,
    ) -> anyhow::Result<Self> {
        let watched_roots: Arc<Mutex<Vec<(String, PathBuf)>>> = Arc::new(Mutex::new(Vec::new()));
        let watched_roots_cb = watched_roots.clone();
        let catalog_cb = catalog.clone();

        let debouncer = new_debouncer(
            Duration::from_millis(200),
            None,
            move |result: DebounceEventResult| {
                let events = match result {
                    Ok(evs) => evs,
                    Err(e) => {
                        tracing::warn!("watcher error: {e:?}");
                        return;
                    }
                };
                let roots = watched_roots_cb.lock().unwrap().clone();
                let mut repo_store_dirty = false;
                for event in events {
                    if !repo_store_dirty && event_touches_repo_store(&event.event, &roots) {
                        repo_store_dirty = true;
                    }
                    handle_event(&event.event, &roots, &catalog_cb);
                }
                // Fire once for the whole batch — a `git pull` lands many files
                // under one debounce window, and the reload/reindex it triggers
                // is a full refresh, not per-file.
                if repo_store_dirty {
                    if let Some(cb) = &on_knowledge_change {
                        cb();
                    }
                }
            },
        )?;

        let mut watcher = Self {
            debouncer,
            watched_roots,
        };

        for (project_id, project_dir) in projects {
            watcher.watch_project_inner(&project_id, &project_dir)?;
        }

        Ok(watcher)
    }

    /// Add a newly registered project to the live watch set. Safe to call after
    /// `start`; duplicate registrations for the same path are silently no-ops.
    pub fn watch_project(&mut self, project_id: &str, project_dir: &Path) -> anyhow::Result<()> {
        self.watch_project_inner(project_id, project_dir)
    }

    fn watch_project_inner(&mut self, project_id: &str, project_dir: &Path) -> anyhow::Result<()> {
        let bbox_dir = project_dir.join(".bbox");
        if !bbox_dir.exists() {
            return Ok(());
        }
        let canonical = match bbox_dir.canonicalize() {
            Ok(c) => c,
            Err(_) => return Ok(()),
        };
        {
            let mut roots = self.watched_roots.lock().unwrap();
            if roots.iter().any(|(_, r)| r == &canonical) {
                return Ok(());
            }
            roots.push((project_id.to_string(), canonical.clone()));
        }
        self.debouncer
            .watch(&canonical, notify::RecursiveMode::Recursive)?;
        Ok(())
    }
}

/// Route a single debounced notify event to the appropriate artifact action.
pub fn handle_event(
    event: &notify::Event,
    roots: &[(String, PathBuf)],
    catalog: &ArtifactCatalog,
) {
    let is_create_or_rename_to = matches!(
        event.kind,
        notify::EventKind::Create(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::To
            ))
    );
    let is_remove = matches!(event.kind, notify::EventKind::Remove(_));

    if !is_create_or_rename_to && !is_remove {
        return;
    }

    for path in &event.paths {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n == ".gitignore")
            .unwrap_or(false)
        {
            continue;
        }

        let (project_id, bbox_root) = match roots
            .iter()
            .find_map(|(pid, r)| path.starts_with(r).then(|| (pid.clone(), r.clone())))
        {
            Some(v) => v,
            None => continue,
        };

        if is_create_or_rename_to {
            handle_create(path, &project_id, &bbox_root, catalog);
        } else {
            handle_remove(path, &project_id, &bbox_root, catalog);
        }
    }
}

fn handle_create(path: &Path, project_id: &str, bbox_root: &Path, catalog: &ArtifactCatalog) {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("watcher: could not read {}: {e}", path.display());
            return;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("watcher: could not parse {}: {e}", path.display());
            return;
        }
    };
    let local = is_local_path(path, bbox_root);
    let kind = match path_to_artifact_kind(path, bbox_root) {
        Some(k) => k,
        None => return,
    };
    let scope = ArtifactScope::Project { project_id, local };
    match catalog.install_value_scoped(
        scope,
        kind,
        path.to_string_lossy().into_owned(),
        &value,
        None,
        None,
        None,
    ) {
        Ok(meta) => tracing::info!(
            "watcher: installed {}/{} v{} (project {})",
            kind.as_str(),
            meta.name,
            meta.version,
            project_id,
        ),
        Err(e) => tracing::warn!("watcher: install failed for {}: {e}", path.display()),
    }
}

fn handle_remove(path: &Path, project_id: &str, bbox_root: &Path, catalog: &ArtifactCatalog) {
    let local = is_local_path(path, bbox_root);
    let kind = match path_to_artifact_kind(path, bbox_root) {
        Some(k) => k,
        None => return,
    };
    let scope = ArtifactScope::Project { project_id, local };
    match catalog.mark_removed_by_source(scope, kind, path) {
        Ok(Some(meta)) => tracing::info!(
            "watcher: marked removed {}/{} (project {})",
            kind.as_str(),
            meta.name,
            project_id,
        ),
        Ok(None) => {}
        Err(e) => tracing::warn!("watcher: mark_removed failed for {}: {e}", path.display()),
    }
}

/// True when an event is a create/modify/remove of a committed repo-owned store
/// file: `<bbox_root>/knowledge/*.json` (any depth) OR a top-level
/// `<bbox_root>/gaps/*.json`. Both stores are repo-owned and live-reloaded.
/// Gaps match top-level only — the `gaps/inbox/` subtree is spool-owned (drop
/// folder) and its churn must not trigger a durable-store reload. Modify is
/// included on purpose: artifact routing ignores in-place modifies, but
/// knowledge/gap entries are edited in place (manual edits, some editor/git
/// write patterns). Access events are ignored.
fn event_touches_repo_store(event: &notify::Event, roots: &[(String, PathBuf)]) -> bool {
    if !matches!(
        event.kind,
        notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
    ) {
        return false;
    }
    event.paths.iter().any(|path| {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            return false;
        }
        roots.iter().any(|(_, root)| {
            path.starts_with(root.join("knowledge"))
                || path.parent() == Some(root.join("gaps").as_path())
        })
    })
}

fn is_local_path(path: &Path, bbox_root: &Path) -> bool {
    path.starts_with(bbox_root.join("local"))
}

fn path_to_artifact_kind(path: &Path, bbox_root: &Path) -> Option<crate::artifacts::ArtifactKind> {
    let rel = path.strip_prefix(bbox_root).ok()?;
    let mut components = rel.components();
    let first = components.next()?.as_os_str().to_str()?;
    let kind_str = if first == "local" {
        components.next()?.as_os_str().to_str()?
    } else {
        first
    };
    crate::artifacts::artifact_kind_from_dir_pub(kind_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::ArtifactCatalog;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn make_workflow(name: &str) -> serde_json::Value {
        serde_json::json!({ "name": name, "version": "1", "steps": [] })
    }

    #[test]
    fn watcher_installs_atomic_rename() {
        let dir = tempdir().unwrap();
        // Canonicalize first: on macOS tempdir() lives under /var, which
        // canonicalizes to /private/var, and the watcher canonicalizes its
        // roots — so the event path must be canonical too or starts_with misses.
        let project_dir = dir.path().canonicalize().unwrap().join("myproject");
        let bbox_dir = project_dir.join(".bbox");
        let wf_dir = bbox_dir.join("workflows");
        std::fs::create_dir_all(&wf_dir).unwrap();

        let catalog_dir = dir.path().join("catalog");
        let catalog = Arc::new(ArtifactCatalog::open(&catalog_dir).unwrap());

        // Simulate atomic rename: write to .tmp then rename into place.
        let tmp_path = wf_dir.join("watch-flow.json.tmp");
        let final_path = wf_dir.join("watch-flow.json");
        let value = make_workflow("watch-flow");
        std::fs::write(&tmp_path, serde_json::to_string(&value).unwrap()).unwrap();
        std::fs::rename(&tmp_path, &final_path).unwrap();

        // Simulate the watcher event handler directly.
        let event = notify::Event {
            kind: notify::EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::To,
            )),
            paths: vec![final_path],
            attrs: Default::default(),
        };
        // Register with a project_id that deliberately differs from the project
        // directory name, to prove the watcher scopes by the registered
        // project_id and not the `.bbox` parent directory basename (regression:
        // handle_event used to reconstruct the id from the dir name).
        let roots = vec![("proj-alpha".to_string(), bbox_dir.canonicalize().unwrap())];
        handle_event(&event, &roots, &catalog);

        // Artifact should be installed under the registered project_id.
        let scoped = catalog
            .load_artifact_value_scoped(
                Some("proj-alpha"),
                crate::artifacts::ArtifactKind::Workflow,
                "watch-flow",
            )
            .unwrap();
        assert!(
            scoped.is_some(),
            "artifact should be installed under the registered project_id"
        );

        // Lookup under the directory basename must be empty — the dir name is
        // not the scope key.
        let by_dirname = catalog
            .load_artifact_value_scoped(
                Some("myproject"),
                crate::artifacts::ArtifactKind::Workflow,
                "watch-flow",
            )
            .unwrap();
        assert!(
            by_dirname.is_none(),
            "artifact must NOT be scoped by the .bbox parent directory name"
        );

        // Global lookup must return None (not polluted).
        let global = catalog
            .load_artifact_value(crate::artifacts::ArtifactKind::Workflow, "watch-flow")
            .unwrap();
        assert!(global.is_none(), "artifact should NOT be in global path");
    }

    #[test]
    fn watcher_deletion_marks_removed_not_deleted() {
        let dir = tempdir().unwrap();
        // Canonicalize first: on macOS tempdir() lives under /var, which
        // canonicalizes to /private/var, and the watcher canonicalizes its
        // roots — so the event path must be canonical too or starts_with misses.
        let project_dir = dir.path().canonicalize().unwrap().join("myproject");
        let bbox_dir = project_dir.join(".bbox");
        let wf_dir = bbox_dir.join("workflows");
        std::fs::create_dir_all(&wf_dir).unwrap();

        let catalog_dir = dir.path().join("catalog");
        let catalog = Arc::new(ArtifactCatalog::open(&catalog_dir).unwrap());
        let artifact_path = wf_dir.join("del-flow.json");
        let value = make_workflow("del-flow");
        std::fs::write(&artifact_path, serde_json::to_string(&value).unwrap()).unwrap();

        // Install via create event.
        let create_event = notify::Event {
            kind: notify::EventKind::Create(notify::event::CreateKind::File),
            paths: vec![artifact_path.clone()],
            attrs: Default::default(),
        };
        // project_id differs from the directory name ("myproject") on purpose.
        let roots = vec![("proj-beta".to_string(), bbox_dir.canonicalize().unwrap())];
        handle_event(&create_event, &roots, &catalog);

        // Delete the file and fire remove event.
        std::fs::remove_file(&artifact_path).unwrap();
        let remove_event = notify::Event {
            kind: notify::EventKind::Remove(notify::event::RemoveKind::File),
            paths: vec![artifact_path.clone()],
            attrs: Default::default(),
        };
        handle_event(&remove_event, &roots, &catalog);

        // Artifact JSON in catalog must still exist (audit trail), scoped under
        // the registered project_id.
        let scoped = catalog
            .load_artifact_value_scoped(
                Some("proj-beta"),
                crate::artifacts::ArtifactKind::Workflow,
                "del-flow",
            )
            .unwrap();
        assert!(
            scoped.is_some(),
            "artifact JSON must be preserved after deletion (audit trail)"
        );
    }

    #[test]
    fn knowledge_change_detection_matches_committed_entries_only() {
        let dir = tempdir().unwrap();
        let bbox_root = dir.path().canonicalize().unwrap().join(".bbox");
        let roots = vec![("proj-k".to_string(), bbox_root.clone())];

        let kb_entry = bbox_root.join("knowledge").join("abc12345.json");
        let mk = |kind: notify::EventKind, path: &std::path::Path| notify::Event {
            kind,
            paths: vec![path.to_path_buf()],
            attrs: Default::default(),
        };

        // Create / modify (in-place edit) / remove of a knowledge json all count.
        for kind in [
            notify::EventKind::Create(notify::event::CreateKind::File),
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            notify::EventKind::Remove(notify::event::RemoveKind::File),
        ] {
            assert!(
                event_touches_repo_store(&mk(kind, &kb_entry), &roots),
                "knowledge {kind:?} should be detected"
            );
        }

        // Access events are not changes.
        assert!(
            !event_touches_repo_store(
                &mk(
                    notify::EventKind::Access(notify::event::AccessKind::Read),
                    &kb_entry
                ),
                &roots
            ),
            "access events must not trigger a refresh"
        );

        // Non-json under knowledge/, and a json under a different .bbox subdir
        // (an artifact), must not be treated as knowledge changes.
        let non_json = bbox_root.join("knowledge").join("README.md");
        let artifact = bbox_root.join("workflows").join("flow.json");
        let create = |p: &std::path::Path| {
            mk(
                notify::EventKind::Create(notify::event::CreateKind::File),
                p,
            )
        };
        assert!(!event_touches_repo_store(&create(&non_json), &roots));
        assert!(!event_touches_repo_store(&create(&artifact), &roots));

        // A path outside any watched root is ignored.
        let foreign = dir
            .path()
            .canonicalize()
            .unwrap()
            .join("other/.bbox/knowledge/x.json");
        assert!(!event_touches_repo_store(&create(&foreign), &roots));

        // Top-level `.bbox/gaps/*.json` is a repo-owned gap store change.
        let gap_entry = bbox_root.join("gaps").join("gap-abc12345.json");
        assert!(
            event_touches_repo_store(&create(&gap_entry), &roots),
            "top-level gaps json should be detected"
        );
        // The spool's `gaps/inbox/` subtree is NOT a durable-store change.
        let spool_drop = bbox_root.join("gaps").join("inbox").join("dropped.json");
        assert!(
            !event_touches_repo_store(&create(&spool_drop), &roots),
            "gaps/inbox/ churn must not trigger a gap-store reload"
        );
    }
}
