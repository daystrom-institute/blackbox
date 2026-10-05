//! Published knowledge snapshots and the local working-tree overlay.
//!
//! Published snapshots come from committed trees: a publisher ref on the
//! bridge, or exact committed sources for an accepted-publication build.
//! Working-tree bytes are never part of a published snapshot.
//!
//! The local overlay is the one place uncommitted knowledge is read. A bound
//! harness diffs its own checkout's `.bbox/knowledge` working files against
//! the checkout's HEAD and applies the result to the published render plan
//! it executes. Nothing here uploads, stores, or serves that overlay.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use bbox_corpus_core::git;
use bbox_corpus_core::identity::PublishedScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::knowledge::{KnowledgeEntry, Scope, StoredKnowledgeEntry};

#[derive(Debug, Clone)]
pub struct PublishedKnowledgeEntry {
    pub entry: KnowledgeEntry,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub struct PublishedKnowledgeSnapshot {
    pub published_scope: PublishedScope,
    pub published_ref: String,
    pub publisher_commit: String,
    pub entries: BTreeMap<String, PublishedKnowledgeEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedKnowledgeSourceLimits {
    max_entries: usize,
    max_file_bytes: usize,
    max_total_bytes: usize,
    max_listing_bytes: usize,
}

impl PublishedKnowledgeSourceLimits {
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
        validate_source_limit(max_entries, Self::MAX_ENTRIES, "published knowledge entry")?;
        validate_source_limit(
            max_file_bytes,
            Self::MAX_FILE_BYTES,
            "published knowledge per-file byte",
        )?;
        validate_source_limit(
            max_total_bytes,
            Self::MAX_TOTAL_BYTES,
            "published knowledge total byte",
        )?;
        validate_source_limit(
            max_listing_bytes,
            Self::MAX_LISTING_BYTES,
            "published knowledge listing byte",
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

impl Default for PublishedKnowledgeSourceLimits {
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
pub struct PublishedKnowledgeSourceFile {
    pub repository_relative_filename: String,
    pub source_bytes: Vec<u8>,
}

/// Working-tree knowledge bytes of one checkout, keyed by basename.
#[derive(Debug, Clone, Default)]
pub struct WorkingKnowledgeSnapshot {
    files: BTreeMap<String, Vec<u8>>,
}

impl WorkingKnowledgeSnapshot {
    pub fn new(files: BTreeMap<String, Vec<u8>>) -> Result<Self> {
        for filename in files.keys() {
            validate_snapshot_filename(filename, "knowledge")?;
        }
        Ok(Self { files })
    }

    pub fn empty() -> Self {
        Self::default()
    }

    /// Read the JSON members of one project's `.bbox/knowledge` directory.
    /// Each member is opened without following a final symlink and read
    /// under the same per-file and total bounds as a committed map. A
    /// missing directory is an empty snapshot.
    pub fn read_project_dir(project_root: &Path) -> Result<Self> {
        let directory_path = project_root.join(".bbox/knowledge");
        let Some(directory) =
            bbox_corpus_core::json_store::NofollowDirectory::open_existing(&directory_path)?
        else {
            return Ok(Self::empty());
        };
        let mut names = Vec::new();
        #[allow(clippy::disallowed_methods)]
        // The harness reads its own bound checkout on a blocking task.
        let entries = std::fs::read_dir(&directory_path)
            .with_context(|| format!("listing {}", directory_path.display()))?;
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            if !entry.file_type()?.is_file() {
                continue;
            }
            names.push(name);
        }
        if names.len() > MAX_LOCAL_ENTRIES {
            anyhow::bail!("working knowledge exceeds its entry limit");
        }
        let mut files = BTreeMap::new();
        let mut total_bytes = 0_usize;
        for name in names {
            let remaining = MAX_LOCAL_TOTAL_BYTES
                .checked_sub(total_bytes)
                .context("working knowledge exceeds its total byte limit")?;
            let Some(bytes) = directory.read_regular(
                &name,
                MAX_LOCAL_FILE_BYTES.min(remaining),
                "working knowledge",
            )?
            else {
                continue;
            };
            total_bytes += bytes.len();
            files.insert(name, bytes);
        }
        Self::new(files)
    }
}

const MAX_LOCAL_ENTRIES: usize = 100_000;
const MAX_LOCAL_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_LOCAL_TOTAL_BYTES: usize = 128 * 1024 * 1024;

/// One knowledge id's local change relative to the checkout's HEAD.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlayValue {
    Upsert {
        entry: Box<KnowledgeEntry>,
        content_hash: String,
    },
    Tombstone,
}

/// Uncommitted knowledge of one checkout: every id whose working bytes
/// differ from the checkout's HEAD, keyed by entry id.
#[derive(Debug, Clone, Default)]
pub struct LocalKnowledgeOverlay {
    pub values: BTreeMap<String, OverlayValue>,
}

impl LocalKnowledgeOverlay {
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Content identity of the overlay: entry ids with their source-byte
    /// hashes or tombstones, in id order.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"bbox-local-knowledge-overlay-v1\0");
        for (entry_id, value) in &self.values {
            hasher.update((entry_id.len() as u64).to_be_bytes());
            hasher.update(entry_id.as_bytes());
            match value {
                OverlayValue::Upsert { content_hash, .. } => {
                    hasher.update([1]);
                    hasher.update((content_hash.len() as u64).to_be_bytes());
                    hasher.update(content_hash.as_bytes());
                }
                OverlayValue::Tombstone => hasher.update([2]),
            }
        }
        hex_digest(hasher.finalize())
    }

    pub fn upserts(&self) -> impl Iterator<Item = &KnowledgeEntry> {
        self.values.values().filter_map(|value| match value {
            OverlayValue::Upsert { entry, .. } => Some(entry.as_ref()),
            OverlayValue::Tombstone => None,
        })
    }

    pub fn tombstones(&self) -> impl Iterator<Item = &str> {
        self.values
            .iter()
            .filter(|(_, value)| matches!(value, OverlayValue::Tombstone))
            .map(|(id, _)| id.as_str())
    }
}

/// Diff one checkout's working knowledge against its own HEAD.
///
/// A checkout with no commit yet has an empty baseline. Working bytes equal
/// to HEAD are already committed and carry no overlay value; a changed or
/// new file is an upsert, and a file removed or retired since HEAD is a
/// tombstone of that id.
pub fn local_knowledge_overlay(
    checkout_root: &Path,
    scope: &PublishedScope,
    working: &WorkingKnowledgeSnapshot,
) -> Result<LocalKnowledgeOverlay> {
    let baseline = match git::current_head(checkout_root) {
        Some(head) => read_committed_map(checkout_root, &head, &knowledge_tree_dir(scope), None)?,
        None => BTreeMap::new(),
    };
    local_overlay_from_maps(&baseline, &working.files)
}

fn local_overlay_from_maps(
    baseline: &BTreeMap<String, Vec<u8>>,
    working: &BTreeMap<String, Vec<u8>>,
) -> Result<LocalKnowledgeOverlay> {
    validate_knowledge_map(working, "working")?;
    let mut values = BTreeMap::new();
    let paths = baseline
        .keys()
        .chain(working.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for filename in paths {
        match (baseline.get(&filename), working.get(&filename)) {
            (Some(before), Some(after)) if before == after => {}
            (_, Some(after)) => {
                let stored: StoredKnowledgeEntry = serde_json::from_slice(after)
                    .with_context(|| format!("parsing working knowledge file {filename}"))?;
                let id = stored.id().to_string();
                match stored.into_entry() {
                    Some(entry) => {
                        values.insert(
                            id,
                            OverlayValue::Upsert {
                                entry: Box::new(entry),
                                content_hash: sha256(after),
                            },
                        );
                    }
                    None => {
                        values.insert(id, OverlayValue::Tombstone);
                    }
                }
            }
            (Some(_), None) => {
                let id = Path::new(&filename)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .with_context(|| format!("knowledge filename is not UTF-8: {filename}"))?;
                values.insert(id.to_string(), OverlayValue::Tombstone);
            }
            (None, None) => {}
        }
    }
    Ok(LocalKnowledgeOverlay { values })
}

pub fn published_scope_hash(scope: &PublishedScope) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-published-scope-v1\0");
    hasher.update((scope.repo_id().len() as u64).to_be_bytes());
    hasher.update(scope.repo_id().as_bytes());
    hasher.update((scope.bbox_root_relpath().len() as u64).to_be_bytes());
    hasher.update(scope.bbox_root_relpath().as_bytes());
    hex_digest(hasher.finalize())
}

/// Load published knowledge only from the committed tree selected by the
/// pinned ref. Working-tree bytes are never consulted.
pub fn load_published_snapshot(
    publisher_root: &Path,
    published_ref: &str,
    scope: &PublishedScope,
    durable_project: &str,
) -> Result<PublishedKnowledgeSnapshot> {
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

/// Load a published snapshot at an already-resolved commit. Callers that
/// cache symbolic-ref resolution use this to avoid rereading every blob when
/// the ref still names the same commit.
pub fn load_published_snapshot_at_commit(
    publisher_root: &Path,
    published_ref: &str,
    publisher_commit: &str,
    scope: &PublishedScope,
    durable_project: &str,
) -> Result<PublishedKnowledgeSnapshot> {
    let mut snapshot = load_published_snapshot_at_commit_unhydrated(
        publisher_root,
        published_ref,
        publisher_commit,
        scope,
        durable_project,
    )?;
    crate::knowledge::hydrate_repo_recall_stats(
        publisher_root,
        snapshot
            .entries
            .values_mut()
            .map(|published| &mut published.entry),
    );
    Ok(snapshot)
}

/// Load immutable committed blobs without merging host-local recall telemetry.
/// Commit-keyed caches store this form and hydrate each returned clone so
/// ranking observes the latest sidecar without rereading Git objects.
pub fn load_published_snapshot_at_commit_unhydrated(
    publisher_root: &Path,
    published_ref: &str,
    publisher_commit: &str,
    scope: &PublishedScope,
    durable_project: &str,
) -> Result<PublishedKnowledgeSnapshot> {
    let tree_dir = knowledge_tree_dir(scope);
    let files = read_committed_map(publisher_root, publisher_commit, &tree_dir, None)?;
    let mut entries = BTreeMap::new();
    for (filename, bytes) in files {
        let stored: StoredKnowledgeEntry = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing published knowledge file {filename}"))?;
        let stem = Path::new(&filename)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("knowledge filename is not UTF-8: {filename}"))?;
        if stem != stored.id() {
            anyhow::bail!(
                "published knowledge filename/id mismatch: {filename} contains id {}",
                stored.id()
            );
        }
        let Some(mut entry) = stored.into_entry() else {
            continue;
        };
        // Repository-owned knowledge is always project-scoped. Committed
        // bytes are untrusted input and may not promote themselves into the
        // operator's global rendered memory or assert catalog identity.
        entry.scope = Scope::Project;
        entry.project = Some(durable_project.to_string());
        entry.project_id = None;
        let id = entry.id.clone();
        if entries
            .insert(
                id.clone(),
                PublishedKnowledgeEntry {
                    entry,
                    content_hash: sha256(&bytes),
                },
            )
            .is_some()
        {
            anyhow::bail!("duplicate published knowledge id: {id}");
        }
    }
    Ok(PublishedKnowledgeSnapshot {
        published_scope: scope.clone(),
        published_ref: published_ref.to_string(),
        publisher_commit: publisher_commit.to_string(),
        entries,
    })
}

/// Load exact committed knowledge JSON for an accepted-publication build.
///
/// This path does not hydrate recall telemetry or normalize records. It
/// validates the committed lane and returns byte-exact, deterministically
/// ordered source files for the transaction-owned publication builder.
pub fn load_published_knowledge_sources_at_commit(
    publisher_root: &Path,
    publisher_commit: &str,
    scope: &PublishedScope,
    alternate_root: Option<&Path>,
    limits: PublishedKnowledgeSourceLimits,
) -> Result<Vec<PublishedKnowledgeSourceFile>> {
    const MAX_TREE_ENTRIES: usize = 200_000;

    scope
        .validate()
        .context("invalid published knowledge scope")?;
    let verified_commit =
        verify_commit_from_repository_path(publisher_root, publisher_commit, alternate_root)
            .with_context(|| {
                format!(
                    "verifying exact published knowledge commit in {}",
                    publisher_root.display()
                )
            })?;
    let tree_dir = knowledge_tree_dir(scope);
    let prefix = format!("{tree_dir}/");
    let repo_paths = git::list_verified_committed_dir_bounded(
        &verified_commit,
        &tree_dir,
        MAX_TREE_ENTRIES,
        limits.max_listing_bytes,
    )
    .with_context(|| {
        format!(
            "listing bounded committed knowledge at {publisher_commit} in {}",
            publisher_root.display()
        )
    })?;

    let mut total_bytes = 0_usize;
    let mut entry_count = 0_usize;
    let mut ids = BTreeSet::new();
    let mut sources = Vec::with_capacity(repo_paths.len().min(limits.max_entries));
    for repo_path in repo_paths {
        let filename = repo_path.strip_prefix(&prefix).ok_or_else(|| {
            anyhow::anyhow!("committed knowledge path is outside its published scope")
        })?;
        if !filename.ends_with(".json") {
            continue;
        }
        entry_count = entry_count
            .checked_add(1)
            .context("published knowledge entry count overflowed")?;
        if entry_count > limits.max_entries {
            anyhow::bail!("published knowledge sources exceed their entry limit");
        }
        validate_snapshot_filename(filename, "published knowledge")?;
        let remaining = limits
            .max_total_bytes
            .checked_sub(total_bytes)
            .ok_or_else(|| {
                anyhow::anyhow!("published knowledge sources exceed their total byte limit")
            })?;
        let read_limit = limits.max_file_bytes.min(remaining);
        let source_bytes = git::read_verified_committed_file_bytes_bounded(
            &verified_commit,
            &repo_path,
            read_limit,
        )
        .with_context(|| {
            format!("reading bounded committed knowledge file {repo_path} at {publisher_commit}")
        })?;
        total_bytes = total_bytes.checked_add(source_bytes.len()).ok_or_else(|| {
            anyhow::anyhow!("published knowledge source total byte count overflowed")
        })?;
        if total_bytes > limits.max_total_bytes {
            anyhow::bail!("published knowledge sources exceed their total byte limit");
        }
        let stored: StoredKnowledgeEntry = serde_json::from_slice(&source_bytes)
            .with_context(|| format!("parsing published knowledge source {repo_path}"))?;
        let stem = Path::new(filename)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("published knowledge filename is not UTF-8")?;
        if stem != stored.id() {
            anyhow::bail!("published knowledge filename and record id disagree");
        }
        if !ids.insert(stored.id().to_string()) {
            anyhow::bail!("published knowledge sources contain a duplicate record id");
        }
        sources.push(PublishedKnowledgeSourceFile {
            repository_relative_filename: repo_path,
            source_bytes,
        });
    }
    Ok(sources)
}

fn validate_knowledge_map(files: &BTreeMap<String, Vec<u8>>, label: &str) -> Result<()> {
    let mut ids = BTreeSet::new();
    for (filename, bytes) in files {
        let stored: StoredKnowledgeEntry = serde_json::from_slice(bytes)
            .with_context(|| format!("parsing {label} knowledge file {filename}"))?;
        let stem = Path::new(filename)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("knowledge filename is not UTF-8: {filename}"))?;
        if stem != stored.id() {
            anyhow::bail!(
                "{label} knowledge filename/id mismatch: {filename} contains id {}",
                stored.id()
            );
        }
        if !ids.insert(stored.id().to_string()) {
            anyhow::bail!("duplicate {label} knowledge id: {}", stored.id());
        }
    }
    Ok(())
}

fn knowledge_tree_dir(scope: &PublishedScope) -> String {
    if scope.bbox_root_relpath() == "." {
        ".bbox/knowledge".to_string()
    } else {
        format!("{}/.bbox/knowledge", scope.bbox_root_relpath())
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
        .with_context(|| format!("verifying committed knowledge map at {commit}"))?;
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
        validate_snapshot_filename(filename, "committed knowledge")?;
        let remaining = MAX_TOTAL_BYTES
            .checked_sub(total_bytes)
            .context("committed knowledge map exceeds its total byte limit")?;
        let bytes = git::read_verified_committed_file_bytes_bounded(
            &verified,
            &repo_path,
            MAX_FILE_BYTES.min(remaining),
        )
        .with_context(|| {
            format!(
                "reading bounded committed knowledge file {repo_path} at {commit} in {}",
                root.display()
            )
        })?;
        total_bytes = total_bytes
            .checked_add(bytes.len())
            .context("committed knowledge byte count overflowed")?;
        if total_bytes > MAX_TOTAL_BYTES {
            anyhow::bail!("committed knowledge map exceeds its total byte limit");
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
    use crate::knowledge::{Category, Priority, Scope};

    fn run(root: &Path, args: &[&str]) {
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
    }

    fn entry(id: &str, content: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            render_placement: Default::default(),
            id: id.into(),
            title: id.into(),
            content: content.into(),
            cluster: None,
            category: Category::Memory,
            scope: Scope::Project,
            project: None,
            project_id: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            render: false,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn write_entry(root: &Path, entry: &KnowledgeEntry) {
        let dir = root.join(".bbox/knowledge");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.json", entry.id)),
            serde_json::to_vec_pretty(entry).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn published_repo_entries_cannot_assert_global_or_catalog_scope() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "-q", "-b", "main"]);
        run(&root, &["config", "user.email", "t@example.com"]);
        run(&root, &["config", "user.name", "Test"]);
        let mut hostile = entry("hostile", "repo bytes");
        hostile.scope = Scope::Global;
        hostile.project_id = Some("forged-project".into());
        write_entry(&root, &hostile);
        run(&root, &["add", ".bbox/knowledge"]);
        run(&root, &["commit", "-q", "-m", "hostile knowledge"]);
        let commit = git::current_head(&root).unwrap();
        let scope = PublishedScope::try_new("repo", ".").unwrap();

        let snapshot = load_published_snapshot_at_commit_unhydrated(
            &root,
            "refs/heads/main",
            &commit,
            &scope,
            "/durable/project",
        )
        .unwrap();
        let loaded = &snapshot.entries["hostile"].entry;
        assert_eq!(loaded.scope, Scope::Project);
        assert_eq!(loaded.project.as_deref(), Some("/durable/project"));
        assert_eq!(loaded.project_id, None);
    }

    fn committed_repo(temp: &tempfile::TempDir) -> std::path::PathBuf {
        let root = temp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "-q", "-b", "main"]);
        run(&root, &["config", "user.email", "t@example.com"]);
        run(&root, &["config", "user.name", "Test"]);
        root
    }

    #[test]
    fn local_overlay_captures_uncommitted_upserts_and_tombstones() {
        let temp = tempfile::tempdir().unwrap();
        let root = committed_repo(&temp);
        write_entry(&root, &entry("keep", "committed"));
        write_entry(&root, &entry("changed", "old"));
        write_entry(&root, &entry("removed", "gone soon"));
        run(&root, &["add", ".bbox/knowledge"]);
        run(&root, &["commit", "-q", "-m", "seed"]);
        write_entry(&root, &entry("changed", "new"));
        write_entry(&root, &entry("added", "untracked"));
        std::fs::remove_file(root.join(".bbox/knowledge/removed.json")).unwrap();

        let scope = PublishedScope::try_new("repo", ".").unwrap();
        let working = WorkingKnowledgeSnapshot::read_project_dir(&root).unwrap();
        let overlay = local_knowledge_overlay(&root, &scope, &working).unwrap();
        assert!(!overlay.values.contains_key("keep"), "{overlay:?}");
        assert!(matches!(
            overlay.values.get("changed"),
            Some(OverlayValue::Upsert { entry, .. }) if entry.content == "new"
        ));
        assert!(matches!(
            overlay.values.get("added"),
            Some(OverlayValue::Upsert { .. })
        ));
        assert!(matches!(
            overlay.values.get("removed"),
            Some(OverlayValue::Tombstone)
        ));
        assert_eq!(overlay.digest().len(), 64);
        assert_eq!(overlay.tombstones().collect::<Vec<_>>(), vec!["removed"]);
    }

    #[test]
    fn local_overlay_is_empty_for_a_clean_or_absent_knowledge_lane() {
        let temp = tempfile::tempdir().unwrap();
        let root = committed_repo(&temp);
        let scope = PublishedScope::try_new("repo", ".").unwrap();
        // Never provisioned: no commit and no knowledge directory.
        let working = WorkingKnowledgeSnapshot::read_project_dir(&root).unwrap();
        assert!(
            local_knowledge_overlay(&root, &scope, &working)
                .unwrap()
                .is_empty()
        );
        write_entry(&root, &entry("clean", "committed"));
        std::fs::write(root.join(".bbox/knowledge/.schema-epoch"), b"{}").unwrap();
        run(&root, &["add", ".bbox/knowledge"]);
        run(&root, &["commit", "-q", "-m", "seed"]);
        let working = WorkingKnowledgeSnapshot::read_project_dir(&root).unwrap();
        assert!(
            local_knowledge_overlay(&root, &scope, &working)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn local_overlay_reads_a_nested_scope_and_rejects_invalid_working_records() {
        let temp = tempfile::tempdir().unwrap();
        let root = committed_repo(&temp);
        let project = root.join("svc");
        write_entry(&project, &entry("nested", "committed"));
        run(&root, &["add", "svc/.bbox/knowledge"]);
        run(&root, &["commit", "-q", "-m", "seed"]);
        let scope = PublishedScope::try_new("repo", "svc").unwrap();
        let working = WorkingKnowledgeSnapshot::read_project_dir(&project).unwrap();
        assert!(
            local_knowledge_overlay(&root, &scope, &working)
                .unwrap()
                .is_empty()
        );

        std::fs::write(project.join(".bbox/knowledge/broken.json"), b"not json").unwrap();
        let working = WorkingKnowledgeSnapshot::read_project_dir(&project).unwrap();
        assert!(local_knowledge_overlay(&root, &scope, &working).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn working_snapshot_skips_symlinked_members() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write_entry(&root, &entry("real", "content"));
        std::fs::write(root.join("outside.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(
            root.join("outside.json"),
            root.join(".bbox/knowledge/linked.json"),
        )
        .unwrap();
        let working = WorkingKnowledgeSnapshot::read_project_dir(&root).unwrap();
        assert_eq!(working.files.keys().collect::<Vec<_>>(), vec!["real.json"]);
    }

    #[test]
    fn working_snapshot_rejects_non_basename_paths() {
        for filename in ["../escape.json", "nested/entry.json", "entry.txt"] {
            assert!(
                WorkingKnowledgeSnapshot::new(BTreeMap::from([(
                    filename.to_string(),
                    b"{}".to_vec(),
                )]))
                .is_err(),
                "unsafe snapshot filename should be rejected: {filename}"
            );
        }
    }

    #[test]
    fn committed_overlay_map_rejects_oversized_blobs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        run(&root, &["init", "-q", "-b", "main"]);
        run(&root, &["config", "user.email", "t@example.com"]);
        run(&root, &["config", "user.name", "Test"]);
        std::fs::write(
            root.join(".bbox/knowledge/oversized.json"),
            vec![b'x'; 2 * 1024 * 1024 + 1],
        )
        .unwrap();
        run(&root, &["add", ".bbox/knowledge"]);
        run(&root, &["commit", "-q", "-m", "oversized"]);
        let commit = git::current_head(&root).unwrap();

        let error = read_committed_map(&root, &commit, ".bbox/knowledge", None).unwrap_err();
        assert!(error.to_string().contains("bounded committed knowledge"));
    }

    #[test]
    fn publication_source_loader_returns_exact_ordered_bytes_and_enforces_limits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        run(&root, &["init", "-q", "-b", "main"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        run(&root, &["config", "user.name", "Test"]);
        let first = serde_json::to_vec_pretty(&entry("a", "first")).unwrap();
        let mut second = serde_json::to_vec(&entry("z", "second")).unwrap();
        second.push(b'\n');
        let directory = root.join(".bbox/knowledge");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("z.json"), &second).unwrap();
        std::fs::write(directory.join("a.json"), &first).unwrap();
        std::fs::write(directory.join(".schema-epoch"), b"{\"schema_epoch\":1}\n").unwrap();
        run(&root, &["add", ".bbox/knowledge"]);
        run(&root, &["commit", "-q", "-m", "seed"]);
        let commit = git::resolve_commit(&root, "HEAD").unwrap();
        let scope = PublishedScope::try_new("repo", ".").unwrap();

        let sources = load_published_knowledge_sources_at_commit(
            &root,
            &commit,
            &scope,
            None,
            PublishedKnowledgeSourceLimits::try_new(
                2,
                PublishedKnowledgeSourceLimits::MAX_FILE_BYTES,
                PublishedKnowledgeSourceLimits::MAX_TOTAL_BYTES,
                PublishedKnowledgeSourceLimits::MAX_LISTING_BYTES,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            sources
                .iter()
                .map(|source| source.repository_relative_filename.as_str())
                .collect::<Vec<_>>(),
            vec![".bbox/knowledge/a.json", ".bbox/knowledge/z.json",]
        );
        assert_eq!(sources[0].source_bytes, first);
        assert_eq!(sources[1].source_bytes, second);

        let defaults = PublishedKnowledgeSourceLimits::default();
        for limits in [
            PublishedKnowledgeSourceLimits::try_new(
                1,
                defaults.max_file_bytes(),
                defaults.max_total_bytes(),
                defaults.max_listing_bytes(),
            )
            .unwrap(),
            PublishedKnowledgeSourceLimits::try_new(
                defaults.max_entries(),
                sources[0].source_bytes.len() - 1,
                defaults.max_total_bytes(),
                defaults.max_listing_bytes(),
            )
            .unwrap(),
            PublishedKnowledgeSourceLimits::try_new(
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
            PublishedKnowledgeSourceLimits::try_new(
                defaults.max_entries(),
                defaults.max_file_bytes(),
                defaults.max_total_bytes(),
                1,
            )
            .unwrap(),
        ] {
            assert!(
                load_published_knowledge_sources_at_commit(&root, &commit, &scope, None, limits)
                    .is_err()
            );
        }
    }

    #[test]
    fn publication_source_limits_reject_zero_and_above_ceiling_values() {
        let defaults = PublishedKnowledgeSourceLimits::default();
        for values in [
            (
                0,
                defaults.max_file_bytes(),
                defaults.max_total_bytes(),
                defaults.max_listing_bytes(),
            ),
            (
                PublishedKnowledgeSourceLimits::MAX_ENTRIES + 1,
                defaults.max_file_bytes(),
                defaults.max_total_bytes(),
                defaults.max_listing_bytes(),
            ),
            (
                defaults.max_entries(),
                PublishedKnowledgeSourceLimits::MAX_FILE_BYTES + 1,
                defaults.max_total_bytes(),
                defaults.max_listing_bytes(),
            ),
            (
                defaults.max_entries(),
                defaults.max_file_bytes(),
                PublishedKnowledgeSourceLimits::MAX_TOTAL_BYTES + 1,
                defaults.max_listing_bytes(),
            ),
            (
                defaults.max_entries(),
                defaults.max_file_bytes(),
                defaults.max_total_bytes(),
                PublishedKnowledgeSourceLimits::MAX_LISTING_BYTES + 1,
            ),
        ] {
            assert!(
                PublishedKnowledgeSourceLimits::try_new(values.0, values.1, values.2, values.3)
                    .is_err()
            );
        }
    }

    #[test]
    fn publication_source_loader_rejects_non_flat_lane_members() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        run(&root, &["init", "-q", "-b", "main"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        run(&root, &["config", "user.name", "Test"]);
        let nested = root.join(".bbox/knowledge/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("entry.json"),
            serde_json::to_vec(&entry("entry", "nested")).unwrap(),
        )
        .unwrap();
        run(&root, &["add", ".bbox/knowledge"]);
        run(&root, &["commit", "-q", "-m", "nested"]);
        let commit = git::resolve_commit(&root, "HEAD").unwrap();

        assert!(
            load_published_knowledge_sources_at_commit(
                &root,
                &commit,
                &PublishedScope::try_new("repo", ".").unwrap(),
                None,
                PublishedKnowledgeSourceLimits::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn publication_source_loader_rejects_invalid_top_level_json_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        run(&root, &["init", "-q", "-b", "main"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        run(&root, &["config", "user.name", "Test"]);
        let directory = root.join(".bbox/knowledge");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(".schema-epoch"), b"metadata\n").unwrap();
        std::fs::write(directory.join("broken.json"), b"{").unwrap();
        run(&root, &["add", ".bbox/knowledge"]);
        run(&root, &["commit", "-q", "-m", "broken"]);
        let commit = git::resolve_commit(&root, "HEAD").unwrap();

        assert!(
            load_published_knowledge_sources_at_commit(
                &root,
                &commit,
                &PublishedScope::try_new("repo", ".").unwrap(),
                None,
                PublishedKnowledgeSourceLimits::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn publication_source_loader_supports_alternate_commit_objects() {
        let publisher_temp = tempfile::tempdir().unwrap();
        let publisher = publisher_temp.path().canonicalize().unwrap();
        run(&publisher, &["init", "-q", "-b", "main"]);
        run(&publisher, &["config", "user.email", "test@example.com"]);
        run(&publisher, &["config", "user.name", "Test"]);
        write_entry(&publisher, &entry("first", "one"));
        run(&publisher, &["add", ".bbox/knowledge"]);
        run(&publisher, &["commit", "-q", "-m", "first"]);

        let clone_temp = tempfile::tempdir().unwrap();
        let checkout = clone_temp.path().join("checkout");
        let output = std::process::Command::new("git")
            .args([
                "clone",
                "--no-local",
                "-q",
                publisher.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let checkout = checkout.canonicalize().unwrap();

        write_entry(&publisher, &entry("second", "two"));
        run(&publisher, &["add", ".bbox/knowledge"]);
        run(&publisher, &["commit", "-q", "-m", "second"]);
        let commit = git::resolve_commit(&publisher, "HEAD").unwrap();
        let sources = load_published_knowledge_sources_at_commit(
            &checkout,
            &commit,
            &PublishedScope::try_new("repo", ".").unwrap(),
            Some(&publisher),
            PublishedKnowledgeSourceLimits::default(),
        )
        .unwrap();

        assert_eq!(sources.len(), 2);
        assert_eq!(
            sources[1].repository_relative_filename,
            ".bbox/knowledge/second.json"
        );
    }
}
