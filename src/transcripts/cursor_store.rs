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
pub(crate) struct TranscriptCursorStore {
    path: PathBuf,
    data: CursorStoreFile,
}

impl TranscriptCursorStore {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            data: CursorStoreFile::default(),
        }
    }

    pub(crate) fn default_for_provider(provider: &str) -> Self {
        Self::new(Self::default_path_for_provider(provider))
    }

    pub(crate) fn default_path_for_provider(provider: &str) -> PathBuf {
        let base = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("state")
            .join("blackbox")
            .join("read-cursors");
        base.join(format!("{provider}.json"))
    }

    pub(crate) fn load(path: impl Into<PathBuf>) -> Result<Self, String> {
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

    pub(crate) fn get(
        &self,
        session_id: &str,
        location: &TranscriptLocation,
    ) -> Option<&TranscriptCursor> {
        let record = self.data.sessions.get(session_id)?;
        (record.location_fingerprint == location_fingerprint(location)).then_some(&record.cursor)
    }

    pub(crate) fn set(
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

    pub(crate) fn save(&self) -> Result<(), String> {
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

pub(crate) fn location_fingerprint(location: &TranscriptLocation) -> String {
    let mut h = DefaultHasher::new();
    location.provider.hash(&mut h);
    location.storage.hash(&mut h);
    canonical_path(&location.path).hash(&mut h);
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
    use tempfile::tempdir;

    use crate::orchestration::providers::Provider;

    use super::*;
    use crate::transcripts::types::TranscriptStorage;

    fn location(path: PathBuf) -> TranscriptLocation {
        TranscriptLocation {
            provider: Provider::Claude,
            storage: TranscriptStorage::JsonlFile,
            path,
            account: Some("claude".into()),
            session_id: Some("s1".into()),
            project: None,
            cwd: None,
            is_subagent: false,
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
    fn corrupt_cursor_file_is_recoverable_error() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("claude.json");
        fs::write(&store_path, "{not-json").unwrap();

        let err = TranscriptCursorStore::load(&store_path).unwrap_err();
        assert!(err.contains("cursor store parse"));
    }
}
