//! Daemon-side wiring for `crates/bbox-connectors` (stage 2 of
//! `design/connectors/remote-source-connectors.md`): connector alias
//! resolution, the async/blocking-pool bridge for `sync_mount`, and mount ->
//! project registration/teardown. `bbox-connectors` stays a plain library -
//! its own AGENTS.md is explicit that it "must never spawn its own tokio
//! tasks or threads" and that stage 2 (the daemon) is expected to pool its
//! synchronous fs calls onto the blocking lane rather than call them from a
//! tokio worker. Everything actor/runtime-shaped for mounts lives here
//! instead, mirroring the tools/storage_gc.rs (tool) + server/storage_gc.rs
//! (background loop) split: this module is the shared domain logic both
//! `tools::mounts` (MCP handlers) and `server::mount_sync` (the periodic
//! background loop) call into, so there is exactly one sync code path.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use bbox_config::config::{Config, ConnectorAliasConfig};
use bbox_connectors::{
    ChangeCursor, GitConnector, MountConfig, MountPolicy, MountRecord, MountStore,
    RemoteSourceConnector, SyncSummary, compute_mount_id, sync_mount,
};

use crate::projects::ProjectSource;
use crate::server::state::SharedState;

/// A sync attempt found the mount already mid-pass. Distinguished (via
/// `anyhow::Error::downcast_ref`) from an ordinary sync failure so callers -
/// the manual `bbox_mount_sync` tool and the periodic loop alike - can shape
/// a clean "busy" outcome instead of reporting a generic error.
#[derive(Debug)]
pub(crate) struct MountBusy(pub(crate) String);

impl std::fmt::Display for MountBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sync already in flight for mount `{}`", self.0)
    }
}

impl std::error::Error for MountBusy {}

/// RAII per-mount sync lock. Acquired by every sync pass (initial,
/// operator-triggered, and periodic) and by mount unregistration, so a
/// concurrent sync and unregister/re-sync can never race the same mount.
struct InFlightGuard {
    locks: Arc<parking_lot::Mutex<HashSet<String>>>,
    mount_id: String,
}

impl InFlightGuard {
    fn acquire(locks: &Arc<parking_lot::Mutex<HashSet<String>>>, mount_id: &str) -> Result<Self> {
        let mut guard = locks.lock();
        if !guard.insert(mount_id.to_string()) {
            return Err(MountBusy(mount_id.to_string()).into());
        }
        drop(guard);
        Ok(Self {
            locks: locks.clone(),
            mount_id: mount_id.to_string(),
        })
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.locks.lock().remove(&self.mount_id);
    }
}

/// Resolve a mount's `connector` alias (operator-facing - `"git"`, or a
/// future `"gdrive-personal"`) to a connector instance. The builtin `git`
/// type is always available with no config (`ConnectorsConfig::get`
/// synthesizes it); any other alias must be declared under
/// `[connectors.<alias>]` in config.toml with an explicit `type` field, or
/// this fails closed with remediation text.
pub(crate) fn resolve_connector(
    cfg: &Config,
    alias: &str,
) -> Result<Arc<dyn RemoteSourceConnector>> {
    let Some(config) = cfg.connectors.get(alias) else {
        anyhow::bail!(
            "unknown connector alias `{alias}`; declare it under [connectors.{alias}] in \
             config.toml with a `type` field (only `type = \"git\"` is supported today), or use \
             the builtin `git` alias directly (it needs no config)"
        );
    };
    match config {
        ConnectorAliasConfig::Git(_) => Ok(Arc::new(GitConnector::new())),
    }
}

/// Redact credential-bearing scope text before it reaches `MountRecord`,
/// logs, or any error surface. Connector-specific: only `git` embeds
/// userinfo in its scope string today; other connector kinds pass their
/// scope through unchanged until they need their own redaction.
fn redact_scope(connector_kind: &str, scope: &str) -> String {
    match connector_kind {
        "git" => bbox_connectors::git_connector::redact_scope(scope),
        _ => scope.to_string(),
    }
}

fn default_materialization_root(store_dir: &Path, mount_id: &str) -> PathBuf {
    store_dir.join("mounts").join(mount_id)
}

/// Manifest path for a mount: a sibling of its materialization root (never
/// inside it, so the walker/indexer never sees it as a project file).
fn manifest_path(store_dir: &Path, mount_id: &str) -> PathBuf {
    store_dir
        .join("mounts")
        .join(format!("{mount_id}.manifest.json"))
}

fn mount_config_for(record: &MountRecord, connector_kind: &str, root: PathBuf) -> MountConfig {
    MountConfig {
        connector_kind: connector_kind.to_string(),
        remote_scope: record.remote_scope.clone(),
        materialization_root: root,
        policy: record.policy.clone(),
        options: Default::default(),
    }
}

/// Selector match for a registered mount: exact `mount_id`, else
/// materialization root, else (redacted) remote scope - mirrors the
/// project-selector resolver's layered fallback shape at a much smaller
/// scale (no alias layer for mounts yet).
fn resolve_mount(mounts: &MountStore, selector: &str) -> Option<MountRecord> {
    if let Some(record) = mounts.get(selector) {
        return Some(record.clone());
    }
    mounts
        .list()
        .into_iter()
        .find(|record| record.materialization_root == selector || record.remote_scope == selector)
}

pub(crate) fn resolve_mount_id(state: &Arc<SharedState>, selector: &str) -> Result<String> {
    let mounts = state.mounts.read();
    resolve_mount(&mounts, selector)
        .map(|record| record.mount_id)
        .with_context(|| format!("mount not registered: {selector}"))
}

pub(crate) fn list_mounts(state: &Arc<SharedState>) -> serde_json::Value {
    serde_json::json!({ "mounts": state.mounts.read().list() })
}

/// `sync_mount` (bbox-connectors) is async but performs its filesystem work
/// synchronously inline (crate AGENTS.md: stage 1 callers run it inline,
/// stage 2 - us - pools it). Bridging an async fn full of interleaved sync
/// I/O and awaited `tokio::process::Command` calls onto the blocking pool
/// means running the WHOLE future inside `spawn_blocking` and driving it
/// with `Handle::block_on` from there (the blocking-pool thread still has
/// access to the ambient runtime's I/O driver via the handle) - not
/// `tokio::task::spawn_blocking` around a plain closure, since the body is
/// genuinely async, and not `block_in_place` alone, since that keeps the
/// call on a normal worker thread rather than moving it off one.
async fn run_sync_on_blocking_pool(
    connector: Arc<dyn RemoteSourceConnector>,
    mount_config: MountConfig,
    manifest_path: PathBuf,
    cursor: Option<ChangeCursor>,
) -> Result<SyncSummary> {
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(sync_mount(
            connector.as_ref(),
            &mount_config,
            &manifest_path,
            cursor,
        ))
    })
    .await
    .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
    .and_then(std::convert::identity)
}

/// Validate + create a mount record and kick off its initial sync in the
/// background. Mirrors `bbox_project_register`'s registration-time-indexing
/// split (`src/tools/projects.rs`): the light lock + durable persist happen
/// before this returns, heavy work (the actual sync, then project
/// registration) runs after, on a spawned task.
pub(crate) async fn register_mount(
    state: &Arc<SharedState>,
    connector_alias: String,
    scope: String,
    path: Option<String>,
    policy: MountPolicy,
    project_alias: Option<String>,
) -> Result<serde_json::Value> {
    let cfg_snapshot = state.config.read().clone();
    let connector = resolve_connector(&cfg_snapshot, &connector_alias)?;
    let connector_kind = connector.kind();
    let redacted_scope = redact_scope(connector_kind, &scope);
    // Credential-embedded scopes are rejected outright rather than accepted
    // and broken later: only the redacted scope is persisted, and every sync
    // after registration rebuilds its MountConfig from the record, so a
    // token-bearing URL would validate here and then fail on all syncs
    // against the `***@` form.
    if redacted_scope != scope {
        anyhow::bail!(
            "scope embeds credentials (userinfo in the URL); embedded tokens are never \
             persisted, so later syncs would run against the redacted URL and fail. Register \
             the credential-free URL and provide auth ambiently (ssh agent or a git credential \
             helper on the daemon host)"
        );
    }
    let mount_id = compute_mount_id(connector_kind, &redacted_scope);

    // List-before-create: re-registering the same (connector, scope) pair
    // returns the existing mount rather than minting a duplicate.
    if let Some(existing) = state.mounts.read().get(&mount_id).cloned() {
        return Ok(serde_json::json!({
            "status": "exists",
            "mount": existing,
        }));
    }

    if let Some(p) = &path
        && !Path::new(p).is_absolute()
    {
        anyhow::bail!(
            "path must be an absolute directory when overriding the materialization root: {p}"
        );
    }
    let root = path
        .map(PathBuf::from)
        .unwrap_or_else(|| default_materialization_root(&state.store_dir, &mount_id));

    // Fail closed: validate the connector can actually reach this scope
    // before any mount state is persisted.
    let probe_config = MountConfig {
        connector_kind: connector_kind.to_string(),
        remote_scope: scope.clone(),
        materialization_root: root.clone(),
        policy: policy.clone(),
        options: Default::default(),
    };
    connector
        .validate(&probe_config)
        .await
        .with_context(|| format!("validating connector `{connector_alias}` against scope"))?;

    let record = MountRecord {
        mount_id: mount_id.clone(),
        connector: connector_alias.clone(),
        remote_scope: redacted_scope,
        materialization_root: root.to_string_lossy().into_owned(),
        project_id: None,
        cursor: None,
        policy,
        created_at: crate::util::now_iso(),
        last_sync: None,
    };
    {
        let mut mounts = state.mounts.write();
        mounts.upsert(record.clone());
    }
    state.persist_mounts_durable().await?;

    // Initial sync + project registration run in the background - same
    // shape as bbox_project_register's "indexing: scheduled/background"
    // response, and the exact same function the periodic loop and the
    // manual bbox_mount_sync tool call, so there is one sync code path.
    let state_for_sync = state.clone();
    let mount_id_for_sync = mount_id.clone();
    tokio::spawn(async move {
        if let Err(err) =
            run_sync_and_register_project(&state_for_sync, &mount_id_for_sync, false, project_alias)
                .await
            && err.downcast_ref::<MountBusy>().is_none()
        {
            tracing::warn!(mount_id = %mount_id_for_sync, error = %err, "initial mount sync failed");
        }
    });

    Ok(serde_json::json!({
        "status": "ok",
        "mount_id": mount_id,
        "connector": connector_alias,
        "materialization_root": record.materialization_root,
        "sync": "started",
        "detail": "mount registered; initial sync and project registration run in the \
                   background - poll bbox_mount_list for sync state and project_id",
    }))
}

/// One sync pass for an already-registered mount: resolve its connector,
/// run `sync_mount` on the blocking pool, persist the updated cursor/
/// last_sync, and nudge the reindexer when files actually changed. Refuses
/// (via `MountBusy`) when a sync for this mount is already in flight.
pub(crate) async fn sync_one_mount(
    state: &Arc<SharedState>,
    mount_id: &str,
    full: bool,
) -> Result<SyncSummary> {
    let _guard = InFlightGuard::acquire(&state.mount_sync_locks, mount_id)?;

    let (record, cfg_snapshot) = {
        let mounts = state.mounts.read();
        let record = mounts
            .get(mount_id)
            .with_context(|| format!("mount not registered: {mount_id}"))?
            .clone();
        (record, state.config.read().clone())
    };

    let connector = resolve_connector(&cfg_snapshot, &record.connector)?;
    let root = PathBuf::from(&record.materialization_root);
    let mount_config = mount_config_for(&record, connector.kind(), root);
    let manifest = manifest_path(&state.store_dir, mount_id);
    let cursor = if full { None } else { record.cursor.clone() };

    let summary = run_sync_on_blocking_pool(connector, mount_config, manifest, cursor).await?;

    {
        let mut mounts = state.mounts.write();
        if let Some(mut current) = mounts.get(mount_id).cloned() {
            current.cursor = summary.next_cursor.clone();
            current.last_sync = Some(summary.clone());
            mounts.upsert(current);
        }
    }
    state.persist_mounts_durable().await?;

    let changed = summary.fetched > 0 || summary.deleted > 0 || summary.renamed > 0;
    if changed {
        // Same out-of-band trigger the `.bbox/knowledge` watcher uses to
        // force a background reindex pass without waiting the full
        // interval (`src/server/state.rs` doc on `reindex_dirty`). Not
        // strictly required for content changes - the periodic reindexer's
        // `needs_reindex` mtime/size scan already walks every registered
        // project root, including a mount's materialization root, and would
        // catch these files on its own - but this avoids the wait, the same
        // "existing reindex path" registration-time indexing already rides.
        state
            .reindex_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // A mount's repo-owned `.bbox` state (knowledge, gaps) changes ONLY
        // through sync passes - there is no fs watcher on mount roots - so a
        // changed sync must also refresh the repo-owned stores the way the
        // watcher does for local projects. No-op when the mount predates its
        // project registration (project_id None: registration below runs the
        // same refresh itself).
        let has_project = {
            let mounts = state.mounts.read();
            mounts
                .get(mount_id)
                .map(|record| record.project_id.is_some())
                .unwrap_or(false)
        };
        if has_project {
            crate::server::routes::sync_kb_project_roots(state);
            crate::server::routes::enqueue_project_knowledge_embeds(
                state,
                &record.materialization_root,
            );
        }
    }

    Ok(summary)
}

/// `sync_one_mount` plus: on the first successful sync (mount has no
/// `project_id` yet and its root now exists), register the materialization
/// root as a normal project and flip its `source` to `RemoteMount`. Called
/// uniformly by the initial-registration background task, the manual
/// `bbox_mount_sync` tool, and the periodic sync loop, so there is exactly
/// one path that can transition a mount from unregistered to
/// project-backed.
pub(crate) async fn run_sync_and_register_project(
    state: &Arc<SharedState>,
    mount_id: &str,
    full: bool,
    project_alias: Option<String>,
) -> Result<SyncSummary> {
    let summary = sync_one_mount(state, mount_id, full).await?;

    let needs_register = {
        let mounts = state.mounts.read();
        mounts
            .get(mount_id)
            .map(|record| record.project_id.is_none())
            .unwrap_or(false)
    };
    if !needs_register {
        return Ok(summary);
    }

    let root = {
        let mounts = state.mounts.read();
        mounts
            .get(mount_id)
            .map(|record| PathBuf::from(&record.materialization_root))
    };
    let Some(root) = root else {
        return Ok(summary);
    };
    if !root.is_dir() {
        // Nothing materialized yet (e.g. every entry was policy-skipped on
        // an empty-so-far scope) - a later pass with real content retries.
        return Ok(summary);
    }

    let project_record = {
        let mut projects = state.projects.write();
        let record = projects.register_path(&root)?;
        if let Some(alias) = &project_alias {
            let declared: std::collections::BTreeSet<String> =
                std::iter::once(alias.clone()).collect();
            projects.sync_declared_aliases(&record.project_id, &declared)?;
        }
        projects.set_source(
            &record.project_id,
            ProjectSource::RemoteMount {
                mount_id: mount_id.to_string(),
            },
        )?
    };
    state.persist_projects_durable().await?;

    {
        let mut mounts = state.mounts.write();
        if let Some(mut record) = mounts.get(mount_id).cloned() {
            record.project_id = Some(project_record.project_id.clone());
            // Keep the mount's stored root byte-identical to the project's
            // canonical_path - `register_path` canonicalizes (resolving
            // symlinks, e.g. macOS's /var -> /private/var), so the two can
            // otherwise diverge in representation while pointing at the
            // same directory, which would silently break any exact-string
            // comparison against `canonical_path` downstream (project ref
            // counts, `resolve_mount` by materialization_root, ...).
            record.materialization_root = project_record.canonical_path.clone();
            mounts.upsert(record);
        }
    }
    state.persist_mounts_durable().await?;

    // Mirror bbox_project_register's post-registration repo-owned load
    // (src/tools/projects.rs): pull the mount's committed `.bbox/knowledge/`
    // into the query surface and enqueue its embeds, so a mounted repo's
    // durable knowledge is searchable without a daemon restart.
    crate::server::routes::sync_kb_project_roots(state);
    crate::server::routes::enqueue_project_knowledge_embeds(state, &project_record.canonical_path);

    Ok(summary)
}

/// Unregister a mount: mirrors `bbox_project_unregister`'s attached-state
/// refusal semantics for the underlying project (refuses when project-scoped
/// refs still exist unless `force=true`), then drops the mount record.
/// `remove_files=true` additionally deletes the materialization root and
/// manifest - refused unless the root carries the ownership marker stamped
/// at materialization time, so a misconfigured path can never aim deletion
/// at operator data (design: "Materialization identity and path safety").
pub(crate) async fn unregister_mount(
    state: &Arc<SharedState>,
    selector: &str,
    remove_files: bool,
    force: bool,
) -> Result<serde_json::Value> {
    let record = {
        let mounts = state.mounts.read();
        resolve_mount(&mounts, selector)
            .with_context(|| format!("mount not registered: {selector}"))?
    };

    // Refuse a concurrent unregister-during-sync race the same way a
    // concurrent sync-during-sync race is refused.
    let _guard = InFlightGuard::acquire(&state.mount_sync_locks, &record.mount_id)?;

    let mut ref_counts = serde_json::Value::Null;
    let mut total_refs: u64 = 0;
    if let Some(project_id) = &record.project_id
        && let Some(project_record) = state.projects.read().resolve(project_id)?
    {
        let counts =
            crate::server::routes::project_ref_counts(state, &project_record.canonical_path)?;
        total_refs = counts
            .as_object()
            .map(|m| m.values().filter_map(|v| v.as_u64()).sum())
            .unwrap_or(0);
        ref_counts = counts;
    }

    if total_refs > 0 && !force {
        anyhow::bail!(
            "mount `{}` project still has {} project-scoped refs; re-run with force=true to \
             orphan them, or migrate them off the project first. counts: {}",
            record.mount_id,
            total_refs,
            ref_counts,
        );
    }

    if let Some(project_id) = &record.project_id {
        {
            let mut projects = state.projects.write();
            // Best-effort: the project may already be gone (hand-
            // unregistered); a mount whose project vanished out from under
            // it should still be droppable.
            let _ = projects.unregister_project(project_id);
        }
        state.persist_projects_durable().await?;
        crate::server::routes::sync_kb_project_roots(state);
        state.nudge_edge_index_rebuild();
    }

    {
        let mut mounts = state.mounts.write();
        mounts.remove(&record.mount_id);
    }
    state.persist_mounts_durable().await?;

    let mut removed_files = false;
    if remove_files {
        let root = PathBuf::from(&record.materialization_root);
        let manifest = manifest_path(&state.store_dir, &record.mount_id);
        removed_files =
            tokio::task::spawn_blocking(move || remove_materialization(&root, &manifest))
                .await
                .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
                .and_then(std::convert::identity)?;
    }

    Ok(serde_json::json!({
        "status": "ok",
        "mount_id": record.mount_id,
        "project_id": record.project_id,
        "ref_counts": ref_counts,
        "orphaned_refs": total_refs,
        "forced": total_refs > 0 && force,
        "removed_files": removed_files,
    }))
}

/// Synchronous fs boundary - run only inside `spawn_blocking` (see
/// `unregister_mount`), matching the `bbox-connectors` materialize.rs
/// convention this mirrors.
#[allow(clippy::disallowed_methods)]
fn remove_materialization(root: &Path, manifest: &Path) -> Result<bool> {
    bbox_connectors::materialize::verify_ownership_marker(root)
        .context("refusing remove_files: materialization root ownership check failed")?;
    if root.is_dir() {
        std::fs::remove_dir_all(root)
            .with_context(|| format!("removing materialization root {}", root.display()))?;
    }
    if manifest.exists() {
        std::fs::remove_file(manifest)
            .with_context(|| format!("removing manifest {}", manifest.display()))?;
    }
    Ok(true)
}
