//! Filesystem-backed evidence probes shared by project-catalog frontends.
//!
//! Probing stays outside [`crate::project_catalog_admin`] transactions. Both
//! the daemon MCP surface and the offline administration CLI use these
//! helpers so attachment-proved transitions interpret committed checkout
//! authority identically.

use std::collections::BTreeMap;
use std::path::Path;

use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{AttachmentId, AttachmentStatus, ProjectId};

use crate::project_catalog_store::ProjectCatalogStore;

/// Resolve the published scope committed at `HEAD` for one checkout project.
///
/// A non-Git checkout or committed project configuration without a recorded
/// repository id proves no published scope. Filesystem and catalog failures
/// are returned to the caller so it can fail closed or record an unreadable
/// attachment as appropriate for its operation.
#[allow(clippy::disallowed_methods)]
pub fn committed_scope_at_head(raw_path: &str) -> anyhow::Result<Option<PublishedScope>> {
    let requested = Path::new(raw_path);
    if !requested.is_absolute() {
        anyhow::bail!("error.project_catalog_admin_path: path must be absolute");
    }
    let project_dir = std::fs::canonicalize(requested)
        .map_err(|error| anyhow::anyhow!("resolving {}: {error}", requested.display()))?;
    if !project_dir.is_dir() {
        anyhow::bail!("error.project_catalog_admin_path: path is not a directory");
    }

    let Some(git_root) = bbox_corpus_core::git::git_root_for_path(&project_dir)
        .and_then(|root| std::fs::canonicalize(root).ok())
    else {
        return Ok(None);
    };
    let committed = match bbox_config::config::load_project_at_ref(&project_dir, "HEAD") {
        Ok(config) => config,
        Err(_) => return Ok(None),
    };
    let Some(repo_id) = committed.project.repo_id else {
        return Ok(None);
    };
    let Some(relpath) = bbox_corpus_core::identity::bbox_root_relpath(&git_root, &project_dir)
    else {
        return Ok(None);
    };
    Ok(PublishedScope::try_new(repo_id, relpath).ok())
}

/// Probe committed scope evidence for every active attachment of one project.
///
/// An unreadable or unrecorded checkout is retained as `None`. Promotion then
/// refuses in the pure domain transaction with the exact attachment id instead
/// of treating a missing observation as agreement.
pub fn active_attachment_scopes(
    store: &ProjectCatalogStore,
    project_id: &ProjectId,
) -> anyhow::Result<BTreeMap<AttachmentId, Option<PublishedScope>>> {
    let state = store
        .snapshot()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut scopes = BTreeMap::new();
    for row in state.attachments().attachments.values() {
        if &row.project_id != project_id || row.status != AttachmentStatus::Attached {
            continue;
        }
        let scope = committed_scope_at_head(&row.checkout_project_dir)
            .ok()
            .flatten();
        scopes.insert(row.attachment_id.clone(), scope);
    }
    Ok(scopes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn committed_scope_uses_head_and_ignores_working_tree_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".bbox")).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.email", "probe@example.invalid"]);
        git(&root, &["config", "user.name", "probe"]);
        std::fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"committed-authority\"\n",
        )
        .unwrap();
        git(&root, &["add", ".bbox/config.toml"]);
        git(&root, &["commit", "-m", "record authority"]);

        std::fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"working-authority\"\n",
        )
        .unwrap();

        let scope = committed_scope_at_head(root.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(scope.repo_id(), "committed-authority");
        assert_eq!(scope.bbox_root_relpath(), ".");
    }

    #[test]
    fn checkout_without_committed_authority_proves_no_scope() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.email", "probe@example.invalid"]);
        git(&root, &["config", "user.name", "probe"]);
        std::fs::write(root.join("README.md"), "probe\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-m", "initial state"]);

        assert!(
            committed_scope_at_head(root.to_str().unwrap())
                .unwrap()
                .is_none()
        );
    }
}
