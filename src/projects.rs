use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ProjectRecord {
    pub project_id: String,
    #[serde(default)]
    pub repo_id: Option<String>,
    pub canonical_path: String,
    pub registered_at: String,
    pub is_git_repo: bool,
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
        let store = load_store(&path)?;
        Ok(Self { path, store })
    }

    pub fn register_path(&mut self, path: impl AsRef<Path>) -> Result<ProjectRecord> {
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
        let record = ProjectRecord {
            project_id,
            repo_id,
            canonical_path,
            registered_at: util::now_iso(),
            is_git_repo,
        };
        self.store.projects.push(record.clone());
        self.store
            .projects
            .sort_by(|a, b| a.canonical_path.cmp(&b.canonical_path));
        self.save()?;
        Ok(record)
    }

    pub fn rename_project(&mut self, p: &ProjectRenameParams) -> Result<ProjectRenameResponse> {
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

    pub fn list(&self) -> Vec<ProjectRecord> {
        self.store.projects.clone()
    }

    pub fn load_records(path: impl AsRef<Path>) -> Result<Vec<ProjectRecord>> {
        Ok(load_store(path.as_ref())?.projects)
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(&self.store)?;
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("projects.json");
        let tmp = self.path.with_file_name(format!("{file_name}.tmp"));
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &self.path).with_context(|| {
            format!(
                "renaming {} to {}",
                tmp.display(),
                self.path.as_path().display()
            )
        })?;
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

    fn init_git_repo(path: &Path) {
        assert!(Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("init")
            .output()
            .unwrap()
            .status
            .success());
        fs::write(path.join("README.md"), "repo").unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("add")
            .arg("README.md")
            .output()
            .unwrap()
            .status
            .success());
        assert!(Command::new("git")
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
            .success());
    }
}
