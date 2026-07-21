//! Dark provisional knowledge overlays.
//!
//! This module computes immutable checkout snapshots without merging them into
//! the live knowledge store, index, render, graph, or inbox. That separation is
//! the slice-3.3 behavior boundary: diagnostics become available while current
//! retrieval remains unchanged until the visibility contract lands.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use bbox_corpus_core::git;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_record::ResolvedCheckoutScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::knowledge::KnowledgeEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisionalMode {
    Published,
    Own,
    All,
}

impl ProvisionalMode {
    pub fn parse(raw: Option<&str>, has_session_checkout: bool) -> Result<Self> {
        match raw {
            None if has_session_checkout => Ok(Self::Own),
            None => Ok(Self::Published),
            Some("published") => Ok(Self::Published),
            Some("own") if has_session_checkout => Ok(Self::Own),
            Some("own") => Ok(Self::Published),
            Some("all") => Ok(Self::All),
            Some(other) => {
                anyhow::bail!("invalid provisional mode {other:?}; expected published, own, or all")
            }
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OverlayKey {
    pub published_scope: PublishedScope,
    pub checkout_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayStamp {
    pub published_scope: PublishedScope,
    pub checkout_id: String,
    pub published_ref: String,
    pub publisher_commit: String,
    pub checkout_head: String,
    pub merge_base: String,
    pub working_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlayValue {
    Upsert {
        entry: Box<KnowledgeEntry>,
        content_hash: String,
    },
    Tombstone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayStatus {
    Valid,
    Invalid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlaySnapshot {
    pub snapshot_id: String,
    pub key: OverlayKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stamp: Option<OverlayStamp>,
    pub status: OverlayStatus,
    #[serde(default)]
    pub values: BTreeMap<String, OverlayValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

impl OverlaySnapshot {
    pub fn invalid(checkout: &ResolvedCheckoutScope, diagnostic: impl Into<String>) -> Self {
        Self {
            snapshot_id: String::new(),
            key: OverlayKey {
                published_scope: checkout.published_scope.clone(),
                checkout_id: checkout.checkout_id.clone(),
            },
            stamp: None,
            status: OverlayStatus::Invalid,
            values: BTreeMap::new(),
            diagnostics: vec![diagnostic.into()],
        }
    }
}

#[derive(Debug, Default)]
pub struct KnowledgeOverlayStore {
    snapshots: BTreeMap<OverlayKey, OverlaySnapshot>,
}

impl KnowledgeOverlayStore {
    /// Replace the complete snapshot for one checkout scope. Invalid snapshots
    /// replace prior valid state instead of leaving stale values visible.
    pub fn publish(&mut self, snapshot: OverlaySnapshot) {
        self.snapshots.insert(snapshot.key.clone(), snapshot);
    }

    pub fn get(
        &self,
        published_scope: &PublishedScope,
        checkout_id: &str,
    ) -> Option<&OverlaySnapshot> {
        self.snapshots.get(&OverlayKey {
            published_scope: published_scope.clone(),
            checkout_id: checkout_id.to_string(),
        })
    }

    pub fn snapshots(&self) -> impl Iterator<Item = &OverlaySnapshot> {
        self.snapshots.values()
    }
}

pub fn published_scope_hash(scope: &PublishedScope) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-published-scope-v1\0");
    hasher.update((scope.repo_id.len() as u64).to_be_bytes());
    hasher.update(scope.repo_id.as_bytes());
    hasher.update((scope.bbox_root_relpath.len() as u64).to_be_bytes());
    hasher.update(scope.bbox_root_relpath.as_bytes());
    hex_digest(hasher.finalize())
}

pub fn provisional_entity_ref(scope: &PublishedScope, checkout_id: &str, entry_id: &str) -> String {
    bbox_corpus_core::entity_ref::EntityRef::ProvisionalKnowledge {
        scope_hash: published_scope_hash(scope),
        checkout_id: checkout_id.to_string(),
        entry_id: entry_id.to_string(),
    }
    .to_string()
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
    let tree_dir = knowledge_tree_dir(scope);
    let files = read_committed_map(publisher_root, &publisher_commit, &tree_dir)?;
    let mut entries = BTreeMap::new();
    for (filename, bytes) in files {
        let mut entry: KnowledgeEntry = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing published knowledge file {filename}"))?;
        let stem = Path::new(&filename)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("knowledge filename is not UTF-8: {filename}"))?;
        if stem != entry.id {
            anyhow::bail!(
                "published knowledge filename/id mismatch: {filename} contains id {}",
                entry.id
            );
        }
        entry.project = Some(durable_project.to_string());
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
        publisher_commit,
        entries,
    })
}

/// Recompute one checkout overlay. Every failure becomes an invalid empty
/// snapshot so callers can publish it atomically and discard stale prior state.
pub fn recompute_overlay(
    publisher_root: &Path,
    published_ref: &str,
    checkout: &ResolvedCheckoutScope,
) -> OverlaySnapshot {
    match recompute_overlay_inner(publisher_root, published_ref, checkout) {
        Ok(snapshot) => snapshot,
        Err(err) => OverlaySnapshot::invalid(checkout, format!("{err:#}")),
    }
}

fn recompute_overlay_inner(
    publisher_root: &Path,
    published_ref: &str,
    checkout: &ResolvedCheckoutScope,
) -> Result<OverlaySnapshot> {
    let checkout_root = Path::new(&checkout.checkout_dir);
    let publisher_commit =
        git::resolve_commit(publisher_root, published_ref).with_context(|| {
            format!(
                "published ref {published_ref} does not resolve in {}",
                publisher_root.display()
            )
        })?;
    let checkout_head = git::current_head(checkout_root)
        .with_context(|| format!("checkout {} has no HEAD", checkout_root.display()))?;
    let merge_base = git::merge_base(checkout_root, &checkout_head, &publisher_commit)
        .with_context(|| {
            format!(
                "no merge base between checkout {} and published commit {}",
                checkout_root.display(),
                publisher_commit
            )
        })?;
    let tree_dir = knowledge_tree_dir(&checkout.published_scope);
    let baseline = read_committed_map(checkout_root, &merge_base, &tree_dir)?;
    let published = read_committed_map(publisher_root, &publisher_commit, &tree_dir)?;
    let working = read_working_map(Path::new(&checkout.checkout_project_dir))?;
    let working_fingerprint = fingerprint_map(&working);

    let mut paths = BTreeSet::new();
    paths.extend(baseline.keys().cloned());
    paths.extend(working.keys().cloned());
    let mut values = BTreeMap::new();
    let mut seen_ids = BTreeSet::new();
    for filename in paths {
        match (baseline.get(&filename), working.get(&filename)) {
            (Some(before), Some(after)) if before == after => {}
            (_, Some(after)) => {
                // Equality at the pinned published ref is already integrated;
                // suppress the provisional variant even for a cherry-pick or
                // content-equivalent merge with different ancestry.
                if published.get(&filename) == Some(after) {
                    continue;
                }
                let entry: KnowledgeEntry = serde_json::from_slice(after)
                    .with_context(|| format!("parsing working knowledge file {filename}"))?;
                let stem = Path::new(&filename)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .with_context(|| format!("knowledge filename is not UTF-8: {filename}"))?;
                if stem != entry.id {
                    anyhow::bail!(
                        "knowledge filename/id mismatch: {filename} contains id {}",
                        entry.id
                    );
                }
                if !seen_ids.insert(entry.id.clone()) {
                    anyhow::bail!("duplicate knowledge id in checkout overlay: {}", entry.id);
                }
                values.insert(
                    entry.id.clone(),
                    OverlayValue::Upsert {
                        entry: Box::new(entry),
                        content_hash: sha256(after),
                    },
                );
            }
            (Some(before), None) => {
                let stem = Path::new(&filename)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .with_context(|| format!("knowledge filename is not UTF-8: {filename}"))?;
                let entry: KnowledgeEntry = serde_json::from_slice(before)
                    .with_context(|| format!("parsing baseline knowledge file {filename}"))?;
                if stem != entry.id {
                    anyhow::bail!(
                        "baseline knowledge filename/id mismatch: {filename} contains id {}",
                        entry.id
                    );
                }
                if published.contains_key(&filename) {
                    values.insert(entry.id, OverlayValue::Tombstone);
                }
            }
            (None, None) => {}
        }
    }

    let stamp = OverlayStamp {
        published_scope: checkout.published_scope.clone(),
        checkout_id: checkout.checkout_id.clone(),
        published_ref: published_ref.to_string(),
        publisher_commit,
        checkout_head,
        merge_base,
        working_fingerprint,
    };
    let snapshot_id = snapshot_id(&stamp, &values)?;
    Ok(OverlaySnapshot {
        snapshot_id,
        key: OverlayKey {
            published_scope: checkout.published_scope.clone(),
            checkout_id: checkout.checkout_id.clone(),
        },
        stamp: Some(stamp),
        status: OverlayStatus::Valid,
        values,
        diagnostics: Vec::new(),
    })
}

fn knowledge_tree_dir(scope: &PublishedScope) -> String {
    if scope.bbox_root_relpath == "." {
        ".bbox/knowledge".to_string()
    } else {
        format!("{}/.bbox/knowledge", scope.bbox_root_relpath)
    }
}

fn read_committed_map(
    root: &Path,
    commit: &str,
    tree_dir: &str,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let prefix = format!("{tree_dir}/");
    let mut files = BTreeMap::new();
    for repo_path in git::list_committed_dir_result(root, commit, tree_dir)? {
        let Some(filename) = repo_path.strip_prefix(&prefix) else {
            continue;
        };
        if filename.contains('/') || !filename.ends_with(".json") {
            continue;
        }
        let bytes =
            git::read_committed_file_bytes(root, commit, &repo_path).with_context(|| {
                format!(
                    "reading committed knowledge file {repo_path} at {commit} in {}",
                    root.display()
                )
            })?;
        files.insert(filename.to_string(), bytes);
    }
    Ok(files)
}

fn read_working_map(project_root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let dir = project_root.join(".bbox").join("knowledge");
    if !dir.exists() {
        return Ok(BTreeMap::new());
    }
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading entry in {}", dir.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("reading file type for {}", entry.path().display()))?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("knowledge filename is not UTF-8: {}", path.display()))?;
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        files.insert(filename.to_string(), bytes);
    }
    Ok(files)
}

fn fingerprint_map(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hasher = Sha256::new();
    for (path, bytes) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        hasher.update([0xff]);
    }
    hex_digest(hasher.finalize())
}

fn snapshot_id(stamp: &OverlayStamp, values: &BTreeMap<String, OverlayValue>) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-knowledge-overlay-v1\0");
    hasher.update(serde_json::to_vec(stamp)?);
    // KnowledgeEntry contains maps whose serde iteration order is not stable
    // across processes. Hash the byte-derived content hash instead of the
    // parsed entry so an identical snapshot has one deterministic identity.
    for (entry_id, value) in values {
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
    Ok(hex_digest(hasher.finalize()))
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
    use crate::knowledge::{Approval, Category, Priority, Scope, Status};
    use std::collections::HashMap;

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
            id: id.into(),
            title: id.into(),
            content: content.into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: false,
            decay: false,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
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
    fn overlay_captures_modifications_untracked_files_and_tombstones() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run(&base, &["init", "-q", "-b", "main"]);
        run(&base, &["config", "user.email", "t@example.com"]);
        run(&base, &["config", "user.name", "Test"]);
        write_entry(&base, &entry("keep", "old"));
        write_entry(&base, &entry("remove", "gone"));
        run(&base, &["add", ".bbox/knowledge"]);
        run(&base, &["commit", "-q", "-m", "seed"]);

        let worktree = temp.path().join("worktree");
        run(
            &base,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        write_entry(&worktree, &entry("keep", "changed"));
        write_entry(&worktree, &entry("new", "untracked"));
        std::fs::remove_file(worktree.join(".bbox/knowledge/remove.json")).unwrap();

        let scope = PublishedScope {
            repo_id: "repo".into(),
            bbox_root_relpath: ".".into(),
        };
        let checkout = ResolvedCheckoutScope {
            published_scope: scope,
            checkout_id: "checkout".into(),
            checkout_dir: worktree.to_string_lossy().into_owned(),
            checkout_project_dir: worktree.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/feature".into()),
        };
        let snapshot = recompute_overlay(&base, "refs/heads/main", &checkout);
        assert_eq!(snapshot.status, OverlayStatus::Valid, "{snapshot:?}");
        assert!(matches!(
            snapshot.values.get("keep"),
            Some(OverlayValue::Upsert { .. })
        ));
        assert!(matches!(
            snapshot.values.get("new"),
            Some(OverlayValue::Upsert { .. })
        ));
        assert!(matches!(
            snapshot.values.get("remove"),
            Some(OverlayValue::Tombstone)
        ));
        assert_eq!(snapshot.snapshot_id.len(), 64);
    }

    #[test]
    fn overlay_suppresses_content_already_at_published_ref() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run(&base, &["init", "-q", "-b", "main"]);
        run(&base, &["config", "user.email", "t@example.com"]);
        run(&base, &["config", "user.name", "Test"]);
        write_entry(&base, &entry("same", "old"));
        run(&base, &["add", ".bbox/knowledge"]);
        run(&base, &["commit", "-q", "-m", "seed"]);

        let worktree = temp.path().join("worktree");
        run(
            &base,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        write_entry(&base, &entry("same", "promoted"));
        run(&base, &["commit", "-q", "-am", "publish"]);
        write_entry(&worktree, &entry("same", "promoted"));

        let checkout = ResolvedCheckoutScope {
            published_scope: PublishedScope {
                repo_id: "repo".into(),
                bbox_root_relpath: ".".into(),
            },
            checkout_id: "checkout".into(),
            checkout_dir: worktree.to_string_lossy().into_owned(),
            checkout_project_dir: worktree.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/feature".into()),
        };
        let snapshot = recompute_overlay(&base, "refs/heads/main", &checkout);
        assert_eq!(snapshot.status, OverlayStatus::Valid, "{snapshot:?}");
        assert!(snapshot.values.is_empty());
    }

    #[test]
    fn overlay_reads_monorepo_scope_from_repo_relative_tree() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("repo");
        let project = base.join("services/web");
        std::fs::create_dir_all(&project).unwrap();
        run(&base, &["init", "-q", "-b", "main"]);
        run(&base, &["config", "user.email", "t@example.com"]);
        run(&base, &["config", "user.name", "Test"]);
        write_entry(&project, &entry("web", "old"));
        run(&base, &["add", "services/web/.bbox/knowledge"]);
        run(&base, &["commit", "-q", "-m", "seed"]);

        let worktree = temp.path().join("worktree");
        run(
            &base,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        let worktree_project = worktree.join("services/web");
        write_entry(&worktree_project, &entry("web", "changed"));
        let checkout = ResolvedCheckoutScope {
            published_scope: PublishedScope {
                repo_id: "repo".into(),
                bbox_root_relpath: "services/web".into(),
            },
            checkout_id: "checkout".into(),
            checkout_dir: worktree.to_string_lossy().into_owned(),
            checkout_project_dir: worktree_project.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/feature".into()),
        };

        let snapshot = recompute_overlay(&project, "refs/heads/main", &checkout);
        assert_eq!(snapshot.status, OverlayStatus::Valid, "{snapshot:?}");
        assert!(matches!(
            snapshot.values.get("web"),
            Some(OverlayValue::Upsert { .. })
        ));
    }

    #[test]
    fn invalid_snapshot_replaces_previous_valid_state() {
        let checkout = ResolvedCheckoutScope {
            published_scope: PublishedScope {
                repo_id: "repo".into(),
                bbox_root_relpath: ".".into(),
            },
            checkout_id: "checkout".into(),
            checkout_dir: "/missing".into(),
            checkout_project_dir: "/missing".into(),
            branch_ref: None,
        };
        let mut store = KnowledgeOverlayStore::default();
        let mut valid = OverlaySnapshot::invalid(&checkout, "first");
        valid.status = OverlayStatus::Valid;
        valid.values.insert("x".into(), OverlayValue::Tombstone);
        store.publish(valid);
        store.publish(OverlaySnapshot::invalid(&checkout, "broken"));
        let current = store
            .get(&checkout.published_scope, &checkout.checkout_id)
            .unwrap();
        assert_eq!(current.status, OverlayStatus::Invalid);
        assert!(current.values.is_empty());
    }

    #[test]
    fn snapshot_id_ignores_hash_map_iteration_order() {
        let stamp = OverlayStamp {
            published_scope: PublishedScope {
                repo_id: "repo".into(),
                bbox_root_relpath: ".".into(),
            },
            checkout_id: "checkout".into(),
            published_ref: "refs/heads/main".into(),
            publisher_commit: "p".into(),
            checkout_head: "h".into(),
            merge_base: "b".into(),
            working_fingerprint: "w".into(),
        };
        let mut left_entry = entry("entry", "same bytes");
        left_entry.variants.insert("a".into(), "1".into());
        left_entry.variants.insert("b".into(), "2".into());
        let mut right_entry = entry("entry", "same bytes");
        right_entry.variants.insert("b".into(), "2".into());
        right_entry.variants.insert("a".into(), "1".into());
        let values = |entry| {
            BTreeMap::from([(
                "entry".into(),
                OverlayValue::Upsert {
                    entry: Box::new(entry),
                    content_hash: "fixed-byte-hash".into(),
                },
            )])
        };

        assert_eq!(
            snapshot_id(&stamp, &values(left_entry)).unwrap(),
            snapshot_id(&stamp, &values(right_entry)).unwrap()
        );
    }
}
