//! Path-free contract between a checkout-side Git blame executor and the
//! corpus-side provenance/edge join.

use std::path::{Component, Path};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::identity::PublishedScope;

pub const BLAME_TRANSPORT_VERSION: u32 = 1;
pub const MAX_BLAME_PATH_BYTES: usize = 4 * 1024;
pub const MAX_BLAME_AUTHOR_BYTES: usize = 512;
pub const MAX_BLAME_TIME_BYTES: usize = 128;
pub const MAX_BLAME_PROJECT_ID_BYTES: usize = 256;
pub const OPERATOR_BLAME_REPO_ID_HEADER: &str = "x-blackbox-blame-repo-id";
pub const OPERATOR_BLAME_ROOT_RELPATH_HEADER: &str = "x-blackbox-blame-root-relpath";
pub const OPERATOR_BLAME_WORKSPACE_ID_HEADER: &str = "x-blackbox-blame-workspace-id";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameExecutionPlanV1 {
    pub version: u32,
    pub project_id: String,
    pub scope: PublishedScope,
    pub workspace_id: String,
    pub target: BlamePlanTargetV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlamePlanTargetV1 {
    WorkspacePath {
        input_path: String,
        line: u64,
    },
    ProjectSnapshot {
        project_relative_path: String,
        display_path: String,
        line: Option<u64>,
        byte_offset: u64,
        commit: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameFactV1 {
    pub version: u32,
    pub project_id: String,
    pub scope: PublishedScope,
    pub workspace_id: String,
    pub git_relative_path: String,
    pub display_path: String,
    pub line: u64,
    pub execution: BlameExecutionV1,
    pub attribution: Option<BlameAttributionV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlameExecutionV1 {
    WorkspaceCurrent { head_commit: Option<String> },
    Snapshot { commit: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameAttributionV1 {
    pub commit_sha: String,
    pub author: String,
    pub author_time: Option<String>,
    pub git_relative_path: String,
}

impl BlameExecutionPlanV1 {
    pub fn validate(&self) -> Result<()> {
        validate_common(
            self.version,
            &self.project_id,
            &self.scope,
            &self.workspace_id,
        )?;
        match &self.target {
            BlamePlanTargetV1::WorkspacePath { input_path, line } => {
                bounded_nonempty(input_path, MAX_BLAME_PATH_BYTES, "input path")?;
                if *line == 0 {
                    bail!("error.blame_plan_invalid: line must be 1-based");
                }
            }
            BlamePlanTargetV1::ProjectSnapshot {
                project_relative_path,
                display_path,
                line,
                commit,
                ..
            } => {
                validate_relative_path(project_relative_path, "project-relative path")?;
                validate_relative_path(display_path, "display path")?;
                if line.is_some_and(|line| line == 0) {
                    bail!("error.blame_plan_invalid: line must be 1-based");
                }
                validate_oid(commit, "snapshot commit")?;
            }
        }
        Ok(())
    }
}

impl BlameFactV1 {
    pub fn validate(&self) -> Result<()> {
        validate_common(
            self.version,
            &self.project_id,
            &self.scope,
            &self.workspace_id,
        )?;
        validate_relative_path(&self.git_relative_path, "Git-relative path")?;
        validate_relative_path(&self.display_path, "display path")?;
        if self.line == 0 {
            bail!("error.blame_fact_invalid: line must be 1-based");
        }
        match &self.execution {
            BlameExecutionV1::WorkspaceCurrent { head_commit } => {
                if let Some(head_commit) = head_commit {
                    validate_oid(head_commit, "workspace HEAD")?;
                }
            }
            BlameExecutionV1::Snapshot { commit } => {
                validate_oid(commit, "snapshot commit")?;
            }
        }
        if let Some(attribution) = &self.attribution {
            validate_oid(&attribution.commit_sha, "attributed commit")?;
            bounded(&attribution.author, MAX_BLAME_AUTHOR_BYTES, "author")?;
            if let Some(author_time) = &attribution.author_time {
                bounded(author_time, MAX_BLAME_TIME_BYTES, "author time")?;
            }
            validate_relative_path(&attribution.git_relative_path, "attribution path")?;
            if attribution.git_relative_path != self.git_relative_path {
                bail!(
                    "error.blame_fact_invalid: attribution path does not match the requested Git-relative path"
                );
            }
        }
        Ok(())
    }

    pub fn validate_against(&self, plan: &BlameExecutionPlanV1) -> Result<()> {
        self.validate()?;
        plan.validate()?;
        if self.project_id != plan.project_id
            || self.scope != plan.scope
            || self.workspace_id != plan.workspace_id
        {
            bail!(
                "error.blame_fact_authority: fact does not match the planned workspace authority"
            );
        }
        match (&plan.target, &self.execution) {
            (
                BlamePlanTargetV1::WorkspacePath { line, .. },
                BlameExecutionV1::WorkspaceCurrent { .. },
            ) if self.line == *line => {}
            (
                BlamePlanTargetV1::ProjectSnapshot {
                    project_relative_path,
                    display_path,
                    line,
                    commit,
                    ..
                },
                BlameExecutionV1::Snapshot {
                    commit: fact_commit,
                },
            ) if project_relative_path == &self.display_path
                && display_path == &self.display_path
                && line.is_none_or(|line| line == self.line)
                && commit == fact_commit => {}
            _ => {
                bail!(
                    "error.blame_fact_authority: fact execution does not match the planned target"
                )
            }
        }
        Ok(())
    }
}

/// Execute one daemon-authored plan inside the checkout that owns its files
/// and Git object database. Both the managed harness and the attended CLI use
/// this leaf so path confinement and snapshot semantics cannot drift.
pub fn execute_plan_in_workspace(
    plan: &BlameExecutionPlanV1,
    workspace_root: &Path,
    project_root: &Path,
    expected_scope: &PublishedScope,
    expected_workspace_id: &str,
) -> Result<BlameFactV1> {
    plan.validate()?;
    if &plan.scope != expected_scope || plan.workspace_id != expected_workspace_id {
        bail!("blame plan is outside the bound workspace authority");
    }
    let workspace_root = workspace_root
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("canonicalizing bound Git root: {error}"))?;
    let project_root = project_root
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("canonicalizing bound project root: {error}"))?;
    if !project_root.starts_with(&workspace_root) {
        bail!("error.checkout_path_invalid: bound project root is outside the Git root");
    }

    let (git_relative_path, display_path, line, execution, blame) = match &plan.target {
        BlamePlanTargetV1::WorkspacePath { input_path, line } => {
            let input = Path::new(input_path);
            if input
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            {
                bail!("error.checkout_path_invalid: blame path contains parent traversal");
            }
            let requested = if input.is_absolute() {
                input.to_path_buf()
            } else {
                project_root.join(input)
            };
            let file = requested
                .canonicalize()
                .map_err(|error| anyhow::anyhow!("canonicalizing bound blame path: {error}"))?;
            if !file.is_file() || !file.starts_with(&project_root) {
                bail!(
                    "error.checkout_attachment_not_found: blame path is outside the bound project"
                );
            }
            let display = file
                .strip_prefix(&project_root)
                .map_err(|error| anyhow::anyhow!("deriving project-relative blame path: {error}"))?
                .to_path_buf();
            let git_relative = file
                .strip_prefix(&workspace_root)
                .map_err(|error| anyhow::anyhow!("deriving Git-relative blame path: {error}"))?
                .to_path_buf();
            let blame = crate::git::blame_for_line_in_root(&workspace_root, &git_relative, *line)?;
            (
                slash_path(&git_relative),
                slash_path(&display),
                *line,
                BlameExecutionV1::WorkspaceCurrent {
                    head_commit: crate::git::current_head(&workspace_root),
                },
                blame,
            )
        }
        BlamePlanTargetV1::ProjectSnapshot {
            project_relative_path,
            display_path,
            line,
            byte_offset,
            commit,
        } => {
            let project_prefix = project_root
                .strip_prefix(&workspace_root)
                .map_err(|error| anyhow::anyhow!("deriving bound project Git prefix: {error}"))?;
            let git_relative = project_prefix.join(project_relative_path);
            let (resolved_line, blame) = crate::git::blame_for_line_or_offset_at_commit(
                &workspace_root,
                &git_relative,
                commit,
                *line,
                *byte_offset,
            )?;
            (
                slash_path(&git_relative),
                display_path.clone(),
                resolved_line,
                BlameExecutionV1::Snapshot {
                    commit: commit.clone(),
                },
                blame,
            )
        }
    };
    let attribution = blame.map(|blame| BlameAttributionV1 {
        commit_sha: blame.commit_sha,
        author: blame.author,
        author_time: blame.author_time,
        git_relative_path: blame.rel_path,
    });
    let fact = BlameFactV1 {
        version: BLAME_TRANSPORT_VERSION,
        project_id: plan.project_id.clone(),
        scope: expected_scope.clone(),
        workspace_id: expected_workspace_id.to_string(),
        git_relative_path,
        display_path,
        line,
        execution,
        attribution,
    };
    fact.validate_against(plan)?;
    Ok(fact)
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn validate_common(
    version: u32,
    project_id: &str,
    scope: &PublishedScope,
    workspace_id: &str,
) -> Result<()> {
    if version != BLAME_TRANSPORT_VERSION {
        bail!("error.blame_transport_version: unsupported blame transport version {version}");
    }
    bounded_nonempty(project_id, MAX_BLAME_PROJECT_ID_BYTES, "project id")?;
    scope.validate()?;
    if workspace_id.len() != 32
        || !workspace_id
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("error.blame_fact_authority: workspace id is not canonical");
    }
    Ok(())
}

fn validate_relative_path(value: &str, label: &str) -> Result<()> {
    bounded_nonempty(value, MAX_BLAME_PATH_BYTES, label)?;
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("error.blame_path_invalid: {label} must be a safe relative path");
    }
    Ok(())
}

fn validate_oid(value: &str, label: &str) -> Result<()> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("error.blame_fact_invalid: {label} is not a canonical Git object id");
    }
    Ok(())
}

fn bounded_nonempty(value: &str, max: usize, label: &str) -> Result<()> {
    if value.is_empty() {
        bail!("error.blame_fact_invalid: {label} is empty");
    }
    bounded(value, max, label)
}

fn bounded(value: &str, max: usize, label: &str) -> Result<()> {
    if value.len() > max {
        bail!("error.blame_fact_invalid: {label} exceeds {max} encoded bytes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo", ".").unwrap()
    }

    fn plan() -> BlameExecutionPlanV1 {
        BlameExecutionPlanV1 {
            version: BLAME_TRANSPORT_VERSION,
            project_id: "project".into(),
            scope: scope(),
            workspace_id: "a".repeat(32),
            target: BlamePlanTargetV1::ProjectSnapshot {
                project_relative_path: "src/lib.rs".into(),
                display_path: "src/lib.rs".into(),
                line: Some(7),
                byte_offset: 0,
                commit: "b".repeat(40),
            },
        }
    }

    fn fact() -> BlameFactV1 {
        BlameFactV1 {
            version: BLAME_TRANSPORT_VERSION,
            project_id: "project".into(),
            scope: scope(),
            workspace_id: "a".repeat(32),
            git_relative_path: "src/lib.rs".into(),
            display_path: "src/lib.rs".into(),
            line: 7,
            execution: BlameExecutionV1::Snapshot {
                commit: "b".repeat(40),
            },
            attribution: Some(BlameAttributionV1 {
                commit_sha: "c".repeat(40),
                author: "author".into(),
                author_time: Some("1 +0000".into()),
                git_relative_path: "src/lib.rs".into(),
            }),
        }
    }

    #[test]
    fn exact_plan_and_fact_validate() {
        fact().validate_against(&plan()).unwrap();
    }

    #[test]
    fn mismatched_snapshot_or_workspace_refuses() {
        let mut wrong_commit = fact();
        wrong_commit.execution = BlameExecutionV1::Snapshot {
            commit: "d".repeat(40),
        };
        assert!(wrong_commit.validate_against(&plan()).is_err());

        let mut wrong_workspace = fact();
        wrong_workspace.workspace_id = "e".repeat(32);
        assert!(wrong_workspace.validate_against(&plan()).is_err());
    }

    #[test]
    fn paths_and_payloads_are_bounded_and_relative() {
        let mut escaped = fact();
        escaped.git_relative_path = "../escape".into();
        assert!(escaped.validate().is_err());

        let mut rooted = fact();
        rooted.display_path = "/tmp/file".into();
        assert!(rooted.validate().is_err());

        let mut oversized = fact();
        oversized.attribution.as_mut().unwrap().author = "x".repeat(MAX_BLAME_AUTHOR_BYTES + 1);
        assert!(oversized.validate().is_err());
    }
}
