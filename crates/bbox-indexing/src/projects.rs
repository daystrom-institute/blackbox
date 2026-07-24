use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

pub use bbox_corpus_core::language::Language;
pub use bbox_corpus_core::project_record::{CheckoutContext, ProjectContext, ProjectRecord};

use bbox_corpus_core::entity_ref;
use bbox_corpus_core::project_catalog::{LegacyProjectRecordV1, LegacyProjectStoreV1};
use bbox_stores::store_persister::StoreSnapshot;
use bbox_util::util;

use crate::project_catalog_migration_lock::ProjectCatalogMigrationLock;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectRegisterParams {
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectRenameParams {
    /// Existing project to rename. Accepts project_id, registered
    /// canonical_path, or an absolute path resolving to a registered project.
    pub project: String,
    /// New absolute project directory path.
    pub new_path: String,
    /// Move the directory on disk before updating bbox state. Default false:
    /// the new_path must already exist.
    #[serde(default)]
    pub move_on_disk: Option<bool>,
    /// Preview without moving or writing state.
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectUnregisterParams {
    /// Project to unregister. Accepts project_id, registered
    /// canonical_path, or an absolute path resolving to a registered project.
    pub project: String,
    /// Remove the registry entry even when project-scoped state
    /// (knowledge, threads, notes, pins, ...) still references it.
    /// Default false: the call refuses with the live ref counts so the
    /// caller can migrate or accept the orphaning explicitly.
    #[serde(default)]
    pub force: Option<bool>,
    /// Preview without modifying the registry.
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectEjectParams {
    /// Project whose central-store knowledge to migrate into the repo. Accepts
    /// project_id, registered canonical_path, or an absolute path resolving to
    /// a registered project.
    pub project: String,
    /// Preview the count without writing repo files or touching the central store.
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ProjectListResponse {
    pub projects: Vec<ProjectRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ProjectRenameResponse {
    pub old_record: ProjectRecord,
    pub record: ProjectRecord,
    pub moved_on_disk: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectInitParams {
    pub path: String,
    #[serde(default)]
    pub force: bool,
}

pub type ProjectStore = LegacyProjectStoreV1;

#[derive(Debug, Clone)]
pub struct ProjectRegistry {
    store: ProjectStore,
    /// Bumped by every `&mut self` mutator so derived snapshots
    /// (`ProjectRecordsSnapshot`) can detect staleness on read instead of
    /// relying on call sites to republish. Any new mutator MUST bump this;
    /// the freshness tests assert it per mutator.
    mutations: u64,
    // StorePersister and every fallback writer retain the registry in an Arc,
    // so this shared guard outlives every version-1 project-store write.
    _migration_lock: Arc<ProjectCatalogMigrationLock>,
}

impl StoreSnapshot for ProjectRegistry {
    type Snapshot = ProjectStore;

    fn snapshot(&self) -> Result<Self::Snapshot> {
        Ok(self.store.clone())
    }
}

impl ProjectRegistry {
    #[allow(dead_code)]
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self::open_with_backfill_status(path)?.0)
    }

    pub fn open_with_backfill_status(path: impl Into<PathBuf>) -> Result<(Self, bool)> {
        let path = path.into();
        let migration_lock = Arc::new(ProjectCatalogMigrationLock::acquire_shared(&path)?);
        let store = load_store(&path)?;
        Ok((
            Self {
                store,
                mutations: 0,
                _migration_lock: migration_lock,
            },
            false,
        ))
    }

    /// Monotonic mutation epoch for derived-snapshot freshness. Starts at 0
    /// per process; not persisted (snapshots are process-local derived state).
    pub fn mutation_epoch(&self) -> u64 {
        self.mutations
    }

    /// Fill an established record's language set from a caller-authorized
    /// checkout walk. Opening the durable registry is intentionally path-free;
    /// daemon startup invokes this only while holding a LocalProjectWalk lease.
    pub fn backfill_languages(&mut self, project_id: &str, languages: BTreeSet<Language>) -> bool {
        self.mutations += 1;
        let Some(record) = self
            .store
            .projects
            .iter_mut()
            .find(|record| record.project_id == project_id)
        else {
            return false;
        };
        if !record.languages.is_empty() || languages.is_empty() {
            return false;
        }
        record.languages = languages;
        true
    }

    pub fn register_path(&mut self, path: impl AsRef<Path>) -> Result<ProjectRecord> {
        self.mutations += 1;
        let path = path.as_ref().to_path_buf();
        self.register_path_locked(&path)
    }

    fn register_path_locked(&mut self, path: &Path) -> Result<ProjectRecord> {
        let canonical = canonical_project_path(path)?;
        let canonical_path = canonical.to_string_lossy().into_owned();
        if let Some(existing) = self
            .store
            .projects
            .iter()
            .find(|project| project.canonical_path == canonical_path)
        {
            return Ok(existing.clone().into());
        }
        let project_id = entity_ref::project_id_for_path(&canonical)?;
        if let Some(existing) = self
            .store
            .projects
            .iter()
            .find(|project| project.project_id == project_id)
        {
            return Ok(existing.clone().into());
        }

        let git_root = entity_ref::git_root_for_path(&canonical);
        let repo_id = git_root
            .as_deref()
            .map(computed_repo_id_for_registry)
            .transpose()?;
        let repo_id = repo_id.flatten();
        let is_git_repo = git_root.is_some();
        let languages = detect_languages(&canonical);
        let record = LegacyProjectRecordV1 {
            project_id,
            repo_id,
            canonical_path,
            registered_at: util::now_iso(),
            is_git_repo,
            languages,
            aliases: BTreeSet::new(),
        };
        self.store.projects.push(record.clone());
        self.store
            .projects
            .sort_by(|a, b| a.canonical_path.cmp(&b.canonical_path));
        Ok(record.into())
    }

    pub fn rename_project(&mut self, p: &ProjectRenameParams) -> Result<ProjectRenameResponse> {
        self.mutations += 1;
        self.rename_project_locked(p)
    }

    fn rename_project_locked(&mut self, p: &ProjectRenameParams) -> Result<ProjectRenameResponse> {
        let idx = self
            .resolve_project_index(&p.project)?
            .with_context(|| format!("project not registered: {}", p.project))?;
        let old_record = self.store.projects[idx].clone();
        let move_on_disk = p.move_on_disk.unwrap_or(false);
        let dry_run = p.dry_run.unwrap_or(false);
        let new_path = PathBuf::from(&p.new_path);
        if !new_path.is_absolute() {
            anyhow::bail!("new_path must be absolute: {}", p.new_path);
        }

        let canonical = if move_on_disk {
            let old_path = PathBuf::from(&old_record.canonical_path);
            if !old_path.is_dir() {
                anyhow::bail!(
                    "cannot move_on_disk: registered path is not a directory: {}",
                    old_path.display()
                );
            }
            if new_path.exists() {
                anyhow::bail!(
                    "cannot move_on_disk: target already exists: {}",
                    new_path.display()
                );
            }
            if let Some(parent) = new_path.parent() {
                if !parent.is_dir() {
                    anyhow::bail!(
                        "cannot move_on_disk: target parent is not a directory: {}",
                        parent.display()
                    );
                }
            }
            if dry_run {
                canonical_nonexistent_absolute_path(&new_path)?
            } else {
                fs::rename(&old_path, &new_path).with_context(|| {
                    format!("moving {} to {}", old_path.display(), new_path.display())
                })?;
                canonical_project_path(&new_path)?
            }
        } else {
            canonical_project_path(&new_path)?
        };
        let canonical_path = canonical.to_string_lossy().into_owned();

        if self
            .store
            .projects
            .iter()
            .enumerate()
            .any(|(other_idx, project)| {
                other_idx != idx && project.canonical_path == canonical_path
            })
        {
            anyhow::bail!("another project is already registered at {canonical_path}");
        }

        let (repo_id, is_git_repo) = if dry_run && move_on_disk {
            (old_record.repo_id.clone(), old_record.is_git_repo)
        } else {
            let git_root = entity_ref::git_root_for_path(&canonical);
            (
                git_root
                    .as_deref()
                    .map(computed_repo_id_for_registry)
                    .transpose()?
                    .flatten(),
                git_root.is_some(),
            )
        };
        let mut record = old_record.clone();
        record.canonical_path = canonical_path;
        record.repo_id = repo_id;
        record.is_git_repo = is_git_repo;
        if !(dry_run && move_on_disk) {
            // Re-detect against the new canonical path. Skip during
            // `dry_run + move_on_disk` because the filesystem hasn't
            // been touched yet — we have nothing to walk.
            let canonical_pb = PathBuf::from(&record.canonical_path);
            if canonical_pb.is_dir() {
                record.languages = detect_languages(&canonical_pb);
            }
        }

        if !dry_run {
            self.store.projects[idx] = record.clone();
            self.store
                .projects
                .sort_by(|a, b| a.canonical_path.cmp(&b.canonical_path));
        }

        Ok(ProjectRenameResponse {
            old_record: old_record.into(),
            record: record.into(),
            moved_on_disk: move_on_disk && !dry_run,
            dry_run,
        })
    }

    /// Resolve `raw` (project_id, canonical_path, or absolute path) without
    /// mutating the registry. Returns `None` when no match is registered.
    pub fn resolve(&self, raw: &str) -> Result<Option<ProjectRecord>> {
        Ok(self
            .resolve_project_index(raw)?
            .map(|idx| self.store.projects[idx].clone().into()))
    }

    pub fn unregister_project(&mut self, raw: &str) -> Result<ProjectRecord> {
        self.mutations += 1;
        let raw = raw.to_string();
        let idx = self
            .resolve_project_index(&raw)?
            .with_context(|| format!("project not registered: {raw}"))?;
        Ok(self.store.projects.remove(idx).into())
    }

    pub fn list(&self) -> Vec<ProjectRecord> {
        self.store
            .projects
            .iter()
            .cloned()
            .map(ProjectRecord::from)
            .collect()
    }

    pub fn load_records(path: impl AsRef<Path>) -> Result<Vec<ProjectRecord>> {
        Ok(load_store(path.as_ref())?
            .projects
            .into_iter()
            .map(ProjectRecord::from)
            .collect())
    }

    fn resolve_project_index(&self, raw: &str) -> Result<Option<usize>> {
        if let Some((idx, _)) = self
            .store
            .projects
            .iter()
            .enumerate()
            .find(|(_, project)| project.project_id == raw || project.canonical_path == raw)
        {
            return Ok(Some(idx));
        }
        if let Some(idx) = unique_alias_index(raw, &self.store.projects) {
            return Ok(Some(idx));
        }
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            if let Ok(canonical) = canonical_project_path(&path) {
                let canonical_path = canonical.to_string_lossy();
                return Ok(self
                    .store
                    .projects
                    .iter()
                    .position(|project| project.canonical_path == canonical_path));
            }
        }
        Ok(None)
    }

    /// Replace `selector`'s materialized alias set with the repo-declared
    /// one. Fail-closed: every declared alias must be well-formed and not
    /// claimed by another registered project, otherwise nothing mutates.
    /// Returns whether the record changed (caller persists on `true`).
    pub fn sync_declared_aliases(
        &mut self,
        selector: &str,
        declared: &BTreeSet<String>,
    ) -> Result<bool> {
        self.mutations += 1;
        let idx = self
            .resolve_project_index(selector)?
            .with_context(|| format!("project not registered: {selector}"))?;
        for alias in declared {
            if !valid_alias(alias) {
                anyhow::bail!(
                    "invalid project alias `{alias}`: aliases must be non-empty, \
                     without `/` or whitespace"
                );
            }
            if let Some(owner) = self
                .store
                .projects
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != idx)
                .find(|(_, project)| project.aliases.contains(alias))
            {
                anyhow::bail!(
                    "project alias `{alias}` already claimed by {} ({}); \
                     conflicting aliases fail closed",
                    owner.1.project_id,
                    owner.1.canonical_path
                );
            }
        }
        let record = &mut self.store.projects[idx];
        if record.aliases == *declared {
            return Ok(false);
        }
        record.aliases = declared.clone();
        Ok(true)
    }
}

fn computed_repo_id_for_registry(git_root: &Path) -> Result<Option<String>> {
    // The registry's legacy computed id is only a bootstrap hint. A shallow
    // boundary is not the repository root, so recording its hash would change
    // after unshallowing and could admit the wrong managed clone in between.
    if bbox_corpus_core::git::is_shallow_repository(git_root) {
        return Ok(None);
    }
    Ok(Some(entity_ref::repo_id_for_root(git_root)?))
}

/// An alias resolves only when exactly one registered project claims it —
/// ambiguity fails closed (registry sync enforces uniqueness, but a
/// hand-edited store must not resolve arbitrarily).
trait ProjectAliases {
    fn aliases(&self) -> &BTreeSet<String>;
}

impl ProjectAliases for LegacyProjectRecordV1 {
    fn aliases(&self) -> &BTreeSet<String> {
        &self.aliases
    }
}

impl ProjectAliases for ProjectRecord {
    fn aliases(&self) -> &BTreeSet<String> {
        &self.aliases
    }
}

fn unique_alias_index<T: ProjectAliases>(raw: &str, projects: &[T]) -> Option<usize> {
    let mut matches = projects
        .iter()
        .enumerate()
        .filter(|(_, project)| project.aliases().contains(raw));
    let (idx, _) = matches.next()?;
    matches.next().is_none().then_some(idx)
}

fn valid_alias(alias: &str) -> bool {
    !alias.is_empty() && !alias.contains('/') && !alias.chars().any(char::is_whitespace)
}

fn load_store(path: &Path) -> Result<ProjectStore> {
    if !path.exists() {
        return Ok(ProjectStore::default());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let store: ProjectStore =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(store)
}

fn canonical_project_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    Ok(entity_ref::canonical_input_path(path)?)
}

/// For a path that is a managed fleet worktree of a registered project, return
/// `(base_canonical_path, worktree_canonical_path)`. The base is the worktree's
/// durable scope (host-local thread keying, project-scoped queries) while the
/// worktree is where repo-owned artifacts (e.g. committed thread records) should
/// be written so they travel with the agent's branch. `None` when the path is
/// not a managed fleet worktree (already registered/descendant, no managed
/// marker, or no registered base shares its git common dir).
pub fn fleet_worktree_scope_and_dir(
    project_dir: &str,
    projects: &[ProjectRecord],
) -> Option<(String, String)> {
    let (base, worktree) = resolve_managed_fleet_worktree(Some(project_dir), projects)?;
    Some((
        base.canonical_path.clone(),
        worktree.to_string_lossy().into_owned(),
    ))
}

/// Shared core: resolve a path to `(base_record, canonical_checkout)` when it
/// is a recognized checkout of a registered project. Three recognized classes:
///
/// 1. **Out-of-tree managed worktrees** — git common dir matches a registered
///    project AND a managed marker is present: either the checked-out branch is
///    `bro-fleet/*` (fleet cockpit dispatch) or the path lives under one of the
///    cockpit-managed worktree parent roots (`$BRO_HOME/fleet/worktrees`,
///    `$BRO_HOME/agent/worktrees` — agent dispatch uses arbitrary branch names,
///    so the path is the signal there). Dispatch creates these outside the
///    registered repo root (under the daemon state dir), so the literal worktree
///    path is not a descendant of any registered root.
/// 2. **In-tree linked worktrees** — the path lies inside a *linked* git
///    worktree nested under the registered root itself (harness worktrees such
///    as `<root>/.claude/worktrees/<name>`). The gate is purely structural: the
///    nearest `.git` marker walking up from the path is a FILE whose gitdir
///    points into `<root>/.git/worktrees/<name>`. A plain subdirectory of the
///    root (no `.git` file ancestor below the root) is NEVER worktree-classed
///    and resolves to `None` (base behavior), as does the root itself.
/// 3. **Managed independent clones** — the clone carries the exact
///    `.git/blackbox-managed-checkout` marker and its durable `repo_id`
///    uniquely matches a registered project. Pool lanes use this class.
///
/// Returns `None` for the registered root, its plain subdirectories, paths with
/// no managed marker (out-of-tree), or paths whose repo doesn't match any
/// registered project.
///
/// Deliberately conservative: this gate guards WRITE-side aliasing (where gap
/// files, threads, slice edits, and code-nav overlays land), so arbitrary
/// out-of-tree user worktrees of a registered repo do not alias. Read-only
/// scope resolution for retrieval uses the broader
/// [`resolve_base_project_for_scope`].
fn resolve_managed_fleet_worktree<'a>(
    project_dir: Option<&str>,
    projects: &'a [ProjectRecord],
) -> Option<(&'a ProjectRecord, PathBuf)> {
    let project_dir = project_dir?;
    let worktree = fs::canonicalize(project_dir).ok()?;
    // A linked worktree nested under the enclosing repository remains an
    // in-tree managed checkout even when only monorepo subprojects are
    // registered. Resolve the checkout first, then select the deepest
    // registered bbox_root_relpath containing the requested path.
    if let Some(checkout_top) = bbox_corpus_core::git::git_root_for_path(&worktree)
        && checkout_top.join(".git").is_file()
        && let Some(base_repo) = bbox_corpus_core::git::linked_worktree_base(&checkout_top)
    {
        let base_repo = fs::canonicalize(base_repo).ok()?;
        if checkout_top.starts_with(&base_repo)
            && let Some(project) =
                select_checkout_project(&worktree, &checkout_top, projects, |p| {
                    bbox_corpus_core::git::git_root_for_path(Path::new(&p.canonical_path))
                        .is_some_and(|root| root == base_repo)
                })
        {
            return Some((project, checkout_top));
        }
    }
    if let Some(owner) = projects.iter().find(|project| {
        let root = Path::new(&project.canonical_path);
        worktree == root || worktree.starts_with(root)
    }) {
        // In-tree path of a registered root: recognize only the linked-
        // worktree shape (class 2 above); anything else keeps base behavior.
        let root = PathBuf::from(&owner.canonical_path);
        return in_tree_linked_worktree_top(&worktree, &root).map(|top| (owner, top));
    }
    if let Some(checkout) = bbox_corpus_core::git::managed_checkout_root(&worktree) {
        let repo_id = bbox_corpus_core::entity_ref::repo_id_for_root(&checkout).ok()?;
        let claiming = projects
            .iter()
            .filter(|project| project.repo_id.as_deref() == Some(repo_id.as_str()))
            .collect::<Vec<_>>();
        // A missing claimant cannot be assigned a monorepo relpath, so it must
        // remain part of the ambiguity instead of being silently skipped.
        if claiming.iter().any(|project| {
            bbox_corpus_core::git::git_root_for_path(Path::new(&project.canonical_path)).is_none()
        }) {
            return None;
        }
        let base = select_checkout_project(&worktree, &checkout, projects, |project| {
            project.repo_id.as_deref() == Some(repo_id.as_str())
        })?;
        return Some((base, checkout));
    }
    let fleet_branch = bbox_corpus_core::git::current_branch(&worktree)
        .is_some_and(|branch| branch.starts_with("bro-fleet/"));
    let under_managed_root = util::cockpit_managed_worktree_roots().iter().any(|root| {
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        worktree.starts_with(&root)
    });
    if !fleet_branch && !under_managed_root {
        return None;
    }
    let worktree_common = bbox_corpus_core::git::git_common_dir(&worktree)?;
    let checkout_top = bbox_corpus_core::git::git_root_for_path(&worktree)?;
    let base = select_checkout_project(&worktree, &checkout_top, projects, |project| {
        bbox_corpus_core::git::git_common_dir(Path::new(&project.canonical_path))
            .is_some_and(|common| common == worktree_common)
    })?;
    Some((base, checkout_top))
}

fn select_checkout_project<'a>(
    requested: &Path,
    checkout_top: &Path,
    projects: &'a [ProjectRecord],
    eligible: impl Fn(&ProjectRecord) -> bool,
) -> Option<&'a ProjectRecord> {
    let requested_rel = requested.strip_prefix(checkout_top).ok()?;
    let mut matches = projects
        .iter()
        .filter(|project| eligible(project))
        .filter_map(|project| {
            let project_root = Path::new(&project.canonical_path);
            let git_root = bbox_corpus_core::git::git_root_for_path(project_root)?;
            let relpath = bbox_corpus_core::identity::bbox_root_relpath(&git_root, project_root)?;
            let rel = (relpath != ".").then(|| PathBuf::from(relpath));
            if rel
                .as_ref()
                .is_some_and(|relpath| !requested_rel.starts_with(relpath))
            {
                return None;
            }
            Some((
                rel.as_ref()
                    .map(|relpath| relpath.components().count())
                    .unwrap_or(0),
                project,
            ))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(depth, _)| *depth);
    let (depth, selected) = matches.pop()?;
    if matches
        .last()
        .is_some_and(|(other_depth, _)| *other_depth == depth)
    {
        return None;
    }
    Some(selected)
}

/// If `path` (canonical) lies inside a linked git worktree nested under the
/// registered base `root` (canonical), return the worktree top: the nearest
/// ancestor of `path` (stopping below `root`) that carries a `.git` FILE whose
/// gitdir points into `<root>/.git/worktrees/<name>`. Structural only — no git
/// subprocess. Fails closed to `None` for:
/// - the root itself or a plain subdirectory (the walk reaches `root` without
///   meeting a `.git` file — a plain directory can never be worktree-classed);
/// - a nested independent checkout (`.git` *directory* short-circuits);
/// - a linked worktree of a DIFFERENT repo parked inside the root (its gitdir
///   base is not `root`).
fn in_tree_linked_worktree_top(path: &Path, root: &Path) -> Option<PathBuf> {
    let mut cursor = path;
    while cursor != root {
        let dot_git = cursor.join(".git");
        if dot_git.is_dir() {
            return None;
        }
        if dot_git.is_file() {
            let base = bbox_corpus_core::git::linked_worktree_base(cursor)?;
            let base = fs::canonicalize(&base).unwrap_or(base);
            return (base == root).then(|| cursor.to_path_buf());
        }
        cursor = cursor.parent()?;
    }
    None
}

// Read-side base-project resolution lives in bbox-corpus-core (next to
// ProjectRecord) so index ingest passes below this crate can stamp docs
// with it; re-exported here for the daemon-side callers. The write-side
// aliasing gates (gaps/threads/rendered files) stay with the conservative
// [`resolve_managed_fleet_worktree`] in this module.
pub use bbox_corpus_core::project_record::resolve_base_project_for_scope;

/// For surfaces that both READ project-scoped state and WRITE files into the
/// project directory (e.g. `bbox_render` scope=project): resolve a path to
/// `(base_scope_path, checkout_dir)`.
///
/// - `base_scope_path` is the registered base root's canonical path — the key
///   under which project-scoped knowledge entries live.
/// - `checkout_dir` is the top of the checkout that actually contains `path`:
///   the worktree root for any worktree (in-tree or out-of-tree), or the base
///   root itself for the root and plain subdirectories. Rendered provider
///   files belong at the top of the checkout the caller is working in, so a
///   worktree gets its own files while scope-filtering still hits the base.
///
/// Uses the broad retrieval resolution of [`resolve_base_project_for_scope`].
/// `None` when no registered project owns the path.
pub fn resolve_scope_and_checkout_dir(
    path: &str,
    projects: &[ProjectRecord],
) -> Option<(String, String)> {
    let base = resolve_base_project_for_scope(path, projects)?;
    let canonical = fs::canonicalize(path).ok()?;
    let base_root = Path::new(&base.canonical_path);
    // Walk up from the requested path to the top of its checkout: the nearest
    // ancestor carrying a `.git` marker (dir for a primary checkout, file for
    // a linked worktree). Stop at the base root — a plain subdirectory of the
    // base resolves to the base itself.
    let mut cursor = canonical.clone();
    let checkout_dir = loop {
        if cursor == base_root {
            break base_root.to_path_buf();
        }
        if cursor.join(".git").exists() {
            break cursor;
        }
        match cursor.parent() {
            Some(parent) => cursor = parent.to_path_buf(),
            None => break base_root.to_path_buf(),
        }
    };
    Some((
        base.canonical_path.clone(),
        checkout_dir.to_string_lossy().into_owned(),
    ))
}

/// How a [`resolve_project_context`] caller intends to use the resolution.
/// Preserves the deliberate read/write asymmetry of the underlying gates
/// instead of collapsing it: retrieval may alias broadly, writes must not.
///
/// The enum itself lives in `bbox_corpus_core::project_selector` so the
/// shared resolver's pure types can name it; this re-export preserves the
/// established path for every existing caller. `Read` uses the broad gate
/// ([`resolve_base_project_for_scope`]); `Write` uses the conservative
/// managed gate ([`resolve_managed_fleet_worktree`]).
pub use bbox_corpus_core::project_selector::ResolveIntent;

/// Single entry point for project-selector resolution
/// (design/corpus/agentic-corpus/project-taxonomy-standardization.md,
/// §Resolution Order). Accepts, in order:
///
/// 1. an exact `project_id` or registered canonical path;
/// 2. a registered alias (unique claim required — ambiguity fails closed);
/// 3. an absolute path: a registered root, a descendant, or a worktree of a
///    registered repo, gated by `intent`.
///
/// Returns `None` when no registered project owns the selector; callers keep
/// their existing legacy fallback (deterministic path-hash id, raw filter
/// pass-through, etc.). Existing resolvers (`ProjectRegistry::resolve`,
/// `fleet_worktree_scope_and_dir`, `resolve_scope_and_checkout_dir`) are the
/// gates this composes; consumers should migrate to this entry point rather
/// than growing new bespoke chains.
///
/// Known write-side quirk, deliberately NOT codified here: the legacy write
/// chain (`resolve_project_write_scope`) lets a plain subdirectory of a
/// registered root fall through to canonicalize-pass-through, keying state
/// under the subdirectory itself. Under `Write` intent this resolver returns
/// the base for the root/exact matches and `None` for plain subdirectories,
/// leaving that fallback decision explicit at the call site.
pub fn resolve_project_context(
    raw: &str,
    projects: &[ProjectRecord],
    intent: ResolveIntent,
) -> Option<ProjectContext> {
    // 1. Exact project_id or registered canonical path.
    if let Some(record) = projects
        .iter()
        .find(|project| project.project_id == raw || project.canonical_path == raw)
    {
        return Some(base_context(record));
    }
    // 2. Registered alias.
    if let Some(idx) = unique_alias_index(raw, projects) {
        return Some(base_context(&projects[idx]));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return None;
    }
    match intent {
        ResolveIntent::Read => {
            let base = resolve_base_project_for_scope(raw, projects)?;
            let (_, checkout_dir) = resolve_scope_and_checkout_dir(raw, projects)?;
            let checkout = (checkout_dir != base.canonical_path).then(|| CheckoutContext {
                managed: resolve_managed_fleet_worktree(Some(raw), projects).is_some(),
                checkout_dir,
                // Minted lazily on first provisional write, not on read-side
                // resolution (design §3.3). Additive: transitional consumers
                // key on `checkout_dir` until the overlay lands.
                checkout_id: None,
            });
            Some(ProjectContext {
                checkout,
                ..base_context(base)
            })
        }
        ResolveIntent::Write => {
            // A canonicalized form of a registered root still resolves.
            if let Ok(canonical) = fs::canonicalize(path) {
                if let Some(record) = projects
                    .iter()
                    .find(|project| Path::new(&project.canonical_path) == canonical)
                {
                    return Some(base_context(record));
                }
            }
            let (base, worktree) = resolve_managed_fleet_worktree(Some(raw), projects)?;
            Some(ProjectContext {
                checkout: Some(CheckoutContext {
                    checkout_dir: worktree.to_string_lossy().into_owned(),
                    managed: true,
                    // Minted lazily on first provisional write (design §3.3).
                    checkout_id: None,
                }),
                ..base_context(base)
            })
        }
    }
}

fn base_context(record: &ProjectRecord) -> ProjectContext {
    ProjectContext {
        project_id: record.project_id.clone(),
        repo_id: record.repo_id.clone(),
        aliases: record.aliases.clone(),
        host_root: record.canonical_path.clone(),
        checkout: None,
    }
}

/// Walk a project root (capped at depth 4) collecting language
/// fingerprints. Skips heavy build/output directories so a polyglot
/// monorepo doesn't pay an O(everything) cost on registration.
pub fn detect_languages(root: &Path) -> BTreeSet<Language> {
    const MAX_DEPTH: usize = 4;
    const SKIP_DIRS: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        "build",
        "out",
        ".gradle",
        ".idea",
        ".vscode",
        "dist",
        ".bbox",
        ".bloop",
        ".metals",
    ];

    let mut found = BTreeSet::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if found.len() >= 2 {
            // All known languages detected; stop early.
            break;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if file_type.is_file() {
                match name_str.as_ref() {
                    "Cargo.toml" => {
                        found.insert(Language::Rust);
                    }
                    "pom.xml"
                    | "build.gradle"
                    | "build.gradle.kts"
                    | "settings.gradle"
                    | "settings.gradle.kts" => {
                        found.insert(Language::Java);
                    }
                    other => {
                        if other.ends_with(".java") {
                            found.insert(Language::Java);
                        }
                    }
                }
            } else if file_type.is_dir() && depth + 1 < MAX_DEPTH {
                if SKIP_DIRS.iter().any(|skip| skip == &name_str) {
                    continue;
                }
                stack.push((path, depth + 1));
            }
        }
    }
    found
}

fn canonical_nonexistent_absolute_path(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .with_context(|| format!("path has no parent: {}", path.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("path has no final component: {}", path.display()))?;
    let parent = fs::canonicalize(parent)
        .with_context(|| format!("canonicalizing parent {}", parent.display()))?;
    Ok(parent.join(file_name))
}

/// Bridge-mode [`ProjectRecordsProvider`]: derives snapshots from the live
/// registry on read whenever the registry's mutation epoch has moved, so no
/// mutation call site has to remember to republish (phase-2 §6.2).
pub struct BridgeProjectRecordsProvider {
    registry: Arc<parking_lot::RwLock<ProjectRegistry>>,
    cache: parking_lot::Mutex<Option<bbox_corpus_core::project_record::ProjectRecordsSnapshot>>,
}

impl BridgeProjectRecordsProvider {
    pub fn new(registry: Arc<parking_lot::RwLock<ProjectRegistry>>) -> Self {
        Self {
            registry,
            cache: parking_lot::Mutex::new(None),
        }
    }
}

impl bbox_corpus_core::project_record::ProjectRecordsProvider for BridgeProjectRecordsProvider {
    fn records_snapshot(&self) -> bbox_corpus_core::project_record::ProjectRecordsSnapshot {
        let mut cache = self.cache.lock();
        let registry = self.registry.read();
        let epoch = registry.mutation_epoch();
        if let Some(snapshot) = cache.as_ref()
            && snapshot.authority_epoch == epoch
        {
            return snapshot.clone();
        }
        let snapshot =
            bbox_corpus_core::project_record::ProjectRecordsSnapshot::from_bridge_records(
                registry.list(),
                epoch,
            );
        drop(registry);
        *cache = Some(snapshot.clone());
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_stores::store_persister::StorePersister;
    use parking_lot::RwLock;
    use std::process::Command;
    use std::sync::Arc;

    #[test]
    fn every_mutator_bumps_the_mutation_epoch_and_the_provider_observes() {
        use bbox_corpus_core::project_record::ProjectRecordsProvider as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("proj");
        fs::create_dir_all(&project).unwrap();

        let registry = ProjectRegistry::open(root.join("projects.json")).unwrap();
        let registry = Arc::new(RwLock::new(registry));
        let provider = BridgeProjectRecordsProvider::new(registry.clone());

        let empty = provider.records_snapshot();
        assert!(empty.records.is_empty());
        assert_eq!(empty.omitted_catalog_count, 0);

        let mut last_epoch = registry.read().mutation_epoch();
        let mut assert_bumped = |registry: &Arc<RwLock<ProjectRegistry>>| {
            let epoch = registry.read().mutation_epoch();
            assert!(epoch > last_epoch, "mutator did not bump the epoch");
            last_epoch = epoch;
        };

        let record = registry.write().register_path(&project).unwrap();
        assert_bumped(&registry);
        // The provider re-derives on read: the new record and the matching
        // corpus id set are visible with no republish call anywhere.
        let snapshot = provider.records_snapshot();
        assert_eq!(snapshot.records.len(), 1);
        assert!(snapshot.corpus_project_ids.contains(&record.project_id));

        registry
            .write()
            .backfill_languages(&record.project_id, [Language::Rust].into_iter().collect());
        assert_bumped(&registry);

        registry
            .write()
            .sync_declared_aliases(&record.project_id, &BTreeSet::from(["al".to_string()]))
            .unwrap();
        assert_bumped(&registry);
        assert!(
            provider
                .records_snapshot()
                .records
                .iter()
                .any(|r| r.aliases.contains("al"))
        );

        registry
            .write()
            .unregister_project(&record.project_id)
            .unwrap();
        assert_bumped(&registry);
        assert!(provider.records_snapshot().records.is_empty());
    }

    #[test]
    fn register_git_and_plain_projects_with_stable_ids() {
        let dir = tempfile::tempdir().unwrap();
        let git_repo = dir.path().join("repo-a");
        let plain = dir.path().join("plain");
        fs::create_dir_all(&git_repo).unwrap();
        fs::create_dir_all(&plain).unwrap();
        init_git_repo(&git_repo);

        let mut registry = ProjectRegistry::open(dir.path().join("projects.json")).unwrap();
        let git_record = registry.register_path(&git_repo).unwrap();
        let plain_record = registry.register_path(&plain).unwrap();

        assert_eq!(
            git_record.project_id,
            entity_ref::project_id_for_path(&git_repo).unwrap()
        );
        assert_eq!(
            plain_record.project_id,
            entity_ref::project_id_for_path(&plain).unwrap()
        );
        assert_ne!(git_record.project_id, plain_record.project_id);
        assert_eq!(
            git_record.repo_id.as_deref(),
            Some(entity_ref::repo_id_for_path(&git_repo).unwrap().as_str())
        );
        assert!(git_record.is_git_repo);
        assert_eq!(plain_record.repo_id, None);
        assert!(!plain_record.is_git_repo);
    }

    #[test]
    fn registration_does_not_record_computed_repo_id_from_shallow_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("shallow-repo");
        fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        let head = bbox_corpus_core::git::current_head(&repo).unwrap();
        fs::write(repo.join(".git/shallow"), format!("{head}\n")).unwrap();
        assert!(bbox_corpus_core::git::is_shallow_repository(&repo));

        let mut registry = ProjectRegistry::open(dir.path().join("projects.json")).unwrap();
        let record = registry.register_path(&repo).unwrap();

        assert!(record.is_git_repo);
        assert_eq!(record.repo_id, None);
    }

    #[tokio::test]
    async fn project_registry_round_trip_through_persister() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("repo");
        fs::create_dir_all(&project).unwrap();
        let store_path = root.join("projects.json");
        let registry = Arc::new(RwLock::new(ProjectRegistry::open(&store_path).unwrap()));
        let persister = StorePersister::spawn(
            "projects-test-roundtrip",
            registry.clone(),
            store_path.clone(),
        );

        let record = registry.write().register_path(&project).unwrap();
        persister.request_durable().await.unwrap();

        let reopened = ProjectRegistry::open(&store_path).unwrap();
        assert_eq!(reopened.list().len(), 1);
        assert_eq!(reopened.list()[0].project_id, record.project_id);
    }

    #[test]
    fn symlink_alias_collapses_to_existing_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let link = dir.path().join("project-link");
        fs::create_dir_all(&project).unwrap();
        std::os::unix::fs::symlink(&project, &link).unwrap();
        let mut registry = ProjectRegistry::open(dir.path().join("projects.json")).unwrap();

        let first = registry.register_path(&project).unwrap();
        let second = registry.register_path(&link).unwrap();

        assert_eq!(first.project_id, second.project_id);
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn register_this_repo_and_sibling_have_stable_project_ids() {
        let dir = tempfile::tempdir().unwrap();
        let this_repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let sibling = dir.path().join("sibling");
        fs::create_dir_all(&sibling).unwrap();
        let mut registry = ProjectRegistry::open(dir.path().join("projects.json")).unwrap();

        let this_record = registry.register_path(this_repo).unwrap();
        let this_again = registry.register_path(this_repo).unwrap();
        let sibling_record = registry.register_path(&sibling).unwrap();

        assert_eq!(this_record.project_id, this_again.project_id);
        assert_eq!(
            this_record.project_id,
            entity_ref::project_id_for_path(this_repo).unwrap()
        );
        assert_eq!(
            sibling_record.project_id,
            entity_ref::project_id_for_path(&sibling).unwrap()
        );
        assert_ne!(this_record.project_id, sibling_record.project_id);
    }

    #[test]
    fn rename_preserves_project_id_and_new_path_reregisters_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let old_path = dir.path().join("old-name");
        let new_path = dir.path().join("new-name");
        fs::create_dir_all(&old_path).unwrap();
        fs::create_dir_all(&new_path).unwrap();
        let mut registry = ProjectRegistry::open(dir.path().join("projects.json")).unwrap();

        let old_record = registry.register_path(&old_path).unwrap();
        let derived_new_id = entity_ref::project_id_for_path(&new_path).unwrap();
        assert_ne!(old_record.project_id, derived_new_id);

        let rename = registry
            .rename_project(&ProjectRenameParams {
                project: old_record.project_id.clone(),
                new_path: new_path.to_string_lossy().into_owned(),
                move_on_disk: None,
                dry_run: None,
            })
            .unwrap();
        assert_eq!(rename.record.project_id, old_record.project_id);
        assert_eq!(
            rename.record.canonical_path,
            fs::canonicalize(&new_path)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );

        let registered_again = registry.register_path(&new_path).unwrap();
        assert_eq!(registered_again.project_id, old_record.project_id);
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn unregister_removes_entry_and_is_repeatable_after_reregister() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        let store_path = dir.path().join("projects.json");
        let mut registry = ProjectRegistry::open(&store_path).unwrap();

        let record = registry.register_path(&project).unwrap();
        assert_eq!(registry.list().len(), 1);

        let removed = registry.unregister_project(&record.project_id).unwrap();
        assert_eq!(removed.project_id, record.project_id);
        assert_eq!(registry.list().len(), 0);

        // Re-registering the same path yields the same project_id (derived
        // from canonical realpath), so an unregister+register round-trip
        // leaves project-scoped state (keyed on project_id) reachable again.
        let again = registry.register_path(&project).unwrap();
        assert_eq!(again.project_id, record.project_id);
    }

    #[test]
    fn unregister_unknown_project_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut registry = ProjectRegistry::open(&store_path).unwrap();
        let err = registry.unregister_project("nonexistent").unwrap_err();
        assert!(err.to_string().contains("project not registered"));
    }

    #[test]
    fn unregister_accepts_canonical_path_and_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        let canonical = fs::canonicalize(&project).unwrap();
        let store_path = dir.path().join("projects.json");
        let mut registry = ProjectRegistry::open(&store_path).unwrap();

        registry.register_path(&project).unwrap();
        registry
            .unregister_project(&canonical.to_string_lossy())
            .unwrap();
        assert_eq!(registry.list().len(), 0);

        registry.register_path(&project).unwrap();
        registry
            .unregister_project(&project.to_string_lossy())
            .unwrap();
        assert_eq!(registry.list().len(), 0);
    }

    #[test]
    fn detect_languages_rust_cargo_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let langs = detect_languages(dir.path());
        assert!(langs.contains(&Language::Rust));
        assert!(!langs.contains(&Language::Java));
    }

    #[test]
    fn detect_languages_maven_pom() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pom.xml"), "<project/>").unwrap();
        let langs = detect_languages(dir.path());
        assert!(langs.contains(&Language::Java));
        assert!(!langs.contains(&Language::Rust));
    }

    #[test]
    fn detect_languages_gradle() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("build.gradle.kts"), "// gradle\n").unwrap();
        let langs = detect_languages(dir.path());
        assert!(langs.contains(&Language::Java));
    }

    #[test]
    fn detect_languages_plain_java_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/com/x")).unwrap();
        fs::write(
            dir.path().join("src/com/x/A.java"),
            "package com.x; class A {}\n",
        )
        .unwrap();
        let langs = detect_languages(dir.path());
        assert!(langs.contains(&Language::Java));
    }

    #[test]
    fn detect_languages_polyglot() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::write(dir.path().join("pom.xml"), "<project/>").unwrap();
        let langs = detect_languages(dir.path());
        assert!(langs.contains(&Language::Rust));
        assert!(langs.contains(&Language::Java));
    }

    #[test]
    fn detect_languages_unsupported_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "hello").unwrap();
        fs::write(dir.path().join("script.py"), "print(1)\n").unwrap();
        let langs = detect_languages(dir.path());
        assert!(langs.is_empty());
    }

    #[tokio::test]
    async fn open_is_path_free_and_backfill_requires_explicit_languages() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("legacy");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();

        // Write a legacy-shaped store JSON without the languages field.
        let store_path = dir.path().join("projects.json");
        let canonical = fs::canonicalize(&project).unwrap();
        let raw = serde_json::json!({
            "version": 1,
            "projects": [{
                "project_id": "test-id",
                "repo_id": null,
                "canonical_path": canonical.to_string_lossy(),
                "registered_at": "2026-01-01T00:00:00Z",
                "is_git_repo": false,
            }],
        });
        fs::write(&store_path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

        let (mut registry, backfilled) =
            ProjectRegistry::open_with_backfill_status(&store_path).unwrap();
        assert!(!backfilled);
        let recs = registry.list();
        assert_eq!(recs.len(), 1);
        assert!(recs[0].languages.is_empty());
        assert!(registry.backfill_languages("test-id", BTreeSet::from([Language::Rust]),));

        // Persisted on disk so subsequent loads skip the walk.
        let registry = Arc::new(RwLock::new(registry));
        let persister = StorePersister::spawn(
            "projects-test-legacy-backfill",
            registry,
            store_path.clone(),
        );
        persister.request_durable().await.unwrap();
        let raw2 = fs::read_to_string(&store_path).unwrap();
        assert!(raw2.contains("\"languages\""));
    }

    fn init_git_repo(path: &Path) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .arg("init")
                .output()
                .unwrap()
                .status
                .success()
        );
        fs::write(path.join("README.md"), "repo").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .arg("add")
                .arg("README.md")
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .args([
                    "-c",
                    "user.name=Blackbox Test",
                    "-c",
                    "user.email=blackbox@example.invalid",
                    "commit",
                    "-m",
                    "initial",
                ])
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    fn add_worktree(base: &Path, branch: &str, path: &Path) {
        let out = Command::new("git")
            .arg("-C")
            .arg(base)
            .args([
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().unwrap(),
                "HEAD",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn clone_repo(base: &Path, path: &Path) {
        let out = Command::new("git")
            .args(["clone", "--quiet"])
            .arg(base)
            .arg(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn record_for(path: &Path, project_id: &str) -> ProjectRecord {
        ProjectRecord {
            project_id: project_id.into(),
            repo_id: None,
            canonical_path: path.to_string_lossy().into_owned(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: true,
            languages: BTreeSet::new(),
            aliases: BTreeSet::new(),
        }
    }

    #[test]
    fn declared_aliases_sync_resolve_and_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_a = tmp.path().join("repo-a");
        let repo_b = tmp.path().join("repo-b");
        fs::create_dir_all(&repo_a).unwrap();
        fs::create_dir_all(&repo_b).unwrap();
        let store_path = tmp.path().join("projects.json");
        let mut registry = ProjectRegistry::open(&store_path).unwrap();
        let rec_a = registry.register_path(&repo_a).unwrap();
        let rec_b = registry.register_path(&repo_b).unwrap();

        // Sync materializes and reports dirty; identical re-sync is clean.
        let declared: BTreeSet<String> = ["blackbox".to_string()].into();
        assert!(
            registry
                .sync_declared_aliases(&rec_a.project_id, &declared)
                .unwrap()
        );
        assert!(
            !registry
                .sync_declared_aliases(&rec_a.project_id, &declared)
                .unwrap()
        );

        // Alias resolves through the registry like an id or path.
        let hit = registry
            .resolve("blackbox")
            .unwrap()
            .expect("alias resolves");
        assert_eq!(hit.project_id, rec_a.project_id);

        // A conflicting claim from another project fails closed, mutating
        // nothing.
        let err = registry
            .sync_declared_aliases(&rec_b.project_id, &declared)
            .unwrap_err();
        assert!(err.to_string().contains("already claimed"), "{err:#}");
        assert!(
            registry
                .resolve(&rec_b.project_id)
                .unwrap()
                .unwrap()
                .aliases
                .is_empty()
        );

        // Malformed aliases fail closed.
        let bad: BTreeSet<String> = ["has space".to_string()].into();
        assert!(
            registry
                .sync_declared_aliases(&rec_b.project_id, &bad)
                .is_err()
        );

        // Alias resolves through resolve_project_context (both intents).
        let records = registry.list();
        for intent in [ResolveIntent::Read, ResolveIntent::Write] {
            let ctx = resolve_project_context("blackbox", &records, intent)
                .expect("alias should resolve to context");
            assert_eq!(ctx.project_id, rec_a.project_id);
            assert!(ctx.aliases.contains("blackbox"));
            assert!(ctx.checkout.is_none());
        }

        // An ambiguous alias (possible only via hand-edited store) fails
        // closed at resolution.
        let mut forged = registry.list();
        forged[1].aliases = declared.clone();
        assert!(unique_alias_index("blackbox", &forged).is_none());
        for intent in [ResolveIntent::Read, ResolveIntent::Write] {
            assert!(resolve_project_context("blackbox", &forged, intent).is_none());
        }

        // Sync removal: an emptied declaration clears the materialized set.
        assert!(
            registry
                .sync_declared_aliases(&rec_a.project_id, &BTreeSet::new())
                .unwrap()
        );
        assert!(registry.resolve("blackbox").unwrap().is_none());
    }

    #[test]
    fn resolve_project_context_honors_selector_forms_and_intent() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        fs::create_dir_all(&base).unwrap();
        init_git_repo(&base);
        let base_canon = base.canonicalize().unwrap();
        let base_str = base_canon.to_str().unwrap();
        let registered = vec![record_for(&base_canon, "base-project")];

        // Selector form 1: exact project_id, both intents.
        for intent in [ResolveIntent::Read, ResolveIntent::Write] {
            let ctx = resolve_project_context("base-project", &registered, intent)
                .expect("project_id should resolve");
            assert_eq!(ctx.project_id, "base-project");
            assert_eq!(ctx.host_root, base_str);
            assert!(ctx.checkout.is_none());
        }

        // Selector form: registered canonical path.
        let ctx = resolve_project_context(base_str, &registered, ResolveIntent::Write)
            .expect("canonical path should resolve");
        assert_eq!(ctx.project_id, "base-project");
        assert!(ctx.checkout.is_none());

        // Plain subdirectory: Read aliases to base with no checkout; Write
        // stays unresolved (the legacy pass-through stays at the call site).
        let subdir = base_canon.join("src");
        fs::create_dir_all(&subdir).unwrap();
        let sub_str = subdir.to_str().unwrap();
        let ctx = resolve_project_context(sub_str, &registered, ResolveIntent::Read)
            .expect("subdirectory should read-resolve to base");
        assert_eq!(ctx.project_id, "base-project");
        assert!(ctx.checkout.is_none());
        assert!(resolve_project_context(sub_str, &registered, ResolveIntent::Write).is_none());

        // Out-of-tree worktree on an arbitrary branch: Read resolves with an
        // unmanaged checkout; Write refuses (fail-closed aliasing gate).
        let wt = tmp.path().join("wt-arbitrary");
        add_worktree(&base, "arc/anything", &wt);
        let wt_canon = wt.canonicalize().unwrap();
        let wt_str = wt_canon.to_str().unwrap();
        let ctx = resolve_project_context(wt_str, &registered, ResolveIntent::Read)
            .expect("arbitrary worktree should read-resolve to base");
        assert_eq!(ctx.project_id, "base-project");
        assert_eq!(ctx.host_root, base_str);
        let checkout = ctx.checkout.expect("worktree input carries checkout");
        assert_eq!(checkout.checkout_dir, wt_str);
        assert!(!checkout.managed);
        assert!(resolve_project_context(wt_str, &registered, ResolveIntent::Write).is_none());

        // Managed fleet worktree (bro-fleet/* branch): both intents resolve,
        // checkout marked managed.
        let fleet = tmp.path().join("wt-fleet");
        add_worktree(&base, "bro-fleet/task-1", &fleet);
        let fleet_canon = fleet.canonicalize().unwrap();
        let fleet_str = fleet_canon.to_str().unwrap();
        for intent in [ResolveIntent::Read, ResolveIntent::Write] {
            let ctx = resolve_project_context(fleet_str, &registered, intent)
                .expect("managed fleet worktree should resolve");
            assert_eq!(ctx.project_id, "base-project");
            assert_eq!(ctx.host_root, base_str);
            let checkout = ctx.checkout.expect("worktree input carries checkout");
            assert_eq!(checkout.checkout_dir, fleet_str);
            assert!(checkout.managed);
        }

        // Unrelated directory: nothing resolves; relative input: nothing.
        let stranger = tmp.path().join("stranger");
        fs::create_dir_all(&stranger).unwrap();
        let stranger_str = stranger.canonicalize().unwrap();
        for intent in [ResolveIntent::Read, ResolveIntent::Write] {
            assert!(
                resolve_project_context(stranger_str.to_str().unwrap(), &registered, intent)
                    .is_none()
            );
            assert!(resolve_project_context("not-a-project", &registered, intent).is_none());
        }
    }

    #[test]
    fn resolve_base_project_for_scope_maps_descendants_and_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        fs::create_dir_all(&base).unwrap();
        init_git_repo(&base);
        let base_canon = base.canonicalize().unwrap();
        let registered = vec![record_for(&base_canon, "base-project")];

        // The registered root itself.
        let hit = resolve_base_project_for_scope(base_canon.to_str().unwrap(), &registered)
            .expect("root should resolve");
        assert_eq!(hit.project_id, "base-project");

        // A plain subdirectory of the root.
        let subdir = base_canon.join("src").join("deep");
        fs::create_dir_all(&subdir).unwrap();
        let hit = resolve_base_project_for_scope(subdir.to_str().unwrap(), &registered)
            .expect("descendant should resolve to the root");
        assert_eq!(hit.project_id, "base-project");

        // An in-tree worktree (descendant of the registered root).
        let in_tree = base_canon.join(".claude").join("worktrees").join("wt-in");
        fs::create_dir_all(in_tree.parent().unwrap()).unwrap();
        add_worktree(&base, "feature/in-tree", &in_tree);
        let hit = resolve_base_project_for_scope(in_tree.to_str().unwrap(), &registered)
            .expect("in-tree worktree should resolve to the root");
        assert_eq!(hit.project_id, "base-project");

        // An out-of-tree worktree on an ARBITRARY branch (no bro-fleet/
        // prefix, not under a managed root): retrieval scope still resolves
        // via the git common dir.
        let out_tree = tmp.path().join("wt-out");
        add_worktree(&base, "arc/anything", &out_tree);
        let hit = resolve_base_project_for_scope(
            out_tree.canonicalize().unwrap().to_str().unwrap(),
            &registered,
        )
        .expect("out-of-tree worktree should resolve via common dir");
        assert_eq!(hit.project_id, "base-project");

        // A subdirectory INSIDE the out-of-tree worktree.
        let wt_sub = out_tree.canonicalize().unwrap().join("nested");
        fs::create_dir_all(&wt_sub).unwrap();
        let hit = resolve_base_project_for_scope(wt_sub.to_str().unwrap(), &registered)
            .expect("worktree subdirectory should resolve via common dir");
        assert_eq!(hit.project_id, "base-project");

        // An unrelated directory resolves to nothing.
        let stranger = tmp.path().join("stranger");
        fs::create_dir_all(&stranger).unwrap();
        assert!(
            resolve_base_project_for_scope(
                stranger.canonicalize().unwrap().to_str().unwrap(),
                &registered
            )
            .is_none()
        );

        // An unrelated git repo (different common dir) resolves to nothing.
        let other_repo = tmp.path().join("other");
        fs::create_dir_all(&other_repo).unwrap();
        init_git_repo(&other_repo);
        assert!(
            resolve_base_project_for_scope(
                other_repo.canonicalize().unwrap().to_str().unwrap(),
                &registered
            )
            .is_none()
        );
    }

    #[test]
    fn resolve_scope_and_checkout_dir_splits_scope_from_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        fs::create_dir_all(&base).unwrap();
        init_git_repo(&base);
        let base_canon = base.canonicalize().unwrap();
        let base_str = base_canon.to_string_lossy().into_owned();
        let registered = vec![record_for(&base_canon, "base-project")];

        // Plain subdirectory: scope AND checkout are the base root.
        let subdir = base_canon.join("src");
        fs::create_dir_all(&subdir).unwrap();
        let (scope, checkout) =
            resolve_scope_and_checkout_dir(subdir.to_str().unwrap(), &registered).unwrap();
        assert_eq!(scope, base_str);
        assert_eq!(checkout, base_str);

        // Out-of-tree worktree: scope is the base, checkout is the worktree.
        let out_tree = tmp.path().join("wt-out");
        add_worktree(&base, "arc/out", &out_tree);
        let out_canon = out_tree.canonicalize().unwrap();
        let (scope, checkout) =
            resolve_scope_and_checkout_dir(out_canon.to_str().unwrap(), &registered).unwrap();
        assert_eq!(scope, base_str);
        assert_eq!(checkout, out_canon.to_string_lossy());

        // Subdirectory inside the worktree maps to the worktree top.
        let wt_sub = out_canon.join("inner").join("dir");
        fs::create_dir_all(&wt_sub).unwrap();
        let (scope, checkout) =
            resolve_scope_and_checkout_dir(wt_sub.to_str().unwrap(), &registered).unwrap();
        assert_eq!(scope, base_str);
        assert_eq!(checkout, out_canon.to_string_lossy());

        // In-tree worktree: scope is the base, checkout is the worktree.
        let in_tree = base_canon.join(".claude").join("worktrees").join("wt-in");
        fs::create_dir_all(in_tree.parent().unwrap()).unwrap();
        add_worktree(&base, "feature/in", &in_tree);
        let (scope, checkout) =
            resolve_scope_and_checkout_dir(in_tree.to_str().unwrap(), &registered).unwrap();
        assert_eq!(scope, base_str);
        assert_eq!(checkout, in_tree.to_string_lossy());

        // Unregistered path resolves to nothing.
        let stranger = tmp.path().join("stranger");
        fs::create_dir_all(&stranger).unwrap();
        assert!(
            resolve_scope_and_checkout_dir(
                stranger.canonicalize().unwrap().to_str().unwrap(),
                &registered
            )
            .is_none()
        );
    }

    #[test]
    fn managed_clone_marker_maps_read_and_write_to_registered_repo_id() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        fs::create_dir_all(&base).unwrap();
        init_git_repo(&base);
        let base_canon = base.canonicalize().unwrap();
        let base_str = base_canon.to_string_lossy().into_owned();
        let mut base_record = record_for(&base_canon, "base-project");
        base_record.repo_id = Some(entity_ref::repo_id_for_root(&base_canon).unwrap());
        let registered = vec![base_record.clone()];

        let clone = tmp.path().join("lane-clone");
        clone_repo(&base, &clone);
        let clone_canon = clone.canonicalize().unwrap();
        let clone_str = clone_canon.to_string_lossy().into_owned();

        // An independent clone has a different git common dir and must not
        // alias until lane tooling opts it in with the exact marker.
        for intent in [ResolveIntent::Read, ResolveIntent::Write] {
            assert!(resolve_project_context(&clone_str, &registered, intent).is_none());
        }
        fs::write(
            clone_canon.join(".git").join("blackbox-managed-checkout"),
            format!("{}\n", bbox_corpus_core::git::MANAGED_CHECKOUT_MARKER_V1),
        )
        .unwrap();

        let nested = clone_canon.join("src").join("nested");
        fs::create_dir_all(&nested).unwrap();
        let nested_str = nested.to_string_lossy().into_owned();
        for intent in [ResolveIntent::Read, ResolveIntent::Write] {
            let ctx = resolve_project_context(&nested_str, &registered, intent)
                .expect("marked clone should resolve to registered base");
            assert_eq!(ctx.project_id, "base-project");
            assert_eq!(ctx.host_root, base_str);
            let checkout = ctx.checkout.expect("managed clone carries checkout");
            assert_eq!(checkout.checkout_dir, clone_str);
            assert!(checkout.managed);
        }

        // Durable identity must be unique. A marker never chooses between two
        // registrations for the same repository.
        let mut ambiguous = registered.clone();
        let mut duplicate = base_record;
        duplicate.project_id = "duplicate-project".into();
        duplicate.canonical_path = tmp.path().join("missing-copy").display().to_string();
        ambiguous.push(duplicate);
        for intent in [ResolveIntent::Read, ResolveIntent::Write] {
            assert!(resolve_project_context(&clone_str, &ambiguous, intent).is_none());
        }
    }

    #[test]
    fn managed_gate_accepts_worktrees_under_cockpit_managed_roots() {
        let _env = util::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        fs::create_dir_all(&base).unwrap();
        init_git_repo(&base);
        let base_canon = base.canonicalize().unwrap();
        let registered = vec![record_for(&base_canon, "base-project")];

        let bro_home = tmp.path().join("bro-home");
        let managed_parent = bro_home.join("agent").join("worktrees");
        fs::create_dir_all(&managed_parent).unwrap();
        // SAFETY: guarded by test_env_lock; restored below.
        let prev = std::env::var_os("BRO_HOME");
        unsafe { std::env::set_var("BRO_HOME", &bro_home) };

        // Agent-dispatch worktree under the managed root on a NON-fleet
        // branch: the path is the managed marker.
        let wt = managed_parent.join("wt-agent");
        add_worktree(&base, "arc/agent-task", &wt);
        let wt_canon = wt.canonicalize().unwrap();
        let resolved =
            fleet_worktree_scope_and_dir(wt_canon.to_string_lossy().as_ref(), &registered);

        // A worktree on a non-fleet branch OUTSIDE any managed root still
        // does not alias on the write side.
        let unmanaged = tmp.path().join("wt-unmanaged");
        add_worktree(&base, "arc/unmanaged", &unmanaged);
        let unmanaged_resolved = fleet_worktree_scope_and_dir(
            unmanaged.canonicalize().unwrap().to_string_lossy().as_ref(),
            &registered,
        );

        match prev {
            Some(prev) => unsafe { std::env::set_var("BRO_HOME", prev) },
            None => unsafe { std::env::remove_var("BRO_HOME") },
        }

        let (scope, dir) = resolved.expect("managed-root worktree should alias");
        assert_eq!(scope, base_canon.to_string_lossy());
        assert_eq!(dir, wt_canon.to_string_lossy());
        assert!(unmanaged_resolved.is_none());
    }

    #[test]
    fn fleet_worktree_scope_and_dir_resolves_bro_fleet_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        fs::create_dir_all(&base).unwrap();
        init_git_repo(&base);
        let base_canon = base.canonicalize().unwrap();

        // Linked worktree on a bro-fleet branch, OUTSIDE the registered base root.
        let worktree = tmp.path().join("wt");
        let out = Command::new("git")
            .arg("-C")
            .arg(&base)
            .args([
                "worktree",
                "add",
                "-b",
                "bro-fleet/test",
                worktree.to_str().unwrap(),
                "HEAD",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let worktree_canon = worktree.canonicalize().unwrap();

        let registered = vec![ProjectRecord {
            project_id: "base-project".into(),
            repo_id: None,
            canonical_path: base_canon.to_string_lossy().into_owned(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: true,
            languages: BTreeSet::new(),
            aliases: Default::default(),
        }];

        // Managed worktree → (base scope, worktree write-dir).
        let (scope, dir) =
            fleet_worktree_scope_and_dir(worktree_canon.to_string_lossy().as_ref(), &registered)
                .expect("managed fleet worktree should resolve");
        assert_eq!(scope, base_canon.to_string_lossy());
        assert_eq!(dir, worktree_canon.to_string_lossy());

        // The registered base itself is not a worktree alias.
        assert!(
            fleet_worktree_scope_and_dir(base_canon.to_string_lossy().as_ref(), &registered)
                .is_none()
        );

        // A plain (non-fleet) dir does not resolve.
        let plain = tmp.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(
            fleet_worktree_scope_and_dir(
                plain.canonicalize().unwrap().to_string_lossy().as_ref(),
                &registered
            )
            .is_none()
        );
    }

    #[test]
    fn fleet_worktree_scope_and_dir_resolves_in_tree_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        fs::create_dir_all(&base).unwrap();
        init_git_repo(&base);
        let base_canon = base.canonicalize().unwrap();
        let registered = vec![record_for(&base_canon, "base-project")];

        // In-tree linked worktree on an arbitrary (non-fleet) branch — the
        // harness worktree shape. The structural .git-file gate recognizes it.
        let in_tree = base_canon.join(".claude").join("worktrees").join("wt-in");
        fs::create_dir_all(in_tree.parent().unwrap()).unwrap();
        add_worktree(&base, "worktree-wt-in", &in_tree);
        let in_canon = in_tree.canonicalize().unwrap();
        let (scope, dir) =
            fleet_worktree_scope_and_dir(in_canon.to_string_lossy().as_ref(), &registered)
                .expect("in-tree linked worktree should resolve");
        assert_eq!(scope, base_canon.to_string_lossy());
        assert_eq!(dir, in_canon.to_string_lossy());

        // A subdirectory inside the in-tree worktree maps to the worktree TOP.
        let sub = in_canon.join("src").join("deep");
        fs::create_dir_all(&sub).unwrap();
        let (scope, dir) =
            fleet_worktree_scope_and_dir(sub.to_string_lossy().as_ref(), &registered)
                .expect("in-tree worktree subdir should resolve to the worktree top");
        assert_eq!(scope, base_canon.to_string_lossy());
        assert_eq!(dir, in_canon.to_string_lossy());

        // The registered root itself never aliases.
        assert!(
            fleet_worktree_scope_and_dir(base_canon.to_string_lossy().as_ref(), &registered)
                .is_none()
        );

        // A plain subdirectory of the root is NEVER worktree-classed.
        let plain_sub = base_canon.join("src").join("plain");
        fs::create_dir_all(&plain_sub).unwrap();
        assert!(
            fleet_worktree_scope_and_dir(plain_sub.to_string_lossy().as_ref(), &registered)
                .is_none()
        );

        // A nested INDEPENDENT checkout (.git directory) short-circuits.
        let nested_repo = base_canon.join("vendor").join("nested");
        fs::create_dir_all(&nested_repo).unwrap();
        init_git_repo(&nested_repo);
        assert!(
            fleet_worktree_scope_and_dir(nested_repo.to_string_lossy().as_ref(), &registered)
                .is_none()
        );

        // A linked worktree of a DIFFERENT repo parked inside the root does
        // not alias to this root (gitdir base mismatch fails closed).
        let other = tmp.path().join("other-repo");
        fs::create_dir_all(&other).unwrap();
        init_git_repo(&other);
        let foreign_wt = base_canon.join("foreign-wt");
        add_worktree(&other, "worktree-foreign", &foreign_wt);
        assert!(
            fleet_worktree_scope_and_dir(foreign_wt.to_string_lossy().as_ref(), &registered)
                .is_none()
        );
    }

    #[test]
    fn write_resolution_selects_deepest_monorepo_scope_in_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        fs::create_dir_all(&base).unwrap();
        init_git_repo(&base);
        for service in ["api", "web"] {
            let knowledge = base
                .join("services")
                .join(service)
                .join(".bbox")
                .join("knowledge");
            fs::create_dir_all(&knowledge).unwrap();
            fs::write(knowledge.join(".gitkeep"), "").unwrap();
        }
        let add = Command::new("git")
            .arg("-C")
            .arg(&base)
            .args(["add", "services"])
            .status()
            .unwrap();
        assert!(add.success());
        let commit = Command::new("git")
            .arg("-C")
            .arg(&base)
            .args([
                "-c",
                "user.name=Blackbox Test",
                "-c",
                "user.email=blackbox@example.invalid",
                "commit",
                "-m",
                "services",
            ])
            .status()
            .unwrap();
        assert!(commit.success());
        let base = base.canonicalize().unwrap();
        let api = base.join("services/api").canonicalize().unwrap();
        let web = base.join("services/web").canonicalize().unwrap();
        let registered = vec![record_for(&api, "api"), record_for(&web, "web")];

        let worktree = base.join(".claude/worktrees/mono");
        fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        add_worktree(&base, "feature-monorepo", &worktree);
        let web_in_worktree = worktree.join("services/web").canonicalize().unwrap();
        let context = resolve_project_context(
            web_in_worktree.to_string_lossy().as_ref(),
            &registered,
            ResolveIntent::Write,
        )
        .expect("monorepo worktree scope");
        assert_eq!(context.project_id, "web");
        assert_eq!(
            context.checkout.unwrap().checkout_dir,
            worktree.canonicalize().unwrap().to_string_lossy()
        );
    }
}
