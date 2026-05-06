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
        let project_id = entity_ref::project_id_for_path(&canonical)?;
        if let Some(existing) = self
            .store
            .projects
            .iter()
            .find(|project| project.project_id == project_id)
        {
            return Ok(existing.clone());
        }

        let is_git_repo = entity_ref::git_root_for_path(&canonical).is_some();
        let repo_id = if is_git_repo {
            Some(entity_ref::repo_id_for_path(&canonical)?)
        } else {
            None
        };
        let record = ProjectRecord {
            project_id,
            repo_id,
            canonical_path: canonical.to_string_lossy().into_owned(),
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
        let tmp = self.path.with_extension("json.tmp");
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
    let canonical = fs::canonicalize(path.as_ref())?;
    if canonical.is_file() {
        anyhow::bail!(
            "project path must be a directory, not a file: {}",
            canonical.display()
        );
    }
    Ok(canonical)
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
