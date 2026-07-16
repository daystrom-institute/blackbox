//! Host-local cursor sidecar.
//!
//! The corpus server is the cursor authority (its per-stream acknowledged byte
//! tail is the truth). This sidecar is a fast-resume cache: it lets the
//! collector skip a full empty-batch resync round-trip per stream on the happy
//! path and remembers the monotonic per-producer record cursor. It is keyed by
//! the SAME stream identity the server uses (`producer/source/account/
//! relative_path`) and stores an acknowledged byte tail per stream, so adopting
//! a server value after a resync is a direct assignment.
//!
//! Why not reuse `bbox_transcript_read::TranscriptCursorStore`? That store is
//! keyed by session id plus a location fingerprint and stores an opaque
//! `TranscriptCursor`; the shipper instead dedupes strictly by the server's
//! stream string and byte range, so a stream-keyed `u64` tail is the exact
//! shape the wire contract wants. Reusing the session-keyed store would force a
//! synthetic location per stream and risk a session-id map collision across
//! accounts. Server-authority semantics make this cache safe to rebuild.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const CURSOR_STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CursorFile {
    version: u32,
    /// Monotonic per-producer record cursor. The server does not parse it for
    /// increments but requires every record's `cursor` field to be nonempty.
    record_cursor: u64,
    /// Stream identity -> acknowledged byte tail.
    #[serde(default)]
    streams: BTreeMap<String, u64>,
}

impl Default for CursorFile {
    fn default() -> Self {
        Self {
            version: CURSOR_STORE_VERSION,
            record_cursor: 0,
            streams: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct CollectorCursors {
    path: PathBuf,
    data: CursorFile,
}

impl CollectorCursors {
    /// Load the sidecar, tolerating a missing file (fresh install). A corrupt
    /// or unversioned file is a hard error: the server can always re-seed via
    /// resync, but silently discarding cursors would re-ship the whole corpus.
    // Small synchronous sidecar loaded once at startup (I2 exception,
    // concurrency-model §5).
    #[allow(clippy::disallowed_methods)]
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self {
                path,
                data: CursorFile::default(),
            });
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("collector cursor read {}: {e}", path.display()))?;
        let data: CursorFile = serde_json::from_str(&raw)
            .map_err(|e| format!("collector cursor parse {}: {e}", path.display()))?;
        if data.version != CURSOR_STORE_VERSION {
            return Err(format!(
                "collector cursor {} has unsupported version {}",
                path.display(),
                data.version
            ));
        }
        Ok(Self { path, data })
    }

    /// Acknowledged byte tail for a stream (0 when unknown).
    pub fn tail(&self, stream: &str) -> u64 {
        self.data.streams.get(stream).copied().unwrap_or(0)
    }

    /// Advance the cached tail after the server confirmed it. Monotonic: a
    /// stale confirmation can never move a stream backwards.
    pub fn set_tail(&mut self, stream: &str, tail: u64) {
        let entry = self.data.streams.entry(stream.to_string()).or_insert(0);
        *entry = (*entry).max(tail);
    }

    /// Adopt the server's authoritative tail during a resync. Unlike
    /// [`set_tail`] this is an absolute assignment: the server value wins even
    /// if the local cache was somehow ahead (it should not be, but the server
    /// is authority).
    pub fn adopt_server_tail(&mut self, stream: &str, tail: u64) {
        self.data.streams.insert(stream.to_string(), tail);
    }

    /// Allocate the next monotonic record cursor value.
    pub fn next_record_cursor(&mut self) -> u64 {
        self.data.record_cursor = self.data.record_cursor.saturating_add(1);
        self.data.record_cursor
    }

    /// Atomically persist the sidecar (tmp write + rename).
    // Tiny atomic sidecar write after a confirmed receipt; the byte payload is
    // a handful of cursors, not on any latency-sensitive path (I2 exception,
    // concurrency-model §5).
    #[allow(clippy::disallowed_methods)]
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("collector cursor mkdir {}: {e}", parent.display()))?;
        }
        let raw = serde_json::to_vec_pretty(&self.data)
            .map_err(|e| format!("collector cursor encode {}: {e}", self.path.display()))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, raw)
            .map_err(|e| format!("collector cursor write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("collector cursor rename {}: {e}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trips_tails_and_record_cursor() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("cursors.json");
        let mut store = CollectorCursors::load(&path).unwrap();
        store.set_tail("collector:h/claude/claude/projects/x/s.jsonl", 128);
        assert_eq!(store.next_record_cursor(), 1);
        assert_eq!(store.next_record_cursor(), 2);
        store.save().unwrap();

        let reloaded = CollectorCursors::load(&path).unwrap();
        assert_eq!(
            reloaded.tail("collector:h/claude/claude/projects/x/s.jsonl"),
            128
        );
        // record cursor persisted; next allocation continues monotonically.
        let mut reloaded = reloaded;
        assert_eq!(reloaded.next_record_cursor(), 3);
    }

    #[test]
    fn set_tail_is_monotonic_but_adopt_is_absolute() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = CollectorCursors::load(root.join("c.json")).unwrap();
        store.set_tail("s", 100);
        store.set_tail("s", 50);
        assert_eq!(store.tail("s"), 100, "set_tail never regresses");
        store.adopt_server_tail("s", 40);
        assert_eq!(store.tail("s"), 40, "server authority wins on resync");
    }

    #[test]
    fn corrupt_file_is_recoverable_error() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("cursors.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(CollectorCursors::load(&path).unwrap_err().contains("parse"));
    }
}
