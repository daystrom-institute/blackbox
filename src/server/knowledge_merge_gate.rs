use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bro_tools::fleet_worktree::{CandidateGateReport, CandidateTree};

/// Evaluate repo-owned knowledge and its generated projections from the
/// immutable candidate tree, never from either live checkout.
#[allow(clippy::disallowed_methods)]
pub(crate) fn evaluate(candidate: &CandidateTree) -> Result<CandidateGateReport> {
    let tree = tempfile::tempdir().context("creating candidate-tree sandbox")?;
    bro_tools::fleet_worktree::materialize_candidate_tree(candidate, tree.path())?;
    let roots = discover_project_roots(tree.path())?;
    if roots.is_empty() {
        return Ok(CandidateGateReport {
            ok: true,
            content: serde_json::json!({
                "projects_checked": 0,
                "render_mismatches": 0,
                "contradictions": 0,
            }),
        });
    }

    let state = tempfile::tempdir().context("creating candidate knowledge state")?;
    let mut knowledge = bbox_knowledge::knowledge::Knowledge::open(&state.path().join("kb.json"))?;
    knowledge.set_project_roots(roots.clone())?;

    let mut projects = Vec::new();
    let mut render_mismatch_count = 0usize;
    let mut contradiction_count = 0usize;
    for root in roots {
        let mut render = knowledge.check_project_render(&root)?;
        for mismatch in &mut render.mismatches {
            mismatch.path = Path::new(&mismatch.path)
                .strip_prefix(tree.path())
                .unwrap_or_else(|_| Path::new(&mismatch.path))
                .to_string_lossy()
                .replace('\\', "/");
        }
        let contradictions = knowledge.project_contradictions(&root);
        render_mismatch_count += render.mismatches.len();
        contradiction_count += contradictions.len();
        let relative_root = root
            .strip_prefix(tree.path())
            .unwrap_or(root.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        projects.push(serde_json::json!({
            "root": if relative_root.is_empty() { "." } else { &relative_root },
            "render": render,
            "contradictions": contradictions,
        }));
    }

    let ok = render_mismatch_count == 0 && contradiction_count == 0;
    Ok(CandidateGateReport {
        ok,
        content: serde_json::json!({
            "error": (!ok).then_some(format!(
                "candidate knowledge gate found {render_mismatch_count} stale render projection(s) and {contradiction_count} scoped contradiction(s)"
            )),
            "projects_checked": projects.len(),
            "render_mismatches": render_mismatch_count,
            "contradictions": contradiction_count,
            "projects": projects,
        }),
    })
}

#[allow(clippy::disallowed_methods)]
fn discover_project_roots(tree: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    let mut pending = vec![tree.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("reading candidate directory {}", dir.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            if entry.file_name() == ".bbox" {
                reject_candidate_symlink(tree, &path)?;
                if !file_type.is_dir() {
                    continue;
                }
                reject_symlinks_beneath(tree, &path.join("knowledge"))?;
                reject_symlinks_beneath(tree, &path.join("gaps"))?;
                if path.join("config.toml").is_file() || path.join("knowledge").is_dir() {
                    if let Some(project) = path.parent() {
                        for provider_file in ["AGENTS.md", "CLAUDE.md", "GEMINI.md"] {
                            reject_candidate_symlink(tree, &project.join(provider_file))?;
                        }
                        roots.push(project.to_path_buf());
                    }
                }
                continue;
            }
            if file_type.is_dir() && !file_type.is_symlink() {
                pending.push(path);
            }
        }
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

#[allow(clippy::disallowed_methods)]
fn reject_symlinks_beneath(candidate_root: &Path, path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "inspecting candidate path {}",
                    candidate_relative_path(candidate_root, path)
                )
            });
        }
    };
    reject_candidate_symlink_with_metadata(candidate_root, path, &metadata)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(path).with_context(|| {
        format!(
            "reading candidate directory {}",
            candidate_relative_path(candidate_root, path)
        )
    })? {
        let entry = entry?;
        let child = entry.path();
        let metadata = std::fs::symlink_metadata(&child).with_context(|| {
            format!(
                "inspecting candidate path {}",
                candidate_relative_path(candidate_root, &child)
            )
        })?;
        reject_candidate_symlink_with_metadata(candidate_root, &child, &metadata)?;
        if metadata.is_dir() {
            reject_symlinks_beneath(candidate_root, &child)?;
        }
    }
    Ok(())
}

#[allow(clippy::disallowed_methods)]
fn reject_candidate_symlink(candidate_root: &Path, path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => reject_candidate_symlink_with_metadata(candidate_root, path, &metadata),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "inspecting candidate path {}",
                candidate_relative_path(candidate_root, path)
            )
        }),
    }
}

fn reject_candidate_symlink_with_metadata(
    candidate_root: &Path,
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<()> {
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "candidate path contains symlink: {}",
            candidate_relative_path(candidate_root, path)
        );
    }
    Ok(())
}

fn candidate_relative_path(candidate_root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(candidate_root).unwrap_or(path);
    let rendered = relative.to_string_lossy().replace('\\', "/");
    if rendered.is_empty() {
        ".".to_string()
    } else {
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_knowledge::knowledge::{Knowledge, RenderParams};
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn candidate(root: &Path) -> CandidateTree {
        CandidateTree {
            repo: root.to_path_buf(),
            tree_oid: git(root, &["rev-parse", "HEAD^{tree}"]),
            target: "main".into(),
            branch: "feature".into(),
        }
    }

    #[test]
    fn candidate_gate_accepts_current_render_and_rejects_stale_projection() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        std::fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        std::fs::write(
            root.join(".bbox/knowledge/rule.json"),
            r#"{
  "id": "rule",
  "title": "candidate rule",
  "content": "always use the candidate tree",
  "category": "convention",
  "scope": "project",
  "priority": "standard",
  "weight": 100,
  "render": true,
  "decay": true,
  "status": "active",
  "approval": "user_confirmed",
  "source": "user",
  "created_at": "2026-07-21T00:00:00Z",
  "updated_at": "2026-07-21T00:00:00Z",
  "recall_count": 0
}"#,
        )
        .unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut knowledge = Knowledge::open(&state.path().join("kb.json")).unwrap();
        knowledge.set_project_roots(vec![root.clone()]).unwrap();
        knowledge
            .render(&RenderParams {
                project: Some(root.to_string_lossy().into_owned()),
                scope: Some("project".into()),
                dry_run: Some(false),
                ..Default::default()
            })
            .unwrap();
        git(
            &root,
            &["add", ".bbox", "AGENTS.md", "CLAUDE.md", "GEMINI.md"],
        );
        git(&root, &["commit", "-q", "-m", "seed"]);
        let first = evaluate(&candidate(&root)).unwrap();
        assert!(
            first.ok,
            "current projections should pass: {:?}",
            first.content
        );

        std::fs::write(
            root.join(".bbox/knowledge/rule.json"),
            std::fs::read_to_string(root.join(".bbox/knowledge/rule.json"))
                .unwrap()
                .replace("candidate tree", "immutable candidate tree"),
        )
        .unwrap();
        git(&root, &["add", ".bbox/knowledge/rule.json"]);
        git(&root, &["commit", "-q", "-m", "stale knowledge"]);
        let stale = evaluate(&candidate(&root)).unwrap();
        assert!(!stale.ok);
        assert_eq!(stale.content["render_mismatches"], serde_json::json!(3));
    }

    #[cfg(unix)]
    #[test]
    fn candidate_gate_rejects_repo_owned_and_provider_symlinks_without_target_disclosure() {
        use std::os::unix::fs::symlink;

        for candidate_path in [
            ".bbox/knowledge/rule.json",
            ".bbox/gaps/gap.json",
            "AGENTS.md",
            "CLAUDE.md",
            "GEMINI.md",
        ] {
            let repo = tempfile::tempdir().unwrap();
            let root = repo.path().canonicalize().unwrap();
            git(&root, &["init", "-q", "-b", "main"]);
            git(&root, &["config", "user.email", "test@example.com"]);
            git(&root, &["config", "user.name", "Test"]);
            std::fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
            std::fs::create_dir_all(root.join(".bbox/gaps")).unwrap();
            std::fs::write(
                root.join(".bbox/config.toml"),
                "[project]\nrepo_id = \"merge-gate-symlink-fixture\"\n",
            )
            .unwrap();
            let link = root.join(candidate_path);
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            symlink("../../outside-secret", &link).unwrap();
            git(&root, &["add", "."]);
            git(&root, &["commit", "-q", "-m", "candidate symlink"]);

            let err = evaluate(&candidate(&root)).unwrap_err();
            let diagnostic = format!("{err:#}");
            assert!(
                diagnostic.contains(&format!(
                    "candidate path contains symlink: {candidate_path}"
                )),
                "{candidate_path}: {diagnostic}"
            );
            assert!(
                !diagnostic.contains("outside-secret"),
                "symlink target leaked for {candidate_path}: {diagnostic}"
            );
        }
    }
}
