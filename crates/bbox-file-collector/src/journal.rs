//! The producer-side manifest journal (design section 10).
//!
//! This is the salvaged `ManifestEntry` from the predecessor design, demoted
//! from durable daemon state to PRODUCER WORKING STATE. It is what a bare
//! cursor cannot be: it maps deletion tombstones back to logical paths,
//! reconciles renames instead of delete-plus-refetch, lets a full walk find
//! orphans, and prevents re-export of unchanged native documents.
//!
//! Losing it entirely costs one full re-enumeration and re-export pass, not
//! data, because the remote store is the durable backlog. The satellite has no
//! spool, exactly as the code collector has none.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::connector::{CheckpointSet, RemoteEntry};

const JOURNAL_VERSION: u32 = 1;

/// What the journal knows about one remote entry's bytes.
///
/// The design states the invariant as prose ("`content_hash` and `size` are
/// `Some` exactly when `state = published`"). Modelling it as an enum makes
/// the invariant STRUCTURAL rather than checked: there is no representable
/// value with a hash and a skip reason, or a published row with no size, so
/// no validator has to be remembered and no sentinel can drift.
// No `deny_unknown_fields`: this enum is FLATTENED into `JournalEntry`, so its
// deserializer is handed the residual key set rather than a self-contained
// map. Denying unknown fields there would make it reject its own container's
// siblings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JournalState {
    Published { content_hash: String, size: u64 },
    Skipped { reason: String },
    Pending,
}

impl JournalState {
    pub fn published(&self) -> Option<(&str, u64)> {
        match self {
            Self::Published { content_hash, size } => Some((content_hash.as_str(), *size)),
            _ => None,
        }
    }
}

// No `deny_unknown_fields` here, and that is a serde constraint rather than a
// looser contract: `#[serde(flatten)]` and `deny_unknown_fields` cannot be
// combined. The flattened `JournalState` is what makes the published-implies-
// hash invariant structural, which is worth more than strictness on a
// producer-local working file that a bad parse already degrades to a full
// pass.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalEntry {
    /// Connector-stable identity: the journal's key, and the reason a rename
    /// is reconcilable at all.
    pub remote_id: String,
    /// etag / revision / changestamp. Re-export of a native document is gated
    /// on THIS, which is what makes "an unchanged native document is never
    /// re-exported" true.
    pub remote_version: String,
    pub logical_path: String,
    /// The raw remote name components this row's logical path was derived
    /// from. Retained so collision assignment can be RECOMPUTED over the
    /// union of the journal and an incremental batch: without it, a delta
    /// carrying one member of a collision group could not see the other
    /// member and would un-suffix a path that must stay suffixed.
    #[serde(default)]
    pub name_path: Vec<String>,
    /// The FAITHFUL remote name, retained for producer-side status output
    /// even where the logical path had to be sanitized or suffixed.
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(flatten)]
    pub state: JournalState,
}

/// Producer working state for one connector source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub version: u32,
    /// Incremented on every full re-enumeration.
    pub cursor_epoch: u64,
    /// The named checkpoint set as of the last successful observation.
    #[serde(default)]
    pub checkpoints: BTreeMap<String, String>,
    /// Keyed by `remote_id`.
    #[serde(default)]
    pub entries: BTreeMap<String, JournalEntry>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            version: JOURNAL_VERSION,
            cursor_epoch: 0,
            checkpoints: BTreeMap::new(),
            entries: BTreeMap::new(),
        }
    }
}

impl Journal {
    /// Load a journal, or start an empty one.
    ///
    /// A missing journal is NOT an error: it costs one full pass. A CORRUPT
    /// journal is also not an error for the same reason, but it is loud,
    /// because a journal that silently resets every cycle would look exactly
    /// like a connector that re-exports everything forever.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).context(format!("reading {}", path.display())),
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(journal) if journal.version == JOURNAL_VERSION => Ok(journal),
            Ok(journal) => {
                tracing::warn!(
                    version = journal.version,
                    expected = JOURNAL_VERSION,
                    path = %path.display(),
                    "connector journal version is unreadable; falling back to a full \
                     re-enumeration pass (working state only, no data is lost)"
                );
                Ok(Self::default())
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    path = %path.display(),
                    "connector journal is unreadable; falling back to a full re-enumeration \
                     pass (working state only, no data is lost)"
                );
                Ok(Self::default())
            }
        }
    }

    /// Durably replace the journal: write a sibling temporary, fsync it,
    /// rename over the destination, then fsync the parent.
    ///
    /// An in-place rewrite could leave a truncated journal, and a truncated
    /// journal read as "nothing known" is a silent full re-export of the whole
    /// scope on the next cycle.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let temporary = temporary_sibling(path);
        let bytes = serde_json::to_vec_pretty(self).context("encoding connector journal")?;
        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&temporary)
                .with_context(|| format!("creating {}", temporary.display()))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, path)
            .with_context(|| format!("publishing {}", path.display()))?;
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    pub fn checkpoint_set(&self) -> Result<CheckpointSet> {
        let mut set = CheckpointSet::full_enumeration();
        for (name, token) in &self.checkpoints {
            set.insert(name.clone(), token.clone())?;
        }
        Ok(set)
    }

    pub fn record_checkpoints(&mut self, checkpoints: &CheckpointSet) {
        self.checkpoints = checkpoints
            .iter()
            .map(|(name, token)| (name.to_string(), token.to_string()))
            .collect();
    }

    /// Discard every checkpoint and count a full re-enumeration.
    ///
    /// The epoch increment is the operator's signal: repeated increments mean
    /// a real problem (revoked consent, tenant policy, a connector bug), so it
    /// is durable rather than in-memory.
    pub fn invalidate_checkpoints(&mut self) -> u64 {
        self.checkpoints.clear();
        self.cursor_epoch = self.cursor_epoch.saturating_add(1);
        self.cursor_epoch
    }

    pub fn get(&self, remote_id: &str) -> Option<&JournalEntry> {
        self.entries.get(remote_id)
    }

    pub fn upsert(&mut self, entry: JournalEntry) {
        self.entries.insert(entry.remote_id.clone(), entry);
    }

    pub fn remove(&mut self, remote_id: &str) -> Option<JournalEntry> {
        self.entries.remove(remote_id)
    }

    /// True when this entry's bytes must be acquired again.
    ///
    /// False exactly when the journal already holds published bytes for this
    /// `remote_id` at this `remote_version`. That single condition is what
    /// makes "incremental publication fetches and exports only changed
    /// entries" and "an unchanged native document is never re-exported" the
    /// same rule rather than two.
    pub fn needs_acquire(&self, entry: &RemoteEntry) -> bool {
        match self.entries.get(&entry.remote_id) {
            Some(known) => {
                known.remote_version != entry.remote_version || known.state.published().is_none()
            }
            None => true,
        }
    }

    /// Journal rows the remote no longer has.
    ///
    /// ONLY sound against a complete enumeration, which is why the caller has
    /// to pass that fact explicitly rather than this reading a flag it could
    /// get wrong: treating a delta's `seen` set as complete would delete every
    /// unchanged document from the corpus on the first incremental cycle.
    pub fn orphans(&self, seen: &BTreeSet<String>, enumeration_was_complete: bool) -> Vec<String> {
        if !enumeration_was_complete {
            return Vec::new();
        }
        self.entries
            .keys()
            .filter(|remote_id| !seen.contains(*remote_id))
            .cloned()
            .collect()
    }

    /// Every published row, as wire manifest entries, sorted by logical path.
    pub fn manifest(&self) -> Result<Vec<bbox_file_source::FileManifestEntryV1>> {
        let mut entries: Vec<bbox_file_source::FileManifestEntryV1> = self
            .entries
            .values()
            .filter_map(|entry| {
                let (content_hash, size) = entry.state.published()?;
                Some(bbox_file_source::FileManifestEntryV1 {
                    logical_path: entry.logical_path.clone(),
                    content_sha256: content_hash.to_string(),
                    size,
                    remote_id: entry.remote_id.clone(),
                    remote_version: entry.remote_version.clone(),
                    remote_url: entry.remote_url.clone(),
                })
            })
            .collect();
        entries.sort_by(|left, right| {
            left.logical_path
                .as_bytes()
                .cmp(right.logical_path.as_bytes())
        });
        // A duplicate here means logical-path assignment failed to
        // disambiguate a collision, which would publish one document over
        // another. Refuse rather than ship a manifest the server will reject
        // with less context than we have.
        for pair in entries.windows(2) {
            if pair[0].logical_path == pair[1].logical_path {
                bail!(
                    "journal holds two published entries at logical path {} ({} and {}); \
                     collision assignment failed",
                    pair[0].logical_path,
                    pair[0].remote_id,
                    pair[1].remote_id
                );
            }
        }
        Ok(entries)
    }

    pub fn published_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.state.published().is_some())
            .count()
    }
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::RemoteEntryKind;

    fn remote(remote_id: &str, version: &str) -> RemoteEntry {
        RemoteEntry {
            remote_id: remote_id.into(),
            name_path: vec!["notes.md".into()],
            kind: RemoteEntryKind::File,
            remote_version: version.into(),
            size: Some(4),
            media_type: None,
            remote_url: None,
            deleted: false,
        }
    }

    fn published(remote_id: &str, version: &str, path: &str) -> JournalEntry {
        JournalEntry {
            remote_id: remote_id.into(),
            remote_version: version.into(),
            logical_path: path.into(),
            name_path: vec![path.into()],
            display_name: path.into(),
            export_format: None,
            remote_url: Some(format!("https://fixture.example/d/{remote_id}")),
            state: JournalState::Published {
                content_hash: "a".repeat(64),
                size: 4,
            },
        }
    }

    #[test]
    fn an_unchanged_entry_is_never_reacquired() {
        let mut journal = Journal::default();
        assert!(
            journal.needs_acquire(&remote("r1", "v1")),
            "an unknown entry must be acquired"
        );
        journal.upsert(published("r1", "v1", "notes.md"));
        assert!(
            !journal.needs_acquire(&remote("r1", "v1")),
            "an unchanged entry is never re-fetched or re-exported"
        );
        assert!(
            journal.needs_acquire(&remote("r1", "v2")),
            "a moved remote_version must be reacquired"
        );
    }

    #[test]
    fn a_pending_or_skipped_row_is_reacquired() {
        let mut journal = Journal::default();
        let mut pending = published("r1", "v1", "notes.md");
        pending.state = JournalState::Pending;
        journal.upsert(pending);
        assert!(journal.needs_acquire(&remote("r1", "v1")));

        let mut skipped = published("r1", "v1", "notes.md");
        skipped.state = JournalState::Skipped {
            reason: "oversize".into(),
        };
        journal.upsert(skipped);
        assert!(journal.needs_acquire(&remote("r1", "v1")));
    }

    #[test]
    fn a_rename_reconciles_in_place_and_costs_no_upload() {
        let mut journal = Journal::default();
        journal.upsert(published("r1", "v1", "Ops/old.md"));
        let hash_before = journal
            .get("r1")
            .unwrap()
            .state
            .published()
            .unwrap()
            .0
            .to_string();

        // Same remote_id, new logical path: the journal row moves, the bytes
        // do not. On the wire this is a moved manifest path whose blob the
        // server already caches.
        let mut renamed = journal.get("r1").unwrap().clone();
        renamed.logical_path = "Ops/new.md".into();
        journal.upsert(renamed);

        assert_eq!(journal.entries.len(), 1, "a rename is not a second entry");
        let manifest = journal.manifest().unwrap();
        assert_eq!(manifest[0].logical_path, "Ops/new.md");
        assert_eq!(
            manifest[0].content_sha256, hash_before,
            "the content hash is unchanged, so the server needs no upload"
        );
    }

    #[test]
    fn orphans_are_only_derivable_from_a_complete_enumeration() {
        let mut journal = Journal::default();
        journal.upsert(published("r1", "v1", "a.md"));
        journal.upsert(published("r2", "v1", "b.md"));
        let seen = BTreeSet::from(["r1".to_string()]);

        assert_eq!(
            journal.orphans(&seen, true),
            vec!["r2".to_string()],
            "a complete walk finds the orphan"
        );
        assert!(
            journal.orphans(&seen, false).is_empty(),
            "a DELTA's seen set says nothing about absence; treating it as \
             complete would delete every unchanged document from the corpus"
        );
    }

    #[test]
    fn the_manifest_is_ordered_published_only_and_carries_remote_metadata() {
        let mut journal = Journal::default();
        journal.upsert(published("r2", "v1", "b.md"));
        journal.upsert(published("r1", "v1", "a.md"));
        let mut skipped = published("r3", "v1", "c.md");
        skipped.state = JournalState::Skipped {
            reason: "oversize".into(),
        };
        journal.upsert(skipped);

        let manifest = journal.manifest().unwrap();
        assert_eq!(manifest.len(), 2, "skipped rows never reach the wire");
        assert_eq!(manifest[0].logical_path, "a.md");
        assert_eq!(manifest[1].logical_path, "b.md");
        assert_eq!(manifest[0].remote_id, "r1");
        assert_eq!(manifest[0].remote_version, "v1");
        assert!(manifest[0].remote_url.is_some());
        bbox_file_source::validate_manifest(
            &manifest,
            bbox_file_source::DEFAULT_MAX_MANIFEST_FILES,
            bbox_file_source::DEFAULT_MAX_MANIFEST_LOGICAL_BYTES,
        )
        .unwrap();
    }

    #[test]
    fn a_collision_that_escaped_assignment_refuses_the_manifest() {
        let mut journal = Journal::default();
        journal.upsert(published("r1", "v1", "same.md"));
        journal.upsert(published("r2", "v1", "same.md"));
        let error = journal.manifest().unwrap_err();
        assert!(error.to_string().contains("collision assignment failed"));
    }

    #[test]
    fn invalidation_increments_a_durable_epoch_and_clears_every_checkpoint() {
        let mut journal = Journal::default();
        let mut set = CheckpointSet::full_enumeration();
        set.insert("root", "token-a").unwrap();
        journal.record_checkpoints(&set);
        assert_eq!(journal.cursor_epoch, 0);

        assert_eq!(journal.invalidate_checkpoints(), 1);
        assert!(journal.checkpoints.is_empty());
        assert!(journal.checkpoint_set().unwrap().is_full_enumeration());
        assert_eq!(journal.invalidate_checkpoints(), 2);
    }

    #[test]
    fn a_journal_round_trips_durably_and_survives_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("nested").join("journal.json");

        assert_eq!(
            Journal::load(&path).unwrap(),
            Journal::default(),
            "a missing journal costs one full pass, not an error"
        );

        let mut journal = Journal::default();
        journal.upsert(published("r1", "v1", "a.md"));
        let mut set = CheckpointSet::full_enumeration();
        set.insert("root", "token-a").unwrap();
        journal.record_checkpoints(&set);
        journal.save(&path).unwrap();

        let reloaded = Journal::load(&path).unwrap();
        assert_eq!(reloaded, journal);
        assert_eq!(
            reloaded.checkpoint_set().unwrap().get("root"),
            Some("token-a")
        );
        assert!(
            !path.parent().unwrap().read_dir().unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
            "the durable write leaves no staging sibling behind"
        );
    }

    #[test]
    fn a_corrupt_journal_degrades_to_a_full_pass_rather_than_refusing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("journal.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(Journal::load(&path).unwrap(), Journal::default());
    }
}
