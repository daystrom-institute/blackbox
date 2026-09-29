//! Transcript project attribution for one reindex pass.
//!
//! Resolves a session cwd to the registered base project that owns it, so
//! every transcript document can carry a base-project stamp that matches work
//! from any checkout of that project. Resolution is lexical for collected
//! projects and never reads their checkout.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Base-project resolution for one reindex pass.
pub struct ToolEdgeContext {
    projects: Vec<ToolEdgeProjectAccess>,
    /// Session-cwd → resolved base project id memo (gap-72fd5932). Distinct
    /// cwds are few relative to session files, and resolution can canonicalize
    /// a path, so memoize per reindex pass.
    base_project_cache: std::sync::Mutex<BTreeMap<String, Option<String>>>,
}

/// Pure project identity plus one validated root for transcript attribution.
///
/// The carrier deliberately holds no project record. A local root is valid
/// only while its upper-layer checkout lease is alive. A collected project's
/// attachment path serves strictly as a lexical transcript namespace: nothing
/// reads that checkout.
#[derive(Clone)]
pub struct ToolEdgeProjectAccess {
    pub project_id: String,
    pub source: ToolEdgeProjectSource,
}

#[derive(Clone)]
pub enum ToolEdgeProjectSource {
    Local { local_root: PathBuf },
    Collected { transcript_root: PathBuf },
}

impl ToolEdgeProjectAccess {
    pub fn local(project_id: impl Into<String>, local_root: PathBuf) -> Self {
        Self {
            project_id: project_id.into(),
            source: ToolEdgeProjectSource::Local { local_root },
        }
    }

    pub fn collected(project_id: impl Into<String>, transcript_root: PathBuf) -> Self {
        Self {
            project_id: project_id.into(),
            source: ToolEdgeProjectSource::Collected { transcript_root },
        }
    }
}

impl ToolEdgeContext {
    pub fn with_project_access(projects: Vec<ToolEdgeProjectAccess>) -> Self {
        Self {
            projects,
            base_project_cache: std::sync::Mutex::default(),
        }
    }

    /// Resolve a session cwd to the registered base project's id, memoized
    /// across the pass (gap-72fd5932). `None` for empty cwds and paths no
    /// registered project owns.
    pub fn base_project_id_for_cwd(&self, cwd: &str) -> Option<String> {
        if cwd.is_empty() {
            return None;
        }
        let mut cache = self
            .base_project_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(hit) = cache.get(cwd) {
            return hit.clone();
        }
        let resolved = self
            .project_for_cwd_path(cwd)
            .map(|(access, _)| access.project_id.clone());
        cache.insert(cwd.to_string(), resolved.clone());
        resolved
    }

    // Index-build path; runs on the IndexWriterActor / reindex thread.
    #[allow(clippy::disallowed_methods)]
    fn project_for_cwd_path(&self, cwd: &str) -> Option<(&ToolEdgeProjectAccess, PathBuf)> {
        let local = fs::canonicalize(cwd)
            .ok()
            .and_then(|cwd| self.project_for_absolute_path(&cwd, true));
        let collected = normalize_lexical_absolute(Path::new(cwd))
            .and_then(|cwd| self.project_for_absolute_path(&cwd, false));
        most_specific_project(local, collected)
    }

    fn project_for_absolute_path(
        &self,
        absolute: &Path,
        local: bool,
    ) -> Option<(&ToolEdgeProjectAccess, PathBuf)> {
        self.projects
            .iter()
            .filter_map(|access| {
                let root = match &access.source {
                    ToolEdgeProjectSource::Local { local_root } if local => local_root,
                    ToolEdgeProjectSource::Collected { transcript_root } if !local => {
                        transcript_root
                    }
                    _ => return None,
                };
                absolute.starts_with(root).then_some((access, root.clone()))
            })
            .max_by_key(|(_access, root)| root.as_os_str().len())
    }
}

fn most_specific_project<'a>(
    left: Option<(&'a ToolEdgeProjectAccess, PathBuf)>,
    right: Option<(&'a ToolEdgeProjectAccess, PathBuf)>,
) -> Option<(&'a ToolEdgeProjectAccess, PathBuf)> {
    match (left, right) {
        (Some(left), Some(right)) => {
            if left.1.as_os_str().len() >= right.1.as_os_str().len() {
                Some(left)
            } else {
                Some(right)
            }
        }
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn normalize_lexical_absolute(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_local_root_resolves_a_cwd_without_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ctx = ToolEdgeContext::with_project_access(vec![ToolEdgeProjectAccess::local(
            "project-1",
            root.clone(),
        )]);
        assert_eq!(
            ctx.base_project_id_for_cwd(root.to_str().unwrap()),
            Some("project-1".into())
        );
        assert_eq!(ctx.base_project_id_for_cwd(""), None);
    }

    #[test]
    fn collected_transcript_root_attributes_without_reading_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let transcript_root = root.join("checkout-that-does-not-exist");
        let ctx = ToolEdgeContext::with_project_access(vec![ToolEdgeProjectAccess::collected(
            "project-1",
            transcript_root.clone(),
        )]);
        assert_eq!(
            ctx.base_project_id_for_cwd(transcript_root.join("crates").to_str().unwrap()),
            Some("project-1".into())
        );
        assert!(
            !transcript_root.exists(),
            "collected attribution must not materialize or read the checkout"
        );
    }

    /// A remote-only project contributes no local root, so a cwd under it is
    /// unattributable and must NOT be re-identified against the one project
    /// that does have a root in this pass.
    #[test]
    fn unresolved_cwds_are_never_reidentified() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let attached = root.join("attached");
        let remote = root.join("remote");
        fs::create_dir_all(&attached).unwrap();
        fs::create_dir_all(&remote).unwrap();
        let ctx = ToolEdgeContext::with_project_access(vec![ToolEdgeProjectAccess::local(
            "attached-project",
            attached,
        )]);
        assert_eq!(ctx.base_project_id_for_cwd(remote.to_str().unwrap()), None);
    }

    #[test]
    fn nested_cwd_resolves_to_the_most_specific_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let nested = root.join("crates").join("inner");
        fs::create_dir_all(&nested).unwrap();
        let ctx = ToolEdgeContext::with_project_access(vec![
            ToolEdgeProjectAccess::local("outer", root.clone()),
            ToolEdgeProjectAccess::local("inner", nested.clone()),
        ]);

        assert_eq!(
            ctx.base_project_id_for_cwd(nested.to_str().unwrap()),
            Some("inner".into())
        );
        assert_eq!(
            ctx.base_project_id_for_cwd(root.join("crates").to_str().unwrap()),
            Some("outer".into())
        );
    }
}
