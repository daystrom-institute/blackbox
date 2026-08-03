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
    /// Accepted generation identity, catalog mode only. Omitted from the
    /// serialization on the bridge so snapshot ids stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_generation: Option<String>,
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

pub const MAX_CONSECUTIVE_TRANSIENT_PRESERVATIONS: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapTransientPreservationOutcome {
    Preserved { attempt: u8 },
    Exhausted,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapOverlayRecomputeErrorKind {
    InvalidContent,
    Transient,
    /// The checkout cannot prove the baseline: it does not contain the
    /// accepted commit, or the two histories share no merge base. Structural
    /// authority, never transient-preserved (plan section 4.12).
    BaselineUnavailable,
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

    pub fn baseline_unavailable(error: anyhow::Error) -> Self {
        Self {
            kind: GapOverlayRecomputeErrorKind::BaselineUnavailable,
            diagnostic: format!("{error:#}"),
        }
    }

    /// True when this failure is a structural fact about authority rather
    /// than a retryable condition.
    pub fn is_structural(&self) -> bool {
        matches!(self.kind, GapOverlayRecomputeErrorKind::BaselineUnavailable)
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
    transient_preservations: BTreeMap<GapOverlayKey, u8>,
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
        self.transient_preservations.remove(&snapshot.key);
        self.snapshots.insert(snapshot.key.clone(), snapshot);
        true
    }

    pub fn preserve_transient_if_latest(
        &mut self,
        generation: u64,
        mut snapshot: GapOverlaySnapshot,
    ) -> GapTransientPreservationOutcome {
        if self.requested_generations.get(&snapshot.key) != Some(&generation) {
            return GapTransientPreservationOutcome::Superseded;
        }
        let attempt = self
            .transient_preservations
            .get(&snapshot.key)
            .copied()
            .unwrap_or_default()
            .saturating_add(1);
        if attempt > MAX_CONSECUTIVE_TRANSIENT_PRESERVATIONS {
            return GapTransientPreservationOutcome::Exhausted;
        }
        self.transient_preservations
            .insert(snapshot.key.clone(), attempt);
        snapshot.diagnostics.push(format!(
            "transient preservation attempt {attempt}/{MAX_CONSECUTIVE_TRANSIENT_PRESERVATIONS}"
        ));
        self.snapshots.insert(snapshot.key.clone(), snapshot);
        GapTransientPreservationOutcome::Preserved { attempt }
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
        self.transient_preservations.remove(&key);
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
        self.transient_preservations
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

/// What the gap diff needs to know about published content. The bridge
/// answers from committed bytes; the catalog answers from the accepted
/// generation manifest, which records each source file digest rather than
/// its blob.
trait PublishedGapAuthority {
    fn contains(&self, filename: &str) -> bool;
    fn matches(&self, filename: &str, working: &[u8]) -> bool;
}

impl PublishedGapAuthority for BTreeMap<String, Vec<u8>> {
    fn contains(&self, filename: &str) -> bool {
        self.contains_key(filename)
    }

    fn matches(&self, filename: &str, working: &[u8]) -> bool {
        self.get(filename).is_some_and(|bytes| bytes == working)
    }
}

/// Repository-relative filename to the lowercase SHA-256 of its exact
/// committed bytes, from an accepted generation manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptedPublishedGapDigests(pub BTreeMap<String, String>);

impl PublishedGapAuthority for AcceptedPublishedGapDigests {
    fn contains(&self, filename: &str) -> bool {
        self.0.contains_key(filename)
    }

    fn matches(&self, filename: &str, working: &[u8]) -> bool {
        self.0
            .get(filename)
            .is_some_and(|digest| digest == &sha256(working))
    }
}

/// The gap overlay diff shared by the bridge and catalog entry points.
/// One implementation keeps the two paths differing only in where
/// published content and ancestry come from.
fn gap_overlay_values_from_maps(
    baseline: &BTreeMap<String, Vec<u8>>,
    published: &dyn PublishedGapAuthority,
    working: &BTreeMap<String, Vec<u8>>,
    checkout_project_dir: &str,
) -> std::result::Result<BTreeMap<String, GapOverlayValue>, GapOverlayRecomputeError> {
    let mut paths = BTreeSet::new();
    paths.extend(baseline.keys().cloned());
    paths.extend(working.keys().cloned());
    let mut values = BTreeMap::new();
    let mut seen_ids = BTreeSet::new();
    for filename in paths {
        match (baseline.get(&filename), working.get(&filename)) {
            (Some(before), Some(after)) if before == after => {}
            (_, Some(after)) => {
                if published.matches(&filename, after) {
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
                stamp_gap(&mut gap, checkout_project_dir);
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
                if published.contains(&filename) {
                    values.insert(gap.id, GapOverlayValue::Tombstone);
                }
            }
            (None, None) => {}
        }
    }
    Ok(values)
}
/// Accepted published gap content plus the identity that stamps it.
///
/// Like the knowledge twin, there is no publisher root and no alternate
/// object database anywhere in this contract: ancestry may come only from
/// the checkout the overlay is computed in (D-007, plan section 4.11).
#[derive(Debug, Clone, Copy)]
pub struct CatalogGapOverlayPublished<'a> {
    pub published_scope: &'a PublishedScope,
    pub checkout_id: &'a str,
    pub full_ref: &'a str,
    pub accepted_commit: &'a str,
    pub accepted_generation: &'a str,
    /// Filename to committed-source digest, from the accepted manifest.
    pub published: &'a AcceptedPublishedGapDigests,
}

/// Catalog-mode gap overlay recompute (plan sections 4.11 and 6.7).
pub fn recompute_catalog_overlay_result(
    published: CatalogGapOverlayPublished<'_>,
    checkout_root: &Path,
    working: &WorkingGapSnapshot,
) -> std::result::Result<GapOverlaySnapshot, GapOverlayRecomputeError> {
    let checkout_head = git::current_head(checkout_root)
        .with_context(|| format!("checkout {} has no HEAD", checkout_root.display()))
        .map_err(GapOverlayRecomputeError::transient)?;
    git::verify_commit_oid_with_alternate(checkout_root, published.accepted_commit, None)
        .with_context(|| {
            format!(
                "checkout {} does not contain accepted commit {}",
                checkout_root.display(),
                published.accepted_commit
            )
        })
        .map_err(GapOverlayRecomputeError::baseline_unavailable)?;
    let merge_base = git::merge_base(checkout_root, &checkout_head, published.accepted_commit)
        .with_context(|| {
            format!(
                "no merge base between checkout {} and accepted commit {}",
                checkout_root.display(),
                published.accepted_commit
            )
        })
        .map_err(GapOverlayRecomputeError::baseline_unavailable)?;
    let tree_dir = gaps_tree_dir(published.published_scope);
    let baseline = read_committed_map(checkout_root, &merge_base, &tree_dir, None)
        .map_err(GapOverlayRecomputeError::transient)?;
    let working = &working.files;
    validate_gap_map(&baseline, "baseline").map_err(GapOverlayRecomputeError::invalid_content)?;
    validate_gap_map(working, "working").map_err(GapOverlayRecomputeError::invalid_content)?;
    let working_fingerprint = fingerprint_map(working);
    // A catalog overlay row carries no host path: the gap view stamps
    // project identity, and the checkout directory is not authority.
    let values = gap_overlay_values_from_maps(&baseline, published.published, working, "")?;

    let stamp = GapOverlayStamp {
        published_scope: published.published_scope.clone(),
        checkout_id: published.checkout_id.to_string(),
        published_ref: published.full_ref.to_string(),
        publisher_commit: published.accepted_commit.to_string(),
        checkout_head,
        merge_base,
        working_fingerprint,
        accepted_generation: Some(published.accepted_generation.to_string()),
    };
    let snapshot_id = snapshot_id(&stamp, &values);
    Ok(GapOverlaySnapshot {
        snapshot_id,
        key: GapOverlayKey {
            published_scope: published.published_scope.clone(),
            checkout_id: published.checkout_id.to_string(),
        },
        stamp: Some(stamp),
        status: GapOverlayStatus::Valid,
        values,
        diagnostics: Vec::new(),
    })
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

    let values = gap_overlay_values_from_maps(
        &baseline,
        &published,
        working,
        &checkout.checkout_project_dir,
    )?;

    let stamp = GapOverlayStamp {
        published_scope: checkout.published_scope.clone(),
        checkout_id: checkout.checkout_id.clone(),
        published_ref: published_ref.to_string(),
        publisher_commit,
        checkout_head,
        merge_base,
        working_fingerprint,
        // The bridge has no accepted generation; omitted from the
        // serialization so snapshot ids are unchanged.
        accepted_generation: None,
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

    /// The catalog gap entry point takes accepted content, a checkout, and
    /// a working snapshot. A publisher root has nowhere to go.
    const _CATALOG_ENTRY_POINT_TAKES_NO_PUBLISHER_ROOT: fn(
        CatalogGapOverlayPublished<'_>,
        &Path,
        &WorkingGapSnapshot,
    ) -> std::result::Result<
        GapOverlaySnapshot,
        GapOverlayRecomputeError,
    > = recompute_catalog_overlay_result;

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

    // ── Catalog gap overlay baseline path (plan section 13.4) ────────

    struct CatalogGapFixture {
        temp: tempfile::TempDir,
        worktree: std::path::PathBuf,
        accepted_commit: String,
        published: AcceptedPublishedGapDigests,
        scope: PublishedScope,
    }

    fn catalog_gap_fixture() -> CatalogGapFixture {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        git(&base, &["init", "-q", "-b", "main"]);
        git(&base, &["config", "user.email", "t@example.com"]);
        git(&base, &["config", "user.name", "Test"]);
        write_gap(&base, &gap("gap-11111111", "accepted"));
        write_gap(&base, &gap("gap-22222222", "accepted"));
        git(&base, &["add", ".bbox/gaps"]);
        git(&base, &["commit", "-q", "-m", "accepted"]);
        let accepted_commit = git::current_head(&base).unwrap();
        let mut published = AcceptedPublishedGapDigests::default();
        for id in ["gap-11111111", "gap-22222222"] {
            published.0.insert(
                format!("{id}.json"),
                sha256(&std::fs::read(base.join(format!(".bbox/gaps/{id}.json"))).unwrap()),
            );
        }
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
        CatalogGapFixture {
            temp,
            worktree,
            accepted_commit,
            published,
            scope: PublishedScope::try_new("repo", ".").unwrap(),
        }
    }

    impl CatalogGapFixture {
        fn recompute(
            &self,
            root: &Path,
        ) -> std::result::Result<GapOverlaySnapshot, GapOverlayRecomputeError> {
            recompute_catalog_overlay_result(
                CatalogGapOverlayPublished {
                    published_scope: &self.scope,
                    checkout_id: "checkout-1",
                    full_ref: "refs/heads/main",
                    accepted_commit: &self.accepted_commit,
                    accepted_generation: "generation-1",
                    published: &self.published,
                },
                root,
                &working_snapshot(root),
            )
        }
    }

    #[test]
    fn catalog_gap_overlay_diffs_the_checkout_against_accepted_content() {
        let fixture = catalog_gap_fixture();
        write_gap(&fixture.worktree, &gap("gap-11111111", "changed"));
        write_gap(&fixture.worktree, &gap("gap-33333333", "untracked"));
        std::fs::remove_file(fixture.worktree.join(".bbox/gaps/gap-22222222.json")).unwrap();

        let snapshot = fixture.recompute(&fixture.worktree).unwrap();
        assert_eq!(snapshot.status, GapOverlayStatus::Valid);
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
        let stamp = snapshot.stamp.unwrap();
        assert_eq!(stamp.publisher_commit, fixture.accepted_commit);
        assert_eq!(stamp.merge_base, fixture.accepted_commit);
        assert_eq!(stamp.accepted_generation.as_deref(), Some("generation-1"));
        // A catalog overlay row carries no host path.
        match snapshot.values.get("gap-11111111") {
            Some(GapOverlayValue::Upsert { gap, .. }) => {
                assert_eq!(gap.project.as_deref(), Some(""));
                assert_eq!(gap.write_dir, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_gap_checkout_without_the_accepted_commit_is_structurally_unavailable() {
        let fixture = catalog_gap_fixture();
        let peer = fixture.temp.path().join("peer");
        std::fs::create_dir_all(&peer).unwrap();
        git(&peer, &["init", "-q", "-b", "main"]);
        git(&peer, &["config", "user.email", "t@example.com"]);
        git(&peer, &["config", "user.name", "Test"]);
        write_gap(&peer, &gap("gap-11111111", "peer"));
        git(&peer, &["add", ".bbox/gaps"]);
        git(&peer, &["commit", "-q", "-m", "peer"]);

        let error = fixture.recompute(&peer).unwrap_err();
        assert_eq!(
            error.kind,
            GapOverlayRecomputeErrorKind::BaselineUnavailable
        );
        assert!(error.is_structural());
    }

    #[test]
    fn an_absent_gap_merge_base_is_structurally_unavailable() {
        let fixture = catalog_gap_fixture();
        git(
            &fixture.worktree,
            &["checkout", "-q", "--orphan", "detached"],
        );
        write_gap(&fixture.worktree, &gap("gap-11111111", "orphan"));
        git(&fixture.worktree, &["add", ".bbox/gaps"]);
        git(&fixture.worktree, &["commit", "-q", "-m", "orphan"]);

        let error = fixture.recompute(&fixture.worktree).unwrap_err();
        assert_eq!(
            error.kind,
            GapOverlayRecomputeErrorKind::BaselineUnavailable
        );
        assert!(error.is_structural());
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
        let key = GapOverlayKey {
            published_scope: checkout.published_scope.clone(),
            checkout_id: checkout.checkout_id.clone(),
        };
        let prior = GapOverlaySnapshot {
            snapshot_id: "prior".into(),
            key: key.clone(),
            stamp: None,
            status: GapOverlayStatus::Valid,
            values: BTreeMap::new(),
            diagnostics: Vec::new(),
        };
        let mut store = GapOverlayStore::default();
        store.publish(prior.clone());
        for attempt in 1..=MAX_CONSECUTIVE_TRANSIENT_PRESERVATIONS {
            let generation = store.begin_refresh(key.clone());
            assert_eq!(
                store.preserve_transient_if_latest(generation, prior.clone()),
                GapTransientPreservationOutcome::Preserved { attempt }
            );
        }
        let exhausted = store.begin_refresh(key.clone());
        assert_eq!(
            store.preserve_transient_if_latest(exhausted, prior.clone()),
            GapTransientPreservationOutcome::Exhausted
        );

        let recovered = store.begin_refresh(key.clone());
        assert!(store.publish_if_latest(recovered, prior.clone()));
        let after_reset = store.begin_refresh(key);
        assert_eq!(
            store.preserve_transient_if_latest(after_reset, prior),
            GapTransientPreservationOutcome::Preserved { attempt: 1 }
        );
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
