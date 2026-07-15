use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::language::Language;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct ProjectRecord {
    pub project_id: String,
    #[serde(default)]
    pub repo_id: Option<String>,
    pub canonical_path: String,
    pub registered_at: String,
    pub is_git_repo: bool,
    /// Languages auto-detected at registration. Empty when the
    /// directory predates the polyglot field — `ProjectRegistry::open`
    /// re-detects empty entries on load and persists the result.
    #[serde(default)]
    pub languages: BTreeSet<Language>,
    /// Alias selectors resolving to this project (taxonomy slice 2).
    /// Materialized from the repo's committed `.bbox/config.toml`
    /// `[project] aliases` declaration at register time and daemon open;
    /// unique across the registry — conflicting claims fail closed.
    #[serde(default)]
    pub aliases: BTreeSet<String>,
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

/// The concrete non-base checkout a [`ProjectContext`] resolution passed
/// through. `checkout_dir` doubles as the checkout identity until a
/// consumer needs a minted `checkout_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct CheckoutContext {
    /// Canonical top of the checkout containing the input path.
    pub checkout_dir: String,
    /// True when the worktree carries a managed marker (fleet/agent
    /// dispatch worktrees, in-tree linked worktrees) — the write-side
    /// aliasing gate. Arbitrary user worktrees of a registered repo resolve
    /// with `managed = false` and must not receive write-side aliasing.
    pub managed: bool,
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
///
/// This is intentionally broader than the conservative managed write gate
/// (`resolve_managed_fleet_worktree` in bbox-indexing): scope resolution
/// here is read-only (which corpus do I query?), so aliasing an arbitrary
/// user worktree of a registered repo to its base project is harmless and
/// exactly what a caller scoping a query wants. Returns `None` for paths no
/// registered project owns; callers keep their existing fallback
/// (deterministic path-hash id, raw filter, etc.).
/// Synchronous checkout-resolution boundary used by corpus indexing and
/// request-side blocking lanes.
#[allow(clippy::disallowed_methods)]
pub fn resolve_base_project_for_scope<'a>(
    path: &str,
    projects: &'a [ProjectRecord],
) -> Option<&'a ProjectRecord> {
    let canonical = std::fs::canonicalize(path).ok()?;
    if let Some(record) = projects.iter().find(|project| {
        let root = std::path::Path::new(&project.canonical_path);
        canonical == root || canonical.starts_with(root)
    }) {
        return Some(record);
    }
    let common = crate::git::git_common_dir(&canonical)?;
    projects.iter().find(|project| {
        crate::git::git_common_dir(std::path::Path::new(&project.canonical_path))
            .is_some_and(|base_common| base_common == common)
    })
}

/// Minimal read-side view of the on-disk project registry file
/// (`projects.json`). The authoritative store type (versioning, writes,
/// migration) lives daemon-side in `projects::ProjectStore`; this
/// deserializer reads only what consumers below the daemon need.
#[derive(Debug, Default, Deserialize)]
struct ProjectStoreView {
    #[serde(default)]
    projects: Vec<ProjectRecord>,
}

/// Load the registered project records from a `projects.json` registry
/// file. Returns an empty list when the file does not exist. This is the
/// thin static read used by index passes; registry mutation stays with the
/// daemon-side `ProjectRegistry`.
#[allow(clippy::disallowed_methods)] // synchronous index-pass snapshot load
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
