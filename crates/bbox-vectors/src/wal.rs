use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn dirty_wals() -> &'static Mutex<HashSet<PathBuf>> {
    static DIRTY: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    DIRTY.get_or_init(|| Mutex::new(HashSet::new()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalRecord {
    pub entity_id: String,
    pub content_hash: String,
    pub model: String,
    pub dims: usize,
    pub vector: Vec<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upserted_at: Option<String>,
    pub deleted_at: Option<String>,
    pub route: String,
}

impl WalRecord {
    pub fn upsert(route: &str, entity_id: &str, content_hash: &str, vector: Vec<f32>) -> Self {
        Self {
            entity_id: entity_id.to_string(),
            content_hash: content_hash.to_string(),
            model: route.to_string(),
            dims: vector.len(),
            vector,
            upserted_at: Some(chrono::Utc::now().to_rfc3339()),
            deleted_at: None,
            route: route.to_string(),
        }
    }

    pub fn delete(route: &str, entity_id: &str) -> Self {
        Self {
            entity_id: entity_id.to_string(),
            content_hash: String::new(),
            model: route.to_string(),
            dims: 0,
            vector: Vec::new(),
            upserted_at: None,
            deleted_at: Some(chrono::Utc::now().to_rfc3339()),
            route: route.to_string(),
        }
    }
}

pub fn append(path: &Path, record: &WalRecord) -> Result<()> {
    append_many(path, std::slice::from_ref(record))
}

/// Append a batch to the WAL. Buffers writes and does NOT call fsync —
/// durability is checkpointed by `sync(path)` from the periodic flusher
/// (every FLUSH_INTERVAL_SECS) and by graceful shutdown.
///
/// Crash window: up to FLUSH_INTERVAL_SECS of writes may be lost on
/// power loss. For Voyage embeddings this is acceptable — chunks are
/// rederivable from source via re-embed; the user re-runs `bbox_reembed`
/// after a known crash and the dedup check skips already-embedded
/// records on repopulate.
///
/// Pre-fix: per-batch fsync forced ~1.5 MB to disk synchronously every
/// 1-2 seconds during heavy ingest, combined with the 12-KB JSON
/// per-record format = sustained disk-write pressure that kept the
/// page cache flushed and triggered tantivy mmap re-paging on next
/// access. This was the dominant disk-I/O source identified in the
/// post-mortem of the agentic-corpus backfill.
pub fn append_many(path: &Path, records: &[WalRecord]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating vector WAL dir {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening vector WAL {}", path.display()))?;
    let mut writer = BufWriter::with_capacity(64 * 1024, file);
    for record in records {
        serde_json::to_writer(&mut writer, record).context("serializing vector WAL record")?;
        writer
            .write_all(b"\n")
            .context("writing vector WAL newline")?;
    }
    writer.flush().context("flushing vector WAL buffer")?;
    // Mark dirty for the next checkpoint sync. The sync happens on the
    // periodic flusher tick (or on shutdown) — see `sync_pending` /
    // `sync_path` below.
    dirty_wals()
        .lock()
        .expect("WAL dirty-set lock poisoned")
        .insert(path.to_path_buf());
    Ok(())
}

/// fsync a single WAL path immediately. Used by the periodic flusher
/// to checkpoint specific partitions.
pub fn sync_path(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("opening vector WAL for sync {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("fsync vector WAL {}", path.display()))?;
    dirty_wals()
        .lock()
        .expect("WAL dirty-set lock poisoned")
        .remove(path);
    Ok(())
}

/// fsync every WAL with pending writes. Called from the periodic
/// flusher and from graceful shutdown. Returns the number of WALs
/// sync'd so callers can log a metric.
pub fn sync_pending() -> Result<usize> {
    let paths: Vec<PathBuf> = {
        let mut guard = dirty_wals().lock().expect("WAL dirty-set lock poisoned");
        let paths = guard.iter().cloned().collect();
        guard.clear();
        paths
    };
    let count = paths.len();
    for path in paths {
        if let Err(err) = sync_path(&path) {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "vector WAL sync failed; will retry next checkpoint"
            );
            // Re-mark as dirty so the next checkpoint retries.
            dirty_wals()
                .lock()
                .expect("WAL dirty-set lock poisoned")
                .insert(path);
        }
    }
    Ok(count)
}

pub fn read_all(path: &Path) -> Result<Vec<WalRecord>> {
    let mut records = Vec::new();
    for_each(path, |record| {
        records.push(record);
        Ok(())
    })?;
    Ok(records)
}

pub fn for_each(path: &Path, mut f: impl FnMut(WalRecord) -> Result<()>) -> Result<()> {
    for_each_from(path, 0, &mut f)
}

pub fn for_each_from(
    path: &Path,
    offset: u64,
    mut f: impl FnMut(WalRecord) -> Result<()>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut file =
        fs::File::open(path).with_context(|| format!("opening vector WAL {}", path.display()))?;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("seeking vector WAL {} to byte {offset}", path.display()))?;
    }
    let reader = BufReader::new(file);
    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading vector WAL line {}", idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str(&line)
            .with_context(|| format!("parsing vector WAL line {}", idx + 1))?;
        f(record)?;
    }
    Ok(())
}

pub fn rewrite(path: &Path, records: impl IntoIterator<Item = WalRecord>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating vector WAL dir {}", parent.display()))?;
    }
    let tmp_path = path.with_extension("wal.tmp");
    // Clean up any stale temp file from a prior failed compaction.
    let _ = fs::remove_file(&tmp_path);
    let result = (|| -> Result<()> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
            .with_context(|| format!("opening vector WAL temp {}", tmp_path.display()))?;
        let mut writer = BufWriter::with_capacity(64 * 1024, file);
        for record in records.into_iter() {
            serde_json::to_writer(&mut writer, &record).context("serializing vector WAL record")?;
            writer
                .write_all(b"\n")
                .context("writing vector WAL newline")?;
        }
        writer.flush().context("flushing vector WAL temp buffer")?;
        writer
            .get_ref()
            .sync_data()
            .with_context(|| format!("fsync vector WAL temp {}", tmp_path.display()))?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(e) = fs::rename(&tmp_path, path).with_context(|| {
                format!(
                    "renaming compacted vector WAL {} to {}",
                    tmp_path.display(),
                    path.display()
                )
            }) {
                let _ = fs::remove_file(&tmp_path);
                return Err(e);
            }
            if let Some(parent) = path.parent() {
                fs::File::open(parent)
                    .with_context(|| format!("opening vector WAL parent {}", parent.display()))?
                    .sync_data()
                    .with_context(|| format!("fsync vector WAL parent {}", parent.display()))?;
            }
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
    }
    dirty_wals()
        .lock()
        .expect("WAL dirty-set lock poisoned")
        .remove(path);
    Ok(())
}
