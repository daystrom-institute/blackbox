use futures::StreamExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::{Query, State as AxumState};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::state::{BlackboxServer, SharedState};
use crate::artifacts::{
    self, ArtifactInstallParams, ArtifactListParams, ArtifactRemoveParams, ArtifactSupersedeParams,
};
use crate::chunker;
use crate::edge_index;
use crate::entity_ref;
use crate::index;
use crate::orchestration;
use crate::orchestration::providers::Provider;
use crate::packets;
use crate::projects::ProjectRecord;
use crate::tools::bro_helpers::{
    build_member_entry, infer_provider_from_path, roster_entry_key, split_csv,
};
use crate::tools::bro_runtime_params::{BroRosterEntry, RosterQuery};

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

pub(crate) async fn admin_packet_compile(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<Value>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let p: packets::CompileParams = match serde_json::from_value(req) {
        Ok(p) => p,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("compile params parse: {e}"),
            )
                .into_response();
        }
    };
    let result = state.packets.read().compile(&p);
    match result {
        Ok(msg) => axum::Json(json!({"status": "ok", "message": msg})).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("compile: {e:#}"),
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
        let response = client.get(source).send().await?.error_for_status()?;
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
            let chunk = chunk?;
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
                | artifacts::ArtifactKind::Atom
                | artifacts::ArtifactKind::Cron
        ),
        "error.retired_artifact_kind: workflows, atoms and crons cannot be activated"
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
                | artifacts::ArtifactKind::Atom
                | artifacts::ArtifactKind::Cron
        ),
        "error.retired_artifact_kind: workflows, atoms and crons cannot be activated"
    );
    let mut completed = Vec::new();
    let mut failed = "validation";
    let kind = p.kind;
    let requested_name = p.name.clone().or_else(|| {
        value
            .get("name")
            .or_else(|| value.get("domain"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
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
                "error.retired_artifact_kind: workflows, atoms and crons cannot be activated"
            );
        }
        artifacts::ArtifactKind::Packet => vec!["packet_compilation"],
        artifacts::ArtifactKind::Brofile => vec!["brofile_file", "brofile_verification"],
        artifacts::ArtifactKind::Team => {
            vec!["teamplate_file", "team_instance", "team_verification"]
        }
        artifacts::ArtifactKind::Cron => {
            anyhow::bail!(
                "error.retired_artifact_kind: workflows, atoms and crons cannot be activated"
            );
        }
        _ => vec![],
    });
    remaining.push("catalog_persistence");
    if has_supersession {
        remaining.push("previous_runtime_deactivation");
    }
    if kind == artifacts::ArtifactKind::Agent {
        remaining.extend([
            "agent_warnings",
            "agent_embedding_queue",
            "agent_provenance",
        ]);
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
        let identity_field = if kind == artifacts::ArtifactKind::Packet {
            "domain"
        } else {
            "name"
        };
        value[identity_field] = Value::String(effective_name);
        // Preserve version's original JSON type unless an override was requested.
        if p.version.is_some() {
            value["version"] = if value.get("version").is_some_and(Value::is_number)
                || matches!(
                    kind,
                    artifacts::ArtifactKind::Workflow
                        | artifacts::ArtifactKind::Agent
                        | artifacts::ArtifactKind::Atom
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
        let mut installed_agent: Option<(
            orchestration::agents::types::AgentRef,
            orchestration::agents::types::AgentManifest,
            Vec<String>,
        )> = None;
        match p.kind {
            artifacts::ArtifactKind::Workflow => {
                anyhow::bail!(
                    "error.retired_artifact_kind: workflows, atoms and crons cannot be activated"
                );
            }
            artifacts::ArtifactKind::Packet => {
                let params: packets::CompileParams = serde_json::from_value(value.clone())?;
                completed.push("validation");
                failed = "packet_compilation";
                state.packets.read().compile(&params)?;
                completed.push("packet_compilation");
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
                // A team artifact IS a teamplate. Install materializes it like
                // its siblings (brofile → brofile store, cron → spec + loop):
                // write the teamplate store, then instantiate the team under the
                // teamplate's own name, so ensemble actors — which resolve
                // instantiated teams only (`load_team`, no teamplate fallback) —
                // can dispatch it immediately (gap-37a280a6).
                let tp: orchestration::team::Teamplate = serde_json::from_value(value.clone())?;
                if tp.advisor.is_some() {
                    anyhow::bail!(
                        "team artifact '{}' declares an advisor; automatic team advisors are \
                     retired. Omit advisor and use explicit bro_exec or bro_resume calls",
                        tp.name
                    );
                }
                // Same fail-loud-at-install posture as agent installs: member
                // brofiles must already exist (install brofiles before teams).
                for member in &tp.members {
                    if orchestration::brofile::resolve_brofile(
                        &member.brofile,
                        &state.store_dir,
                        None,
                    )
                    .is_none()
                    {
                        anyhow::bail!(
                            "team artifact '{}': member brofile not found: {} \
                         (install brofiles before teams)",
                            tp.name,
                            member.brofile
                        );
                    }
                }
                completed.push("validation");
                failed = "teamplate_file";
                orchestration::team::save_teamplate(&tp, "global", &state.store_dir, None)?;
                completed.push("teamplate_file");
                failed = "team_instance";
                // Re-install/upgrade must not clobber a live team's member
                // sessions: instantiate only when no team holds the name yet.
                let _lock = orchestration::team::lock_teams();
                if orchestration::team::load_team(&tp.name, &state.store_dir).is_none() {
                    orchestration::team::instantiate_team(&tp, &tp.name, None, &state.store_dir)?;
                }
                completed.push("team_instance");
                failed = "team_verification";
                if orchestration::team::load_team(&tp.name, &state.store_dir)
                    .is_none_or(|saved| saved.name != tp.name)
                {
                    anyhow::bail!("saved team is unavailable under its installed identity");
                }
                completed.push("team_verification");
            }
            artifacts::ArtifactKind::Cron => {
                anyhow::bail!(
                    "error.retired_artifact_kind: workflows, atoms and crons cannot be activated"
                );
            }
            artifacts::ArtifactKind::Agent => {
                if !value.is_object() {
                    anyhow::bail!("agent artifact must be a JSON object");
                }
                let adapter_registry = state.agent_adapter_registry.read();
                let catalog = state.artifacts.read();
                let ctx = orchestration::agents::validate::InstallCtx {
                    adapter_registry: &adapter_registry,
                    brofile_exists: |name: &str| -> bool {
                        catalog
                            .metadata_for(artifacts::ArtifactKind::Brofile, name)
                            .ok()
                            .flatten()
                            .is_some_and(|m| m.active)
                    },
                    agent_exists: |name: &str| -> bool {
                        catalog
                            .metadata_for(artifacts::ArtifactKind::Agent, name)
                            .ok()
                            .flatten()
                            .is_some_and(|m| m.active)
                    },
                };
                orchestration::agents::validate::validate_agent_install(&value, &ctx)?;
                drop(catalog);
                let manifest_value = value
                    .get("manifest")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("agent artifact missing manifest"))?;
                let mut manifest: orchestration::agents::types::AgentManifest =
                    serde_json::from_value(manifest_value)?;
                let name = p
                    .name
                    .clone()
                    .or_else(|| {
                        value
                            .get("name")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .ok_or_else(|| anyhow::anyhow!("agent artifact missing name"))?;
                let version = p
                    .version
                    .clone()
                    .or_else(|| value.get("version").and_then(artifact_version_string))
                    .ok_or_else(|| anyhow::anyhow!("agent artifact missing version"))?
                    .parse::<u32>()
                    .map_err(|_| anyhow::anyhow!("agent artifact version must parse as u32"))?;
                let agent_ref = orchestration::agents::types::AgentRef { name, version };
                manifest.embedding = Some(crate::embed_runtime::agent_manifest_embedding(
                    &agent_ref, &manifest,
                ));
                value["manifest"]["embedding"] = serde_json::to_value(&manifest.embedding)?;
                let install_warnings = agent_install_warnings(state, &manifest);
                installed_agent = Some((agent_ref, manifest, install_warnings));
            }
            artifacts::ArtifactKind::Atom => {
                anyhow::bail!(
                    "error.retired_artifact_kind: workflows, atoms and crons cannot be activated"
                );
            }
        }
        if !completed.contains(&"validation") {
            completed.push("validation");
        }
        failed = "catalog_persistence";
        let mut meta = state.artifacts.write().install_value(
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
        if let Some((agent_ref, manifest, install_warnings)) = installed_agent {
            failed = "agent_warnings";
            if !install_warnings.is_empty() {
                meta = state.artifacts.write().update_install_warnings(
                    artifacts::ArtifactKind::Agent,
                    &agent_ref.name,
                    install_warnings,
                )?;
            }
            completed.push("agent_warnings");
            failed = "agent_embedding_queue";
            crate::embed_runtime::enqueue_agent_manifest(&agent_ref, &manifest);
            completed.push("agent_embedding_queue");
            failed = "agent_provenance";
            persist_agent_provenance_edges(state, &agent_ref, &manifest)?;
            completed.push("agent_provenance");
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
        .filter(|entry| {
            // Team is deliberately absent: boot-restoring teams from active
            // artifacts would resurrect deliberately-dissolved teams
            // (dissolution must stick; re-install is the explicit
            // re-materialization path). Teamplate/team stores are
            // file-backed and survive restarts on their own.
            matches!(
                entry.kind,
                artifacts::ArtifactKind::Packet | artifacts::ArtifactKind::Brofile
            )
        })
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
                    "error.retired_artifact_kind: workflows, atoms and crons cannot be activated"
                );
            }
            artifacts::ArtifactKind::Packet => {
                let params: packets::CompileParams = serde_json::from_value(value.clone())
                    .with_context(|| format!("parsing packet artifact '{}'", entry.name))?;
                // Idempotent on purpose: this runs on every daemon boot, and
                // the unconditional compile used to mint a new packet file
                // per restart (33 artifacts × N restarts ≈ 2k duplicates).
                match state
                    .packets
                    .read()
                    .compile_idempotent(&params)
                    .with_context(|| format!("compiling packet artifact '{}'", entry.name))?
                {
                    packets::CompileOutcome::Created(id) => {
                        tracing::info!(artifact = %entry.name, packet = %id, "packet artifact compiled");
                    }
                    packets::CompileOutcome::UnchangedExisting(id) => {
                        tracing::debug!(artifact = %entry.name, packet = %id, "packet artifact unchanged; reusing existing packet");
                    }
                }
                restored += 1;
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

pub(crate) fn agent_install_warnings(
    state: &Arc<SharedState>,
    manifest: &orchestration::agents::types::AgentManifest,
) -> Vec<String> {
    let Some(overlay) = manifest.filter_overlay.as_ref() else {
        return Vec::new();
    };
    let (base_allow, base_disallow) = if let Some(brofile_ref) = manifest.brofile_ref.as_ref() {
        let Some(brofile) =
            orchestration::brofile::resolve_brofile(brofile_ref, &state.store_dir, None)
        else {
            return Vec::new();
        };
        match brofile.filters {
            Some(filters) => (filters.allow, filters.disallow),
            None => (Vec::new(), Vec::new()),
        }
    } else if let Some(inline) = manifest.brofile_inline.as_ref() {
        BlackboxServer::extract_inline_filters(inline)
    } else {
        (Vec::new(), Vec::new())
    };

    let mut warnings = Vec::new();
    for allowed in &overlay.allow {
        if base_disallow.contains(allowed) {
            warnings.push(format!(
                "filter_overlay.allow `{allowed}` is also disallowed by the base brofile; deny-wins merge keeps it disallowed"
            ));
        }
    }
    for disallowed in &overlay.disallow {
        if base_allow.contains(disallowed) {
            warnings.push(format!(
                "filter_overlay.disallow `{disallowed}` overrides a base brofile allow entry"
            ));
        }
    }
    warnings
}

pub(crate) fn artifact_version_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub(crate) fn persist_agent_provenance_edges(
    state: &Arc<SharedState>,
    agent_ref: &orchestration::agents::types::AgentRef,
    manifest: &orchestration::agents::types::AgentManifest,
) -> anyhow::Result<()> {
    use orchestration::agents::types::AgentProvenance;
    let Some(AgentProvenance::Distilled {
        evidence_session_ids,
        created_from_threads,
        ..
    }) = manifest.provenance.as_ref()
    else {
        return Ok(());
    };
    let source = entity_ref::EntityRef::Agent {
        name: agent_ref.name.clone(),
        version: agent_ref.version,
    };
    let mut edges = Vec::new();
    for session in evidence_session_ids {
        let target = entity_ref::EntityRef::parse(session)?;
        if !matches!(target, entity_ref::EntityRef::Session { .. }) {
            anyhow::bail!("distilled agent evidence ref is not a session: {session}");
        }
        edges.push(agent_derived_from_edge(source.clone(), target));
    }
    for thread in created_from_threads {
        let target = entity_ref::EntityRef::parse(thread)?;
        if !matches!(target, entity_ref::EntityRef::Thread { .. }) {
            anyhow::bail!("distilled agent thread ref is not a thread: {thread}");
        }
        edges.push(agent_derived_from_edge(source.clone(), target));
    }
    let edges_dir = edge_sidecar_dir(state);
    let written = edge_index::append_explicit_edges(&edges_dir, "agents", &edges)?;
    if written > 0 {
        // Persist first and wake the single-flight watcher. An artifact tool
        // must never synchronously parse the complete project graph merely to
        // publish a handful of provenance edges.
        state.nudge_edge_index_rebuild();
    }
    Ok(())
}

pub(crate) fn agent_derived_from_edge(
    source: entity_ref::EntityRef,
    target: entity_ref::EntityRef,
) -> edge_index::Edge {
    edge_index::Edge {
        source,
        kind: "DERIVED_FROM".into(),
        target,
        provenance: chunker::EdgeProvenance::Explicit,
        confidence: chunker::EdgeConfidence::Exact,
        metadata: Default::default(),
        project_id: None,
    }
}

pub(crate) fn deactivate_artifact(
    state: &Arc<SharedState>,
    kind: artifacts::ArtifactKind,
    name: &str,
) -> anyhow::Result<()> {
    match kind {
        artifacts::ArtifactKind::Workflow => {}
        artifacts::ArtifactKind::Packet => {
            state.packets.read().remove_domain(name)?;
        }
        artifacts::ArtifactKind::Brofile => {
            orchestration::brofile::delete_brofile(name, "global", &state.store_dir, None);
        }
        artifacts::ArtifactKind::Agent => {
            // No separate registry to deactivate for agents (yet).
        }
        artifacts::ArtifactKind::Atom => {}
        artifacts::ArtifactKind::Team => {
            // Teams are stored purely as artifacts; no separate registry to deactivate.
        }
        artifacts::ArtifactKind::Cron => {}
    }
    Ok(())
}

pub(crate) fn edge_sidecar_dir(state: &SharedState) -> std::path::PathBuf {
    bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
        &state.idx.read().reindex_config().projects_path,
    )
}

pub(crate) fn rebuild_edge_index_from_shared(
    state: &SharedState,
    include_tantivy_projection: bool,
) -> anyhow::Result<()> {
    let edges_dir = edge_sidecar_dir(state);
    rebuild_edge_index_from_shared_at(state, include_tantivy_projection, &edges_dir)
}

pub(crate) fn rebuild_edge_index_from_shared_at(
    state: &SharedState,
    include_tantivy_projection: bool,
    edges_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let registered_project_ids = state.corpus_registered_project_ids();
    let prepared = (|| -> anyhow::Result<_> {
        let authority = capture_edge_rebuild_authority(&edges_dir, Some(&registered_project_ids))?;
        let max_bytes = edge_index_rebuild_max_input_bytes();
        if authority.signature.bytes > max_bytes {
            anyhow::bail!(
                "edge-index rebuild refused: active sidecar input is {} bytes (limit {}); compact/rematerialize the active edge set before retrying",
                authority.signature.bytes,
                max_bytes
            );
        }
        let rebuilt = build_edge_index_from_shared_at_authority(
            state,
            include_tantivy_projection,
            edges_dir,
            &authority.manifest,
        )?;
        let (selectors, searcher) = {
            let index = state.idx.read();
            (index.refresh_active_code_selectors()?, index.searcher())
        };
        Ok((authority, rebuilt, selectors, searcher))
    })();
    let (authority, rebuilt, selectors, searcher) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = state.code_sources.store().record_health_failure(
                "_edge_index",
                "rebuild_failed",
                &error.to_string(),
            );
            return Err(error);
        }
    };
    if let Err(error) = bbox_edge_sidecar::snapshot::with_manifest_coordinator(|| {
        let current = capture_edge_rebuild_authority(&edges_dir, Some(&registered_project_ids))?;
        if current != authority {
            anyhow::bail!(
                "edge-index rebuild input changed while it was being parsed; refusing stale publication"
            );
        }
        *state.code_read_view.write() = std::sync::Arc::new(super::CodeReadView {
            active_selectors: selectors,
            searcher,
            edge_index: std::sync::Arc::new(rebuilt),
            catalog_epoch: state.records_provider.records_snapshot().authority_epoch,
            git_overlays: super::state::read_git_overlays_for_view(
                &state.project_authority,
                &edges_dir,
                &state.git_transport_cutover,
                &state.code_sources,
            ),
        });
        state
            .edge_index_ready
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }) {
        let _ = state.code_sources.store().record_health_failure(
            "_edge_index",
            "rebuild_failed",
            &error.to_string(),
        );
        tracing::error!(%error, "edge-index rebuild manifest coordination failed");
        return Err(error);
    }
    state
        .code_sources
        .store()
        .clear_health_failure("_edge_index", "rebuild_failed")?;
    Ok(())
}

fn build_edge_index_from_shared_at_authority(
    state: &SharedState,
    include_tantivy_projection: bool,
    edges_dir: &std::path::Path,
    authority: &edge_index::SidecarManifestAuthority,
) -> anyhow::Result<edge_index::EdgeIndex> {
    let started = std::time::Instant::now();
    // F3: the COMPLETE catalog id set, through the one shared accessor that
    // also seeds startup, the storage tools, and the background GC pass.
    let registered_project_ids = state.corpus_registered_project_ids();
    // The store read-locks cover ONLY the in-memory store projections (fast).
    // The sidecar load below is a multi-GB disk parse and must run with NO
    // store guards held: parking_lot is fair, so a writer queued behind these
    // guards blocks every new reader for the scan duration (measured 13-100s+
    // in prod), stalling tokio workers that touch any store.
    //
    // All guards must also drop before acquiring `edge_index.write()`.
    // Holding idx.read()/kb.read()/etc. across that acquisition is a deadlock
    // hazard:
    //   A (this rebuild)        holds idx.read, wants edge_index.write
    //   R (auto-reindex commit) wants idx.write -> queues behind A; a queued
    //                           writer then blocks new idx *readers* (parking_lot
    //                           is fair, so readers don't starve the writer)
    //   D (a graph tool, e.g.   holds edge_index.read (live arg), wants idx.read
    //      bbox_blame)          -> blocked behind R
    // => A waits on D's edge_index.read, D waits on R's queued idx.write, R waits
    //    on A's idx.read. Cycle. Acquiring edge_index.write() with no store locks
    //    held removes A from the cycle entirely.
    let (mut rebuilt, mut seen) = {
        let idx = state.idx.read();
        let kb = state.kb.read();
        let threads = state.threads.read();
        let notes = state.notes.read();
        let task_store = state.task_store.read();
        let roadmap = state.roadmap.read();
        edge_index::EdgeIndex::project_store_edges(&edge_index::EdgeStoreRefs {
            index: &idx,
            knowledge: &kb,
            threads: &threads,
            notes: &notes,
            session_brofile_rows: task_store.session_brofile_rows(),
            roadmap: &roadmap,
            edges_dir: edges_dir.to_path_buf(),
            registered_project_ids: Some(registered_project_ids.clone()),
            include_tantivy_projection,
            include_observed: true,
        })
        // all store read-guards drop here
    };
    rebuilt.load_sidecar_edges_from_authority(
        &edges_dir,
        Some(&registered_project_ids),
        &mut seen,
        true,
        authority,
    )?;
    rebuilt.log_rebuilt(include_tantivy_projection, started);
    Ok(rebuilt)
}

const DEFAULT_EDGE_INDEX_REBUILD_MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const DEFAULT_EDGE_INDEX_NUDGE_MAX_CURRENT_EDGES: usize = 250_000;

fn edge_index_rebuild_max_input_bytes() -> u64 {
    std::env::var("BLACKBOX_EDGE_INDEX_REBUILD_MAX_INPUT_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_EDGE_INDEX_REBUILD_MAX_INPUT_BYTES)
}

pub(crate) fn ensure_edge_index_rebuild_admitted_at(
    state: &SharedState,
    edges_dir: &std::path::Path,
    additional_bytes: u64,
) -> anyhow::Result<u64> {
    let registered_project_ids = state.corpus_registered_project_ids();
    let authority = capture_edge_rebuild_authority(edges_dir, Some(&registered_project_ids))?;
    let max_bytes = edge_index_rebuild_max_input_bytes();
    let projected_bytes = authority.signature.bytes.saturating_add(additional_bytes);
    if projected_bytes > max_bytes {
        anyhow::bail!(
            "edge-index rebuild refused: projected active sidecar input is {} bytes (limit {}); compact/rematerialize the active edge set before retrying",
            projected_bytes,
            max_bytes
        );
    }
    Ok(max_bytes)
}

fn edge_index_nudge_max_current_edges() -> usize {
    std::env::var("BLACKBOX_EDGE_INDEX_NUDGE_MAX_CURRENT_EDGES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_EDGE_INDEX_NUDGE_MAX_CURRENT_EDGES)
}

fn should_rebuild_edge_index(
    nudged: bool,
    sidecars_changed: bool,
    published_edge_count: usize,
    nudge_limit: usize,
) -> bool {
    sidecars_changed || nudged && published_edge_count <= nudge_limit
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EdgeSidecarSignature {
    files: u64,
    bytes: u64,
    modified_nanos: u128,
    path_identity: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct EdgeRebuildAuthority {
    manifest: edge_index::SidecarManifestAuthority,
    signature: EdgeSidecarSignature,
}

fn fold_sidecar_path(
    signature: &mut EdgeSidecarSignature,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
    path: &std::path::Path,
) {
    use std::hash::{Hash, Hasher};

    if !seen.insert(path.to_path_buf()) {
        return;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if !meta.is_file() {
        return;
    }
    signature.files += 1;
    signature.bytes = signature.bytes.saturating_add(meta.len());
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    signature.modified_nanos = signature.modified_nanos.wrapping_add(modified);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    meta.len().hash(&mut hasher);
    modified.hash(&mut hasher);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.dev().hash(&mut hasher);
        meta.ino().hash(&mut hasher);
    }
    signature.path_identity ^= hasher.finish();
}

fn sidecar_project_is_admitted(
    path: &std::path::Path,
    registered_project_ids: Option<&std::collections::HashSet<String>>,
) -> bool {
    let Some(registered) = registered_project_ids else {
        return true;
    };
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|project_id| project_id == "agents" || registered.contains(project_id))
}

fn fold_jsonl_dir(
    signature: &mut EdgeSidecarSignature,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
    dir: &std::path::Path,
    registered_project_ids: Option<&std::collections::HashSet<String>>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
            && sidecar_project_is_admitted(&path, registered_project_ids)
        {
            fold_sidecar_path(signature, seen, &path);
        }
    }
}

fn capture_edge_rebuild_authority(
    edges_dir: &std::path::Path,
    registered_project_ids: Option<&std::collections::HashSet<String>>,
) -> anyhow::Result<EdgeRebuildAuthority> {
    let mut manifest = edge_index::SidecarManifestAuthority::capture(edges_dir)?;
    // `updated_at` is operational metadata, not loader authority. Reindex can
    // refresh it while leaving every selected path and selector unchanged;
    // including it in the before/after equality made a semantic no-op look
    // like an input mutation.
    if let edge_index::SidecarManifestAuthority::Manifest(index) = &mut manifest {
        index.updated_at = None;
    }
    let mut sig = EdgeSidecarSignature {
        files: 0,
        bytes: 0,
        modified_nanos: 0,
        path_identity: 0,
    };
    let mut seen = std::collections::HashSet::new();

    match &manifest {
        edge_index::SidecarManifestAuthority::Manifest(index) => {
            for loadable in index.active_paths_for_loader(edges_dir)? {
                fold_sidecar_path(&mut sig, &mut seen, &loadable.path);
            }
            // Manifest mode still unions post-migration explicit/observed and
            // top-level compatibility lanes. No inactive materialized tree is
            // an input and therefore none belongs in the watcher signature.
            fold_jsonl_dir(&mut sig, &mut seen, edges_dir, registered_project_ids);
            fold_jsonl_dir(
                &mut sig,
                &mut seen,
                &edges_dir.join("explicit"),
                registered_project_ids,
            );
            fold_jsonl_dir(
                &mut sig,
                &mut seen,
                &edges_dir.join("observed"),
                registered_project_ids,
            );
        }
        edge_index::SidecarManifestAuthority::LegacyMissing => {
            fold_jsonl_dir(&mut sig, &mut seen, edges_dir, registered_project_ids);
            for lane in ["derived", "explicit", "observed"] {
                let lane_dir = edges_dir.join(lane);
                fold_jsonl_dir(&mut sig, &mut seen, &lane_dir, registered_project_ids);
                if let Ok(entries) = std::fs::read_dir(&lane_dir) {
                    for entry in entries.filter_map(Result::ok) {
                        if entry.path().is_dir() {
                            fold_jsonl_dir(
                                &mut sig,
                                &mut seen,
                                &entry.path(),
                                registered_project_ids,
                            );
                        }
                    }
                }
            }
        }
    }
    // Fold the semantic manifest authority, not manifest-index.json's inode
    // and mtime. The reindexer can rewrite a byte-equivalent authority file;
    // file metadata is not a graph input and must not launch a 1+ GiB parse.
    // Active-pointer and selector changes remain visible through the stable
    // serialized authority, even when already-materialized JSONL metadata is
    // unchanged (for example, a branch switch back to a retained snapshot).
    let mut manifest_hasher = std::collections::hash_map::DefaultHasher::new();
    match &manifest {
        edge_index::SidecarManifestAuthority::Manifest(index) => {
            std::hash::Hash::hash(&1_u8, &mut manifest_hasher);
            std::hash::Hash::hash(&serde_json::to_vec(index)?, &mut manifest_hasher);
        }
        edge_index::SidecarManifestAuthority::LegacyMissing => {
            std::hash::Hash::hash(&0_u8, &mut manifest_hasher);
        }
    }
    sig.path_identity ^= std::hash::Hasher::finish(&manifest_hasher);
    Ok(EdgeRebuildAuthority {
        manifest,
        signature: sig,
    })
}

#[cfg(test)]
fn edge_sidecar_signature(edges_dir: &std::path::Path) -> anyhow::Result<EdgeSidecarSignature> {
    capture_edge_rebuild_authority(edges_dir, None).map(|authority| authority.signature)
}

/// Watcher thread that rebuilds the EdgeIndex when edge sidecars change.
/// The auto-reindex thread writes new docs + edge sidecars every interval,
/// but it can't trigger a rebuild itself (it spawns before SharedState exists).
/// The watcher uses sidecar-only rebuilds so background maintenance does not
/// materialize every stored Tantivy document.
pub(crate) fn spawn_edge_index_rebuild_watcher(
    state: Arc<SharedState>,
    interval: std::time::Duration,
) {
    std::thread::Builder::new()
        .name("blackbox-edge-rebuild".into())
        .spawn(move || {
            let _scope = crate::util::BlockingScope::enter();
            // Nudge channel: async tool handlers whose store mutations change
            // projected edges wake this thread instead of rebuilding inline.
            let nudge_rx = state.edge_rebuild_nudge_rx.lock().unwrap().take();
            // Eager startup already published a graph. Deferred startup did
            // not: rebuild immediately in the background, and keep graph
            // consumers fail-closed until this publication succeeds.
            let mut pending_nudge = !state
                .edge_index_ready
                .load(std::sync::atomic::Ordering::Acquire);
            if !pending_nudge {
                std::thread::sleep(std::time::Duration::from_secs(20));
            }
            let mut last_seen: u64 = state.idx.read().num_docs();
            let edges_dir = edge_sidecar_dir(&state);
            let mut last_signature = capture_edge_rebuild_authority(
                &edges_dir,
                Some(&state.corpus_registered_project_ids()),
            )
            .ok()
            .map(|authority| authority.signature);
            loop {
                if !pending_nudge {
                    pending_nudge = match &nudge_rx {
                    Some(rx) => match rx.recv_timeout(interval) {
                        Ok(()) => true,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
                        // All senders dropped — SharedState is gone; exit.
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    },
                    None => {
                        std::thread::sleep(interval);
                        false
                    }
                    };
                }
                let Some(publication_guard) =
                    state.index_writer.try_begin_edge_index_rebuild()
                else {
                    tracing::debug!(
                        pending_nudge,
                        "edge-index watcher deferred while a reindex publication is active"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                };
                let nudged = std::mem::take(&mut pending_nudge);
                let current = state.idx.read().num_docs();
                let registered_project_ids = state.corpus_registered_project_ids();
                let signature = match capture_edge_rebuild_authority(
                    &edges_dir,
                    Some(&registered_project_ids),
                ) {
                    Ok(authority) => authority.signature,
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            nudged,
                            "edge-index watcher authority capture failed; keeping the last published graph"
                        );
                        if !state
                            .edge_index_ready
                            .load(std::sync::atomic::Ordering::Acquire)
                        {
                            pending_nudge = true;
                            drop(publication_guard);
                            std::thread::sleep(interval);
                        }
                        last_seen = current;
                        continue;
                    }
                };
                let sidecars_changed = Some(signature) != last_signature;
                let published_edge_count = state
                    .code_read_view
                    .read()
                    .edge_index
                    .edge_count();
                if should_rebuild_edge_index(
                    nudged,
                    sidecars_changed,
                    published_edge_count,
                    edge_index_nudge_max_current_edges(),
                ) {
                    let started = std::time::Instant::now();
                    tracing::info!(
                        current_docs = current,
                        sidecar_files = signature.files,
                        sidecar_bytes = signature.bytes,
                        nudged,
                        sidecars_changed,
                        "edge-index watcher rebuild started"
                    );
                    match rebuild_edge_index_from_shared(&state, false) {
                        Ok(()) => {
                            tracing::info!(
                                prev_docs = last_seen,
                                new_docs = current,
                                sidecar_files = signature.files,
                                sidecar_bytes = signature.bytes,
                                nudged,
                                sidecars_changed,
                                elapsed_ms = started.elapsed().as_millis(),
                                "edge-index watcher: sidecars changed or store nudge, EdgeIndex rebuilt"
                            );
                            last_signature = capture_edge_rebuild_authority(
                                &edges_dir,
                                Some(&state.corpus_registered_project_ids()),
                            )
                            .ok()
                            .map(|authority| authority.signature)
                            .or(Some(signature));
                            let _ = state.code_sources.store().clear_health_failure(
                                "_edge_index",
                                "store_refresh_deferred",
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                nudged,
                                elapsed_ms = started.elapsed().as_millis(),
                                "edge-index watcher rebuild failed; retaining prior signature for retry"
                            );
                            if !state
                                .edge_index_ready
                                .load(std::sync::atomic::Ordering::Acquire)
                            {
                                pending_nudge = true;
                                drop(publication_guard);
                                std::thread::sleep(interval);
                            }
                        }
                    }
                } else if nudged {
                    let detail = format!(
                        "structured-edge refresh deferred: the published graph has {published_edge_count} edges (nudge rebuild limit {}) and sidecar authority did not change",
                        edge_index_nudge_max_current_edges()
                    );
                    let _ = state.code_sources.store().record_health_failure(
                        "_edge_index",
                        "store_refresh_deferred",
                        &detail,
                    );
                    tracing::warn!(
                        published_edge_count,
                        limit = edge_index_nudge_max_current_edges(),
                        "edge-index watcher deferred a store-only nudge to avoid rebuilding a large unchanged sidecar graph"
                    );
                } else if current != last_seen {
                    let searcher = { state.idx.read().searcher() };
                    state.publish_code_read_searcher(searcher);
                    tracing::debug!(
                        prev_docs = last_seen,
                        new_docs = current,
                        sidecar_files = signature.files,
                        sidecar_bytes = signature.bytes,
                        "edge-index watcher: corpus changed without sidecar changes; pinned searcher refreshed"
                    );
                }
                last_seen = current;
            }
        })
        .expect("failed to spawn edge index rebuild watcher");
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
    let notes = state
        .notes
        .read()
        .all()
        .iter()
        .filter(|note| note.project.as_deref() == Some(project))
        .count();
    let pins = state.pins.read().project_ref_count(project);
    let packets = state
        .packets
        .read()
        .list_all()?
        .iter()
        .filter(|packet| packet.project.as_deref() == Some(project))
        .count();
    let slack_channel_bindings = state.slack_channel_bindings.list(None, Some(project)).len();
    let slack_proposal_links = state.slack_proposal_links.project_ref_count(project);
    let teams = orchestration::team::load_all_teams(&state.store_dir)
        .iter()
        .filter(|team| team.project_dir.as_deref() == Some(project))
        .count();
    let whiteboards = state
        .whiteboards
        .list_ids()
        .iter()
        .filter(|id| {
            state
                .whiteboards
                .get(id)
                .is_some_and(|board| board.read().project == project)
        })
        .count();
    let gaps = state
        .gaps
        .read()
        .all()
        .iter()
        .filter(|gap| gap.project.as_deref() == Some(project))
        .count();
    let roadmap = state
        .roadmap
        .read()
        .all_items()
        .iter()
        .filter(|item| item.project.as_deref() == Some(project))
        .count();

    Ok(json!({
        "knowledge": knowledge,
        "threads": threads,
        "notes": notes,
        "pins": pins,
        "packets": packets,
        "slack_channel_bindings": slack_channel_bindings,
        "slack_proposal_links": slack_proposal_links,
        "teams": teams,
        "whiteboards": whiteboards,
        "gaps": gaps,
        "roadmap": roadmap,
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
/// store also contains working-tree repo entries for overlay construction, so
/// reading it directly would publish provisional bytes under `knowledge:*`.
pub(crate) fn published_knowledge_for_embedding(
    state: &std::sync::Arc<SharedState>,
    project_dir: Option<&str>,
) -> anyhow::Result<Vec<crate::knowledge::KnowledgeEntry>> {
    let view = super::BlackboxServer::new(state.clone())
        .session_knowledge_view(project_dir, Some("published"))?;
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
    for entry in entries.iter().filter(|e| {
        knowledge_entry_belongs_to_project(e, project_dir, &project.project_id)
            && matches!(
                e.status,
                crate::knowledge::Status::Active | crate::knowledge::Status::Superseded
            )
    }) {
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
    let notes = state
        .notes
        .write()
        .rename_project_refs(old_project, new_project)?;
    if notes > 0 {
        // This sync migration helper cannot await; notes persistence is write-behind here.
        state.notes_persister.request();
    }
    let pins = state
        .pins
        .write()
        .rename_project_refs(old_project, new_project)?;
    if pins > 0 {
        // This sync migration helper cannot await; pins persistence is write-behind here.
        state.pins_persister.request();
    }
    let packets = state
        .packets
        .read()
        .rename_project_refs(old_project, new_project)?;
    let slack_channel_bindings = state.slack_channel_bindings.rename_project_refs(
        old_project,
        new_project,
        Some(record.project_id.as_str()),
    )?;
    let slack_proposal_links = state
        .slack_proposal_links
        .rename_project_refs(old_project, new_project)?;
    let teams =
        orchestration::team::rename_project_refs(&state.store_dir, old_project, new_project);
    let whiteboards = state
        .whiteboards
        .rename_project_refs(old_project, new_project)?;

    // Phase-2 §8.4 coverage fixes: gaps and roadmap rows previously
    // orphaned silently on rename, and webhooks kept a stale execution
    // target. Same write-behind persistence discipline as their siblings.
    let gaps = state
        .gaps
        .write()
        .rename_project_refs(old_project, new_project)?;
    let roadmap = state
        .roadmap
        .write()
        .rename_project_refs(old_project, new_project)?;
    if roadmap > 0 {
        state.roadmap_persister.request();
    }

    Ok(json!({
        "knowledge": knowledge,
        "threads": threads,
        "notes": notes,
        "pins": pins,
        "packets": packets,
        "slack_channel_bindings": slack_channel_bindings,
        "slack_proposal_links": slack_proposal_links,
        "teams": teams,
        "whiteboards": whiteboards,
        "gaps": gaps,
        "roadmap": roadmap,
    }))
}

pub(crate) fn persist_named_json<T: Serialize>(
    dir: &Path,
    name: &str,
    value: &T,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(value)?)?;
    Ok(())
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
    let bf = orchestration::brofile::Brofile {
        name: req.name.clone(),
        provider,
        account: req.account,
        lens: req.lens,
        model: req.model,
        effort: req.effort,
        tool_defaults: None,
        filters: None,
        surface: None,
        coerce_workspace: None,
        runtime: None,
        context: None,
        code_mode: None,
        service_tier: req.service_tier,
    };
    if let Err(e) = orchestration::brofile::save_brofile(&bf, "global", &state.store_dir, None) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"status": "error", "name": req.name, "error": e.to_string()})),
        )
            .into_response();
    }
    axum::Json(json!({"status": "upserted", "name": req.name})).into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct AdminTeamUpsertReq {
    name: String,
    members: Vec<AdminTeamMemberReq>,
}

/// One member in an admin team upsert: either a bare brofile name
/// (legacy — member names auto-assigned m1..mN) or `{name, brofile}`
/// so members carry meaningful identities. Named members matter for
/// ensemble `${member.name}` prompt templating and whiteboard
/// auto-apply attribution (member name = registered board agent).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum AdminTeamMemberReq {
    Brofile(String),
    Named { name: String, brofile: String },
}

impl AdminTeamMemberReq {
    fn resolved(&self, index: usize) -> (String, String) {
        match self {
            Self::Brofile(brofile) => (format!("m{}", index + 1), brofile.clone()),
            Self::Named { name, brofile } => (name.clone(), brofile.clone()),
        }
    }
}

pub(crate) async fn admin_team_upsert(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminTeamUpsertReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let resolved: Vec<(String, String)> = req
        .members
        .iter()
        .enumerate()
        .map(|(i, m)| m.resolved(i))
        .collect();
    let teamplate = orchestration::team::Teamplate {
        name: req.name.clone(),
        members: resolved
            .iter()
            .map(|(name, brofile)| orchestration::team::TeamplateMember {
                brofile: brofile.clone(),
                alias: Some(name.clone()),
                count: 1,
            })
            .collect(),
        advisor: None,
        diversity_floor: None,
    };
    if let Err(error) =
        orchestration::team::save_teamplate(&teamplate, "global", &state.store_dir, None)
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": format!("teamplate was not saved: {error}")})),
        )
            .into_response();
    }
    let team = orchestration::team::Team {
        name: req.name.clone(),
        teamplate: req.name.clone(),
        members: resolved
            .iter()
            .map(|(name, brofile)| orchestration::team::TeamMember {
                name: name.clone(),
                brofile: brofile.clone(),
                session_id: None,
                task_history: Vec::new(),
            })
            .collect(),
        advisor: None,
        project_dir: None,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        diversity_floor: None,
    };
    let _lock = orchestration::team::lock_teams();
    orchestration::team::save_team(&team, &state.store_dir);
    axum::Json(json!({"status": "upserted", "name": req.name})).into_response()
}

pub(crate) async fn roster_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<RosterQuery>,
) -> Result<axum::Json<Vec<BroRosterEntry>>, axum::http::StatusCode> {
    let store_dir = state.store_dir.clone();
    let config = state.idx.read().reindex_config();

    let wanted_teams = split_csv(&query.teams);
    let wanted_bros = split_csv(&query.bros);
    let wanted_sessions = split_csv(&query.sessions);
    let wanted_providers: Vec<Provider> = split_csv(&query.providers)
        .iter()
        .filter_map(|p| p.parse::<Provider>().ok())
        .collect();

    let no_selectors =
        wanted_teams.is_empty() && wanted_bros.is_empty() && wanted_sessions.is_empty();

    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();

    // Team selectors — each contributes all members. Unknown teams are
    // skipped silently; the empty roster speaks for itself at the CLI layer.
    for tn in &wanted_teams {
        if let Some(team) = orchestration::team::load_team(tn, &store_dir) {
            for member in &team.members {
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    // Bro selectors — include every match across all teams (deduped by team::bro).
    if !wanted_bros.is_empty() {
        for team in orchestration::team::load_all_teams(&store_dir) {
            for member in &team.members {
                if !wanted_bros.iter().any(|b| b == &member.name) {
                    continue;
                }
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    // Session selectors — synthetic adhoc lanes.
    for sid in &wanted_sessions {
        let key = format!("session::{sid}");
        if !seen.insert(key) {
            continue;
        }
        let path = index::find_session_file(sid, &config.roots, config.codex_root.as_deref());
        let provider = path.as_deref().and_then(infer_provider_from_path);
        entries.push(BroRosterEntry {
            bro: sid.chars().take(8).collect(),
            bro_selector: sid.clone(),
            team: "adhoc".into(),
            provider: provider
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".into()),
            account: None,
            session_id: Some(sid.clone()),
            jsonl_path: path.map(|p| p.to_string_lossy().into_owned()),
            brofile: String::new(),
            model: None,
        });
    }

    // No selectors → full roster across every team (legacy default).
    if no_selectors {
        for team in orchestration::team::load_all_teams(&store_dir) {
            for member in &team.members {
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    // Bro selectors that the team-walk above didn't resolve fall
    // through here: we synthesize ad-hoc entries from currently-known
    // tasks whose `bro_label` matches. This is the only path that
    // surfaces brofile-only dispatched bros (workflow implementer /
    // advisor nodes) — they have no team membership, so the team
    // walk skips them. Without this, `bro tail keystone-impl` returns
    // an empty roster and the CLI bails with "bro does not exist".
    if !wanted_bros.is_empty() {
        let task_store = state.task_store.read();
        for task in task_store.all_tasks() {
            let inner = task.inner.lock();
            let label = match &inner.bro_label {
                Some(l) => l.clone(),
                None => continue,
            };
            // Match either bare-label (`keystone-impl`) or the
            // `team::member` form so callers can use either.
            let (team, member) = match label.split_once("::") {
                Some((t, m)) => (t.to_string(), m.to_string()),
                None => ("adhoc".to_string(), label.clone()),
            };
            let matches = wanted_bros.iter().any(|w| w == &member || w == &label);
            if !matches {
                continue;
            }
            let key = format!("{team}::{member}");
            if !seen.insert(key) {
                continue;
            }
            let session_id = if inner.session_id == "pending" {
                None
            } else {
                Some(inner.session_id.clone())
            };
            let jsonl_path = session_id.as_deref().and_then(|sid| {
                index::find_session_file(sid, &config.roots, config.codex_root.as_deref())
                    .map(|p| p.to_string_lossy().into_owned())
            });
            entries.push(BroRosterEntry {
                bro: member,
                bro_selector: label,
                team,
                provider: inner.provider.to_string(),
                account: None,
                session_id,
                jsonl_path,
                brofile: String::new(),
                model: None,
            });
        }
    }

    if !wanted_providers.is_empty() {
        entries.retain(|e| {
            e.provider
                .parse::<Provider>()
                .ok()
                .map(|p| wanted_providers.contains(&p))
                .unwrap_or(false)
        });
    }

    Ok(axum::Json(entries))
}
#[cfg(test)]
mod tests {
    use super::*;

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
            id: "embed-source".into(),
            title: "embed source".into(),
            content: content.into(),
            cluster: None,
            variants: Default::default(),
            category: crate::knowledge::Category::Memory,
            scope: crate::knowledge::Scope::Project,
            project: None,
            project_id: None,
            providers: Vec::new(),
            priority: crate::knowledge::Priority::Standard,
            weight: 100,
            status: crate::knowledge::Status::Active,
            approval: crate::knowledge::Approval::UserConfirmed,
            render: false,
            decay: false,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
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

    #[test]
    fn admin_team_upsert_accepts_bare_and_named_members() {
        let req: AdminTeamUpsertReq = serde_json::from_str(
            r#"{"name":"t","members":[
                "some-brofile",
                {"name":"security","brofile":"spec-security"}
            ]}"#,
        )
        .expect("mixed member shapes should parse");
        let resolved: Vec<(String, String)> = req
            .members
            .iter()
            .enumerate()
            .map(|(i, m)| m.resolved(i))
            .collect();
        assert_eq!(
            resolved,
            vec![
                ("m1".to_string(), "some-brofile".to_string()),
                ("security".to_string(), "spec-security".to_string()),
            ]
        );
    }

    #[test]
    fn rebuild_releases_store_locks_before_taking_edge_index_write() {
        // Regression for the rebuild/reindex/blame deadlock: rebuild must not
        // hold idx.read()/kb.read() while acquiring edge_index.write(). We hold
        // edge_index.read() to force the rebuild to park on edge_index.write(),
        // then prove idx and kb are still acquirable during that wait. Pre-fix
        // the rebuild held idx.read across the write acquisition, so idx.write()
        // would never succeed here (the deadlock window).
        use std::sync::Arc;
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(&tmp.path().join("bro")));

        // Hold a reader on the combined view so the rebuild's final write blocks.
        let held = state.code_read_view.read();

        let st = state.clone();
        let handle = std::thread::spawn(move || {
            rebuild_edge_index_from_shared(&st, false).unwrap();
        });

        // Let the rebuild acquire its store read-locks, finish computing
        // (trivial on an empty test corpus), and PARK on edge_index.write()
        // (blocked because `held` is alive). It cannot return until we drop
        // `held`, so after this settle it is definitively waiting on the write.
        // No early break — we must observe the steady state, not the
        // pre-acquisition race (an early break is what made the first cut of
        // this test pass against the buggy code).
        std::thread::sleep(Duration::from_millis(400));

        // The rebuild must still be parked (it can't complete until we release).
        assert!(
            !handle.is_finished(),
            "precondition: rebuild should be blocked on edge_index.write()"
        );
        // Fixed code dropped the store read-guards before acquiring the write,
        // so idx/kb are free now. Buggy code holds idx.read()/kb.read() while
        // parked here, so these would be None.
        assert!(
            state.idx.try_write().is_some(),
            "idx.write() must be free while rebuild waits on edge_index.write()"
        );
        assert!(
            state.kb.try_write().is_some(),
            "kb.write() must be free while rebuild waits on edge_index.write()"
        );

        // Let the rebuild finish.
        drop(held);
        handle.join().unwrap();
    }

    /// Regression for the 2026-08-25 cage index-plane deadlock:
    /// `bbox_hybrid_search` / `bbox_discover_seed_entities` hold
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

    fn signature_test_edge(kind: &str) -> edge_index::Edge {
        edge_index::Edge {
            source: entity_ref::EntityRef::Knowledge {
                id: "source".into(),
            },
            kind: kind.into(),
            target: entity_ref::EntityRef::Knowledge {
                id: "target".into(),
            },
            provenance: chunker::EdgeProvenance::Derived,
            confidence: chunker::EdgeConfidence::Exact,
            metadata: Default::default(),
            project_id: None,
        }
    }

    #[test]
    fn edge_sidecar_signature_ignores_inactive_snapshots_and_write_tmp_dirs() {
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
        let base = edge_sidecar_signature(edges_dir).unwrap();

        bbox_edge_sidecar::snapshot::write_snapshot_files(
            edges_dir,
            "p",
            "head-inactive",
            &[("project.jsonl", &[signature_test_edge("INACTIVE")])],
        )
        .unwrap();
        assert_eq!(
            base,
            edge_sidecar_signature(edges_dir).unwrap(),
            "inactive snapshot bytes are not rebuild inputs"
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
            edge_sidecar_signature(edges_dir).unwrap(),
            "*.write-tmp jsonl must not affect the signature"
        );
    }

    #[test]
    fn edge_sidecar_signature_tracks_manifest_index_active_pointers() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        // Baseline: no manifest-index present.
        let sig0 = edge_sidecar_signature(edges_dir).unwrap();
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
        let sig1 = edge_sidecar_signature(edges_dir).unwrap();
        assert_ne!(sig0, sig1);

        // A different active-pointer set — e.g. a branch switch flipping
        // active_snapshot between two already-materialized snapshots — changes
        // no `.jsonl` mtime, so only the manifest-index fold catches it.
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
        let sig2 = edge_sidecar_signature(edges_dir).unwrap();
        assert_ne!(
            sig1, sig2,
            "active-pointer change must change the signature even with no .jsonl change"
        );
    }

    #[test]
    fn edge_sidecar_signature_ignores_manifest_timestamp_only_rewrites() {
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
        let base = edge_sidecar_signature(edges_dir).unwrap();

        let mut index = bbox_edge_sidecar::manifest::ManifestIndex::load(edges_dir).unwrap();
        index.updated_at = Some("timestamp-only-rewrite".into());
        index.write_atomic(edges_dir).unwrap();

        assert_eq!(
            base,
            edge_sidecar_signature(edges_dir).unwrap(),
            "volatile manifest timestamps are not graph inputs"
        );
    }

    #[test]
    fn edge_rebuild_refuses_oversized_active_input_before_parsing() {
        let mut env = crate::util::TestEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = SharedState::for_test(&root.join("bro"));
        let edges_dir = edge_sidecar_dir(&state);
        std::fs::create_dir_all(&edges_dir).unwrap();
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
        env.set("BLACKBOX_EDGE_INDEX_REBUILD_MAX_INPUT_BYTES", "1");
        let error = rebuild_edge_index_from_shared(&state, false).unwrap_err();
        assert!(
            error.to_string().contains("active sidecar input"),
            "unexpected refusal: {error:#}"
        );
    }

    #[test]
    fn store_only_nudge_does_not_rebuild_a_large_unchanged_graph() {
        assert!(!should_rebuild_edge_index(true, false, 250_001, 250_000));
        assert!(should_rebuild_edge_index(true, false, 250_000, 250_000));
        assert!(
            should_rebuild_edge_index(false, true, usize::MAX, 0),
            "authority changes still require a rebuild regardless of current graph size"
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
