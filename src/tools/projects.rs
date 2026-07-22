use anyhow::Context;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::artifacts;
use crate::config;
use crate::edge_index;
use crate::index::{self, ReindexParams};
use crate::mcp_tools;
use crate::mcp_tools::provenance::ProvenanceParams;
use crate::orchestration;
use crate::projects::{
    ProjectEjectParams, ProjectInitParams, ProjectListResponse, ProjectRegisterParams,
    ProjectRenameParams, ProjectUnregisterParams,
};
use crate::server::routes::{
    migrate_project_refs, project_ref_counts, trigger_project_bootstrap_arc,
};
use crate::server::state::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::json;

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
        description = "Register a project directory and schedule background agentic-corpus indexing. The path must be an absolute directory path (file paths and missing paths are rejected). Re-registering the same canonical path is idempotent — returns the existing record without modifying registered_at. project_id is derived from canonicalized realpath and is per-machine; repo_id derives from first commit SHA with remote fallback. Use bbox_project_list to inspect registered projects."
    )]
    pub(crate) async fn bbox_project_register(
        &self,
        Parameters(p): Parameters<ProjectRegisterParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        // Phase 1: register + alias materialization + persist — light lock
        // ops + async I/O on the runtime. Declared aliases come from the
        // repo's committed `.bbox/config.toml` and sync under the same write
        // lock so the persisted record carries them; a conflicting alias
        // claim fails the call (fail closed) while the registration itself
        // stands — fix the config and re-register to converge.
        let declared_aliases = config::load_project_at_ref(Path::new(&p.path), "HEAD")
            .map(|cfg| cfg.project.aliases.into_iter().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let res = {
            let mut projects = self.state.projects.write();
            projects.register_path(&p.path).and_then(|record| {
                projects.sync_declared_aliases(&record.project_id, &declared_aliases)?;
                projects.resolve(&record.project_id)?.with_context(|| {
                    format!("project vanished mid-register: {}", record.project_id)
                })
            })
        };
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
        // Phase 2: heavy fs work (MCP migration, config load, artifact discovery,
        // provenance import, watcher, kb sync) on the blocking pool.
        let server = self.clone();
        let result: anyhow::Result<String> = tokio::task::spawn_blocking(move || {
            orchestration::mcp::migrate_project_mcp_path(&PathBuf::from(&record.canonical_path))?;
            let project_config = config::load_project(Path::new(&record.canonical_path))?;
            let project_config_loaded = true;
            if project_config.mcp.enabled == Some(false) {
                tracing::info!(
                    "Project MCP is disabled via {}",
                    Path::new(&record.canonical_path)
                        .join(".bbox")
                        .join("config.toml")
                        .display()
                );
            }
            // Auto-discover and install .bbox/ artifacts unless explicitly disabled.
            if project_config.artifacts.auto_discover != Some(false) {
                let catalog = server.state.artifacts.read();
                match artifacts::discover_and_install_project_artifacts(
                    Path::new(&record.canonical_path),
                    &record.project_id,
                    &catalog,
                ) {
                    Ok(installed) if !installed.is_empty() => {
                        tracing::info!(
                            "Installed {} project artifact(s) for {}",
                            installed.len(),
                            record.project_id
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("artifact auto-discover for {}: {e:#}", record.project_id)
                    }
                }
            }
            let edges_dir = edge_index::edges_dir_from_bro_store(&server.state.store_dir);
            let provenance_params = ProvenanceParams {
                project_id: Some(record.project_id.clone()),
            };
            mcp_tools::provenance::import_provenance_to_edges_dir(
                &provenance_params,
                std::slice::from_ref(&record),
                &edges_dir,
            )?;
            // Register with the live .bbox/ watcher so future file changes
            // are picked up without a daemon restart.
            if let Ok(mut guard) = server.state.bbox_watcher.lock() {
                if let Some(w) = guard.as_mut() {
                    if let Err(e) =
                        w.watch_project(&record.project_id, Path::new(&record.canonical_path))
                    {
                        tracing::warn!("watcher add project {}: {e:#}", record.project_id);
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
                // lint-concurrency: allow(thread-spawn) — one-shot registration backfill; relocation to an owner module tracked in thread-935b467d
                std::thread::spawn(move || {
                    match index::backfill_tool_edges_for_project(
                        &reindex_cfg,
                        &project_for_backfill,
                    ) {
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
                "indexing": {
                    "status": "scheduled",
                    "mode": "background",
                    "detail": "project registration is durable; project-file indexing and edge projection are picked up by the background reindexer after this response, and embeddings across all routes (docs/code/notes/visual:*) then converge automatically via the background residue sweeper — no manual bbox_reembed is required"
                },
            });
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

    #[tool(
        name = "bbox_project_init",
        description = "Initialize a project-local .bbox workspace. Creates `.bbox/config.toml`, `.bbox/mcp.json`, `.bbox/local/.gitignore` and default subdirectories, and records the durable repo_id for Git projects. Idempotent by default; force=true refreshes replaceable skeleton files but always merge-preserves identity-bearing config.toml."
    )]
    pub(crate) async fn bbox_project_init(
        &self,
        Parameters(p): Parameters<ProjectInitParams>,
    ) -> CallToolResult {
        Self::run_blocking("bbox_project_init", move || {
            let path = Path::new(&p.path);
            if !path.is_absolute() {
                anyhow::bail!("project path must be absolute: {}", p.path);
            }
            if !path.exists() {
                anyhow::bail!("project path does not exist: {}", p.path);
            }
            let result = init_project_path(path, p.force)?;
            Ok(serde_json::to_string_pretty(&json!({
                "project": result.canonical,
                "created": result.created,
                "skipped": result.skipped,
                "repo_id": result.repo_id,
                "repo_id_recorded": result.repo_id_recorded,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_rename",
        description = "Rename a registered bbox project root while preserving its project_id and migrating project-scoped bbox state. Accepts project (project_id, registered canonical_path, or absolute path), new_path (absolute directory path), optional move_on_disk (default false), and optional dry_run. Updates project registry, knowledge, threads, notes, pins, packets, Slack channel bindings, live teams, whiteboards, pollers, and crons, then reindexes project files."
    )]
    pub(crate) async fn bbox_project_rename(
        &self,
        Parameters(p): Parameters<ProjectRenameParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        // Phase 1: rename in registry + async persist.
        let res = {
            let mut projects = self.state.projects.write();
            projects.rename_project(&p)
        };
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
                // Re-point kb roots at the new path now that migration has
                // rewritten entries into the renamed repo's `.bbox/`, and
                // re-enqueue embeds under the new path.
                // Use update_project_roots (no reload) because in-memory
                // mutations from migrate_rename are still live; reload would
                // clobber them.
                let roots: Vec<std::path::PathBuf> = server
                    .state
                    .projects
                    .read()
                    .list()
                    .into_iter()
                    .map(|r| std::path::PathBuf::from(r.canonical_path))
                    .collect();
                server.state.kb.write().update_project_roots(roots);
                crate::server::routes::enqueue_project_knowledge_embeds(
                    &server.state,
                    &new_project,
                );
                counts
            };

            let reindex = if response.dry_run {
                None
            } else {
                let result = server
                    .state
                    .idx
                    .write()
                    .reindex(&ReindexParams { full: Some(false) })?;
                server.rebuild_edge_index_from_stores();
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
        description = "Unregister a project root from the bbox project registry. Accepts project (project_id, registered canonical_path, or absolute path). Removes the registry entry only; does NOT delete project-scoped state (knowledge, threads, notes, pins, packets, Slack bindings, teams, whiteboards, pollers, crons) keyed on the project_id, which is derived from the canonical realpath and is stable across unregister+re-register. By default refuses when refs still exist and returns the counts; pass force=true to orphan them, or bbox_project_rename to migrate first. dry_run=true previews counts without mutating the registry."
    )]
    pub(crate) async fn bbox_project_unregister(
        &self,
        Parameters(p): Parameters<ProjectUnregisterParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let result: anyhow::Result<String> = async {
            let force = p.force.unwrap_or(false);
            let dry_run = p.dry_run.unwrap_or(false);

            let record = self
                .state
                .projects
                .read()
                .resolve(&p.project)?
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

            let removed = {
                let mut projects = self.state.projects.write();
                projects.unregister_project(&p.project)?
            };
            self.state.persist_projects_durable().await?;

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
        description = "Migrate a registered project's central-store knowledge entries into the repo's committed .bbox/knowledge/ (one file per entry), so the project's durable knowledge travels with the checkout. Accepts project (project_id, registered canonical_path, or absolute path) and optional dry_run. Entries are written without the absolute project path (location encodes scope) and dropped from the central store. dry_run=true reports the count without writing. Commit the resulting .bbox/ files to publish them."
    )]
    pub(crate) async fn bbox_project_eject(
        &self,
        Parameters(p): Parameters<ProjectEjectParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let server = self.clone();
        // Phase 1: fs + store work on the blocking pool.
        let fs_result = tokio::task::spawn_blocking(move || {
            let record = server
                .state
                .projects
                .read()
                .resolve(&p.project)?
                .with_context(|| format!("project not registered: {}", p.project))?;
            let dir = record.canonical_path.clone();
            let dry_run = p.dry_run.unwrap_or(false);
            let recorded_repo_id = if !dry_run && record.is_git_repo {
                Some(config::ensure_recorded_repo_id(Path::new(&dir))?)
            } else {
                None
            };

            // Ensure the project's repo is in kb roots so already-ejected files
            // are accounted for and the post-eject reload loads from the repo.
            crate::server::routes::sync_kb_project_roots(&server.state);

            let entries = if dry_run {
                server.state.kb.read().count_project_entries(&dir)
            } else {
                server.state.kb.write().eject_project_to_repo(&dir)?
            };

            Ok::<_, anyhow::Error>((record, dir, dry_run, entries, recorded_repo_id))
        })
        .await
        .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
        .and_then(std::convert::identity);

        match fs_result {
            Ok((record, dir, dry_run, entries, recorded_repo_id)) => {
                // Phase 2: await the kb persister durable ack on the runtime.
                if !dry_run && let Err(e) = self.state.kb_persister.request_durable().await {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    tracing::warn!(target: "blackbox::tool", tool = "bbox_project_eject", elapsed_ms = ms, error = %e, "err");
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                match serde_json::to_string_pretty(&json!({
                    "status": if dry_run { "dry_run" } else { "ok" },
                    "project_id": record.project_id,
                    "canonical_path": dir,
                    "entries": entries,
                    "repo_id": recorded_repo_id.as_ref().map(|recorded| &recorded.repo_id),
                    "repo_id_recorded": recorded_repo_id.as_ref().is_some_and(|recorded| recorded.newly_recorded),
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
                projects: self.state.projects.read().list(),
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
        BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())))
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
    }
}
