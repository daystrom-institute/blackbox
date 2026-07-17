//! `MountRecord` and its persisted store - a sibling of
//! `bbox-indexing::projects::ProjectStore`, same store discipline (whole-
//! file JSON, atomic write, `StoreSnapshot` for daemon-side
//! `StorePersister` wrapping in stage 2). No credential material may ever
//! appear on `MountRecord`, its store file, or logs derived from it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use bbox_stores::store_persister::StoreSnapshot;

use crate::driver::SyncSummary;
use crate::policy::MountPolicy;
use crate::types::ChangeCursor;

const MOUNT_STORE_VERSION: u32 = 1;

/// One configured mount: connector + remote scope + policy, materialized at
/// one local root and feeding one registered project.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MountRecord {
    /// Hash of connector kind + remote identity/scope - see
    /// [`compute_mount_id`]. Stable for the same (connector, scope) pair.
    pub mount_id: String,
    /// Config alias, e.g. "gdrive-personal" or "git-blackbox-fork". For the
    /// git connector this is operator-chosen, not derived from the URL.
    pub connector: String,
    /// Connector-shaped scope string (credential-redacted - see
    /// `git_connector::redact_scope`; never the raw operator-supplied URL
    /// when it carries userinfo).
    pub remote_scope: String,
    pub materialization_root: String,
    /// The registered project this mount feeds. Stage 1 leaves project
    /// auto-registration to the caller (daemon wiring, stage 2); this
    /// field is populated once that registration has happened.
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub cursor: Option<ChangeCursor>,
    #[serde(default)]
    pub policy: MountPolicy,
    pub created_at: String,
    #[serde(default)]
    pub last_sync: Option<SyncSummary>,
    /// Full error chain of the most recent FAILED sync, cleared on the next
    /// success. A failed sync does not update `last_sync`, so without this
    /// field a mount whose syncs always fail is indistinguishable from one
    /// that never synced - silence is never a verdict.
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Deterministic id for a (connector kind, remote scope) pair. `scope`
/// should already be credential-redacted - the id must not encode secret
/// material even indirectly (an id collision analysis on a leaked id string
/// should reveal nothing about credentials).
pub fn compute_mount_id(connector_kind: &str, scope: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(connector_kind.as_bytes());
    hasher.update(b":");
    hasher.update(scope.as_bytes());
    hex::encode(&hasher.finalize()[..4])
}

/// The on-disk shape of the mount store. Public only because
/// `StoreSnapshot::Snapshot` must be - construct/inspect a store through
/// `MountStore`, not this type directly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MountStoreFile {
    #[serde(default = "mount_store_version")]
    version: u32,
    mounts: Vec<MountRecord>,
}

fn mount_store_version() -> u32 {
    MOUNT_STORE_VERSION
}

#[derive(Debug, Clone)]
pub struct MountStore {
    file: MountStoreFile,
}

impl StoreSnapshot for MountStore {
    type Snapshot = MountStoreFile;

    fn snapshot(&self) -> Result<Self::Snapshot> {
        Ok(self.file.clone())
    }
}

impl MountStore {
    /// Synchronous storage boundary, matching `ProjectRegistry::open` -
    /// async callers resolve this on a blocking lane.
    #[allow(clippy::disallowed_methods)]
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self {
                file: MountStoreFile::default(),
            });
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let file: MountStoreFile =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Self { file })
    }

    /// Atomically persist the store (temp file + same-dir rename).
    #[allow(clippy::disallowed_methods)]
    pub fn save(&self, path: &Path) -> Result<()> {
        bbox_corpus_core::json_store::with_store_lock(path, || {
            bbox_corpus_core::json_store::atomic_write_json_locked(path, &self.file)
        })
    }

    pub fn list(&self) -> Vec<MountRecord> {
        self.file.mounts.clone()
    }

    pub fn get(&self, mount_id: &str) -> Option<&MountRecord> {
        self.file.mounts.iter().find(|m| m.mount_id == mount_id)
    }

    /// Insert or replace the record sharing `mount_id`.
    pub fn upsert(&mut self, record: MountRecord) {
        if let Some(existing) = self
            .file
            .mounts
            .iter_mut()
            .find(|m| m.mount_id == record.mount_id)
        {
            *existing = record;
        } else {
            self.file.mounts.push(record);
        }
    }

    /// Remove the record with `mount_id`, returning it if present.
    pub fn remove(&mut self, mount_id: &str) -> Option<MountRecord> {
        let idx = self
            .file
            .mounts
            .iter()
            .position(|m| m.mount_id == mount_id)?;
        Some(self.file.mounts.remove(idx))
    }
}

/// Default mount store path: `BLACKBOX_MOUNTS_PATH` override, else
/// `<state_dir>/blackbox-mounts.json` - sibling of
/// `bbox_util::util::blackbox_projects_path`.
pub fn default_mount_store_path(home: &Path) -> PathBuf {
    std::env::var("BLACKBOX_MOUNTS_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| bbox_util::util::blackbox_state_dir(home).join("blackbox-mounts.json"))
}

/// Default materialization root for a mount:
/// `<state_dir>/blackbox/mounts/<mount_id>/`.
pub fn default_materialization_root(home: &Path, mount_id: &str) -> PathBuf {
    bbox_util::util::blackbox_state_dir(home)
        .join("mounts")
        .join(mount_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(mount_id: &str) -> MountRecord {
        MountRecord {
            mount_id: mount_id.to_string(),
            connector: "git".to_string(),
            remote_scope: "https://example.invalid/repo.git".to_string(),
            materialization_root: "/tmp/mount".to_string(),
            project_id: None,
            cursor: None,
            policy: MountPolicy::default(),
            created_at: "2026-07-16T00:00:00Z".to_string(),
            last_sync: None,
            last_error: None,
        }
    }

    #[test]
    fn mount_id_is_deterministic_and_kind_scoped() {
        let a = compute_mount_id("git", "https://example.invalid/repo.git");
        let b = compute_mount_id("git", "https://example.invalid/repo.git");
        let c = compute_mount_id("gdrive", "https://example.invalid/repo.git");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn store_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("mounts.json");
        let mut store = MountStore::open(&path).unwrap();
        store.upsert(record("m1"));
        store.save(&path).unwrap();

        let reloaded = MountStore::open(&path).unwrap();
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(reloaded.get("m1").unwrap().connector, "git");
    }

    #[test]
    fn missing_store_file_opens_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("nope.json");
        assert!(MountStore::open(&path).unwrap().list().is_empty());
    }

    #[test]
    fn upsert_replaces_and_remove_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("mounts.json");
        let mut store = MountStore::open(&path).unwrap();
        store.upsert(record("m1"));
        let mut updated = record("m1");
        updated.connector = "gdrive".to_string();
        store.upsert(updated);
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.get("m1").unwrap().connector, "gdrive");

        let removed = store.remove("m1").unwrap();
        assert_eq!(removed.mount_id, "m1");
        assert!(store.get("m1").is_none());
    }

    #[test]
    fn no_credential_shaped_fields_on_mount_record() {
        // Guard against a future field addition smuggling secrets onto the
        // record: serialize a record and assert none of the house
        // secret-shaped keywords appear in the field names.
        let value = serde_json::to_value(record("m1")).unwrap();
        let obj = value.as_object().unwrap();
        for key in obj.keys() {
            let lower = key.to_lowercase();
            assert!(
                !lower.contains("token")
                    && !lower.contains("secret")
                    && !lower.contains("password")
                    && !lower.contains("credential"),
                "MountRecord field `{key}` looks credential-shaped"
            );
        }
    }
}
