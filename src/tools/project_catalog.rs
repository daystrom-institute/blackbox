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
    AcceptedPublicationScopeAgreement, AcceptedPublicationSourceBinding, AcceptedPublicationState,
    AutoAdvanceGrantUpdate, PublishError, PublishSourceFile, PublishSources, PublisherPublishMode,
};
use bbox_indexing::checkout_access::{
    CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest, CheckoutAccessSourceLane,
    CheckoutAttachmentSelector,
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
    /// Local attachment whose checkout carries the ref being published.
    /// Mutually exclusive with source_generation_id.
    #[serde(default)]
    pub attachment_id: Option<String>,
    /// Ready remote publication candidate. Mutually exclusive with
    /// attachment_id.
    #[serde(default)]
    pub source_generation_id: Option<String>,
    /// `establish` for a project's first pointer, `advance` to move one.
    pub mode: String,
    /// Attachment publication only: fully qualified publisher ref, for
    /// example `refs/heads/main`. Remote candidates carry their ref in
    /// immutable source evidence and refuse this parameter.
    #[serde(default)]
    pub full_ref: Option<String>,
    /// Advance only: the generation id the caller expects to replace.
    #[serde(default)]
    pub expected_generation_id: Option<String>,
    /// Advance only: the SHA-256 of the pointer the caller expects to
    /// replace.
    #[serde(default)]
    pub expected_pointer_sha256: Option<String>,
    /// Operator authority over this project's standing auto-advance grant.
    /// Omit to leave the grant exactly as it is (the default). `true`
    /// installs it on the pointer this call writes, which is the audited
    /// operator act that lets later Ready candidates from the SAME bound
    /// producer, scope, and ref be accepted without a further operator
    /// call. `false` revokes it. Agents pass this through from operator
    /// input and never default or infer it.
    #[serde(default)]
    pub auto_advance: Option<bool>,
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
        // Identity only. Vendor coordinates never render here; they render
        // beside the scope as observations so a reader cannot mistake one
        // for the other.
        ProjectScope::Connector(scope) => json!({
            "kind": "connector",
            "connector_source_id": scope.connector_source_id().as_str(),
            "connector_kind": scope.connector_kind().as_str(),
        }),
    }
}

/// Observed vendor coordinates for a connector project, or `null`.
///
/// Rendered under an explicit `connector_observations` key, never merged
/// into the scope object: these are the producer's claims about where the
/// source lives, refreshed on every onboarding report, and the daemon
/// verifies none of them.
fn connector_observations_json(
    catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
    project: &bbox_corpus_core::project_catalog::CorpusProject,
) -> serde_json::Value {
    if project.scope.connector().is_none() {
        return serde_json::Value::Null;
    }
    match catalog.connector_observations.get(&project.project_id) {
        Some(observed) => json!({
            "observed_at": observed.observed_at,
            "producer_id": observed.producer_id,
            "remote_authority": observed.remote_authority,
            "remote_root_id": observed.remote_root_id,
            "remote_display_name": observed.remote_display_name,
        }),
        None => serde_json::Value::Null,
    }
}

/// The file-source publication state of one connector-scoped project.
///
/// Phase 0 reported `publication_lanes: []` because no lane existed. Phase 1
/// mounts one, so this renders what that lane actually knows: the active
/// generation and its ordinal, per-state generation counts, the freshness
/// facts (file count, logical bytes, the display-only remote watermark), the
/// producer's own publication telemetry, and any cursor degradation.
///
/// Three things are deliberately absent.
///
/// No credential material, and none is reachable: every field here comes from
/// the durable generation record, whose wire types carry opaque tokens the
/// leaf contract already bounded and scheme-restricted. `remote_url` is not
/// projected at all, because it is per-entry manifest data rather than
/// publication status and rendering a manifest through a status call would
/// make an unbounded response out of a bounded one.
///
/// No freshness AUTHORITY. `remote_watermark` is rendered beside a name that
/// says what it is; the manifest digest is the fingerprint and the generation
/// id already carries it.
///
/// No error when the store has never heard of this scope. A connector project
/// that onboarded and has not yet published reports an absent active
/// generation and empty counts, which is the honest reading of "onboarded, no
/// publication yet" and is exactly the phase-0 state this replaces.
fn connector_publication_json(
    store: &bbox_file_source_store::FileSourceStore,
    scope: &bbox_corpus_core::project_catalog::ConnectorScope,
) -> serde_json::Value {
    let generations = match store.generations(scope) {
        Ok(generations) => generations,
        // A read failure is REPORTED, not swallowed and not fatal: publisher
        // status is an observational call and a connector lane that cannot be
        // read must not take down the whole status response for a project.
        Err(error) => {
            return json!({
                "readable": false,
                "diagnostic": error.to_string().chars().take(256).collect::<String>(),
            });
        }
    };
    let active_record = store.active_generation(scope).ok().flatten();
    let mut states: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for generation in &generations {
        let label = serde_json::to_value(generation.state)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string());
        *states.entry(label).or_insert(0) += 1;
    }
    let active = active_record.as_ref().and_then(|record| {
        let generation = generations
            .iter()
            .find(|generation| generation.generation_id == record.generation_id)?;
        Some(json!({
            "generation_id": generation.generation_id,
            "ordinal": generation.ordinal,
            "producer_id": generation.producer_id,
            "selector": record.selector,
            "installed_at": record.installed_at,
            "document_count": record.document_count,
            "superseded_generation_id": record.superseded_generation_id,
            "file_count": generation.descriptor.file_count,
            "logical_bytes": generation.descriptor.logical_bytes,
            "cursor_epoch": generation.descriptor.cursor_epoch,
            // Named for what it is on the wire: display and diagnostic only.
            // The manifest digest is the fingerprint.
            "remote_watermark_display_only": generation.descriptor.remote_watermark,
            "manifest_sha256": generation.descriptor.manifest_sha256,
            "telemetry": {
                "entries_enumerated": generation.telemetry.entries_enumerated,
                "blobs_fetched": generation.telemetry.blobs_fetched,
                "documents_exported": generation.telemetry.documents_exported,
                "skipped": generation.telemetry.skipped,
                "total_skipped": generation.telemetry.total_skipped(),
            },
        }))
    });
    // Cursor degradation is surfaced from the LATEST generation carrying one
    // rather than only from the active one: a degradation reported by a
    // generation that has not activated yet is still the operator's signal,
    // and hiding it until activation is the "silently absorbed" failure the
    // design forbids by name.
    let degradation = generations
        .iter()
        .rev()
        .find_map(|generation| generation.degradation.as_ref())
        .map(|degradation| {
            json!({
                "checkpoint_name": degradation.checkpoint_name,
                "cause": degradation.cause,
                "cursor_epoch": degradation.cursor_epoch,
                "observed_at": degradation.observed_at,
                "entries_enumerated": degradation.entries_enumerated,
                "blobs_refetched": degradation.blobs_refetched,
                "documents_reexported": degradation.documents_reexported,
            })
        });
    json!({
        "readable": true,
        "active": active,
        "generation_count": generations.len(),
        "generation_states": states,
        "last_cursor_degradation": degradation,
    })
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
    bbox_indexing::project_catalog_probe::active_attachment_scopes(store, project_id)
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
        description = "List every project in the durable catalog, including remote-only projects with no attachment on this host. Path-free rows: project_id, display_name, scope (published repo_id + bbox_root_relpath, legacy_local, or connector connector_source_id + connector_kind), connector_observations (a connector producer's observed vendor coordinates, null for every other scope family; observations are replaceable claims, never identity), operator and nominated aliases, and the count of active attachments. Returns the catalog epoch to pass as expected_catalog_epoch on a following administration call. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
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
                        "connector_observations":
                            connector_observations_json(state.catalog(), project),
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
        description = "Read one catalog project: its id, display name, scope, connector_observations, aliases, pending alias nominations, and repo-history reference, plus a separate host_local_attachments section carrying this host's attachment rows (attachment_id, status, kind, checkout dir, relpath). The catalog section stays path-free; attachment paths are host-local operator data. A connector-scoped project has no attachment and no repo history, and its vendor coordinates render only as connector_observations: they are what a producer reported, refreshed on every onboarding, and never the project's identity. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
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
                    "connector_observations":
                        connector_observations_json(state.catalog(), project),
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
        let checkout_access = self.state.checkout_access.clone();
        let bound_project = match parse_project_id(&p.project_id) {
            Ok(project_id) => project_id,
            Err(error) => return Self::err_text(&format!("Error: {error}")),
        };
        if self
            .state
            .knowledge_transport_cutover
            .covers_project(&bound_project)
        {
            self.observe_knowledge_transport_operation(
                bound_project.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::AcceptedPublicationMutation,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::AuthoritativeRefusal,
            );
            return Self::err_text(
                "error.knowledge_transport_authoritative: covered projects cannot bind accepted publication to a checkout attachment",
            );
        }
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
            let Some(_row) = state.attachments().attachments.get(&attachment_id) else {
                anyhow::bail!(
                    "error.project_catalog_admin_unknown_attachment: {attachment_id} is not in the store"
                );
            };
            let Some(project) = state.catalog().projects.get(&project_id) else {
                anyhow::bail!(
                    "error.project_catalog_admin_unknown_project: {project_id} is not in the catalog"
                );
            };
            let ProjectScope::Published(catalog_scope) = &project.scope else {
                anyhow::bail!(
                    "error.project_catalog_admin_scope_required: publisher binding requires a published project"
                );
            };
            let lease = Arc::new(
                checkout_access
                    .acquire(CheckoutAccessRequest {
                        project_id: project_id.to_string(),
                        attachment: CheckoutAttachmentSelector::AttachmentId(
                            attachment_id.to_string(),
                        ),
                        expected_scope: Some(catalog_scope.clone()),
                        kind: CheckoutAccessKind::PublisherConfigTreeRead,
                        intent: CheckoutAccessIntent::Read,
                        source_lane: CheckoutAccessSourceLane::NativeAttachment,
                    })
                    .map_err(anyhow::Error::new)?,
            );
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
                    lease.project_root(),
                ),
                revalidate_checkout: {
                    let checkout_access = checkout_access.clone();
                    let lease = Arc::clone(&lease);
                    Box::new(move || checkout_access.revalidate(&lease).is_ok())
                },
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
        description = "Establish or advance one published project's accepted publication. mode=establish creates the first pointer; mode=advance requires the generation and pointer tokens from bbox_project_publisher_status. Select exactly one source: attachment_id with full_ref reads a capable attached checkout, while source_generation_id consumes a Ready remote publication candidate and derives its producer, scope, ref, commit, and both source lanes from pinned immutable evidence. Candidate mode refuses caller-supplied full_ref. Both paths validate knowledge and gaps into one immutable generation and swap only after rechecking catalog authority and source freshness. Publishing uses the catalog's current scope, which clears a scope-migration bridge. dry_run validates and writes nothing. Requires expected_catalog_epoch and a bounded audit_reason. auto_advance is operator authority over this project's standing auto-advance grant: omit it to leave the grant unchanged, pass true to install it on the pointer this call writes, or false to revoke it. A granted project accepts later Ready candidates from the same bound producer, catalog scope, and published ref through this same validation and compare-and-swap discipline, with audit_reason policy:auto_advance; establish, rollback, scope changes, and every other non-linear move stay manual. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
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
        let knowledge_transport_governed = self
            .state
            .knowledge_transport_cutover
            .covers_project(&project_id);
        if knowledge_transport_governed && p.attachment_id.is_some() {
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::AcceptedPublicationMutation,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::AuthoritativeRefusal,
            );
        }
        let producer_auth = self.state.code_sources.producer_auth();
        let checkout_access = self.state.checkout_access.clone();
        let knowledge_sources = self.state.knowledge_sources.store();
        let dry_run = p.dry_run;
        let remote_candidate_source = p.source_generation_id.is_some();
        let swap_uncertain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swap_uncertain_inner = swap_uncertain.clone();
        let result =
            Self::run_blocking("bbox_project_publisher_advance", move || {
                let audit_reason = bounded_audit_reason(&p.audit_reason)?;
                let mode = publish_mode_from_params(&p)?;
                // The grant this call installs, revokes, or leaves alone.
                // It is bound to the operator's own audit reason, so the
                // pointer records WHICH operator act authorized every
                // later policy acceptance.
                let auto_advance_update = match p.auto_advance {
                    None => AutoAdvanceGrantUpdate::Inherit,
                    Some(enabled) => AutoAdvanceGrantUpdate::Set {
                        enabled,
                        reason: audit_reason.clone(),
                    },
                };
                let receipt = match (&p.attachment_id, &p.source_generation_id) {
                (Some(attachment_id), None) => {
                    if knowledge_transport_governed {
                        anyhow::bail!(
                            "error.knowledge_transport_authoritative: covered projects may advance accepted publication only from a Ready remote candidate"
                        );
                    }
                    let attachment_id = parse_attachment_id(attachment_id)?;
                    let full_ref = p.full_ref.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "error.accepted_publication_ref_missing: attachment publication \
                             requires full_ref"
                        )
                    })?;
                    let catalog_scope = project_catalog_admin::preflight_publish_authority(
                        &store,
                        p.expected_catalog_epoch,
                        &committed,
                        &attachment_id,
                    )
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                    let lease = Arc::new(
                        checkout_access
                            .acquire(CheckoutAccessRequest {
                                project_id: committed.to_string(),
                                attachment: CheckoutAttachmentSelector::AttachmentId(
                                    attachment_id.to_string(),
                                ),
                                expected_scope: Some(catalog_scope),
                                kind: CheckoutAccessKind::PublisherConfigTreeRead,
                                intent: CheckoutAccessIntent::Read,
                                source_lane: CheckoutAccessSourceLane::NativeAttachment,
                            })
                            .map_err(anyhow::Error::new)?,
                    );
                    let probe = publisher_publish_probe(lease.project_root(), full_ref, {
                        let checkout_access = checkout_access.clone();
                        let lease = Arc::clone(&lease);
                        Box::new(move || {
                            checkout_access.revalidate(&lease).map_err(|error| {
                                PublishError::refusal(error.code.as_str(), error.to_string())
                            })
                        })
                    })?;
                    project_catalog_admin::publish_accepted_publication(
                        &store,
                        runtime.as_ref(),
                        &project_catalog_admin::PublisherPublishRequest {
                            mode: mode.clone(),
                            project_id: committed.clone(),
                            attachment_id,
                            full_ref: full_ref.to_string(),
                            expected_epoch: p.expected_catalog_epoch,
                            dry_run: p.dry_run,
                            auto_advance: auto_advance_update.clone(),
                        },
                        probe,
                    )
                }
                (None, Some(source_generation_id)) => {
                    if p.full_ref.is_some() {
                        anyhow::bail!(
                            "error.accepted_publication_candidate_required: candidate publication \
                             derives full_ref from immutable source evidence"
                        );
                    }
                    // The SAME entry point the auto-advance policy calls.
                    // Keeping one candidate-acceptance path is what makes
                    // "policy acceptance validates identically to an
                    // operator acceptance" structural.
                    crate::server::publisher_auto_advance::publish_from_ready_candidate(
                        &store,
                        runtime.as_ref(),
                        producer_auth.as_ref(),
                        knowledge_sources.as_ref(),
                        &committed,
                        source_generation_id,
                        mode.clone(),
                        p.expected_catalog_epoch,
                        p.dry_run,
                        auto_advance_update.clone(),
                    )
                }
                _ => anyhow::bail!(
                    "error.accepted_publication_candidate_required: provide exactly one of \
                     attachment_id or source_generation_id"
                ),
            }
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
                    "auto_advance_grant": match &auto_advance_update {
                        AutoAdvanceGrantUpdate::Inherit => "inherited",
                        AutoAdvanceGrantUpdate::Set { enabled: true, .. } => "granted",
                        AutoAdvanceGrantUpdate::Set { enabled: false, .. } => "revoked",
                    },
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
            self.refresh_published_graph_views(&project_id);
        }
        if succeeded && !dry_run {
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::AcceptedPublicationMutation,
                if remote_candidate_source {
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote
                } else {
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Local
                },
            );
        }
        result
    }

    #[tool(
        name = "bbox_project_publisher_status",
        description = "Read-only accepted-publication status and runtime health for one catalog project. Reports current, prior-fallback, missing, or corrupt state; accepted scope, ref, commit, generation identity, pointer SHA-256, and the typed source binding. Attachment bindings name an attachment; producer bindings name the producer plus source generation id and evidence hash. It also reports scope agreement and advance availability. A `health` object adds the bounded runtime projection: binding status, source kind, recorded attachment capabilities, overlay outcomes, and watcher state. An `auto_advance` object reports the standing operator grant (enabled, the audit reason that installed it, and whether the accepted binding is producer-bound and therefore eligible) plus the last policy attempt and why it did or did not move the pointer. Generation id and pointer SHA-256 are the compare-and-swap tokens bbox_project_publisher_advance requires. The response also echoes the project's catalog `scope`, and for a connector source adds a `connector` object naming the connector_source_id, the connector kind, the producer's observed vendor coordinates, the publication_lanes it claims, and a `file_source` object carrying that lane's state: the active generation with its ordinal, producer, collected selector, document and file counts, logical bytes, cursor epoch, manifest digest, and the producer's publication telemetry (entries enumerated, blobs fetched, documents exported, per-reason skip counters); the per-state generation counts; and the last reported cursor degradation with its cause and cost. `remote_watermark_display_only` is named for what it is and is never freshness authority. A connector project that has onboarded without publishing reports an absent active generation rather than an error, and an unreadable lane reports readable=false with a diagnostic instead of failing the whole call. No credential material appears anywhere in this response. The call is observational, takes no checkout lease, and is path-free. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority."
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
        let server = self.clone();
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
                // A connector project has no published scope to agree or
                // disagree with, exactly like a legacy-local one.
                bbox_corpus_core::project_catalog::ProjectScope::LegacyLocal
                | bbox_corpus_core::project_catalog::ProjectScope::Connector(_) => None,
            };
            let status = runtime
                .status(&project_id, catalog_scope)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let source_binding = status.binding_stamp().map(|stamp| match stamp.source() {
                AcceptedPublicationSourceBinding::Attachment { attachment_id } => json!({
                    "kind": "attachment",
                    "attachment_id": attachment_id.as_str(),
                }),
                AcceptedPublicationSourceBinding::Producer {
                    producer_id,
                    source_generation_id,
                    source_generation_sha256,
                } => json!({
                    "kind": "producer",
                    "producer_id": producer_id,
                    "source_generation_id": source_generation_id,
                    "source_generation_sha256": source_generation_sha256,
                }),
            });
            // The accepted fields stay exactly where they were: the
            // generation id and pointer SHA-256 here are the advance
            // compare-and-swap tokens, and moving them would break every
            // caller that reads them. P5-G ADDS the runtime health view
            // beside them (plan section 8, P5-G mechanic 4).
            let runtime_health = server.state.project_runtime_status(project_id.as_str());
            // Auto-advance state is REPORTED, never inferred by the caller:
            // the standing grant is a pointer fact and the last attempt is
            // the only place a policy refusal is visible without a log
            // dive. A Ready candidate that is not serving must be able to
            // say why (design/daemon-runtime/publisher-auto-advance.md).
            let auto_advance_grant = runtime
                .auto_advance_grant(&project_id)
                .ok()
                .flatten()
                .map(|grant| {
                    json!({
                        "enabled": grant.enabled,
                        "granted_reason": grant.granted_reason,
                        "eligible_binding": grant.source.kind() == "producer",
                    })
                });
            let auto_advance_last_attempt = server
                .state
                .knowledge_sources
                .auto_advance_ledger()
                .last_attempt(project_id.as_str());
            // A connector project reports its family and, since phase 1
            // mounted the lane, its actual file-source publication state.
            // `publication_lanes` names the lanes that exist rather than
            // staying empty: a reader can now tell "this project publishes
            // through file-source" from "this project claims no lane at all",
            // which is the distinction phase 0 could not express.
            let connector = project.scope.connector().map(|scope| {
                let file_source =
                    connector_publication_json(server.state.file_sources.store().as_ref(), scope);
                json!({
                    "connector_source_id": scope.connector_source_id().as_str(),
                    "connector_kind": scope.connector_kind().as_str(),
                    "observations": connector_observations_json(state.catalog(), project),
                    "publication_lanes": vec!["file_source"],
                    "file_source": file_source,
                })
            });
            Ok(serde_json::to_string_pretty(&json!({
                "project_id": project_id.as_str(),
                "scope": scope_json(&project.scope),
                "connector": connector,
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
                "source_binding": source_binding,
                "attachment_id": status.binding_stamp().and_then(|stamp| stamp.attachment_id()).map(|id| id.as_str()),
                "pointer_sha256": status.binding_stamp().map(|stamp| stamp.pointer_sha256()),
                "diagnostic": status.failure().map(|failure| failure.code()),
                "epoch": state.epoch(),
                "auto_advance": {
                    "grant": auto_advance_grant,
                    "last_attempt": auto_advance_last_attempt,
                },
                "health": runtime_health,
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

pub(crate) struct NominationOutcome {
    pub(crate) recorded: Vec<String>,
    pub(crate) epoch: Option<u64>,
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
pub(crate) fn ingest_alias_nominations(
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
    project_dir: &Path,
    full_ref: &str,
    revalidate_checkout: Box<dyn Fn() -> Result<(), PublishError> + Send + Sync>,
) -> anyhow::Result<project_catalog_admin::PublisherPublishProbe> {
    let project_dir = project_dir.to_path_buf();
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
            graphs: Vec::new(),
            evidence: Vec::new(),
        },
        revalidate_checkout,
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
                ProjectScope::Connector(_) => {
                    "this project is connector-scoped and owns no local checkout, so there \
                     is nothing to relocate"
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

    fn cover_knowledge_transport_project(
        server: &mut BlackboxServer,
        project_id: &str,
        scope: PublishedScope,
    ) {
        use bbox_indexing::knowledge_transport_cutover::{
            KnowledgeTransportCapabilityBaselineV1, KnowledgeTransportCutoverMarkerV1,
            KnowledgeTransportCutoverRuntimeV1, PredictedKnowledgeTransportCutoverRowV1,
        };
        use bbox_indexing::project_catalog_inventory::Sha256ValueV1;

        let capabilities = [
            CheckoutAccessKind::PublisherConfigTreeRead,
            CheckoutAccessKind::KnowledgeGapOverlayRead,
            CheckoutAccessKind::ArtifactWatchDiscovery,
            CheckoutAccessKind::RepositoryMutation,
        ];
        let marker = KnowledgeTransportCutoverMarkerV1 {
            version: 1,
            applied_at: "unix:1".into(),
            report_artifact_hash: Sha256ValueV1::digest(b"report"),
            resolution_artifact_hash: Sha256ValueV1::digest(b"resolution"),
            predecessor_marker_checksum: None,
            predecessor_catalog_epoch: 1,
            inventory_hash: Sha256ValueV1::digest(b"inventory"),
            observation_snapshot_hash: Sha256ValueV1::digest(b"observations"),
            rows: vec![PredictedKnowledgeTransportCutoverRowV1 {
                project_id: ProjectId::parse(project_id).unwrap(),
                scope,
                producer_id: "producer".into(),
                grant_commitment: Sha256ValueV1::digest(b"grant"),
                accepted_generation_id: "a".repeat(64),
                accepted_generation_sha256: "b".repeat(64),
                accepted_pointer_sha256: "c".repeat(64),
                source_generation_id: format!("kps_{}", "d".repeat(64)),
                source_generation_sha256: "e".repeat(64),
                publication_parity_commitment: Sha256ValueV1::digest(b"publication"),
                parity_workspace_ids: Vec::new(),
                workspace_parity_commitment: Sha256ValueV1::digest(b"workspace"),
                shadow_observation_commitment: Sha256ValueV1::digest(b"shadow"),
                capability_baselines: capabilities
                    .into_iter()
                    .map(|capability| KnowledgeTransportCapabilityBaselineV1 {
                        capability,
                        granted: 0,
                        denied: 0,
                    })
                    .collect(),
                observation_window_start_sequence: 0,
                observation_window_end_sequence: 0,
            }],
            checksum_sha256: Sha256ValueV1::digest(b"test fixture bypasses marker decoding"),
        };
        Arc::get_mut(&mut server.state)
            .expect("test server has one state owner")
            .knowledge_transport_cutover = Arc::new(
            KnowledgeTransportCutoverRuntimeV1::from_marker(Some(marker)),
        );
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
                attachment_id: Some("att_00000000000000000000000000000000".into()),
                source_generation_id: None,
                mode: mode.into(),
                full_ref: Some("refs/heads/main".into()),
                expected_generation_id: generation.map(str::to_owned),
                expected_pointer_sha256: pointer.map(str::to_owned),
                auto_advance: None,
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
                    attachment_id: Some("att_00000000000000000000000000000000".into()),
                    source_generation_id: None,
                    mode: "establish".into(),
                    full_ref: Some("refs/heads/main".into()),
                    expected_generation_id: None,
                    expected_pointer_sha256: None,
                    auto_advance: None,
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
    fn connector_scope_renders_as_its_own_family() {
        use bbox_corpus_core::project_catalog::ConnectorScope;

        let rendered = scope_json(&ProjectScope::Connector(
            ConnectorScope::try_new("csrc_5f2c1d9a4b6e470e", "gdrive").unwrap(),
        ));
        assert_eq!(
            rendered,
            json!({
                "kind": "connector",
                "connector_source_id": "csrc_5f2c1d9a4b6e470e",
                "connector_kind": "gdrive",
            }),
            "the scope object carries identity only, never a vendor coordinate"
        );
        assert_eq!(
            scope_json(&ProjectScope::LegacyLocal),
            json!({ "kind": "legacy_local" }),
            "the other families render exactly as before"
        );
        assert_eq!(
            scope_json(&ProjectScope::Published(
                PublishedScope::try_new("repo-a", ".").unwrap()
            )),
            json!({
                "kind": "published",
                "repo_id": "repo-a",
                "bbox_root_relpath": ".",
            })
        );
    }

    #[test]
    fn connector_observations_render_beside_the_scope_not_inside_it() {
        use bbox_corpus_core::project_catalog::{
            CatalogSnapshotV2, ConnectorObservationsV1, ConnectorScope, CorpusProject,
        };
        use std::collections::BTreeSet;

        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        let connector_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();
        let published_id = ProjectId::parse("p_000000000000000000000000000000b1").unwrap();
        let project = |project_id: &ProjectId, scope: ProjectScope| CorpusProject {
            project_id: project_id.clone(),
            scope,
            operator_aliases: BTreeSet::new(),
            nominated_aliases: BTreeSet::new(),
            display_name: "Example".into(),
            created_at: "2026-08-13T00:00:00Z".into(),
            registered_at_compat: None,
            repo_history: None,
            languages: BTreeSet::new(),
        };
        let connector_project = project(
            &connector_id,
            ProjectScope::Connector(
                ConnectorScope::try_new("csrc_5f2c1d9a4b6e470e", "gdrive").unwrap(),
            ),
        );
        let published_project = project(
            &published_id,
            ProjectScope::Published(PublishedScope::try_new("repo-a", ".").unwrap()),
        );
        catalog
            .projects
            .insert(connector_id.clone(), connector_project.clone());
        catalog
            .projects
            .insert(published_id.clone(), published_project.clone());
        catalog.connector_observations.insert(
            connector_id.clone(),
            ConnectorObservationsV1 {
                observed_at: "2026-08-13T00:00:00Z".into(),
                producer_id: Some("producer-a".into()),
                remote_authority: Some("tenant.example".into()),
                remote_root_id: Some("0ABcDeFgHiJkLmN".into()),
                remote_display_name: Some("Ops shared folder".into()),
            },
        );
        catalog.sync_version();
        catalog.validate().unwrap();

        assert_eq!(
            connector_observations_json(&catalog, &connector_project),
            json!({
                "observed_at": "2026-08-13T00:00:00Z",
                "producer_id": "producer-a",
                "remote_authority": "tenant.example",
                "remote_root_id": "0ABcDeFgHiJkLmN",
                "remote_display_name": "Ops shared folder",
            })
        );
        assert_eq!(
            connector_observations_json(&catalog, &published_project),
            serde_json::Value::Null,
            "only a connector project can carry connector observations"
        );

        // A connector project onboarded but never reported on renders null
        // observations rather than an invented coordinate.
        catalog.connector_observations.remove(&connector_id);
        assert_eq!(
            connector_observations_json(&catalog, &connector_project),
            serde_json::Value::Null
        );
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
        let probe = publisher_publish_probe(
            Path::new(&row.checkout_project_dir),
            "refs/heads/main",
            Box::new(|| Ok(())),
        )
        .unwrap();
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
        let error = publisher_publish_probe(
            Path::new(&row.checkout_project_dir),
            "refs/heads/does-not-exist",
            Box::new(|| Ok(())),
        )
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
        let error = publisher_publish_probe(
            Path::new(&row.checkout_project_dir),
            "refs/heads/dangling",
            Box::new(|| Ok(())),
        )
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

        let error = publisher_publish_probe(
            Path::new(&row.checkout_project_dir),
            "refs/heads/identityless",
            Box::new(|| Ok(())),
        )
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
                    source: None,
                    author: None,
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
        fixture.attach_overlay_checkout(
            "p_dryrun",
            &scope,
            &checkout,
            "att_00000000000000000000000000000d01",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01",
            true,
        );
        fixture.install_publication(
            "p_dryrun",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "generationone")],
            &[],
        );
        let server = fixture.server_with_checkout_authority();
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
                attachment_id: Some("att_00000000000000000000000000000d01".into()),
                source_generation_id: None,
                mode: "advance".into(),
                full_ref: Some("refs/heads/main".into()),
                expected_generation_id: Some(tokens.0),
                expected_pointer_sha256: Some(tokens.1),
                auto_advance: None,
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

    /// KT-F closeout: the attachment-backed publisher remains a live local
    /// adapter for uncovered projects, while a covered row refuses before
    /// the checkout broker can acquire or deny a lease. The positive control
    /// prevents this from passing merely because the fixture never had a
    /// usable checkout.
    #[tokio::test]
    async fn strict_knowledge_transport_closes_only_the_covered_publisher_adapter() {
        use crate::server::state::catalog_fixture::CatalogFixture;

        const PROJECT_ID: &str = "p_covered_publisher";
        const ATTACHMENT_ID: &str = "att_00000000000000000000000000000e02";
        const CHECKOUT_ID: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeee02";

        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().canonicalize().unwrap().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        publishing_checkout(&checkout);
        let scope = PublishedScope::try_new("repo_probe", ".").unwrap();

        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT_ID, &scope);
        fixture.attach_overlay_checkout(
            PROJECT_ID,
            &scope,
            &checkout,
            ATTACHMENT_ID,
            CHECKOUT_ID,
            true,
        );
        let mut server = fixture.server_with_checkout_authority();

        let before_local = server.state.checkout_access.health().sequence;
        let local = server
            .bbox_project_publisher_advance(Parameters(ProjectPublisherAdvanceParams {
                project_id: PROJECT_ID.into(),
                attachment_id: Some(ATTACHMENT_ID.into()),
                source_generation_id: None,
                mode: "establish".into(),
                full_ref: Some("refs/heads/main".into()),
                expected_generation_id: None,
                expected_pointer_sha256: None,
                auto_advance: None,
                dry_run: true,
                expected_catalog_epoch: fixture.epoch(),
                audit_reason: "uncovered positive control".into(),
            }))
            .await;
        assert!(!local.is_error.unwrap_or(false), "{}", error_text(&local));
        let after_local = server.state.checkout_access.health().sequence;
        assert!(
            after_local > before_local,
            "positive control must reach the checkout broker"
        );

        cover_knowledge_transport_project(&mut server, PROJECT_ID, scope);
        let covered = server
            .bbox_project_publisher_advance(Parameters(ProjectPublisherAdvanceParams {
                project_id: PROJECT_ID.into(),
                attachment_id: Some(ATTACHMENT_ID.into()),
                source_generation_id: None,
                mode: "establish".into(),
                full_ref: Some("refs/heads/main".into()),
                expected_generation_id: None,
                expected_pointer_sha256: None,
                auto_advance: None,
                dry_run: true,
                expected_catalog_epoch: fixture.epoch(),
                audit_reason: "covered refusal".into(),
            }))
            .await;
        let text = error_text(&covered);
        assert!(covered.is_error.unwrap_or(false), "{text}");
        assert!(
            text.contains("error.knowledge_transport_authoritative"),
            "{text}"
        );
        assert_eq!(
            server.state.checkout_access.health().sequence,
            after_local,
            "covered publisher request must refuse before checkout acquisition"
        );
    }

    /// A covered project gap write must reach the checkout-owner
    /// backchannel even when the checkout is absent from the daemon's
    /// filesystem (the zero-authority cage shape): the daemon validates,
    /// mints, and enqueues the exact committed-file bytes for collector
    /// delivery instead of touching a checkout lease.
    #[tokio::test]
    async fn covered_project_gap_write_enqueues_for_checkout_owner_delivery() {
        use crate::server::state::catalog_fixture::CatalogFixture;

        covered_project_gap_write_enqueues(CatalogFixture::new()).await;
    }

    /// The same admission over a store the OPERATOR genesis path produced.
    ///
    /// Greenfield onboarding is the case `project-catalog genesis` exists to
    /// serve, and it is only useful if the resulting store admits the
    /// collector backchannel exactly as a store reached any other way does.
    /// Running the identical assertions over both fixtures is what proves the
    /// genesis store is not a second-class one.
    #[tokio::test]
    async fn genesis_store_admits_the_covered_project_gap_backchannel() {
        use crate::server::state::catalog_fixture::CatalogFixture;

        covered_project_gap_write_enqueues(CatalogFixture::new_over_genesis_store()).await;
    }

    async fn covered_project_gap_write_enqueues(
        fixture: crate::server::state::catalog_fixture::CatalogFixture,
    ) {
        const PROJECT_ID: &str = "p_covered_gap_write0";
        const ATTACHMENT_ID: &str = "att_00000000000000000000000000000e03";
        const CHECKOUT_ID: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeee03";

        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().canonicalize().unwrap().join("absent-checkout");
        let scope = PublishedScope::try_new("repo_probe", ".").unwrap();

        fixture.add_published_project(PROJECT_ID, &scope);
        fixture.attach_overlay_checkout(
            PROJECT_ID,
            &scope,
            &checkout,
            ATTACHMENT_ID,
            CHECKOUT_ID,
            true,
        );
        let mut server = fixture.server_with_checkout_authority();
        cover_knowledge_transport_project(&mut server, PROJECT_ID, scope.clone());

        let before = server.state.checkout_access.health().sequence;
        let result = server
            .bbox_gap(Parameters(crate::gaps::GapFileParams {
                title: "covered gap".into(),
                gap_kind: "tooling".into(),
                domain: "test-domain".into(),
                wanted_capability: "file through the daemon".into(),
                dedupe_key: "tooling/test-domain/covered-gap".into(),
                impact: None,
                blocking_level: None,
                missing_primitive: None,
                fallback_used: None,
                evidence: None,
                suggested_owner: None,
                notes: None,
                scope: Some("project".into()),
                project: Some(checkout.to_string_lossy().into_owned()),
                project_id: None,
                write_dir: None,
                task_id: None,
                session_id: None,
                provider: None,
                bro: None,
                thread_id: None,
                allow_recurrence: None,
            }))
            .await;
        let text = format!("{:?}", result.content);
        assert!(!result.is_error.unwrap_or(false), "{text}");
        assert!(text.contains("checkout-owner lane"), "{text}");
        assert_eq!(
            server.state.checkout_access.health().sequence,
            before,
            "the backchannel must not acquire a checkout lease"
        );
        let pending = server.state.checkout_mutations.read();
        assert_eq!(pending.pending_count(), 1);
        let (mutations, deferred) = pending.poll(&std::collections::BTreeSet::from([scope]));
        assert_eq!(deferred, 0);
        let mutation = &mutations[0];
        assert_eq!(mutation.mode, "write");
        assert!(mutation.relative_path.starts_with(".bbox/gaps/gap-"));
        let content = mutation.content_json.as_deref().unwrap();
        assert!(content.contains("covered gap"), "{content}");
        assert!(
            content.contains("tooling/test-domain/covered-gap"),
            "{content}"
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
                attachment_id: Some("att_00000000000000000000000000000f01".into()),
                source_generation_id: None,
                mode: "establish".into(),
                full_ref: Some("refs/heads/main".into()),
                expected_generation_id: None,
                expected_pointer_sha256: None,
                auto_advance: None,
                dry_run: false,
                expected_catalog_epoch: 9_999,
                audit_reason: "stale epoch".into(),
            }))
            .await;
        let text = error_text(&stale);
        assert!(text.contains("error.project_catalog_stale_epoch"), "{text}");

        let stale_candidate = server
            .bbox_project_publisher_advance(Parameters(ProjectPublisherAdvanceParams {
                project_id: "p_denied".into(),
                attachment_id: None,
                source_generation_id: Some(format!("kps_{}", "1".repeat(64))),
                mode: "establish".into(),
                full_ref: None,
                expected_generation_id: None,
                expected_pointer_sha256: None,
                auto_advance: None,
                dry_run: false,
                expected_catalog_epoch: 9_999,
                audit_reason: "stale candidate epoch".into(),
            }))
            .await;
        let text = error_text(&stale_candidate);
        assert!(text.contains("error.project_catalog_stale_epoch"), "{text}");
        assert!(
            !text.contains("error.accepted_publication_candidate_required"),
            "candidate existence was consulted before catalog authority: {text}"
        );

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
                attachment_id: Some("att_00000000000000000000000000000f01".into()),
                source_generation_id: None,
                mode: "establish".into(),
                full_ref: Some("refs/heads/main".into()),
                expected_generation_id: None,
                expected_pointer_sha256: None,
                auto_advance: None,
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

    #[tokio::test]
    async fn ready_remote_candidate_publishes_without_a_checkout() {
        use std::io::Cursor;

        use crate::server::state::catalog_fixture::{COMMIT_ONE, CatalogFixture, knowledge_entry};
        use bbox_knowledge_source::{
            GitObjectFormatV1, PublicationCandidateDescriptorV1, SCHEMA_VERSION,
            SourceFileManifestEntryV1, SourceLaneV1, SourceManifestDescriptorV1,
            SourceManifestPageV1, source_file_blob_sha256, source_manifest_sha256,
        };
        use bbox_knowledge_source_store::PublicationAuthorityV1;

        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_candidate_tool", &scope);
        let server = fixture.server();
        let catalog = fixture.store().snapshot().unwrap().catalog().clone();
        server
            .state
            .code_sources
            .install_auth_for_test(std::sync::Arc::new(
                crate::server::producer_auth::ProducerAuthRuntime::for_test_catalog(
                    vec![(
                        bro_rpc::ServiceToken::parse("1".repeat(64)).unwrap(),
                        crate::server::producer_auth::ProducerGrant {
                            producer_id: "producer-a".into(),
                            projects: std::collections::BTreeMap::from([(
                                scope.clone(),
                                "p_candidate_tool".into(),
                            )]),
                        },
                    )],
                    catalog.as_ref(),
                ),
            ));
        let store = server.state.knowledge_sources.store();
        let source_bytes =
            serde_json::to_vec(&knowledge_entry("knowledge-a", "remote candidate content"))
                .unwrap();
        let manifest_entry = SourceFileManifestEntryV1 {
            repository_relative_filename: ".bbox/knowledge/knowledge-a.json".into(),
            encoded_bytes: source_bytes.len() as u64,
            content_sha256: source_file_blob_sha256(&source_bytes),
        };
        let knowledge_manifest = vec![manifest_entry.clone()];
        let graph_sources = [
            (
                "edges.jsonl",
                include_bytes!(
                    "../../crates/bbox-project-graph/tests/fixtures/governance-record/edges.jsonl"
                )
                .as_slice(),
            ),
            (
                "graph.json",
                include_bytes!(
                    "../../crates/bbox-project-graph/tests/fixtures/governance-record/graph.json"
                )
                .as_slice(),
            ),
            (
                "schema.json",
                include_bytes!(
                    "../../crates/bbox-project-graph/tests/fixtures/governance-record/schema.json"
                )
                .as_slice(),
            ),
            (
                "vertices.jsonl",
                include_bytes!(
                    "../../crates/bbox-project-graph/tests/fixtures/governance-record/vertices.jsonl"
                )
                .as_slice(),
            ),
        ];
        let graph_manifest = graph_sources
            .iter()
            .map(|(filename, bytes)| SourceFileManifestEntryV1 {
                repository_relative_filename: format!(".bbox/graphs/governance-record/{filename}"),
                encoded_bytes: bytes.len() as u64,
                content_sha256: source_file_blob_sha256(bytes),
            })
            .collect::<Vec<_>>();
        let descriptor = PublicationCandidateDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope: scope.clone(),
            full_ref: "refs/heads/main".into(),
            publisher_commit: COMMIT_ONE.into(),
            object_format: GitObjectFormatV1::Sha1,
            knowledge: SourceManifestDescriptorV1 {
                manifest_sha256: source_manifest_sha256(
                    SourceLaneV1::Knowledge,
                    &knowledge_manifest,
                ),
                file_count: 1,
                logical_bytes: source_bytes.len() as u64,
                page_count: 1,
            },
            gaps: SourceManifestDescriptorV1 {
                manifest_sha256: source_manifest_sha256(SourceLaneV1::Gaps, &[]),
                file_count: 0,
                logical_bytes: 0,
                page_count: 0,
            },
            graphs: SourceManifestDescriptorV1 {
                manifest_sha256: source_manifest_sha256(SourceLaneV1::Graphs, &graph_manifest),
                file_count: graph_manifest.len() as u64,
                logical_bytes: graph_sources
                    .iter()
                    .map(|(_, bytes)| bytes.len() as u64)
                    .sum(),
                page_count: 1,
            },
            evidence: SourceManifestDescriptorV1 {
                manifest_sha256: source_manifest_sha256(SourceLaneV1::Evidence, &[]),
                file_count: 0,
                logical_bytes: 0,
                page_count: 0,
            },
        };
        let authority = PublicationAuthorityV1 {
            producer_id: "producer-a".into(),
            project_id: "p_candidate_tool".into(),
            scope,
        };
        let upload = store
            .begin_publication_upload(&authority, descriptor)
            .unwrap();
        store
            .put_publication_manifest_page(
                &authority,
                &upload.upload_id,
                SourceLaneV1::Knowledge,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: knowledge_manifest,
                },
            )
            .unwrap();
        store
            .put_publication_manifest_page(
                &authority,
                &upload.upload_id,
                SourceLaneV1::Graphs,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: graph_manifest.clone(),
                },
            )
            .unwrap();
        store
            .missing_publication_blobs(&authority, &upload.upload_id, None)
            .unwrap();
        store
            .install_publication_blob(
                &authority,
                &upload.upload_id,
                &manifest_entry.content_sha256,
                manifest_entry.encoded_bytes,
                Cursor::new(source_bytes),
            )
            .unwrap();
        for ((_, source_bytes), manifest_entry) in graph_sources.iter().zip(&graph_manifest) {
            store
                .install_publication_blob(
                    &authority,
                    &upload.upload_id,
                    &manifest_entry.content_sha256,
                    manifest_entry.encoded_bytes,
                    Cursor::new(*source_bytes),
                )
                .unwrap();
        }
        let source_generation_id = store
            .finalize_publication_upload(&authority, &upload.upload_id)
            .unwrap()
            .source_generation_id;
        let epoch = server
            .state
            .project_authority
            .catalog_store()
            .unwrap()
            .snapshot()
            .unwrap()
            .epoch();

        let result = server
            .bbox_project_publisher_advance(Parameters(ProjectPublisherAdvanceParams {
                project_id: "p_candidate_tool".into(),
                attachment_id: None,
                source_generation_id: Some(source_generation_id.clone()),
                mode: "establish".into(),
                full_ref: None,
                expected_generation_id: None,
                expected_pointer_sha256: None,
                auto_advance: None,
                dry_run: false,
                expected_catalog_epoch: epoch,
                audit_reason: "accept remote candidate".into(),
            }))
            .await;
        assert_ne!(result.is_error, Some(true), "{}", error_text(&result));

        let status = server
            .bbox_project_publisher_status(Parameters(ProjectPublisherStatusParams {
                project_id: "p_candidate_tool".into(),
            }))
            .await;
        assert_ne!(status.is_error, Some(true), "{}", error_text(&status));
        let body: serde_json::Value = serde_json::from_str(&error_text(&status)).unwrap();
        assert_eq!(body["source_binding"]["kind"], "producer");
        assert_eq!(
            body["source_binding"]["source_generation_id"],
            source_generation_id
        );
        assert!(body["attachment_id"].is_null());

        let listed = server
            .bbox_project_graph_list(Parameters(crate::tools::graph::ProjectGraphListParams {
                project: Some("p_candidate_tool".into()),
                visibility: Some("published".into()),
            }))
            .await;
        let listed_text = error_text(&listed);
        assert!(listed_text.contains("governance-record"), "{listed_text}");

        let described = server
            .bbox_project_graph_describe(Parameters(crate::tools::graph::ProjectGraphExactParams {
                project: "p_candidate_tool".into(),
                graph_id: "governance-record".into(),
                visibility: Some("published".into()),
            }))
            .await;
        let described_text = error_text(&described);
        assert!(
            described_text.contains("governance-record-schema"),
            "{described_text}"
        );

        let inspected = server
            .bbox_inspect_entity(Parameters(crate::mcp_tools::inspect::InspectEntityParams {
                entity_ref: "project_graph_vertex:p_candidate_tool:governance-record:record/case@2"
                    .into(),
                provisional: Some("published".into()),
                edge_types: None,
                direction: Some("both".into()),
                per_type_limit: Some(10),
                property_mode: Some("full".into()),
            }))
            .await;
        let inspected_text = error_text(&inspected);
        assert!(inspected_text.contains("record/case@2"), "{inspected_text}");
    }

    // ── Auto-advance policy ─────────────────────────────────────────
    //
    // design/daemon-runtime/publisher-auto-advance.md. These drive the
    // real HTTP-free store path: a producer grant, a real Ready candidate
    // in the knowledge-source store, and the real acceptance path.

    /// Stand up a published project with a producer grant, and return the
    /// server plus a closure that mints one Ready publication candidate.
    ///
    /// It returns the fixture too: dropping a `CatalogFixture` removes its
    /// tempdir, so the caller must hold it for the life of the test.
    struct AutoAdvanceFixture {
        fixture: crate::server::state::catalog_fixture::CatalogFixture,
        server: crate::server::BlackboxServer,
        scope: bbox_corpus_core::identity::PublishedScope,
        project_id: String,
    }

    impl AutoAdvanceFixture {
        fn new(project_id: &str) -> Self {
            use crate::server::state::catalog_fixture::CatalogFixture;

            let fixture = CatalogFixture::new();
            let scope = CatalogFixture::scope(".");
            fixture.add_published_project(project_id, &scope);
            let server = fixture.server();
            let catalog = fixture.store().snapshot().unwrap().catalog().clone();
            server
                .state
                .code_sources
                .install_auth_for_test(std::sync::Arc::new(
                    crate::server::producer_auth::ProducerAuthRuntime::for_test_catalog(
                        vec![(
                            bro_rpc::ServiceToken::parse("1".repeat(64)).unwrap(),
                            crate::server::producer_auth::ProducerGrant {
                                producer_id: "producer-a".into(),
                                projects: std::collections::BTreeMap::from([(
                                    scope.clone(),
                                    project_id.into(),
                                )]),
                            },
                        )],
                        catalog.as_ref(),
                    ),
                ));
            Self {
                fixture,
                server,
                scope,
                project_id: project_id.to_string(),
            }
        }

        fn epoch(&self) -> u64 {
            self.fixture.store().snapshot().unwrap().epoch()
        }

        /// Upload and finalize one Ready candidate WITHOUT going through
        /// the HTTP finalize handler, so the policy trigger is not fired.
        /// Tests that want the trigger call `finalize_through_trigger`.
        fn stage_candidate(&self, entry_id: &str, content: &str, commit: &str) -> String {
            self.stage_candidate_at(entry_id, content, commit, "refs/heads/main", &self.scope)
        }

        fn stage_candidate_at(
            &self,
            entry_id: &str,
            content: &str,
            commit: &str,
            full_ref: &str,
            scope: &bbox_corpus_core::identity::PublishedScope,
        ) -> String {
            use std::io::Cursor;

            use crate::server::state::catalog_fixture::knowledge_entry;
            use bbox_knowledge_source::{
                GitObjectFormatV1, PublicationCandidateDescriptorV1, SCHEMA_VERSION,
                SourceFileManifestEntryV1, SourceLaneV1, SourceManifestDescriptorV1,
                SourceManifestPageV1, source_file_blob_sha256, source_manifest_sha256,
            };
            use bbox_knowledge_source_store::PublicationAuthorityV1;

            let store = self.server.state.knowledge_sources.store();
            let source_bytes = serde_json::to_vec(&knowledge_entry(entry_id, content)).unwrap();
            let manifest_entry = SourceFileManifestEntryV1 {
                repository_relative_filename: format!(".bbox/knowledge/{entry_id}.json"),
                encoded_bytes: source_bytes.len() as u64,
                content_sha256: source_file_blob_sha256(&source_bytes),
            };
            let knowledge_manifest = vec![manifest_entry.clone()];
            let descriptor = PublicationCandidateDescriptorV1 {
                schema_version: SCHEMA_VERSION,
                scope: scope.clone(),
                full_ref: full_ref.into(),
                publisher_commit: commit.into(),
                object_format: GitObjectFormatV1::Sha1,
                knowledge: SourceManifestDescriptorV1 {
                    manifest_sha256: source_manifest_sha256(
                        SourceLaneV1::Knowledge,
                        &knowledge_manifest,
                    ),
                    file_count: 1,
                    logical_bytes: source_bytes.len() as u64,
                    page_count: 1,
                },
                gaps: SourceManifestDescriptorV1 {
                    manifest_sha256: source_manifest_sha256(SourceLaneV1::Gaps, &[]),
                    file_count: 0,
                    logical_bytes: 0,
                    page_count: 0,
                },
                graphs: SourceManifestDescriptorV1 {
                    manifest_sha256: source_manifest_sha256(SourceLaneV1::Graphs, &[]),
                    file_count: 0,
                    logical_bytes: 0,
                    page_count: 0,
                },
                evidence: SourceManifestDescriptorV1 {
                    manifest_sha256: source_manifest_sha256(SourceLaneV1::Evidence, &[]),
                    file_count: 0,
                    logical_bytes: 0,
                    page_count: 0,
                },
            };
            let authority = PublicationAuthorityV1 {
                producer_id: "producer-a".into(),
                project_id: self.project_id.clone(),
                scope: scope.clone(),
            };
            let upload = store
                .begin_publication_upload(&authority, descriptor)
                .unwrap();
            store
                .put_publication_manifest_page(
                    &authority,
                    &upload.upload_id,
                    SourceLaneV1::Knowledge,
                    0,
                    &SourceManifestPageV1 {
                        page_index: 0,
                        entries: knowledge_manifest,
                    },
                )
                .unwrap();
            // NOT an idle query. `missing_publication_blobs` is the only
            // caller of the store's manifest-completion step, so it is what
            // seals the manifest and moves the upload from
            // ReceivingManifest to MissingBlobs. Skipping it leaves every
            // later call refusing with InvalidState, which is why the real
            // route order puts GET .../missing between the manifest pages
            // and the first blob PUT.
            let missing = store
                .missing_publication_blobs(&authority, &upload.upload_id, None)
                .unwrap();
            assert_eq!(
                missing.hashes,
                vec![manifest_entry.content_sha256.clone()],
                "the sealed manifest names exactly the blob this candidate is about to upload"
            );
            store
                .install_publication_blob(
                    &authority,
                    &upload.upload_id,
                    &manifest_entry.content_sha256,
                    manifest_entry.encoded_bytes,
                    Cursor::new(source_bytes),
                )
                .unwrap();
            store
                .finalize_publication_upload(&authority, &upload.upload_id)
                .unwrap()
                .source_generation_id
        }

        async fn establish_from(
            &self,
            source_generation_id: &str,
            auto_advance: Option<bool>,
            audit_reason: &str,
        ) -> CallToolResult {
            self.server
                .bbox_project_publisher_advance(Parameters(ProjectPublisherAdvanceParams {
                    project_id: self.project_id.clone(),
                    attachment_id: None,
                    source_generation_id: Some(source_generation_id.to_string()),
                    mode: "establish".into(),
                    full_ref: None,
                    expected_generation_id: None,
                    expected_pointer_sha256: None,
                    auto_advance,
                    dry_run: false,
                    expected_catalog_epoch: self.epoch(),
                    audit_reason: audit_reason.into(),
                }))
                .await
        }

        async fn status(&self) -> serde_json::Value {
            let status = self
                .server
                .bbox_project_publisher_status(Parameters(ProjectPublisherStatusParams {
                    project_id: self.project_id.clone(),
                }))
                .await;
            assert_ne!(status.is_error, Some(true), "{}", error_text(&status));
            serde_json::from_str(&error_text(&status)).unwrap()
        }
    }

    /// Default OFF. A project whose operator never granted the policy sees
    /// a Ready candidate arrive and keeps serving the generation it was
    /// serving before.
    #[tokio::test]
    async fn policy_off_leaves_a_ready_candidate_unaccepted() {
        use crate::server::state::catalog_fixture::COMMIT_ONE;

        let fixture = AutoAdvanceFixture::new("p_policy_off");
        let first = fixture.stage_candidate("knowledge-a", "first", COMMIT_ONE);
        let result = fixture
            .establish_from(&first, None, "operator establishes")
            .await;
        assert_ne!(result.is_error, Some(true), "{}", error_text(&result));
        let before = fixture.status().await;

        let second = fixture.stage_candidate(
            "knowledge-a",
            "second",
            "2222222222222222222222222222222222222222",
        );
        let outcome = fixture
            .server
            .attempt_publisher_auto_advance("p_policy_off", &second);
        assert_eq!(
            outcome,
            crate::server::publisher_auto_advance::AutoAdvanceOutcome::PolicyDisabled
        );

        let after = fixture.status().await;
        assert_eq!(
            after["generation_id"], before["generation_id"],
            "an ungranted project does not move its pointer"
        );
        assert_eq!(after["auto_advance"]["grant"]["enabled"], false);
        assert_eq!(
            after["auto_advance"]["last_attempt"]["outcome"],
            "policy_disabled"
        );
    }

    /// The activation rule. The grant is read from the CURRENTLY ACCEPTED
    /// generation's pointer, so the operator advance that installs it does
    /// NOT retroactively authorize itself, and the next candidate is the
    /// first one the policy may accept.
    #[tokio::test]
    async fn the_grant_is_read_from_the_accepted_pointer_not_the_incoming_candidate() {
        use crate::server::state::catalog_fixture::COMMIT_ONE;

        let fixture = AutoAdvanceFixture::new("p_policy_activation");
        let first = fixture.stage_candidate("knowledge-a", "first", COMMIT_ONE);
        // Before any pointer exists there is nothing to read a grant from,
        // and establish stays manual.
        let premature = fixture
            .server
            .attempt_publisher_auto_advance("p_policy_activation", &first);
        assert_eq!(
            premature,
            crate::server::publisher_auto_advance::AutoAdvanceOutcome::NoAcceptedPublication
        );

        let granted = fixture
            .establish_from(&first, Some(true), "operator grants auto-advance")
            .await;
        assert_ne!(granted.is_error, Some(true), "{}", error_text(&granted));
        let status = fixture.status().await;
        assert_eq!(status["auto_advance"]["grant"]["enabled"], true);
        assert_eq!(
            status["auto_advance"]["grant"]["granted_reason"],
            "operator grants auto-advance"
        );
        assert_eq!(status["auto_advance"]["grant"]["eligible_binding"], true);
    }

    /// Policy on: one Ready candidate from the bound producer, on the same
    /// scope and ref, advances exactly once and stamps the policy audit
    /// reason. A second attempt for the same candidate does nothing.
    #[tokio::test]
    async fn policy_on_advances_a_ready_candidate_exactly_once() {
        use crate::server::state::catalog_fixture::{COMMIT_ONE, COMMIT_TWO};

        let fixture = AutoAdvanceFixture::new("p_policy_on");
        let first = fixture.stage_candidate("knowledge-a", "first", COMMIT_ONE);
        fixture
            .establish_from(&first, Some(true), "operator grants auto-advance")
            .await;
        let before = fixture.status().await;

        let second = fixture.stage_candidate("knowledge-a", "second", COMMIT_TWO);
        let outcome = fixture
            .server
            .attempt_publisher_auto_advance("p_policy_on", &second);
        assert!(outcome.accepted(), "{outcome:?}");

        let after = fixture.status().await;
        assert_ne!(
            after["generation_id"], before["generation_id"],
            "the policy moved the accepted pointer"
        );
        assert_eq!(after["accepted_commit"], COMMIT_TWO);
        assert_eq!(after["source_binding"]["kind"], "producer");
        assert_eq!(after["source_binding"]["source_generation_id"], second);
        // The grant survives the policy's own advance: a policy acceptance
        // inherits, it does not re-grant.
        assert_eq!(after["auto_advance"]["grant"]["enabled"], true);
        assert_eq!(
            after["auto_advance"]["grant"]["granted_reason"],
            "operator grants auto-advance"
        );
        assert_eq!(after["auto_advance"]["last_attempt"]["outcome"], "accepted");
        assert_eq!(
            after["auto_advance"]["last_attempt"]["source_generation_id"],
            second
        );

        // Exactly once. A repeated finalize of the same upload must not
        // produce a second attempt.
        let repeat = fixture
            .server
            .attempt_publisher_auto_advance("p_policy_on", &second);
        assert_eq!(
            repeat,
            crate::server::publisher_auto_advance::AutoAdvanceOutcome::AlreadyAttempted
        );
        let unchanged = fixture.status().await;
        assert_eq!(unchanged["generation_id"], after["generation_id"]);
    }

    /// The policy audit trail names the policy, the producer, and the
    /// source generation, so a policy acceptance is distinguishable from
    /// an operator one after the fact.
    #[tokio::test]
    async fn a_policy_acceptance_stamps_a_policy_audit_reason() {
        use crate::server::publisher_auto_advance::policy_audit_reason;

        let reason = policy_audit_reason("producer-a", "kps_example");
        assert_eq!(
            reason,
            "policy:auto_advance producer=producer-a source=kps_example"
        );
    }

    /// A candidate the acceptance path refuses leaves the prior accepted
    /// generation serving, and the refusal is observable rather than
    /// silent. Here the candidate is from a producer the accepted pointer
    /// is not bound to.
    #[tokio::test]
    async fn a_candidate_the_policy_refuses_leaves_the_pointer_untouched() {
        use crate::server::state::catalog_fixture::{COMMIT_ONE, COMMIT_TWO};

        let fixture = AutoAdvanceFixture::new("p_policy_refused");
        let first = fixture.stage_candidate("knowledge-a", "first", COMMIT_ONE);
        fixture
            .establish_from(&first, Some(true), "operator grants auto-advance")
            .await;
        let before = fixture.status().await;

        // A candidate on a different published ref is not the linear fast
        // path this policy covers.
        let moved = fixture.stage_candidate_at(
            "knowledge-a",
            "second",
            COMMIT_TWO,
            "refs/heads/release",
            &fixture.scope,
        );
        let outcome = fixture
            .server
            .attempt_publisher_auto_advance("p_policy_refused", &moved);
        assert_eq!(
            outcome,
            crate::server::publisher_auto_advance::AutoAdvanceOutcome::RefChanged
        );

        let after = fixture.status().await;
        assert_eq!(
            after["generation_id"], before["generation_id"],
            "a refused candidate must not move the pointer"
        );
        assert_eq!(after["accepted_commit"], COMMIT_ONE);
        assert_eq!(
            after["auto_advance"]["last_attempt"]["outcome"], "ref_changed",
            "the refusal is surfaced in status, not only logged"
        );
    }

    /// A candidate the ACCEPTANCE PATH itself refuses is reported with the
    /// refusing layer's own code, and the prior generation keeps serving.
    #[tokio::test]
    async fn an_acceptance_path_refusal_surfaces_its_own_error_code() {
        use crate::server::state::catalog_fixture::COMMIT_ONE;

        let fixture = AutoAdvanceFixture::new("p_policy_stale");
        let first = fixture.stage_candidate("knowledge-a", "first", COMMIT_ONE);
        fixture
            .establish_from(&first, Some(true), "operator grants auto-advance")
            .await;
        let before = fixture.status().await;

        // A generation id that names no candidate: the acceptance path
        // refuses at candidate selection.
        let outcome = fixture
            .server
            .attempt_publisher_auto_advance("p_policy_stale", &format!("kps_{}", "9".repeat(64)));
        let crate::server::publisher_auto_advance::AutoAdvanceOutcome::Refused { code, .. } =
            outcome
        else {
            panic!("expected a refusal");
        };
        assert_eq!(code, "error.accepted_publication_candidate_required");

        let after = fixture.status().await;
        assert_eq!(after["generation_id"], before["generation_id"]);
        assert_eq!(
            after["auto_advance"]["last_attempt"]["code"],
            "error.accepted_publication_candidate_required"
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

    /// P5-G mechanic 4: the status tool keeps every accepted field where it
    /// was (the generation id and pointer SHA-256 are advance CAS tokens)
    /// and ADDS the bounded runtime health view beside them.
    #[tokio::test]
    async fn publisher_status_carries_the_runtime_health_view() {
        use crate::server::state::catalog_fixture::{COMMIT_ONE, CatalogFixture, knowledge_entry};

        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().canonicalize().unwrap().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        let scope = PublishedScope::try_new("repo_example", ".").unwrap();
        let fixture = CatalogFixture::new();
        fixture.add_published_project("p_status", &scope);
        fixture.attach_overlay_checkout(
            "p_status",
            &scope,
            &checkout,
            CatalogFixture::attachment().as_str(),
            "cccccccccccccccccccccccccccccc0f",
            true,
        );
        fixture.install_publication(
            "p_status",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "published")],
            &[],
        );
        let server = fixture.server();

        let result = server
            .bbox_project_publisher_status(Parameters(ProjectPublisherStatusParams {
                project_id: "p_status".into(),
            }))
            .await;
        assert_ne!(result.is_error, Some(true), "{}", error_text(&result));
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();

        // The compare-and-swap tokens stay exactly where callers read them.
        assert_eq!(body["accepted_state"], "current");
        assert!(body["generation_id"].is_string());
        assert!(body["pointer_sha256"].is_string());

        let health = &body["health"];
        assert_eq!(health["binding"]["status"], "attached");
        assert_eq!(health["accepted"]["state"], "current");
        let capabilities = health["attachments"][0]["available"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(capabilities.contains(&"repo_knowledge"), "{capabilities:?}");
        assert!(
            !capabilities.contains(&"repo_mutation"),
            "an unrecorded bit is absent, not denied: {capabilities:?}"
        );

        // Path-free (plan 13.6), asserted against a real checkout path.
        let needle = checkout.to_string_lossy().into_owned();
        assert!(
            !error_text(&result).contains(&needle),
            "publisher status leaked a checkout path"
        );
    }

    #[tokio::test]
    async fn publisher_status_reports_a_remote_producer_binding_without_an_attachment() {
        use crate::server::state::catalog_fixture::{COMMIT_ONE, CatalogFixture, knowledge_entry};
        use bbox_indexing::accepted_publication_runtime::{
            AcceptedPublicationSourceBinding, PublishRequest, PublishSourceFile, PublishSources,
            PublisherPublishMode,
        };

        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_status_remote", &scope);
        let server = fixture.server();
        let project_id = ProjectId::parse("p_status_remote").unwrap();
        let source_generation_id = format!("kps_{}", "1".repeat(64));
        let runtime = server.state.accepted_publications.as_ref().unwrap();
        let prepared = runtime
            .prepare_publish(
                PublishRequest {
                    mode: PublisherPublishMode::Establish,
                    project_id: project_id.clone(),
                    source: AcceptedPublicationSourceBinding::Producer {
                        producer_id: "producer-a".into(),
                        source_generation_id: source_generation_id.clone(),
                        source_generation_sha256: "2".repeat(64),
                    },
                    scope,
                    full_ref: "refs/heads/main".into(),
                    accepted_commit: COMMIT_ONE.into(),
                    dry_run: false,
                    auto_advance: AutoAdvanceGrantUpdate::Inherit,
                },
                PublishSources {
                    knowledge: vec![PublishSourceFile {
                        repository_relative_filename: ".bbox/knowledge/knowledge-a.json".into(),
                        source_bytes: serde_json::to_vec(&knowledge_entry(
                            "knowledge-a",
                            "remote published",
                        ))
                        .unwrap(),
                    }],
                    gaps: Vec::new(),
                    graphs: Vec::new(),
                    evidence: Vec::new(),
                },
            )
            .unwrap();
        runtime.commit_publish(prepared, &mut || Ok(())).unwrap();

        let result = server
            .bbox_project_publisher_status(Parameters(ProjectPublisherStatusParams {
                project_id: project_id.to_string(),
            }))
            .await;
        assert_ne!(result.is_error, Some(true), "{}", error_text(&result));
        let body: serde_json::Value = serde_json::from_str(&error_text(&result)).unwrap();
        assert_eq!(body["source_binding"]["kind"], "producer");
        assert_eq!(
            body["source_binding"]["source_generation_id"],
            source_generation_id
        );
        assert!(body["attachment_id"].is_null());
        assert_eq!(body["health"]["binding"]["status"], "producer");
        assert_eq!(body["health"]["binding"]["source_kind"], "producer");
    }

    /// Bridge mode refusal is unchanged by the widening.
    #[tokio::test]
    async fn publisher_status_still_refuses_on_bridge() {
        let tmp = tempfile::tempdir().unwrap();
        let server = bridge_server(&tmp);
        let result = server
            .bbox_project_publisher_status(Parameters(ProjectPublisherStatusParams {
                project_id: "p_00000000000000000000000000000000".into(),
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            error_text(&result).contains("error.project_catalog_inactive"),
            "{}",
            error_text(&result)
        );
    }
}
