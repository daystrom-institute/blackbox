use crate::server::*;
use crate::*;
use crate::projects::ProjectInitParams;
use anyhow::Context;
use std::path::{Path, PathBuf};
use std::fs;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::projects_tools()
}

#[derive(Debug, Clone)]
struct ProjectInitResult {
    canonical: String,
    created: Vec<String>,
    skipped: Vec<String>,
}

fn write_or_skip_file(path: &Path, contents: &str, force: bool, created: &mut Vec<String>, skipped: &mut Vec<String>) -> anyhow::Result<()> {
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

fn write_or_skip_mcp(path: &Path, force: bool, created: &mut Vec<String>, skipped: &mut Vec<String>) -> anyhow::Result<()> {
    let path_display = path.to_string_lossy().to_string();
    if path.exists() && !force {
        skipped.push(path_display);
        return Ok(());
    }
    fs::create_dir_all(
        path.parent()
            .context("target path has no parent for initialization")?,
    )?;
    crate::orchestration::mcp::McpStore::new().save(path)?;
    created.push(path_display);
    Ok(())
}

fn init_project_path(project_dir: &Path, force: bool) -> anyhow::Result<ProjectInitResult> {
    let project_dir = project_dir
        .canonicalize()
        .context("canonicalizing project path for initialization")?;
    if !project_dir.is_dir() {
        anyhow::bail!("project path must be an existing directory: {}", project_dir.display());
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
    ];
    for dir in &dirs {
        let path = dir.as_path();
        if force || !path.exists() {
            fs::create_dir_all(path)?;
        }
    }

    write_or_skip_file(
        &bbox_dir.join("config.toml"),
        "# Project-local blackbox configuration.\n[roadmap]\n[mcp]\n[artifacts]\n",
        force,
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

    Ok(ProjectInitResult {
        canonical: project_dir.to_string_lossy().into_owned(),
        created,
        skipped,
    })
}

#[tool_router(router = projects_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_project_register",
        description = "Register a project directory for agentic-corpus indexing. The path must be an absolute directory path (file paths and missing paths are rejected). Re-registering the same canonical path is idempotent — returns the existing record without modifying registered_at. Triggers the project-bootstrap-arc which walks the project, chunks files, writes to the index, and emits structural edges. project_id is derived from the canonicalized realpath and is per-machine; not portable across hosts. repo_id is null for non-git projects; for git projects it derives from the first-commit SHA (with remote-URL fallback for shallow clones), so it survives clones. Use bbox_project_list to inspect registered projects."
    )]
    pub(crate) fn bbox_project_register(
        &self,
        Parameters(p): Parameters<ProjectRegisterParams>,
    ) -> CallToolResult {
        Self::run("bbox_project_register", || {
            let record = self.state.projects.write().register_path(&p.path)?;
            crate::orchestration::mcp::migrate_project_mcp_path(&PathBuf::from(&record.canonical_path))?;
            let project_config = crate::config::load_project(Path::new(&record.canonical_path))?;
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
                let catalog = self.state.artifacts.read();
                match crate::artifacts::discover_and_install_project_artifacts(
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
                    Err(e) => tracing::warn!("artifact auto-discover for {}: {e:#}", record.project_id),
                }
            }
            let edges_dir = edge_index::edges_dir_from_bro_store(&self.state.store_dir);
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
            if let Ok(mut guard) = self.state.bbox_watcher.lock() {
                if let Some(w) = guard.as_mut() {
                    if let Err(e) = w.watch_project(
                        &record.project_id,
                        Path::new(&record.canonical_path),
                    ) {
                        tracing::warn!("watcher add project {}: {e:#}", record.project_id);
                    }
                }
            }
            trigger_project_bootstrap_arc(self.state.clone(), record.clone());
            self.state
                .idx
                .write()
                .reindex(&ReindexParams { full: Some(false) })?;
            // Rebuild EdgeIndex AFTER reindex so freshly-derived edges from the
            // new project's chunks (IN_FILE, CONTAINS_SYMBOL, NEXT_CHUNK, etc.)
            // are projected into the in-memory index. Doing this before reindex
            // (the prior order) left the new project's edges invisible until
            // the next unrelated rebuild trigger.
            self.rebuild_edge_index_from_stores();
            let response = json!({
                "record": record,
                "project_config_loaded": project_config_loaded,
            });
            Ok(serde_json::to_string_pretty(&response)?)
        })
    }

    #[tool(
        name = "bbox_project_init",
        description = "Initialize a project-local .bbox workspace. Creates `.bbox/config.toml`, `.bbox/mcp.json`, `.bbox/local/.gitignore` and default subdirectories. Idempotent by default; set force=true to overwrite skeleton files while preserving subdirectory contents."
    )]
    pub(crate) fn bbox_project_init(
        &self,
        Parameters(p): Parameters<ProjectInitParams>,
    ) -> CallToolResult {
        Self::run("bbox_project_init", || {
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
            }))?)
        })
    }

    #[tool(
        name = "bbox_project_rename",
        description = "Rename a registered bbox project root while preserving its project_id and migrating project-scoped bbox state. Accepts project (project_id, registered canonical_path, or absolute path), new_path (absolute directory path), optional move_on_disk (default false), and optional dry_run. Updates project registry, knowledge, threads, notes, pins, packets, Slack channel bindings, live teams, councils, whiteboards, pollers, and crons, then reindexes project files."
    )]
    pub(crate) fn bbox_project_rename(
        &self,
        Parameters(p): Parameters<ProjectRenameParams>,
    ) -> CallToolResult {
        Self::run("bbox_project_rename", || {
            let response = self.state.projects.write().rename_project(&p)?;
            let old_project = response.old_record.canonical_path.clone();
            let new_project = response.record.canonical_path.clone();

            let counts = if response.dry_run {
                project_ref_counts(&self.state, &old_project)?
            } else {
                migrate_project_refs(&self.state, &old_project, &new_project, &response.record)?
            };

            let reindex = if response.dry_run {
                None
            } else {
                let result = self
                    .state
                    .idx
                    .write()
                    .reindex(&ReindexParams { full: Some(false) })?;
                self.rebuild_edge_index_from_stores();
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
    use tempfile::tempdir;

    #[test]
    fn project_init_creates_bbox_skeleton() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path().join("project");
        std::fs::create_dir_all(&dir_path).unwrap();
        let result = init_project_path(&dir_path, false).unwrap();
        let cfg_path = dir_path.join(".bbox").join("config.toml");
        let mcp_path = dir_path.join(".bbox").join("mcp.json");
        let gitignore_path = dir_path.join(".bbox").join("local").join(".gitignore");
        assert!(cfg_path.exists());
        assert!(mcp_path.exists());
        assert!(gitignore_path.exists());
        assert!(result.created.contains(&cfg_path.to_string_lossy().into_owned()));
        let store = crate::orchestration::mcp::McpStore::load(&mcp_path).unwrap();
        assert_eq!(store.version, 1);
        assert_eq!(result.canonical, dir_path.canonicalize().unwrap().to_string_lossy());
        assert_eq!(result.skipped.len(), 0);
    }

    #[test]
    fn project_init_is_idempotent_without_force() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path().join("project");
        std::fs::create_dir_all(&dir_path).unwrap();
        let cfg_path = dir_path.join(".bbox").join("config.toml");
        init_project_path(&dir_path, false).unwrap();
        std::fs::write(&cfg_path, "# tweaked").unwrap();

        let result = init_project_path(&dir_path, false).unwrap();
        let cfg = std::fs::read_to_string(&cfg_path).unwrap();
        assert_eq!(cfg, "# tweaked");
        assert!(result.skipped.contains(&cfg_path.to_string_lossy().to_string()));
        assert!(!result.created.contains(&cfg_path.to_string_lossy().to_string()));
    }

    #[test]
    fn project_init_force_overwrites_skeleton_files() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path().join("project");
        std::fs::create_dir_all(&dir_path).unwrap();
        init_project_path(&dir_path, false).unwrap();

        let cfg_path = dir_path.join(".bbox").join("config.toml");
        std::fs::write(&cfg_path, "# custom\n").unwrap();
        let result = init_project_path(&dir_path, true).unwrap();

        let contents = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(!contents.contains("# custom"));
        assert!(result.created.contains(&cfg_path.to_string_lossy().to_string()));
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
        let catalog = crate::artifacts::ArtifactCatalog::open(&catalog_dir).unwrap();

        let installed = crate::artifacts::discover_and_install_project_artifacts(
            &project_dir,
            "proj-abc",
            &catalog,
        )
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
        assert!(artifact_path.exists(), "artifact not written to scoped path");
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
        let catalog = crate::artifacts::ArtifactCatalog::open(&catalog_dir).unwrap();

        let first = crate::artifacts::discover_and_install_project_artifacts(
            &project_dir,
            "proj-xyz",
            &catalog,
        )
        .unwrap();
        let second = crate::artifacts::discover_and_install_project_artifacts(
            &project_dir,
            "proj-xyz",
            &catalog,
        )
        .unwrap();

        // Both calls succeed and return the same version — second install is a hash-match noop.
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].version, second[0].version);
        assert_eq!(
            first[0].content_sha256,
            second[0].content_sha256,
            "hash must be stable across identical installs"
        );
    }
}
