//! Published snapshots of repo-owned gap notes.
//!
//! Published gaps come only from committed trees: a publisher ref on the
//! bridge, or exact committed sources for an accepted-publication build.
//! Working-tree bytes are never part of a published snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use bbox_corpus_core::git;
use bbox_corpus_core::identity::PublishedScope;
use sha2::{Digest, Sha256};

use crate::gaps::GapNote;

#[derive(Debug, Clone)]
pub struct PublishedGapEntry {
    pub gap: GapNote,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub struct PublishedGapSnapshot {
    pub published_scope: PublishedScope,
    pub published_ref: String,
    pub publisher_commit: String,
    pub gaps: BTreeMap<String, PublishedGapEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedGapSourceLimits {
    max_entries: usize,
    max_file_bytes: usize,
    max_total_bytes: usize,
    max_listing_bytes: usize,
}

impl PublishedGapSourceLimits {
    pub const MAX_ENTRIES: usize = 100_000;
    pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
    pub const MAX_TOTAL_BYTES: usize = 128 * 1024 * 1024;
    pub const MAX_LISTING_BYTES: usize = 32 * 1024 * 1024;

    pub fn try_new(
        max_entries: usize,
        max_file_bytes: usize,
        max_total_bytes: usize,
        max_listing_bytes: usize,
    ) -> Result<Self> {
        validate_source_limit(max_entries, Self::MAX_ENTRIES, "published gap entry")?;
        validate_source_limit(
            max_file_bytes,
            Self::MAX_FILE_BYTES,
            "published gap per-file byte",
        )?;
        validate_source_limit(
            max_total_bytes,
            Self::MAX_TOTAL_BYTES,
            "published gap total byte",
        )?;
        validate_source_limit(
            max_listing_bytes,
            Self::MAX_LISTING_BYTES,
            "published gap listing byte",
        )?;
        Ok(Self {
            max_entries,
            max_file_bytes,
            max_total_bytes,
            max_listing_bytes,
        })
    }

    pub fn max_entries(self) -> usize {
        self.max_entries
    }

    pub fn max_file_bytes(self) -> usize {
        self.max_file_bytes
    }

    pub fn max_total_bytes(self) -> usize {
        self.max_total_bytes
    }

    pub fn max_listing_bytes(self) -> usize {
        self.max_listing_bytes
    }
}

impl Default for PublishedGapSourceLimits {
    fn default() -> Self {
        Self {
            max_entries: Self::MAX_ENTRIES,
            max_file_bytes: Self::MAX_FILE_BYTES,
            max_total_bytes: Self::MAX_TOTAL_BYTES,
            max_listing_bytes: Self::MAX_LISTING_BYTES,
        }
    }
}

fn validate_source_limit(value: usize, ceiling: usize, label: &str) -> Result<()> {
    if value == 0 || value > ceiling {
        anyhow::bail!("{label} limit must be between 1 and {ceiling}");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedGapSourceFile {
    pub repository_relative_filename: String,
    pub source_bytes: Vec<u8>,
}

pub fn load_published_snapshot(
    publisher_root: &Path,
    published_ref: &str,
    scope: &PublishedScope,
    durable_project: &str,
) -> Result<PublishedGapSnapshot> {
    let publisher_commit =
        git::resolve_commit(publisher_root, published_ref).with_context(|| {
            format!(
                "published ref {published_ref} does not resolve in {}",
                publisher_root.display()
            )
        })?;
    load_published_snapshot_at_commit(
        publisher_root,
        published_ref,
        &publisher_commit,
        scope,
        durable_project,
    )
}

pub fn load_published_snapshot_at_commit(
    publisher_root: &Path,
    published_ref: &str,
    publisher_commit: &str,
    scope: &PublishedScope,
    durable_project: &str,
) -> Result<PublishedGapSnapshot> {
    let tree_dir = gaps_tree_dir(scope);
    let files = read_committed_map(publisher_root, publisher_commit, &tree_dir, None)?;
    let mut gaps = BTreeMap::new();
    for (filename, bytes) in files {
        let mut gap: GapNote = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing published gap file {filename}"))?;
        validate_filename_id(&filename, &gap.id, "published gap")?;
        stamp_gap(&mut gap, durable_project);
        let id = gap.id.clone();
        if gaps
            .insert(
                id.clone(),
                PublishedGapEntry {
                    gap,
                    content_hash: sha256(&bytes),
                },
            )
            .is_some()
        {
            anyhow::bail!("duplicate published gap id: {id}");
        }
    }
    Ok(PublishedGapSnapshot {
        published_scope: scope.clone(),
        published_ref: published_ref.to_string(),
        publisher_commit: publisher_commit.to_string(),
        gaps,
    })
}

/// Load exact committed gap JSON for an accepted-publication build.
///
/// This path does not stamp host-local project metadata or normalize records.
/// It validates the committed lane and returns byte-exact, deterministically
/// ordered source files for the transaction-owned publication builder.
pub fn load_published_gap_sources_at_commit(
    publisher_root: &Path,
    publisher_commit: &str,
    scope: &PublishedScope,
    alternate_root: Option<&Path>,
    limits: PublishedGapSourceLimits,
) -> Result<Vec<PublishedGapSourceFile>> {
    const MAX_TREE_ENTRIES: usize = 200_000;

    scope.validate().context("invalid published gap scope")?;
    let verified_commit =
        verify_commit_from_repository_path(publisher_root, publisher_commit, alternate_root)
            .with_context(|| {
                format!(
                    "verifying exact published gap commit in {}",
                    publisher_root.display()
                )
            })?;
    let tree_dir = gaps_tree_dir(scope);
    let prefix = format!("{tree_dir}/");
    let repo_paths = git::list_verified_committed_dir_bounded(
        &verified_commit,
        &tree_dir,
        MAX_TREE_ENTRIES,
        limits.max_listing_bytes,
    )
    .with_context(|| {
        format!(
            "listing bounded committed gaps at {publisher_commit} in {}",
            publisher_root.display()
        )
    })?;

    let mut total_bytes = 0_usize;
    let mut entry_count = 0_usize;
    let mut ids = BTreeSet::new();
    let mut sources = Vec::with_capacity(repo_paths.len().min(limits.max_entries));
    for repo_path in repo_paths {
        let filename = repo_path
            .strip_prefix(&prefix)
            .ok_or_else(|| anyhow::anyhow!("committed gap path is outside its published scope"))?;
        if filename.contains('/') || !filename.ends_with(".json") {
            continue;
        }
        entry_count = entry_count
            .checked_add(1)
            .context("published gap entry count overflowed")?;
        if entry_count > limits.max_entries {
            anyhow::bail!("published gap sources exceed their entry limit");
        }
        validate_snapshot_filename(filename, "published gap")?;
        let remaining = limits
            .max_total_bytes
            .checked_sub(total_bytes)
            .ok_or_else(|| {
                anyhow::anyhow!("published gap sources exceed their total byte limit")
            })?;
        let read_limit = limits.max_file_bytes.min(remaining);
        let source_bytes = git::read_verified_committed_file_bytes_bounded(
            &verified_commit,
            &repo_path,
            read_limit,
        )
        .with_context(|| {
            format!("reading bounded committed gap file {repo_path} at {publisher_commit}")
        })?;
        total_bytes = total_bytes
            .checked_add(source_bytes.len())
            .ok_or_else(|| anyhow::anyhow!("published gap source total byte count overflowed"))?;
        if total_bytes > limits.max_total_bytes {
            anyhow::bail!("published gap sources exceed their total byte limit");
        }
        let gap: GapNote = serde_json::from_slice(&source_bytes)
            .with_context(|| format!("parsing published gap source {repo_path}"))?;
        validate_filename_id(filename, &gap.id, "published gap source")?;
        if !ids.insert(gap.id) {
            anyhow::bail!("published gap sources contain a duplicate record id");
        }
        sources.push(PublishedGapSourceFile {
            repository_relative_filename: repo_path,
            source_bytes,
        });
    }
    Ok(sources)
}

fn stamp_gap(gap: &mut GapNote, project: &str) {
    gap.project = Some(project.to_string());
    gap.write_dir = None;
    if gap.updated_at.is_empty() {
        gap.updated_at = gap.created_at.clone();
    }
}

fn validate_filename_id(filename: &str, id: &str, label: &str) -> Result<()> {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .with_context(|| format!("gap filename is not UTF-8: {filename}"))?;
    if stem != id {
        anyhow::bail!("{label} filename/id mismatch: {filename} contains id {id}");
    }
    Ok(())
}

fn gaps_tree_dir(scope: &PublishedScope) -> String {
    if scope.bbox_root_relpath() == "." {
        ".bbox/gaps".to_string()
    } else {
        format!("{}/.bbox/gaps", scope.bbox_root_relpath())
    }
}

fn read_committed_map(
    root: &Path,
    commit: &str,
    tree_dir: &str,
    alternate_root: Option<&Path>,
) -> Result<BTreeMap<String, Vec<u8>>> {
    const MAX_TREE_ENTRIES: usize = 100_000;
    const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
    const MAX_TOTAL_BYTES: usize = 128 * 1024 * 1024;
    const MAX_LISTING_BYTES: usize = 32 * 1024 * 1024;

    let verified = verify_commit_from_repository_path(root, commit, alternate_root)
        .with_context(|| format!("verifying committed gap map at {commit}"))?;
    let prefix = format!("{tree_dir}/");
    let mut files = BTreeMap::new();
    let mut total_bytes = 0_usize;
    for repo_path in git::list_verified_committed_dir_bounded(
        &verified,
        tree_dir,
        MAX_TREE_ENTRIES,
        MAX_LISTING_BYTES,
    )? {
        let Some(filename) = repo_path.strip_prefix(&prefix) else {
            continue;
        };
        if filename.contains('/') || !filename.ends_with(".json") {
            continue;
        }
        validate_snapshot_filename(filename, "committed gap")?;
        let remaining = MAX_TOTAL_BYTES
            .checked_sub(total_bytes)
            .context("committed gap map exceeds its total byte limit")?;
        let bytes = git::read_verified_committed_file_bytes_bounded(
            &verified,
            &repo_path,
            MAX_FILE_BYTES.min(remaining),
        )
        .with_context(|| format!("reading bounded committed gap file {repo_path} at {commit}"))?;
        total_bytes = total_bytes
            .checked_add(bytes.len())
            .context("committed gap byte count overflowed")?;
        if total_bytes > MAX_TOTAL_BYTES {
            anyhow::bail!("committed gap map exceeds its total byte limit");
        }
        files.insert(filename.to_string(), bytes);
    }
    Ok(files)
}

fn verify_commit_from_repository_path(
    root: &Path,
    commit: &str,
    alternate_root: Option<&Path>,
) -> Result<git::VerifiedCommit> {
    let repository_root = git::git_root_for_path(root)
        .with_context(|| format!("resolving repository root for {}", root.display()))?;
    let alternate_repository_root = alternate_root
        .map(|alternate| {
            git::git_root_for_path(alternate).with_context(|| {
                format!(
                    "resolving alternate repository root for {}",
                    alternate.display()
                )
            })
        })
        .transpose()?;
    git::verify_commit_oid_with_alternate(
        &repository_root,
        commit,
        alternate_repository_root.as_deref(),
    )
}

fn validate_snapshot_filename(filename: &str, label: &str) -> Result<()> {
    let path = Path::new(filename);
    let mut components = path.components();
    let Some(std::path::Component::Normal(name)) = components.next() else {
        anyhow::bail!("{label} snapshot filename is not a confined basename: {filename}");
    };
    if components.next().is_some()
        || name.to_str() != Some(filename)
        || path.extension().and_then(|extension| extension.to_str()) != Some("json")
    {
        anyhow::bail!("{label} snapshot filename is not a confined JSON basename: {filename}");
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(hasher.finalize())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gaps::{BlockingLevel, GapImpact, GapKind, GapResolution};

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn gap(id: &str, title: &str) -> GapNote {
        GapNote {
            id: id.into(),
            title: title.into(),
            gap_kind: GapKind::Tooling,
            domain: "overlay-test".into(),
            wanted_capability: "preserve checkout-local gap state".into(),
            missing_primitive: None,
            fallback_used: None,
            evidence: Vec::new(),
            impact: GapImpact::Medium,
            blocking_level: BlockingLevel::WorkaroundAvailable,
            dedupe_key: format!("tooling/overlay-test/{id}"),
            suggested_owner: None,
            notes: None,
            supersedes: None,
            superseded_by: None,
            resolution: GapResolution::Unresolved,
            project: None,
            project_id: None,
            write_dir: None,
            task_id: None,
            session_id: None,
            provider: None,
            bro: None,
            thread_id: None,
            created_at: "2026-07-21T00:00:00Z".into(),
            updated_at: "2026-07-21T00:00:00Z".into(),
            resolved_at: None,
            resolution_note: None,
        }
    }

    #[test]
    fn committed_overlay_map_rejects_oversized_blobs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".bbox/gaps")).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        std::fs::write(
            root.join(".bbox/gaps/oversized.json"),
            vec![b'x'; 2 * 1024 * 1024 + 1],
        )
        .unwrap();
        git(&root, &["add", ".bbox/gaps"]);
        git(&root, &["commit", "-q", "-m", "oversized"]);
        let commit = git::current_head(&root).unwrap();

        let error = read_committed_map(&root, &commit, ".bbox/gaps", None).unwrap_err();
        assert!(error.to_string().contains("bounded committed gap"));
    }

    #[test]
    fn publication_source_loader_returns_exact_ordered_bytes_and_enforces_limits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        let first = serde_json::to_vec_pretty(&gap("gap-11111111", "first")).unwrap();
        let mut second = serde_json::to_vec(&gap("gap-22222222", "second")).unwrap();
        second.push(b'\n');
        let directory = root.join(".bbox/gaps");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("gap-22222222.json"), &second).unwrap();
        std::fs::write(directory.join("gap-11111111.json"), &first).unwrap();
        std::fs::write(directory.join(".lane-metadata"), b"metadata\n").unwrap();
        let inbox = directory.join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("gap-33333333.json"),
            serde_json::to_vec(&gap("gap-33333333", "inbox")).unwrap(),
        )
        .unwrap();
        git(&root, &["add", ".bbox/gaps"]);
        git(&root, &["commit", "-q", "-m", "seed"]);
        let commit = git::resolve_commit(&root, "HEAD").unwrap();
        let scope = PublishedScope::try_new("repo", ".").unwrap();

        let sources = load_published_gap_sources_at_commit(
            &root,
            &commit,
            &scope,
            None,
            PublishedGapSourceLimits::try_new(
                2,
                PublishedGapSourceLimits::MAX_FILE_BYTES,
                PublishedGapSourceLimits::MAX_TOTAL_BYTES,
                PublishedGapSourceLimits::MAX_LISTING_BYTES,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            sources
                .iter()
                .map(|source| source.repository_relative_filename.as_str())
                .collect::<Vec<_>>(),
            vec![
                ".bbox/gaps/gap-11111111.json",
                ".bbox/gaps/gap-22222222.json",
            ]
        );
        assert_eq!(sources[0].source_bytes, first);
        assert_eq!(sources[1].source_bytes, second);

        let defaults = PublishedGapSourceLimits::default();
        for limits in [
            PublishedGapSourceLimits::try_new(
                1,
                defaults.max_file_bytes(),
                defaults.max_total_bytes(),
                defaults.max_listing_bytes(),
            )
            .unwrap(),
            PublishedGapSourceLimits::try_new(
                defaults.max_entries(),
                sources[0].source_bytes.len() - 1,
                defaults.max_total_bytes(),
                defaults.max_listing_bytes(),
            )
            .unwrap(),
            PublishedGapSourceLimits::try_new(
                defaults.max_entries(),
                defaults.max_file_bytes(),
                sources
                    .iter()
                    .map(|source| source.source_bytes.len())
                    .sum::<usize>()
                    - 1,
                defaults.max_listing_bytes(),
            )
            .unwrap(),
            PublishedGapSourceLimits::try_new(
                defaults.max_entries(),
                defaults.max_file_bytes(),
                defaults.max_total_bytes(),
                1,
            )
            .unwrap(),
        ] {
            assert!(
                load_published_gap_sources_at_commit(&root, &commit, &scope, None, limits).is_err()
            );
        }
    }

    #[test]
    fn publication_source_limits_reject_zero_and_above_ceiling_values() {
        let defaults = PublishedGapSourceLimits::default();
        for values in [
            (
                0,
                defaults.max_file_bytes(),
                defaults.max_total_bytes(),
                defaults.max_listing_bytes(),
            ),
            (
                PublishedGapSourceLimits::MAX_ENTRIES + 1,
                defaults.max_file_bytes(),
                defaults.max_total_bytes(),
                defaults.max_listing_bytes(),
            ),
            (
                defaults.max_entries(),
                PublishedGapSourceLimits::MAX_FILE_BYTES + 1,
                defaults.max_total_bytes(),
                defaults.max_listing_bytes(),
            ),
            (
                defaults.max_entries(),
                defaults.max_file_bytes(),
                PublishedGapSourceLimits::MAX_TOTAL_BYTES + 1,
                defaults.max_listing_bytes(),
            ),
            (
                defaults.max_entries(),
                defaults.max_file_bytes(),
                defaults.max_total_bytes(),
                PublishedGapSourceLimits::MAX_LISTING_BYTES + 1,
            ),
        ] {
            assert!(
                PublishedGapSourceLimits::try_new(values.0, values.1, values.2, values.3).is_err()
            );
        }
    }

    #[test]
    fn publication_source_loader_ignores_nested_spool_members() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        let nested = root.join(".bbox/gaps/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("gap-11111111.json"),
            serde_json::to_vec(&gap("gap-11111111", "nested")).unwrap(),
        )
        .unwrap();
        git(&root, &["add", ".bbox/gaps"]);
        git(&root, &["commit", "-q", "-m", "nested"]);
        let commit = git::resolve_commit(&root, "HEAD").unwrap();

        assert_eq!(
            load_published_gap_sources_at_commit(
                &root,
                &commit,
                &PublishedScope::try_new("repo", ".").unwrap(),
                None,
                PublishedGapSourceLimits::default(),
            )
            .unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn publication_source_loader_rejects_invalid_top_level_json_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        let directory = root.join(".bbox/gaps");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(".lane-metadata"), b"metadata\n").unwrap();
        std::fs::write(directory.join("broken.json"), b"{").unwrap();
        git(&root, &["add", ".bbox/gaps"]);
        git(&root, &["commit", "-q", "-m", "broken"]);
        let commit = git::resolve_commit(&root, "HEAD").unwrap();

        assert!(
            load_published_gap_sources_at_commit(
                &root,
                &commit,
                &PublishedScope::try_new("repo", ".").unwrap(),
                None,
                PublishedGapSourceLimits::default(),
            )
            .is_err()
        );
    }
}
