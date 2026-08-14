use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::types::{TranscriptCursor, TranscriptLocation};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CursorStoreFile {
    version: u32,
    sessions: HashMap<String, CursorRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CursorRecord {
    location_fingerprint: String,
    cursor: TranscriptCursor,
    updated_at_ms: u64,
}

impl Default for CursorStoreFile {
    fn default() -> Self {
        Self {
            version: 1,
            sessions: HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct TranscriptCursorStore {
    path: PathBuf,
    data: CursorStoreFile,
}

impl TranscriptCursorStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            data: CursorStoreFile::default(),
        }
    }

    pub fn default_for_provider(provider: &str) -> Self {
        Self::new(Self::default_path_for_provider(provider))
    }

    pub fn default_path_for_provider(provider: &str) -> PathBuf {
        let base = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("state")
            .join("blackbox")
            .join("read-cursors");
        base.join(format!("{provider}.json"))
    }

    pub fn load(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self::new(path));
        }
        let raw = fs::read_to_string(&path)
            .map_err(|e| format!("cursor store read {}: {e}", path.display()))?;
        let data: CursorStoreFile = serde_json::from_str(&raw)
            .map_err(|e| format!("cursor store parse {}: {e}", path.display()))?;
        if data.version != 1 {
            return Err(format!(
                "cursor store {} has unsupported version {}",
                path.display(),
                data.version
            ));
        }
        Ok(Self { path, data })
    }

    pub fn get(
        &self,
        session_id: &str,
        location: &TranscriptLocation,
    ) -> Option<&TranscriptCursor> {
        let record = self.data.sessions.get(session_id)?;
        (record.location_fingerprint == location_fingerprint(location)).then_some(&record.cursor)
    }

    pub fn set(
        &mut self,
        session_id: impl Into<String>,
        location: &TranscriptLocation,
        cursor: TranscriptCursor,
    ) {
        self.data.sessions.insert(
            session_id.into(),
            CursorRecord {
                location_fingerprint: location_fingerprint(location),
                cursor,
                updated_at_ms: now_ms(),
            },
        );
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cursor store mkdir {}: {e}", parent.display()))?;
        }
        let raw = serde_json::to_vec_pretty(&self.data)
            .map_err(|e| format!("cursor store encode {}: {e}", self.path.display()))?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, raw).map_err(|e| format!("cursor store write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .map_err(|e| format!("cursor store rename {}: {e}", self.path.display()))?;
        Ok(())
    }
}

/// Fingerprint a location so a stored cursor is only ever replayed against
/// the location it was taken from.
///
/// The anchor is the location's [`TranscriptLocation::locator`], not its path.
/// For every file-backed location that IS the canonicalized path, unchanged.
/// For a store-backed location it is the record key (workspace and channel),
/// which is what makes a conversation cursor survive the landing store's root
/// moving: the journal's host path is an implementation detail of where the
/// daemon keeps its state dir, and fingerprinting it would silently invalidate
/// every cursor the first time that dir was relocated. Canonicalization is
/// applied only on the path arm, because there is nothing on a filesystem to
/// canonicalize a record key against.
pub fn location_fingerprint(location: &TranscriptLocation) -> String {
    let mut h = DefaultHasher::new();
    location.source.hash(&mut h);
    location.storage.hash(&mut h);
    match &location.logical_key {
        Some(key) => key.hash(&mut h),
        None => canonical_path(&location.path).hash(&mut h),
    }
    location.session_id.hash(&mut h);
    location.account.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::super::types::TranscriptSource;
    use tempfile::tempdir;

    use bro_core::Provider;

    use super::*;
    use crate::transcripts::types::TranscriptStorage;

    fn location(path: PathBuf) -> TranscriptLocation {
        TranscriptLocation {
            source: TranscriptSource::Harness(Provider::Glm),
            storage: TranscriptStorage::JsonlFile,
            path,
            account: Some("claude".into()),
            session_id: Some("s1".into()),
            project: None,
            cwd: None,
            is_subagent: false,
            logical_key: None,
        }
    }

    /// A store-backed location: no source-owned file, identity is the record
    /// key, and the journal path is incidental.
    fn landed_location(journal: PathBuf) -> TranscriptLocation {
        TranscriptLocation {
            source: TranscriptSource::Slack,
            storage: TranscriptStorage::LandedRecords,
            path: journal,
            account: Some("T0FIXTURE".into()),
            session_id: Some("C0CHANNEL".into()),
            project: None,
            cwd: None,
            is_subagent: false,
            logical_key: Some("slack:T0FIXTURE/C0CHANNEL".into()),
        }
    }

    #[test]
    fn cursor_store_round_trips_and_checks_location_fingerprint() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("claude.json");
        let loc = location(dir.path().join("s1.jsonl"));

        let mut store = TranscriptCursorStore::new(&store_path);
        store.set("s1", &loc, TranscriptCursor::byte_offset(42));
        store.save().unwrap();

        let loaded = TranscriptCursorStore::load(&store_path).unwrap();
        assert_eq!(
            loaded.get("s1", &loc),
            Some(&TranscriptCursor::byte_offset(42))
        );

        let other = location(dir.path().join("other.jsonl"));
        assert_eq!(loaded.get("s1", &other), None);
    }

    #[test]
    fn a_store_backed_location_fingerprints_on_its_record_key_not_its_path() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        // The same channel, whose landing store moved to a second state dir.
        // Nothing about the message identity changed, so the cursor must
        // still apply.
        let before = landed_location(root.join("state-a").join("journal.ndjson"));
        let after = landed_location(root.join("state-b").join("journal.ndjson"));
        assert_eq!(
            location_fingerprint(&before),
            location_fingerprint(&after),
            "a relocated landing store is not a different conversation"
        );

        // A different channel under the same store IS a different location.
        let mut other = before.clone();
        other.logical_key = Some("slack:T0FIXTURE/C0OTHER".into());
        other.session_id = Some("C0OTHER".into());
        assert_ne!(location_fingerprint(&before), location_fingerprint(&other));

        // And the path arm is untouched for file-backed locations.
        let file_a = location(root.join("s1.jsonl"));
        let file_b = location(root.join("s2.jsonl"));
        assert_ne!(
            location_fingerprint(&file_a),
            location_fingerprint(&file_b),
            "a file-backed location is still identified by its path"
        );
    }

    #[test]
    fn corrupt_cursor_file_is_recoverable_error() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("claude.json");
        fs::write(&store_path, "{not-json").unwrap();

        let err = TranscriptCursorStore::load(&store_path).unwrap_err();
        assert!(err.contains("cursor store parse"));
    }
}
