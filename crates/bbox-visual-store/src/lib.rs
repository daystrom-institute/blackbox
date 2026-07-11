//! Content-hash-addressed store for visual (image/video) payload bytes,
//! kept OUTSIDE tantivy per the multimodal embedding design
//! (design/corpus/agentic-corpus/multimodal-embedding-routing.md,
//! "Multimodal Chunk Model"). Chunks and embed requests carry a
//! [`VisualPayloadRef`] (content hash + media type + byte length), never
//! inline bytes; the actual bytes are loaded from disk only at the point an
//! embed request is built (`crates/bbox-embed`).
//!
//! ## Layout
//!
//! `<root>/<hash[0:2]>/<hash>.blob` - sharded by hash prefix so one
//! directory never accumulates every payload the corpus has ever seen.
//! Writes are dedup-by-hash: the same image bytes recurring across a corpus
//! (shared screenshots, a re-exported PDF's figure, the same slide image
//! reused across decks) are written once.
//!
//! ## Garbage collection (deferred)
//!
//! There is currently NO reference-counting or mark-sweep GC for this
//! store: a blob written for a chunk that is later deleted, or whose
//! source file is removed or re-chunked, stays on disk indefinitely. This
//! mirrors the vector store's own residue story (orphaned partitions need
//! explicit `bbox_embed_partitions` pruning) but is not wired to any
//! pruning tool yet. Deferred rather than half-built: correct GC needs
//! either (a) a live refcount bumped/decremented by every chunker/reindex
//! pass that references a hash, or (b) a mark-sweep pass that walks every
//! current chunk's [`VisualPayloadRef`] (round-tripped through
//! [`VisualPayloadRef::encode`]/[`VisualPayloadRef::decode`] on the
//! chunk's `symbol` field - see `crates/bbox-chunker/src/ximg.rs` and
//! `src/embed_runtime.rs`) and deletes on-disk blobs with no surviving
//! referrer. Either is a self-contained follow-up; file it as a gap rather
//! than bolting a partial version on here.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
#[cfg(any(test, feature = "test-support"))]
use parking_lot::{Mutex, MutexGuard, RwLock};
use sha2::{Digest, Sha256};

/// Content-hash-addressed reference to a visual payload. Small enough to
/// live inline on a `Chunk`/`EmbedRequest` (never the raw bytes) and to
/// round-trip through a text carrier field via [`encode`]/[`decode`].
///
/// [`encode`]: VisualPayloadRef::encode
/// [`decode`]: VisualPayloadRef::decode
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualPayloadRef {
    /// SHA-256 hex digest of the raw payload bytes; the store's lookup key.
    pub content_hash: String,
    /// MIME type, e.g. `image/png`.
    pub media_type: String,
    /// Byte length of the raw payload - enough to preflight provider size
    /// caps without re-reading the blob from disk.
    pub byte_len: u64,
}

const ENCODE_PREFIX: &str = "visual_ref:v1:";

impl VisualPayloadRef {
    /// Encode as a single-line string suitable for a text carrier field
    /// (this repo uses `Chunk::symbol` - see `ximg.rs`'s chunker and
    /// `embed_runtime.rs`'s backfill decode). Kept intentionally simple
    /// (colon-delimited, versioned prefix) rather than JSON: it rides an
    /// existing plain-text tantivy field, not a new schema column.
    pub fn encode(&self) -> String {
        format!(
            "{ENCODE_PREFIX}{}:{}:{}",
            self.content_hash, self.media_type, self.byte_len
        )
    }

    /// Inverse of [`encode`](Self::encode). `None` on anything that isn't
    /// exactly this format - callers treat that as "not a visual chunk"
    /// rather than an error, since the carrier field is shared with other
    /// chunk kinds' unrelated `symbol` usage.
    pub fn decode(value: &str) -> Option<Self> {
        let rest = value.strip_prefix(ENCODE_PREFIX)?;
        let mut parts = rest.splitn(3, ':');
        let content_hash = parts.next()?.to_string();
        let media_type = parts.next()?.to_string();
        let byte_len = parts.next()?.parse::<u64>().ok()?;
        if content_hash.is_empty() || media_type.is_empty() {
            return None;
        }
        Some(Self {
            content_hash,
            media_type,
            byte_len,
        })
    }
}

/// SHA-256 hex digest of `bytes` - the store's content-addressing hash.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub struct VisualPayloadStore {
    root: PathBuf,
}

impl VisualPayloadStore {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// On-disk path for a payload by content hash: `<root>/<hash[0:2]>/<hash>.blob`.
    pub fn path_for(&self, content_hash: &str) -> PathBuf {
        let shard = if content_hash.len() >= 2 {
            &content_hash[..2]
        } else {
            "xx"
        };
        self.root.join(shard).join(format!("{content_hash}.blob"))
    }

    /// Write `bytes` under its content hash and return the ref. A no-op
    /// (aside from the hash) when a blob with this hash already exists:
    /// payloads are immutable once written, so dedup is just an existence
    /// check, not a content comparison.
    ///
    /// Callers always run on an already-blocking-safe context: chunkers
    /// execute inside the `IndexWriterActor`'s dedicated thread or a
    /// `spawn_blocking` closure (never a bare tokio worker) - the same
    /// sanctioned idiom as `crates/bbox-corpus-index/src/index/git_history.rs`.
    #[allow(clippy::disallowed_methods)]
    pub fn put(&self, bytes: &[u8], media_type: &str) -> Result<VisualPayloadRef> {
        let content_hash = hash_bytes(bytes);
        let path = self.path_for(&content_hash);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating visual payload directory {}", parent.display())
                })?;
            }
            // Write-then-rename so a crash mid-write never leaves a
            // half-written blob visible under its final content-hash path.
            let tmp = path.with_extension("blob.tmp");
            std::fs::write(&tmp, bytes)
                .with_context(|| format!("writing visual payload {}", tmp.display()))?;
            std::fs::rename(&tmp, &path)
                .with_context(|| format!("finalizing visual payload {}", path.display()))?;
        }
        Ok(VisualPayloadRef {
            content_hash,
            media_type: media_type.to_string(),
            byte_len: bytes.len() as u64,
        })
    }
}

/// Default on-disk root: `<state_dir>/blackbox/visual-payloads`,
/// overridable via `BLACKBOX_VISUAL_PAYLOAD_DIR` (same convention as the
/// other `BLACKBOX_*_DIR`/`BLACKBOX_*_PATH` state overrides).
pub fn default_dir() -> PathBuf {
    if let Ok(path) = std::env::var("BLACKBOX_VISUAL_PAYLOAD_DIR")
        && !path.trim().is_empty()
    {
        return PathBuf::from(path);
    }
    dirs::state_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("state")
        })
        .join("blackbox")
        .join("visual-payloads")
}

static GLOBAL_STORE: OnceLock<Arc<VisualPayloadStore>> = OnceLock::new();

// Test-global-store plumbing, mirroring bbox-vectors::install_test_global
// exactly: gated on `test` (this crate's own tests) OR the `test-support`
// feature (downstream crates' tests, since cfg(test) doesn't cross crate
// boundaries).
#[cfg(any(test, feature = "test-support"))]
static TEST_GLOBAL_STORE: OnceLock<RwLock<Option<Arc<VisualPayloadStore>>>> = OnceLock::new();
#[cfg(any(test, feature = "test-support"))]
static TEST_GLOBAL_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
fn test_global_store() -> &'static RwLock<Option<Arc<VisualPayloadStore>>> {
    TEST_GLOBAL_STORE.get_or_init(|| RwLock::new(None))
}

#[cfg(any(test, feature = "test-support"))]
pub struct TestGlobalStoreGuard {
    previous: Option<Arc<VisualPayloadStore>>,
    _lock: MutexGuard<'static, ()>,
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestGlobalStoreGuard {
    fn drop(&mut self) {
        *test_global_store().write() = self.previous.take();
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn install_test_global(store: Arc<VisualPayloadStore>) -> TestGlobalStoreGuard {
    let lock = TEST_GLOBAL_STORE_LOCK.get_or_init(|| Mutex::new(())).lock();
    let mut slot = test_global_store().write();
    let previous = slot.replace(store);
    TestGlobalStoreGuard {
        previous,
        _lock: lock,
    }
}

/// Process-wide default store. Chunkers and the embed queue both go
/// through this rather than threading a store handle through trait
/// signatures that predate this crate (`SourceFormatChunker::chunk`).
pub fn global() -> Arc<VisualPayloadStore> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(store) = test_global_store().read().clone() {
        return store;
    }
    GLOBAL_STORE
        .get_or_init(|| Arc::new(VisualPayloadStore::open(default_dir())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_is_content_addressed_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = VisualPayloadStore::open(root.clone());
        let bytes = b"pretend png bytes";
        let first = store.put(bytes, "image/png").unwrap();
        let second = store.put(bytes, "image/png").unwrap();
        assert_eq!(first, second);
        assert_eq!(first.byte_len, bytes.len() as u64);
        let path = store.path_for(&first.content_hash);
        assert!(path.starts_with(&root));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn different_bytes_hash_to_different_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = VisualPayloadStore::open(dir.path().to_path_buf());
        let a = store.put(b"aaaa", "image/png").unwrap();
        let b = store.put(b"bbbb", "image/png").unwrap();
        assert_ne!(a.content_hash, b.content_hash);
    }

    #[test]
    fn encode_decode_round_trips() {
        let payload = VisualPayloadRef {
            content_hash: "abc123".into(),
            media_type: "image/jpeg".into(),
            byte_len: 4096,
        };
        let encoded = payload.encode();
        assert_eq!(VisualPayloadRef::decode(&encoded), Some(payload));
    }

    #[test]
    fn decode_rejects_foreign_symbol_values() {
        assert_eq!(VisualPayloadRef::decode("just a function name"), None);
        assert_eq!(VisualPayloadRef::decode("visual_ref:v1:onlyhash"), None);
        assert_eq!(
            VisualPayloadRef::decode("visual_ref:v1:hash:image/png:not-a-number"),
            None
        );
    }

    #[test]
    fn install_test_global_overrides_and_restores() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(VisualPayloadStore::open(dir.path().to_path_buf()));
        {
            let _guard = install_test_global(store.clone());
            assert_eq!(global().root(), store.root());
        }
    }
}
