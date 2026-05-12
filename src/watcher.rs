use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::RecommendedWatcher;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};

use crate::artifacts::{ArtifactCatalog, ArtifactScope};

/// Handle to the filesystem watcher that monitors `.bbox/` directories for
/// installed artifact changes. Keeps the watcher alive for the daemon lifetime.
pub struct BbxWatcher {
    debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    watched_roots: Arc<Mutex<Vec<PathBuf>>>,
}

impl BbxWatcher {
    /// Start watching all registered projects.
    pub fn start(
        projects: Vec<(String, PathBuf)>, // (project_id, canonical_project_dir)
        catalog: Arc<ArtifactCatalog>,
    ) -> anyhow::Result<Self> {
        let watched_roots: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
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
                for event in events {
                    handle_event(&event.event, &roots, &catalog_cb);
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

    fn watch_project_inner(&mut self, _project_id: &str, project_dir: &Path) -> anyhow::Result<()> {
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
            if roots.contains(&canonical) {
                return Ok(());
            }
            roots.push(canonical.clone());
        }
        self.debouncer
            .watch(&canonical, notify::RecursiveMode::Recursive)?;
        Ok(())
    }
}

/// Route a single debounced notify event to the appropriate artifact action.
pub(crate) fn handle_event(event: &notify::Event, roots: &[PathBuf], catalog: &ArtifactCatalog) {
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

        let (project_id, bbox_root) = match roots.iter().find_map(|r| {
            path.starts_with(r).then(|| {
                let pid = r
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                (pid, r.clone())
            })
        }) {
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
        let project_dir = dir.path().join("myproject");
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
        let roots = vec![bbox_dir.canonicalize().unwrap()];
        handle_event(&event, &roots, &catalog);

        // Artifact should be installed in the scoped path for "myproject".
        let scoped = catalog
            .load_artifact_value_scoped(
                Some("myproject"),
                crate::artifacts::ArtifactKind::Workflow,
                "watch-flow",
            )
            .unwrap();
        assert!(
            scoped.is_some(),
            "artifact should be installed in scoped path"
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
        let project_dir = dir.path().join("myproject");
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
        let roots = vec![bbox_dir.canonicalize().unwrap()];
        handle_event(&create_event, &roots, &catalog);

        // Delete the file and fire remove event.
        std::fs::remove_file(&artifact_path).unwrap();
        let remove_event = notify::Event {
            kind: notify::EventKind::Remove(notify::event::RemoveKind::File),
            paths: vec![artifact_path.clone()],
            attrs: Default::default(),
        };
        handle_event(&remove_event, &roots, &catalog);

        // Artifact JSON in catalog must still exist (audit trail).
        let scoped = catalog
            .load_artifact_value_scoped(
                Some("myproject"),
                crate::artifacts::ArtifactKind::Workflow,
                "del-flow",
            )
            .unwrap();
        assert!(
            scoped.is_some(),
            "artifact JSON must be preserved after deletion (audit trail)"
        );
    }
}
