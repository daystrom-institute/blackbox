//! Catalog administration MCP surface
//! (design/daemon-runtime/durable-project-catalog-phase2-impl.md §7.9).
//!
//! Every tool here is registered unconditionally so the roster and the tool
//! docs stay stable across runtime modes, and every one refuses with
//! `error.project_catalog_inactive` while the version-1 bridge is the
//! authority (plan §7.1). Filesystem probing runs off-lock inside the
//! blocking phase and enters the domain layer as data: this module never
//! writes either snapshot directly, and `bbox_indexing::project_catalog_admin`
//! owns every invariant.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{
    AttachmentCapabilities, AttachmentId, AttachmentKind, AttachmentStatus, ProjectId,
    ProjectScope, ScopeMigrationKind,
};
use bbox_corpus_core::project_selector::{ProjectResolveError, ProjectSelectorRequest};
use bbox_indexing::project_catalog_admin;
use bbox_indexing::project_catalog_store::ProjectCatalogStore;
use bbox_indexing::project_resolver::ProjectResolverEngine;

use crate::config;
use crate::server::state::BlackboxServer;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::project_catalog_tools()
}

/// Longest accepted `audit_reason`, matching the catalog's own bounded
/// audit-text limit so a refusal happens here rather than deep in a
/// transaction closure.
const MAX_AUDIT_REASON_BYTES: usize = 1024;

// ── Parameters ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct CatalogListParams {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct CatalogGetParams {
    /// Project selector: catalog project id, an accepted alias, or any
    /// selector the resolver proves to exactly one project.
    pub project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectAttachParams {
    /// Project selector resolved Selection-class: exactly one project or a
    /// typed refusal.
    pub project: String,
    /// Absolute path of the checkout directory to attach.
    pub path: String,
    /// Catalog epoch the caller read; a mismatch is a typed refusal.
    pub expected_catalog_epoch: u64,
    /// Bounded operator reason recorded with the operation.
    pub audit_reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectDetachParams {
    pub attachment_id: String,
    pub expected_catalog_epoch: u64,
    pub audit_reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectDefaultAttachmentParams {
    pub project: String,
    /// Attachment to record as the default local source. Absent clears the
    /// selection.
    #[serde(default)]
    pub attachment_id: Option<String>,
    pub expected_catalog_epoch: u64,
    pub audit_reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectPromoteParams {
    /// Exact catalog project id. Promotion never accepts a loose selector:
    /// the register refusal hands the operator the id.
    pub project_id: String,
    pub attachment_id: String,
    pub proposed_repo_id: String,
    pub proposed_relpath: String,
    pub expected_catalog_epoch: u64,
    pub audit_reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectScopeMigrateParams {
    pub project_id: String,
    pub expected_old_repo_id: String,
    pub expected_old_relpath: String,
    pub new_repo_id: String,
    pub new_relpath: String,
    /// `relpath-move` or `repo-authority-change`.
    pub kind: String,
    pub attachment_id: String,
    /// Operator authority for a recorded-authority change. Agents pass this
    /// through from operator input and never default or infer it.
    #[serde(default)]
    pub acknowledge_repo_authority_change: bool,
    #[serde(default)]
    pub dry_run: bool,
    pub expected_catalog_epoch: u64,
    pub audit_reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectPublisherBindParams {
    pub project_id: String,
    pub attachment_id: String,
    pub expected_catalog_epoch: u64,
    pub audit_reason: String,
}

// ── Shared helpers ──────────────────────────────────────────────────────

fn catalog_inactive() -> String {
    format!("Error: {}", ProjectResolveError::catalog_inactive())
}

fn bounded_audit_reason(raw: &str) -> anyhow::Result<String> {
    let reason = raw.trim();
    if reason.is_empty() {
        anyhow::bail!("error.project_catalog_admin_audit_reason: audit_reason is required");
    }
    if reason.len() > MAX_AUDIT_REASON_BYTES {
        anyhow::bail!(
            "error.project_catalog_admin_audit_reason: audit_reason exceeds {MAX_AUDIT_REASON_BYTES} bytes"
        );
    }
    Ok(reason.to_string())
}

fn parse_project_id(raw: &str) -> anyhow::Result<ProjectId> {
    ProjectId::parse(raw.to_string()).map_err(|error| anyhow::anyhow!("{error}"))
}

fn parse_attachment_id(raw: &str) -> anyhow::Result<AttachmentId> {
    AttachmentId::parse(raw.to_string()).map_err(|error| anyhow::anyhow!("{error}"))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Resolve a caller selector to exactly one catalog project id through the
/// shared resolver engine (Selection class: unknown and ambiguous selectors
/// fail closed).
fn resolve_project_selection(
    store: &ProjectCatalogStore,
    selector: &str,
) -> anyhow::Result<ProjectId> {
    let state = store
        .snapshot()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let engine = ProjectResolverEngine::v2(state.catalog(), state.attachments());
    let request = ProjectSelectorRequest::selection(
        selector.to_string(),
        bbox_corpus_core::project_selector::ResolveIntent::Read,
    );
    let resolution = engine
        .resolve(&request)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let Some(project_id) = resolution.project_id() else {
        anyhow::bail!("error.project_selector_unknown: {selector} does not resolve to a project");
    };
    parse_project_id(project_id)
}

fn scope_json(scope: &ProjectScope) -> serde_json::Value {
    match scope {
        ProjectScope::Published(scope) => json!({
            "kind": "published",
            "repo_id": scope.repo_id(),
            "bbox_root_relpath": scope.bbox_root_relpath(),
        }),
        ProjectScope::LegacyLocal => json!({ "kind": "legacy_local" }),
    }
}

/// Daemon-observed checkout facts. Probing is filesystem work and runs only
/// inside the blocking phase.
struct CheckoutProbe {
    checkout_dir: PathBuf,
    checkout_project_dir: PathBuf,
    project_root_relpath: String,
    kind: AttachmentKind,
    validated_scope: Option<PublishedScope>,
    branch_ref: Option<String>,
    capabilities: AttachmentCapabilities,
    /// Aliases the committed config declares at `HEAD`, for the nomination
    /// ingestion of plan §7.6.
    declared_aliases: Vec<String>,
}

// Checkout probing is filesystem work by definition. It is reached only from
// the tools' `run_blocking` closures, never inline on a tokio worker.
#[allow(clippy::disallowed_methods)]
fn probe_checkout(raw_path: &str) -> anyhow::Result<CheckoutProbe> {
    let requested = Path::new(raw_path);
    if !requested.is_absolute() {
        anyhow::bail!("error.project_catalog_admin_path: path must be absolute");
    }
    let project_dir = std::fs::canonicalize(requested)
        .map_err(|error| anyhow::anyhow!("resolving {}: {error}", requested.display()))?;
    if !project_dir.is_dir() {
        anyhow::bail!("error.project_catalog_admin_path: path is not a directory");
    }

    let git_root = bbox_corpus_core::git::git_root_for_path(&project_dir)
        .and_then(|root| std::fs::canonicalize(root).ok());
    // Kind detection follows checkout shape, not caller assertion: a managed
    // clone carries the marker inside its `.git` directory, a linked worktree
    // has a `.git` FILE pointing into the base repository, everything else is
    // a base checkout (including non-git directories).
    let (checkout_dir, kind) = match &git_root {
        Some(root) => {
            let kind = if bbox_corpus_core::git::managed_checkout_root(root).is_some() {
                AttachmentKind::ManagedClone
            } else if bbox_corpus_core::git::linked_worktree_base(root).is_some() {
                AttachmentKind::Worktree
            } else {
                AttachmentKind::Base
            };
            (root.clone(), kind)
        }
        None => (project_dir.clone(), AttachmentKind::Base),
    };

    let project_root_relpath =
        bbox_corpus_core::identity::bbox_root_relpath(&checkout_dir, &project_dir).ok_or_else(
            || {
                anyhow::anyhow!(
                    "error.project_catalog_admin_path: {} is not inside its checkout top {}",
                    project_dir.display(),
                    checkout_dir.display()
                )
            },
        )?;

    // Committed authority resolved at HEAD. A checkout whose committed config
    // records no repo_id proves no scope, which is exactly what a legacy-local
    // attachment needs and what a published attachment is refused for.
    let committed = config::load_project_at_ref(&project_dir, "HEAD").ok();
    let declared_aliases = committed
        .as_ref()
        .map(|cfg| cfg.project.aliases.clone())
        .unwrap_or_default();
    let validated_scope = match (&git_root, committed.as_ref()) {
        (Some(root), Some(cfg)) => cfg.project.repo_id.as_ref().and_then(|repo_id| {
            let relpath = bbox_corpus_core::identity::bbox_root_relpath(root, &project_dir)?;
            PublishedScope::try_new(repo_id.clone(), relpath).ok()
        }),
        _ => None,
    };

    let is_git = git_root.is_some();
    let has_bbox_dir = project_dir.join(".bbox").is_dir();
    let capabilities = AttachmentCapabilities {
        local_code_source: true,
        git_history: is_git,
        blame: is_git,
        repo_knowledge: has_bbox_dir,
        repo_mutation: has_bbox_dir,
        render_output: true,
        provenance_note_io: is_git,
        artifact_watching: has_bbox_dir,
    };

    Ok(CheckoutProbe {
        branch_ref: bbox_corpus_core::git::current_branch(&checkout_dir),
        checkout_dir,
        checkout_project_dir: project_dir,
        project_root_relpath,
        kind,
        validated_scope,
        capabilities,
        declared_aliases,
    })
}

/// Read one JSON field from a small store-owned record. Returns `Ok(None)`
/// when the file is absent, and an error when it exists but cannot be read
/// or does not carry the field: a bridge generation is never invented.
#[allow(clippy::disallowed_methods)]
fn read_json_field(path: &Path, field: &str) -> anyhow::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("reading {}: {error}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| anyhow::anyhow!("parsing {}: {error}", path.display()))?;
    match value.get(field).and_then(|found| found.as_str()) {
        Some(found) => Ok(Some(found.to_string())),
        None => anyhow::bail!(
            "error.project_catalog_admin_bridge_generation: {} exists but carries no readable {field}; \
             use the offline project-catalog surface for this project",
            path.display()
        ),
    }
}

/// Active collected generation for the project, read from the code-source
/// activation record.
fn code_bridge_generation(
    state_dir: &Path,
    project_id: &ProjectId,
) -> anyhow::Result<Option<String>> {
    let root = state_dir.join("code-sources");
    let Ok(paths) = bbox_code_source_store::CodeSourceStorePaths::new(&root) else {
        return Ok(None);
    };
    read_json_field(&paths.activation(project_id), "generation_id")
}

fn accepted_publication_pointer(projects_path: &Path, project_id: &ProjectId) -> Option<PathBuf> {
    projects_path.parent().map(|parent| {
        parent
            .join("accepted-publications")
            .join("pointers")
            .join(format!("{project_id}.json"))
    })
}

/// Accepted publication generation for the project, read from the pointer.
fn publication_bridge_generation(
    projects_path: &Path,
    project_id: &ProjectId,
) -> anyhow::Result<Option<String>> {
    let Some(pointer) = accepted_publication_pointer(projects_path, project_id) else {
        return Ok(None);
    };
    read_json_field(&pointer, "accepted_generation")
}

/// Committed scope every active attachment of the project currently proves,
/// probed off-lock. Unreadable checkouts contribute `None`, which the domain
/// layer treats as a refusal rather than agreement.
fn probe_attachment_scopes(
    store: &ProjectCatalogStore,
    project_id: &ProjectId,
) -> anyhow::Result<std::collections::BTreeMap<AttachmentId, Option<PublishedScope>>> {
    let state = store
        .snapshot()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut scopes = std::collections::BTreeMap::new();
    for row in state.attachments().attachments.values() {
        if &row.project_id != project_id || row.status != AttachmentStatus::Attached {
            continue;
        }
        let probed = probe_checkout(&row.checkout_project_dir)
            .ok()
            .and_then(|probe| probe.validated_scope);
        scopes.insert(row.attachment_id.clone(), probed);
    }
    Ok(scopes)
}

// ── Tools ───────────────────────────────────────────────────────────────

#[tool_router(router = project_catalog_tools)]
impl BlackboxServer {
    fn catalog_store(&self) -> Option<Arc<ProjectCatalogStore>> {
        self.state.project_authority.catalog_store().cloned()
    }

    fn catalog_paths(&self) -> (PathBuf, PathBuf) {
        let config = self.state.config.read();
        (
            config.paths.projects_path.clone(),
            config.paths.state_dir.clone(),
        )
    }

    #[tool(
        name = "bbox_project_catalog_list",
        description = "List every project in the durable catalog, including remote-only projects with no attachment on this host. Path-free rows: project_id, display_name, scope (published repo_id + bbox_root_relpath, or legacy_local), operator and nominated aliases, and the count of active attachments. Returns the catalog epoch to pass as expected_catalog_epoch on a following administration call. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
    )]
    pub(crate) async fn bbox_project_catalog_list(
        &self,
        Parameters(_p): Parameters<CatalogListParams>,
    ) -> CallToolResult {
        let Some(store) = self.catalog_store() else {
            return Self::err_text(&catalog_inactive());
        };
        Self::run_blocking("bbox_project_catalog_list", move || {
            let state = store
                .snapshot()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let projects: Vec<serde_json::Value> = state
                .catalog()
                .projects
                .values()
                .map(|project| {
                    let active = state
                        .attachments()
                        .attachments
                        .values()
                        .filter(|row| {
                            row.project_id == project.project_id
                                && row.status == AttachmentStatus::Attached
                        })
                        .count();
                    json!({
                        "project_id": project.project_id.as_str(),
                        "display_name": project.display_name,
                        "scope": scope_json(&project.scope),
                        "operator_aliases": project.operator_aliases,
                        "nominated_aliases": project.nominated_aliases,
                        "active_attachments": active,
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&json!({
                "epoch": state.epoch(),
                "projects": projects,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_catalog_get",
        description = "Read one catalog project: its id, display name, scope, aliases, pending alias nominations, and repo-history reference, plus a separate host_local_attachments section carrying this host's attachment rows (attachment_id, status, kind, checkout dir, relpath). The catalog section stays path-free; attachment paths are host-local operator data. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
    )]
    pub(crate) async fn bbox_project_catalog_get(
        &self,
        Parameters(p): Parameters<CatalogGetParams>,
    ) -> CallToolResult {
        let Some(store) = self.catalog_store() else {
            return Self::err_text(&catalog_inactive());
        };
        Self::run_blocking("bbox_project_catalog_get", move || {
            let project_id = resolve_project_selection(&store, &p.project)?;
            let state = store.snapshot().map_err(|error| anyhow::anyhow!("{error}"))?;
            let Some(project) = state.catalog().projects.get(&project_id) else {
                anyhow::bail!(
                    "error.project_catalog_admin_unknown_project: {project_id} is not in the catalog"
                );
            };
            let attachments: Vec<serde_json::Value> = state
                .attachments()
                .attachments
                .values()
                .filter(|row| row.project_id == project_id)
                .map(|row| {
                    json!({
                        "attachment_id": row.attachment_id.as_str(),
                        "status": row.status,
                        "kind": row.kind,
                        "checkout_id": row.checkout_id,
                        "checkout_project_dir": row.checkout_project_dir,
                        "project_root_relpath": row.project_root_relpath,
                        "capabilities": row.capabilities,
                    })
                })
                .collect();
            let default_attachment = state
                .attachments()
                .default_attachments
                .get(&project_id)
                .map(|id| id.as_str().to_string());
            Ok(serde_json::to_string_pretty(&json!({
                "epoch": state.epoch(),
                "project": {
                    "project_id": project.project_id.as_str(),
                    "display_name": project.display_name,
                    "scope": scope_json(&project.scope),
                    "operator_aliases": project.operator_aliases,
                    "nominated_aliases": project.nominated_aliases,
                    "repo_history": project.repo_history.as_ref().map(|id| id.as_str()),
                },
                "host_local_attachments": attachments,
                "default_attachment": default_attachment,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_attach",
        description = "Attach a local checkout to an existing catalog project. The daemon probes the path off-lock (canonical checkout top, checkout identity, kind: base, linked worktree, or managed clone, committed scope at HEAD, observed capabilities) and the catalog transaction revalidates identity and uniqueness. A published project accepts only a checkout whose committed config proves the same scope exactly; a mismatch returns the scope-migration or promotion refusal instead of attaching. Well-formed, non-colliding aliases declared by the committed config are recorded as pending nominations, never accepted automatically. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
    )]
    pub(crate) async fn bbox_project_attach(
        &self,
        Parameters(p): Parameters<ProjectAttachParams>,
    ) -> CallToolResult {
        let Some(store) = self.catalog_store() else {
            return Self::err_text(&catalog_inactive());
        };
        Self::run_blocking("bbox_project_attach", move || {
            let _audit_reason = bounded_audit_reason(&p.audit_reason)?;
            let probe = probe_checkout(&p.path)?;
            let project_id = resolve_project_selection(&store, &p.project)?;
            let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&probe.checkout_dir)?;
            let attach_probe = project_catalog_admin::AttachProbe {
                checkout_id,
                checkout_dir: probe.checkout_dir.to_string_lossy().into_owned(),
                checkout_project_dir: probe.checkout_project_dir.to_string_lossy().into_owned(),
                project_root_relpath: probe.project_root_relpath.clone(),
                kind: probe.kind,
                validated_scope: probe.validated_scope.clone(),
                // The bootstrap hint stays absent here: attach proves scope
                // from committed authority, and a computed hint is only a
                // pre-recorded-authority bootstrap value.
                computed_repo_hint: None,
                branch_ref: probe.branch_ref.clone(),
                capabilities: probe.capabilities,
                attached_at: now_rfc3339(),
            };
            let receipt = project_catalog_admin::attach_checkout(
                &store,
                p.expected_catalog_epoch,
                &project_id,
                &attach_probe,
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;

            // Nomination ingestion (plan §7.6) is a separate epoch-bumping
            // transaction: a refused or empty nomination set never fails the
            // attachment that already committed.
            let nominated = ingest_alias_nominations(
                &store,
                receipt.commit.epoch,
                &project_id,
                &probe.declared_aliases,
            );

            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "project_id": project_id.as_str(),
                "attachment_id": receipt.attachment_id.as_str(),
                "kind": attach_probe.kind,
                "checkout_project_dir": attach_probe.checkout_project_dir,
                "project_root_relpath": attach_probe.project_root_relpath,
                "epoch": nominated.epoch.unwrap_or(receipt.commit.epoch),
                "catalog_sha256": receipt.commit.catalog_sha256,
                "attachments_sha256": receipt.commit.attachments_sha256,
                "nominated_aliases": nominated.recorded,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_detach",
        description = "Detach one attachment: the row is marked detached with a timestamp, every logical store, entity ref, and generation is left untouched, and the catalog keeps its data. Census and watcher deregistration is scoped to the detached attachment's checkout and scope pair only, so a monorepo checkout carrying sibling attachments for other projects keeps their census rows and watcher coverage. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
    )]
    pub(crate) async fn bbox_project_detach(
        &self,
        Parameters(p): Parameters<ProjectDetachParams>,
    ) -> CallToolResult {
        let Some(store) = self.catalog_store() else {
            return Self::err_text(&catalog_inactive());
        };
        let server = self.clone();
        Self::run_blocking("bbox_project_detach", move || {
            let _audit_reason = bounded_audit_reason(&p.audit_reason)?;
            let attachment_id = parse_attachment_id(&p.attachment_id)?;
            // Read the row before the transaction: detach clears the
            // capability bits and the pair keys are needed afterwards.
            let state = store.snapshot().map_err(|error| anyhow::anyhow!("{error}"))?;
            let Some(row) = state.attachments().attachments.get(&attachment_id).cloned() else {
                anyhow::bail!(
                    "error.project_catalog_admin_unknown_attachment: {attachment_id} is not in the store"
                );
            };
            let commit = project_catalog_admin::detach_attachment(
                &store,
                p.expected_catalog_epoch,
                &attachment_id,
                &now_rfc3339(),
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;

            let census_removed = server.deregister_detached_pair(&row);

            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "attachment_id": attachment_id.as_str(),
                "project_id": row.project_id.as_str(),
                "checkout_id": row.checkout_id,
                "census_row_removed": census_removed,
                "epoch": commit.epoch,
                "catalog_sha256": commit.catalog_sha256,
                "attachments_sha256": commit.attachments_sha256,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_default_attachment",
        description = "Record or clear the operator-selected default local-source attachment for one project. Path operations use it when no session pin and no explicit selector is present. The selection is host-local attachment data, never catalog data; it must name an active attachment of the same project, and omitting attachment_id clears it. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
    )]
    pub(crate) async fn bbox_project_default_attachment(
        &self,
        Parameters(p): Parameters<ProjectDefaultAttachmentParams>,
    ) -> CallToolResult {
        let Some(store) = self.catalog_store() else {
            return Self::err_text(&catalog_inactive());
        };
        Self::run_blocking("bbox_project_default_attachment", move || {
            let _audit_reason = bounded_audit_reason(&p.audit_reason)?;
            let project_id = resolve_project_selection(&store, &p.project)?;
            let selection = p
                .attachment_id
                .as_deref()
                .map(parse_attachment_id)
                .transpose()?;
            let commit = project_catalog_admin::set_default_attachment(
                &store,
                p.expected_catalog_epoch,
                &project_id,
                selection.as_ref(),
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "project_id": project_id.as_str(),
                "default_attachment": selection.as_ref().map(|id| id.as_str()),
                "epoch": commit.epoch,
                "catalog_sha256": commit.catalog_sha256,
                "attachments_sha256": commit.attachments_sha256,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_promote",
        description = "Promote a legacy-local catalog project to the published scope its checkouts now prove. Requires the exact project_id, the designated attachment, and the proposed repo_id and bbox_root_relpath. The daemon probes every active attachment of the project at HEAD; each one must prove the exact proposed scope or the promotion refuses with per-attachment diagnostics, and the designated attachment cannot overrule siblings. An owned scope refuses and points at the offline compatibility workflow rather than merging. One pair transaction flips the scope, writes the attachment-proved promotion record with its proof, and performs the repo-history authority transition. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
    )]
    pub(crate) async fn bbox_project_promote(
        &self,
        Parameters(p): Parameters<ProjectPromoteParams>,
    ) -> CallToolResult {
        let Some(store) = self.catalog_store() else {
            return Self::err_text(&catalog_inactive());
        };
        let (projects_path, state_dir) = self.catalog_paths();
        Self::run_blocking("bbox_project_promote", move || {
            let audit_reason = bounded_audit_reason(&p.audit_reason)?;
            let project_id = parse_project_id(&p.project_id)?;
            let attachment_id = parse_attachment_id(&p.attachment_id)?;
            let proposed_scope =
                PublishedScope::try_new(p.proposed_repo_id.clone(), p.proposed_relpath.clone())
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
            let evidence = project_catalog_admin::PromotionEvidence {
                attachment_scopes: probe_attachment_scopes(&store, &project_id)?,
                code_bridge_generation: code_bridge_generation(&state_dir, &project_id)?,
                publication_bridge_generation: publication_bridge_generation(
                    &projects_path,
                    &project_id,
                )?,
                operator_invocation: "mcp:bbox_project_promote".to_string(),
                operator_reason: Some(audit_reason),
                proved_at: now_rfc3339(),
            };
            let receipt = project_catalog_admin::promote_project(
                &store,
                p.expected_catalog_epoch,
                &project_id,
                &attachment_id,
                &proposed_scope,
                &evidence,
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "project_id": project_id.as_str(),
                "scope_migration_id": receipt.scope_migration_id.as_str(),
                "scope": {
                    "repo_id": proposed_scope.repo_id(),
                    "bbox_root_relpath": proposed_scope.bbox_root_relpath(),
                },
                "code_bridge_generation": evidence.code_bridge_generation,
                "publication_bridge_generation": evidence.publication_bridge_generation,
                "epoch": receipt.commit.epoch,
                "catalog_sha256": receipt.commit.catalog_sha256,
                "attachments_sha256": receipt.commit.attachments_sha256,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_scope_migrate",
        description = "Attachment-proved scope migration for a published catalog project: kind=relpath-move for a monorepo relocation, kind=repo-authority-change for a recorded-authority change. The daemon probes every active attachment at HEAD (and, for a relpath move, the relocated directory, which must exist) and the pair transaction rewrites the catalog scope, relocates the attachments, appends host-local path bindings, and writes the migration record with its proof. A repo-authority change requires acknowledge_repo_authority_change, which agents pass through from operator input and never default or infer. dry_run validates the complete mutation and commits nothing. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
    )]
    pub(crate) async fn bbox_project_scope_migrate(
        &self,
        Parameters(p): Parameters<ProjectScopeMigrateParams>,
    ) -> CallToolResult {
        let Some(store) = self.catalog_store() else {
            return Self::err_text(&catalog_inactive());
        };
        let (projects_path, state_dir) = self.catalog_paths();
        Self::run_blocking("bbox_project_scope_migrate", move || {
            let audit_reason = bounded_audit_reason(&p.audit_reason)?;
            let project_id = parse_project_id(&p.project_id)?;
            let designated_attachment = parse_attachment_id(&p.attachment_id)?;
            let kind = match p.kind.as_str() {
                "relpath-move" => ScopeMigrationKind::RelpathMove,
                "repo-authority-change" => ScopeMigrationKind::RepoAuthorityChange,
                other => anyhow::bail!(
                    "error.project_catalog_admin_migration_kind: unsupported migration kind {other}"
                ),
            };
            let expected_old_scope = PublishedScope::try_new(
                p.expected_old_repo_id.clone(),
                p.expected_old_relpath.clone(),
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
            let new_scope = PublishedScope::try_new(p.new_repo_id.clone(), p.new_relpath.clone())
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let attachment_probes =
                probe_migration_attachments(&store, &project_id, &kind, &new_scope)?;
            let request = project_catalog_admin::ScopeMigrationRequest {
                project_id: project_id.clone(),
                expected_old_scope,
                new_scope: new_scope.clone(),
                kind,
                designated_attachment,
                acknowledge_repo_authority_change: p.acknowledge_repo_authority_change,
                attachment_probes,
                code_bridge_generation: code_bridge_generation(&state_dir, &project_id)?,
                publication_bridge_generation: publication_bridge_generation(
                    &projects_path,
                    &project_id,
                )?,
                operator_invocation: "mcp:bbox_project_scope_migrate".to_string(),
                operator_reason: Some(audit_reason),
                migrated_at: now_rfc3339(),
            };
            let receipt = project_catalog_admin::scope_migrate_attached(
                &store,
                p.expected_catalog_epoch,
                &request,
                p.dry_run,
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "project_id": project_id.as_str(),
                "dry_run": p.dry_run,
                "scope": {
                    "repo_id": new_scope.repo_id(),
                    "bbox_root_relpath": new_scope.bbox_root_relpath(),
                },
                "scope_migration_id": receipt
                    .as_ref()
                    .map(|receipt| receipt.scope_migration_id.as_str()),
                "epoch": receipt.as_ref().map(|receipt| receipt.commit.epoch),
                "catalog_sha256": receipt
                    .as_ref()
                    .map(|receipt| receipt.commit.catalog_sha256.clone()),
                "attachments_sha256": receipt
                    .as_ref()
                    .map(|receipt| receipt.commit.attachments_sha256.clone()),
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_publisher_bind",
        description = "Rebind the accepted-publication pointer of a published project to another of its attachments. The pointer's ref, accepted commit, accepted scope, generation, and payload bytes are unchanged: only the attachment binding moves, so the strict pointer and generation agreement holds identically before and after. The new attachment's object database must already contain the pointer's accepted commit, and a project with no pointer refuses rather than inventing one. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
    )]
    pub(crate) async fn bbox_project_publisher_bind(
        &self,
        Parameters(p): Parameters<ProjectPublisherBindParams>,
    ) -> CallToolResult {
        let Some(store) = self.catalog_store() else {
            return Self::err_text(&catalog_inactive());
        };
        let (projects_path, _state_dir) = self.catalog_paths();
        Self::run_blocking("bbox_project_publisher_bind", move || {
            let _audit_reason = bounded_audit_reason(&p.audit_reason)?;
            let project_id = parse_project_id(&p.project_id)?;
            let attachment_id = parse_attachment_id(&p.attachment_id)?;
            let state = store.snapshot().map_err(|error| anyhow::anyhow!("{error}"))?;
            if state.epoch() != p.expected_catalog_epoch {
                anyhow::bail!(
                    "error.project_catalog_stale_epoch: expected epoch {} does not match the current catalog epoch {}",
                    p.expected_catalog_epoch,
                    state.epoch()
                );
            }
            let Some(row) = state.attachments().attachments.get(&attachment_id).cloned() else {
                anyhow::bail!(
                    "error.project_catalog_admin_unknown_attachment: {attachment_id} is not in the store"
                );
            };
            let Some(accepted_commit) = accepted_commit_for(&projects_path, &project_id)? else {
                anyhow::bail!(
                    "error.project_catalog_admin_pointer_missing: project {project_id} has no accepted \
                     publication pointer; the migration recorded no published content, and a pointer \
                     without its generation is not representable"
                );
            };
            let probe = project_catalog_admin::PublisherBindProbe {
                accepted_commit_present: commit_present_in_checkout(
                    &accepted_commit,
                    Path::new(&row.checkout_project_dir),
                ),
            };
            let receipt = project_catalog_admin::bind_publisher_attachment(
                &store,
                &projects_path,
                &project_id,
                &attachment_id,
                &probe,
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "project_id": project_id.as_str(),
                "attachment_id": receipt.attachment_id.as_str(),
                "epoch": state.epoch(),
            }))?)
        })
        .await
    }

    /// Deregister exactly the detached attachment's (checkout, scope) census
    /// row and its paired watcher carrier. Sibling attachments in the same
    /// checkout keep their rows, coverage, and overlay discovery: the census
    /// key is composite for exactly this reason (plan §7.3).
    fn deregister_detached_pair(
        &self,
        row: &bbox_corpus_core::project_catalog::CheckoutAttachment,
    ) -> bool {
        let census_removed = match row.validated_scope.as_ref() {
            Some(scope) => {
                let lifecycle = match self.state.checkout_access.lifecycle_mutation_guard() {
                    Ok(guard) => guard,
                    Err(error) => {
                        tracing::warn!(
                            attachment = %row.attachment_id,
                            error = %error,
                            "detach could not take the lifecycle guard for census deregistration"
                        );
                        return false;
                    }
                };
                let removed = self
                    .state
                    .checkout_registry
                    .write()
                    .deregister_scope(&row.checkout_id, scope);
                drop(lifecycle);
                match removed {
                    Ok(removed) => removed,
                    Err(error) => {
                        tracing::warn!(
                            attachment = %row.attachment_id,
                            error = %error,
                            "detach could not remove the census row for this attachment pair"
                        );
                        false
                    }
                }
            }
            // A scope-less attachment owns no scope-keyed census row.
            None => false,
        };
        if let Ok(mut guard) = self.state.bbox_watcher.lock()
            && let Some(watcher) = guard.as_mut()
            && let Ok(carrier) = crate::watcher::ArtifactWatchCarrier::checkout(
                row.project_id.as_str().to_string(),
                row.checkout_id.clone(),
            )
            && let Err(error) = watcher.unwatch_carrier(&carrier)
        {
            tracing::warn!(
                attachment = %row.attachment_id,
                checkout_id = %row.checkout_id,
                error = %error,
                "detach could not remove the paired checkout watcher registration"
            );
        }
        census_removed
    }
}

struct NominationOutcome {
    recorded: Vec<String>,
    epoch: Option<u64>,
}

/// Mirror of the catalog snapshot's alias rule, applied before the
/// nomination transaction so one malformed declaration cannot fail the
/// whole batch at commit-time validation.
fn well_formed_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 96
        && alias.trim() == alias
        && !matches!(alias, "." | "..")
        && !alias.contains(['/', '\\', '%'])
        && !alias.chars().any(char::is_whitespace)
        && !alias.bytes().any(|byte| byte.is_ascii_control())
}

/// Record well-formed, non-colliding committed aliases as pending
/// nominations (plan §7.6). Acceptance stays an offline operator action, so
/// nothing here grants selector authority. Collisions and malformed entries
/// are skipped with a warning rather than failing the committed attachment.
fn ingest_alias_nominations(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    project_id: &ProjectId,
    declared: &[String],
) -> NominationOutcome {
    let nothing = || NominationOutcome {
        recorded: Vec::new(),
        epoch: None,
    };
    if declared.is_empty() {
        return nothing();
    }
    let recorded = std::sync::Mutex::new(Vec::<String>::new());
    let project_id = project_id.clone();
    let declared = declared.to_vec();
    let commit = store.transact(expected_epoch, |catalog, _attachments| {
        let taken: std::collections::BTreeSet<String> = catalog
            .projects
            .values()
            .flat_map(|project| {
                project
                    .operator_aliases
                    .iter()
                    .cloned()
                    .chain(std::iter::once(project.project_id.as_str().to_string()))
            })
            .collect();
        let Some(project) = catalog.projects.get_mut(&project_id) else {
            return Ok(());
        };
        let mut accepted = recorded.lock().expect("nomination buffer is not poisoned");
        for alias in &declared {
            if !well_formed_alias(alias)
                || taken.contains(alias)
                || project.nominated_aliases.contains(alias)
            {
                continue;
            }
            project.nominated_aliases.insert(alias.clone());
            accepted.push(alias.clone());
        }
        Ok(())
    });
    match commit {
        Ok(commit) => NominationOutcome {
            recorded: recorded.into_inner().unwrap_or_default(),
            epoch: Some(commit.epoch),
        },
        Err(error) => {
            tracing::warn!(
                project = %project_id,
                error = %error,
                "attach could not record committed alias nominations"
            );
            nothing()
        }
    }
}

/// Probe each active attachment for a scope migration: the scope its
/// committed config now resolves plus, for a relpath move, the relocated
/// project directory (which must exist on disk).
fn probe_migration_attachments(
    store: &ProjectCatalogStore,
    project_id: &ProjectId,
    kind: &ScopeMigrationKind,
    new_scope: &PublishedScope,
) -> anyhow::Result<
    std::collections::BTreeMap<AttachmentId, project_catalog_admin::MigrationAttachmentProbe>,
> {
    let state = store
        .snapshot()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut probes = std::collections::BTreeMap::new();
    for row in state.attachments().attachments.values() {
        if &row.project_id != project_id || row.status != AttachmentStatus::Attached {
            continue;
        }
        let (new_project_root_relpath, new_checkout_project_dir) = match kind {
            ScopeMigrationKind::RelpathMove => {
                let relpath = new_scope.bbox_root_relpath().to_string();
                let relocated = relocated_project_dir(&row.checkout_dir, &relpath)?;
                (relpath, relocated)
            }
            // A recorded-authority change relocates nothing.
            _ => (
                row.project_root_relpath.clone(),
                row.checkout_project_dir.clone(),
            ),
        };
        let resolved_scope = probe_checkout(&new_checkout_project_dir)
            .ok()
            .and_then(|probe| probe.validated_scope);
        probes.insert(
            row.attachment_id.clone(),
            project_catalog_admin::MigrationAttachmentProbe {
                resolved_scope,
                new_project_root_relpath,
                new_checkout_project_dir,
            },
        );
    }
    Ok(probes)
}

// Relocation probing is filesystem work reached only from the tools'
// `run_blocking` closures.
#[allow(clippy::disallowed_methods)]
fn relocated_project_dir(checkout_dir: &str, relpath: &str) -> anyhow::Result<String> {
    let base = Path::new(checkout_dir);
    let candidate = if relpath == "." {
        base.to_path_buf()
    } else {
        base.join(relpath)
    };
    let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
        anyhow::anyhow!(
            "error.project_catalog_admin_path: relocated project dir {} is unreadable: {error}",
            candidate.display()
        )
    })?;
    if !canonical.is_dir() {
        anyhow::bail!(
            "error.project_catalog_admin_path: relocated project dir {} is not a directory",
            canonical.display()
        );
    }
    Ok(canonical.to_string_lossy().into_owned())
}

/// The accepted commit the project's publication pointer currently names.
/// `None` means the project has no pointer at all.
fn accepted_commit_for(
    projects_path: &Path,
    project_id: &ProjectId,
) -> anyhow::Result<Option<String>> {
    let Some(pointer) = accepted_publication_pointer(projects_path, project_id) else {
        return Ok(None);
    };
    read_json_field(&pointer, "accepted_commit")
}

/// Containment check for the publisher rebind: the pointer's accepted commit
/// must already be an object of the new attachment's repository.
fn commit_present_in_checkout(accepted_commit: &str, checkout_project_dir: &Path) -> bool {
    let root = bbox_corpus_core::git::git_root_for_path(checkout_project_dir)
        .unwrap_or_else(|| checkout_project_dir.to_path_buf());
    bbox_corpus_core::git::verify_commit_oid_with_alternate(&root, accepted_commit, None).is_ok()
}

#[cfg(test)]
// Test fixtures build throwaway checkout directories directly; the handler
// bodies keep routing their filesystem work through the blocking pool.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::server::state::SharedState;

    fn bridge_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(std::sync::Arc::new(SharedState::for_test(tmp.path())))
    }

    fn error_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| content.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn every_catalog_tool_refuses_on_the_version_one_bridge() {
        let tmp = tempfile::tempdir().unwrap();
        let server = bridge_server(&tmp);
        let expected = catalog_inactive();

        let results = vec![
            server
                .bbox_project_catalog_list(Parameters(CatalogListParams {}))
                .await,
            server
                .bbox_project_catalog_get(Parameters(CatalogGetParams {
                    project: "p_00000000000000000000000000000000".into(),
                }))
                .await,
            server
                .bbox_project_attach(Parameters(ProjectAttachParams {
                    project: "p_00000000000000000000000000000000".into(),
                    path: tmp.path().to_string_lossy().into_owned(),
                    expected_catalog_epoch: 1,
                    audit_reason: "bridge refusal".into(),
                }))
                .await,
            server
                .bbox_project_detach(Parameters(ProjectDetachParams {
                    attachment_id: "att_00000000000000000000000000000000".into(),
                    expected_catalog_epoch: 1,
                    audit_reason: "bridge refusal".into(),
                }))
                .await,
            server
                .bbox_project_default_attachment(Parameters(ProjectDefaultAttachmentParams {
                    project: "p_00000000000000000000000000000000".into(),
                    attachment_id: None,
                    expected_catalog_epoch: 1,
                    audit_reason: "bridge refusal".into(),
                }))
                .await,
            server
                .bbox_project_promote(Parameters(ProjectPromoteParams {
                    project_id: "p_00000000000000000000000000000000".into(),
                    attachment_id: "att_00000000000000000000000000000000".into(),
                    proposed_repo_id: "repo".into(),
                    proposed_relpath: ".".into(),
                    expected_catalog_epoch: 1,
                    audit_reason: "bridge refusal".into(),
                }))
                .await,
            server
                .bbox_project_scope_migrate(Parameters(ProjectScopeMigrateParams {
                    project_id: "p_00000000000000000000000000000000".into(),
                    expected_old_repo_id: "repo".into(),
                    expected_old_relpath: ".".into(),
                    new_repo_id: "repo".into(),
                    new_relpath: "services/api".into(),
                    kind: "relpath-move".into(),
                    attachment_id: "att_00000000000000000000000000000000".into(),
                    acknowledge_repo_authority_change: false,
                    dry_run: true,
                    expected_catalog_epoch: 1,
                    audit_reason: "bridge refusal".into(),
                }))
                .await,
            server
                .bbox_project_publisher_bind(Parameters(ProjectPublisherBindParams {
                    project_id: "p_00000000000000000000000000000000".into(),
                    attachment_id: "att_00000000000000000000000000000000".into(),
                    expected_catalog_epoch: 1,
                    audit_reason: "bridge refusal".into(),
                }))
                .await,
        ];

        for result in &results {
            let text = error_text(result);
            assert!(
                text.contains("error.project_catalog_inactive"),
                "bridge mode must refuse every catalog admin tool; got {text}"
            );
            assert_eq!(text, expected, "refusal text must be the shared one");
        }
    }

    #[test]
    fn audit_reason_is_bounded_and_required() {
        assert!(bounded_audit_reason("  ").is_err());
        assert!(bounded_audit_reason(&"x".repeat(MAX_AUDIT_REASON_BYTES + 1)).is_err());
        assert_eq!(
            bounded_audit_reason("  relocating the monorepo root  ").unwrap(),
            "relocating the monorepo root"
        );
    }

    #[test]
    fn project_catalog_probe_detects_a_non_git_base_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("plain");
        std::fs::create_dir_all(&project).unwrap();

        let probe = probe_checkout(project.to_str().unwrap()).unwrap();
        assert_eq!(probe.kind, AttachmentKind::Base);
        assert_eq!(probe.project_root_relpath, ".");
        assert_eq!(probe.checkout_dir, project);
        assert!(probe.validated_scope.is_none());
        assert!(probe.capabilities.local_code_source);
        assert!(!probe.capabilities.git_history);
        assert!(!probe.capabilities.repo_knowledge);
    }

    #[test]
    fn project_catalog_probe_rejects_relative_and_missing_paths() {
        assert!(probe_checkout("relative/path").is_err());
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("absent");
        assert!(probe_checkout(missing.to_str().unwrap()).is_err());
    }
}
