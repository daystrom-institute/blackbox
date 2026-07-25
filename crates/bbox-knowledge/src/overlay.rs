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
            Some("own") => {
                anyhow::bail!("provisional mode own requires authoritative checkout context")
            }
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

/// Immutable checkout bytes captured by the authority adapter.
///
/// The overlay layer deliberately cannot reopen checkout paths. Production
/// callers build this snapshot through the checkout lease's confined,
/// descriptor-relative reader and retain the lease until recomputation
/// finishes. This closes the former read-dir-then-open symlink race.
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

pub const MAX_CONSECUTIVE_TRANSIENT_PRESERVATIONS: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransientPreservationOutcome {
    Preserved { attempt: u8 },
    Exhausted,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayRecomputeErrorKind {
    InvalidContent,
    Transient,
}

#[derive(Debug)]
pub struct OverlayRecomputeError {
    pub kind: OverlayRecomputeErrorKind,
    diagnostic: String,
}

impl OverlayRecomputeError {
    pub fn invalid_content(error: anyhow::Error) -> Self {
        Self {
            kind: OverlayRecomputeErrorKind::InvalidContent,
            diagnostic: format!("{error:#}"),
        }
    }

    pub fn transient(error: anyhow::Error) -> Self {
        Self {
            kind: OverlayRecomputeErrorKind::Transient,
            diagnostic: format!("{error:#}"),
        }
    }
}

impl std::fmt::Display for OverlayRecomputeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.diagnostic)
    }
}

impl std::error::Error for OverlayRecomputeError {}

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
    requested_generations: BTreeMap<OverlayKey, u64>,
    transient_preservations: BTreeMap<OverlayKey, u8>,
    next_generation: u64,
}

impl KnowledgeOverlayStore {
    /// Reserve a publication generation before doing filesystem work. A later
    /// refresh for the same checkout invalidates older in-flight work.
    pub fn begin_refresh(&mut self, key: OverlayKey) -> u64 {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("knowledge overlay refresh generation exhausted");
        let generation = self.next_generation;
        self.requested_generations.insert(key, generation);
        generation
    }

    /// Publish only when no newer refresh for this checkout was requested.
    pub fn publish_if_latest(&mut self, generation: u64, snapshot: OverlaySnapshot) -> bool {
        if self.requested_generations.get(&snapshot.key) != Some(&generation) {
            return false;
        }
        self.transient_preservations.remove(&snapshot.key);
        self.snapshots.insert(snapshot.key.clone(), snapshot);
        true
    }

    /// Preserve one previously valid snapshot for a bounded transient window.
    ///
    /// A checkout that remains unreadable must not expose stale provisional
    /// values forever. Successful or invalid publication resets the sequence;
    /// callers replace the snapshot with an invalid empty value once this
    /// method reports exhaustion.
    pub fn preserve_transient_if_latest(
        &mut self,
        generation: u64,
        mut snapshot: OverlaySnapshot,
    ) -> TransientPreservationOutcome {
        if self.requested_generations.get(&snapshot.key) != Some(&generation) {
            return TransientPreservationOutcome::Superseded;
        }
        let attempt = self
            .transient_preservations
            .get(&snapshot.key)
            .copied()
            .unwrap_or_default()
            .saturating_add(1);
        if attempt > MAX_CONSECUTIVE_TRANSIENT_PRESERVATIONS {
            return TransientPreservationOutcome::Exhausted;
        }
        self.transient_preservations
            .insert(snapshot.key.clone(), attempt);
        snapshot.diagnostics.push(format!(
            "transient preservation attempt {attempt}/{MAX_CONSECUTIVE_TRANSIENT_PRESERVATIONS}"
        ));
        self.snapshots.insert(snapshot.key.clone(), snapshot);
        TransientPreservationOutcome::Preserved { attempt }
    }

    /// Replace the complete snapshot for one checkout scope. Invalid snapshots
    /// replace prior valid state instead of leaving stale values visible.
    pub fn publish(&mut self, snapshot: OverlaySnapshot) {
        let generation = self.begin_refresh(snapshot.key.clone());
        let published = self.publish_if_latest(generation, snapshot);
        debug_assert!(published);
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

    /// Remove one checkout scope after registry reconciliation or explicit
    /// teardown. Provisional bytes are never retained after their checkout is
    /// gone; the branch or live checkout is their only durable source.
    pub fn remove(
        &mut self,
        published_scope: &PublishedScope,
        checkout_id: &str,
    ) -> Option<OverlaySnapshot> {
        let key = OverlayKey {
            published_scope: published_scope.clone(),
            checkout_id: checkout_id.to_string(),
        };
        self.requested_generations.remove(&key);
        self.transient_preservations.remove(&key);
        self.snapshots.remove(&key)
    }

    /// Remove every scope carried by one checkout. A monorepo checkout can
    /// own several independently published `.bbox` roots.
    pub fn remove_checkout(&mut self, checkout_id: &str) -> Vec<OverlaySnapshot> {
        let keys = self
            .snapshots
            .keys()
            .filter(|key| key.checkout_id == checkout_id)
            .cloned()
            .collect::<Vec<_>>();
        self.requested_generations
            .retain(|key, _| key.checkout_id != checkout_id);
        self.transient_preservations
            .retain(|key, _| key.checkout_id != checkout_id);
        keys.into_iter()
            .filter_map(|key| self.snapshots.remove(&key))
            .collect()
    }
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
        let entry: KnowledgeEntry = serde_json::from_slice(&source_bytes)
            .with_context(|| format!("parsing published knowledge source {repo_path}"))?;
        let stem = Path::new(filename)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("published knowledge filename is not UTF-8")?;
        if stem != entry.id {
            anyhow::bail!("published knowledge filename and record id disagree");
        }
        if !ids.insert(entry.id) {
            anyhow::bail!("published knowledge sources contain a duplicate record id");
        }
        sources.push(PublishedKnowledgeSourceFile {
            repository_relative_filename: repo_path,
            source_bytes,
        });
    }
    Ok(sources)
}

/// Recompute one checkout overlay. Every failure becomes an invalid empty
/// snapshot so callers can publish it atomically and discard stale prior state.
pub fn recompute_overlay(
    publisher_root: &Path,
    published_ref: &str,
    checkout_root: &Path,
    working: &WorkingKnowledgeSnapshot,
    checkout: &ResolvedCheckoutScope,
) -> OverlaySnapshot {
    match recompute_overlay_result(
        publisher_root,
        published_ref,
        checkout_root,
        working,
        checkout,
    ) {
        Ok(snapshot) => snapshot,
        Err(err) => OverlaySnapshot::invalid(checkout, format!("{err:#}")),
    }
}

/// Recompute one checkout overlay while preserving whether a failure came
/// from invalid repository content or from transient Git and filesystem work.
pub fn recompute_overlay_result(
    publisher_root: &Path,
    published_ref: &str,
    checkout_root: &Path,
    working: &WorkingKnowledgeSnapshot,
    checkout: &ResolvedCheckoutScope,
) -> std::result::Result<OverlaySnapshot, OverlayRecomputeError> {
    let publisher_commit = git::resolve_commit(publisher_root, published_ref)
        .with_context(|| {
            format!(
                "published ref {published_ref} does not resolve in {}",
                publisher_root.display()
            )
        })
        .map_err(OverlayRecomputeError::transient)?;
    let checkout_head = git::current_head(checkout_root)
        .with_context(|| format!("checkout {} has no HEAD", checkout_root.display()))
        .map_err(OverlayRecomputeError::transient)?;
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
    .map_err(OverlayRecomputeError::transient)?;
    let tree_dir = knowledge_tree_dir(&checkout.published_scope);
    let baseline = read_committed_map(checkout_root, &merge_base, &tree_dir, Some(publisher_root))
        .map_err(OverlayRecomputeError::transient)?;
    let published = read_committed_map(publisher_root, &publisher_commit, &tree_dir, None)
        .map_err(OverlayRecomputeError::transient)?;
    let working = &working.files;
    validate_knowledge_map(&baseline, "baseline")
        .map_err(OverlayRecomputeError::invalid_content)?;
    validate_knowledge_map(&published, "published")
        .map_err(OverlayRecomputeError::invalid_content)?;
    validate_knowledge_map(working, "working").map_err(OverlayRecomputeError::invalid_content)?;
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
                // Equality at the pinned published ref is already integrated;
                // suppress the provisional variant even for a cherry-pick or
                // content-equivalent merge with different ancestry.
                if published.get(&filename) == Some(after) {
                    continue;
                }
                let entry: KnowledgeEntry = serde_json::from_slice(after)
                    .with_context(|| format!("parsing working knowledge file {filename}"))
                    .map_err(OverlayRecomputeError::invalid_content)?;
                let stem = Path::new(&filename)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .with_context(|| format!("knowledge filename is not UTF-8: {filename}"))
                    .map_err(OverlayRecomputeError::invalid_content)?;
                if stem != entry.id {
                    return Err(OverlayRecomputeError::invalid_content(anyhow::anyhow!(
                        "knowledge filename/id mismatch: {filename} contains id {}",
                        entry.id
                    )));
                }
                if !seen_ids.insert(entry.id.clone()) {
                    return Err(OverlayRecomputeError::invalid_content(anyhow::anyhow!(
                        "duplicate knowledge id in checkout overlay: {}",
                        entry.id
                    )));
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
                    .with_context(|| format!("knowledge filename is not UTF-8: {filename}"))
                    .map_err(OverlayRecomputeError::invalid_content)?;
                let entry: KnowledgeEntry = serde_json::from_slice(before)
                    .with_context(|| format!("parsing baseline knowledge file {filename}"))
                    .map_err(OverlayRecomputeError::invalid_content)?;
                if stem != entry.id {
                    return Err(OverlayRecomputeError::invalid_content(anyhow::anyhow!(
                        "baseline knowledge filename/id mismatch: {filename} contains id {}",
                        entry.id
                    )));
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
    let snapshot_id = snapshot_id(&stamp, &values).map_err(OverlayRecomputeError::transient)?;
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

fn validate_knowledge_map(files: &BTreeMap<String, Vec<u8>>, label: &str) -> Result<()> {
    let mut ids = BTreeSet::new();
    for (filename, bytes) in files {
        let entry: KnowledgeEntry = serde_json::from_slice(bytes)
            .with_context(|| format!("parsing {label} knowledge file {filename}"))?;
        let stem = Path::new(filename)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("knowledge filename is not UTF-8: {filename}"))?;
        if stem != entry.id {
            anyhow::bail!(
                "{label} knowledge filename/id mismatch: {filename} contains id {}",
                entry.id
            );
        }
        if !ids.insert(entry.id.clone()) {
            anyhow::bail!("duplicate {label} knowledge id: {}", entry.id);
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

    #[test]
    fn explicit_own_requires_checkout_authority() {
        assert!(ProvisionalMode::parse(Some("own"), false).is_err());
        assert_eq!(
            ProvisionalMode::parse(None, false).unwrap(),
            ProvisionalMode::Published
        );
    }

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
            project_id: None,
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

    fn working_snapshot(root: &Path) -> WorkingKnowledgeSnapshot {
        let dir = root.join(".bbox/knowledge");
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
        WorkingKnowledgeSnapshot::new(files).unwrap()
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

        let scope = PublishedScope::try_new("repo", ".").unwrap();
        let checkout = ResolvedCheckoutScope {
            project_id: "test-project".into(),
            published_scope: scope,
            checkout_id: "checkout".into(),
            checkout_dir: worktree.to_string_lossy().into_owned(),
            checkout_project_dir: worktree.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/feature".into()),
        };
        let working = working_snapshot(&worktree);
        write_entry(&worktree, &entry("keep", "swapped-after-capture"));
        let snapshot = recompute_overlay(&base, "refs/heads/main", &worktree, &working, &checkout);
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
        assert!(matches!(
            snapshot.values.get("keep"),
            Some(OverlayValue::Upsert { entry, .. }) if entry.content == "changed"
        ));
        assert_eq!(snapshot.snapshot_id.len(), 64);
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
            project_id: "test-project".into(),
            published_scope: PublishedScope::try_new("repo", ".").unwrap(),
            checkout_id: "checkout".into(),
            checkout_dir: worktree.to_string_lossy().into_owned(),
            checkout_project_dir: worktree.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/feature".into()),
        };
        let working = working_snapshot(&worktree);
        let snapshot = recompute_overlay(&base, "refs/heads/main", &worktree, &working, &checkout);
        assert_eq!(snapshot.status, OverlayStatus::Valid, "{snapshot:?}");
        assert!(snapshot.values.is_empty());
    }

    #[test]
    fn overlay_suppresses_tombstone_already_absent_at_published_ref() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run(&base, &["init", "-q", "-b", "main"]);
        run(&base, &["config", "user.email", "t@example.com"]);
        run(&base, &["config", "user.name", "Test"]);
        write_entry(&base, &entry("gone", "published then removed"));
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
        std::fs::remove_file(worktree.join(".bbox/knowledge/gone.json")).unwrap();
        std::fs::remove_file(base.join(".bbox/knowledge/gone.json")).unwrap();
        run(&base, &["add", ".bbox/knowledge"]);
        run(&base, &["commit", "-q", "-m", "publish deletion"]);

        let checkout = ResolvedCheckoutScope {
            project_id: "test-project".into(),
            published_scope: PublishedScope::try_new("repo", ".").unwrap(),
            checkout_id: "checkout".into(),
            checkout_dir: worktree.to_string_lossy().into_owned(),
            checkout_project_dir: worktree.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/feature".into()),
        };
        let working = working_snapshot(&worktree);
        let snapshot = recompute_overlay(&base, "refs/heads/main", &worktree, &working, &checkout);
        assert_eq!(snapshot.status, OverlayStatus::Valid, "{snapshot:?}");
        assert!(snapshot.values.is_empty(), "{snapshot:?}");
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
            project_id: "test-project".into(),
            published_scope: PublishedScope::try_new("repo", "services/web").unwrap(),
            checkout_id: "checkout".into(),
            checkout_dir: worktree.to_string_lossy().into_owned(),
            checkout_project_dir: worktree_project.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/feature".into()),
        };

        let working = working_snapshot(&worktree_project);
        let snapshot =
            recompute_overlay(&project, "refs/heads/main", &worktree, &working, &checkout);
        assert_eq!(snapshot.status, OverlayStatus::Valid, "{snapshot:?}");
        assert!(matches!(
            snapshot.values.get("web"),
            Some(OverlayValue::Upsert { .. })
        ));
    }

    #[test]
    fn invalid_snapshot_replaces_previous_valid_state() {
        let checkout = ResolvedCheckoutScope {
            project_id: "test-project".into(),
            published_scope: PublishedScope::try_new("repo", ".").unwrap(),
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
    fn recompute_classifies_invalid_content_separately_from_git_failures() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let base = root.join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run(&base, &["init", "-q", "-b", "main"]);
        run(&base, &["config", "user.email", "test@example.com"]);
        run(&base, &["config", "user.name", "Test"]);
        write_entry(&base, &entry("seed", "published"));
        run(&base, &["add", ".bbox/knowledge"]);
        run(&base, &["commit", "-q", "-m", "seed"]);

        let worktree = root.join("worktree");
        run(
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

        std::fs::write(worktree.join(".bbox/knowledge/broken.json"), b"{").unwrap();
        let malformed_working = working_snapshot(&worktree);
        let malformed = recompute_overlay_result(
            &base,
            "refs/heads/main",
            &worktree,
            &malformed_working,
            &checkout,
        )
        .unwrap_err();
        assert_eq!(malformed.kind, OverlayRecomputeErrorKind::InvalidContent);

        let mut missing_checkout = checkout;
        missing_checkout.checkout_dir = root.join("missing").to_string_lossy().into_owned();
        missing_checkout.checkout_project_dir = missing_checkout.checkout_dir.clone();
        let missing_root = root.join("missing");
        let transient = recompute_overlay_result(
            &base,
            "refs/heads/main",
            &missing_root,
            &WorkingKnowledgeSnapshot::empty(),
            &missing_checkout,
        )
        .unwrap_err();
        assert_eq!(transient.kind, OverlayRecomputeErrorKind::Transient);
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
        let key = OverlayKey {
            published_scope: checkout.published_scope.clone(),
            checkout_id: checkout.checkout_id.clone(),
        };
        let mut store = KnowledgeOverlayStore::default();
        let stale = store.begin_refresh(key.clone());
        let current = store.begin_refresh(key);

        assert!(store.publish_if_latest(current, OverlaySnapshot::invalid(&checkout, "current")));
        assert!(!store.publish_if_latest(stale, OverlaySnapshot::invalid(&checkout, "stale")));
        assert_eq!(
            store
                .get(&checkout.published_scope, &checkout.checkout_id)
                .unwrap()
                .diagnostics,
            ["current"]
        );
    }

    #[test]
    fn transient_preservation_expires_and_success_resets_the_bound() {
        let checkout = ResolvedCheckoutScope {
            project_id: "test-project".into(),
            published_scope: PublishedScope::try_new("repo", ".").unwrap(),
            checkout_id: "checkout".into(),
            checkout_dir: "/missing".into(),
            checkout_project_dir: "/missing".into(),
            branch_ref: None,
        };
        let key = OverlayKey {
            published_scope: checkout.published_scope.clone(),
            checkout_id: checkout.checkout_id.clone(),
        };
        let prior = OverlaySnapshot {
            snapshot_id: "prior".into(),
            key: key.clone(),
            stamp: None,
            status: OverlayStatus::Valid,
            values: BTreeMap::new(),
            diagnostics: Vec::new(),
        };
        let mut store = KnowledgeOverlayStore::default();
        store.publish(prior.clone());
        for attempt in 1..=MAX_CONSECUTIVE_TRANSIENT_PRESERVATIONS {
            let generation = store.begin_refresh(key.clone());
            assert_eq!(
                store.preserve_transient_if_latest(generation, prior.clone()),
                TransientPreservationOutcome::Preserved { attempt }
            );
        }
        let exhausted = store.begin_refresh(key.clone());
        assert_eq!(
            store.preserve_transient_if_latest(exhausted, prior.clone()),
            TransientPreservationOutcome::Exhausted
        );

        let recovered = store.begin_refresh(key.clone());
        assert!(store.publish_if_latest(recovered, prior.clone()));
        let after_reset = store.begin_refresh(key);
        assert_eq!(
            store.preserve_transient_if_latest(after_reset, prior),
            TransientPreservationOutcome::Preserved { attempt: 1 }
        );
    }

    #[test]
    fn snapshot_id_ignores_hash_map_iteration_order() {
        let stamp = OverlayStamp {
            published_scope: PublishedScope::try_new("repo", ".").unwrap(),
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
