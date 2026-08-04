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
use bbox_indexing::accepted_publication_runtime::{
    AcceptedPublicationScopeAgreement, AcceptedPublicationState, PublishSourceFile, PublishSources,
    PublisherPublishMode,
};
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
pub(crate) struct ProjectPublisherAdvanceParams {
    pub project_id: String,
    /// The attachment whose checkout carries the ref being published.
    pub attachment_id: String,
    /// `establish` for a project's first pointer, `advance` to move one.
    pub mode: String,
    /// Fully qualified publisher ref, for example `refs/heads/main`.
    pub full_ref: String,
    /// Advance only: the generation id the caller expects to replace.
    #[serde(default)]
    pub expected_generation_id: Option<String>,
    /// Advance only: the SHA-256 of the pointer the caller expects to
    /// replace.
    #[serde(default)]
    pub expected_pointer_sha256: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    pub expected_catalog_epoch: u64,
    pub audit_reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectPublisherStatusParams {
    pub project_id: String,
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

/// What catalog attachment authority knows about an absolute checkout path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CatalogPathAuthority {
    /// One active attachment resolves the path to this project.
    Attached(String),
    /// Catalog attachment rows govern this path, but no ACTIVE attachment
    /// resolves it: detached, ambiguous, or otherwise unusable. Not a
    /// bootstrap case, and not something a lease can be named for either.
    Governed { diagnostic: String },
    /// Catalog authority could not be read, so nothing about the path is
    /// proved. Not a bootstrap case.
    Unreadable { code: String, diagnostic: String },
    /// No attachment row at any status names or contains this path. This is
    /// the plan 4.19 bootstrap case and the ONLY one.
    Absent,
}

/// Whether any attachment row, at ANY status, names or contains the path.
///
/// Containment counts deliberately: a subdirectory of an attached checkout
/// is governed by that attachment even though no row names it exactly, and
/// scaffolding it lease-free would write inside a governed tree.
fn attachment_row_governing(
    state: &bbox_indexing::project_catalog_store::ProjectCatalogState,
    canonical_path: &str,
) -> Option<String> {
    let candidate = Path::new(canonical_path);
    state
        .attachments()
        .attachments
        .values()
        .find(|attachment| {
            [&attachment.checkout_dir, &attachment.checkout_project_dir]
                .into_iter()
                .any(|root| candidate.starts_with(Path::new(root)))
        })
        .map(|attachment| {
            format!(
                "path is governed by attachment {} of project {} (status {:?})",
                attachment.attachment_id.as_str(),
                attachment.project_id.as_str(),
                attachment.status,
            )
        })
}

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
            let alias_accept_commands =
                alias_accept_commands(&project.project_id, state.epoch(), &project.nominated_aliases);
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
                "alias_accept_commands": alias_accept_commands,
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
            let audit_reason = bounded_audit_reason(&p.audit_reason)?;
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

            // A recorded nomination is only a pending one: the response
            // hands the operator the exact epoch-checked acceptance command
            // for the epoch this attachment just published (plan §7.6).
            let epoch = nominated.epoch.unwrap_or(receipt.commit.epoch);
            let accept_commands = alias_accept_commands(&project_id, epoch, &nominated.recorded);

            // The bounded reason is `operator_invocation`-class data: it is
            // audited in the log line and the response, never duplicated into
            // a parallel audit store (plan §7.1, D-012).
            tracing::info!(
                tool = "bbox_project_attach",
                project_id = %project_id,
                attachment_id = %receipt.attachment_id,
                audit_reason = %audit_reason,
                "catalog administration mutation"
            );

            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "project_id": project_id.as_str(),
                "attachment_id": receipt.attachment_id.as_str(),
                "audit_reason": audit_reason,
                "kind": attach_probe.kind,
                "checkout_project_dir": attach_probe.checkout_project_dir,
                "project_root_relpath": attach_probe.project_root_relpath,
                "epoch": epoch,
                "catalog_sha256": receipt.commit.catalog_sha256,
                "attachments_sha256": receipt.commit.attachments_sha256,
                "nominated_aliases": nominated.recorded,
                "alias_accept_commands": accept_commands,
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
            let audit_reason = bounded_audit_reason(&p.audit_reason)?;
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

            tracing::info!(
                tool = "bbox_project_detach",
                project_id = %row.project_id,
                attachment_id = %attachment_id,
                audit_reason = %audit_reason,
                "catalog administration mutation"
            );

            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "attachment_id": attachment_id.as_str(),
                "project_id": row.project_id.as_str(),
                "checkout_id": row.checkout_id,
                "audit_reason": audit_reason,
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
            let audit_reason = bounded_audit_reason(&p.audit_reason)?;
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
            tracing::info!(
                tool = "bbox_project_default_attachment",
                project_id = %project_id,
                attachment_id = selection.as_ref().map(|id| id.as_str()).unwrap_or("cleared"),
                audit_reason = %audit_reason,
                "catalog administration mutation"
            );
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "project_id": project_id.as_str(),
                "default_attachment": selection.as_ref().map(|id| id.as_str()),
                "audit_reason": audit_reason,
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
        let bound_project = match parse_project_id(&p.project_id) {
            Ok(project_id) => project_id,
            Err(error) => return Self::err_text(&format!("Error: {error}")),
        };
        let result = Self::run_blocking("bbox_project_publisher_bind", move || {
            let audit_reason = bounded_audit_reason(&p.audit_reason)?;
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
            // The domain op revalidates the epoch and the attachment's
            // Attached status inside the publication-lock critical section
            // (real CAS); the snapshot above only shapes the early typed
            // refusals and the probe.
            let receipt = project_catalog_admin::bind_publisher_attachment(
                &store,
                &projects_path,
                p.expected_catalog_epoch,
                &project_id,
                &attachment_id,
                &probe,
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
            // Receipt carries the pointer content hash (plan §7.7) read
            // back from the just-rebound pointer file.
            let pointer_sha256 =
                pointer_content_sha256(&projects_path, &project_id);
            tracing::info!(
                tool = "bbox_project_publisher_bind",
                project_id = %project_id,
                attachment_id = %receipt.attachment_id,
                audit_reason = %audit_reason,
                "catalog administration mutation"
            );
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "project_id": project_id.as_str(),
                "attachment_id": receipt.attachment_id.as_str(),
                "audit_reason": audit_reason,
                "epoch": receipt.catalog_epoch,
                "pointer_sha256": pointer_sha256,
            }))?)
        })
        .await;
        if !result.is_error.unwrap_or(false) {
            // Rebind moves binding identity only. Accepted content is
            // byte-identical across it, so the projected caches keyed by
            // content identity must survive (plan section 12).
            if let Some(runtime) = &self.state.accepted_publications {
                runtime.invalidate_binding(&bound_project);
            }
        }
        result
    }

    #[tool(
        name = "bbox_project_publisher_advance",
        description = "Establish or advance one published project's accepted publication. mode=establish creates the project's first pointer and requires that no pointer exists; mode=advance moves an existing pointer and requires expected_generation_id and expected_pointer_sha256, which bbox_project_publisher_status returns. The named attachment must be attached, carry the catalog's current published scope, and hold the repo_knowledge capability. The daemon resolves full_ref in that checkout, reads the committed project identity and both source lanes at that commit, validates knowledge and gaps into one immutable generation, and swaps the pointer only after rechecking the catalog epoch, the attachment, and the ref. Publishing always uses the catalog's CURRENT scope, which is what clears a scope-migration bridge. dry_run validates and writes nothing. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
    )]
    pub(crate) async fn bbox_project_publisher_advance(
        &self,
        Parameters(p): Parameters<ProjectPublisherAdvanceParams>,
    ) -> CallToolResult {
        let Some(store) = self.catalog_store() else {
            return Self::err_text(&catalog_inactive());
        };
        let Some(runtime) = self.state.accepted_publications.clone() else {
            return Self::err_text(&catalog_inactive());
        };
        let project_id = match parse_project_id(&p.project_id) {
            Ok(project_id) => project_id,
            Err(error) => return Self::err_text(&format!("Error: {error}")),
        };
        let committed = project_id.clone();
        let dry_run = p.dry_run;
        let swap_uncertain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swap_uncertain_inner = swap_uncertain.clone();
        let result = Self::run_blocking("bbox_project_publisher_advance", move || {
            let audit_reason = bounded_audit_reason(&p.audit_reason)?;
            let attachment_id = parse_attachment_id(&p.attachment_id)?;
            let mode = publish_mode_from_params(&p)?;
            // Authority first, checkout second (plan section 7.2 steps 1
            // to 3). A denied request must not make the daemon resolve a
            // ref or read committed trees: it learns only that it was
            // denied. The domain layer re-reads these same gates, so this
            // is an early refusal, not the authority of record.
            project_catalog_admin::preflight_publish_authority(
                &store,
                p.expected_catalog_epoch,
                &committed,
                &attachment_id,
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
            let state = store.snapshot().map_err(|error| anyhow::anyhow!("{error}"))?;
            let Some(row) = state.attachments().attachments.get(&attachment_id).cloned() else {
                anyhow::bail!(
                    "error.project_catalog_admin_unknown_attachment: {attachment_id} is not in the store"
                );
            };
            let probe = publisher_publish_probe(&row, &p.full_ref)?;
            let receipt = project_catalog_admin::publish_accepted_publication(
                &store,
                runtime.as_ref(),
                &project_catalog_admin::PublisherPublishRequest {
                    mode,
                    project_id: committed.clone(),
                    attachment_id,
                    full_ref: p.full_ref.clone(),
                    expected_epoch: p.expected_catalog_epoch,
                    dry_run: p.dry_run,
                },
                probe,
            )
            .map_err(|error| {
                // A failure raised at or after the swap leaves the new
                // pointer possibly installed. The caller still sees the
                // refusal, and the daemon still has to reconverge from
                // whatever is installed, so the flag travels out of the
                // blocking closure beside the error.
                if error.may_have_swapped() {
                    swap_uncertain_inner.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                anyhow::anyhow!("{error}")
            })?;
            tracing::info!(
                tool = "bbox_project_publisher_advance",
                project_id = %committed,
                mode = %p.mode,
                dry_run = p.dry_run,
                generation_id = %receipt.generation_id(),
                audit_reason = %audit_reason,
                "catalog administration mutation"
            );
            Ok(serde_json::to_string_pretty(&json!({
                "status": if receipt.is_dry_run() { "dry_run" } else { "ok" },
                "project_id": committed.as_str(),
                "mode": p.mode,
                "dry_run": receipt.is_dry_run(),
                "generation_id": receipt.generation_id(),
                "generation_sha256": receipt.generation_hash(),
                "pointer_sha256": receipt.pointer_sha256(),
                "previous_pointer_sha256": receipt.previous_pointer_sha256(),
                "audit_reason": audit_reason,
                "epoch": p.expected_catalog_epoch,
            }))?)
        })
        .await;
        let succeeded = !result.is_error.unwrap_or(false);
        let swapped = swap_uncertain.load(std::sync::atomic::Ordering::SeqCst);
        if dry_run && swapped {
            // Structurally impossible: commit_publish refuses a dry-run
            // preparation before it takes the publication lock. Report it
            // loudly and then converge, because the safe direction on a
            // broken assumption is to reconcile, not to skip.
            tracing::error!(
                project_id = %project_id,
                "a dry-run publish reported swap uncertainty; reconciling defensively"
            );
        }
        // A dry run installs no generation and swaps no pointer, so it must
        // not invalidate a projection or enqueue an index replacement. Its
        // whole contract is that nothing durable moves.
        if (succeeded && !dry_run) || swapped {
            // Accepted content identity may have changed. Drop the
            // projections and reconverge the published index from whatever
            // pointer is now installed (plan section 7.3 steps 17 to 19).
            // A reported failure that reached the swap converges too, or
            // the index would keep serving a generation no pointer names.
            // Binding-only operations never reach this path.
            self.invalidate_catalog_published_content(&project_id);
            self.converge_published_knowledge_index(&project_id);
        }
        result
    }

    #[tool(
        name = "bbox_project_publisher_status",
        description = "Read-only accepted-publication status for one catalog project. Reports whether the project serves its current generation, has fallen back to its prior generation, has no pointer at all, or is corrupt; the accepted scope, ref, commit, and generation identity; the bound attachment and the pointer SHA-256; whether the accepted scope still agrees with the catalog's current scope; and whether an advance is available. The generation id and pointer SHA-256 it returns are the compare-and-swap tokens bbox_project_publisher_advance requires. Opens no checkout and takes no lease. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
    )]
    pub(crate) async fn bbox_project_publisher_status(
        &self,
        Parameters(p): Parameters<ProjectPublisherStatusParams>,
    ) -> CallToolResult {
        let Some(store) = self.catalog_store() else {
            return Self::err_text(&catalog_inactive());
        };
        let Some(runtime) = self.state.accepted_publications.clone() else {
            return Self::err_text(&catalog_inactive());
        };
        Self::run_blocking("bbox_project_publisher_status", move || {
            let project_id = parse_project_id(&p.project_id)?;
            let state = store.snapshot().map_err(|error| anyhow::anyhow!("{error}"))?;
            let Some(project) = state.catalog().projects.get(&project_id) else {
                anyhow::bail!(
                    "error.project_catalog_admin_unknown_project: {project_id} is not in the catalog"
                );
            };
            let catalog_scope = match &project.scope {
                bbox_corpus_core::project_catalog::ProjectScope::Published(scope) => Some(scope),
                bbox_corpus_core::project_catalog::ProjectScope::LegacyLocal => None,
            };
            let status = runtime
                .status(&project_id, catalog_scope)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok(serde_json::to_string_pretty(&json!({
                "project_id": project_id.as_str(),
                "accepted_state": accepted_state_label(status.state()),
                "published_available": status.published_available(),
                "advance_available": status.advance_available(),
                "scope_agreement": scope_agreement_label(status.scope_agreement()),
                "accepted_scope": status.content_stamp().map(|stamp| json!({
                    "repo_id": stamp.accepted_scope().repo_id(),
                    "bbox_root_relpath": stamp.accepted_scope().bbox_root_relpath(),
                })),
                "full_ref": status.content_stamp().map(|stamp| stamp.full_ref()),
                "accepted_commit": status.content_stamp().map(|stamp| stamp.accepted_commit()),
                "generation_id": status.content_stamp().map(|stamp| stamp.generation_id()),
                "generation_sha256": status.content_stamp().map(|stamp| stamp.generation_hash()),
                "attachment_id": status.binding_stamp().map(|stamp| stamp.attachment_id().as_str()),
                "pointer_sha256": status.binding_stamp().map(|stamp| stamp.pointer_sha256()),
                "diagnostic": status.failure().map(|failure| failure.code()),
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

/// The exact epoch-checked offline command that accepts one pending
/// nomination (plan §7.6). Acceptance is CLI-only authority (D-005), and the
/// epoch is durable pair state, so the check holds across a daemon stop: a
/// nomination accepted against a stale read refuses and the operator re-reads
/// rather than granting host-wide selector authority from a stale snapshot.
fn alias_accept_commands<'a>(
    project_id: &ProjectId,
    epoch: u64,
    nominations: impl IntoIterator<Item = &'a String>,
) -> Vec<String> {
    nominations
        .into_iter()
        .map(|alias| {
            format!(
                "blackbox project-catalog alias accept --project {project_id} \
                 --alias {alias} --expected-epoch {epoch}"
            )
        })
        .collect()
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

/// Content hash of the project's accepted-publication pointer file, for the
/// publisher-bind receipt (plan §7.7). `None` when the pointer is absent.
// Reached only from the tools' `run_blocking` closures, never inline on a
// tokio worker.
#[allow(clippy::disallowed_methods)]
fn pointer_content_sha256(projects_path: &Path, project_id: &ProjectId) -> Option<String> {
    use sha2::{Digest, Sha256};
    let pointer = accepted_publication_pointer(projects_path, project_id)?;
    let bytes = std::fs::read(pointer).ok()?;
    Some(hex::encode(Sha256::digest(&bytes)))
}

/// Map the wire mode plus its optional tokens onto the typed publish mode.
/// Establish must not carry compare-and-swap tokens and advance must carry
/// both: a half-specified advance is a caller error, never a silent
/// establish (D-040).
fn publish_mode_from_params(
    params: &ProjectPublisherAdvanceParams,
) -> anyhow::Result<PublisherPublishMode> {
    match params.mode.trim() {
        "establish" => {
            if params.expected_generation_id.is_some() || params.expected_pointer_sha256.is_some() {
                anyhow::bail!(
                    "error.project_catalog_admin_publish_mode: establish carries no expected \
                     pointer tokens; a project that already publishes advances instead"
                );
            }
            Ok(PublisherPublishMode::Establish)
        }
        "advance" => {
            let (Some(expected_generation_id), Some(expected_pointer_sha256)) = (
                params.expected_generation_id.clone(),
                params.expected_pointer_sha256.clone(),
            ) else {
                anyhow::bail!(
                    "error.project_catalog_admin_publish_mode: advance requires both \
                     expected_generation_id and expected_pointer_sha256; \
                     bbox_project_publisher_status returns them"
                );
            };
            Ok(PublisherPublishMode::Advance {
                expected_generation_id,
                expected_pointer_sha256,
            })
        }
        other => anyhow::bail!(
            "error.project_catalog_admin_publish_mode: unknown mode {other}; use establish or advance"
        ),
    }
}

/// Resolve the ref, the committed identity, and both source lanes in the
/// attachment's checkout. All of it happens off-lock, and the returned
/// re-resolver is what the commit path calls inside the publication lock.
#[allow(clippy::disallowed_methods)] // reached only from run_blocking closures
fn publisher_publish_probe(
    row: &bbox_corpus_core::project_catalog::CheckoutAttachment,
    full_ref: &str,
) -> anyhow::Result<project_catalog_admin::PublisherPublishProbe> {
    let project_dir = PathBuf::from(&row.checkout_project_dir);
    let git_root = bbox_corpus_core::git::git_root_for_path(&project_dir)
        .unwrap_or_else(|| project_dir.clone());
    let Some(resolved_commit) = bbox_corpus_core::git::resolve_commit(&git_root, full_ref) else {
        anyhow::bail!(
            "error.accepted_publication_ref_missing: {full_ref} does not resolve in the named \
             attachment's checkout"
        );
    };
    // Committed identity AT THE ACCEPTED COMMIT, never at HEAD: the scope
    // being published is the one the accepted bytes declare.
    let committed =
        config::load_project_at_ref(&project_dir, &resolved_commit).map_err(|error| {
            anyhow::anyhow!(
                "error.accepted_publication_committed_identity: the committed project config is \
             unreadable at the accepted commit: {error}"
            )
        })?;
    let Some(repo_id) = committed.project.repo_id.clone() else {
        anyhow::bail!(
            "error.accepted_publication_committed_identity: the committed project config declares \
             no repo_id at the accepted commit"
        );
    };
    let Some(relpath) = bbox_corpus_core::identity::bbox_root_relpath(&git_root, &project_dir)
    else {
        anyhow::bail!(
            "error.project_catalog_admin_path: the attachment project directory is not inside its \
             checkout top"
        );
    };
    let committed_scope = PublishedScope::try_new(repo_id, relpath)
        .map_err(|error| anyhow::anyhow!("error.project_scope_unknown: {error}"))?;
    let knowledge = bbox_knowledge::overlay::load_published_knowledge_sources_at_commit(
        &git_root,
        &resolved_commit,
        &committed_scope,
        None,
        bbox_knowledge::overlay::PublishedKnowledgeSourceLimits::default(),
    )
    .map_err(|error| anyhow::anyhow!("error.accepted_publication_source_capture: {error}"))?;
    let gaps = bbox_gaps::overlay::load_published_gap_sources_at_commit(
        &git_root,
        &resolved_commit,
        &committed_scope,
        None,
        bbox_gaps::overlay::PublishedGapSourceLimits::default(),
    )
    .map_err(|error| anyhow::anyhow!("error.accepted_publication_source_capture: {error}"))?;
    let revalidation_root = git_root.clone();
    let revalidation_ref = full_ref.to_string();
    Ok(project_catalog_admin::PublisherPublishProbe {
        resolved_commit,
        committed_scope,
        sources: PublishSources {
            knowledge: knowledge
                .into_iter()
                .map(|file| PublishSourceFile {
                    repository_relative_filename: file.repository_relative_filename,
                    source_bytes: file.source_bytes,
                })
                .collect(),
            gaps: gaps
                .into_iter()
                .map(|file| PublishSourceFile {
                    repository_relative_filename: file.repository_relative_filename,
                    source_bytes: file.source_bytes,
                })
                .collect(),
        },
        revalidate_ref: Box::new(move || {
            bbox_corpus_core::git::resolve_commit(&revalidation_root, &revalidation_ref)
        }),
    })
}

fn accepted_state_label(state: AcceptedPublicationState) -> &'static str {
    match state {
        AcceptedPublicationState::Current => "current",
        AcceptedPublicationState::Prior => "prior",
        AcceptedPublicationState::Missing => "missing",
        AcceptedPublicationState::Corrupt => "corrupt",
    }
}

fn scope_agreement_label(agreement: AcceptedPublicationScopeAgreement) -> &'static str {
    match agreement {
        AcceptedPublicationScopeAgreement::Agreed => "agreed",
        AcceptedPublicationScopeAgreement::RefreshRequired => "scope_refresh_required",
        AcceptedPublicationScopeAgreement::Unevaluated => "unevaluated",
    }
}

/// Containment check for the publisher rebind: the pointer's accepted commit
/// must already be an object of the new attachment's repository.
fn commit_present_in_checkout(accepted_commit: &str, checkout_project_dir: &Path) -> bool {
    let root = bbox_corpus_core::git::git_root_for_path(checkout_project_dir)
        .unwrap_or_else(|| checkout_project_dir.to_path_buf());
    bbox_corpus_core::git::verify_commit_oid_with_alternate(&root, accepted_commit, None).is_ok()
}

// ── Lifecycle catalog arms (plan §9.1) ─────────────────────────────────
//
// The five compatibility lifecycle tools dispatch here in catalog mode. They
// carry no expected_catalog_epoch (version-1 wire compatibility), so each
// arm pins the epoch from its own snapshot read; a concurrent admin mutation
// surfaces as the store's stale-epoch refusal, never a silent retry.

impl BlackboxServer {
    /// `bbox_project_register` catalog composite: probe, find-or-create by
    /// validated scope + active attachment, attach in one pair transaction,
    /// then ingest alias nominations. Returns the registered projection row
    /// for the shared enrichment pipeline.
    pub(crate) fn register_catalog_arm(
        &self,
        store: &Arc<ProjectCatalogStore>,
        path: &str,
    ) -> anyhow::Result<(
        bbox_corpus_core::project_record::ProjectRecord,
        serde_json::Value,
    )> {
        let probe = probe_checkout(path)?;
        let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&probe.checkout_dir)?;
        let display_name = probe
            .checkout_project_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| probe.checkout_project_dir.to_string_lossy().into_owned());
        let attach_probe = project_catalog_admin::AttachProbe {
            checkout_id,
            checkout_dir: probe.checkout_dir.to_string_lossy().into_owned(),
            checkout_project_dir: probe.checkout_project_dir.to_string_lossy().into_owned(),
            project_root_relpath: probe.project_root_relpath.clone(),
            kind: probe.kind.clone(),
            validated_scope: probe.validated_scope.clone(),
            computed_repo_hint: None,
            branch_ref: probe.branch_ref.clone(),
            capabilities: probe.capabilities.clone(),
            attached_at: now_rfc3339(),
        };
        let epoch = store
            .snapshot()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .epoch();
        let receipt = project_catalog_admin::register_composite(
            &store,
            epoch,
            &attach_probe,
            &display_name,
            &now_rfc3339(),
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        // Nomination ingestion (plan §7.6): a refused or empty nomination
        // set never fails the registration that already committed.
        let nomination_epoch = receipt
            .commit
            .as_ref()
            .map(|commit| commit.epoch)
            .unwrap_or(epoch);
        let nominated = ingest_alias_nominations(
            store,
            nomination_epoch,
            &receipt.project_id,
            &probe.declared_aliases,
        );
        let record = self
            .state
            .records_provider
            .records_snapshot()
            .records
            .iter()
            .find(|record| record.project_id == receipt.project_id.as_str())
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "registered attachment did not surface in the compatibility projection"
                )
            })?;
        // Same nomination contract as attach: the summary carries the exact
        // epoch-checked acceptance command for the epoch it reports.
        let accept_commands = alias_accept_commands(
            &receipt.project_id,
            nominated.epoch.unwrap_or(nomination_epoch),
            &nominated.recorded,
        );
        let summary = json!({
            "project_id": receipt.project_id.as_str(),
            "attachment_id": receipt.attachment_id.as_str(),
            "created_project": receipt.created_project,
            "already_attached": receipt.already_attached,
            "epoch": nominated.epoch.or(receipt.commit.as_ref().map(|c| c.epoch)),
            "nominated_aliases": nominated.recorded,
            "alias_accept_commands": accept_commands,
        });
        Ok((record, summary))
    }

    /// `bbox_project_rename` catalog arm: attachment relocation. Same
    /// checkout identity (marker-proved), same validated scope, same
    /// relpath; one pair transaction updates the attachment paths and
    /// appends the §8.4 ledger row. No owner-store rows are rewritten.
    pub(crate) fn rename_catalog_arm(
        &self,
        store: &Arc<ProjectCatalogStore>,
        p: &bbox_indexing::projects::ProjectRenameParams,
    ) -> anyhow::Result<String> {
        if p.move_on_disk.unwrap_or(false) {
            anyhow::bail!(
                "error.project_catalog_admin_unsupported: catalog-mode rename records a \
                 relocation after the checkout has moved; move the directory first and \
                 re-run without move_on_disk"
            );
        }
        let dry_run = p.dry_run.unwrap_or(false);
        // Resolve the existing attachment through the shared engine: the
        // selector may be the OLD path, the project id, or an alias.
        let state = store
            .snapshot()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let engine = ProjectResolverEngine::v2(state.catalog(), state.attachments());
        let resolved = engine
            .resolve_attached(&ProjectSelectorRequest::selection(
                p.project.clone(),
                bbox_corpus_core::project_selector::ResolveIntent::Write,
            ))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let bbox_corpus_core::project_selector::ResolvedAttachment::Catalog {
            attachment_id, ..
        } = &resolved.attachment
        else {
            anyhow::bail!("error.project_selector_unknown: {}", p.project);
        };
        let attachment_id = parse_attachment_id(attachment_id)?;
        let row = state
            .attachments()
            .attachments
            .get(&attachment_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("resolved attachment vanished from the snapshot"))?;
        let owner_scope = state
            .catalog()
            .projects
            .get(&row.project_id)
            .map(|project| project.scope.clone())
            .ok_or_else(|| anyhow::anyhow!("attachment references an unknown project"))?;
        let epoch = state.epoch();
        drop(state);

        // Probe the NEW path. Sameness is the durable checkout-id marker,
        // read (never minted) at the moved location: path existence and
        // inode reuse never prove sameness.
        let new_probe = probe_checkout(&p.new_path)?;
        let marker = new_probe.checkout_dir.join(".bbox/local/checkout-id");
        let moved_checkout_id = bbox_corpus_core::identity::read_checkout_id(&marker)
            .ok()
            .flatten();
        let Some(moved_checkout_id) = moved_checkout_id else {
            let instruction = match owner_scope {
                ProjectScope::LegacyLocal => {
                    "the moved directory carries no checkout identity marker; run \
                     bbox_project_init at the new path to establish identity, or detach \
                     the old attachment and re-attach the new path explicitly"
                }
                ProjectScope::Published(_) => {
                    "the moved directory carries no checkout identity marker; detach the \
                     old attachment and re-attach the new path explicitly"
                }
            };
            anyhow::bail!("error.project_catalog_admin_checkout_identity_missing: {instruction}");
        };
        let relocation = project_catalog_admin::RelocationProbe {
            checkout_id: moved_checkout_id,
            new_checkout_dir: new_probe.checkout_dir.to_string_lossy().into_owned(),
            new_checkout_project_dir: new_probe
                .checkout_project_dir
                .to_string_lossy()
                .into_owned(),
            resolved_scope: new_probe.validated_scope.clone(),
        };
        if dry_run {
            // Read-only agreement report; the apply path revalidates inside
            // the transaction.
            let identity_matches = relocation.checkout_id == row.checkout_id;
            return Ok(serde_json::to_string_pretty(&json!({
                "status": "dry_run",
                "project_id": row.project_id.as_str(),
                "attachment_id": attachment_id.as_str(),
                "old_checkout_project_dir": row.checkout_project_dir,
                "new_checkout_project_dir": relocation.new_checkout_project_dir,
                "checkout_identity_matches": identity_matches,
                "owner_store_rows_rewritten": false,
            }))?);
        }
        let commit =
            project_catalog_admin::relocate_attachment(&store, epoch, &attachment_id, &relocation)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(serde_json::to_string_pretty(&json!({
            "status": "ok",
            "project_id": row.project_id.as_str(),
            "attachment_id": attachment_id.as_str(),
            "old_checkout_project_dir": row.checkout_project_dir,
            "new_checkout_project_dir": relocation.new_checkout_project_dir,
            "ledger_binding_appended": true,
            "owner_store_rows_rewritten": false,
            "epoch": commit.epoch,
            "catalog_sha256": commit.catalog_sha256,
            "attachments_sha256": commit.attachments_sha256,
        }))?)
    }

    /// `bbox_project_unregister` catalog arm: unregister is detach. Logical
    /// state stays untouched; catalog deletion is the offline retire
    /// surface.
    pub(crate) fn unregister_catalog_arm(
        &self,
        store: &Arc<ProjectCatalogStore>,
        p: &bbox_indexing::projects::ProjectUnregisterParams,
    ) -> anyhow::Result<String> {
        let dry_run = p.dry_run.unwrap_or(false);
        let state = store
            .snapshot()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let engine = ProjectResolverEngine::v2(state.catalog(), state.attachments());
        let resolved = engine
            .resolve_attached(&ProjectSelectorRequest::selection(
                p.project.clone(),
                bbox_corpus_core::project_selector::ResolveIntent::Write,
            ))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let bbox_corpus_core::project_selector::ResolvedAttachment::Catalog {
            attachment_id, ..
        } = &resolved.attachment
        else {
            anyhow::bail!("error.project_selector_unknown: {}", p.project);
        };
        let attachment_id = parse_attachment_id(attachment_id)?;
        let row = state
            .attachments()
            .attachments
            .get(&attachment_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("resolved attachment vanished from the snapshot"))?;
        let epoch = state.epoch();
        drop(state);
        if dry_run {
            return Ok(serde_json::to_string_pretty(&json!({
                "status": "dry_run",
                "project_id": row.project_id.as_str(),
                "attachment_id": attachment_id.as_str(),
                "would_detach": true,
                "logical_state": "preserved",
                "catalog_deletion": "blackbox project-catalog retire",
            }))?);
        }
        let commit =
            project_catalog_admin::detach_attachment(&store, epoch, &attachment_id, &now_rfc3339())
                .map_err(|error| anyhow::anyhow!("{error}"))?;
        let census_removed = self.deregister_detached_pair(&row);
        Ok(serde_json::to_string_pretty(&json!({
            "status": "ok",
            "project_id": row.project_id.as_str(),
            "attachment_id": attachment_id.as_str(),
            "detached": true,
            "census_row_removed": census_removed,
            "logical_state": "preserved",
            "catalog_deletion": "blackbox project-catalog retire",
            "epoch": commit.epoch,
        }))?)
    }

    /// `bbox_project_init` catalog follow-up: when init newly records repo
    /// authority inside a checkout attached to a `LegacyLocal` project,
    /// promotion is the required next action (plan §9.1).
    /// What catalog authority says about an absolute checkout path.
    ///
    /// The distinction is load-bearing for the plan 4.19 bootstrap
    /// exception: ONLY a path that catalog authority does not know at all
    /// may be scaffolded without a `RepositoryMutation` lease. Collapsing
    /// every failure into "unregistered" would let a detached attachment,
    /// an ambiguous selector, or an unreadable catalog take the lease-free
    /// path and write into a checkout the catalog still governs.
    pub(crate) fn catalog_path_authority(
        &self,
        store: &Arc<ProjectCatalogStore>,
        canonical_path: &str,
    ) -> CatalogPathAuthority {
        let state = match store.snapshot() {
            Ok(state) => state,
            Err(error) => {
                return CatalogPathAuthority::Unreadable {
                    code: error.code().to_string(),
                    diagnostic: error.to_string(),
                };
            }
        };
        let engine = ProjectResolverEngine::v2(state.catalog(), state.attachments());
        match engine.resolve_attached(&ProjectSelectorRequest::selection(
            canonical_path.to_string(),
            bbox_corpus_core::project_selector::ResolveIntent::Write,
        )) {
            Ok(resolved) => {
                CatalogPathAuthority::Attached(resolved.project.project_id().to_owned())
            }
            // The resolver considers ACTIVE attachments only, so its refusal
            // does not prove the catalog is ignorant of this path. Ask the
            // attachment rows directly, at every status, before conceding
            // that the path is unregistered.
            Err(error) => match attachment_row_governing(&state, canonical_path) {
                Some(governing) => CatalogPathAuthority::Governed {
                    diagnostic: format!("{governing} ({})", error.code()),
                },
                None => CatalogPathAuthority::Absent,
            },
        }
    }

    pub(crate) fn init_catalog_next_action(
        &self,
        store: &Arc<ProjectCatalogStore>,
        canonical_path: &str,
        repo_id_recorded: bool,
    ) -> Option<serde_json::Value> {
        if !repo_id_recorded {
            return None;
        }
        let state = store.snapshot().ok()?;
        let engine = ProjectResolverEngine::v2(state.catalog(), state.attachments());
        let resolved = engine
            .resolve_attached(&ProjectSelectorRequest::selection(
                canonical_path.to_string(),
                bbox_corpus_core::project_selector::ResolveIntent::Read,
            ))
            .ok()?;
        let project_id = resolved.project.project_id().to_owned();
        let owner = state
            .catalog()
            .projects
            .get(&ProjectId::parse(&project_id).ok()?)?;
        matches!(owner.scope, ProjectScope::LegacyLocal).then(|| {
            json!({
                "action": "promotion_required",
                "project_id": project_id,
                "detail": "this checkout now records repo authority for a legacy-local \
                           project; run bbox_project_promote to publish it",
            })
        })
    }
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

    #[test]
    fn publish_mode_parsing_refuses_half_specified_requests() {
        fn params(
            mode: &str,
            generation: Option<&str>,
            pointer: Option<&str>,
        ) -> ProjectPublisherAdvanceParams {
            ProjectPublisherAdvanceParams {
                project_id: "p_00000000000000000000000000000000".into(),
                attachment_id: "att_00000000000000000000000000000000".into(),
                mode: mode.into(),
                full_ref: "refs/heads/main".into(),
                expected_generation_id: generation.map(str::to_owned),
                expected_pointer_sha256: pointer.map(str::to_owned),
                dry_run: false,
                expected_catalog_epoch: 1,
                audit_reason: "mode parsing".into(),
            }
        }

        assert!(matches!(
            publish_mode_from_params(&params("establish", None, None)).unwrap(),
            PublisherPublishMode::Establish
        ));
        // Establish carries no compare-and-swap tokens (D-040): a caller
        // that has tokens is advancing, not establishing.
        assert!(publish_mode_from_params(&params("establish", Some("a"), None)).is_err());
        assert!(publish_mode_from_params(&params("advance", Some("a"), None)).is_err());
        assert!(publish_mode_from_params(&params("advance", None, Some("b"))).is_err());
        assert!(matches!(
            publish_mode_from_params(&params("advance", Some("a"), Some("b"))).unwrap(),
            PublisherPublishMode::Advance { .. }
        ));
        assert!(publish_mode_from_params(&params("bind", None, None)).is_err());
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
            server
                .bbox_project_publisher_advance(Parameters(ProjectPublisherAdvanceParams {
                    project_id: "p_00000000000000000000000000000000".into(),
                    attachment_id: "att_00000000000000000000000000000000".into(),
                    mode: "establish".into(),
                    full_ref: "refs/heads/main".into(),
                    expected_generation_id: None,
                    expected_pointer_sha256: None,
                    dry_run: false,
                    expected_catalog_epoch: 1,
                    audit_reason: "bridge refusal".into(),
                }))
                .await,
            server
                .bbox_project_publisher_status(Parameters(ProjectPublisherStatusParams {
                    project_id: "p_00000000000000000000000000000000".into(),
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

    fn git(root: &Path, args: &[&str]) {
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

    /// One real publishing checkout: a git repository whose committed
    /// `.bbox` tree declares a repo id and carries one record in each lane.
    fn publishing_checkout(root: &Path) -> bbox_corpus_core::project_catalog::CheckoutAttachment {
        std::fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        std::fs::create_dir_all(root.join(".bbox/gaps")).unwrap();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.email", "probe@example.invalid"]);
        git(root, &["config", "user.name", "probe"]);
        std::fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"repo_probe\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".bbox/knowledge/knowledge-a.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "knowledge-a",
                "title": "probe entry",
                "content": "accepted content",
                "category": "convention",
                "scope": "project",
                "priority": "standard",
                "status": "active",
                "approval": "user_confirmed",
                "source": "user",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-02T00:00:00Z",
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join(".bbox/gaps/gap-1234abcd.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "gap-1234abcd",
                "title": "probe gap",
                "gap_kind": "tooling",
                "domain": "publication",
                "wanted_capability": "probe the publish path",
                "dedupe_key": "tooling/publication/probe",
                "impact": "medium",
                "blocking_level": "workaround_available",
                "resolution": "unresolved",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-02T00:00:00Z",
            }))
            .unwrap(),
        )
        .unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-m", "publishable state"]);

        bbox_corpus_core::project_catalog::CheckoutAttachment {
            attachment_id: AttachmentId::parse("att_00000000000000000000000000000e01").unwrap(),
            project_id: ProjectId::parse("p_000000000000000000000000000000e1").unwrap(),
            checkout_id: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeee01".into(),
            checkout_dir: root.to_string_lossy().into_owned(),
            checkout_project_dir: root.to_string_lossy().into_owned(),
            project_root_relpath: ".".into(),
            kind: AttachmentKind::Base,
            validated_scope: Some(PublishedScope::try_new("repo_probe", ".").unwrap()),
            computed_repo_hint: None,
            branch_ref: Some("refs/heads/main".into()),
            capabilities: AttachmentCapabilities {
                repo_knowledge: true,
                ..Default::default()
            },
            status: AttachmentStatus::Attached,
            attached_at: "2026-08-03T00:00:00Z".into(),
            detached_at: None,
        }
    }

    /// The publish probe against a real repository: the success path, then
    /// the two refusals that stop a publish before the domain layer.
    #[test]
    fn the_publish_probe_captures_committed_state_and_refuses_unresolvable_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let row = publishing_checkout(&root);

        // Success: the ref resolves, the committed identity at that commit
        // supplies the scope, and both lanes are captured.
        let probe = publisher_publish_probe(&row, "refs/heads/main").unwrap();
        assert_eq!(probe.resolved_commit.len(), 40);
        assert_eq!(
            probe.committed_scope,
            PublishedScope::try_new("repo_probe", ".").unwrap()
        );
        assert_eq!(probe.sources.knowledge.len(), 1);
        assert_eq!(probe.sources.gaps.len(), 1);
        assert_eq!(
            probe.sources.knowledge[0].repository_relative_filename,
            ".bbox/knowledge/knowledge-a.json"
        );
        // The in-lock re-resolver reads the same ref through the same path.
        assert_eq!(
            (probe.revalidate_ref)(),
            Some(probe.resolved_commit.clone())
        );

        // A ref that does not exist.
        let error = publisher_publish_probe(&row, "refs/heads/does-not-exist")
            .err()
            .expect("an unresolvable ref refuses");
        assert!(
            error
                .to_string()
                .starts_with("error.accepted_publication_ref_missing"),
            "{error}"
        );

        // A ref that exists but names an object this repository does not
        // have. The probe resolves the accepted commit THROUGH the ref with
        // `rev-parse --verify <ref>^{commit}`, so a missing commit object is
        // indistinguishable from a missing ref here: both are "this ref does
        // not name a commit we have". The refusal is deliberately the same.
        std::fs::write(
            root.join(".git/refs/heads/dangling"),
            format!("{}\n", "0".repeat(39) + "1"),
        )
        .unwrap();
        let error = publisher_publish_probe(&row, "refs/heads/dangling")
            .err()
            .expect("a dangling ref refuses");
        assert!(
            error
                .to_string()
                .starts_with("error.accepted_publication_ref_missing"),
            "{error}"
        );
    }

    /// A commit that resolves but declares no repo id cannot name a scope,
    /// so the publish refuses before any generation is built.
    #[test]
    fn the_publish_probe_refuses_a_commit_without_committed_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let row = publishing_checkout(&root);
        // A second branch whose commit carries no committed repo id.
        git(&root, &["checkout", "-b", "identityless"]);
        std::fs::write(root.join(".bbox/config.toml"), "[project]\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-m", "drop the committed repo id"]);

        let error = publisher_publish_probe(&row, "refs/heads/identityless")
            .err()
            .expect("a commit without committed identity refuses");
        assert!(
            error
                .to_string()
                .starts_with("error.accepted_publication_committed_identity"),
            "{error}"
        );
    }

    fn index_search(server: &BlackboxServer, query: &str) -> String {
        let view = server.state.code_read_view.read().clone();
        server
            .state
            .idx
            .read()
            .search_with_active_selectors_and_searcher(
                &crate::index::SearchParams {
                    query: query.into(),
                    mode: None,
                    account: None,
                    project: None,
                    role: None,
                    include_subagents: None,
                    limit: Some(5),
                    exclude_self: None,
                },
                &view.active_selectors,
                &view.searcher,
            )
            .unwrap()
    }

    /// A dry run must move nothing durable, including the search index.
    ///
    /// The handler used to treat any non-error result as a publish, so a
    /// successful dry run invalidated the projections and enqueued a scope
    /// replacement: a durable index mutation from an operation whose whole
    /// contract is that nothing durable moves.
    ///
    /// Making that observable needs a pending divergence. The index is
    /// converged at G1 and the accepted store is then moved to G2 without
    /// converging, so a convergence the dry run should never perform would
    /// visibly flip the index to G2.
    #[tokio::test]
    async fn a_dry_run_publish_moves_nothing_durable() {
        use crate::server::state::catalog_fixture::{
            COMMIT_ONE, COMMIT_TWO, CatalogFixture, knowledge_entry,
        };

        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().canonicalize().unwrap().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        publishing_checkout(&checkout);
        let scope = PublishedScope::try_new("repo_probe", ".").unwrap();

        let fixture = CatalogFixture::new();
        fixture.add_published_project("p_dryrun", &scope);
        fixture.attach_checkout(
            "p_dryrun",
            &scope,
            &checkout,
            "att_00000000000000000000000000000d01",
        );
        fixture.install_publication(
            "p_dryrun",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "generationone")],
            &[],
        );
        let server = fixture.server();
        server.state.install_code_read_view_commit_hook();
        let project_id = ProjectId::parse("p_dryrun").unwrap();

        // Converge the index at G1.
        server.converge_published_knowledge_index(&project_id);
        server.state.index_writer.flush_blocking().unwrap();
        server.state.idx.write().reader_reload_for_test();
        assert!(index_search(&server, "generationone").contains("knowledge-a"));

        // Move accepted content to G2 WITHOUT converging: the index now
        // lags the accepted store, so any convergence is observable.
        fixture.install_publication(
            "p_dryrun",
            &scope,
            COMMIT_TWO,
            &[knowledge_entry("knowledge-a", "generationtwo")],
            &[],
        );
        let runtime = server.state.accepted_publications.clone().unwrap();
        let tokens = runtime.advance_tokens(&project_id).unwrap().unwrap();

        let response = server
            .bbox_project_publisher_advance(Parameters(ProjectPublisherAdvanceParams {
                project_id: "p_dryrun".into(),
                attachment_id: "att_00000000000000000000000000000d01".into(),
                mode: "advance".into(),
                full_ref: "refs/heads/main".into(),
                expected_generation_id: Some(tokens.0),
                expected_pointer_sha256: Some(tokens.1),
                dry_run: true,
                expected_catalog_epoch: fixture.epoch(),
                audit_reason: "dry run".into(),
            }))
            .await;
        let text = error_text(&response);
        assert!(!response.is_error.unwrap_or(false), "{text}");
        assert!(text.contains("\"dry_run\""), "{text}");

        // No scope replacement was enqueued, so the index still lags at G1.
        server.state.index_writer.flush_blocking().unwrap();
        server.state.idx.write().reader_reload_for_test();
        assert!(
            index_search(&server, "generationone").contains("knowledge-a"),
            "a dry run must not enqueue an index replacement"
        );
        assert!(
            !index_search(&server, "generationtwo").contains("knowledge-a"),
            "a dry run must not converge the index to newer accepted content"
        );
    }
    /// Denied publish requests must not touch a repository.
    ///
    /// The fixture has no checkout anywhere: the catalog holds one
    /// published project and nothing else. If the handler probed before
    /// deciding authority it would fail on the missing checkout and report
    /// a ref or path error. Each refusal below is an authority code, which
    /// is only reachable if the preflight ran first.
    #[tokio::test]
    async fn denied_publish_requests_never_reach_git_or_the_source_loaders() {
        use crate::server::state::catalog_fixture::CatalogFixture;

        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_denied", &scope);
        let server = fixture.server();

        let stale = server
            .bbox_project_publisher_advance(Parameters(ProjectPublisherAdvanceParams {
                project_id: "p_denied".into(),
                attachment_id: "att_00000000000000000000000000000f01".into(),
                mode: "establish".into(),
                full_ref: "refs/heads/main".into(),
                expected_generation_id: None,
                expected_pointer_sha256: None,
                dry_run: false,
                expected_catalog_epoch: 9_999,
                audit_reason: "stale epoch".into(),
            }))
            .await;
        let text = error_text(&stale);
        assert!(text.contains("error.project_catalog_stale_epoch"), "{text}");

        let epoch = server
            .state
            .project_authority
            .catalog_store()
            .unwrap()
            .snapshot()
            .unwrap()
            .epoch();
        let unknown_attachment = server
            .bbox_project_publisher_advance(Parameters(ProjectPublisherAdvanceParams {
                project_id: "p_denied".into(),
                attachment_id: "att_00000000000000000000000000000f01".into(),
                mode: "establish".into(),
                full_ref: "refs/heads/main".into(),
                expected_generation_id: None,
                expected_pointer_sha256: None,
                dry_run: false,
                expected_catalog_epoch: epoch,
                audit_reason: "unknown attachment".into(),
            }))
            .await;
        let text = error_text(&unknown_attachment);
        assert!(
            text.contains("error.project_catalog_admin_unknown_attachment"),
            "{text}"
        );
        // Neither refusal mentions a ref, a commit, or a path, which is
        // what a probe-first handler would have produced here.
        assert!(
            !text.contains("error.accepted_publication_ref_missing"),
            "{text}"
        );
    }

    /// Convergence after a publish: the published knowledge reaches the
    /// search index, and a project with no verified content leaves the
    /// index alone rather than clearing rows a fallback may serve.
    #[tokio::test]
    async fn published_index_convergence_replaces_the_scope_and_skips_unverified_projects() {
        use crate::server::state::catalog_fixture::{COMMIT_ONE, CatalogFixture, knowledge_entry};

        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_converge", &scope);
        fixture.install_publication(
            "p_converge",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "converged content")],
            &[],
        );
        let server = fixture.server();
        // The read view pins a searcher; without the commit hook a fresh
        // commit is durable but invisible to this pinned view.
        server.state.install_code_read_view_commit_hook();
        let project_id = ProjectId::parse("p_converge").unwrap();

        server.converge_published_knowledge_index(&project_id);
        server.state.index_writer.flush_blocking().unwrap();
        server.state.idx.write().reader_reload_for_test();
        assert!(
            index_search(&server, "converged").contains("knowledge-a"),
            "the published entry reached the search index"
        );

        // A project with no publication at all: convergence is a no-op,
        // not an index clear.
        fixture.add_published_project("p_unpublished", &CatalogFixture::scope("sub/none"));
        server.converge_published_knowledge_index(&ProjectId::parse("p_unpublished").unwrap());
        server.state.index_writer.flush_blocking().unwrap();
        server.state.idx.write().reader_reload_for_test();
        assert!(
            index_search(&server, "converged").contains("knowledge-a"),
            "an unverifiable project must not clear another project rows"
        );
    }

    #[test]
    fn project_catalog_probe_rejects_relative_and_missing_paths() {
        assert!(probe_checkout("relative/path").is_err());
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("absent");
        assert!(probe_checkout(missing.to_str().unwrap()).is_err());
    }
}
