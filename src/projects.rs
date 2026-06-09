use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

pub use bbox_corpus_core::language::Language;

use crate::entity_ref;
use crate::util;

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
pub(crate) struct ProjectInitParams {
    pub path: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectStore {
    version: u32,
    projects: Vec<ProjectRecord>,
}

impl Default for ProjectStore {
    fn default() -> Self {
        Self {
            version: 1,
            projects: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectRegistry {
    path: PathBuf,
    store: ProjectStore,
}

impl ProjectRegistry {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut store = load_store(&path)?;
        let mut dirty = false;
        for record in store.projects.iter_mut() {
            if !record.languages.is_empty() {
                continue;
            }
            let canonical = PathBuf::from(&record.canonical_path);
            if !canonical.is_dir() {
                continue;
            }
            let detected = detect_languages(&canonical);
            if !detected.is_empty() {
                record.languages = detected;
                dirty = true;
            }
        }
        let registry = Self { path, store };
        if dirty {
            registry.save()?;
        }
        Ok(registry)
    }

    pub fn register_path(&mut self, path: impl AsRef<Path>) -> Result<ProjectRecord> {
        let path = path.as_ref().to_path_buf();
        let store_path = self.path.clone();
        crate::json_store::with_store_lock(&store_path, || {
            self.reload()?;
            self.register_path_locked(&path)
        })
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
            return Ok(existing.clone());
        }
        let project_id = entity_ref::project_id_for_path(&canonical)?;
        if let Some(existing) = self
            .store
            .projects
            .iter()
            .find(|project| project.project_id == project_id)
        {
            return Ok(existing.clone());
        }

        let git_root = entity_ref::git_root_for_path(&canonical);
        let repo_id = git_root
            .as_deref()
            .map(entity_ref::repo_id_for_root)
            .transpose()?;
        let is_git_repo = git_root.is_some();
        let languages = detect_languages(&canonical);
        let record = ProjectRecord {
            project_id,
            repo_id,
            canonical_path,
            registered_at: util::now_iso(),
            is_git_repo,
            languages,
        };
        self.store.projects.push(record.clone());
        self.store
            .projects
            .sort_by(|a, b| a.canonical_path.cmp(&b.canonical_path));
        self.save()?;
        Ok(record)
    }

    pub fn rename_project(&mut self, p: &ProjectRenameParams) -> Result<ProjectRenameResponse> {
        let store_path = self.path.clone();
        crate::json_store::with_store_lock(&store_path, || {
            self.reload()?;
            self.rename_project_locked(p)
        })
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
                    .map(entity_ref::repo_id_for_root)
                    .transpose()?,
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
            self.save()?;
        }

        Ok(ProjectRenameResponse {
            old_record,
            record,
            moved_on_disk: move_on_disk && !dry_run,
            dry_run,
        })
    }

    /// Resolve `raw` (project_id, canonical_path, or absolute path) without
    /// mutating the registry. Returns `None` when no match is registered.
    pub fn resolve(&self, raw: &str) -> Result<Option<ProjectRecord>> {
        Ok(self
            .resolve_project_index(raw)?
            .map(|idx| self.store.projects[idx].clone()))
    }

    pub fn unregister_project(&mut self, raw: &str) -> Result<ProjectRecord> {
        let store_path = self.path.clone();
        let raw = raw.to_string();
        crate::json_store::with_store_lock(&store_path, || {
            self.reload()?;
            let idx = self
                .resolve_project_index(&raw)?
                .with_context(|| format!("project not registered: {raw}"))?;
            let removed = self.store.projects.remove(idx);
            self.save()?;
            Ok(removed)
        })
    }

    pub fn list(&self) -> Vec<ProjectRecord> {
        self.store.projects.clone()
    }

    pub fn load_records(path: impl AsRef<Path>) -> Result<Vec<ProjectRecord>> {
        Ok(load_store(path.as_ref())?.projects)
    }

    fn save(&self) -> Result<()> {
        crate::json_store::atomic_write_json_locked(&self.path, &self.store)
    }

    pub fn reload(&mut self) -> Result<()> {
        if self.path.exists() {
            let raw = fs::read_to_string(&self.path)
                .with_context(|| format!("reading {}", self.path.display()))?;
            self.store = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", self.path.display()))?;
        }
        Ok(())
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

/// If `project_dir` is a managed fleet worktree, synthesize a [`ProjectRecord`]
/// aliasing it to its registered base project. A managed fleet worktree is one
/// whose checked-out branch is `bro-fleet/*` and whose git common dir matches a
/// registered project — fleet dispatch creates these outside the registered
/// repo root (under the daemon state dir), so the literal worktree path is not a
/// descendant of any registered root and project-scoped tools would otherwise
/// reject it.
///
/// Returns `None` when the path is already a registered root or descendant (no
/// aliasing needed), is not on a `bro-fleet/*` branch, or no registered project
/// shares its git common dir. The synthesized record carries a `:fleet-worktree`
/// project_id suffix and the worktree's own canonical path, so a registration
/// check accepts the worktree while callers can still tell it apart from a
/// first-class registered root.
///
/// Shared by the slice tools ([`crate::slices`]) and code navigation
/// ([`crate::code_nav`]) so a fleet-dispatched agent working inside an isolated
/// worktree can use project-scoped tools without registering each ephemeral
/// worktree.
pub(crate) fn managed_fleet_worktree_project(
    project_dir: Option<&str>,
    projects: &[ProjectRecord],
) -> Option<ProjectRecord> {
    let (base, worktree) = resolve_managed_fleet_worktree(project_dir, projects)?;
    Some(ProjectRecord {
        project_id: format!("{}:fleet-worktree", base.project_id),
        repo_id: base.repo_id.clone(),
        canonical_path: worktree.to_string_lossy().into_owned(),
        registered_at: "fleet-managed".to_string(),
        is_git_repo: true,
        languages: base.languages.clone(),
    })
}

/// For a path that is a managed fleet worktree of a registered project, return
/// `(base_canonical_path, worktree_canonical_path)`. The base is the worktree's
/// durable scope (host-local thread keying, project-scoped queries) while the
/// worktree is where repo-owned artifacts (e.g. committed thread records) should
/// be written so they travel with the agent's branch. `None` when the path is
/// not a managed fleet worktree (already registered/descendant, not `bro-fleet/*`,
/// or no registered base shares its git common dir).
pub(crate) fn fleet_worktree_scope_and_dir(
    project_dir: &str,
    projects: &[ProjectRecord],
) -> Option<(String, String)> {
    let (base, worktree) = resolve_managed_fleet_worktree(Some(project_dir), projects)?;
    Some((
        base.canonical_path.clone(),
        worktree.to_string_lossy().into_owned(),
    ))
}

/// Shared core: resolve a path to `(base_record, canonical_worktree)` when it is
/// a managed fleet worktree of a registered project. A managed fleet worktree is
/// one whose checked-out branch is `bro-fleet/*` and whose git common dir matches
/// a registered project — fleet dispatch creates these outside the registered
/// repo root (under the daemon state dir), so the literal worktree path is not a
/// descendant of any registered root. Returns `None` when the path is already a
/// registered root/descendant (no resolution needed — early-returns before any
/// git call), is not on a `bro-fleet/*` branch, or no registered project shares
/// its git common dir.
fn resolve_managed_fleet_worktree<'a>(
    project_dir: Option<&str>,
    projects: &'a [ProjectRecord],
) -> Option<(&'a ProjectRecord, PathBuf)> {
    let project_dir = project_dir?;
    let worktree = fs::canonicalize(project_dir).ok()?;
    if projects.iter().any(|project| {
        let root = Path::new(&project.canonical_path);
        worktree == root || worktree.starts_with(root)
    }) {
        return None;
    }
    let branch = crate::git::current_branch(&worktree)?;
    if !branch.starts_with("bro-fleet/") {
        return None;
    }
    let worktree_common = crate::git::git_common_dir(&worktree)?;
    let base = projects.iter().find(|project| {
        crate::git::git_common_dir(Path::new(&project.canonical_path))
            .is_some_and(|common| common == worktree_common)
    })?;
    Some((base, worktree))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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

    #[test]
    fn open_redetects_languages_for_legacy_records() {
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

        let registry = ProjectRegistry::open(&store_path).unwrap();
        let recs = registry.list();
        assert_eq!(recs.len(), 1);
        assert!(recs[0].languages.contains(&Language::Rust));

        // Persisted on disk so subsequent loads skip the walk.
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
}
