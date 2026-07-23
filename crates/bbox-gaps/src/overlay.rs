//! Checkout-scoped provisional overlays for repo-owned gap notes.
//!
//! Gap records share the knowledge lane's checkout registry, publisher pin,
//! merge-base model, and content-equality promotion rule. The typed snapshot
//! stays in this crate because gaps have no search or render projection.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use bbox_corpus_core::git;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_record::ResolvedCheckoutScope;
use serde::{Deserialize, Serialize};
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

/// Immutable checkout bytes captured by the authority adapter.
///
/// Overlay recomputation consumes these bytes without reopening checkout
/// paths. Production callers must obtain them through the checkout lease's
/// confined descriptor-relative reader before authority revalidation.
#[derive(Debug, Clone, Default)]
pub struct WorkingGapSnapshot {
    files: BTreeMap<String, Vec<u8>>,
}

impl WorkingGapSnapshot {
    pub fn new(files: BTreeMap<String, Vec<u8>>) -> Result<Self> {
        for filename in files.keys() {
            validate_snapshot_filename(filename, "gap")?;
        }
        Ok(Self { files })
    }

    pub fn empty() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GapOverlayKey {
    pub published_scope: PublishedScope,
    pub checkout_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapOverlayStamp {
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
pub enum GapOverlayValue {
    Upsert {
        gap: Box<GapNote>,
        content_hash: String,
    },
    Tombstone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapOverlayStatus {
    Valid,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapOverlayRecomputeErrorKind {
    InvalidContent,
    Transient,
}

#[derive(Debug)]
pub struct GapOverlayRecomputeError {
    pub kind: GapOverlayRecomputeErrorKind,
    diagnostic: String,
}

impl GapOverlayRecomputeError {
    pub fn invalid_content(error: anyhow::Error) -> Self {
        Self {
            kind: GapOverlayRecomputeErrorKind::InvalidContent,
            diagnostic: format!("{error:#}"),
        }
    }

    pub fn transient(error: anyhow::Error) -> Self {
        Self {
            kind: GapOverlayRecomputeErrorKind::Transient,
            diagnostic: format!("{error:#}"),
        }
    }
}

impl std::fmt::Display for GapOverlayRecomputeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.diagnostic)
    }
}

impl std::error::Error for GapOverlayRecomputeError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GapOverlaySnapshot {
    pub snapshot_id: String,
    pub key: GapOverlayKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stamp: Option<GapOverlayStamp>,
    pub status: GapOverlayStatus,
    #[serde(default)]
    pub values: BTreeMap<String, GapOverlayValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

impl GapOverlaySnapshot {
    pub fn invalid(checkout: &ResolvedCheckoutScope, diagnostic: impl Into<String>) -> Self {
        Self {
            snapshot_id: String::new(),
            key: GapOverlayKey {
                published_scope: checkout.published_scope.clone(),
                checkout_id: checkout.checkout_id.clone(),
            },
            stamp: None,
            status: GapOverlayStatus::Invalid,
            values: BTreeMap::new(),
            diagnostics: vec![diagnostic.into()],
        }
    }
}

#[derive(Debug, Default)]
pub struct GapOverlayStore {
    snapshots: BTreeMap<GapOverlayKey, GapOverlaySnapshot>,
    requested_generations: BTreeMap<GapOverlayKey, u64>,
    next_generation: u64,
}

impl GapOverlayStore {
    pub fn begin_refresh(&mut self, key: GapOverlayKey) -> u64 {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("gap overlay refresh generation exhausted");
        let generation = self.next_generation;
        self.requested_generations.insert(key, generation);
        generation
    }

    pub fn publish_if_latest(&mut self, generation: u64, snapshot: GapOverlaySnapshot) -> bool {
        if self.requested_generations.get(&snapshot.key) != Some(&generation) {
            return false;
        }
        self.snapshots.insert(snapshot.key.clone(), snapshot);
        true
    }

    pub fn publish(&mut self, snapshot: GapOverlaySnapshot) {
        let generation = self.begin_refresh(snapshot.key.clone());
        let published = self.publish_if_latest(generation, snapshot);
        debug_assert!(published);
    }

    pub fn get(
        &self,
        published_scope: &PublishedScope,
        checkout_id: &str,
    ) -> Option<&GapOverlaySnapshot> {
        self.snapshots.get(&GapOverlayKey {
            published_scope: published_scope.clone(),
            checkout_id: checkout_id.to_string(),
        })
    }

    pub fn snapshots(&self) -> impl Iterator<Item = &GapOverlaySnapshot> {
        self.snapshots.values()
    }

    pub fn remove(
        &mut self,
        published_scope: &PublishedScope,
        checkout_id: &str,
    ) -> Option<GapOverlaySnapshot> {
        let key = GapOverlayKey {
            published_scope: published_scope.clone(),
            checkout_id: checkout_id.to_string(),
        };
        self.requested_generations.remove(&key);
        self.snapshots.remove(&key)
    }

    pub fn remove_checkout(&mut self, checkout_id: &str) -> Vec<GapOverlaySnapshot> {
        let keys = self
            .snapshots
            .keys()
            .filter(|key| key.checkout_id == checkout_id)
            .cloned()
            .collect::<Vec<_>>();
        self.requested_generations
            .retain(|key, _| key.checkout_id != checkout_id);
        keys.into_iter()
            .filter_map(|key| self.snapshots.remove(&key))
            .collect()
    }
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

pub fn recompute_overlay(
    publisher_root: &Path,
    published_ref: &str,
    checkout_root: &Path,
    working: &WorkingGapSnapshot,
    checkout: &ResolvedCheckoutScope,
) -> GapOverlaySnapshot {
    match recompute_overlay_result(
        publisher_root,
        published_ref,
        checkout_root,
        working,
        checkout,
    ) {
        Ok(snapshot) => snapshot,
        Err(err) => GapOverlaySnapshot::invalid(checkout, format!("{err:#}")),
    }
}

pub fn recompute_overlay_result(
    publisher_root: &Path,
    published_ref: &str,
    checkout_root: &Path,
    working: &WorkingGapSnapshot,
    checkout: &ResolvedCheckoutScope,
) -> std::result::Result<GapOverlaySnapshot, GapOverlayRecomputeError> {
    let publisher_commit = git::resolve_commit(publisher_root, published_ref)
        .with_context(|| {
            format!(
                "published ref {published_ref} does not resolve in {}",
                publisher_root.display()
            )
        })
        .map_err(GapOverlayRecomputeError::transient)?;
    let checkout_head = git::current_head(checkout_root)
        .with_context(|| format!("checkout {} has no HEAD", checkout_root.display()))
        .map_err(GapOverlayRecomputeError::transient)?;
    let merge_base = git::merge_base_with_alternate(
        checkout_root,
        &checkout_head,
        &publisher_commit,
        Some(publisher_root),
    )
    .with_context(|| {
        format!(
            "no merge base between checkout {} and published commit {}",
            checkout_root.display(),
            publisher_commit
        )
    })
    .map_err(GapOverlayRecomputeError::transient)?;
    let tree_dir = gaps_tree_dir(&checkout.published_scope);
    let baseline = read_committed_map(checkout_root, &merge_base, &tree_dir, Some(publisher_root))
        .map_err(GapOverlayRecomputeError::transient)?;
    let published = read_committed_map(publisher_root, &publisher_commit, &tree_dir, None)
        .map_err(GapOverlayRecomputeError::transient)?;
    let working = &working.files;
    validate_gap_map(&baseline, "baseline").map_err(GapOverlayRecomputeError::invalid_content)?;
    validate_gap_map(&published, "published").map_err(GapOverlayRecomputeError::invalid_content)?;
    validate_gap_map(working, "working").map_err(GapOverlayRecomputeError::invalid_content)?;
    let working_fingerprint = fingerprint_map(working);

    let mut paths = BTreeSet::new();
    paths.extend(baseline.keys().cloned());
    paths.extend(working.keys().cloned());
    let mut values = BTreeMap::new();
    let mut seen_ids = BTreeSet::new();
    for filename in paths {
        match (baseline.get(&filename), working.get(&filename)) {
            (Some(before), Some(after)) if before == after => {}
            (_, Some(after)) => {
                if published.get(&filename) == Some(after) {
                    continue;
                }
                let mut gap: GapNote = serde_json::from_slice(after)
                    .with_context(|| format!("parsing working gap file {filename}"))
                    .map_err(GapOverlayRecomputeError::invalid_content)?;
                validate_filename_id(&filename, &gap.id, "working gap")
                    .map_err(GapOverlayRecomputeError::invalid_content)?;
                if !seen_ids.insert(gap.id.clone()) {
                    return Err(GapOverlayRecomputeError::invalid_content(anyhow::anyhow!(
                        "duplicate gap id in checkout overlay: {}",
                        gap.id
                    )));
                }
                stamp_gap(&mut gap, &checkout.checkout_project_dir);
                values.insert(
                    gap.id.clone(),
                    GapOverlayValue::Upsert {
                        gap: Box::new(gap),
                        content_hash: sha256(after),
                    },
                );
            }
            (Some(before), None) => {
                let gap: GapNote = serde_json::from_slice(before)
                    .with_context(|| format!("parsing baseline gap file {filename}"))
                    .map_err(GapOverlayRecomputeError::invalid_content)?;
                validate_filename_id(&filename, &gap.id, "baseline gap")
                    .map_err(GapOverlayRecomputeError::invalid_content)?;
                if published.contains_key(&filename) {
                    values.insert(gap.id, GapOverlayValue::Tombstone);
                }
            }
            (None, None) => {}
        }
    }

    let stamp = GapOverlayStamp {
        published_scope: checkout.published_scope.clone(),
        checkout_id: checkout.checkout_id.clone(),
        published_ref: published_ref.to_string(),
        publisher_commit,
        checkout_head,
        merge_base,
        working_fingerprint,
    };
    let snapshot_id = snapshot_id(&stamp, &values);
    Ok(GapOverlaySnapshot {
        snapshot_id,
        key: GapOverlayKey {
            published_scope: checkout.published_scope.clone(),
            checkout_id: checkout.checkout_id.clone(),
        },
        stamp: Some(stamp),
        status: GapOverlayStatus::Valid,
        values,
        diagnostics: Vec::new(),
    })
}

fn stamp_gap(gap: &mut GapNote, project: &str) {
    gap.project = Some(project.to_string());
    gap.write_dir = None;
    gap.provisional_checkout_id = None;
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

fn validate_gap_map(files: &BTreeMap<String, Vec<u8>>, label: &str) -> Result<()> {
    let mut ids = BTreeSet::new();
    for (filename, bytes) in files {
        let gap: GapNote = serde_json::from_slice(bytes)
            .with_context(|| format!("parsing {label} gap file {filename}"))?;
        validate_filename_id(filename, &gap.id, &format!("{label} gap"))?;
        if !ids.insert(gap.id.clone()) {
            anyhow::bail!("duplicate {label} gap id: {}", gap.id);
        }
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
    let prefix = format!("{tree_dir}/");
    let mut files = BTreeMap::new();
    for repo_path in
        git::list_committed_dir_result_with_alternate(root, commit, tree_dir, alternate_root)?
    {
        let Some(filename) = repo_path.strip_prefix(&prefix) else {
            continue;
        };
        if filename.contains('/') || !filename.ends_with(".json") {
            continue;
        }
        let bytes =
            git::read_committed_file_bytes_with_alternate(root, commit, &repo_path, alternate_root)
                .with_context(|| format!("reading committed gap file {repo_path} at {commit}"))?;
        files.insert(filename.to_string(), bytes);
    }
    Ok(files)
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

fn snapshot_id(stamp: &GapOverlayStamp, values: &BTreeMap<String, GapOverlayValue>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-gap-overlay-v1\0");
    hasher.update(serde_json::to_vec(stamp).unwrap_or_default());
    for (id, value) in values {
        hasher.update((id.len() as u64).to_be_bytes());
        hasher.update(id.as_bytes());
        match value {
            GapOverlayValue::Upsert { content_hash, .. } => {
                hasher.update([1]);
                hasher.update(content_hash.as_bytes());
            }
            GapOverlayValue::Tombstone => hasher.update([2]),
        }
    }
    hex_digest(hasher.finalize())
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
            write_dir: None,
            provisional_checkout_id: None,
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

    fn write_gap(root: &Path, gap: &GapNote) {
        let dir = root.join(".bbox/gaps");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.json", gap.id)),
            serde_json::to_vec_pretty(gap).unwrap(),
        )
        .unwrap();
    }

    fn working_snapshot(root: &Path) -> WorkingGapSnapshot {
        let dir = root.join(".bbox/gaps");
        let mut files = BTreeMap::new();
        if dir.is_dir() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().and_then(|extension| extension.to_str()) == Some("json")
                    && path.is_file()
                {
                    let filename = path.file_name().unwrap().to_str().unwrap().to_string();
                    files.insert(filename, std::fs::read(path).unwrap());
                }
            }
        }
        WorkingGapSnapshot::new(files).unwrap()
    }

    #[test]
    fn overlay_captures_gap_upserts_and_tombstones() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        git(&base, &["init", "-q", "-b", "main"]);
        git(&base, &["config", "user.email", "test@example.com"]);
        git(&base, &["config", "user.name", "Test"]);
        write_gap(&base, &gap("gap-11111111", "old"));
        write_gap(&base, &gap("gap-22222222", "remove"));
        git(&base, &["add", ".bbox/gaps"]);
        git(&base, &["commit", "-q", "-m", "seed"]);

        let worktree = temp.path().join("worktree");
        git(
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
        write_gap(&worktree, &gap("gap-11111111", "changed"));
        write_gap(&worktree, &gap("gap-33333333", "new"));
        std::fs::remove_file(worktree.join(".bbox/gaps/gap-22222222.json")).unwrap();

        let checkout = ResolvedCheckoutScope {
            project_id: "test-project".into(),
            published_scope: PublishedScope::try_new("repo", ".").unwrap(),
            checkout_id: "checkout".into(),
            checkout_dir: worktree.to_string_lossy().into_owned(),
            checkout_project_dir: worktree.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/feature".into()),
        };
        let working = working_snapshot(&worktree);
        write_gap(&worktree, &gap("gap-11111111", "swapped-after-capture"));
        let snapshot = recompute_overlay(&base, "refs/heads/main", &worktree, &working, &checkout);
        assert_eq!(snapshot.status, GapOverlayStatus::Valid, "{snapshot:?}");
        assert!(matches!(
            snapshot.values.get("gap-11111111"),
            Some(GapOverlayValue::Upsert { .. })
        ));
        assert!(matches!(
            snapshot.values.get("gap-33333333"),
            Some(GapOverlayValue::Upsert { .. })
        ));
        assert!(matches!(
            snapshot.values.get("gap-22222222"),
            Some(GapOverlayValue::Tombstone)
        ));
        assert!(matches!(
            snapshot.values.get("gap-11111111"),
            Some(GapOverlayValue::Upsert { gap, .. }) if gap.title == "changed"
        ));
        assert_eq!(snapshot.snapshot_id.len(), 64);

        write_gap(&worktree, &gap("gap-11111111", "changed"));
        write_gap(&base, &gap("gap-11111111", "changed"));
        std::fs::remove_file(base.join(".bbox/gaps/gap-22222222.json")).unwrap();
        git(&base, &["add", ".bbox/gaps"]);
        git(&base, &["commit", "-q", "-m", "promote"]);
        let promoted_working = working_snapshot(&worktree);
        let promoted = recompute_overlay(
            &base,
            "refs/heads/main",
            &worktree,
            &promoted_working,
            &checkout,
        );
        assert_eq!(promoted.status, GapOverlayStatus::Valid);
        assert!(!promoted.values.contains_key("gap-11111111"));
        assert!(!promoted.values.contains_key("gap-22222222"));
        assert!(promoted.values.contains_key("gap-33333333"));
    }

    #[test]
    fn recompute_classifies_invalid_content_separately_from_git_failures() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let base = root.join("repo");
        std::fs::create_dir_all(&base).unwrap();
        git(&base, &["init", "-q", "-b", "main"]);
        git(&base, &["config", "user.email", "test@example.com"]);
        git(&base, &["config", "user.name", "Test"]);
        write_gap(&base, &gap("gap-11111111", "published"));
        git(&base, &["add", ".bbox/gaps"]);
        git(&base, &["commit", "-q", "-m", "seed"]);

        let worktree = root.join("worktree");
        git(
            &base,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature-classification",
                worktree.to_str().unwrap(),
            ],
        );
        let checkout = ResolvedCheckoutScope {
            project_id: "test-project".into(),
            published_scope: PublishedScope::try_new("repo", ".").unwrap(),
            checkout_id: "checkout".into(),
            checkout_dir: worktree.to_string_lossy().into_owned(),
            checkout_project_dir: worktree.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/feature-classification".into()),
        };

        std::fs::write(worktree.join(".bbox/gaps/broken.json"), b"{").unwrap();
        let malformed_working = working_snapshot(&worktree);
        let malformed = recompute_overlay_result(
            &base,
            "refs/heads/main",
            &worktree,
            &malformed_working,
            &checkout,
        )
        .unwrap_err();
        assert_eq!(malformed.kind, GapOverlayRecomputeErrorKind::InvalidContent);

        let mut missing_checkout = checkout;
        missing_checkout.checkout_dir = root.join("missing").to_string_lossy().into_owned();
        missing_checkout.checkout_project_dir = missing_checkout.checkout_dir.clone();
        let missing_root = root.join("missing");
        let transient = recompute_overlay_result(
            &base,
            "refs/heads/main",
            &missing_root,
            &WorkingGapSnapshot::empty(),
            &missing_checkout,
        )
        .unwrap_err();
        assert_eq!(transient.kind, GapOverlayRecomputeErrorKind::Transient);
    }

    #[test]
    fn working_snapshot_rejects_non_basename_paths() {
        for filename in ["../escape.json", "nested/gap.json", "gap.txt"] {
            assert!(
                WorkingGapSnapshot::new(BTreeMap::from([(filename.to_string(), b"{}".to_vec(),)]))
                    .is_err(),
                "unsafe snapshot filename should be rejected: {filename}"
            );
        }
    }

    #[test]
    fn stale_refresh_cannot_overwrite_newer_snapshot() {
        let checkout = ResolvedCheckoutScope {
            project_id: "test-project".into(),
            published_scope: PublishedScope::try_new("repo", ".").unwrap(),
            checkout_id: "checkout".into(),
            checkout_dir: "/missing".into(),
            checkout_project_dir: "/missing".into(),
            branch_ref: None,
        };
        let key = GapOverlayKey {
            published_scope: checkout.published_scope.clone(),
            checkout_id: checkout.checkout_id.clone(),
        };
        let mut store = GapOverlayStore::default();
        let stale = store.begin_refresh(key.clone());
        let current = store.begin_refresh(key);

        assert!(
            store.publish_if_latest(current, GapOverlaySnapshot::invalid(&checkout, "current"))
        );
        assert!(!store.publish_if_latest(stale, GapOverlaySnapshot::invalid(&checkout, "stale")));
        assert_eq!(
            store
                .get(&checkout.published_scope, &checkout.checkout_id)
                .unwrap()
                .diagnostics,
            ["current"]
        );
    }
}
