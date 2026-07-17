//! `bbox_mount_*` MCP tools: register, list, sync, and unregister remote-
//! source connector mounts (design/connectors/remote-source-connectors.md).
//! Handlers stay thin per `src/tools/CLAUDE.md` - all behavior lives in
//! `crate::connectors_runtime`, shared with the periodic sync loop in
//! `server::mount_sync` so there is exactly one sync code path.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::connectors_runtime;
use crate::server::state::BlackboxServer;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::mounts_tools()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MountRegisterParams {
    /// Connector alias. The builtin "git" is always available with no
    /// config; other aliases resolve through `[connectors.<alias>]` in
    /// config.toml (config-alias-with-type pattern).
    pub connector: String,
    /// Connector-shaped scope string. For "git": a clone URL, optionally
    /// with a `#<ref>` suffix (default: the remote's default branch).
    pub scope: String,
    /// Absolute materialization root override. Default:
    /// `<state_dir>/mounts/<mount_id>/`.
    #[serde(default)]
    pub path: Option<String>,
    /// Mount policy overrides (include/exclude globs, max_file_bytes,
    /// max_total_bytes, native_export, visual_embed). Object or JSON-encoded
    /// string; unset fields take bbox_connectors::MountPolicy defaults.
    #[serde(default)]
    pub policy: Option<serde_json::Value>,
    /// Alias to declare on the project this mount registers.
    #[serde(default)]
    pub project_alias: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MountSyncParams {
    /// mount_id, materialization_root, or remote_scope of a registered mount.
    pub mount: String,
    /// Ignore the stored cursor and force a full walk of the scope.
    #[serde(default)]
    pub full: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MountUnregisterParams {
    /// mount_id, materialization_root, or remote_scope of a registered mount.
    pub mount: String,
    /// Delete the materialization root and manifest. Refused unless the
    /// root carries the ownership marker stamped at materialization time.
    /// Default false: keeps bytes on disk.
    #[serde(default)]
    pub remove_files: Option<bool>,
    /// Unregister even when the underlying project still has project-scoped
    /// refs (knowledge, threads, notes, pins, ...). Default false: refuses
    /// and reports the counts.
    #[serde(default)]
    pub force: Option<bool>,
}

#[tool_router(router = mounts_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_mount_register",
        description = "Register a remote-source connector mount: validate the connector against scope (fail closed), create the mount record, and run the initial sync in the background (returns immediately with mount_id + sync status, like registration-time project indexing). Once the initial sync materializes files, the materialization root auto-registers as a normal project with source=RemoteMount. Re-registering an existing (connector, scope) pair returns the existing mount, not a duplicate. Poll bbox_mount_list for sync state and project_id."
    )]
    pub(crate) async fn bbox_mount_register(
        &self,
        Parameters(p): Parameters<MountRegisterParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let policy = match p.policy {
            Some(v) => match Self::parse_spec(v, "mount policy") {
                Ok(policy) => policy,
                Err(resp) => return resp,
            },
            None => bbox_connectors::MountPolicy::default(),
        };
        let result = connectors_runtime::register_mount(
            &self.state,
            p.connector,
            p.scope,
            p.path,
            policy,
            p.project_alias,
        )
        .await;
        match result {
            Ok(value) => {
                let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool = "bbox_mount_register", elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_mount_register", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_mount_list",
        description = "List registered remote-source connector mounts with connector, scope, materialization_root, project_id, cursor presence, and last_sync (fetched/skipped/deleted/renamed/errors/degradations)."
    )]
    pub(crate) async fn bbox_mount_list(&self) -> CallToolResult {
        Self::ok_json(&connectors_runtime::list_mounts(&self.state))
    }

    #[tool(
        name = "bbox_mount_sync",
        description = "Run a manual sync pass for one registered mount. full=true discards the stored cursor and forces a full walk. Refuses with status=\"busy\" when a sync for this mount (manual or the periodic background pass) is already in flight - no error, just try again shortly."
    )]
    pub(crate) async fn bbox_mount_sync(
        &self,
        Parameters(p): Parameters<MountSyncParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let mount_id = match connectors_runtime::resolve_mount_id(&self.state, &p.mount) {
            Ok(id) => id,
            Err(e) => return Self::err_text(&format!("Error: {e:#}")),
        };
        let full = p.full.unwrap_or(false);
        let result =
            connectors_runtime::run_sync_and_register_project(&self.state, &mount_id, full, None)
                .await;
        match result {
            Ok(summary) => {
                let text = serde_json::to_string_pretty(&json!({
                    "status": "ok",
                    "mount_id": mount_id,
                    "summary": summary,
                }))
                .unwrap_or_default();
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool = "bbox_mount_sync", elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) if e.downcast_ref::<connectors_runtime::MountBusy>().is_some() => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool = "bbox_mount_sync", elapsed_ms = ms, "busy");
                Self::ok_json(&json!({
                    "status": "busy",
                    "mount_id": mount_id,
                    "detail": format!("{e:#}"),
                }))
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_mount_sync", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_mount_unregister",
        description = "Unregister a mount. Mirrors bbox_project_unregister's attached-state refusal: refuses when the underlying project still has project-scoped refs unless force=true. remove_files=true additionally deletes the materialization root and manifest, refused unless the root carries the ownership marker stamped at materialization time; default false keeps bytes on disk."
    )]
    pub(crate) async fn bbox_mount_unregister(
        &self,
        Parameters(p): Parameters<MountUnregisterParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let remove_files = p.remove_files.unwrap_or(false);
        let force = p.force.unwrap_or(false);
        let result =
            connectors_runtime::unregister_mount(&self.state, &p.mount, remove_files, force).await;
        match result {
            Ok(value) => {
                let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool = "bbox_mount_unregister", elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_mount_unregister", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::ProjectRegisterParams;
    use crate::server::state::{BlackboxServer, SharedState};
    use crate::{entity_ref, knowledge};
    use rmcp::model::CallToolResult;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    // Fixture repo setup is a synchronous, test-only git harness - unrelated
    // to the async library/daemon code under test (same allowance
    // `bbox-connectors`'s own git_connector.rs tests carry).
    #[allow(clippy::disallowed_methods)]
    fn run_git(path: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[allow(clippy::disallowed_methods)]
    fn init_fixture_repo(dir: &Path) -> std::path::PathBuf {
        let repo = dir.join("remote");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        run_git(&repo, &["add", "a.txt"]);
        run_git(&repo, &["commit", "-m", "add a"]);
        repo
    }

    fn file_url(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())))
    }

    fn result_json(result: &CallToolResult) -> serde_json::Value {
        let text = serde_json::to_value(result).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        serde_json::from_str(&text).unwrap()
    }

    /// Poll `bbox_mount_list` until `pred` matches the mount's entry, or
    /// panic after a generous timeout. The initial sync (and the periodic
    /// loop) run on a spawned background task; the register/sync tools
    /// themselves return as soon as the light-lock phase is durable, so
    /// tests observe convergence the same way an operator would.
    async fn wait_for_mount(
        server: &BlackboxServer,
        mount_id: &str,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        for _ in 0..300 {
            let listed = result_json(&server.bbox_mount_list().await);
            if let Some(entry) = listed["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["mount_id"] == mount_id)
                && pred(entry)
            {
                return entry.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("mount {mount_id} did not converge in time");
    }

    #[tokio::test]
    async fn register_rejects_credential_embedded_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        // Only the redacted scope is persisted and every post-registration
        // sync rebuilds from the record, so a token-bearing URL must be
        // refused up front instead of validating and then failing forever.
        let register = server
            .bbox_mount_register(Parameters(MountRegisterParams {
                connector: "git".to_string(),
                scope: "https://x-access-token:tok123@example.invalid/org/repo.git".to_string(),
                path: None,
                policy: None,
                project_alias: None,
            }))
            .await;
        assert_eq!(register.is_error, Some(true));
        let text = format!("{register:?}");
        assert!(text.contains("embeds credentials"), "unexpected: {text}");
        assert!(
            !text.contains("tok123"),
            "credential leaked into error: {text}"
        );
        assert!(server.state.mounts.read().list().is_empty());
    }

    #[tokio::test]
    async fn mount_registration_loads_committed_repo_owned_knowledge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_fixture_repo(tmp.path());

        // A committed project entry as it lands in git (project omitted on
        // disk), mirroring projects.rs's
        // register_loads_and_counts_committed_project_knowledge.
        let entry = crate::knowledge::KnowledgeEntry {
            id: "m0000001".into(),
            title: "mounted repo rule".into(),
            content: "committed convention travels with the mount".into(),
            cluster: None,
            variants: Default::default(),
            category: crate::knowledge::Category::Convention,
            scope: crate::knowledge::Scope::Project,
            project: None,
            providers: vec![],
            priority: crate::knowledge::Priority::Standard,
            weight: 100,
            status: crate::knowledge::Status::Active,
            approval: crate::knowledge::Approval::UserConfirmed,
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
        std::fs::create_dir_all(repo.join(".bbox").join("knowledge")).unwrap();
        std::fs::write(
            repo.join(".bbox").join("knowledge").join("m0000001.json"),
            serde_json::to_string(&entry).unwrap(),
        )
        .unwrap();
        run_git(&repo, &["add", ".bbox"]);
        run_git(&repo, &["commit", "-m", "add repo-owned knowledge"]);

        let server = test_server(&tmp);
        let register = server
            .bbox_mount_register(Parameters(MountRegisterParams {
                connector: "git".to_string(),
                scope: file_url(&repo),
                path: None,
                policy: None,
                project_alias: None,
            }))
            .await;
        assert_ne!(register.is_error, Some(true));
        let mount_id = result_json(&register)["mount_id"]
            .as_str()
            .unwrap()
            .to_string();

        let _ = wait_for_mount(&server, &mount_id, |m| !m["project_id"].is_null()).await;
        assert!(
            server.state.kb.read().entry("m0000001").is_some(),
            "mounted repo's committed .bbox/knowledge entry should load once the \
             mount's project registers, without a daemon restart"
        );
    }

    #[tokio::test]
    async fn register_syncs_in_background_and_registers_project_with_mount_source() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_fixture_repo(tmp.path());
        let server = test_server(&tmp);

        let register = server
            .bbox_mount_register(Parameters(MountRegisterParams {
                connector: "git".to_string(),
                scope: file_url(&repo),
                path: None,
                policy: None,
                project_alias: None,
            }))
            .await;
        assert_ne!(register.is_error, Some(true));
        let response = result_json(&register);
        assert_eq!(response["status"], "ok");
        assert_eq!(response["sync"], "started");
        let mount_id = response["mount_id"].as_str().unwrap().to_string();

        let entry = wait_for_mount(&server, &mount_id, |m| !m["project_id"].is_null()).await;
        assert_eq!(entry["last_sync"]["fetched"], 1);
        assert!(entry["last_sync"]["errors"].as_array().unwrap().is_empty());

        let project_id = entry["project_id"].as_str().unwrap().to_string();
        let project = server
            .state
            .projects
            .read()
            .resolve(&project_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            project.source,
            crate::projects::ProjectSource::RemoteMount {
                mount_id: mount_id.clone()
            }
        );

        // The mount source marker is visible through bbox_project_list too,
        // not just the resolved record - it's a plain serialized field.
        let listed = result_json(&server.bbox_project_list());
        let projects = listed["projects"].as_array().unwrap();
        let mounted = projects
            .iter()
            .find(|p| p["project_id"] == project_id)
            .unwrap();
        assert_eq!(mounted["source"]["kind"], "remote_mount");
        assert_eq!(mounted["source"]["mount_id"], mount_id);

        let materialized = Path::new(entry["materialization_root"].as_str().unwrap());
        assert_eq!(
            std::fs::read_to_string(materialized.join("a.txt")).unwrap(),
            "hello\n"
        );
    }

    #[tokio::test]
    async fn re_register_same_connector_and_scope_returns_existing_mount_not_a_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_fixture_repo(tmp.path());
        let server = test_server(&tmp);
        let params = || MountRegisterParams {
            connector: "git".to_string(),
            scope: file_url(&repo),
            path: None,
            policy: None,
            project_alias: None,
        };

        let first = result_json(&server.bbox_mount_register(Parameters(params())).await);
        assert_eq!(first["status"], "ok");
        let mount_id = first["mount_id"].as_str().unwrap().to_string();

        let second = result_json(&server.bbox_mount_register(Parameters(params())).await);
        assert_eq!(second["status"], "exists");
        assert_eq!(second["mount"]["mount_id"], mount_id);

        let listed = result_json(&server.bbox_mount_list().await);
        assert_eq!(listed["mounts"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_connector_alias_fails_closed_with_remediation_text() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        let register = server
            .bbox_mount_register(Parameters(MountRegisterParams {
                connector: "not-a-real-alias".to_string(),
                scope: "irrelevant".to_string(),
                path: None,
                policy: None,
                project_alias: None,
            }))
            .await;
        assert_eq!(register.is_error, Some(true));
        let text = serde_json::to_value(&register).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(text.contains("[connectors."), "{text}");
    }

    #[tokio::test]
    async fn manual_sync_full_forces_full_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_fixture_repo(tmp.path());
        let server = test_server(&tmp);

        let register = result_json(
            &server
                .bbox_mount_register(Parameters(MountRegisterParams {
                    connector: "git".to_string(),
                    scope: file_url(&repo),
                    path: None,
                    policy: None,
                    project_alias: None,
                }))
                .await,
        );
        let mount_id = register["mount_id"].as_str().unwrap().to_string();
        wait_for_mount(&server, &mount_id, |m| !m["project_id"].is_null()).await;

        let synced = result_json(
            &server
                .bbox_mount_sync(Parameters(MountSyncParams {
                    mount: mount_id.clone(),
                    full: Some(true),
                }))
                .await,
        );
        assert_eq!(synced["status"], "ok");
        assert_eq!(synced["mount_id"], mount_id);
        // Full resync of an unchanged single-file repo: resume-without-
        // refetch means zero new fetches, but the pass still completes ok.
        assert!(synced["summary"]["errors"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sync_refuses_busy_when_already_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_fixture_repo(tmp.path());
        let server = test_server(&tmp);

        let register = result_json(
            &server
                .bbox_mount_register(Parameters(MountRegisterParams {
                    connector: "git".to_string(),
                    scope: file_url(&repo),
                    path: None,
                    policy: None,
                    project_alias: None,
                }))
                .await,
        );
        let mount_id = register["mount_id"].as_str().unwrap().to_string();

        // Simulate an in-flight sync by taking the same per-mount lock the
        // background initial sync and the sync tool both acquire.
        server
            .state
            .mount_sync_locks
            .lock()
            .insert(mount_id.clone());

        let busy = result_json(
            &server
                .bbox_mount_sync(Parameters(MountSyncParams {
                    mount: mount_id.clone(),
                    full: Some(false),
                }))
                .await,
        );
        assert_eq!(busy["status"], "busy");
        assert_eq!(busy["mount_id"], mount_id);

        server.state.mount_sync_locks.lock().remove(&mount_id);
    }

    #[tokio::test]
    async fn unregister_refuses_attached_refs_then_force_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_fixture_repo(tmp.path());
        let server = test_server(&tmp);

        let register = result_json(
            &server
                .bbox_mount_register(Parameters(MountRegisterParams {
                    connector: "git".to_string(),
                    scope: file_url(&repo),
                    path: None,
                    policy: None,
                    project_alias: None,
                }))
                .await,
        );
        let mount_id = register["mount_id"].as_str().unwrap().to_string();
        let entry = wait_for_mount(&server, &mount_id, |m| !m["project_id"].is_null()).await;
        let materialization_root = entry["materialization_root"].as_str().unwrap().to_string();

        server
            .state
            .kb
            .write()
            .remember(
                &knowledge::RememberParams {
                    content: "mounted project fact".into(),
                    category: None,
                    title: Some("mounted project fact".into()),
                    scope: Some("project".into()),
                    project: Some(materialization_root.clone()),
                    decay: None,
                    review_at: None,
                    expires_at: None,
                },
                false,
            )
            .unwrap();

        let refused = server
            .bbox_mount_unregister(Parameters(MountUnregisterParams {
                mount: mount_id.clone(),
                remove_files: None,
                force: None,
            }))
            .await;
        assert_eq!(refused.is_error, Some(true));

        let forced = result_json(
            &server
                .bbox_mount_unregister(Parameters(MountUnregisterParams {
                    mount: mount_id.clone(),
                    remove_files: Some(true),
                    force: Some(true),
                }))
                .await,
        );
        assert_eq!(forced["status"], "ok");
        assert_eq!(forced["forced"], true);
        assert_eq!(forced["removed_files"], true);

        let listed = result_json(&server.bbox_mount_list().await);
        assert!(listed["mounts"].as_array().unwrap().is_empty());
        assert!(!Path::new(&materialization_root).exists());

        let projects = result_json(&server.bbox_project_list());
        assert!(projects["projects"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unregister_default_keeps_materialized_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_fixture_repo(tmp.path());
        let server = test_server(&tmp);

        let register = result_json(
            &server
                .bbox_mount_register(Parameters(MountRegisterParams {
                    connector: "git".to_string(),
                    scope: file_url(&repo),
                    path: None,
                    policy: None,
                    project_alias: None,
                }))
                .await,
        );
        let mount_id = register["mount_id"].as_str().unwrap().to_string();
        let entry = wait_for_mount(&server, &mount_id, |m| !m["project_id"].is_null()).await;
        let materialization_root = entry["materialization_root"].as_str().unwrap().to_string();

        let result = result_json(
            &server
                .bbox_mount_unregister(Parameters(MountUnregisterParams {
                    mount: mount_id,
                    remove_files: None,
                    force: None,
                }))
                .await,
        );
        assert_eq!(result["status"], "ok");
        assert_eq!(result["removed_files"], false);
        assert!(Path::new(&materialization_root).join("a.txt").exists());
    }

    /// Sanity check that mounts.rs's params types don't collide with an
    /// existing registered project the caller wired up separately (project
    /// registration keeps working normally alongside mounts).
    #[tokio::test]
    async fn plain_project_registration_is_unaffected_by_mounts_wiring() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("plain-project");
        std::fs::create_dir_all(&project).unwrap();
        let server = test_server(&tmp);

        let registered = server
            .bbox_project_register(Parameters(ProjectRegisterParams {
                path: project.to_string_lossy().into_owned(),
            }))
            .await;
        assert_ne!(registered.is_error, Some(true));

        let listed = result_json(&server.bbox_project_list());
        let projects = listed["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(
            projects[0]["project_id"],
            entity_ref::project_id_for_path(&project).unwrap()
        );
        assert_eq!(projects[0]["source"]["kind"], "local_fs");
    }
}
