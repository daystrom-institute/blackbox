use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::projects_tools()
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
            let edges_dir = edge_index::edges_dir_from_bro_store(&self.state.store_dir);
            let provenance_params = ProvenanceParams {
                project_id: Some(record.project_id.clone()),
            };
            mcp_tools::provenance::import_provenance_to_edges_dir(
                &provenance_params,
                std::slice::from_ref(&record),
                &edges_dir,
            )?;
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
            Ok(serde_json::to_string_pretty(&record)?)
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
