use futures::StreamExt;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::{Query, State as AxumState};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use super::state::SharedState;
use crate::artifacts::{
    self, ArtifactInstallParams, ArtifactListParams, ArtifactRemoveParams, ArtifactSupersedeParams,
};
use crate::orchestration;
use crate::projects::ProjectRecord;

/// True iff the bind host string resolves to a loopback address.
/// Recognized: `127.0.0.0/8` literals, `localhost` (string match —
/// resolution is host-config dependent and we keep it conservative),
/// `::1`. `0.0.0.0` and any other IPv4 are treated as non-loopback.
pub(crate) fn is_loopback_bind(bind_host: &str) -> bool {
    let h = bind_host.trim();
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

// ── Admin HTTP endpoints (plain JSON; no MCP framing) ──────────────
//
// These wrap the same operations the MCP tools expose so install
// scripts can use plain `curl`. They're loopback-only via the listener
// binding.

pub(crate) async fn admin_runtime_metrics() -> impl axum::response::IntoResponse {
    axum::Json(json!({
        "status": "ok",
        "snapshot": super::runtime_metrics::latest_runtime_metrics_snapshot(),
        // Separate top-level key, not folded into `snapshot`: the snapshot is
        // republished by a task on the serving runtime every 60s and goes
        // stale precisely when the runtime stalls, whereas these counters are
        // maintained off-runtime and are always current. Their disagreement is
        // itself a signal (healthz-ingest-starvation.md §5.2).
        "scheduler_latency": super::runtime_metrics::scheduler_latency_snapshot(),
    }))
    .into_response()
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OrchestrationActivityParams {
    /// Look-back window (minutes) for the recent thread/note/knowledge
    /// writes section. Default 10, clamped to 24h.
    pub(crate) writes_window_minutes: Option<u64>,
}

/// `GET /admin/orchestration-activity`: the convergence gate's probe. Cheap
/// (in-memory reads only) and machine-readable; see `server::drain`.
pub(crate) async fn admin_orchestration_activity(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<OrchestrationActivityParams>,
) -> impl axum::response::IntoResponse {
    let window = super::drain::clamp_writes_window_minutes(query.writes_window_minutes);
    axum::Json(super::drain::orchestration_activity_snapshot(
        &state, window,
    ))
    .into_response()
}

/// `GET /admin/drain`: current admission drain state.
pub(crate) async fn admin_drain_status(
    AxumState(state): AxumState<Arc<SharedState>>,
) -> impl axum::response::IntoResponse {
    axum::Json(json!({
        "status": "ok",
        "drain": state.drain.snapshot(),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct DrainSetParams {
    pub(crate) draining: bool,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default)]
    pub(crate) set_by: Option<String>,
}

/// `POST /admin/drain {"draining": true|false, "reason"?, "set_by"?}`:
/// enter or leave admission drain. Persists the marker before answering so
/// a crash after the 200 cannot lose the toggle. Idempotent both ways.
pub(crate) async fn admin_drain_set(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<DrainSetParams>,
) -> impl axum::response::IntoResponse {
    let outcome = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            if req.draining {
                state.drain.set(req.reason, req.set_by).map(|_| ())
            } else {
                state.drain.clear()
            }
        })
        .await
    };
    match outcome {
        Ok(Ok(())) => {
            tracing::warn!(
                target: "blackbox::drain",
                draining = state.drain.is_draining(),
                "admission drain toggled via /admin/drain"
            );
            axum::Json(json!({
                "status": "ok",
                "drain": state.drain.snapshot(),
            }))
            .into_response()
        }
        Ok(Err(e)) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("drain marker write failed: {e}"),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("drain toggle task failed: {e}"),
        )
            .into_response(),
    }
}

pub(crate) async fn read_artifact_source(source: &str) -> anyhow::Result<Value> {
    const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;
    let raw = if source.starts_with("http://") || source.starts_with("https://") {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;
        let response = client
            .get(source)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?
            .error_for_status()
            .map_err(reqwest::Error::without_url)?;
        let scheme = response.url().scheme();
        if scheme != "http" && scheme != "https" {
            anyhow::bail!("artifact source redirected to unsupported scheme `{scheme}`");
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !(content_type.contains("application/json")
            || content_type.contains("text/json")
            || content_type.contains("text/plain"))
        {
            anyhow::bail!("artifact source content-type must be JSON or text/plain");
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_ARTIFACT_BYTES as u64)
        {
            anyhow::bail!("artifact source too large; limit is {MAX_ARTIFACT_BYTES} bytes");
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(reqwest::Error::without_url)?;
            if bytes.len() + chunk.len() > MAX_ARTIFACT_BYTES {
                anyhow::bail!("artifact source too large; limit is {MAX_ARTIFACT_BYTES} bytes");
            }
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8(bytes)?
    } else {
        std::fs::read_to_string(source)?
    };
    Ok(serde_json::from_str(&raw)?)
}

pub(crate) async fn install_artifact_from_params(
    state: &Arc<SharedState>,
    p: ArtifactInstallParams,
) -> anyhow::Result<artifacts::ArtifactMetadata> {
    anyhow::ensure!(
        !matches!(
            p.kind,
            artifacts::ArtifactKind::Workflow
                | artifacts::ArtifactKind::Agent
                | artifacts::ArtifactKind::Packet
                | artifacts::ArtifactKind::Atom
                | artifacts::ArtifactKind::Cron
                | artifacts::ArtifactKind::Team
        ),
        "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
    );
    let value = read_artifact_source(&p.source).await?;
    install_artifact_value(state, p, value).await
}

#[derive(Debug)]
pub(crate) struct ArtifactInstallFailure {
    kind: artifacts::ArtifactKind,
    name: Option<String>,
    completed: Vec<&'static str>,
    failed: &'static str,
    remaining: Vec<&'static str>,
    cause: anyhow::Error,
}
impl ArtifactInstallFailure {
    pub(crate) fn response(&self) -> Value {
        let reason = self
            .cause
            .chain()
            .find_map(|error| error.downcast_ref::<std::io::Error>())
            .map(|error| format!("storage error: {:?}", error.kind()))
            .unwrap_or_else(|| self.cause.to_string());
        json!({"error": "error.artifact_install_failed", "kind": self.kind,
            "name": self.name, "completed": self.completed, "failed": self.failed,
            "not_attempted": self.remaining, "reason": reason,
            "failed_step_may_have_partial_effects": self.failed != "validation"})
    }
}
impl std::fmt::Display for ArtifactInstallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "artifact install failed at {}: {}",
            self.failed, self.cause
        )
    }
}
impl std::error::Error for ArtifactInstallFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

pub(crate) async fn install_artifact_value(
    state: &Arc<SharedState>,
    p: ArtifactInstallParams,
    mut value: Value,
) -> anyhow::Result<artifacts::ArtifactMetadata> {
    anyhow::ensure!(
        !matches!(
            p.kind,
            artifacts::ArtifactKind::Workflow
                | artifacts::ArtifactKind::Agent
                | artifacts::ArtifactKind::Packet
                | artifacts::ArtifactKind::Atom
                | artifacts::ArtifactKind::Cron
                | artifacts::ArtifactKind::Team
        ),
        "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
    );
    let mut completed = Vec::new();
    let mut failed = "validation";
    let kind = p.kind;
    let requested_name = p
        .name
        .clone()
        .or_else(|| value.get("name").and_then(Value::as_str).map(str::to_owned));
    let supersedes = p
        .supersedes
        .as_deref()
        .or_else(|| value.get("supersedes").and_then(Value::as_str));
    let has_supersession =
        supersedes.is_some_and(|previous| Some(previous) != requested_name.as_deref());
    let mut remaining = vec!["validation"];
    remaining.extend(match kind {
        artifacts::ArtifactKind::Workflow => {
            anyhow::bail!(
                "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
            );
        }
        artifacts::ArtifactKind::Packet => {
            anyhow::bail!(
                "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
            );
        }
        artifacts::ArtifactKind::Brofile => vec!["brofile_file", "brofile_verification"],
        artifacts::ArtifactKind::Team => {
            anyhow::bail!(
                "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
            );
        }
        artifacts::ArtifactKind::Cron => {
            anyhow::bail!(
                "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
            );
        }
        _ => vec![],
    });
    remaining.push("catalog_persistence");
    if has_supersession {
        remaining.push("previous_runtime_deactivation");
    }
    let result: anyhow::Result<artifacts::ArtifactMetadata> = (|| {
        if !value.is_object() {
            anyhow::bail!("{} artifact must be a JSON object", kind.as_str());
        }
        let (effective_name, effective_version) = artifacts::validate_install_identity(
            kind,
            &value,
            p.name.as_deref(),
            p.version.as_deref(),
        )?;
        value["name"] = Value::String(effective_name);
        // Preserve version's original JSON type unless an override was requested.
        if p.version.is_some() {
            value["version"] = if value.get("version").is_some_and(Value::is_number)
                || matches!(
                    kind,
                    artifacts::ArtifactKind::Workflow | artifacts::ArtifactKind::Atom
                ) {
                Value::from(
                    effective_version
                        .parse::<u32>()
                        .map_err(|_| anyhow::anyhow!("artifact version must parse as u32"))?,
                )
            } else {
                Value::String(effective_version)
            };
        }
        match p.kind {
            artifacts::ArtifactKind::Workflow => {
                anyhow::bail!(
                    "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
                );
            }
            artifacts::ArtifactKind::Packet => {
                anyhow::bail!(
                    "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
                );
            }
            artifacts::ArtifactKind::Brofile => {
                let brofile: orchestration::brofile::Brofile =
                    serde_json::from_value(value.clone())?;
                completed.push("validation");
                failed = "brofile_file";
                let written = orchestration::brofile::save_brofile(
                    &brofile,
                    "global",
                    &state.store_dir,
                    None,
                )
                .map_err(|e| anyhow::anyhow!("brofile registry write failed: {e}"))?;
                completed.push("brofile_file");
                failed = "brofile_verification";
                // Post-install verification — the artifact catalog reports
                // "active" only when the runtime registry can actually see
                // the brofile. Prevents silent G11-style desync where the
                // catalog says installed but bro_brofile list returns
                // empty.
                if orchestration::brofile::resolve_brofile(&brofile.name, &state.store_dir, None)
                    .is_none_or(|saved| saved.name != brofile.name)
                {
                    anyhow::bail!(
                        "brofile written to {} but resolve_brofile returned None — runtime registry desync",
                        written.display()
                    );
                }
                completed.push("brofile_verification");
            }
            artifacts::ArtifactKind::Team => {
                anyhow::bail!(
                    "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
                );
            }
            artifacts::ArtifactKind::Cron => {
                anyhow::bail!(
                    "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
                );
            }
            artifacts::ArtifactKind::Agent => {
                anyhow::bail!(
                    "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
                );
            }
            artifacts::ArtifactKind::Atom => {
                anyhow::bail!(
                    "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
                );
            }
        }
        if !completed.contains(&"validation") {
            completed.push("validation");
        }
        failed = "catalog_persistence";
        let meta = state.artifacts.write().install_value(
            p.kind,
            p.source,
            &value,
            p.name,
            p.version,
            p.supersedes,
        )?;
        completed.push("catalog_persistence");
        if let Some(prev) = meta
            .supersedes
            .as_deref()
            .filter(|prev| *prev != meta.name.as_str())
        {
            failed = "previous_runtime_deactivation";
            deactivate_artifact(state, meta.kind, prev)?;
            completed.push("previous_runtime_deactivation");
        }
        Ok(meta)
    })();
    result.map_err(|cause| {
        remaining.retain(|step| !completed.contains(step) && *step != failed);
        let mut failed_step = failed;
        if let Some(persistence) = cause.downcast_ref::<artifacts::ArtifactPersistenceFailure>() {
            completed.extend(persistence.completed.iter().copied());
            failed_step = persistence.failed;
            let catalog_steps = [
                "artifact_content",
                "catalog_metadata",
                "catalog_version_snapshot",
                "previous_catalog_supersession",
            ];
            if let Some(index) = catalog_steps.iter().position(|step| *step == failed_step) {
                remaining.splice(
                    0..0,
                    catalog_steps[index + 1..].iter().copied().filter(|step| {
                        *step != "previous_catalog_supersession" || has_supersession
                    }),
                );
            }
        }
        ArtifactInstallFailure {
            kind,
            name: requested_name,
            completed,
            failed: failed_step,
            remaining,
            cause,
        }
        .into()
    })
}

pub(crate) fn restore_runtime_artifacts_from_catalog(
    state: &Arc<SharedState>,
) -> anyhow::Result<usize> {
    let entries = state.artifacts.read().list(&ArtifactListParams {
        kind: None,
        name: None,
        include_superseded: false,
    })?;
    let mut restored = 0usize;

    for entry in entries
        .into_iter()
        .filter(|entry| entry.active)
        .filter(|entry| entry.kind == artifacts::ArtifactKind::Brofile)
    {
        let Some(value) = state
            .artifacts
            .read()
            .load_artifact_value(entry.kind, &entry.name)?
        else {
            tracing::warn!(
                "active {} artifact '{}' has no catalog payload; runtime registry not restored",
                entry.kind.as_str(),
                entry.name
            );
            continue;
        };

        match entry.kind {
            artifacts::ArtifactKind::Workflow => {
                anyhow::bail!(
                    "error.retired_artifact_kind: workflows, agents, packets, atoms, crons and teams cannot be activated"
                );
            }
            artifacts::ArtifactKind::Brofile => {
                let brofile: orchestration::brofile::Brofile =
                    serde_json::from_value(value.clone())
                        .with_context(|| format!("parsing brofile artifact '{}'", entry.name))?;
                let written = orchestration::brofile::save_brofile(
                    &brofile,
                    "global",
                    &state.store_dir,
                    None,
                )
                .map_err(|e| anyhow::anyhow!("brofile registry write failed: {e}"))?;
                if orchestration::brofile::resolve_brofile(&brofile.name, &state.store_dir, None)
                    .is_none()
                {
                    anyhow::bail!(
                        "brofile written to {} but resolve_brofile returned None — runtime registry desync",
                        written.display()
                    );
                }
                restored += 1;
            }
            _ => {}
        }
    }

    Ok(restored)
}

pub(crate) fn deactivate_artifact(
    state: &Arc<SharedState>,
    kind: artifacts::ArtifactKind,
    name: &str,
) -> anyhow::Result<()> {
    match kind {
        artifacts::ArtifactKind::Workflow | artifacts::ArtifactKind::Packet => {}
        artifacts::ArtifactKind::Brofile => {
            orchestration::brofile::delete_brofile(name, "global", &state.store_dir, None);
        }
        artifacts::ArtifactKind::Agent
        | artifacts::ArtifactKind::Atom
        | artifacts::ArtifactKind::Team
        | artifacts::ArtifactKind::Cron => {}
    }
    Ok(())
}

pub(crate) fn edge_sidecar_dir(state: &SharedState) -> std::path::PathBuf {
    bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
        &state.idx.read().reindex_config().projects_path,
    )
}

/// Republish the pinned code read view from current authority: the active
/// selector map (manifest plus registered corpus projects), the searcher,
/// the catalog epoch and the Git overlay selection. Nothing here parses edge
/// rows; the view carries selectors and overlays, not a graph.
///
/// The selectors are read and the view swapped under the manifest
/// coordinator, in the order every activation publisher uses (coordinator,
/// then the index lock), so a concurrent activation can neither interleave
/// nor be reverted by a stale selector map. The coordinator is not
/// reentrant: no caller may already hold it.
pub(crate) fn refresh_code_read_view(state: &SharedState) -> anyhow::Result<()> {
    let edges_dir = edge_sidecar_dir(state);
    bbox_edge_sidecar::snapshot::with_manifest_coordinator(|| {
        let (selectors, searcher) = {
            let index = state.idx.read();
            (index.refresh_active_code_selectors()?, index.searcher())
        };
        *state.code_read_view.write() = std::sync::Arc::new(super::CodeReadView {
            active_selectors: selectors,
            searcher,
            catalog_epoch: state.records_provider.records_snapshot().authority_epoch,
            git_overlays: super::state::read_git_overlays_for_view(
                &state.project_authority,
                &edges_dir,
            ),
        });
        Ok(())
    })
}

/// Hash of the manifest authority the code read view derives from. The
/// volatile `updated_at` stamp is excluded: reindex refreshes it while every
/// selected path and selector stays the same. Snapshot member bytes are not
/// view inputs, so neither inactive snapshots nor in-progress write
/// directories move this value.
fn manifest_view_signature(edges_dir: &std::path::Path) -> anyhow::Result<u64> {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match bbox_edge_sidecar::manifest::try_load_manifest_index(edges_dir) {
        Ok(mut index) => {
            index.updated_at = None;
            1_u8.hash(&mut hasher);
            serde_json::to_vec(&index)?.hash(&mut hasher);
        }
        Err(bbox_edge_sidecar::manifest::ManifestFallbackReason::MissingNotMigrated) => {
            0_u8.hash(&mut hasher);
        }
        Err(reason) => anyhow::bail!("active edge manifest is unavailable: {reason:?}"),
    }
    Ok(hasher.finish())
}

/// Everything the published view derives from besides the searcher: the
/// manifest authority, the registered corpus project set and the catalog
/// epoch.
fn code_view_authority(state: &SharedState, edges_dir: &std::path::Path) -> anyhow::Result<u64> {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    manifest_view_signature(edges_dir)?.hash(&mut hasher);
    let mut registered = state
        .corpus_registered_project_ids()
        .into_iter()
        .collect::<Vec<_>>();
    registered.sort();
    registered.hash(&mut hasher);
    state
        .records_provider
        .records_snapshot()
        .authority_epoch
        .hash(&mut hasher);
    Ok(hasher.finish())
}

/// Background thread that keeps the pinned code read view current. The
/// reindex thread writes snapshots and manifest entries without a handle on
/// `SharedState`, and registration changes the corpus project set, so this
/// thread republishes the view when its authority changes or a caller nudges
/// it, and otherwise refreshes only the searcher when the corpus moved. Every
/// pass is a manifest read, never an edge parse.
pub(crate) fn spawn_code_read_view_refresher(
    state: Arc<SharedState>,
    interval: std::time::Duration,
) {
    std::thread::Builder::new()
        .name("blackbox-code-view".into())
        .spawn(move || {
            let _scope = crate::util::BlockingScope::enter();
            let nudge_rx = state.code_view_refresh_nudge_rx.lock().unwrap().take();
            run_code_read_view_refresher(&state, interval, nudge_rx);
        })
        .expect("failed to spawn code read view refresher");
}

fn run_code_read_view_refresher(
    state: &SharedState,
    interval: std::time::Duration,
    nudge_rx: Option<std::sync::mpsc::Receiver<()>>,
) {
    let edges_dir = edge_sidecar_dir(state);
    let mut cursor = CodeViewCursor {
        last_docs: state.idx.read().num_docs(),
        last_authority: code_view_authority(state, &edges_dir).ok(),
    };
    loop {
        let nudged = match &nudge_rx {
            Some(rx) => match rx.recv_timeout(interval) {
                Ok(()) => true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
                // All senders dropped: SharedState is gone.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            },
            None => {
                std::thread::sleep(interval);
                false
            }
        };
        run_code_read_view_pass(state, &edges_dir, &mut cursor, nudged);
    }
}

/// What the refresher last published: the corpus document count and the view
/// authority hash.
struct CodeViewCursor {
    last_docs: u64,
    last_authority: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeViewPass {
    /// Authority could not be read; the published view stays.
    AuthorityUnavailable,
    Republished,
    RepublishFailed,
    SearcherRefreshed,
    Unchanged,
}

fn run_code_read_view_pass(
    state: &SharedState,
    edges_dir: &std::path::Path,
    cursor: &mut CodeViewCursor,
    nudged: bool,
) -> CodeViewPass {
    let docs = state.idx.read().num_docs();
    let authority = match code_view_authority(state, edges_dir) {
        Ok(authority) => authority,
        Err(error) => {
            tracing::warn!(%error, nudged, "code view authority unavailable; keeping the published view");
            return CodeViewPass::AuthorityUnavailable;
        }
    };
    let outcome = if nudged || cursor.last_authority != Some(authority) {
        match refresh_code_read_view(state) {
            Ok(()) => {
                cursor.last_authority = Some(authority);
                CodeViewPass::Republished
            }
            Err(error) => {
                tracing::warn!(%error, nudged, "code view republish failed; retrying on the next pass");
                return CodeViewPass::RepublishFailed;
            }
        }
    } else if docs != cursor.last_docs {
        let searcher = { state.idx.read().searcher() };
        state.publish_code_read_searcher(searcher);
        CodeViewPass::SearcherRefreshed
    } else {
        CodeViewPass::Unchanged
    };
    cursor.last_docs = docs;
    outcome
}

pub(crate) fn project_ref_counts(state: &Arc<SharedState>, project: &str) -> anyhow::Result<Value> {
    let knowledge = state
        .kb
        .read()
        .all_entries()
        .iter()
        .filter(|entry| entry.project.as_deref() == Some(project))
        .count();
    let threads = state
        .threads
        .read()
        .all()
        .iter()
        .filter(|thread| thread.project == project)
        .count();
    let slack_channel_bindings = state.slack_channel_bindings.list(None, Some(project)).len();
    let slack_proposal_links = state.slack_proposal_links.project_ref_count(project);
    let gaps = state
        .gaps
        .read()
        .all()
        .iter()
        .filter(|gap| gap.project.as_deref() == Some(project))
        .count();
    Ok(json!({
        "knowledge": knowledge,
        "threads": threads,
        "slack_channel_bindings": slack_channel_bindings,
        "slack_proposal_links": slack_proposal_links,
        "gaps": gaps,
    }))
}

/// Re-derive repository carriers from the live registry so committed
/// knowledge and gaps are loaded only through checkout authority.
pub(crate) fn sync_kb_project_roots(state: &SharedState) {
    let repo_io = std::sync::Arc::new(super::repo_io::RepoIoAuthority::new(
        state.checkout_access.clone(),
    ));
    // Records and their exact attachment targets come from ONE catalog
    // epoch (F4). On failure every arm below is skipped, which preserves the
    // last-good carrier set rather than installing a moving-ladder one.
    let inputs = match super::repo_io::CatalogBaseTargets::read_consistent_for_state(state) {
        Ok(inputs) => inputs,
        Err(error) => {
            tracing::warn!("repository-carrier sync skipped, carriers unchanged: {error:#}");
            return;
        }
    };
    let projects = inputs.records;
    let catalog_targets = inputs.targets;
    let local_projects = projects
        .iter()
        .filter(|project| {
            !state
                .knowledge_transport_cutover
                .covers_project_str(&project.project_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    match super::repo_io::RepoIoAuthority::knowledge_base_carriers(
        &local_projects,
        catalog_targets.as_ref(),
    ) {
        Ok(knowledge_carriers) => {
            if let Err(error) = state.kb.write().configure_repo_io(
                repo_io.clone(),
                repo_io.clone(),
                knowledge_carriers,
            ) {
                tracing::warn!("knowledge repository-carrier sync failed: {error:#}");
            }
        }
        Err(error) => {
            tracing::warn!("knowledge repository-carrier sync failed: {error:#}");
        }
    }
    match super::repo_io::RepoIoAuthority::gap_base_carriers(
        &local_projects,
        catalog_targets.as_ref(),
    ) {
        Ok(gap_carriers) => {
            if let Err(error) =
                state
                    .gaps
                    .write()
                    .configure_repo_io(repo_io.clone(), repo_io, gap_carriers)
            {
                tracing::warn!("gap repository-carrier sync failed: {error:#}");
            }
        }
        Err(error) => {
            tracing::warn!("gap repository-carrier sync failed: {error:#}");
        }
    }
}

/// Materialize knowledge bytes that are authorized for published vector ids.
/// This must use the same committed publisher view as Tantivy. The central
/// store also contains working-tree repo entries, so reading it directly
/// would publish uncommitted bytes under `knowledge:*`.
pub(crate) fn published_knowledge_for_embedding(
    state: &std::sync::Arc<SharedState>,
    project_dir: Option<&str>,
) -> anyhow::Result<Vec<crate::knowledge::KnowledgeEntry>> {
    let view = super::BlackboxServer::new(state.clone()).session_knowledge_view(project_dir)?;
    Ok(view.knowledge.all_entries().to_vec())
}

fn knowledge_entry_belongs_to_project(
    entry: &crate::knowledge::KnowledgeEntry,
    project_dir: &str,
    project_id: &str,
) -> bool {
    entry.project.as_deref() == Some(project_dir) || entry.project_id.as_deref() == Some(project_id)
}

/// Enqueue embeddings for a project's committed knowledge entries. The BM25
/// reindex picks up committed `.bbox/knowledge/` automatically, but vector
/// coverage is driven by enqueue, so a project registered from a clone would
/// otherwise be invisible to vector search until a manual reembed. The embed
/// worker dedupes by (entity_id, chunk_hash), so re-enqueuing already-embedded
/// entries is a cheap no-op. Returns the number of entries enqueued.
pub(crate) fn enqueue_project_knowledge_embeds(
    state: &std::sync::Arc<SharedState>,
    project_dir: &str,
) -> usize {
    let server = super::BlackboxServer::new(state.clone());
    let projects = state.records_provider.records_snapshot().records;
    let matching = projects
        .iter()
        .filter(|project| project.canonical_path == project_dir)
        .collect::<Vec<_>>();
    let [project] = matching.as_slice() else {
        tracing::warn!(
            project = project_dir,
            "project knowledge embed source has no unique registered attachment"
        );
        return 0;
    };
    let publication_lease = if state.project_authority.is_bridge() {
        Some(
            match super::checkout_access::published_scope_for_project(
                &state.checkout_access,
                &project.project_id,
            ) {
                Ok(Some(scope)) => match server
                    .authorize_publisher(&projects, &scope)
                    .and_then(|publisher| server.acquire_authorized_publisher_lease(&publisher))
                {
                    Ok(lease) => lease,
                    Err(error) => {
                        tracing::warn!(
                            project = project_dir,
                            error = %error,
                            "project knowledge embed publisher authority unavailable"
                        );
                        return 0;
                    }
                },
                Ok(None) => match super::checkout_access::acquire_selected_project_access(
                    &state.checkout_access,
                    &project.project_id,
                    bbox_indexing::checkout_access::CheckoutAccessKind::PublisherConfigTreeRead,
                    bbox_indexing::checkout_access::CheckoutAccessIntent::Read,
                ) {
                    Ok(lease) => lease,
                    Err(error) => {
                        tracing::warn!(
                            project = project_dir,
                            error = %error,
                            "legacy project knowledge embed authority unavailable"
                        );
                        return 0;
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        project = project_dir,
                        error = %error,
                        "project knowledge embed scope authority unavailable"
                    );
                    return 0;
                }
            },
        )
    } else {
        None
    };
    let entries = match published_knowledge_for_embedding(state, Some(project_dir)) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                project = project_dir,
                error = %error,
                "project knowledge embed source unavailable"
            );
            return 0;
        }
    };
    let publication = match publication_lease.as_ref() {
        Some(publication_lease) => {
            match state.checkout_access.publication_guard(publication_lease) {
                Ok(publication) => Some(publication),
                Err(error) => {
                    tracing::warn!(
                        project = project_dir,
                        error = %error,
                        "project knowledge embed publication authority changed"
                    );
                    return 0;
                }
            }
        }
        None => None,
    };
    let mut enqueued = 0usize;
    for entry in entries
        .iter()
        .filter(|e| knowledge_entry_belongs_to_project(e, project_dir, &project.project_id))
    {
        let entity_id = crate::index::knowledge_entity_id(&entry.id);
        let chunk_hash = crate::index::knowledge_chunk_hash(entry);
        crate::embed_queue::enqueue_knowledge(entry, &entity_id, &chunk_hash);
        enqueued += 1;
    }
    drop(publication);
    enqueued
}

pub(crate) fn migrate_project_refs(
    state: &Arc<SharedState>,
    old_project: &str,
    new_project: &str,
    record: &ProjectRecord,
) -> anyhow::Result<Value> {
    let knowledge = state
        .kb
        .write()
        .rename_project_refs(old_project, new_project)?;
    if knowledge > 0 {
        // This sync migration helper cannot await; knowledge persistence is write-behind here.
        state.kb_persister.request();
    }
    let threads = state
        .threads
        .write()
        .rename_project_refs(old_project, new_project)?;
    if threads > 0 {
        // This sync migration helper cannot await; threads persistence is write-behind here.
        state.threads_persister.request();
    }
    let slack_channel_bindings = state.slack_channel_bindings.rename_project_refs(
        old_project,
        new_project,
        Some(record.project_id.as_str()),
    )?;
    let slack_proposal_links = state
        .slack_proposal_links
        .rename_project_refs(old_project, new_project)?;
    let gaps = state
        .gaps
        .write()
        .rename_project_refs(old_project, new_project)?;
    Ok(json!({
        "knowledge": knowledge,
        "threads": threads,
        "slack_channel_bindings": slack_channel_bindings,
        "slack_proposal_links": slack_proposal_links,
        "gaps": gaps,
    }))
}

pub(crate) async fn admin_artifact_install(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<ArtifactInstallParams>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    match install_artifact_from_params(&state, req).await {
        Ok(meta) => axum::Json(json!({"status": "installed", "artifact": meta})).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("artifact install: {e:#}"),
        )
            .into_response(),
    }
}

pub(crate) async fn admin_artifact_list(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<ArtifactListParams>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    match state.artifacts.read().list(&query) {
        Ok(rows) => axum::Json(json!({"artifacts": rows})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("artifact list: {e:#}"),
        )
            .into_response(),
    }
}

pub(crate) async fn admin_artifact_supersede(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<ArtifactSupersedeParams>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    match state
        .artifacts
        .write()
        .supersede(req.kind, &req.name, &req.superseded_by)
    {
        Ok(meta) => match deactivate_artifact(&state, req.kind, &req.name) {
            Ok(()) => axum::Json(json!({"status": "superseded", "artifact": meta})).into_response(),
            Err(e) => (
                axum::http::StatusCode::BAD_REQUEST,
                format!("artifact deactivate: {e:#}"),
            )
                .into_response(),
        },
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("artifact supersede: {e:#}"),
        )
            .into_response(),
    }
}

pub(crate) async fn admin_artifact_remove(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<ArtifactRemoveParams>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    if !req.dry_run && !req.confirm {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "artifact remove: hard artifact removal requires confirm=true".to_string(),
        )
            .into_response();
    }
    if !req.dry_run {
        if let Err(e) = state
            .artifacts
            .read()
            .remove_hard(req.kind, &req.name, true, true)
        {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("artifact remove: {e:#}"),
            )
                .into_response();
        }
        if let Err(e) = deactivate_artifact(&state, req.kind, &req.name) {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("artifact deactivate: {e:#}"),
            )
                .into_response();
        }
    }
    match state
        .artifacts
        .write()
        .remove_hard(req.kind, &req.name, req.dry_run, req.confirm)
    {
        Ok(result) => axum::Json(json!({"status": "removed", "artifact": result})).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("artifact remove: {e:#}"),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct AdminBrofileUpsertReq {
    name: String,
    provider: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    lens: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
}

pub(crate) async fn admin_brofile_upsert(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminBrofileUpsertReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let provider: orchestration::providers::Provider = match req.provider.parse() {
        Ok(p) => p,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("unknown provider '{}'", req.provider),
            )
                .into_response();
        }
    };
    // This route rewrites the brofile from the request's fields. Tool
    // filters and an edit discipline are restrictions someone set on purpose,
    // so existing ones are carried over and named in the response; a rewrite
    // must not silently lift them. An unreadable existing brofile is not
    // overwritten.
    let (filters, edit_discipline) =
        match orchestration::brofile::read_brofile(&req.name, "global", &state.store_dir, None) {
            Ok(Some(existing)) => (existing.filters, existing.edit_discipline),
            Ok(None) => (None, None),
            Err(e) => {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(
                        json!({"status": "error", "name": req.name, "error": e.to_string()}),
                    ),
                )
                    .into_response();
            }
        };
    let bf = orchestration::brofile::Brofile {
        name: req.name.clone(),
        provider,
        account: req.account,
        lens: req.lens,
        model: req.model,
        effort: req.effort,
        tool_defaults: None,
        filters,
        surface: None,
        coerce_workspace: None,
        runtime: None,
        context: None,
        code_mode: None,
        edit_discipline,
        service_tier: req.service_tier,
    };
    let mut kept = Vec::new();
    if bf.filters.is_some() {
        kept.push("filters");
    }
    if bf.edit_discipline.is_some() {
        kept.push("edit_discipline");
    }
    if let Err(e) = orchestration::brofile::save_brofile(&bf, "global", &state.store_dir, None) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"status": "error", "name": req.name, "error": e.to_string()})),
        )
            .into_response();
    }
    axum::Json(json!({"status": "upserted", "name": req.name, "kept": kept})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_ref;
    use crate::server::state::BlackboxServer;

    #[tokio::test]
    async fn admin_brofile_upsert_keeps_existing_filters_and_edit_discipline() {
        use axum::response::IntoResponse;
        use orchestration::brofile::{EditDiscipline, read_brofile, save_brofile};
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));
        let upsert = |model: &'static str, name: &'static str| {
            let state = state.clone();
            async move {
                let response = admin_brofile_upsert(
                    AxumState(state),
                    axum::Json(
                        serde_json::from_value(
                            json!({ "name": name, "provider": "glm", "model": model }),
                        )
                        .unwrap(),
                    ),
                )
                .await
                .into_response();
                assert!(response.status().is_success());
                let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .unwrap();
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()
            }
        };
        let stored = |name: &str| {
            read_brofile(name, "global", &state.store_dir, None)
                .unwrap()
                .unwrap()
        };

        // A brofile created by the route carries neither restriction.
        assert_eq!(upsert("first-model", "plain").await["kept"], json!([]));
        let plain = stored("plain");
        assert!(plain.filters.is_none() && plain.edit_discipline.is_none());

        // Filters survive a rewrite of the other fields, and the response
        // says so.
        let mut filtered = plain.clone();
        filtered.name = "filtered".to_string();
        filtered.filters = Some(orchestration::mcp::McpFilters {
            disallow: vec!["glob".to_string()],
            ..Default::default()
        });
        save_brofile(&filtered, "global", &state.store_dir, None).unwrap();
        assert_eq!(
            upsert("second-model", "filtered").await["kept"],
            json!(["filters"])
        );
        let rewritten = stored("filtered");
        assert_eq!(rewritten.model.as_deref(), Some("second-model"));
        assert_eq!(
            rewritten.filters.unwrap().disallow,
            vec!["glob".to_string()]
        );
        assert_eq!(rewritten.edit_discipline, None);

        // So does an edit discipline.
        let mut strict = plain.clone();
        strict.name = "strict".to_string();
        strict.edit_discipline = Some(EditDiscipline::Structured);
        save_brofile(&strict, "global", &state.store_dir, None).unwrap();
        assert_eq!(
            upsert("third-model", "strict").await["kept"],
            json!(["edit_discipline"])
        );
        let rewritten = stored("strict");
        assert_eq!(rewritten.model.as_deref(), Some("third-model"));
        assert_eq!(rewritten.edit_discipline, Some(EditDiscipline::Structured));
        assert!(rewritten.filters.is_none());
    }

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())))
    }

    fn git(root: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn embedding_test_entry(content: &str) -> crate::knowledge::KnowledgeEntry {
        crate::knowledge::KnowledgeEntry {
            render_placement: Default::default(),
            id: "embed-source".into(),
            title: "embed source".into(),
            content: content.into(),
            cluster: None,
            category: crate::knowledge::Category::Memory,
            scope: crate::knowledge::Scope::Project,
            project: None,
            project_id: None,
            providers: Vec::new(),
            priority: crate::knowledge::Priority::Standard,
            render: false,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    #[test]
    fn embedding_source_uses_committed_publisher_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        std::fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"embedding-family\"\n",
        )
        .unwrap();
        let entry_path = root.join(".bbox/knowledge/embed-source.json");
        std::fs::write(
            &entry_path,
            serde_json::to_vec_pretty(&embedding_test_entry("published bytes")).unwrap(),
        )
        .unwrap();
        git(&root, &["add", ".bbox"]);
        git(&root, &["commit", "-q", "-m", "published knowledge"]);
        let root = root.canonicalize().unwrap();

        let server = test_server(&temp);
        server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&root)
            .unwrap();
        sync_kb_project_roots(&server.state);

        std::fs::write(
            &entry_path,
            serde_json::to_vec_pretty(&embedding_test_entry("uncommitted bytes")).unwrap(),
        )
        .unwrap();
        sync_kb_project_roots(&server.state);
        assert_eq!(
            server
                .state
                .kb
                .read()
                .entry("embed-source")
                .unwrap()
                .content,
            "uncommitted bytes",
            "fixture must expose working-tree bytes in the central overlay store"
        );

        let entries =
            published_knowledge_for_embedding(&server.state, Some(root.to_str().unwrap())).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "published bytes");

        let lifecycle = server
            .state
            .checkout_access
            .lifecycle_mutation_guard()
            .unwrap();
        assert_eq!(
            enqueue_project_knowledge_embeds(&server.state, root.to_str().unwrap()),
            0,
            "embedding publication must stop when its checkout fence is unavailable"
        );
        drop(lifecycle);
    }

    #[test]
    fn catalog_embedding_rows_match_stable_project_identity_without_a_path() {
        let mut entry = embedding_test_entry("published bytes");
        entry.project_id = Some("p_catalog".into());

        assert!(knowledge_entry_belongs_to_project(
            &entry,
            "/checkout/path",
            "p_catalog"
        ));
        assert!(!knowledge_entry_belongs_to_project(
            &entry,
            "/checkout/path",
            "p_other"
        ));
    }

    /// Regression for the 2026-08-25 cage index-plane deadlock:
    /// `bbox_hybrid_search` holds
    /// `state.idx.read()` across the whole search call, and provider
    /// property/label lookups re-acquire the same lock on the same thread.
    /// A writer queued between the two acquisitions (history activation's
    /// `republish_code_read_view` taking `idx.write()`) parked the nested
    /// read forever: reader waits on writer, writer waits on reader, and
    /// every later `idx.read()` (all searches, stats, the edge watcher)
    /// piled up behind them. The fix is `read_recursive()` at every
    /// acquisition inside bbox-providers (invariant documented on
    /// `CorpusStores::idx`).
    ///
    /// Against the buggy code this test deadlocks rather than asserting;
    /// the nextest per-test timeout turns that hang into a named failure.
    #[test]
    fn provider_reads_do_not_deadlock_behind_queued_idx_writer() {
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(&tmp.path().join("bro")));

        // The outer guard the search tools hold across provider calls.
        let outer = state.idx.read();

        // Queue a writer behind the held read and let it park.
        let st = state.clone();
        let writer = std::thread::spawn(move || {
            let _w = st.idx.write();
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !writer.is_finished(),
            "precondition: writer must be parked behind the outer read guard"
        );

        // The provider-side lookup runs on THIS thread (the incident
        // shape) and must complete while the writer is still parked.
        let ctx = crate::providers::ProviderContext::new(state.corpus_stores());
        let looked_up = ctx
            .indexed_entity_properties("session:claude:does-not-exist")
            .expect("empty-corpus lookup must not error");
        assert!(looked_up.is_none(), "empty corpus has no entity properties");

        drop(outer);
        writer.join().unwrap();
    }

    fn signature_test_edge(kind: &str) -> bbox_edge_sidecar::edge_sidecar::Edge {
        bbox_edge_sidecar::edge_sidecar::Edge {
            source: entity_ref::EntityRef::Knowledge {
                id: "source".into(),
            },
            kind: kind.into(),
            target: entity_ref::EntityRef::Knowledge {
                id: "target".into(),
            },
            provenance: bbox_chunker::EdgeProvenance::Derived,
            confidence: bbox_chunker::EdgeConfidence::Exact,
            metadata: Default::default(),
            project_id: None,
        }
    }

    #[test]
    fn manifest_view_signature_ignores_inactive_snapshots_and_write_tmp_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        bbox_edge_sidecar::snapshot::switch_to_clean_snapshot(
            edges_dir,
            "p",
            "repo",
            Some("main"),
            "head-a",
            vec![signature_test_edge("ACTIVE")],
            vec![],
            vec![],
        )
        .unwrap();
        let base = manifest_view_signature(edges_dir).unwrap();

        bbox_edge_sidecar::snapshot::write_snapshot_files(
            edges_dir,
            "p",
            "head-inactive",
            &[("project.jsonl", &[signature_test_edge("INACTIVE")])],
        )
        .unwrap();
        assert_eq!(
            base,
            manifest_view_signature(edges_dir).unwrap(),
            "inactive snapshot bytes are not view inputs"
        );

        let mat = edges_dir.join("materialized/workspace/p");
        // An in-progress temp dir's jsonl must not move the signature.
        std::fs::create_dir_all(mat.join("dirty-current.write-tmp")).unwrap();
        std::fs::write(
            mat.join("dirty-current.write-tmp/project.jsonl"),
            "half-written-overlay",
        )
        .unwrap();
        assert_eq!(
            base,
            manifest_view_signature(edges_dir).unwrap(),
            "*.write-tmp jsonl must not affect the signature"
        );
    }

    #[test]
    fn manifest_view_signature_tracks_manifest_index_active_pointers() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        // Baseline: no manifest-index present.
        let sig0 = manifest_view_signature(edges_dir).unwrap();
        bbox_edge_sidecar::snapshot::switch_to_clean_snapshot(
            edges_dir,
            "p",
            "repo",
            Some("main"),
            "head-a",
            vec![signature_test_edge("A")],
            vec![],
            vec![],
        )
        .unwrap();
        let sig1 = manifest_view_signature(edges_dir).unwrap();
        assert_ne!(sig0, sig1);

        // A different active-pointer set, such as a branch switch flipping
        // active_snapshot between two materialized snapshots, must move the
        // signature.
        bbox_edge_sidecar::snapshot::switch_to_clean_snapshot(
            edges_dir,
            "p",
            "repo",
            Some("feature"),
            "head-b",
            vec![signature_test_edge("B")],
            vec![],
            vec![],
        )
        .unwrap();
        let sig2 = manifest_view_signature(edges_dir).unwrap();
        assert_ne!(
            sig1, sig2,
            "active-pointer change must change the signature even with no .jsonl change"
        );
    }

    #[test]
    fn manifest_view_signature_ignores_manifest_timestamp_only_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        bbox_edge_sidecar::snapshot::switch_to_clean_snapshot(
            edges_dir,
            "p",
            "repo",
            Some("main"),
            "head-a",
            vec![signature_test_edge("ACTIVE")],
            vec![],
            vec![],
        )
        .unwrap();
        let base = manifest_view_signature(edges_dir).unwrap();

        let mut index = bbox_edge_sidecar::manifest::ManifestIndex::load(edges_dir).unwrap();
        index.updated_at = Some("timestamp-only-rewrite".into());
        index.write_atomic(edges_dir).unwrap();

        assert_eq!(
            base,
            manifest_view_signature(edges_dir).unwrap(),
            "volatile manifest timestamps are not view inputs"
        );
    }

    /// A pass republishes the view when a nudge arrives or the manifest
    /// authority moves, refreshes only the searcher when the corpus moved,
    /// and otherwise leaves the published view alone. No pass reads an edge
    /// row, so an oversized or malformed lane file cannot fail one.
    #[test]
    fn view_refresher_republishes_on_authority_change_and_nudge_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root.join("bro")));
        let edges_dir = edge_sidecar_dir(&state);
        std::fs::create_dir_all(edges_dir.join("observed")).unwrap();
        std::fs::write(
            edges_dir.join("observed/p.jsonl"),
            "not an edge row\n".repeat(64),
        )
        .unwrap();
        let mut cursor = CodeViewCursor {
            last_docs: state.idx.read().num_docs(),
            last_authority: code_view_authority(&state, &edges_dir).ok(),
        };
        let published = || state.code_read_view.read().clone();

        let before = published();
        assert_eq!(
            run_code_read_view_pass(&state, &edges_dir, &mut cursor, false),
            CodeViewPass::Unchanged
        );
        assert!(Arc::ptr_eq(&before, &published()));

        assert_eq!(
            run_code_read_view_pass(&state, &edges_dir, &mut cursor, true),
            CodeViewPass::Republished
        );
        assert!(!Arc::ptr_eq(&before, &published()));

        bbox_edge_sidecar::snapshot::switch_to_clean_snapshot(
            &edges_dir,
            "p",
            "repo",
            Some("main"),
            "head-a",
            vec![signature_test_edge("ACTIVE")],
            vec![],
            vec![],
        )
        .unwrap();
        let before = published();
        assert_eq!(
            run_code_read_view_pass(&state, &edges_dir, &mut cursor, false),
            CodeViewPass::Republished
        );
        assert!(!Arc::ptr_eq(&before, &published()));
        assert_eq!(
            run_code_read_view_pass(&state, &edges_dir, &mut cursor, false),
            CodeViewPass::Unchanged
        );
    }

    #[tokio::test]
    async fn read_artifact_source_rejects_oversized_http_response() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let response = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: application/json\r\n",
                "Content-Length: 1048577\r\n",
                "\r\n",
                "{}"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let err = read_artifact_source(&format!("http://{addr}/artifact.json"))
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("too large"), "got: {err}");
    }
}
