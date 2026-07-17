//! Per-mount materialization manifest: the durable record mapping remote
//! identity to local materialization state. It is what a bare change cursor
//! cannot be - deletion tombstones map back to local paths through it,
//! renames reconcile as moves instead of delete+refetch, and it is the
//! collision ledger for the case-fold/NFC path-safety encoding.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

const MANIFEST_VERSION: u32 = 1;

/// State invariant, not a sentinel: `content_hash` is `Some` exactly when
/// `state == Materialized`. `Skipped`/`Pending` carry `None`. Enforced by
/// [`ManifestEntry::is_state_consistent`] and exercised in tests, not by a
/// type-level encoding, so the wire shape stays a flat, greppable JSON
/// object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryState {
    Materialized,
    /// Policy excluded this entry (never fetched) or a per-entry error
    /// prevented materialization. `reason` is operator-readable.
    Skipped {
        reason: String,
    },
    /// Enumerated but not yet processed. Transient - a well-formed manifest
    /// on disk between sync passes should carry no `Pending` entries; the
    /// state exists for driver bookkeeping mid-batch.
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Connector-stable identity carried forward across renames.
    pub remote_id: String,
    pub remote_version: String,
    /// Faithful scope-relative display path (never encoded/collision-
    /// suffixed) - the reversible half of the path-safety encoding.
    pub logical_path: String,
    /// Materialization-root-relative encoded path actually written to
    /// disk.
    pub physical_path: String,
    #[serde(default)]
    pub export_format: Option<String>,
    #[serde(default)]
    pub content_hash: Option<String>,
    pub state: EntryState,
}

impl ManifestEntry {
    pub fn is_state_consistent(&self) -> bool {
        matches!(self.state, EntryState::Materialized) == self.content_hash.is_some()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default = "manifest_version")]
    version: u32,
    entries: Vec<ManifestEntry>,
}

fn manifest_version() -> u32 {
    MANIFEST_VERSION
}

impl Manifest {
    pub fn new() -> Self {
        Self {
            version: MANIFEST_VERSION,
            entries: Vec::new(),
        }
    }

    /// Load a manifest from `path`. Missing file returns an empty manifest
    /// (first sync of a fresh mount).
    ///
    /// Synchronous storage boundary, matching `bbox-indexing`'s
    /// `ProjectRegistry::open` convention - async callers resolve this on a
    /// blocking lane.
    #[allow(clippy::disallowed_methods)]
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let manifest: Self = serde_json::from_str(&raw)
            .with_context(|| format!("parsing manifest {}", path.display()))?;
        Ok(manifest)
    }

    /// Atomically persist the manifest (temp file + same-dir rename).
    ///
    /// Synchronous storage boundary - see [`Manifest::open`].
    #[allow(clippy::disallowed_methods)]
    pub fn save(&self, path: &Path) -> Result<()> {
        bbox_corpus_core::json_store::with_store_lock(path, || {
            bbox_corpus_core::json_store::atomic_write_json_locked(path, self)
        })
    }

    pub fn entries(&self) -> impl Iterator<Item = &ManifestEntry> {
        self.entries.iter()
    }

    pub fn get(&self, remote_id: &str) -> Option<&ManifestEntry> {
        self.entries.iter().find(|e| e.remote_id == remote_id)
    }

    pub fn find_by_logical_path(&self, logical_path: &str) -> Option<&ManifestEntry> {
        self.entries.iter().find(|e| e.logical_path == logical_path)
    }

    /// Insert or replace the entry sharing `remote_id`.
    pub fn upsert(&mut self, entry: ManifestEntry) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.remote_id == entry.remote_id)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    /// Remove the entry with `remote_id`, returning it if present.
    pub fn remove_by_remote_id(&mut self, remote_id: &str) -> Option<ManifestEntry> {
        let idx = self.entries.iter().position(|e| e.remote_id == remote_id)?;
        Some(self.entries.remove(idx))
    }

    /// True iff every entry in the manifest satisfies the state invariant.
    pub fn is_consistent(&self) -> bool {
        self.entries.iter().all(ManifestEntry::is_state_consistent)
    }
}

/// Sanitize one path component for local materialization: replace path
/// separators and control characters, trim edge whitespace, and never emit
/// an empty component. Does not itself prevent traversal - callers reject
/// `.`/`..` components before sanitizing (see [`encode_physical_path`]).
pub fn sanitize_path_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' | '\0' => out.push('_'),
            c if (c as u32) < 0x20 => out.push('_'),
            c => out.push(c),
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Case-fold + NFC-normalize a path for collision comparison. The local
/// filesystem may be case-insensitive and Unicode-normalizing (APFS), so
/// two distinct remote names can collapse onto one local path even when
/// their raw bytes differ.
fn fold_key(s: &str) -> String {
    s.nfc().collect::<String>().to_lowercase()
}

/// Derive the materialization-root-relative physical path for
/// `logical_path`, disambiguating against `manifest` when the case-folded,
/// NFC-normalized candidate collides with another entry's physical path.
/// Collisions get a short suffix derived from `remote_id` appended to the
/// final component's stem, so two colliding remote names still materialize
/// as distinct files with their faithful `logical_path` preserved in the
/// manifest.
///
/// Rejects `.`/`..`/absolute-looking components up front (path safety -
/// containment is re-verified by [`join_contained`] before any write).
pub fn encode_physical_path(
    logical_path: &str,
    remote_id: &str,
    manifest: &Manifest,
) -> Result<String> {
    let components: Vec<&str> = logical_path.split('/').filter(|c| !c.is_empty()).collect();
    if components.is_empty() {
        anyhow::bail!("remote path `{logical_path}` has no path components");
    }
    if components.iter().any(|c| *c == "." || *c == "..") {
        anyhow::bail!("remote path `{logical_path}` contains a traversal component");
    }

    let sanitized: Vec<String> = components
        .iter()
        .map(|c| sanitize_path_component(c))
        .collect();
    let mut candidate = sanitized.join("/");
    let candidate_key = fold_key(&candidate);

    let collides = manifest
        .entries()
        .any(|e| e.remote_id != remote_id && fold_key(&e.physical_path) == candidate_key);
    if collides {
        let suffix = short_hash(remote_id);
        candidate = append_disambiguation_suffix(&candidate, &suffix);
    }

    Ok(candidate)
}

/// Insert `-<suffix>` before the final component's extension (or at the end
/// when there is none).
fn append_disambiguation_suffix(path: &str, suffix: &str) -> String {
    let (parent, last) = match path.rsplit_once('/') {
        Some((p, l)) => (Some(p), l),
        None => (None, path),
    };
    let disambiguated = match last.rsplit_once('.') {
        // Keep a leading-dot dotfile's name intact rather than treating the
        // dot as an extension separator.
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}-{suffix}.{ext}"),
        _ => format!("{last}-{suffix}"),
    };
    match parent {
        Some(p) => format!("{p}/{disambiguated}"),
        None => disambiguated,
    }
}

fn short_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(&hasher.finalize()[..4])
}

/// Join `physical_path` onto `root`, re-verifying containment: every
/// component must be `Normal` (no `..`, no root/prefix components). Defense
/// in depth against a future encoder regression - [`encode_physical_path`]
/// already rejects traversal at the logical-path level.
pub fn join_contained(root: &Path, physical_path: &str) -> Result<PathBuf> {
    for component in Path::new(physical_path).components() {
        match component {
            Component::Normal(_) => {}
            other => {
                anyhow::bail!("physical path `{physical_path}` has disallowed component {other:?}")
            }
        }
    }
    Ok(root.join(physical_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(remote_id: &str, logical: &str, physical: &str) -> ManifestEntry {
        ManifestEntry {
            remote_id: remote_id.to_string(),
            remote_version: "v1".to_string(),
            logical_path: logical.to_string(),
            physical_path: physical.to_string(),
            export_format: None,
            content_hash: Some("hash".to_string()),
            state: EntryState::Materialized,
        }
    }

    #[test]
    fn state_invariant_holds_and_is_checkable() {
        let materialized = entry("a", "a.txt", "a.txt");
        assert!(materialized.is_state_consistent());

        let skipped = ManifestEntry {
            content_hash: None,
            state: EntryState::Skipped {
                reason: "excluded".to_string(),
            },
            ..entry("b", "b.txt", "b.txt")
        };
        assert!(skipped.is_state_consistent());

        let inconsistent = ManifestEntry {
            content_hash: Some("hash".to_string()),
            state: EntryState::Skipped {
                reason: "excluded".to_string(),
            },
            ..entry("c", "c.txt", "c.txt")
        };
        assert!(!inconsistent.is_state_consistent());
    }

    #[test]
    fn manifest_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("manifest.json");
        let mut manifest = Manifest::new();
        manifest.upsert(entry("a", "a.txt", "a.txt"));
        manifest.save(&path).unwrap();

        let reloaded = Manifest::open(&path).unwrap();
        assert_eq!(reloaded.entries().count(), 1);
        assert_eq!(reloaded.get("a").unwrap().logical_path, "a.txt");
    }

    #[test]
    fn missing_manifest_file_opens_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("nope.json");
        let manifest = Manifest::open(&path).unwrap();
        assert_eq!(manifest.entries().count(), 0);
    }

    #[test]
    fn upsert_replaces_existing_remote_id() {
        let mut manifest = Manifest::new();
        manifest.upsert(entry("a", "a.txt", "a.txt"));
        manifest.upsert(entry("a", "renamed.txt", "renamed.txt"));
        assert_eq!(manifest.entries().count(), 1);
        assert_eq!(manifest.get("a").unwrap().logical_path, "renamed.txt");
    }

    #[test]
    fn remove_by_remote_id_returns_and_drops() {
        let mut manifest = Manifest::new();
        manifest.upsert(entry("a", "a.txt", "a.txt"));
        let removed = manifest.remove_by_remote_id("a").unwrap();
        assert_eq!(removed.remote_id, "a");
        assert!(manifest.get("a").is_none());
        assert!(manifest.remove_by_remote_id("a").is_none());
    }

    #[test]
    fn case_fold_collision_materializes_distinct_physical_paths() {
        let mut manifest = Manifest::new();
        let first = encode_physical_path("Report.txt", "remote-a", &manifest).unwrap();
        manifest.upsert(ManifestEntry {
            physical_path: first.clone(),
            ..entry("remote-a", "Report.txt", &first)
        });

        // Same case-folded name, different remote id - collides on APFS.
        let second = encode_physical_path("report.txt", "remote-b", &manifest).unwrap();
        assert_ne!(first, second);
        assert!(second.starts_with("report-") || second.contains("report-"));
    }

    #[test]
    fn nfc_nfd_collision_materializes_distinct_physical_paths() {
        // "cafe" + combining acute (NFD) vs precomposed "café" (NFC) fold to
        // the same key.
        let nfd_name = "cafe\u{0301}.txt";
        let nfc_name = "café.txt";
        assert_eq!(fold_key(nfd_name), fold_key(nfc_name));

        let mut manifest = Manifest::new();
        let first = encode_physical_path(nfd_name, "remote-a", &manifest).unwrap();
        manifest.upsert(ManifestEntry {
            physical_path: first.clone(),
            ..entry("remote-a", nfd_name, &first)
        });
        let second = encode_physical_path(nfc_name, "remote-b", &manifest).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn same_remote_id_does_not_self_collide_on_re_encode() {
        let mut manifest = Manifest::new();
        let first = encode_physical_path("Report.txt", "remote-a", &manifest).unwrap();
        manifest.upsert(ManifestEntry {
            physical_path: first.clone(),
            ..entry("remote-a", "Report.txt", &first)
        });
        // Re-encoding the SAME remote id (e.g. re-processing an unchanged
        // entry) must not treat its own prior physical path as a collision.
        let again = encode_physical_path("Report.txt", "remote-a", &manifest).unwrap();
        assert_eq!(first, again);
    }

    #[test]
    fn encode_rejects_traversal_components() {
        let manifest = Manifest::new();
        assert!(encode_physical_path("../etc/passwd", "r", &manifest).is_err());
        assert!(encode_physical_path("a/../b", "r", &manifest).is_err());
    }

    #[test]
    fn join_contained_rejects_escaping_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(join_contained(&root, "safe/name.txt").is_ok());
        assert!(join_contained(&root, "../escape.txt").is_err());
    }

    #[test]
    fn sanitize_replaces_separators_and_control_chars() {
        assert_eq!(sanitize_path_component("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_path_component(""), "_");
        assert_eq!(sanitize_path_component("  padded  "), "padded");
    }
}
