use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::identity::PublishedScope;
use crate::language::Language;
use crate::project_catalog::{
    AttachmentStatus, CorpusProject, LegacyProjectRecordV1, ProjectScope,
    ValidatedCheckoutAttachment,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct ProjectRecord {
    pub project_id: String,
    #[serde(default)]
    pub repo_id: Option<String>,
    pub canonical_path: String,
    pub registered_at: String,
    pub is_git_repo: bool,
    /// Languages auto-detected at registration. Empty when the directory
    /// predates the polyglot field; daemon startup backfills it only through
    /// an explicit local-project-walk lease.
    #[serde(default)]
    pub languages: BTreeSet<Language>,
    /// Alias selectors resolving to this project (taxonomy slice 2).
    /// Materialized from the repo's committed `.bbox/config.toml`
    /// `[project] aliases` declaration at register time and daemon open;
    /// unique across the registry — conflicting claims fail closed.
    #[serde(default)]
    pub aliases: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectRecordJoinError {
    reason: &'static str,
}

impl std::fmt::Display for ProjectRecordJoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "catalog project and attachment cannot form a compatibility record: {}",
            self.reason
        )
    }
}

impl std::error::Error for ProjectRecordJoinError {}

impl ProjectRecord {
    /// Build the temporary path-bearing v1 view from a logical project and an
    /// attachment obtained from a fully cross-validated snapshot pair.
    ///
    /// A detached, cross-project, or scope-mismatched attachment is refused.
    /// Remote catalog projects therefore never receive a fabricated path.
    pub fn from_catalog_attachment(
        project: &CorpusProject,
        validated_attachment: ValidatedCheckoutAttachment<'_>,
    ) -> Result<Self, ProjectRecordJoinError> {
        let attachment = validated_attachment.attachment();
        if validated_attachment.project() != project {
            return Err(ProjectRecordJoinError {
                reason: "project is not the validated catalog record",
            });
        }
        if attachment.status != AttachmentStatus::Attached {
            return Err(ProjectRecordJoinError {
                reason: "attachment is detached",
            });
        }
        if attachment.project_id != project.project_id {
            return Err(ProjectRecordJoinError {
                reason: "attachment belongs to another project",
            });
        }
        let published_repo_id = match (&project.scope, &attachment.validated_scope) {
            (ProjectScope::Published(project_scope), Some(attachment_scope))
                if project_scope == attachment_scope =>
            {
                Some(project_scope.repo_id().to_string())
            }
            (ProjectScope::LegacyLocal, None) => None,
            _ => {
                return Err(ProjectRecordJoinError {
                    reason: "attachment scope disagrees with project",
                });
            }
        };
        let repo_id = validated_attachment
            .repo_history()
            .map(|history| history.primary_namespace.as_str().to_string())
            .or(published_repo_id);
        Ok(Self {
            project_id: project.project_id.as_str().to_string(),
            repo_id,
            canonical_path: attachment.checkout_project_dir.clone(),
            registered_at: project
                .registered_at_compat
                .clone()
                .unwrap_or_else(|| project.created_at.clone()),
            is_git_repo: validated_attachment.repo_history().is_some()
                || attachment.validated_scope.is_some(),
            languages: project.languages.clone(),
            aliases: project.operator_aliases.clone(),
        })
    }
}

impl From<LegacyProjectRecordV1> for ProjectRecord {
    fn from(record: LegacyProjectRecordV1) -> Self {
        Self {
            project_id: record.project_id,
            repo_id: record.repo_id,
            canonical_path: record.canonical_path,
            registered_at: record.registered_at,
            is_git_repo: record.is_git_repo,
            languages: record.languages,
            aliases: record.aliases,
        }
    }
}

impl From<ProjectRecord> for LegacyProjectRecordV1 {
    fn from(record: ProjectRecord) -> Self {
        Self {
            project_id: record.project_id,
            repo_id: record.repo_id,
            canonical_path: record.canonical_path,
            registered_at: record.registered_at,
            is_git_repo: record.is_git_repo,
            languages: record.languages,
            aliases: record.aliases,
        }
    }
}

/// Resolved project identity plus the concrete checkout view that produced
/// it — the structured result of project-selector resolution per
/// `design/corpus/agentic-corpus/project-taxonomy-standardization.md`.
///
/// Identity fields (`project_id`, `repo_id`, `host_root`) describe the
/// durable registered project; `checkout` describes the specific checkout
/// the caller's input pointed into when that differs from the base root.
/// Tools that only search the corpus stop at `project_id`; tools that touch
/// files continue to `checkout`. The forward-looking workspace layer
/// (`/work` mounts, path maps) is deliberately absent until a containment
/// consumer exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct ProjectContext {
    /// Registry project id (8-hex, host-scoped realpath hash).
    pub project_id: String,
    /// Cross-host repo identity (first-commit SHA hash) when known.
    #[serde(default)]
    pub repo_id: Option<String>,
    /// Registered alias selectors for this project.
    #[serde(default)]
    pub aliases: BTreeSet<String>,
    /// Canonical path of the registered base checkout — the durable
    /// project-scope key on this host.
    pub host_root: String,
    /// Present when the resolved input was inside a checkout other than the
    /// base root (an in-tree linked worktree or an out-of-tree worktree).
    /// `None` for the base root and its plain subdirectories.
    #[serde(default)]
    pub checkout: Option<CheckoutContext>,
}

/// The concrete checkout a [`ProjectContext`] resolution passed through.
///
/// Under the identity contract EVERY resolved project normalizes to a concrete
/// checkout, the registered base included, so a base-scoped session has its own
/// overlay identity exactly like a worktree session (design §3.2). `managed`
/// still distinguishes the base/managed-worktree write gate from an arbitrary
/// user worktree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct CheckoutContext {
    /// Canonical top of the checkout containing the input path.
    pub checkout_dir: String,
    /// True when the worktree carries a managed marker (fleet/agent
    /// dispatch worktrees, in-tree linked worktrees) — the write-side
    /// aliasing gate. Arbitrary user worktrees of a registered repo resolve
    /// with `managed = false` and must not receive write-side aliasing. The
    /// normalized base checkout is `managed = true`.
    pub managed: bool,
    /// Durable, reuse-safe checkout identity persisted in
    /// `.bbox/local/checkout-id` (see
    /// `bbox_corpus_core::identity::ensure_checkout_id`). It is the overlay's
    /// primary key and the GC identity, so it is strong-random and NOT
    /// path-derived: a replacement checkout at the same path mints a fresh one.
    /// `None` on resolutions that predate minting or when the marker could not
    /// be written; `checkout_dir` remains the transitional fallback until the
    /// overlay consumes this (design §3.2). Host-local — never travels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_id: Option<String>,
}

/// Scope-aware checkout descriptor consumed by the provisional overlay.
/// Paths are host-local; `published_scope` and `checkout_id` are the stable
/// keys. The checkout top owns the reuse-safe marker while
/// `checkout_project_dir` projects a monorepo subproject into that checkout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct ResolvedCheckoutScope {
    /// Host-local project registry identity selected by the same conservative
    /// write resolution that admitted this checkout. It is routing context,
    /// not durable cross-host authority; `published_scope` remains that
    /// authority.
    #[serde(default)]
    pub project_id: String,
    pub published_scope: PublishedScope,
    pub checkout_id: String,
    pub checkout_dir: String,
    pub checkout_project_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_ref: Option<String>,
}

/// Resolve a caller-supplied filesystem path to the registered project that
/// owns it, for project-scoped RETRIEVAL (index / graph / knowledge scope
/// resolution). Acceptance, in order:
///
/// 1. the path is a registered root or a descendant of one → that record.
///    Covers plain subdirectories AND in-tree worktrees (e.g.
///    `.claude/worktrees/<name>` under the repo) — both scope to the root
///    project for retrieval purposes.
/// 2. the path is inside any git worktree whose common dir matches a
///    registered project's → that record. Covers out-of-tree worktrees —
///    fleet (`bro-fleet/*`), agent dispatch, workflow arcs — regardless of
///    branch name or parent directory.
/// 3. the path is inside a full independent clone carrying the exact managed
///    checkout marker and its durable `repo_id` uniquely matches one
///    registered project. Pool lanes use this path because they cannot share
///    a git common dir with the host checkout.
///
/// This is intentionally broader than the conservative managed write gate
/// (`resolve_managed_fleet_worktree` in bbox-indexing): scope resolution
/// here is read-only (which corpus do I query?), so aliasing an arbitrary
/// user worktree of a registered repo to its base project is harmless and
/// exactly what a caller scoping a query wants. Returns `None` for paths no
/// registered project owns; callers keep their existing fallback
/// (deterministic path-hash id, raw filter, etc.).
pub fn resolve_base_project_for_scope<'a>(
    path: &str,
    projects: &'a [ProjectRecord],
) -> Option<&'a ProjectRecord> {
    let canonical = std::fs::canonicalize(path).ok()?;
    if let Some(record) = unique_deepest_project(projects.iter().filter_map(|project| {
        let root = std::path::Path::new(&project.canonical_path);
        (canonical == root || canonical.starts_with(root))
            .then_some((root.components().count(), project))
    })) {
        return Some(record);
    }
    if let (Some(common), Some(checkout_root)) = (
        crate::git::git_common_dir(&canonical),
        crate::git::git_root_for_path(&canonical),
    ) && let Some(record) = unique_deepest_project(projects.iter().filter_map(|project| {
        let project_root = std::path::Path::new(&project.canonical_path);
        let project_git_root = crate::git::git_root_for_path(project_root)?;
        (crate::git::git_common_dir(&project_git_root).as_ref() == Some(&common)).then_some(())?;
        let rel = project_root.strip_prefix(&project_git_root).ok()?;
        let projected_root = checkout_root.join(rel);
        (canonical == projected_root || canonical.starts_with(&projected_root))
            .then_some((rel.components().count(), project))
    })) {
        return Some(record);
    }
    let checkout = crate::git::managed_checkout_root(&canonical)?;
    let repo_id = crate::entity_ref::repo_id_for_root(&checkout).ok()?;
    let repo_matches = projects
        .iter()
        .filter(|project| project.repo_id.as_deref() == Some(repo_id.as_str()))
        .collect::<Vec<_>>();
    if repo_matches.is_empty() {
        return None;
    }
    let candidates = repo_matches
        .into_iter()
        .map(|project| {
            let project_root = std::path::Path::new(&project.canonical_path);
            let project_git_root = crate::git::git_root_for_path(project_root)?;
            let rel = project_root.strip_prefix(&project_git_root).ok()?;
            let projected_root = checkout.join(rel);
            (canonical == projected_root || canonical.starts_with(&projected_root))
                .then_some((rel.components().count(), project))
        })
        .collect::<Option<Vec<_>>>()?;
    unique_deepest_project(candidates.into_iter())
}

fn unique_deepest_project<'a>(
    candidates: impl Iterator<Item = (usize, &'a ProjectRecord)>,
) -> Option<&'a ProjectRecord> {
    let mut selected = None;
    let mut selected_depth = 0;
    let mut ambiguous = false;
    for (depth, project) in candidates {
        if selected.is_none() || depth > selected_depth {
            selected = Some(project);
            selected_depth = depth;
            ambiguous = false;
        } else if depth == selected_depth {
            ambiguous = true;
        }
    }
    (!ambiguous).then_some(selected).flatten()
}

/// Minimal read-side view of the on-disk project registry file
/// (`projects.json`). The version-1 persistence input is represented
/// explicitly as `project_catalog::LegacyProjectStoreV1`; this deserializer
/// reads only what consumers below the daemon need.
#[derive(Debug, Default, Deserialize)]
struct ProjectStoreView {
    #[serde(default)]
    projects: Vec<ProjectRecord>,
}

/// The injected project-identity snapshot of phase-2 §6.2. Daemon-runtime
/// consumers receive this instead of reading `projects.json` from disk or
/// taking the registry by type, so the catalog-mode runtime can feed them
/// without v1 bytes existing at all.
///
/// `records` carries attached projects only (the path-bearing compatibility
/// rows); `corpus_project_ids` carries the COMPLETE catalog project-id set,
/// which seeds corpus identity surfaces (active-selector maps, edge
/// registered-project sets) so remote-only collected state is never hidden
/// by an attached-only gate. On the version-1 bridge the two views coincide.
#[derive(Debug, Clone)]
pub struct ProjectRecordsSnapshot {
    pub records: std::sync::Arc<Vec<ProjectRecord>>,
    pub corpus_project_ids: std::sync::Arc<BTreeSet<String>>,
    pub omitted_catalog_count: u64,
    /// Authority epoch this snapshot was derived from: the registry mutation
    /// epoch on the bridge, the catalog epoch in catalog mode.
    pub authority_epoch: u64,
}

impl ProjectRecordsSnapshot {
    /// Bridge derivation: the corpus id set is exactly the record id set.
    pub fn from_bridge_records(records: Vec<ProjectRecord>, authority_epoch: u64) -> Self {
        let corpus_project_ids = records
            .iter()
            .map(|record| record.project_id.clone())
            .collect::<BTreeSet<_>>();
        Self {
            records: std::sync::Arc::new(records),
            corpus_project_ids: std::sync::Arc::new(corpus_project_ids),
            omitted_catalog_count: 0,
            authority_epoch,
        }
    }

    /// The COMPLETE catalog project-id set as a `HashSet`, for the sidecar
    /// registered-project gate and the storage-GC liveness seed.
    ///
    /// Phase 3 plan section 7 item 3 (F3/F4): every "is this project
    /// registered?" surface derives its set here. Two daemon constructors
    /// previously built the same set from `records` (attached rows only),
    /// which dropped a remote-only project's edges on the first runtime
    /// rebuild and deleted its sidecars after the background GC's 30-day
    /// orphan fuse while the equivalent MCP tools treated it as live.
    pub fn registered_project_ids(&self) -> std::collections::HashSet<String> {
        self.corpus_project_ids.iter().cloned().collect()
    }

    pub fn empty() -> Self {
        Self {
            records: std::sync::Arc::new(Vec::new()),
            corpus_project_ids: std::sync::Arc::new(BTreeSet::new()),
            omitted_catalog_count: 0,
            authority_epoch: 0,
        }
    }
}

/// Source of fresh [`ProjectRecordsSnapshot`]s for long-lived consumers
/// (index writer, providers). Implementations derive on read against their
/// authority's epoch, so staleness is unrepresentable rather than
/// discipline-dependent.
pub trait ProjectRecordsProvider: Send + Sync {
    fn records_snapshot(&self) -> ProjectRecordsSnapshot;

    /// Whether a durable producer-transport cutover row closes checkout Git
    /// history for this project. Bridge and offline providers retain the
    /// default `false`; catalog runtime providers override it from their
    /// checksummed marker and live project-to-repository binding.
    fn git_history_transport_governed(&self, _project_id: &str) -> bool {
        false
    }

    /// Per-project code identity for the COMPLETE corpus id set (Phase 3
    /// plan section 7 item 1). Source planning needs an identity for every
    /// catalog project, including one with zero attachments, so it cannot
    /// derive identities from `records` (attached rows only).
    ///
    /// The default derivation is the bridge one: project the attached v1
    /// rows, which on the bridge are exactly the corpus id set. Catalog-mode
    /// providers override this to build from catalog projects plus their
    /// resolved history records, so a remote-only project is present here
    /// even though it has no `ProjectRecord`. A project id absent from the
    /// returned map has no derivable identity this pass; planning classifies
    /// it as unavailable rather than silently skipping it.
    fn code_identities(
        &self,
    ) -> BTreeMap<String, crate::code_project_identity::CodeProjectIdentity> {
        self.records_snapshot()
            .records
            .iter()
            .filter_map(|record| {
                match crate::code_project_identity::CodeProjectIdentity::from_bridge_record(record) {
                    Ok(identity) => Some((record.project_id.clone(), identity)),
                    Err(error) => {
                        tracing::warn!(
                            project_id = %record.project_id,
                            %error,
                            "projecting a bridge code identity failed; project omitted from planning"
                        );
                        None
                    }
                }
            })
            .collect()
    }

    /// Bounded description of the provider's most recent degradation
    /// (stale-cache serving, omitted projection rows), for doctor/health
    /// surfacing. `None` means the last snapshot derived cleanly.
    fn last_degradation(&self) -> Option<String> {
        None
    }
}

/// Load the registered project records from a `projects.json` registry
/// file. Returns an empty list when the file does not exist. This is the
/// thin static read used by index passes; registry mutation stays with the
/// daemon-side `ProjectRegistry`. After the phase-2 injection refactor this
/// has no daemon-runtime caller: runtime consumers receive
/// [`ProjectRecordsSnapshot`]s from their authority instead.
pub fn load_project_records(
    path: impl AsRef<std::path::Path>,
) -> anyhow::Result<Vec<ProjectRecord>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let store: ProjectStoreView = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
    Ok(store.projects)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &std::path::Path, id: &str) -> ProjectRecord {
        ProjectRecord {
            project_id: id.into(),
            repo_id: None,
            canonical_path: path.to_string_lossy().into_owned(),
            registered_at: "2026-07-22T00:00:00Z".into(),
            is_git_repo: false,
            languages: BTreeSet::new(),
            aliases: BTreeSet::new(),
        }
    }

    #[test]
    fn nested_scope_resolution_prefers_deepest_registered_root() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().canonicalize().unwrap();
        let inner = outer.join("services/api");
        let target = inner.join("src/deep");
        std::fs::create_dir_all(&target).unwrap();
        let projects = vec![record(&outer, "outer"), record(&inner, "inner")];

        let resolved = resolve_base_project_for_scope(target.to_str().unwrap(), &projects)
            .expect("nested path should resolve");
        assert_eq!(resolved.project_id, "inner");
    }

    #[test]
    fn equal_depth_scope_matches_fail_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let projects = vec![record(&root, "first"), record(&root, "second")];

        assert!(resolve_base_project_for_scope(root.to_str().unwrap(), &projects).is_none());
    }
}
