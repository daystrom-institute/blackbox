use anyhow::Context;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::artifacts;
use crate::config;
use crate::index;
use crate::mcp_tools;
use crate::orchestration;
use crate::projects::{
    ProjectEjectParams, ProjectInitParams, ProjectListResponse, ProjectRegisterParams,
    ProjectRenameParams, ProjectUnregisterParams,
};
use crate::server::routes::{
    migrate_project_refs, project_ref_counts, trigger_project_bootstrap_arc,
};
use crate::server::state::BlackboxServer;
use crate::tools::project_catalog::CatalogPathAuthority;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::{Value, json};

use bbox_indexing::checkout_access::{CheckoutAccessIntent, CheckoutAccessKind};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::projects_tools()
}

#[derive(Debug, Clone)]
struct ProjectInitResult {
    canonical: String,
    created: Vec<String>,
    skipped: Vec<String>,
    repo_id: Option<String>,
    repo_id_recorded: bool,
}

#[derive(Debug)]
struct PreparedProjectArtifact {
    kind: artifacts::ArtifactKind,
    source: String,
    local: bool,
    value: Value,
}

/// True when a lease acquisition failed only because the resolved
/// attachment does not record the requested capability (phase-2 §9.1
/// enrichment degradation; only the catalog authority can produce it).
fn capability_denied(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<bbox_indexing::checkout_access::CheckoutAccessError>()
        .is_some_and(|error| {
            error.code == bbox_indexing::checkout_access::CheckoutAccessErrorCode::CapabilityDenied
        })
}

/// Render one mutation-lease refusal in the section 9 vocabulary for the
/// init/eject/mutation row: `error.project_attachment_required` when no
/// attachment can receive the write, `error.project_capability_denied` when
/// one resolved but does not record `repo_mutation`.
///
/// Every other lease code passes through with its own stable prefix rather
/// than being flattened into one of those two, which would report a
/// lifecycle conflict or an unsafe root as a missing capability.
fn mutation_refusal(
    project_id: &str,
    refusal: crate::server::checkout_access::MutationLeaseRefusal,
) -> anyhow::Error {
    use crate::server::checkout_access::MutationLeaseRefusal;
    use bbox_indexing::checkout_access::CheckoutAccessErrorCode as Code;

    match refusal {
        MutationLeaseRefusal::Selection(error) => error,
        MutationLeaseRefusal::Lease(error) => match error.code {
            Code::AttachmentNotFound | Code::AttachmentInactive => anyhow::anyhow!(
                "error.project_attachment_required: project {project_id} has no active \
                 attachment able to receive repository writes"
            ),
            Code::CapabilityDenied => anyhow::anyhow!(
                "error.project_capability_denied: project {project_id} resolves an attachment \
                 that does not record repo_mutation"
            ),
            code => anyhow::anyhow!(
                "error.checkout_access.{}: {}",
                code.as_str(),
                error.diagnostic
            ),
        },
    }
}

// The registration path invokes this helper only from its surrounding
// `spawn_blocking` phase. Keeping the filesystem read here preserves the
// prepare-then-publish boundary while making the blocking-pool contract explicit.
#[allow(clippy::disallowed_methods)]
fn prepare_project_artifacts(project_root: &Path) -> anyhow::Result<Vec<PreparedProjectArtifact>> {
    let mut prepared = Vec::new();
    for artifact in artifacts::discover_project_artifacts(project_root)? {
        let raw = match fs::read_to_string(&artifact.path) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(
                    path = %artifact.path,
                    error = %error,
                    "artifact registration preparation skipped unreadable artifact"
                );
                continue;
            }
        };
        let value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    path = %artifact.path,
                    error = %error,
                    "artifact registration preparation skipped invalid artifact"
                );
                continue;
            }
        };
        prepared.push(PreparedProjectArtifact {
            kind: artifact.kind,
            source: artifact.path,
            local: artifact.local,
            value,
        });
    }
    Ok(prepared)
}

fn publish_project_artifacts(
    prepared: Vec<PreparedProjectArtifact>,
    project_id: &str,
    catalog: &artifacts::ArtifactCatalog,
) -> Vec<artifacts::ArtifactMetadata> {
    let mut installed = Vec::new();
    for artifact in prepared {
        let scope = artifacts::ArtifactScope::Project {
            project_id,
            local: artifact.local,
        };
        match catalog.install_value_scoped(
            scope,
            artifact.kind,
            artifact.source.clone(),
            &artifact.value,
            None,
            None,
            None,
        ) {
            Ok(metadata) => installed.push(metadata),
            Err(error) => tracing::warn!(
                path = %artifact.source,
                error = %error,
                "artifact registration publication failed"
            ),
        }
    }
    installed
}

// migration debt: project-init scaffolding writes inline; run_blocking conversion tracked in thread-935b467d.
#[allow(clippy::disallowed_methods)]
fn write_or_skip_file(
    path: &Path,
    contents: &str,
    force: bool,
    created: &mut Vec<String>,
    skipped: &mut Vec<String>,
) -> anyhow::Result<()> {
    let path_display = path.to_string_lossy().to_string();
    if path.exists() && !force {
        skipped.push(path_display);
        return Ok(());
    }
    fs::create_dir_all(
        path.parent()
            .context("target path has no parent for initialization")?,
    )?;
    fs::write(path, contents)?;
    if path.exists() {
        created.push(path_display);
    }
    Ok(())
}

// migration debt: project-init scaffolding writes inline; run_blocking conversion tracked in thread-935b467d.
#[allow(clippy::disallowed_methods)]
fn write_or_skip_mcp(
    path: &Path,
    force: bool,
    created: &mut Vec<String>,
    skipped: &mut Vec<String>,
) -> anyhow::Result<()> {
    let path_display = path.to_string_lossy().to_string();
    if path.exists() && !force {
        skipped.push(path_display);
        return Ok(());
    }
    fs::create_dir_all(
        path.parent()
            .context("target path has no parent for initialization")?,
    )?;
    orchestration::mcp::McpStore::new().save(path)?;
    created.push(path_display);
    Ok(())
}

/// Canonicalize the init target so it can be matched against catalog
/// attachments before any scaffolding is written.
// The init handler body runs entirely inside `run_blocking`, so this read is
// already on the blocking pool, the same sanction the scaffolding writes below
// carry.
#[allow(clippy::disallowed_methods)]
fn canonical_init_target(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

// migration debt: project-init scaffolding writes inline; run_blocking conversion tracked in thread-935b467d.
#[allow(clippy::disallowed_methods)]
fn init_project_path(project_dir: &Path, force: bool) -> anyhow::Result<ProjectInitResult> {
    let project_dir = project_dir
        .canonicalize()
        .context("canonicalizing project path for initialization")?;
    if !project_dir.is_dir() {
        anyhow::bail!(
            "project path must be an existing directory: {}",
            project_dir.display()
        );
    }

    let mut created = Vec::new();
    let mut skipped = Vec::new();
    let bbox_dir = project_dir.join(".bbox");
    fs::create_dir_all(&bbox_dir)?;

    let dirs = [
        bbox_dir.join("brofiles"),
        bbox_dir.join("workflows"),
        bbox_dir.join("packets"),
        bbox_dir.join("teams"),
        bbox_dir.join("agents"),
        bbox_dir.join("local"),
        // Marks the project repo-owned for durable knowledge: project-scoped
        // bbox_learn/decide land here and travel with the checkout.
        bbox_dir.join("knowledge"),
        // Marks the project repo-owned for substrate gap notes: project-scoped
        // bbox_gap records land here (top-level) and travel with the checkout.
        // (The spool drop folder lives under `gaps/inbox/`, created on demand.)
        bbox_dir.join("gaps"),
    ];
    for dir in &dirs {
        let path = dir.as_path();
        if force || !path.exists() {
            fs::create_dir_all(path)?;
        }
    }

    let config_path = bbox_dir.join("config.toml");
    // Project config carries identity and operator-owned declarations. Even a
    // forced skeleton refresh must merge it, never replace it wholesale.
    write_or_skip_file(
        &config_path,
        "# Project-local blackbox configuration.\n[roadmap]\n[mcp]\n[artifacts]\n",
        false,
        &mut created,
        &mut skipped,
    )?;
    write_or_skip_mcp(
        &bbox_dir.join("mcp.json"),
        force,
        &mut created,
        &mut skipped,
    )?;
    write_or_skip_file(
        &bbox_dir.join("local").join(".gitignore"),
        "*\n!.gitignore\n",
        force,
        &mut created,
        &mut skipped,
    )?;

    let recorded = if bbox_corpus_core::git::git_root_for_path(&project_dir).is_some() {
        Some(config::ensure_recorded_repo_id(&project_dir)?)
    } else {
        None
    };

    Ok(ProjectInitResult {
        canonical: project_dir.to_string_lossy().into_owned(),
        created,
        skipped,
        repo_id: recorded.as_ref().map(|recorded| recorded.repo_id.clone()),
        repo_id_recorded: recorded.is_some_and(|recorded| recorded.newly_recorded),
    })
}

#[tool_router(router = projects_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_project_register",
        description = "Register a project directory and schedule background agentic-corpus indexing. The path must be an absolute directory path (file paths and missing paths are rejected). Re-registering the same canonical path is idempotent — returns the existing record without modifying registered_at. project_id is derived from canonicalized realpath and is per-machine; repo_id derives from first commit SHA with remote fallback. In catalog mode this is the find-or-create composite: the checkout attaches to the project owning its committed scope (or a new Published/LegacyLocal project is minted), config-declared aliases become pending nominations, and a scope disagreement returns the exact promotion or scope-migration handoff instead of a second project. Use bbox_project_list to inspect registered projects."
    )]
    pub(crate) async fn bbox_project_register(
        &self,
        Parameters(p): Parameters<ProjectRegisterParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        if !Path::new(&p.path).exists() {
            return Self::err_text(&format!(
                "Error: error.project_onboarding_remote: {} is not visible to this daemon. \
                 Onboard it through the checkout owner instead: add the project to the \
                 checkout-host collector config, run `bbox-code-collector --config <cfg> \
                 init {}` on that host for scaffolding, and the collector onboards it \
                 through the producer channel on its next cycle. See \
                 design/daemon-runtime/remote-project-onboarding.md",
                p.path, p.path
            ));
        }
        // Catalog arm (plan §9.1): the compatibility composite. Probing,
        // find-or-create, attach, and nomination ingestion run on the
        // blocking pool; the enrichment pipeline is shared with the bridge
        // arm behind the same capability leases.
        if let Some(store) = self.state.project_authority.catalog_store().cloned() {
            let server = self.clone();
            let result: anyhow::Result<String> = tokio::task::spawn_blocking(move || {
                let (record, catalog_summary) = server.register_catalog_arm(&store, &p.path)?;
                server.state.nudge_edge_index_rebuild();
                let mut response = server.run_post_register_pipeline(record)?;
                if let Some(map) = response.as_object_mut() {
                    map.insert("catalog".into(), catalog_summary);
                }
                Ok(serde_json::to_string_pretty(&response)?)
            })
            .await
            .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
            .and_then(std::convert::identity);
            return match result {
                Ok(text) => {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    tracing::info!(target: "blackbox::tool", tool = "bbox_project_register", elapsed_ms = ms, bytes = text.len(), "ok");
                    Self::ok_text(&text)
                }
                Err(e) => {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    tracing::warn!(target: "blackbox::tool", tool = "bbox_project_register", elapsed_ms = ms, error = %e, "err");
                    Self::err_text(&format!("Error: {e:#}"))
                }
            };
        }
        // Phase 1: register + alias materialization + persist — light lock
        // ops + async I/O on the runtime. Declared aliases come from the
        // repo's committed `.bbox/config.toml` and sync under the same write
        // lock so the persisted record carries them; a conflicting alias
        // claim fails the call (fail closed) while the registration itself
        // stands — fix the config and re-register to converge.
        let declared_aliases = config::load_project_at_ref(Path::new(&p.path), "HEAD")
            .map(|cfg| cfg.project.aliases.into_iter().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let lifecycle = match self.state.checkout_access.lifecycle_mutation_guard() {
            Ok(guard) => guard,
            Err(error) => {
                return Self::err_text(&format!("Error: {error}"));
            }
        };
        let res = match self.state.project_authority.bridge_registry() {
            Err(error) => Err(error),
            Ok(registry) => {
                let mut projects = registry.write();
                projects.register_path(&p.path).and_then(|record| {
                    projects.sync_declared_aliases(&record.project_id, &declared_aliases)?;
                    projects.resolve(&record.project_id)?.with_context(|| {
                        format!("project vanished mid-register: {}", record.project_id)
                    })
                })
            }
        };
        drop(lifecycle);
        let record = match res {
            Ok(record) => record,
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_project_register", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        };
        if let Err(e) = self.state.persist_projects_durable().await {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            tracing::warn!(target: "blackbox::tool", tool = "bbox_project_register", elapsed_ms = ms, error = %e, "err");
            return Self::err_text(&format!("Error: {e:#}"));
        }
        self.state.nudge_edge_index_rebuild();
        // Phase 2: heavy fs work (MCP migration, config load, artifact discovery,
        // provenance import, watcher, kb sync) on the blocking pool.
        let server = self.clone();
        let result: anyhow::Result<String> = tokio::task::spawn_blocking(move || {
            let response = server.run_post_register_pipeline(record)?;
            Ok(serde_json::to_string_pretty(&response)?)
        })
        .await
        .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
        .and_then(std::convert::identity);

        match result {
            Ok(text) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool = "bbox_project_register", elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_project_register", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    /// The post-register enrichment pipeline (plan §9.1): MCP migration,
    /// project config + artifact discovery, provenance import, watcher and
    /// kb registration, and the transcript-edge backfill, all behind the
    /// same capability leases in both authority modes. Blocking work: call
    /// from the blocking pool only.
    ///
    /// Capability semantics: a catalog attachment records what its checkout
    /// shape supports; a step whose capability is not recorded is skipped
    /// and reported, never a registration failure.
    fn run_post_register_pipeline(
        &self,
        record: crate::projects::ProjectRecord,
    ) -> anyhow::Result<serde_json::Value> {
        let server = self.clone();
        // Capability-gated enrichment (phase-2 §9.1): an attachment that
        // does not record a step's capability skips that step. The
        // version-1 authority records no capability bits and never denies,
        // so bridge-mode enrichment is unchanged; catalog attachments
        // enrich to exactly what their observed checkout shape supports.
        let mut skipped_enrichment: Vec<&'static str> = Vec::new();
        {
            match crate::server::checkout_access::acquire_selected_project_access(
                &server.state.checkout_access,
                &record.project_id,
                CheckoutAccessKind::RepositoryMutation,
                CheckoutAccessIntent::Write,
            ) {
                Ok(migration_lease) => {
                    let migration_publication = server
                        .state
                        .checkout_access
                        .publication_guard(&migration_lease)
                        .map_err(anyhow::Error::new)?;
                    orchestration::mcp::migrate_project_mcp_path(migration_lease.project_root())?;
                    drop(migration_publication);
                }
                Err(error) if capability_denied(&error) => {
                    skipped_enrichment.push("mcp_migration");
                }
                Err(error) => return Err(error),
            }
            let mut project_config_loaded = false;
            match crate::server::checkout_access::acquire_selected_project_access(
                &server.state.checkout_access,
                &record.project_id,
                CheckoutAccessKind::ArtifactWatchDiscovery,
                CheckoutAccessIntent::Read,
            ) {
                Ok(artifact_lease) => {
                    let project_config = config::load_project(artifact_lease.project_root())?;
                    project_config_loaded = true;
                    if project_config.mcp.enabled == Some(false) {
                        tracing::info!(
                            "Project MCP is disabled via {}",
                            artifact_lease
                                .project_root()
                                .join(".bbox/config.toml")
                                .display()
                        );
                    }
                    let prepared_artifacts =
                        if project_config.artifacts.auto_discover != Some(false) {
                            prepare_project_artifacts(artifact_lease.project_root())?
                        } else {
                            Vec::new()
                        };
                    let artifact_publication = server
                        .state
                        .checkout_access
                        .publication_guard(&artifact_lease)
                        .map_err(anyhow::Error::new)?;
                    let installed = publish_project_artifacts(
                        prepared_artifacts,
                        &record.project_id,
                        &server.state.artifacts.read(),
                    );
                    if !installed.is_empty() {
                        tracing::info!(
                            "Installed {} project artifact(s) for {}",
                            installed.len(),
                            record.project_id
                        );
                    }
                    drop(artifact_publication);
                }
                Err(error) if capability_denied(&error) => {
                    skipped_enrichment.push("artifact_discovery");
                }
                Err(error) => return Err(error),
            }
            let edges_dir = crate::server::edge_sidecar_dir(&server.state);
            let provenance_lease =
                match crate::server::checkout_access::acquire_selected_project_access(
                    &server.state.checkout_access,
                    &record.project_id,
                    CheckoutAccessKind::ProvenanceNoteIo,
                    CheckoutAccessIntent::Read,
                ) {
                    Ok(lease) => Some(lease),
                    Err(error) if capability_denied(&error) => {
                        skipped_enrichment.push("provenance_import");
                        None
                    }
                    Err(error) => return Err(error),
                };
            if let Some(provenance_lease) = provenance_lease {
                let provenance_project = mcp_tools::provenance::ProvenanceProject {
                    project_id: record.project_id.clone(),
                    project_root: provenance_lease.project_root().to_path_buf(),
                };
                let resolve_legacy_target =
                    |project_id: &str,
                     root: &Path,
                     absolute_path: &Path,
                     byte_range: Option<(u64, u64)>| {
                        if project_id != record.project_id {
                            anyhow::bail!(
                                "error.project_mismatch: provenance target belongs to another project"
                            );
                        }
                        bbox_indexing::index::resolve_current_project_chunk_entity(
                            &record.project_id,
                            root,
                            absolute_path,
                            byte_range,
                        )
                    };
                let prepared_provenance = mcp_tools::provenance::prepare_provenance_import(
                    std::slice::from_ref(&provenance_project),
                    &resolve_legacy_target,
                )?;
                let provenance_publication = server
                    .state
                    .checkout_access
                    .publication_guard(&provenance_lease)
                    .map_err(anyhow::Error::new)?;
                mcp_tools::provenance::publish_prepared_provenance_import(
                    prepared_provenance,
                    &edges_dir,
                )?;
                drop(provenance_publication);
            }
            // Register with the live .bbox/ watcher so future file changes
            // are picked up without a daemon restart.
            if let Ok(mut guard) = server.state.bbox_watcher.lock() {
                if let Some(w) = guard.as_mut() {
                    match crate::watcher::ArtifactWatchCarrier::selected(record.project_id.clone())
                    {
                        Ok(carrier) => {
                            if let Err(e) = w.watch_project(carrier) {
                                tracing::warn!("watcher add project {}: {e:#}", record.project_id);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "watcher rejected project {} carrier: {e:#}",
                                record.project_id
                            );
                        }
                    }
                }
            }
            // Load this project's committed `.bbox/knowledge/` into the query
            // surface (project-scoped durable knowledge is repo-owned) and
            // enqueue its embeds so a freshly-cloned repo is vector-searchable
            // without a manual reembed.
            crate::server::routes::sync_kb_project_roots(&server.state);
            crate::server::routes::enqueue_project_knowledge_embeds(
                &server.state,
                &record.canonical_path,
            );
            // P1 backfill: retroactively emit observed tool-call edges for the
            // newly registered project by walking all prior transcripts. Runs
            // in a background thread so the registration response is immediate.
            // Uses append_edges_dedup so re-running is safe.
            {
                let reindex_cfg = server.state.idx.read().reindex_config();
                let project_for_backfill = record.clone();
                let checkout_access = server.state.checkout_access.clone();
                // lint-concurrency: allow(thread-spawn) — one-shot registration backfill; relocation to an owner module tracked in thread-935b467d
                std::thread::spawn(move || {
                    let result = (|| {
                        let local = crate::server::checkout_access::acquire_selected_project_access(
                        &checkout_access,
                        &project_for_backfill.project_id,
                        bbox_indexing::checkout_access::CheckoutAccessKind::LocalProjectWalk,
                        bbox_indexing::checkout_access::CheckoutAccessIntent::Read,
                        )?;
                        let git = project_for_backfill
                            .is_git_repo
                            .then(|| {
                                crate::server::checkout_access::acquire_selected_project_access(
                                    &checkout_access,
                                    &project_for_backfill.project_id,
                                    bbox_indexing::checkout_access::CheckoutAccessKind::GitHistory,
                                    bbox_indexing::checkout_access::CheckoutAccessIntent::Read,
                                )
                            })
                            .transpose()?;
                        index::backfill_tool_edges_for_project(
                            &reindex_cfg,
                            &project_for_backfill.project_id,
                            local.project_root(),
                            git.as_ref().map(|lease| lease.checkout_root()),
                            || {
                                checkout_access
                                    .publication_guard_for(
                                        std::iter::once(&local).chain(git.iter()),
                                    )
                                    .map_err(anyhow::Error::new)
                            },
                        )
                    })();
                    match result {
                        Ok(written) => tracing::info!(
                            project_id = %project_for_backfill.project_id,
                            edges_written = written,
                            "P1 backfill complete"
                        ),
                        Err(err) => tracing::warn!(
                            project_id = %project_for_backfill.project_id,
                            error = %err,
                            "P1 backfill failed"
                        ),
                    }
                });
            }

            trigger_project_bootstrap_arc(server.state.clone(), record.clone());
            let response = json!({
                "record": record,
                "project_config_loaded": project_config_loaded,
                "skipped_enrichment": skipped_enrichment,
                "indexing": {
                    "status": "scheduled",
                    "mode": "background",
                    "detail": "project registration is durable; project-file indexing and edge projection are picked up by the background reindexer after this response, and embeddings across all routes (docs/code/notes/visual:*) then converge automatically via the background residue sweeper — no manual bbox_reembed is required"
                },
            });
            Ok(response)
        }
    }

    #[tool(
        name = "bbox_project_init",
        description = "Initialize a project-local .bbox workspace. Creates `.bbox/config.toml`, `.bbox/mcp.json`, `.bbox/local/.gitignore` and default subdirectories, and records the durable repo_id for Git projects. Idempotent by default; force=true refreshes replaceable skeleton files but always merge-preserves identity-bearing config.toml."
    )]
    pub(crate) async fn bbox_project_init(
        &self,
        Parameters(p): Parameters<ProjectInitParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_project_init", move || {
            let path = Path::new(&p.path);
            if !path.is_absolute() {
                anyhow::bail!("project path must be absolute: {}", p.path);
            }
            if !path.exists() {
                anyhow::bail!(
                    "error.project_onboarding_remote: {} is not visible to this daemon. \
                     Onboard it through the checkout owner instead: add the project to the \
                     checkout-host collector config, run `bbox-code-collector --config <cfg> \
                     init {}` on that host for scaffolding, and the collector onboards it \
                     through the producer channel on its next cycle. See \
                     design/daemon-runtime/remote-project-onboarding.md",
                    p.path,
                    p.path
                );
            }
            // Bootstrap exception, plan section 4.19: an UNREGISTERED absolute
            // path is initialized with no lease, because attach needs the
            // identity-bearing config this very call creates and requiring an
            // attachment first would be circular. It is an exception, not the
            // rule: once a selector resolves the path to a catalog project,
            // the same scaffolding writes are a catalog-targeted mutation and
            // take RepositoryMutation under the publication guard.
            let authority = match server.state.project_authority.catalog_store() {
                None => CatalogPathAuthority::Absent,
                Some(store) => match canonical_init_target(path) {
                    Some(canonical) => {
                        server.catalog_path_authority(store, &canonical.to_string_lossy())
                    }
                    // The path exists but will not canonicalize, so it cannot
                    // be compared against catalog authority at all. Unproved
                    // is not unregistered.
                    None => CatalogPathAuthority::Unreadable {
                        code: "error.project_catalog_admin_path".into(),
                        diagnostic: format!("{} could not be canonicalized", path.display()),
                    },
                },
            };
            let result = match authority {
                // Bootstrap: no attachment row at any status knows this path.
                CatalogPathAuthority::Absent => init_project_path(path, p.force)?,
                CatalogPathAuthority::Attached(project_id) => {
                    let lease = crate::server::checkout_access::acquire_project_mutation_lease(
                        &server,
                        &project_id,
                    )
                    .map_err(|refusal| mutation_refusal(&project_id, refusal))?;
                    let publication = server
                        .state
                        .checkout_access
                        .publication_guard(&lease)
                        .map_err(anyhow::Error::new)?;
                    let result = init_project_path(lease.project_root(), p.force);
                    drop(publication);
                    let result = result?;
                    server
                        .state
                        .checkout_access
                        .revalidate(&lease)
                        .map_err(anyhow::Error::new)?;
                    result
                }
                CatalogPathAuthority::Governed { diagnostic } => anyhow::bail!(
                    "error.project_attachment_required: {diagnostic}; scaffolding a governed \
                     checkout requires an active attachment recording repo_mutation"
                ),
                CatalogPathAuthority::Unreadable { code, diagnostic } => anyhow::bail!(
                    "{code}: catalog authority is unreadable, so this path cannot be proved \
                     unregistered ({diagnostic})"
                ),
            };
            // Catalog arm (plan §9.1): init stays a filesystem initializer;
            // newly recorded authority inside a checkout attached to a
            // legacy-local project reports promotion as the next action.
            let next_action = server
                .state
                .project_authority
                .catalog_store()
                .and_then(|store| {
                    server.init_catalog_next_action(
                        store,
                        &result.canonical,
                        result.repo_id_recorded,
                    )
                });
            Ok(serde_json::to_string_pretty(&json!({
                "project": result.canonical,
                "created": result.created,
                "skipped": result.skipped,
                "repo_id": result.repo_id,
                "repo_id_recorded": result.repo_id_recorded,
                "next_action": next_action,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_rename",
        description = "Rename a registered bbox project root while preserving its project_id and migrating project-scoped bbox state. Accepts project (project_id, registered canonical_path, or absolute path), new_path (absolute directory path), optional move_on_disk (default false), and optional dry_run. Updates project registry, knowledge, threads, notes, pins, packets, Slack channel bindings, live teams, whiteboards, pollers, and crons, then reindexes project files. In catalog mode rename is attachment relocation: the moved checkout must carry the same checkout-id marker and resolve the same scope, the ledger records the historical path, owner-store rows are never rewritten, and move_on_disk is refused (move first, then rename)."
    )]
    pub(crate) async fn bbox_project_rename(
        &self,
        Parameters(p): Parameters<ProjectRenameParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        // Catalog arm (plan §9.1): rename is attachment relocation with a
        // ledger append; owner-store rows are never rewritten.
        if let Some(store) = self.state.project_authority.catalog_store().cloned() {
            let server = self.clone();
            let result: anyhow::Result<String> =
                tokio::task::spawn_blocking(move || server.rename_catalog_arm(&store, &p))
                    .await
                    .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
                    .and_then(std::convert::identity);
            return match result {
                Ok(text) => {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    tracing::info!(target: "blackbox::tool", tool = "bbox_project_rename", elapsed_ms = ms, bytes = text.len(), "ok");
                    Self::ok_text(&text)
                }
                Err(e) => {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    tracing::warn!(target: "blackbox::tool", tool = "bbox_project_rename", elapsed_ms = ms, error = %e, "err");
                    Self::err_text(&format!("Error: {e:#}"))
                }
            };
        }
        // Phase 1: rename in registry + async persist.
        let lifecycle = match self.state.checkout_access.lifecycle_mutation_guard() {
            Ok(guard) => guard,
            Err(error) => {
                return Self::err_text(&format!("Error: {error}"));
            }
        };
        let res = match self.state.project_authority.bridge_registry() {
            Err(error) => Err(error),
            Ok(registry) => {
                let mut projects = registry.write();
                projects.rename_project(&p)
            }
        };
        drop(lifecycle);
        let response = match res {
            Ok(response) => response,
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_project_rename", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        };
        if !response.dry_run {
            if let Err(e) = self.state.persist_projects_durable().await {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_project_rename", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        }
        // Phase 2: heavy fs migration + reindex on the blocking pool.
        let server = self.clone();
        let result: anyhow::Result<String> = tokio::task::spawn_blocking(move || {
            let old_project = response.old_record.canonical_path.clone();
            let new_project = response.record.canonical_path.clone();

            let counts = if response.dry_run {
                project_ref_counts(&server.state, &old_project)?
            } else {
                let counts = migrate_project_refs(
                    &server.state,
                    &old_project,
                    &new_project,
                    &response.record,
                )?;
                // Re-point logical carriers without reloading because the
                // in-memory mutations from migrate_rename are still live.
                let inputs = crate::server::repo_io::CatalogBaseTargets::read_consistent_for_state(
                    &server.state,
                )?;
                let carriers = crate::server::repo_io::RepoIoAuthority::knowledge_base_carriers(
                    &inputs.records,
                    inputs.targets.as_ref(),
                )?;
                server.state.kb.write().update_project_carriers(carriers);
                if let Ok(mut guard) = server.state.bbox_watcher.lock()
                    && let Some(watcher) = guard.as_mut()
                    && let Ok(carrier) = crate::watcher::ArtifactWatchCarrier::selected(
                        response.record.project_id.clone(),
                    )
                {
                    if let Err(error) = watcher.unwatch_carrier(&carrier) {
                        tracing::warn!(
                            project = %response.record.project_id,
                            error = %error,
                            "project rename could not remove the prior watcher registration"
                        );
                    }
                    if let Err(error) = watcher.watch_project(carrier) {
                        tracing::warn!(
                            project = %response.record.project_id,
                            error = %error,
                            "project rename could not register the replacement watcher"
                        );
                    }
                }
                crate::server::routes::enqueue_project_knowledge_embeds(
                    &server.state,
                    &new_project,
                );
                counts
            };

            let reindex = if response.dry_run {
                None
            } else {
                let result = server.state.index_writer.run_reindex_pass(false, true)?;
                server.state.nudge_edge_index_rebuild();
                Some(result)
            };

            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "old_record": response.old_record,
                "record": response.record,
                "moved_on_disk": response.moved_on_disk,
                "dry_run": response.dry_run,
                "migrated_refs": counts,
                "reindex": reindex,
            }))?)
        })
        .await
        .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
        .and_then(std::convert::identity);

        match result {
            Ok(text) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool = "bbox_project_rename", elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_project_rename", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_project_unregister",
        description = "Unregister a project root from the bbox project registry. Accepts project (project_id, registered canonical_path, or absolute path). Removes the registry entry only; does NOT delete project-scoped state (knowledge, threads, notes, pins, packets, Slack bindings, teams, whiteboards, pollers, crons) keyed on the project_id, which is derived from the canonical realpath and is stable across unregister+re-register. By default refuses when refs still exist and returns the counts; pass force=true to orphan them, or bbox_project_rename to migrate first. dry_run=true previews counts without mutating the registry. In catalog mode unregister is detach: the attachment is marked detached with census deregistration scoped to its checkout and scope pair, every logical store keeps its rows, and catalog deletion is the offline project-catalog retire surface."
    )]
    pub(crate) async fn bbox_project_unregister(
        &self,
        Parameters(p): Parameters<ProjectUnregisterParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        // Catalog arm (plan §9.1): unregister is detach; logical state
        // stays and catalog deletion is the offline retire surface.
        if let Some(store) = self.state.project_authority.catalog_store().cloned() {
            let server = self.clone();
            let result: anyhow::Result<String> =
                tokio::task::spawn_blocking(move || server.unregister_catalog_arm(&store, &p))
                    .await
                    .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
                    .and_then(std::convert::identity);
            return match result {
                Ok(text) => {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    tracing::info!(target: "blackbox::tool", tool = "bbox_project_unregister", elapsed_ms = ms, bytes = text.len(), "ok");
                    Self::ok_text(&text)
                }
                Err(e) => {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    tracing::warn!(target: "blackbox::tool", tool = "bbox_project_unregister", elapsed_ms = ms, error = %e, "err");
                    Self::err_text(&format!("Error: {e:#}"))
                }
            };
        }
        let result: anyhow::Result<String> = async {
            let force = p.force.unwrap_or(false);
            let dry_run = p.dry_run.unwrap_or(false);

            let record = self
                .state
                .project_authority.bridge_registry()?.read().resolve(&p.project)?
                .with_context(|| format!("project not registered: {}", p.project))?;

            let counts = project_ref_counts(&self.state, &record.canonical_path)?;
            let total_refs: u64 = counts
                .as_object()
                .map(|m| m.values().filter_map(|v| v.as_u64()).sum())
                .unwrap_or(0);

            if dry_run {
                return Ok(serde_json::to_string_pretty(&json!({
                    "status": "dry_run",
                    "record": record,
                    "ref_counts": counts,
                    "total_refs": total_refs,
                    "would_remove": true,
                    "force_required": total_refs > 0,
                }))?);
            }

            if total_refs > 0 && !force {
                anyhow::bail!(
                    "project {} still has {} project-scoped refs across {}; re-run with force=true to orphan them, or use bbox_project_rename to migrate first. counts: {}",
                    record.project_id,
                    total_refs,
                    counts
                        .as_object()
                        .map(|m| {
                            m.iter()
                                .filter(|(_, v)| v.as_u64().unwrap_or(0) > 0)
                                .map(|(k, _)| k.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                    counts,
                );
            }

            let unregister_state = self.state.clone();
            let unregister_project = p.project.clone();
            let removed = tokio::task::spawn_blocking(move || {
                let _lifecycle = unregister_state
                    .checkout_access
                    .lifecycle_mutation_guard()
                    .map_err(anyhow::Error::new)?;
                let registry = unregister_state.project_authority.bridge_registry()?;
                let mut projects = registry.write();
                projects.unregister_project(&unregister_project)
            })
            .await
            .map_err(|error| anyhow::anyhow!("project unregister task failed: {error}"))??;
            self.state.persist_projects_durable().await?;

            let checkout_rows = self.state.checkout_registry.read().rows().to_vec();
            if let Ok(mut guard) = self.state.bbox_watcher.lock()
                && let Some(watcher) = guard.as_mut()
            {
                if let Ok(carrier) =
                    crate::watcher::ArtifactWatchCarrier::selected(record.project_id.clone())
                    && let Err(error) = watcher.unwatch_carrier(&carrier)
                {
                    tracing::warn!(
                        project = %record.project_id,
                        error = %error,
                        "project unregister could not remove its watcher registration"
                    );
                }
                for row in &checkout_rows {
                    if row.project_id.as_deref() != Some(record.project_id.as_str()) {
                        continue;
                    }
                    if let Ok(carrier) = crate::watcher::ArtifactWatchCarrier::checkout(
                        record.project_id.clone(),
                        row.checkout_id.clone(),
                    ) && let Err(error) = watcher.unwatch_carrier(&carrier)
                    {
                        tracing::warn!(
                            project = %record.project_id,
                            checkout_id = %row.checkout_id,
                            error = %error,
                            "project unregister could not remove a checkout watcher registration"
                        );
                    }
                }
            }

            // Drop the unregistered project's repo from kb roots; its committed
            // `.bbox/knowledge/` stays on disk and reloads on re-register.
            crate::server::routes::sync_kb_project_roots(&self.state);

            // Nudge the watcher to rebuild EdgeIndex so edges keyed on the
            // removed project stop surfacing. Async handler — the rebuild's
            // multi-GB sidecar parse must not run inline on a tokio worker;
            // stale edges for a few seconds after unregister are acceptable.
            self.state.nudge_edge_index_rebuild();

            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "record": removed,
                "ref_counts": counts,
                "orphaned_refs": total_refs,
                "forced": total_refs > 0 && force,
            }))?)
        }
        .await;
        match result {
            Ok(text) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool = "bbox_project_unregister", elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_project_unregister", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_project_eject",
        description = "Migrate a registered project's central-store knowledge entries into the repo's committed .bbox/knowledge/ (one file per entry), so the project's durable knowledge travels with the checkout. Accepts project (project_id, registered canonical_path, or absolute path) and optional dry_run. Entries are written without the absolute project path (location encodes scope), dropped from the central store, and a clean schema-epoch marker is written by this explicit operator action. dry_run=true reports the count without writing. Commit the resulting .bbox/ files to publish them."
    )]
    pub(crate) async fn bbox_project_eject(
        &self,
        Parameters(p): Parameters<ProjectEjectParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let server = self.clone();
        // Phase 1: fs + store work on the blocking pool.
        let fs_result = tokio::task::spawn_blocking(move || {
            // Selection-class engine resolution (phase-2 §9.1): eject gains
            // no catalog semantics beyond resolving through the shared
            // resolver; the projection row supplies the record shape in
            // both modes.
            let resolved_id = server.validate_project_selection(&p.project)?;
            // Records and their exact base-attachment targets from ONE
            // catalog epoch: the lease this operation takes must name the
            // same attachment the record and its carriers were derived from.
            let inputs =
                crate::server::repo_io::CatalogBaseTargets::read_consistent_for_state(&server.state)?;
            let record = inputs
                .records
                .iter()
                .find(|record| record.project_id == resolved_id)
                .cloned();
            // A remote-only catalog project resolves but has no attachment to
            // write into. Eject writes repository files, so the honest refusal
            // is attachment-required, not "not registered": the project exists
            // and its published knowledge still serves.
            let record = match record {
                Some(record) => record,
                None if server.state.project_authority.catalog_store().is_some() => {
                    anyhow::bail!(
                        "error.project_attachment_required: eject writes repository files and \
                         project {resolved_id} has no active attachment"
                    )
                }
                None => anyhow::bail!("project not registered: {}", p.project),
            };
            let dry_run = p.dry_run.unwrap_or(false);

            // Ensure the project's repo is in kb roots so already-ejected files
            // are accounted for and the post-eject reload loads from the repo.
            crate::server::routes::sync_kb_project_roots(&server.state);

            if dry_run {
                // A preview writes nothing, so the record's display path is
                // the right key to count against and no lease is needed.
                let preview_dir = record.canonical_path.clone();
                let entries = server.state.kb.read().count_project_entries(&preview_dir);
                return Ok::<_, anyhow::Error>((record, preview_dir, true, entries, None, false));
            }

            // One RepositoryMutation lease covers every durable write this
            // eject performs, and one publication guard fences those writes
            // together with the central-store flush (plan section 8, P5-F
            // mutation items 2 and 3). Flushing here rather than awaiting the
            // persister after the blocking phase is what puts the central
            // store inside the same fence.
            //
            // In catalog mode the lease names the EXACT base attachment the
            // destination came from. Going through the ladder here was
            // checkpoint 2 finding 3: on a base-plus-worktree project a
            // session or operator default selects the worktree, so the guard
            // pinned the worktree while the ejection wrote the base.
            let lease = match inputs.targets.as_ref() {
                Some(targets) => {
                    let (attachment_id, expected_scope) = targets
                        .base_attachment(&record.project_id)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "error.project_attachment_required: catalog project {} has no \
                                 unambiguous active base attachment to eject into",
                                record.project_id
                            )
                        })?;
                    server
                        .state
                        .checkout_access
                        .acquire(bbox_indexing::checkout_access::CheckoutAccessRequest {
                            project_id: record.project_id.clone(),
                            attachment:
                                bbox_indexing::checkout_access::CheckoutAttachmentSelector::AttachmentId(
                                    attachment_id,
                                ),
                            expected_scope,
                            kind: CheckoutAccessKind::RepositoryMutation,
                            intent: CheckoutAccessIntent::Write,
                            source_lane:
                                bbox_indexing::checkout_access::CheckoutAccessSourceLane::NativeAttachment,
                        })
                        .map_err(|error| {
                            mutation_refusal(
                                &record.project_id,
                                crate::server::checkout_access::MutationLeaseRefusal::Lease(error),
                            )
                        })?
                }
                None => crate::server::checkout_access::acquire_project_mutation_lease(
                    &server,
                    &record.project_id,
                )
                .map_err(|refusal| mutation_refusal(&record.project_id, refusal))?,
            };
            let publication = server
                .state
                .checkout_access
                .publication_guard(&lease)
                .map_err(anyhow::Error::new)?;
            // Every repository destination is derived from the acquired
            // lease, so the guard and the writes cannot name different
            // checkouts. A destination that disagrees with the carrier the
            // record was built from has no carrier and fails closed below.
            let dir = lease.project_root().to_string_lossy().into_owned();
            let recorded_repo_id = record
                .is_git_repo
                .then(|| config::ensure_recorded_repo_id(lease.project_root()))
                .transpose()?;
            let entries = server.state.kb.write().eject_project_to_repo(&dir)?;
            let report = server.write_knowledge_schema_epoch_marker(Path::new(&dir))?;
            server.state.kb_persister.flush_blocking()?;
            drop(publication);
            server
                .state
                .checkout_access
                .revalidate(&lease)
                .map_err(anyhow::Error::new)?;

            Ok::<_, anyhow::Error>((
                record,
                dir,
                dry_run,
                entries,
                recorded_repo_id,
                !report.marked_scopes.is_empty(),
            ))
        })
        .await
        .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
        .and_then(std::convert::identity);

        match fs_result {
            Ok((record, dir, dry_run, entries, recorded_repo_id, schema_epoch_marker_written)) => {
                // The central store was already flushed under the publication
                // guard above, so there is no durability ack left to await.
                match serde_json::to_string_pretty(&json!({
                    "status": if dry_run { "dry_run" } else { "ok" },
                    "project_id": record.project_id,
                    "canonical_path": dir,
                    "entries": entries,
                    "repo_id": recorded_repo_id.as_ref().map(|recorded| &recorded.repo_id),
                    "repo_id_recorded": recorded_repo_id.as_ref().is_some_and(|recorded| recorded.newly_recorded),
                    "schema_epoch_marker_written": schema_epoch_marker_written,
                    "target": format!("{}/.bbox/knowledge", dir),
                    "detail": if dry_run {
                        "preview only; re-run without dry_run to write repo files and drop central copies"
                    } else {
                        "written to repo .bbox/knowledge/ and removed from central store; commit the files to publish"
                    },
                })) {
                    Ok(text) => {
                        let ms = start.elapsed().as_secs_f64() * 1000.0;
                        tracing::info!(target: "blackbox::tool", tool = "bbox_project_eject", elapsed_ms = ms, bytes = text.len(), "ok");
                        Self::ok_text(&text)
                    }
                    Err(e) => {
                        let err = anyhow::Error::new(e);
                        let ms = start.elapsed().as_secs_f64() * 1000.0;
                        tracing::warn!(target: "blackbox::tool", tool = "bbox_project_eject", elapsed_ms = ms, error = %err, "err");
                        Self::err_text(&format!("Error: {err:#}"))
                    }
                }
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_project_eject", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_project_list",
        description = "List registered project roots with their project_id, repo_id (null for non-git), canonical_path, registered_at, and is_git_repo flag. Idempotent read; safe to call repeatedly. project_ids are stable across daemon restarts. Use this before bbox_project_register to check whether a path is already registered."
    )]
    pub(crate) fn bbox_project_list(&self) -> CallToolResult {
        Self::ok_json(
            &serde_json::to_value(ProjectListResponse {
                projects: self
                    .state
                    .records_provider
                    .records_snapshot()
                    .records
                    .as_ref()
                    .clone(),
            })
            .unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::projects::ProjectRecord;
    use crate::server::state::{BlackboxServer, SharedState};
    use crate::{entity_ref, knowledge, notes, pins, threads};
    use rmcp::handler::server::wrapper::Parameters;
    use serde_json::Value;
    use tempfile::tempdir;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
    }

    #[tokio::test]
    async fn register_loads_and_counts_committed_project_knowledge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".bbox").join("knowledge")).unwrap();
        let repo = repo.canonicalize().unwrap();

        // A committed project entry as it lands in git (project omitted on disk).
        let entry = knowledge::KnowledgeEntry {
            id: "r0000001".into(),
            title: "repo rule".into(),
            content: "committed convention".into(),
            cluster: None,
            variants: Default::default(),
            category: knowledge::Category::Convention,
            scope: knowledge::Scope::Project,
            project: None,
            project_id: None,
            providers: vec![],
            priority: knowledge::Priority::Standard,
            weight: 100,
            status: knowledge::Status::Active,
            approval: knowledge::Approval::UserConfirmed,
            render: true,
            decay: true,
            review_at: None,
            supersedes: None,
            links: vec![],
            rationale: None,
            expires_at: None,
            source: "user".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        };
        std::fs::write(
            repo.join(".bbox").join("knowledge").join("r0000001.json"),
            serde_json::to_string(&entry).unwrap(),
        )
        .unwrap();

        let server = test_server(&tmp);
        let reg = server
            .bbox_project_register(Parameters(ProjectRegisterParams {
                path: repo.to_string_lossy().into_owned(),
            }))
            .await;
        assert_ne!(reg.is_error, Some(true));

        // Register loaded the committed entry into the query surface...
        assert!(
            server.state.kb.read().entry("r0000001").is_some(),
            "committed .bbox/knowledge entry should load on register"
        );
        // ...and it is counted for embed enqueue (vector coverage without reembed).
        let enqueued = crate::server::routes::enqueue_project_knowledge_embeds(
            &server.state,
            &repo.to_string_lossy(),
        );
        assert_eq!(enqueued, 1);
    }

    #[test]
    fn project_init_creates_bbox_skeleton() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path().join("project");
        std::fs::create_dir_all(&dir_path).unwrap();
        // macOS tempdir lives under /var (symlink to /private/var); init_project_path
        // canonicalizes, so derive expected paths from the canonical root too.
        let dir_path = dir_path.canonicalize().unwrap();
        let result = init_project_path(&dir_path, false).unwrap();
        let cfg_path = dir_path.join(".bbox").join("config.toml");
        let mcp_path = dir_path.join(".bbox").join("mcp.json");
        let gitignore_path = dir_path.join(".bbox").join("local").join(".gitignore");
        assert!(cfg_path.exists());
        assert!(mcp_path.exists());
        assert!(gitignore_path.exists());
        assert!(
            result
                .created
                .contains(&cfg_path.to_string_lossy().into_owned())
        );
        let store = orchestration::mcp::McpStore::load(&mcp_path).unwrap();
        assert_eq!(store.version, 1);
        assert_eq!(
            result.canonical,
            dir_path.canonicalize().unwrap().to_string_lossy()
        );
        assert_eq!(result.skipped.len(), 0);
    }

    #[test]
    fn project_init_is_idempotent_without_force() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path().join("project");
        std::fs::create_dir_all(&dir_path).unwrap();
        // macOS tempdir lives under /var (symlink to /private/var); init_project_path
        // canonicalizes, so derive expected paths from the canonical root too.
        let dir_path = dir_path.canonicalize().unwrap();
        let cfg_path = dir_path.join(".bbox").join("config.toml");
        init_project_path(&dir_path, false).unwrap();
        std::fs::write(&cfg_path, "# tweaked").unwrap();

        let result = init_project_path(&dir_path, false).unwrap();
        let cfg = std::fs::read_to_string(&cfg_path).unwrap();
        assert_eq!(cfg, "# tweaked");
        assert!(
            result
                .skipped
                .contains(&cfg_path.to_string_lossy().to_string())
        );
        assert!(
            !result
                .created
                .contains(&cfg_path.to_string_lossy().to_string())
        );
    }

    #[test]
    fn project_init_force_preserves_identity_bearing_config() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path().join("project");
        std::fs::create_dir_all(&dir_path).unwrap();
        // macOS tempdir lives under /var (symlink to /private/var); init_project_path
        // canonicalizes, so derive expected paths from the canonical root too.
        let dir_path = dir_path.canonicalize().unwrap();
        init_project_path(&dir_path, false).unwrap();

        let cfg_path = dir_path.join(".bbox").join("config.toml");
        std::fs::write(&cfg_path, "# custom\n").unwrap();
        let result = init_project_path(&dir_path, true).unwrap();

        let contents = std::fs::read_to_string(&cfg_path).unwrap();
        assert_eq!(contents, "# custom\n");
        assert!(
            result
                .skipped
                .contains(&cfg_path.to_string_lossy().to_string())
        );
        assert!(
            !result
                .created
                .contains(&cfg_path.to_string_lossy().to_string())
        );
    }

    #[test]
    fn project_init_records_repo_id_and_force_never_remints_it() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(root.join("seed"), "x").unwrap();
        run(&["add", "seed"]);
        run(&["commit", "-q", "-m", "seed"]);
        let root = root.canonicalize().unwrap();

        let first = init_project_path(&root, false).unwrap();
        assert!(first.repo_id_recorded);
        let repo_id = first.repo_id.unwrap();
        let config_before = std::fs::read_to_string(root.join(".bbox/config.toml")).unwrap();

        let second = init_project_path(&root, true).unwrap();
        assert_eq!(second.repo_id.as_deref(), Some(repo_id.as_str()));
        assert!(!second.repo_id_recorded);
        assert_eq!(
            std::fs::read_to_string(root.join(".bbox/config.toml")).unwrap(),
            config_before
        );
    }

    #[test]
    fn prepared_registration_artifacts_are_not_installed_until_publish() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join("myproject");
        let workflow_dir = project_dir.join(".bbox/workflows");
        std::fs::create_dir_all(&workflow_dir).unwrap();
        std::fs::write(
            workflow_dir.join("prepared-flow.json"),
            r#"{"name":"prepared-flow","version":"1","steps":[]}"#,
        )
        .unwrap();
        let catalog_dir = dir.path().join("catalog");
        let catalog = artifacts::ArtifactCatalog::open(&catalog_dir).unwrap();
        let installed_path =
            catalog_dir.join("projects/proj-prepared/committed/workflow/prepared-flow.json");

        let prepared = prepare_project_artifacts(&project_dir).unwrap();
        assert_eq!(prepared.len(), 1);
        assert!(!installed_path.exists());

        let installed = publish_project_artifacts(prepared, "proj-prepared", &catalog);
        assert_eq!(installed.len(), 1);
        assert!(installed_path.exists());
    }

    #[test]
    fn register_installs_bbox_artifacts_scoped_to_project() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join("myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        // Plant a workflow artifact in .bbox/workflows/
        let wf_dir = project_dir.join(".bbox").join("workflows");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("test-flow.json"),
            r#"{"name":"test-flow","version":"1","steps":[]}"#,
        )
        .unwrap();

        let catalog_dir = dir.path().join("catalog");
        let catalog = artifacts::ArtifactCatalog::open(&catalog_dir).unwrap();

        let installed =
            artifacts::discover_and_install_project_artifacts(&project_dir, "proj-abc", &catalog)
                .unwrap();

        assert_eq!(installed.len(), 1);
        let meta = &installed[0];
        assert_eq!(meta.name, "test-flow");
        assert_eq!(meta.project_id.as_deref(), Some("proj-abc"));
        assert!(!meta.local);
        // Artifact file should exist under the project-scoped path.
        // kind.as_str() returns "workflow" (singular), not "workflows".
        let artifact_path = catalog_dir
            .join("projects")
            .join("proj-abc")
            .join("committed")
            .join("workflow")
            .join("test-flow.json");
        assert!(
            artifact_path.exists(),
            "artifact not written to scoped path"
        );
    }

    #[test]
    fn register_repeated_noop_by_hash() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join("myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        let wf_dir = project_dir.join(".bbox").join("workflows");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("idempotent-flow.json"),
            r#"{"name":"idempotent-flow","version":"1","steps":[]}"#,
        )
        .unwrap();

        let catalog_dir = dir.path().join("catalog");
        let catalog = artifacts::ArtifactCatalog::open(&catalog_dir).unwrap();

        let first =
            artifacts::discover_and_install_project_artifacts(&project_dir, "proj-xyz", &catalog)
                .unwrap();
        let second =
            artifacts::discover_and_install_project_artifacts(&project_dir, "proj-xyz", &catalog)
                .unwrap();

        // Both calls succeed and return the same version — second install is a hash-match noop.
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].version, second[0].version);
        assert_eq!(
            first[0].content_sha256, second[0].content_sha256,
            "hash must be stable across identical installs"
        );
    }

    #[tokio::test]
    async fn bbox_project_list_round_trips_through_tool_serialization() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let server = test_server(&tmp);

        let register = server
            .bbox_project_register(Parameters(ProjectRegisterParams {
                path: project.to_string_lossy().into_owned(),
            }))
            .await;
        assert_ne!(register.is_error, Some(true));
        let register_text = serde_json::to_value(&register).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let register_response: serde_json::Value = serde_json::from_str(&register_text).unwrap();
        assert_eq!(
            register_response["indexing"]["status"].as_str(),
            Some("scheduled")
        );
        assert_eq!(
            register_response["indexing"]["mode"].as_str(),
            Some("background")
        );

        let listed = server.bbox_project_list();
        assert_ne!(listed.is_error, Some(true));
        let wire = serde_json::to_value(&listed).unwrap();
        let text = wire["content"][0]["text"].as_str().unwrap();
        let response: ProjectListResponse = serde_json::from_str(text).unwrap();

        assert_eq!(response.projects.len(), 1);
        assert_eq!(
            response.projects[0].project_id,
            entity_ref::project_id_for_path(&project).unwrap()
        );
    }

    #[tokio::test]
    async fn bbox_project_rename_migrates_project_scoped_state() {
        let tmp = tempfile::tempdir().unwrap();
        let old_project = tmp.path().join("old-project");
        let new_project = tmp.path().join("new-project");
        std::fs::create_dir_all(&old_project).unwrap();
        std::fs::create_dir_all(&new_project).unwrap();
        let old_project = std::fs::canonicalize(&old_project)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let new_project = std::fs::canonicalize(&new_project)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let server = test_server(&tmp);

        let register = server
            .bbox_project_register(Parameters(ProjectRegisterParams {
                path: old_project.clone(),
            }))
            .await;
        assert_ne!(register.is_error, Some(true));
        let text = serde_json::to_value(&register).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let registered: ProjectRecord = serde_json::from_value(
            serde_json::to_value(
                &serde_json::from_str::<serde_json::Value>(&text).unwrap()["record"],
            )
            .unwrap(),
        )
        .unwrap();

        server
            .state
            .kb
            .write()
            .remember(
                &knowledge::RememberParams {
                    content: "project fact".into(),
                    category: None,
                    title: Some("project fact".into()),
                    scope: Some("project".into()),
                    project: Some(old_project.clone()),
                    project_id: None,
                    decay: None,
                    review_at: None,
                    expires_at: None,
                },
                false,
            )
            .unwrap();
        server
            .state
            .threads
            .write()
            .thread(&threads::ThreadParams {
                action: "open".into(),
                name: None,
                id: None,
                topic: Some("project thread".into()),
                project: Some(old_project.clone()),
                project_id: None,
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: None,
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: None,
                origin: None,
            })
            .unwrap();
        // Test setup is sync-only; thread persistence is write-behind here.
        server.state.threads_persister.request();
        server
            .state
            .notes
            .write()
            .create(&notes::NoteParams {
                kind: "learned".into(),
                body: "project note".into(),
                task_id: None,
                session_id: None,
                project: Some(old_project.clone()),
                project_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
        server
            .state
            .pins
            .write()
            .pin(&pins::PinParams {
                action: "set".into(),
                content: Some("project pin".into()),
                title: Some("project pin".into()),
                scope: Some("session".into()),
                target: Some("sid".into()),
                project: Some(old_project.clone()),
                ..Default::default()
            })
            .unwrap();

        server
            .state
            .gaps
            .write()
            .file(&crate::gaps::GapFileParams {
                title: "project gap".into(),
                gap_kind: "tooling".into(),
                domain: "rename-coverage".into(),
                wanted_capability: "gap rows must follow a project rename".into(),
                dedupe_key: "tooling/rename-coverage/follow".into(),
                impact: None,
                blocking_level: None,
                missing_primitive: None,
                fallback_used: None,
                evidence: None,
                suggested_owner: None,
                notes: None,
                scope: Some("project".into()),
                project: Some(old_project.clone()),
                project_id: None,
                write_dir: None,
                task_id: None,
                session_id: None,
                provider: None,
                bro: None,
                thread_id: None,
                allow_recurrence: None,
            })
            .unwrap();
        server
            .state
            .roadmap
            .write()
            .create(
                "project roadmap item".into(),
                "must follow rename".into(),
                crate::roadmap::RoadmapCategory::Feature,
                crate::roadmap::RoadmapPriority::Medium,
                "project".into(),
                Some(old_project.clone()),
                None,
                None,
            )
            .unwrap();

        let renamed = server
            .bbox_project_rename(Parameters(ProjectRenameParams {
                project: registered.project_id.clone(),
                new_path: new_project.clone(),
                move_on_disk: None,
                dry_run: None,
            }))
            .await;
        assert_ne!(renamed.is_error, Some(true));
        let text = serde_json::to_value(&renamed).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let payload: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            payload["record"]["project_id"].as_str(),
            Some(registered.project_id.as_str())
        );
        assert_eq!(
            payload["record"]["canonical_path"].as_str(),
            Some(new_project.as_str())
        );
        assert_eq!(payload["migrated_refs"]["knowledge"], 1);
        assert_eq!(payload["migrated_refs"]["threads"], 1);
        assert_eq!(payload["migrated_refs"]["notes"], 1);
        assert_eq!(payload["migrated_refs"]["pins"], 1);
        assert_eq!(payload["migrated_refs"]["gaps"], 1);
        assert_eq!(payload["migrated_refs"]["roadmap"], 1);
        assert_eq!(payload["migrated_refs"]["webhooks"], 0);

        assert_eq!(
            server.state.kb.read().all_entries()[0].project.as_deref(),
            Some(new_project.as_str())
        );
        assert_eq!(
            server.state.threads.read().all()[0].project.as_str(),
            new_project.as_str()
        );
        assert_eq!(
            server.state.notes.read().all()[0].project.as_deref(),
            Some(new_project.as_str())
        );
        assert_eq!(server.state.pins.read().project_ref_count(&new_project), 1);
        assert_eq!(
            server.state.gaps.read().all()[0].project.as_deref(),
            Some(new_project.as_str()),
            "gap rows follow the rename instead of orphaning"
        );
        assert_eq!(
            server.state.roadmap.read().all_items()[0]
                .project
                .as_deref(),
            Some(new_project.as_str()),
            "roadmap items follow the rename instead of orphaning"
        );
    }

    mod catalog_mutation {
        use super::*;
        use crate::server::state::catalog_fixture::CatalogFixture;

        const PROJECT: &str = "proj_mutate";

        fn text(result: &rmcp::model::CallToolResult) -> String {
            result
                .content
                .iter()
                .filter_map(|content| content.as_text().map(|text| text.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// A remote-only catalog project resolves but has no attachment to
        /// write into. Eject writes repository files, so the refusal names
        /// the missing attachment rather than claiming the project is
        /// unregistered: its published knowledge still serves.
        #[tokio::test]
        async fn catalog_eject_of_a_remote_only_project_requires_an_attachment() {
            let fixture = CatalogFixture::new();
            fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
            let server = fixture.server();

            let result = server
                .bbox_project_eject(Parameters(ProjectEjectParams {
                    project: PROJECT.into(),
                    dry_run: None,
                }))
                .await;

            assert_eq!(result.is_error, Some(true));
            let text = text(&result);
            assert!(text.contains("error.project_attachment_required"), "{text}");
            assert!(
                !text.contains("not registered"),
                "a published remote-only project is registered: {text}"
            );
        }

        /// An attachment that records `repo_knowledge` but not
        /// `repo_mutation` is a capability refusal, not a missing
        /// attachment. Reporting it as attachment-required would send the
        /// operator to attach a checkout that is already attached.
        #[tokio::test]
        async fn catalog_eject_without_repo_mutation_is_capability_denied() {
            let fixture = CatalogFixture::new();
            let scope = CatalogFixture::scope(".");
            fixture.add_published_project(PROJECT, &scope);
            let directory = tempdir().unwrap();
            let checkout = directory.path().canonicalize().unwrap().join("checkout");
            std::fs::create_dir_all(&checkout).unwrap();
            fixture.attach_overlay_checkout(
                PROJECT,
                &scope,
                &checkout,
                "att_00000000000000000000000000000d01",
                "dddddddddddddddddddddddddddddd01",
                // repo_knowledge only: repo_mutation stays unset.
                true,
            );
            let server = fixture.server_with_checkout_authority();

            let result = server
                .bbox_project_eject(Parameters(ProjectEjectParams {
                    project: PROJECT.into(),
                    dry_run: None,
                }))
                .await;

            assert_eq!(result.is_error, Some(true));
            let text = text(&result);
            assert!(text.contains("error.project_capability_denied"), "{text}");
        }

        /// The bootstrap exception (plan 4.19) survives catalog mode: a path
        /// with no attachment is scaffolded with no lease, because attach
        /// needs the identity-bearing config this call writes.
        #[tokio::test]
        async fn unregistered_init_bootstraps_under_catalog_authority() {
            let fixture = CatalogFixture::new();
            let server = fixture.server_with_checkout_authority();
            let directory = tempdir().unwrap();
            let project = directory.path().canonicalize().unwrap().join("fresh");
            std::fs::create_dir_all(&project).unwrap();

            let result = server
                .bbox_project_init(Parameters(ProjectInitParams {
                    path: project.to_string_lossy().into_owned(),
                    force: false,
                }))
                .await;

            assert_ne!(result.is_error, Some(true), "{}", text(&result));
            assert!(project.join(".bbox/config.toml").exists());
            assert!(project.join(".bbox/knowledge").is_dir());
        }

        /// The same call against an ATTACHED catalog path is no longer
        /// bootstrap: it is a catalog-targeted mutation, so an attachment
        /// without `repo_mutation` refuses instead of scaffolding.
        #[tokio::test]
        async fn init_against_an_attached_path_requires_repo_mutation() {
            let fixture = CatalogFixture::new();
            let scope = CatalogFixture::scope(".");
            fixture.add_published_project(PROJECT, &scope);
            let directory = tempdir().unwrap();
            let checkout = directory.path().canonicalize().unwrap().join("checkout");
            std::fs::create_dir_all(&checkout).unwrap();
            fixture.attach_overlay_checkout(
                PROJECT,
                &scope,
                &checkout,
                "att_00000000000000000000000000000d01",
                "dddddddddddddddddddddddddddddd01",
                true,
            );
            let server = fixture.server_with_checkout_authority();

            let result = server
                .bbox_project_init(Parameters(ProjectInitParams {
                    path: checkout.to_string_lossy().into_owned(),
                    force: false,
                }))
                .await;

            assert_eq!(result.is_error, Some(true));
            let text = text(&result);
            assert!(text.contains("error.project_capability_denied"), "{text}");
            assert!(
                !checkout.join(".bbox/workflows").exists(),
                "a refused catalog-targeted init must write nothing"
            );
        }

        /// F2: a DETACHED attachment still governs its checkout path. The
        /// resolver only considers active attachments, so its refusal used
        /// to read as "unregistered" and took the lease-free bootstrap into
        /// a tree the catalog still governs.
        #[tokio::test]
        async fn init_into_a_detached_attachment_path_refuses_instead_of_bootstrapping() {
            let fixture = CatalogFixture::new();
            let scope = CatalogFixture::scope(".");
            fixture.add_published_project(PROJECT, &scope);
            let directory = tempdir().unwrap();
            let checkout = directory.path().canonicalize().unwrap().join("checkout");
            std::fs::create_dir_all(&checkout).unwrap();
            fixture.attach_overlay_checkout(
                PROJECT,
                &scope,
                &checkout,
                "att_00000000000000000000000000000f01",
                "ffffffffffffffffffffffffffffff01",
                true,
            );
            let server = fixture.server_with_checkout_authority();
            CatalogFixture::detach_in_server(&server, "att_00000000000000000000000000000f01");

            let result = server
                .bbox_project_init(Parameters(ProjectInitParams {
                    path: checkout.to_string_lossy().into_owned(),
                    force: false,
                }))
                .await;

            assert_eq!(result.is_error, Some(true));
            let text = text(&result);
            assert!(text.contains("error.project_attachment_required"), "{text}");
            assert!(
                !checkout.join(".bbox/workflows").exists(),
                "a governed checkout must not be scaffolded lease-free"
            );
        }

        /// F2: a subdirectory of a governed checkout is governed too. No row
        /// names it exactly, so an exact-match test would let it bootstrap.
        #[tokio::test]
        async fn init_inside_a_governed_checkout_refuses() {
            let fixture = CatalogFixture::new();
            let scope = CatalogFixture::scope(".");
            fixture.add_published_project(PROJECT, &scope);
            let directory = tempdir().unwrap();
            let checkout = directory.path().canonicalize().unwrap().join("checkout");
            let nested = checkout.join("crates").join("inner");
            std::fs::create_dir_all(&nested).unwrap();
            fixture.attach_overlay_checkout(
                PROJECT,
                &scope,
                &checkout,
                "att_00000000000000000000000000000f01",
                "ffffffffffffffffffffffffffffff01",
                true,
            );
            let server = fixture.server_with_checkout_authority();
            CatalogFixture::detach_in_server(&server, "att_00000000000000000000000000000f01");

            let result = server
                .bbox_project_init(Parameters(ProjectInitParams {
                    path: nested.to_string_lossy().into_owned(),
                    force: false,
                }))
                .await;

            assert_eq!(result.is_error, Some(true));
            assert!(!nested.join(".bbox/workflows").exists());
        }

        /// F2: the authority projection itself, at each arm. Absent is the
        /// only bootstrap-eligible verdict.
        #[test]
        fn catalog_path_authority_separates_absent_from_governed() {
            let fixture = CatalogFixture::new();
            let scope = CatalogFixture::scope(".");
            fixture.add_published_project(PROJECT, &scope);
            let directory = tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let checkout = root.join("checkout");
            std::fs::create_dir_all(&checkout).unwrap();
            let elsewhere = root.join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            fixture.attach_overlay_checkout(
                PROJECT,
                &scope,
                &checkout,
                "att_00000000000000000000000000000f01",
                "ffffffffffffffffffffffffffffff01",
                true,
            );
            let server = fixture.server_with_checkout_authority();
            let store = server
                .state
                .project_authority
                .catalog_store()
                .expect("catalog authority");

            assert_eq!(
                server.catalog_path_authority(store, &checkout.to_string_lossy()),
                CatalogPathAuthority::Attached(PROJECT.to_string())
            );
            assert_eq!(
                server.catalog_path_authority(store, &elsewhere.to_string_lossy()),
                CatalogPathAuthority::Absent
            );

            CatalogFixture::detach_in_server(&server, "att_00000000000000000000000000000f01");
            assert!(matches!(
                server.catalog_path_authority(store, &checkout.to_string_lossy()),
                CatalogPathAuthority::Governed { .. }
            ));
        }

        /// Attach one row with full control over kind, capabilities, and
        /// operator-default status. The shared fixture fixes kind to Base and
        /// sets only repo_knowledge; F3 needs a base PLUS a worktree with
        /// different capability bits and a default that points at the
        /// worktree.
        #[allow(clippy::too_many_arguments)] // one argument per durable attachment field
        fn attach_row(
            server: &BlackboxServer,
            attachment_id: &str,
            checkout_id: &str,
            dir: &Path,
            kind: bbox_corpus_core::project_catalog::AttachmentKind,
            repo_mutation: bool,
            default_for_project: bool,
        ) {
            use bbox_corpus_core::project_catalog::{
                AttachmentCapabilities, AttachmentId, AttachmentStatus, CheckoutAttachment,
                ProjectId,
            };

            std::fs::create_dir_all(dir.join(".bbox").join("local")).unwrap();
            std::fs::write(
                dir.join(".bbox").join("local").join("checkout-id"),
                format!("{checkout_id}\n"),
            )
            .unwrap();
            let store = server
                .state
                .project_authority
                .catalog_store()
                .expect("catalog authority");
            let scope = CatalogFixture::scope(".");
            let project_id = ProjectId::parse(PROJECT).unwrap();
            let attachment_id = AttachmentId::parse(attachment_id).unwrap();
            let dir = dir.to_string_lossy().into_owned();
            let epoch = store.snapshot().unwrap().epoch();
            store
                .transact(epoch, |_catalog, attachments| {
                    attachments.attachments.insert(
                        attachment_id.clone(),
                        CheckoutAttachment {
                            attachment_id: attachment_id.clone(),
                            project_id: project_id.clone(),
                            checkout_id: checkout_id.to_string(),
                            checkout_dir: dir.clone(),
                            checkout_project_dir: dir.clone(),
                            project_root_relpath: ".".into(),
                            kind,
                            validated_scope: Some(scope.clone()),
                            computed_repo_hint: None,
                            branch_ref: Some("refs/heads/main".into()),
                            capabilities: AttachmentCapabilities {
                                repo_knowledge: true,
                                repo_mutation,
                                ..Default::default()
                            },
                            status: AttachmentStatus::Attached,
                            attached_at: "2026-08-03T00:00:00Z".into(),
                            detached_at: None,
                        },
                    );
                    if default_for_project {
                        attachments
                            .default_attachments
                            .insert(project_id.clone(), attachment_id.clone());
                    }
                    Ok(())
                })
                .unwrap();
        }

        const BASE_ATT: &str = "att_00000000000000000000000000000b01";
        const WORKTREE_ATT: &str = "att_00000000000000000000000000000b02";
        const BASE_CO: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb01";
        const WORKTREE_CO: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb02";

        struct BaseAndWorktree {
            server: BlackboxServer,
            base: std::path::PathBuf,
            worktree: std::path::PathBuf,
            _directory: tempfile::TempDir,
        }

        /// A project with a base and a worktree, the operator default
        /// pointing at the WORKTREE - the exact topology F3 names.
        fn base_plus_worktree(
            base_repo_mutation: bool,
            worktree_repo_mutation: bool,
        ) -> BaseAndWorktree {
            let fixture = CatalogFixture::new();
            fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
            let server = fixture.server_with_checkout_authority();
            let directory = tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let base = root.join("base");
            let worktree = root.join("worktree");
            std::fs::create_dir_all(&base).unwrap();
            std::fs::create_dir_all(&worktree).unwrap();
            // The compatibility record reports is_git_repo, and eject records
            // a repo_id for a Git project, so the destination has to be one.
            for dir in [&base, &worktree] {
                let status = std::process::Command::new("git")
                    .args(["init", "--initial-branch", "main"])
                    .current_dir(dir)
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            attach_row(
                &server,
                BASE_ATT,
                BASE_CO,
                &base,
                bbox_corpus_core::project_catalog::AttachmentKind::Base,
                base_repo_mutation,
                false,
            );
            attach_row(
                &server,
                WORKTREE_ATT,
                WORKTREE_CO,
                &worktree,
                bbox_corpus_core::project_catalog::AttachmentKind::Worktree,
                worktree_repo_mutation,
                true,
            );
            BaseAndWorktree {
                server,
                base,
                worktree,
                _directory: directory,
            }
        }

        /// F3: the guard must pin the attachment the ejection writes. With
        /// the operator default on the worktree and repo_mutation absent from
        /// the BASE, leasing through the ladder succeeded on the worktree
        /// while the ejection wrote the base. Leasing the exact base
        /// attachment turns that silent wrong-attachment write into the
        /// correct capability refusal.
        #[tokio::test]
        async fn eject_leases_the_base_not_the_defaulted_worktree() {
            let topology = base_plus_worktree(false, true);

            let result = topology
                .server
                .bbox_project_eject(Parameters(ProjectEjectParams {
                    project: PROJECT.into(),
                    dry_run: None,
                }))
                .await;

            assert_eq!(result.is_error, Some(true), "{}", text(&result));
            let text = text(&result);
            assert!(text.contains("error.project_capability_denied"), "{text}");
            assert!(
                !topology.base.join(".bbox/knowledge").exists(),
                "the base must not be written while its own attachment lacks repo_mutation"
            );
            assert!(
                !topology.worktree.join(".bbox/knowledge").exists(),
                "the worktree is not the ejection destination either"
            );
        }

        /// The complement: the BASE carries repo_mutation and the defaulted
        /// worktree does not. The ladder would have refused on the worktree;
        /// leasing the exact destination succeeds and writes the base.
        #[tokio::test]
        async fn eject_succeeds_when_the_base_is_capable_and_the_default_is_not() {
            let topology = base_plus_worktree(true, false);

            let result = topology
                .server
                .bbox_project_eject(Parameters(ProjectEjectParams {
                    project: PROJECT.into(),
                    dry_run: None,
                }))
                .await;

            assert_ne!(result.is_error, Some(true), "{}", text(&result));
            assert!(
                topology.base.join(".bbox/knowledge").is_dir(),
                "ejection writes the base checkout it leased"
            );
            assert!(
                !topology.worktree.join(".bbox/knowledge").exists(),
                "the defaulted worktree is never the ejection destination"
            );
        }
    }
}
