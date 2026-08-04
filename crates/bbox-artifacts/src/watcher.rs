use std::cell::RefCell;
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
    /// Native catalog attachment identity (plan section 4.16).
    ///
    /// `Selected` re-runs a ladder whose answer moves when attachments
    /// change, and a checkout id names a working tree rather than the
    /// catalog row that grants `artifact_watching`. An attachment id names
    /// exactly one row, so a registration cannot silently follow a
    /// different checkout after an attach, detach, or rebind.
    AttachmentId(String),
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

    /// Native catalog carrier naming one attachment row.
    pub fn for_attachment(
        project_id: impl Into<String>,
        attachment_id: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let attachment_id = attachment_id.into();
        validate_logical_id("attachment id", &attachment_id)?;
        Self::new(
            project_id,
            ArtifactWatchAttachment::AttachmentId(attachment_id),
        )
    }

    /// True for the native catalog carrier. Catalog reconciliation owns
    /// exactly these registrations and must leave the bridge `Selected` and
    /// provisional `CheckoutId` registrations alone.
    pub fn is_attachment(&self) -> bool {
        matches!(self.attachment, ArtifactWatchAttachment::AttachmentId(_))
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
/// while the opaque lease remains alive. Checkout bytes are available only
/// through the descriptor-relative reader, which must reject symlinks in every
/// path component. A denial must not invoke `operation`.
pub trait ArtifactWatchRead {
    fn project_root(&self) -> &Path;

    fn read_relative_file(&self, relative: &Path) -> anyhow::Result<Vec<u8>>;

    /// R17F4: descriptor-relative absence inspection. Returns true when
    /// the relative path does not exist (or is inaccessible) under the
    /// confined checkout descriptor. Implementations must reject symlinks
    /// in every path component, same as read_relative_file.
    fn check_relative_absence(&self, relative: &Path) -> anyhow::Result<bool>;
}

pub trait ArtifactWatchAccess: Send + Sync {
    fn with_discovery(
        &self,
        carrier: &ArtifactWatchCarrier,
        prepare: &mut dyn FnMut(&dyn ArtifactWatchRead) -> anyhow::Result<()>,
        publish: &mut dyn FnMut(&dyn ArtifactWatchRead) -> anyhow::Result<()>,
    ) -> anyhow::Result<()>;
}

/// What installing one registration did. Reconciliation reports these so a
/// duplicate event is visibly a no-op rather than an unmeasured re-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrationChange {
    Unchanged,
    Installed,
    Relocated,
}

/// Whether a registration may follow its carrier to a new root.
#[derive(Debug, Clone, Copy)]
enum RootChange {
    Refuse,
    Relocate,
}

/// Counts from one catalog reconciliation pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArtifactWatchReconcileReport {
    pub added: usize,
    pub removed: usize,
    pub relocated: usize,
    /// Carriers whose discovery lease refused. A no-capability or detached
    /// attachment lands here, and no watcher is installed for it.
    pub failed: usize,
}

impl ArtifactWatchReconcileReport {
    pub fn is_noop(&self) -> bool {
        self.added == 0 && self.removed == 0 && self.relocated == 0
    }
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
        self.register_inner(carrier, artifact_routing, RootChange::Refuse)
            .map(|change| change != RegistrationChange::Unchanged)
    }

    /// Install one registration, optionally accepting a moved root.
    ///
    /// `RootChange::Refuse` is the default and the safety property for the
    /// bridge carriers: a `Selected` or `CheckoutId` carrier that suddenly
    /// resolves elsewhere is drift, and silently following it would watch a
    /// tree the caller never named. Catalog reconciliation passes
    /// `RootChange::Relocate` because relocation is a legitimate, observed
    /// catalog event there, and it must replace the registration exactly
    /// once rather than accumulate a second one.
    fn register_inner(
        &mut self,
        carrier: ArtifactWatchCarrier,
        artifact_routing: bool,
        on_root_change: RootChange,
    ) -> anyhow::Result<RegistrationChange> {
        let existing = self
            .registrations
            .lock()
            .unwrap()
            .iter()
            .find(|registration| registration.carrier == carrier)
            .cloned();
        let relocating = matches!(on_root_change, RootChange::Relocate);
        if !relocating
            && existing
                .as_ref()
                .is_some_and(|registration| registration.artifact_routing || !artifact_routing)
        {
            return Ok(RegistrationChange::Unchanged);
        }

        let mut invoked = false;
        let mut discovered_root = None;
        let mut installed_new_watch = false;
        let registrations = self.registrations.clone();
        let debouncer = &mut self.debouncer;
        let mut operation = |read: &dyn ArtifactWatchRead| {
            if invoked {
                anyhow::bail!("artifact watch authority invoked registration more than once");
            }
            invoked = true;
            let project_root = read.project_root();
            let Some(bbox_root) = canonical_bbox_root(project_root) else {
                return Ok(());
            };
            if !relocating
                && existing
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
                installed_new_watch = true;
            }
            discovered_root = Some(bbox_root);
            Ok(())
        };
        let mut publish = |_read: &dyn ArtifactWatchRead| Ok(());
        let discovery_result = self
            .access
            .with_discovery(&carrier, &mut operation, &mut publish);
        drop(operation);
        if let Err(error) = discovery_result {
            if installed_new_watch && let Some(root) = discovered_root.as_ref() {
                let _ = self.debouncer.unwatch(root);
            }
            return Err(error);
        }
        if !invoked {
            anyhow::bail!("artifact watch authority did not invoke registration");
        }
        let Some(bbox_root) = discovered_root else {
            return Ok(RegistrationChange::Unchanged);
        };

        let mut change = RegistrationChange::Installed;
        let mut vacated_root = None;
        {
            let mut registrations = self.registrations.lock().unwrap();
            if let Some(existing) = registrations
                .iter_mut()
                .find(|registration| registration.carrier == carrier)
            {
                if existing.bbox_root != bbox_root {
                    if !relocating {
                        anyhow::bail!("artifact watch carrier changed roots without removal");
                    }
                    vacated_root = Some(std::mem::replace(&mut existing.bbox_root, bbox_root));
                    change = RegistrationChange::Relocated;
                } else if existing.artifact_routing || !artifact_routing {
                    change = RegistrationChange::Unchanged;
                }
                existing.artifact_routing |= artifact_routing;
            } else {
                registrations.push(WatchRegistration {
                    carrier,
                    bbox_root,
                    artifact_routing,
                });
            }
        }
        if let Some(vacated) = vacated_root {
            self.unwatch_if_unreferenced(&vacated)?;
        }
        Ok(change)
    }

    /// Reconcile the native catalog registrations to exactly `desired`.
    ///
    /// Bridge `Selected` and provisional `CheckoutId` registrations are left
    /// untouched: this reconciler owns only the attachment-id lane. Removals
    /// are idempotent, and re-running with an unchanged desired set installs
    /// and removes nothing, so a duplicate post-commit event is a no-op.
    pub fn reconcile_attachment_registrations(
        &mut self,
        desired: &[ArtifactWatchCarrier],
    ) -> ArtifactWatchReconcileReport {
        let mut report = ArtifactWatchReconcileReport::default();
        let desired = desired
            .iter()
            .filter(|carrier| carrier.is_attachment())
            .cloned()
            .collect::<Vec<_>>();

        let stale = self
            .registrations
            .lock()
            .unwrap()
            .iter()
            .filter(|registration| registration.carrier.is_attachment())
            .filter(|registration| !desired.contains(&registration.carrier))
            .map(|registration| registration.carrier.clone())
            .collect::<Vec<_>>();
        for carrier in stale {
            match self.unwatch_carrier(&carrier) {
                Ok(true) => report.removed += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        project = %carrier.project_id,
                        attachment = ?carrier.attachment,
                        error = %error,
                        "artifact watcher could not remove a stale catalog registration"
                    );
                    report.failed += 1;
                }
            }
        }

        for carrier in desired {
            match self.register_inner(carrier.clone(), true, RootChange::Relocate) {
                Ok(RegistrationChange::Installed) => report.added += 1,
                Ok(RegistrationChange::Relocated) => report.relocated += 1,
                Ok(RegistrationChange::Unchanged) => {}
                Err(error) => {
                    tracing::debug!(
                        project = %carrier.project_id,
                        attachment = ?carrier.attachment,
                        error = %error,
                        "artifact watcher skipped an unavailable catalog carrier"
                    );
                    report.failed += 1;
                }
            }
        }
        report
    }

    fn unwatch_if_unreferenced(&mut self, root: &Path) -> anyhow::Result<()> {
        let still_watched = self
            .registrations
            .lock()
            .unwrap()
            .iter()
            .any(|registration| registration.bbox_root == root);
        if !still_watched {
            self.debouncer.unwatch(root)?;
        }
        Ok(())
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

enum PreparedArtifactAction {
    Install {
        project_id: String,
        local: bool,
        kind: crate::artifacts::ArtifactKind,
        source: String,
        value: serde_json::Value,
    },
    Remove {
        project_id: String,
        local: bool,
        kind: crate::artifacts::ArtifactKind,
        source: PathBuf,
        relative_source: PathBuf,
        expected_name: String,
        expected_version: String,
        expected_content_sha256: Option<String>,
    },
}

impl PreparedArtifactAction {
    fn publish(self, catalog: &ArtifactCatalog, publish_read: &dyn ArtifactWatchRead) {
        match self {
            Self::Install {
                project_id,
                local,
                kind,
                source,
                value,
            } => match catalog.install_value_scoped(
                ArtifactScope::Project {
                    project_id: &project_id,
                    local,
                },
                kind,
                source,
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
                Err(error) => tracing::warn!(
                    artifact_kind = %kind.as_str(),
                    error = %error,
                    "watcher: prepared artifact install failed"
                ),
            },
            Self::Remove {
                project_id,
                local,
                kind,
                source,
                relative_source,
                expected_name,
                expected_version,
                expected_content_sha256,
            } => {
                // R17F4: re-verify source absence under the publication
                // lease before mutating the catalog.
                match publish_read.check_relative_absence(&relative_source) {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::info!(
                            artifact_kind = %kind.as_str(),
                            source = %source.display(),
                            "watcher: removal skipped, source reappeared before publication"
                        );
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(
                            artifact_kind = %kind.as_str(),
                            source = %source.display(),
                            error = %error,
                            "watcher: removal skipped, could not re-verify absence"
                        );
                        return;
                    }
                }
                match catalog.mark_removed_by_source_if_identity(
                    ArtifactScope::Project {
                        project_id: &project_id,
                        local,
                    },
                    kind,
                    &source,
                    &expected_name,
                    &expected_version,
                    expected_content_sha256.as_deref(),
                ) {
                    Ok(Some(meta)) => tracing::info!(
                        "watcher: marked removed {}/{} (project {})",
                        kind.as_str(),
                        meta.name,
                        project_id,
                    ),
                    Ok(None) => {}
                    Err(error) => tracing::warn!(
                        artifact_kind = %kind.as_str(),
                        error = %error,
                        "watcher: prepared artifact removal failed"
                    ),
                }
            }
        }
    }
}

/// Read and validate an authorized event without publishing catalog state.
/// Publication happens only after the daemon adapter has revalidated the
/// lease that guarded these bytes.
fn prepare_artifact_event(
    event: &notify::Event,
    project_id: &str,
    bbox_root: &Path,
    read: &dyn ArtifactWatchRead,
    catalog: &ArtifactCatalog,
) -> Vec<PreparedArtifactAction> {
    let is_create_or_rename_to = matches!(
        event.kind,
        notify::EventKind::Create(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::To
            ))
    );
    let is_remove = matches!(event.kind, notify::EventKind::Remove(_));

    if !is_create_or_rename_to && !is_remove {
        return Vec::new();
    }

    let mut actions = Vec::new();
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
            if let Some(action) = prepare_create(path, project_id, bbox_root, read) {
                actions.push(action);
            }
        } else if let Some(action) = prepare_remove(path, project_id, bbox_root, read, catalog) {
            actions.push(action);
        }
    }
    actions
}

fn handle_event_batch(
    events: &[notify::Event],
    registrations: &[WatchRegistration],
    access: &dyn ArtifactWatchAccess,
    catalog: &ArtifactCatalog,
    on_knowledge_change: Option<&KnowledgeChangeCallback>,
) {
    for registration in registrations {
        let mut repo_store_dirty = false;
        for event in events.iter().filter(|event| {
            event
                .paths
                .iter()
                .any(|path| path.starts_with(&registration.bbox_root))
        }) {
            let mut invoked = false;
            let mut event_repo_store_dirty = false;
            let prepared_actions = RefCell::new(Vec::new());
            let mut operation = |read: &dyn ArtifactWatchRead| {
                if invoked {
                    anyhow::bail!("artifact watch authority invoked event handling more than once");
                }
                invoked = true;
                let project_root = read.project_root();
                let Some(bbox_root) = canonical_bbox_root(project_root) else {
                    anyhow::bail!("authorized artifact watch root has no .bbox directory");
                };
                if bbox_root != registration.bbox_root {
                    anyhow::bail!("artifact watch registration no longer matches authorized root");
                }
                if registration.artifact_routing {
                    *prepared_actions.borrow_mut() = prepare_artifact_event(
                        event,
                        &registration.carrier.project_id,
                        &bbox_root,
                        read,
                        catalog,
                    );
                }
                event_repo_store_dirty =
                    event_touches_repo_store(event, std::slice::from_ref(&bbox_root));
                Ok(())
            };
            let mut publish = |read: &dyn ArtifactWatchRead| {
                for action in prepared_actions.borrow_mut().drain(..) {
                    action.publish(catalog, read);
                }
                Ok(())
            };
            let discovery_result =
                access.with_discovery(&registration.carrier, &mut operation, &mut publish);
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
            } else {
                repo_store_dirty |= event_repo_store_dirty;
            }
        }
        if repo_store_dirty && let Some(callback) = on_knowledge_change {
            callback(&registration.carrier);
        }
    }
}

fn prepare_create(
    path: &Path,
    project_id: &str,
    bbox_root: &Path,
    read: &dyn ArtifactWatchRead,
) -> Option<PreparedArtifactAction> {
    let Some(source) = logical_artifact_source(path, bbox_root) else {
        return None;
    };
    let raw = match read.read_relative_file(&source) {
        Ok(raw) => raw,
        Err(e) => {
            tracing::debug!("watcher: confined read refused {}: {e}", source.display());
            return None;
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("watcher: could not parse {}: {e}", path.display());
            return None;
        }
    };
    let local = is_local_path(&path, bbox_root);
    let kind = match path_to_artifact_kind(&path, bbox_root) {
        Some(k) => k,
        None => return None,
    };
    Some(PreparedArtifactAction::Install {
        project_id: project_id.to_owned(),
        local,
        kind,
        source: source.to_string_lossy().into_owned(),
        value,
    })
}

fn prepare_remove(
    path: &Path,
    project_id: &str,
    bbox_root: &Path,
    read: &dyn ArtifactWatchRead,
    catalog: &ArtifactCatalog,
) -> Option<PreparedArtifactAction> {
    let Ok(relative) = path.strip_prefix(bbox_root) else {
        return None;
    };
    if relative.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return None;
    }
    let Some(source) = logical_artifact_source(path, bbox_root) else {
        return None;
    };
    let local = is_local_path(path, bbox_root);
    let kind = match path_to_artifact_kind(path, bbox_root) {
        Some(k) => k,
        None => return None,
    };
    let scope = ArtifactScope::Project { project_id, local };
    let metadata = match catalog.active_artifact_by_source(scope, kind, &source) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                artifact_kind = %kind.as_str(),
                error = %error,
                "watcher: could not bind removal to active artifact identity"
            );
            return None;
        }
    };
    // R17F4: prove source absence through the descriptor-relative reader
    // (confined checkout descriptor), not via raw path-based exists().
    // source is already relative to the project root (includes .bbox/
    // prefix from logical_artifact_source). This is the first absence
    // check under the discovery lease.
    let relative_source = source.clone();
    match read.check_relative_absence(&relative_source) {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                artifact_kind = %kind.as_str(),
                source = %source.display(),
                "watcher: remove event ignored because source still exists"
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                artifact_kind = %kind.as_str(),
                source = %source.display(),
                error = %error,
                "watcher: could not verify source absence under descriptor"
            );
            return None;
        }
    }
    Some(PreparedArtifactAction::Remove {
        project_id: project_id.to_owned(),
        local,
        kind,
        source,
        relative_source,
        expected_name: metadata.name,
        expected_version: metadata.version,
        expected_content_sha256: metadata.content_sha256,
    })
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

fn logical_artifact_source(path: &Path, bbox_root: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(bbox_root).ok()?;
    if relative.as_os_str().is_empty()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return None;
    }
    Some(PathBuf::from(".bbox").join(relative))
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
        deny_relative_reads: AtomicBool,
        fail_after_operation: AtomicBool,
        operation_calls: AtomicUsize,
        relative_read_calls: AtomicUsize,
        operation_active: AtomicBool,
    }

    struct TestWatchRead<'a> {
        access: &'a TestWatchAccess,
        root: PathBuf,
    }

    impl ArtifactWatchRead for TestWatchRead<'_> {
        fn project_root(&self) -> &Path {
            &self.root
        }

        fn check_relative_absence(&self, relative: &Path) -> anyhow::Result<bool> {
            self.access
                .relative_read_calls
                .fetch_add(1, Ordering::SeqCst);
            if self.access.deny_relative_reads.load(Ordering::SeqCst) {
                anyhow::bail!("confined artifact read denied");
            }
            let mut current = self.root.clone();
            let components = relative.components().collect::<Vec<_>>();
            if components.is_empty() {
                anyhow::bail!("empty relative artifact path");
            }
            for (index, component) in components.iter().enumerate() {
                let std::path::Component::Normal(name) = component else {
                    anyhow::bail!("unsafe relative artifact path");
                };
                current.push(name);
                let metadata = match std::fs::symlink_metadata(&current) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                    Err(e) => return Err(e.into()),
                };
                if metadata.file_type().is_symlink() {
                    anyhow::bail!("symlinked artifact path refused");
                }
                let is_last = index + 1 == components.len();
                if is_last {
                    return Ok(false);
                }
                if !metadata.is_dir() {
                    anyhow::bail!("artifact path component has the wrong type");
                }
            }
            Ok(false)
        }

        fn read_relative_file(&self, relative: &Path) -> anyhow::Result<Vec<u8>> {
            self.access
                .relative_read_calls
                .fetch_add(1, Ordering::SeqCst);
            if self.access.deny_relative_reads.load(Ordering::SeqCst) {
                anyhow::bail!("confined artifact read denied");
            }
            let mut current = self.root.clone();
            let components = relative.components().collect::<Vec<_>>();
            if components.is_empty() {
                anyhow::bail!("empty relative artifact path");
            }
            for (index, component) in components.iter().enumerate() {
                let std::path::Component::Normal(name) = component else {
                    anyhow::bail!("unsafe relative artifact path");
                };
                current.push(name);
                let metadata = std::fs::symlink_metadata(&current)?;
                if metadata.file_type().is_symlink() {
                    anyhow::bail!("symlinked artifact path refused");
                }
                let is_last = index + 1 == components.len();
                if (is_last && !metadata.is_file()) || (!is_last && !metadata.is_dir()) {
                    anyhow::bail!("artifact path component has the wrong type");
                }
            }
            Ok(std::fs::read(current)?)
        }
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
            prepare: &mut dyn FnMut(&dyn ArtifactWatchRead) -> anyhow::Result<()>,
            publish: &mut dyn FnMut(&dyn ArtifactWatchRead) -> anyhow::Result<()>,
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
            let read = TestWatchRead { access: self, root };
            let result = prepare(&read);
            self.operation_active.store(false, Ordering::SeqCst);
            result?;
            if self.fail_after_operation.load(Ordering::SeqCst) {
                anyhow::bail!("artifact watch discovery changed after operation");
            }
            publish(&read)
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
        assert_eq!(
            logical_artifact_source(
                &wf_dir.join("watch-flow.json").canonicalize().unwrap(),
                &bbox_dir.canonicalize().unwrap(),
            )
            .unwrap(),
            PathBuf::from(".bbox/workflows/watch-flow.json"),
            "catalog source identity must not retain the checkout path"
        );

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
    fn prepared_remove_does_not_deactivate_newer_reinstall() {
        let directory = tempdir().unwrap();
        let project_dir = directory.path().canonicalize().unwrap().join("project");
        let bbox_dir = project_dir.join(".bbox");
        let workflow_dir = bbox_dir.join("workflows");
        std::fs::create_dir_all(&workflow_dir).unwrap();
        let catalog = ArtifactCatalog::open(directory.path().join("catalog")).unwrap();
        let access = TestWatchAccess::default();
        let test_read = TestWatchRead {
            access: &access,
            root: project_dir.clone(),
        };
        let source = PathBuf::from(".bbox/workflows/reinstall.json");
        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                crate::artifacts::ArtifactKind::Workflow,
                source.to_string_lossy().into_owned(),
                &serde_json::json!({"name": "reinstall", "version": "1", "steps": []}),
                None,
                None,
                None,
            )
            .unwrap();
        let action = prepare_remove(
            &workflow_dir.join("reinstall.json"),
            "p1",
            &bbox_dir,
            &test_read,
            &catalog,
        )
        .unwrap();

        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                crate::artifacts::ArtifactKind::Workflow,
                source.to_string_lossy().into_owned(),
                &serde_json::json!({"name": "reinstall", "version": "2", "steps": []}),
                None,
                None,
                None,
            )
            .unwrap();
        action.publish(&catalog, &test_read);

        let active = catalog
            .active_artifact_by_source(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                crate::artifacts::ArtifactKind::Workflow,
                &source,
            )
            .unwrap()
            .unwrap();
        assert!(active.active);
        assert_eq!(active.version, "2");
    }

    // R16F4: delayed remove after reinstall with same name+version but
    // different content must NOT deactivate the reinstalled artifact.
    // The content hash identity check prevents a stale removal prepared
    // against the original content from deactivating the reinstall.
    #[test]
    fn r16f4_delayed_remove_after_reinstall_same_version_different_content() {
        let directory = tempdir().unwrap();
        let project_dir = directory.path().canonicalize().unwrap().join("project");
        let bbox_dir = project_dir.join(".bbox");
        let workflow_dir = bbox_dir.join("workflows");
        std::fs::create_dir_all(&workflow_dir).unwrap();
        let catalog = ArtifactCatalog::open(directory.path().join("catalog")).unwrap();
        let access = TestWatchAccess::default();
        let test_read = TestWatchRead {
            access: &access,
            root: project_dir.clone(),
        };
        let source = PathBuf::from(".bbox/workflows/reinstall.json");
        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                crate::artifacts::ArtifactKind::Workflow,
                source.to_string_lossy().into_owned(),
                &serde_json::json!({"name": "reinstall", "version": "1", "steps": [{"action": "original"}]}),
                None,
                None,
                None,
            )
            .unwrap();
        let action = prepare_remove(
            &workflow_dir.join("reinstall.json"),
            "p1",
            &bbox_dir,
            &test_read,
            &catalog,
        )
        .unwrap();

        // Reinstall with SAME name+version but DIFFERENT content.
        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                crate::artifacts::ArtifactKind::Workflow,
                source.to_string_lossy().into_owned(),
                &serde_json::json!({"name": "reinstall", "version": "1", "steps": [{"action": "replaced"}]}),
                None,
                None,
                None,
            )
            .unwrap();
        action.publish(&catalog, &test_read);

        let active = catalog
            .active_artifact_by_source(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                crate::artifacts::ArtifactKind::Workflow,
                &source,
            )
            .unwrap()
            .unwrap();
        assert!(
            active.active,
            "same-version reinstall with different content must NOT be deactivated by stale removal"
        );
    }

    // R16F4: remove event for a source that still exists is ignored.
    // A remove event that fires while the file is still present is
    // spurious and must not deactivate the artifact.
    #[test]
    fn r16f4_remove_event_ignored_when_source_still_exists() {
        let directory = tempdir().unwrap();
        let project_dir = directory.path().canonicalize().unwrap().join("project");
        let bbox_dir = project_dir.join(".bbox");
        let workflow_dir = bbox_dir.join("workflows");
        std::fs::create_dir_all(&workflow_dir).unwrap();
        let catalog = ArtifactCatalog::open(directory.path().join("catalog")).unwrap();
        let access = TestWatchAccess::default();
        let test_read = TestWatchRead {
            access: &access,
            root: project_dir.clone(),
        };
        let source = PathBuf::from(".bbox/workflows/persist.json");
        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                crate::artifacts::ArtifactKind::Workflow,
                source.to_string_lossy().into_owned(),
                &serde_json::json!({"name": "persist", "version": "1", "steps": []}),
                None,
                None,
                None,
            )
            .unwrap();

        // Write the actual source file so it exists when prepare_remove runs.
        std::fs::write(
            workflow_dir.join("persist.json"),
            serde_json::to_vec(&serde_json::json!({"name": "persist", "version": "1"})).unwrap(),
        )
        .unwrap();

        let action = prepare_remove(
            &workflow_dir.join("persist.json"),
            "p1",
            &bbox_dir,
            &test_read,
            &catalog,
        );
        assert!(
            action.is_none(),
            "remove event for existing source must produce no action"
        );
    }

    // R17F4: descriptor-relative absence re-verified under the publication
    // lease. If the source file reappears between prepare and publish, the
    // removal must be skipped.
    #[test]
    fn r17f4_remove_skipped_when_source_reappears_before_publish() {
        let directory = tempdir().unwrap();
        let project_dir = directory.path().canonicalize().unwrap().join("project");
        let bbox_dir = project_dir.join(".bbox");
        let workflow_dir = bbox_dir.join("workflows");
        std::fs::create_dir_all(&workflow_dir).unwrap();
        let catalog = ArtifactCatalog::open(directory.path().join("catalog")).unwrap();
        let access = TestWatchAccess::default();
        let test_read = TestWatchRead {
            access: &access,
            root: project_dir.clone(),
        };
        let source = PathBuf::from(".bbox/workflows/reinstall.json");
        let original = serde_json::json!({"name": "reinstall", "version": "1", "steps": []});
        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                crate::artifacts::ArtifactKind::Workflow,
                source.to_string_lossy().into_owned(),
                &original,
                None,
                None,
                None,
            )
            .unwrap();

        // Source is absent at prepare time.
        let action = prepare_remove(
            &workflow_dir.join("reinstall.json"),
            "p1",
            &bbox_dir,
            &test_read,
            &catalog,
        )
        .unwrap();

        // Recreate the file with identical content before publish.
        std::fs::write(
            workflow_dir.join("reinstall.json"),
            serde_json::to_vec(&original).unwrap(),
        )
        .unwrap();

        // Publish must re-check absence and skip the removal.
        action.publish(&catalog, &test_read);

        let active = catalog
            .active_artifact_by_source(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                crate::artifacts::ArtifactKind::Workflow,
                &source,
            )
            .unwrap()
            .unwrap();
        assert_eq!(active.name, "reinstall", "artifact must remain active");
    }

    // R17F4: the absence check must be descriptor-relative. A swapped
    // parent component (replacing a directory) must not trick the check.
    #[test]
    fn r17f4_remove_detects_swapped_parent_component() {
        let directory = tempdir().unwrap();
        let project_dir = directory.path().canonicalize().unwrap().join("project");
        let bbox_dir = project_dir.join(".bbox");
        let workflow_dir = bbox_dir.join("workflows");
        std::fs::create_dir_all(&workflow_dir).unwrap();
        let catalog = ArtifactCatalog::open(directory.path().join("catalog")).unwrap();
        let access = TestWatchAccess::default();
        let test_read = TestWatchRead {
            access: &access,
            root: project_dir.clone(),
        };
        let source = PathBuf::from(".bbox/workflows/swap.json");
        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "p1",
                    local: false,
                },
                crate::artifacts::ArtifactKind::Workflow,
                source.to_string_lossy().into_owned(),
                &serde_json::json!({"name": "swap", "version": "1", "steps": []}),
                None,
                None,
                None,
            )
            .unwrap();

        // Replace the workflows directory with a symlink to a temp dir
        // that contains swap.json. A path-based check would follow the
        // symlink and see the file as absent. The descriptor-relative
        // check must detect the symlink and refuse.
        let other_dir = directory.path().join("other");
        std::fs::create_dir_all(&other_dir).unwrap();
        std::fs::remove_dir_all(&workflow_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&other_dir, &workflow_dir).unwrap();

        let action = prepare_remove(
            &workflow_dir.join("swap.json"),
            "p1",
            &bbox_dir,
            &test_read,
            &catalog,
        );
        // The descriptor-relative reader rejects the symlink in the path
        // component, so prepare_remove returns None (no action).
        assert!(
            action.is_none(),
            "symlinked parent component must not bypass absence check"
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
    fn provisional_carrier_resolves_without_artifact_routing() {
        let dir = tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("checkout");
        std::fs::create_dir_all(project.join(".bbox/knowledge")).unwrap();
        let carrier = ArtifactWatchCarrier::checkout("proj-gamma", "checkout-gamma").unwrap();
        let access = TestWatchAccess::default();
        access.insert(carrier.clone(), project.clone());
        let mut discovered = None;
        let mut publish = |_read: &dyn ArtifactWatchRead| Ok(());
        access
            .with_discovery(
                &carrier,
                &mut |read| {
                    discovered = Some(registration(carrier.clone(), read.project_root(), false));
                    Ok(())
                },
                &mut publish,
            )
            .unwrap();

        let discovered = discovered.unwrap();
        assert_eq!(discovered.carrier, carrier);
        assert_eq!(
            discovered.bbox_root,
            project.join(".bbox").canonicalize().unwrap()
        );
        assert!(!discovered.artifact_routing);
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
        access.insert(carrier.clone(), project.clone());
        let catalog = Arc::new(ArtifactCatalog::open(dir.path().join("catalog")).unwrap());
        let registration = registration(carrier, &project, true);
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
            &[registration],
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
    fn artifact_event_reads_only_through_authority_reader() {
        let dir = tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("project");
        let workflows = project.join(".bbox/workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        let artifact_path = workflows.join("confined-flow.json");
        std::fs::write(
            &artifact_path,
            serde_json::to_vec(&make_workflow("confined-flow")).unwrap(),
        )
        .unwrap();
        let carrier = ArtifactWatchCarrier::selected("proj-confined").unwrap();
        let access = TestWatchAccess::default();
        access.insert(carrier.clone(), project.clone());
        access.deny_relative_reads.store(true, Ordering::SeqCst);
        let event = notify::Event {
            kind: notify::EventKind::Create(notify::event::CreateKind::File),
            paths: vec![artifact_path],
            attrs: Default::default(),
        };
        let catalog = ArtifactCatalog::open(dir.path().join("catalog")).unwrap();

        handle_event_batch(
            &[event],
            &[registration(carrier, &project, true)],
            &access,
            &catalog,
            None,
        );

        assert_eq!(access.relative_read_calls.load(Ordering::SeqCst), 1);
        assert!(
            catalog
                .load_artifact_value_scoped(
                    Some("proj-confined"),
                    crate::artifacts::ArtifactKind::Workflow,
                    "confined-flow",
                )
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_artifact_leaf_is_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        let workflows = project.join(".bbox/workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        let outside = root.join("outside.json");
        std::fs::write(
            &outside,
            serde_json::to_vec(&make_workflow("linked-flow")).unwrap(),
        )
        .unwrap();
        let linked = workflows.join("linked-flow.json");
        symlink(&outside, &linked).unwrap();
        let carrier = ArtifactWatchCarrier::selected("proj-linked").unwrap();
        let access = TestWatchAccess::default();
        access.insert(carrier.clone(), project.clone());
        let event = notify::Event {
            kind: notify::EventKind::Create(notify::event::CreateKind::File),
            paths: vec![linked],
            attrs: Default::default(),
        };
        let catalog = ArtifactCatalog::open(root.join("catalog")).unwrap();

        handle_event_batch(
            &[event],
            &[registration(carrier, &project, true)],
            &access,
            &catalog,
            None,
        );

        assert_eq!(access.relative_read_calls.load(Ordering::SeqCst), 1);
        assert!(
            catalog
                .load_artifact_value_scoped(
                    Some("proj-linked"),
                    crate::artifacts::ArtifactKind::Workflow,
                    "linked-flow",
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn repo_change_callback_runs_only_after_discovery_scope_succeeds() {
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
                !access_cb.operation_active.load(Ordering::SeqCst),
                "repository callback must run only after discovery revalidation succeeds"
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

    #[test]
    fn post_operation_authority_failure_publishes_no_artifact_or_reload() {
        let dir = tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("project");
        let workflows = project.join(".bbox/workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        let artifact_path = workflows.join("stale-flow.json");
        std::fs::write(
            &artifact_path,
            serde_json::to_string(&make_workflow("stale-flow")).unwrap(),
        )
        .unwrap();
        let carrier = ArtifactWatchCarrier::selected("proj-stale").unwrap();
        let access = Arc::new(TestWatchAccess::default());
        access.insert(carrier.clone(), project.clone());
        access.fail_after_operation.store(true, Ordering::SeqCst);
        let callback_calls = Arc::new(AtomicUsize::new(0));
        let callback_calls_cb = callback_calls.clone();
        let callback: KnowledgeChangeCallback = Arc::new(move |_| {
            callback_calls_cb.fetch_add(1, Ordering::SeqCst);
        });
        let event = notify::Event {
            kind: notify::EventKind::Create(notify::event::CreateKind::File),
            paths: vec![artifact_path],
            attrs: Default::default(),
        };
        let catalog = ArtifactCatalog::open(dir.path().join("catalog")).unwrap();

        handle_event_batch(
            &[event],
            &[registration(carrier, &project, true)],
            access.as_ref(),
            &catalog,
            Some(&callback),
        );

        assert!(
            catalog
                .load_artifact_value_scoped(
                    Some("proj-stale"),
                    crate::artifacts::ArtifactKind::Workflow,
                    "stale-flow",
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(callback_calls.load(Ordering::SeqCst), 0);
    }

    fn project_with_bbox(root: &Path, name: &str) -> PathBuf {
        let project = root.join(name);
        std::fs::create_dir_all(project.join(".bbox").join("workflows")).unwrap();
        project
    }

    fn reconciling_watcher(
        access: Arc<TestWatchAccess>,
        catalog: Arc<ArtifactCatalog>,
    ) -> BbxWatcher {
        BbxWatcher::start(Vec::new(), access, catalog, None).unwrap()
    }

    fn registered_carriers(watcher: &BbxWatcher) -> Vec<ArtifactWatchCarrier> {
        watcher
            .registrations
            .lock()
            .unwrap()
            .iter()
            .map(|registration| registration.carrier.clone())
            .collect()
    }

    /// The first reconciliation installs the capable attachment; a second
    /// pass over the same catalog state changes nothing. A post-commit
    /// observer may deliver the same event twice, so idempotence is the
    /// property, not a happy accident of ordering.
    #[test]
    fn catalog_reconciliation_installs_once_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = project_with_bbox(&root, "checkout-a");
        let carrier =
            ArtifactWatchCarrier::for_attachment("proj-alpha", "att_0000000000000000000000000001")
                .unwrap();
        let access = Arc::new(TestWatchAccess::default());
        access.insert(carrier.clone(), project.clone());
        let catalog = Arc::new(ArtifactCatalog::open(&root.join("catalog")).unwrap());
        let mut watcher = reconciling_watcher(access, catalog);

        let first = watcher.reconcile_attachment_registrations(std::slice::from_ref(&carrier));
        assert_eq!(first.added, 1);
        assert_eq!(first.removed, 0);
        assert_eq!(first.relocated, 0);
        assert_eq!(registered_carriers(&watcher), vec![carrier.clone()]);

        let second = watcher.reconcile_attachment_registrations(std::slice::from_ref(&carrier));
        assert!(second.is_noop(), "{second:?}");
        assert_eq!(registered_carriers(&watcher), vec![carrier]);
    }

    /// Detach drops the attachment from the desired set. The registration
    /// goes with it, and removing it again is a no-op rather than an error.
    #[test]
    fn catalog_reconciliation_removes_a_detached_registration_idempotently() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = project_with_bbox(&root, "checkout-a");
        let carrier =
            ArtifactWatchCarrier::for_attachment("proj-alpha", "att_0000000000000000000000000001")
                .unwrap();
        let access = Arc::new(TestWatchAccess::default());
        access.insert(carrier.clone(), project);
        let catalog = Arc::new(ArtifactCatalog::open(&root.join("catalog")).unwrap());
        let mut watcher = reconciling_watcher(access, catalog);
        watcher.reconcile_attachment_registrations(std::slice::from_ref(&carrier));

        let detached = watcher.reconcile_attachment_registrations(&[]);
        assert_eq!(detached.removed, 1);
        assert!(registered_carriers(&watcher).is_empty());

        let again = watcher.reconcile_attachment_registrations(&[]);
        assert!(again.is_noop(), "{again:?}");
    }

    /// A relocated attachment keeps its identity and moves its root exactly
    /// once. The failure this guards is two registrations for one
    /// attachment, which would double every event it sees.
    #[test]
    fn catalog_reconciliation_relocates_a_moved_attachment_exactly_once() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let original = project_with_bbox(&root, "checkout-a");
        let moved = project_with_bbox(&root, "checkout-b");
        let carrier =
            ArtifactWatchCarrier::for_attachment("proj-alpha", "att_0000000000000000000000000001")
                .unwrap();
        let access = Arc::new(TestWatchAccess::default());
        access.insert(carrier.clone(), original.clone());
        let catalog = Arc::new(ArtifactCatalog::open(&root.join("catalog")).unwrap());
        let mut watcher = reconciling_watcher(access.clone(), catalog);
        watcher.reconcile_attachment_registrations(std::slice::from_ref(&carrier));

        access.insert(carrier.clone(), moved.clone());
        let relocation = watcher.reconcile_attachment_registrations(std::slice::from_ref(&carrier));
        assert_eq!(relocation.relocated, 1);
        assert_eq!(relocation.added, 0);

        let registrations = watcher.registrations.lock().unwrap().clone();
        assert_eq!(registrations.len(), 1, "{registrations:#?}");
        assert_eq!(
            registrations[0].bbox_root,
            moved.join(".bbox").canonicalize().unwrap()
        );
        drop(registrations);

        let settled = watcher.reconcile_attachment_registrations(std::slice::from_ref(&carrier));
        assert!(settled.is_noop(), "{settled:?}");
    }

    /// An attachment without `artifact_watching` produces no carrier at all,
    /// so reconciliation installs no watcher for it. The daemon-side carrier
    /// projection owns that filter; here the equivalent is a carrier whose
    /// discovery lease refuses.
    #[test]
    fn a_carrier_whose_discovery_refuses_installs_no_watcher() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let carrier =
            ArtifactWatchCarrier::for_attachment("proj-alpha", "att_0000000000000000000000000001")
                .unwrap();
        // Not inserted into the fake authority: discovery refuses, exactly
        // as a capability-denied lease would.
        let access = Arc::new(TestWatchAccess::default());
        let catalog = Arc::new(ArtifactCatalog::open(&root.join("catalog")).unwrap());
        let mut watcher = reconciling_watcher(access, catalog);

        let report = watcher.reconcile_attachment_registrations(std::slice::from_ref(&carrier));
        assert_eq!(report.added, 0);
        assert_eq!(report.failed, 1);
        assert!(registered_carriers(&watcher).is_empty());
    }

    /// Reconciliation owns the attachment lane only. A bridge `Selected`
    /// registration and a provisional `CheckoutId` one survive a catalog
    /// pass that names neither.
    #[test]
    fn catalog_reconciliation_leaves_bridge_registrations_alone() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = project_with_bbox(&root, "checkout-a");
        let selected = ArtifactWatchCarrier::selected("proj-alpha").unwrap();
        let checkout = ArtifactWatchCarrier::checkout("proj-alpha", "checkout-1").unwrap();
        let access = Arc::new(TestWatchAccess::default());
        access.insert(selected.clone(), project.clone());
        access.insert(checkout.clone(), project);
        let catalog = Arc::new(ArtifactCatalog::open(&root.join("catalog")).unwrap());
        let mut watcher = reconciling_watcher(access, catalog);
        watcher.watch_project(selected.clone()).unwrap();
        watcher.watch_repo_store(checkout.clone()).unwrap();

        let report = watcher.reconcile_attachment_registrations(&[]);
        assert!(report.is_noop(), "{report:?}");
        let mut surviving = registered_carriers(&watcher);
        surviving.sort();
        let mut expected = vec![selected, checkout];
        expected.sort();
        assert_eq!(surviving, expected);
    }

    /// Durable artifact metadata is catalog state, not watcher state. A
    /// project whose every registration is gone keeps the artifacts already
    /// installed for it; only filesystem discovery stops.
    #[test]
    fn durable_artifact_metadata_survives_with_no_watcher() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = project_with_bbox(&root, "checkout-a");
        let carrier =
            ArtifactWatchCarrier::for_attachment("proj-alpha", "att_0000000000000000000000000001")
                .unwrap();
        let access = Arc::new(TestWatchAccess::default());
        access.insert(carrier.clone(), project);
        let catalog = Arc::new(ArtifactCatalog::open(&root.join("catalog")).unwrap());
        catalog
            .install_value_scoped(
                ArtifactScope::Project {
                    project_id: "proj-alpha",
                    local: false,
                },
                crate::artifacts::ArtifactKind::Workflow,
                ".bbox/workflows/durable.json".to_string(),
                &make_workflow("durable"),
                None,
                None,
                None,
            )
            .unwrap();
        let mut watcher = reconciling_watcher(access, catalog.clone());
        watcher.reconcile_attachment_registrations(std::slice::from_ref(&carrier));

        assert_eq!(watcher.reconcile_attachment_registrations(&[]).removed, 1);
        assert!(registered_carriers(&watcher).is_empty());
        assert!(
            catalog
                .load_artifact_value_scoped(
                    Some("proj-alpha"),
                    crate::artifacts::ArtifactKind::Workflow,
                    "durable",
                )
                .unwrap()
                .is_some(),
            "durable artifact metadata must outlive its watcher registration"
        );
    }
}
