use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::RecommendedWatcher;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};

use crate::artifacts::{ArtifactCatalog, ArtifactScope};

const MAX_LOGICAL_ID_BYTES: usize = 256;

/// Fired once per affected carrier in a debounced batch when a committed
/// `.bbox/knowledge/*.json` file is created, modified, or removed. The daemon
/// uses it to reload the in-memory knowledge store and trigger a search reindex.
/// The logical carrier lets follow-on repository reads stay scoped to the
/// attachment that produced the event.
pub type KnowledgeChangeCallback = Arc<dyn Fn(&ArtifactWatchCarrier) + Send + Sync>;

/// Path-free attachment selector for artifact and repository-store watching.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtifactWatchAttachment {
    Selected,
    CheckoutId(String),
}

/// Logical identity presented to the daemon-owned discovery authority.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactWatchCarrier {
    project_id: String,
    attachment: ArtifactWatchAttachment,
}

impl ArtifactWatchCarrier {
    pub fn selected(project_id: impl Into<String>) -> anyhow::Result<Self> {
        Self::new(project_id, ArtifactWatchAttachment::Selected)
    }

    pub fn checkout(
        project_id: impl Into<String>,
        checkout_id: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let checkout_id = checkout_id.into();
        validate_logical_id("checkout id", &checkout_id)?;
        Self::new(project_id, ArtifactWatchAttachment::CheckoutId(checkout_id))
    }

    fn new(
        project_id: impl Into<String>,
        attachment: ArtifactWatchAttachment,
    ) -> anyhow::Result<Self> {
        let project_id = project_id.into();
        validate_logical_id("project id", &project_id)?;
        Ok(Self {
            project_id,
            attachment,
        })
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn attachment(&self) -> &ArtifactWatchAttachment {
        &self.attachment
    }
}

fn validate_logical_id(label: &str, value: &str) -> anyhow::Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("artifact watch {label} is required");
    }
    if value.len() > MAX_LOGICAL_ID_BYTES {
        anyhow::bail!("artifact watch {label} exceeds {MAX_LOGICAL_ID_BYTES} bytes");
    }
    if value.contains('/') || value.contains('\\') || Path::new(value).is_absolute() {
        anyhow::bail!("artifact watch {label} must be a path-free logical identifier");
    }
    Ok(())
}

/// Operation-scoped checkout discovery authority.
///
/// Implementations resolve the logical carrier, acquire
/// `ArtifactWatchDiscovery` read authority, and invoke `operation` exactly once
/// while the opaque lease remains alive. A denial must not invoke `operation`.
pub trait ArtifactWatchAccess: Send + Sync {
    fn with_discovery(
        &self,
        carrier: &ArtifactWatchCarrier,
        operation: &mut dyn FnMut(&Path) -> anyhow::Result<()>,
    ) -> anyhow::Result<()>;
}

#[derive(Debug, Clone)]
struct WatchRegistration {
    carrier: ArtifactWatchCarrier,
    /// Retained only for OS watch registration and event-to-carrier routing.
    /// It is never filesystem authority for reading event bytes.
    bbox_root: PathBuf,
    artifact_routing: bool,
}

/// Handle to the filesystem watcher that monitors `.bbox/` directories for
/// installed artifact changes and committed knowledge changes. Keeps the
/// watcher alive for the daemon lifetime.
pub struct BbxWatcher {
    debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    registrations: Arc<Mutex<Vec<WatchRegistration>>>,
    access: Arc<dyn ArtifactWatchAccess>,
}

impl BbxWatcher {
    /// Start watching all registered projects.
    ///
    /// `on_knowledge_change`, when set, is invoked once for each affected
    /// logical carrier in a debounced batch. This is detected independently of
    /// the artifact-routing gate below (which ignores in-place modifies),
    /// because knowledge files are also edited in place.
    pub fn start(
        projects: Vec<ArtifactWatchCarrier>,
        access: Arc<dyn ArtifactWatchAccess>,
        catalog: Arc<ArtifactCatalog>,
        on_knowledge_change: Option<KnowledgeChangeCallback>,
    ) -> anyhow::Result<Self> {
        let registrations: Arc<Mutex<Vec<WatchRegistration>>> = Arc::new(Mutex::new(Vec::new()));
        let registrations_cb = registrations.clone();
        let access_cb = access.clone();
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
                let events = events
                    .into_iter()
                    .map(|event| event.event)
                    .collect::<Vec<_>>();
                let registrations = registrations_cb.lock().unwrap().clone();
                handle_event_batch(
                    &events,
                    &registrations,
                    access_cb.as_ref(),
                    &catalog_cb,
                    on_knowledge_change.as_ref(),
                );
            },
        )?;

        let mut watcher = Self {
            debouncer,
            registrations,
            access,
        };

        for carrier in projects {
            if let Err(err) = watcher.watch_project(carrier.clone()) {
                tracing::warn!(
                    project = %carrier.project_id,
                    attachment = ?carrier.attachment,
                    error = %err,
                    "artifact watcher skipped unavailable startup carrier"
                );
            }
        }

        Ok(watcher)
    }

    /// Add a newly registered project to the live watch set. Safe to call after
    /// `start`; duplicate registrations for the same carrier are no-ops.
    pub fn watch_project(&mut self, carrier: ArtifactWatchCarrier) -> anyhow::Result<bool> {
        self.register(carrier, true)
    }

    /// Add a checkout project root to the knowledge/gap watch set without
    /// routing its other `.bbox` files through the artifact catalog.
    pub fn watch_repo_store(&mut self, carrier: ArtifactWatchCarrier) -> anyhow::Result<bool> {
        self.register(carrier, false)
    }

    /// Remove a provisional checkout root from the knowledge/gap watch set.
    /// Registered project roots retain their watch because they also carry
    /// artifact-install authority.
    pub fn unwatch_repo_store(&mut self, carrier: &ArtifactWatchCarrier) -> anyhow::Result<bool> {
        self.remove_registration(carrier, true)
    }

    /// Remove any watch registration for a logical carrier.
    pub fn unwatch_carrier(&mut self, carrier: &ArtifactWatchCarrier) -> anyhow::Result<bool> {
        self.remove_registration(carrier, false)
    }

    fn register(
        &mut self,
        carrier: ArtifactWatchCarrier,
        artifact_routing: bool,
    ) -> anyhow::Result<bool> {
        let existing = self
            .registrations
            .lock()
            .unwrap()
            .iter()
            .find(|registration| registration.carrier == carrier)
            .cloned();
        if existing
            .as_ref()
            .is_some_and(|registration| registration.artifact_routing || !artifact_routing)
        {
            return Ok(false);
        }

        let mut invoked = false;
        let mut discovered_root = None;
        let registrations = self.registrations.clone();
        let debouncer = &mut self.debouncer;
        let mut operation = |project_root: &Path| {
            if invoked {
                anyhow::bail!("artifact watch authority invoked registration more than once");
            }
            invoked = true;
            let Some(bbox_root) = canonical_bbox_root(project_root) else {
                return Ok(());
            };
            if existing
                .as_ref()
                .is_some_and(|registration| registration.bbox_root != bbox_root)
            {
                anyhow::bail!("artifact watch carrier changed roots without removal");
            }
            let already_watched = registrations
                .lock()
                .unwrap()
                .iter()
                .any(|registration| registration.bbox_root == bbox_root);
            if !already_watched {
                debouncer.watch(&bbox_root, notify::RecursiveMode::Recursive)?;
            }
            discovered_root = Some(bbox_root);
            Ok(())
        };
        let discovery_result = self.access.with_discovery(&carrier, &mut operation);
        drop(operation);
        discovery_result?;
        if !invoked {
            anyhow::bail!("artifact watch authority did not invoke registration");
        }
        let Some(bbox_root) = discovered_root else {
            return Ok(false);
        };

        let mut registrations = self.registrations.lock().unwrap();
        if let Some(existing) = registrations
            .iter_mut()
            .find(|registration| registration.carrier == carrier)
        {
            if existing.bbox_root != bbox_root {
                anyhow::bail!("artifact watch carrier changed roots without removal");
            }
            existing.artifact_routing |= artifact_routing;
        } else {
            registrations.push(WatchRegistration {
                carrier,
                bbox_root,
                artifact_routing,
            });
        }
        Ok(true)
    }

    fn remove_registration(
        &mut self,
        carrier: &ArtifactWatchCarrier,
        repo_only: bool,
    ) -> anyhow::Result<bool> {
        let removed_root = {
            let mut registrations = self.registrations.lock().unwrap();
            let Some(index) = registrations.iter().position(|registration| {
                registration.carrier == *carrier && (!repo_only || !registration.artifact_routing)
            }) else {
                return Ok(false);
            };
            registrations.remove(index).bbox_root
        };
        let still_watched = self
            .registrations
            .lock()
            .unwrap()
            .iter()
            .any(|registration| registration.bbox_root == removed_root);
        if !still_watched {
            self.debouncer.unwatch(&removed_root)?;
        }
        Ok(true)
    }
}

fn canonical_bbox_root(project_dir: &Path) -> Option<PathBuf> {
    let project_dir = project_dir.canonicalize().ok()?;
    let bbox_dir = project_dir.join(".bbox");
    if !bbox_dir.is_dir() {
        return None;
    }
    let bbox_dir = bbox_dir.canonicalize().ok()?;
    bbox_dir.starts_with(&project_dir).then_some(bbox_dir)
}

/// Route an authorized event to the appropriate artifact action.
fn handle_artifact_event(
    event: &notify::Event,
    project_id: &str,
    bbox_root: &Path,
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

        if !path.starts_with(bbox_root) {
            continue;
        }

        if is_create_or_rename_to {
            handle_create(path, project_id, bbox_root, catalog);
        } else {
            handle_remove(path, project_id, bbox_root, catalog);
        }
    }
}

fn handle_event_batch(
    events: &[notify::Event],
    registrations: &[WatchRegistration],
    access: &dyn ArtifactWatchAccess,
    catalog: &ArtifactCatalog,
    on_knowledge_change: Option<&KnowledgeChangeCallback>,
) {
    for registration in registrations {
        if !events.iter().any(|event| {
            event
                .paths
                .iter()
                .any(|path| path.starts_with(&registration.bbox_root))
        }) {
            continue;
        }

        let mut invoked = false;
        let mut operation = |project_root: &Path| {
            if invoked {
                anyhow::bail!("artifact watch authority invoked event handling more than once");
            }
            invoked = true;
            let Some(bbox_root) = canonical_bbox_root(project_root) else {
                anyhow::bail!("authorized artifact watch root has no .bbox directory");
            };
            if bbox_root != registration.bbox_root {
                anyhow::bail!("artifact watch registration no longer matches authorized root");
            }

            let mut repo_store_dirty = false;
            for event in events {
                if !event.paths.iter().any(|path| path.starts_with(&bbox_root)) {
                    continue;
                }
                if registration.artifact_routing {
                    handle_artifact_event(
                        event,
                        &registration.carrier.project_id,
                        &bbox_root,
                        catalog,
                    );
                }
                repo_store_dirty |=
                    event_touches_repo_store(event, std::slice::from_ref(&bbox_root));
            }
            if repo_store_dirty && let Some(callback) = on_knowledge_change {
                callback(&registration.carrier);
            }
            Ok(())
        };
        let discovery_result = access.with_discovery(&registration.carrier, &mut operation);
        drop(operation);
        if let Err(err) = discovery_result {
            tracing::debug!(
                project = %registration.carrier.project_id,
                attachment = ?registration.carrier.attachment,
                error = %err,
                "artifact watcher skipped event for unavailable carrier"
            );
        } else if !invoked {
            tracing::debug!(
                project = %registration.carrier.project_id,
                attachment = ?registration.carrier.attachment,
                "artifact watcher authority skipped event operation"
            );
        }
    }
}

fn handle_create(path: &Path, project_id: &str, bbox_root: &Path, catalog: &ArtifactCatalog) {
    let path = match path.canonicalize() {
        Ok(path) if path.starts_with(bbox_root) => path,
        Ok(_) => {
            tracing::debug!("watcher: refused artifact path outside authorized .bbox root");
            return;
        }
        Err(e) => {
            tracing::debug!("watcher: could not resolve {}: {e}", path.display());
            return;
        }
    };
    let raw = match std::fs::read_to_string(&path) {
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
    let local = is_local_path(&path, bbox_root);
    let kind = match path_to_artifact_kind(&path, bbox_root) {
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
    let Ok(relative) = path.strip_prefix(bbox_root) else {
        return;
    };
    if relative.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return;
    }
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
fn event_touches_repo_store(event: &notify::Event, roots: &[PathBuf]) -> bool {
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
        roots.iter().any(|root| {
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
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[derive(Default)]
    struct TestWatchAccess {
        roots: Mutex<BTreeMap<ArtifactWatchCarrier, PathBuf>>,
        denied: AtomicBool,
        operation_calls: AtomicUsize,
        operation_active: AtomicBool,
    }

    impl TestWatchAccess {
        fn insert(&self, carrier: ArtifactWatchCarrier, root: PathBuf) {
            self.roots.lock().unwrap().insert(carrier, root);
        }
    }

    impl ArtifactWatchAccess for TestWatchAccess {
        fn with_discovery(
            &self,
            carrier: &ArtifactWatchCarrier,
            operation: &mut dyn FnMut(&Path) -> anyhow::Result<()>,
        ) -> anyhow::Result<()> {
            if self.denied.load(Ordering::SeqCst) {
                anyhow::bail!("artifact watch discovery denied");
            }
            let root = self
                .roots
                .lock()
                .unwrap()
                .get(carrier)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown artifact watch carrier"))?;
            self.operation_calls.fetch_add(1, Ordering::SeqCst);
            self.operation_active.store(true, Ordering::SeqCst);
            let result = operation(&root);
            self.operation_active.store(false, Ordering::SeqCst);
            result
        }
    }

    fn registration(
        carrier: ArtifactWatchCarrier,
        project_dir: &Path,
        artifact_routing: bool,
    ) -> WatchRegistration {
        WatchRegistration {
            carrier,
            bbox_root: project_dir.join(".bbox").canonicalize().unwrap(),
            artifact_routing,
        }
    }

    fn make_workflow(name: &str) -> serde_json::Value {
        serde_json::json!({ "name": name, "version": "1", "steps": [] })
    }

    #[test]
    fn watch_carriers_reject_path_shaped_identifiers() {
        assert!(ArtifactWatchCarrier::selected("/tmp/project").is_err());
        assert!(ArtifactWatchCarrier::checkout("project", "worktrees/one").is_err());
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
        let carrier = ArtifactWatchCarrier::selected("proj-alpha").unwrap();
        let access = TestWatchAccess::default();
        access.insert(carrier.clone(), project_dir.clone());
        let registrations = vec![registration(carrier, &project_dir, true)];
        handle_event_batch(&[event], &registrations, &access, &catalog, None);

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
        let carrier = ArtifactWatchCarrier::selected("proj-beta").unwrap();
        let access = TestWatchAccess::default();
        access.insert(carrier.clone(), project_dir.clone());
        let registrations = vec![registration(carrier, &project_dir, true)];
        handle_event_batch(&[create_event], &registrations, &access, &catalog, None);

        // Delete the file and fire remove event.
        std::fs::remove_file(&artifact_path).unwrap();
        let remove_event = notify::Event {
            kind: notify::EventKind::Remove(notify::event::RemoveKind::File),
            paths: vec![artifact_path.clone()],
            attrs: Default::default(),
        };
        handle_event_batch(&[remove_event], &registrations, &access, &catalog, None);

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
        let roots = vec![bbox_root.clone()];

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

    #[test]
    fn provisional_root_is_watched_without_artifact_authority() {
        let dir = tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("checkout");
        std::fs::create_dir_all(project.join(".bbox/knowledge")).unwrap();
        let catalog = Arc::new(ArtifactCatalog::open(dir.path().join("catalog")).unwrap());
        let carrier = ArtifactWatchCarrier::checkout("proj-gamma", "checkout-gamma").unwrap();
        let access = Arc::new(TestWatchAccess::default());
        access.insert(carrier.clone(), project);
        let mut watcher = BbxWatcher::start(Vec::new(), access, catalog, None).unwrap();

        assert!(watcher.watch_repo_store(carrier.clone()).unwrap());
        assert_eq!(watcher.registrations.lock().unwrap().len(), 1);
        assert!(!watcher.registrations.lock().unwrap()[0].artifact_routing);
        assert!(!watcher.watch_repo_store(carrier.clone()).unwrap());
        assert!(watcher.unwatch_repo_store(&carrier).unwrap());
        assert!(watcher.registrations.lock().unwrap().is_empty());
    }

    #[test]
    fn denied_event_authority_reads_nothing_and_mutates_no_catalog_state() {
        let dir = tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("project");
        let workflows = project.join(".bbox/workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        let artifact_path = workflows.join("denied-flow.json");
        std::fs::write(
            &artifact_path,
            serde_json::to_string(&make_workflow("denied-flow")).unwrap(),
        )
        .unwrap();
        let knowledge_path = project.join(".bbox/knowledge/entry.json");
        std::fs::create_dir_all(knowledge_path.parent().unwrap()).unwrap();
        std::fs::write(&knowledge_path, "{}").unwrap();
        let carrier = ArtifactWatchCarrier::selected("proj-denied").unwrap();
        let access = Arc::new(TestWatchAccess::default());
        access.insert(carrier.clone(), project);
        let catalog = Arc::new(ArtifactCatalog::open(dir.path().join("catalog")).unwrap());
        let mut watcher =
            BbxWatcher::start(Vec::new(), access.clone(), catalog.clone(), None).unwrap();
        assert!(watcher.watch_project(carrier).unwrap());
        let registration_calls = access.operation_calls.load(Ordering::SeqCst);
        access.denied.store(true, Ordering::SeqCst);
        let artifact_event = notify::Event {
            kind: notify::EventKind::Create(notify::event::CreateKind::File),
            paths: vec![artifact_path],
            attrs: Default::default(),
        };
        let knowledge_event = notify::Event {
            kind: notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![knowledge_path],
            attrs: Default::default(),
        };
        let reload_calls = Arc::new(AtomicUsize::new(0));
        let reload_calls_cb = reload_calls.clone();
        let callback: KnowledgeChangeCallback = Arc::new(move |_| {
            reload_calls_cb.fetch_add(1, Ordering::SeqCst);
        });

        handle_event_batch(
            &[artifact_event, knowledge_event],
            &watcher.registrations.lock().unwrap().clone(),
            access.as_ref(),
            &catalog,
            Some(&callback),
        );

        assert_eq!(
            access.operation_calls.load(Ordering::SeqCst),
            registration_calls,
            "denial must not invoke the filesystem operation"
        );
        assert_eq!(
            reload_calls.load(Ordering::SeqCst),
            0,
            "denial must not invoke repository-store reloads"
        );
        assert!(
            catalog
                .load_artifact_value_scoped(
                    Some("proj-denied"),
                    crate::artifacts::ArtifactKind::Workflow,
                    "denied-flow",
                )
                .unwrap()
                .is_none(),
            "denial must not mutate the artifact catalog"
        );
    }

    #[test]
    fn repo_change_callback_runs_inside_discovery_scope() {
        let dir = tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("project");
        let knowledge = project.join(".bbox/knowledge");
        std::fs::create_dir_all(&knowledge).unwrap();
        let entry = knowledge.join("entry.json");
        std::fs::write(&entry, "{}").unwrap();
        let carrier = ArtifactWatchCarrier::selected("proj-scoped").unwrap();
        let access = Arc::new(TestWatchAccess::default());
        access.insert(carrier.clone(), project.clone());
        let registration = registration(carrier, &project, false);
        let access_cb = access.clone();
        let callback_calls = Arc::new(AtomicUsize::new(0));
        let callback_calls_cb = callback_calls.clone();
        let callback: KnowledgeChangeCallback = Arc::new(move |_| {
            assert!(
                access_cb.operation_active.load(Ordering::SeqCst),
                "repository callback must run while discovery authority is active"
            );
            callback_calls_cb.fetch_add(1, Ordering::SeqCst);
        });
        let event = notify::Event {
            kind: notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![entry],
            attrs: Default::default(),
        };
        let catalog = ArtifactCatalog::open(dir.path().join("catalog")).unwrap();

        handle_event_batch(
            &[event],
            &[registration],
            access.as_ref(),
            &catalog,
            Some(&callback),
        );

        assert_eq!(callback_calls.load(Ordering::SeqCst), 1);
        assert!(!access.operation_active.load(Ordering::SeqCst));
    }
}
