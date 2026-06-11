use std::path::PathBuf;

/// Managed worktree parent roots recognized by the cockpit-facing daemon
/// endpoints. Keep `/control/closeout` validation and task metadata projection
/// on this one definition so the roster never advertises a different policy
/// than the request-time guard enforces.
pub(crate) fn cockpit_managed_worktree_roots() -> [PathBuf; 2] {
    let bro_home = crate::util::bro_home_dir(&dirs::home_dir().unwrap_or_default());
    [
        bro_home.join("fleet").join("worktrees"),
        bro_home.join("agent").join("worktrees"),
    ]
}

pub(crate) fn managed_worktree_for_cwd(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd.map(str::trim).filter(|cwd| !cwd.is_empty())?;
    managed_worktree_path_for_cwd(cwd, &cockpit_managed_worktree_roots())
        .map(|path| path.to_string_lossy().into_owned())
}

/// Find the managed worktree path for a given cwd.
/// Returns the actual worktree directory (which contains a .git file for git worktrees)
/// rather than just the first component under the managed root.
pub(crate) fn managed_worktree_path_for_cwd(cwd: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    let cwd = PathBuf::from(cwd.trim());
    let cwd = canonicalize_or_self(cwd);

    for root in roots {
        let root = canonicalize_or_self(root.clone());
        let Ok(relative) = cwd.strip_prefix(&root) else {
            continue;
        };
        
        // Find the worktree by looking for the nearest ancestor with a .git marker
        // (either a .git directory or a .git file for git worktrees)
        let mut current = cwd.clone();
        while current.starts_with(&root) && current != root {
            if current.join(".git").exists() {
                return Some(current);
            }
            current = current.parent()?.to_path_buf();
        }
        
        // Fallback to first component if no .git found (for non-git managed worktrees)
        let first_component = relative.components().next()?;
        return Some(root.join(first_component.as_os_str()));
    }

    None
}

fn canonicalize_or_self(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn managed_worktree_path_maps_descendant_to_first_child_under_managed_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("fleet/worktrees");
        let worktree = root.join("task-abc");
        let descendant = worktree.join("src/bin");
        std::fs::create_dir_all(&descendant).unwrap();

        assert_eq!(
            managed_worktree_path_for_cwd(descendant.to_str().unwrap(), &[root.clone()]),
            Some(worktree)
        );
        let root_str = root.to_string_lossy().into_owned();
        assert_eq!(managed_worktree_path_for_cwd(&root_str, &[root]), None);
    }
}
