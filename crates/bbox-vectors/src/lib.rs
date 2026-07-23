#![allow(dead_code)] // E3 lands vector API; H1 wires search callers.

pub mod distance;
pub mod hnsw;
pub mod migration_inventory;
pub mod slab;
pub mod wal;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use parking_lot::RwLock;
#[cfg(any(test, feature = "test-support"))]
use parking_lot::{Mutex, MutexGuard};
use serde::{Deserialize, Serialize};

use self::hnsw::{HnswIndex, HnswMetrics, HnswOptions, SearchHit};
use self::slab::VectorSlab;
use self::wal::WalRecord;

const VECTOR_SCHEMA_VERSION: &str = "agentic-corpus-e3";
const VECTOR_SNAPSHOT_VERSION: &str = "agentic-corpus-e3-snapshot-v1";
const VECTOR_SNAPSHOT_MAGIC: &[u8; 16] = b"BBOXVSNAPv1\0\0\0\0\0";
const SNAPSHOT_FILE: &str = "snapshot.bin";
const SNAPSHOT_TMP_FILE: &str = "snapshot.bin.tmp";
const SNAPSHOT_MIN_RECORDS: usize = 100_000;

static GLOBAL_STORE: OnceLock<Arc<VectorStore>> = OnceLock::new();

// Test-global-store plumbing — gated on `test` (this crate's own tests) OR the
// `test-support` feature (downstream crates' tests, since cfg(test) doesn't cross
// crate boundaries). The read-path checks in global()/try_global()/etc. below use
// the same gate so an installed test store is actually consulted under the feature.
#[cfg(any(test, feature = "test-support"))]
static TEST_GLOBAL_STORE: OnceLock<RwLock<Option<Arc<VectorStore>>>> = OnceLock::new();
#[cfg(any(test, feature = "test-support"))]
static TEST_GLOBAL_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
fn test_global_store() -> &'static RwLock<Option<Arc<VectorStore>>> {
    TEST_GLOBAL_STORE.get_or_init(|| RwLock::new(None))
}

#[cfg(any(test, feature = "test-support"))]
pub struct TestGlobalStoreGuard {
    previous: Option<Arc<VectorStore>>,
    _lock: MutexGuard<'static, ()>,
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestGlobalStoreGuard {
    fn drop(&mut self) {
        *test_global_store().write() = self.previous.take();
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn install_test_global(store: Arc<VectorStore>) -> TestGlobalStoreGuard {
    let lock = TEST_GLOBAL_STORE_LOCK.get_or_init(|| Mutex::new(())).lock();
    let mut slot = test_global_store().write();
    let previous = slot.replace(store);
    TestGlobalStoreGuard {
        previous,
        _lock: lock,
    }
}

pub fn install_global(store: Arc<VectorStore>) {
    let already = GLOBAL_STORE.set(store.clone()).is_err();
    if already {
        return;
    }
    spawn_periodic_flusher(store.clone());
    spawn_periodic_compactor(store);
}

/// Periodic flusher thread. Walks every partition every FLUSH_INTERVAL_SECS
/// and force-flushes derived files (slab.bin / ids.bin / graph.bin / meta.json)
/// for any partition with `wal_records > last_flushed_wal_records`. The
/// per-batch upsert path is throttled (see flush_derived_files_throttled),
/// so this thread is what actually keeps the on-disk derived projections
/// fresh during steady-state ingest.
fn spawn_periodic_flusher(store: Arc<VectorStore>) {
    std::thread::Builder::new()
        .name("blackbox-vectors-flush".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(FLUSH_INTERVAL_SECS));
                let partitions: Vec<_> = store.partitions.read().values().cloned().collect();
                for partition in partitions {
                    let needs = partition.read().needs_flush();
                    if !needs {
                        continue;
                    }
                    let mut p = partition.write();
                    let active = p.slab.active_count();
                    let dims = p.slab.dims();
                    let est_bytes = active.saturating_mul(dims).saturating_mul(4);
                    let started = std::time::Instant::now();
                    let result = p.flush_derived_files();
                    let elapsed_ms = started.elapsed().as_millis();
                    match result {
                        Ok(()) => {
                            // Logged at INFO so users can correlate disk load
                            // with bbox flushes. Each line is one slab.bin
                            // rewrite of `est_bytes` MB.
                            tracing::info!(
                                route = %p.route,
                                wal_records = p.wal_records,
                                active_count = active,
                                slab_bytes = est_bytes,
                                elapsed_ms,
                                "vector partition derived files flushed"
                            );
                            if p.needs_snapshot_refresh() {
                                p.write_snapshot_best_effort("periodic_refresh");
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                route = %p.route,
                                error = %err,
                                "vector partition periodic flush failed; will retry"
                            );
                        }
                    }
                }
            }
        })
        .expect("failed to spawn vector flush thread");
}

fn spawn_periodic_compactor(store: Arc<VectorStore>) {
    std::thread::Builder::new()
        .name("blackbox-vectors-compact".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(COMPACT_INTERVAL_SECS));
                match store.compact_partitions(None) {
                    Ok(stats) => {
                        for stat in stats {
                            tracing::info!(
                                route = %stat.route,
                                before_wal_records = stat.before_wal_records,
                                after_wal_records = stat.after_wal_records,
                                before_slab_entries = stat.before_slab_entries,
                                after_slab_entries = stat.after_slab_entries,
                                elapsed_ms = stat.elapsed_ms,
                                "vector partition compacted"
                            );
                        }
                    }
                    Err(err) => tracing::warn!(
                        error = %err,
                        "vector partition compaction failed; will retry"
                    ),
                }
            }
        })
        .expect("failed to spawn vector compaction thread");
}

pub fn global() -> Arc<VectorStore> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(store) = test_global_store().read().clone() {
        return store;
    }

    GLOBAL_STORE
        .get_or_init(|| {
            let store = Arc::new(
                VectorStore::open(default_vectors_dir()).expect("default vector store should open"),
            );
            spawn_periodic_flusher(store.clone());
            spawn_periodic_compactor(store.clone());
            store
        })
        .clone()
}

/// Return the installed vector store only if it is already available.
///
/// This deliberately avoids `OnceLock::get_or_init`: search/status tools use
/// it to degrade during cold-start warmup instead of blocking behind a
/// multi-GB partition load.
pub fn try_global() -> Option<Arc<VectorStore>> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(store) = test_global_store().read().clone() {
        return Some(store);
    }

    GLOBAL_STORE.get().cloned()
}

pub fn upsert(route: &str, entity_id: &str, content_hash: &str, vector: Vec<f32>) -> Result<()> {
    global().upsert(route, entity_id, content_hash, vector)
}

pub fn upsert_batch(route: &str, records: Vec<VectorUpsert>) -> Result<()> {
    global().upsert_batch(route, records)
}

pub fn delete(route: &str, entity_id: &str) -> Result<()> {
    global().delete(route, entity_id)
}

pub fn delete_entity_all_routes(entity_id: &str) -> Result<()> {
    global().delete_entity_all_routes(entity_id)
}

pub fn contains_active(route: &str, entity_id: &str, content_hash: &str) -> Result<bool> {
    global().contains_active(route, entity_id, content_hash)
}

pub(crate) fn try_contains_active_if_initialized(
    route: &str,
    entity_id: &str,
    content_hash: &str,
) -> Result<Option<bool>> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(store) = test_global_store().read().clone() {
        return store
            .contains_active(route, entity_id, content_hash)
            .map(Some);
    }

    let Some(store) = GLOBAL_STORE.get().cloned() else {
        return Ok(None);
    };
    store
        .contains_active(route, entity_id, content_hash)
        .map(Some)
}

pub fn search(route: &str, query: &[f32], k: usize) -> Result<Vec<SearchHit>> {
    global().search(route, query, k)
}

// Promoted pub(crate) -> pub: called from the daemon (src/embed/mod.rs) across
// the new crate boundary.
pub fn iter_active(
    route: &str,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<std::vec::IntoIter<VectorEntry>> {
    global().iter_active(route, since)
}

// Promoted pub(crate) -> pub: called from src/embed/mod.rs across the crate boundary.
pub fn active_entity_hashes(route: &str) -> Result<Vec<(String, String)>> {
    global().active_entity_hashes(route)
}

// Promoted pub(crate) -> pub: called from src/embed/mod.rs across the crate boundary.
pub fn cluster_neighbors_within_route(
    route: &str,
    similarity_threshold: f32,
) -> Result<Vec<VectorCluster>> {
    global().cluster_neighbors_within_route(route, similarity_threshold)
}

pub fn rebuild(route: &str) -> Result<()> {
    global().rebuild(route)
}

pub fn compact_partitions(max_partitions: Option<usize>) -> Result<Vec<RouteCompactionStats>> {
    global().compact_partitions(max_partitions)
}

pub fn metrics() -> BTreeMap<String, PartitionMetrics> {
    global().metrics()
}

pub fn try_metrics() -> Option<BTreeMap<String, PartitionMetrics>> {
    try_global().map(|store| store.metrics())
}

/// Non-blocking partition metrics: None during cold-start warmup, and
/// partitions under an active write-lock hold (rebuild) are omitted.
pub fn metrics_nonblocking() -> Option<BTreeMap<String, PartitionMetrics>> {
    try_global().map(|store| store.metrics_nonblocking())
}

/// Partition lifecycle inventory against the installed global store.
/// Degrades to Ok(None) during cold-start warmup (same contract as
/// `try_metrics`).
pub fn partition_infos() -> Result<Option<Vec<PartitionInfo>>> {
    let Some(store) = try_global() else {
        return Ok(None);
    };
    store.partition_infos().map(Some)
}

/// Remove one partition from the installed global store. Errors during
/// cold-start warmup rather than silently doing nothing.
pub fn remove_partition(route: &str) -> Result<bool> {
    let Some(store) = try_global() else {
        anyhow::bail!("vector store is still warming up; retry shortly");
    };
    store.remove_partition(route)
}

/// Sampled self-recall probe against the installed global store. Degrades to
/// Ok(None) during cold-start warmup (same contract as `try_metrics`).
pub fn self_recall_probe(route: &str, sample_every: usize, k: usize) -> Result<Option<f64>> {
    let Some(store) = try_global() else {
        return Ok(None);
    };
    store.self_recall_probe(route, sample_every, k)
}

pub fn default_vectors_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("state")
        })
        .join("blackbox")
        .join("vectors")
}

#[derive(Debug)]
pub struct VectorStore {
    root: PathBuf,
    partitions: RwLock<BTreeMap<String, Arc<RwLock<Partition>>>>,
}

impl VectorStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)
            .with_context(|| format!("creating vector store {}", root.display()))?;
        let store = Self {
            root,
            partitions: RwLock::new(BTreeMap::new()),
        };
        store.load_existing_partitions()?;
        Ok(store)
    }

    /// Open the store root without loading existing partitions.
    ///
    /// This is for process state that only needs a placeholder handle while the
    /// real global vector store warms asynchronously. Search/status callers use
    /// `try_global` to degrade until that warmup completes.
    pub fn open_unloaded(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)
            .with_context(|| format!("creating vector store {}", root.display()))?;
        Ok(Self {
            root,
            partitions: RwLock::new(BTreeMap::new()),
        })
    }

    pub fn upsert(
        &self,
        route: &str,
        entity_id: &str,
        content_hash: &str,
        vector: Vec<f32>,
    ) -> Result<()> {
        let partition = self.partition(route)?;
        let mut partition = partition.write();
        partition
            .upsert(entity_id, content_hash, vector)
            .with_context(|| format!("upserting vector entity {entity_id} into {route}"))?;
        partition
            .flush_derived_files_throttled()
            .with_context(|| format!("flushing vector partition {route}"))
    }

    pub fn upsert_batch(&self, route: &str, records: Vec<VectorUpsert>) -> Result<()> {
        let partition = self.partition(route)?;
        let mut partition = partition.write();
        partition.upsert_batch(records)?;
        partition
            .flush_derived_files_throttled()
            .with_context(|| format!("flushing vector partition {route}"))
    }

    /// Force-checkpoint every partition's derived metadata and fsync every
    /// dirty WAL. Startup uses snapshots opportunistically and falls back to
    /// WAL replay when a snapshot is absent or stale.
    pub fn flush_all(&self) -> Result<()> {
        let partitions: Vec<_> = self.partitions.read().values().cloned().collect();
        for partition in partitions {
            let mut partition = partition.write();
            partition.flush_derived_full()?;
        }
        crate::wal::sync_pending().ok();
        Ok(())
    }

    pub fn delete(&self, route: &str, entity_id: &str) -> Result<()> {
        let partition = self.partition(route)?;
        let result = partition
            .write()
            .delete(entity_id)
            .with_context(|| format!("deleting vector entity {entity_id} from {route}"));
        result
    }

    pub fn delete_entity_all_routes(&self, entity_id: &str) -> Result<()> {
        let partitions = self.partitions.read().values().cloned().collect::<Vec<_>>();
        for partition in partitions {
            partition.write().delete(entity_id)?;
        }
        Ok(())
    }

    pub fn contains_active(
        &self,
        route: &str,
        entity_id: &str,
        content_hash: &str,
    ) -> Result<bool> {
        let Some(partition) = self.partitions.read().get(route).cloned() else {
            return Ok(false);
        };
        let contains = partition.read().contains_active(entity_id, content_hash);
        Ok(contains)
    }

    pub fn search(&self, route: &str, query: &[f32], k: usize) -> Result<Vec<SearchHit>> {
        let Some(partition) = self.partitions.read().get(route).cloned() else {
            return Ok(Vec::new());
        };
        let hits = partition.read().search(query, k);
        Ok(hits)
    }

    pub(crate) fn iter_active(
        &self,
        route: &str,
        since: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<std::vec::IntoIter<VectorEntry>> {
        let Some(partition) = self.partitions.read().get(route).cloned() else {
            return Ok(Vec::new().into_iter());
        };
        let entries = partition.read().active_entries_since(since);
        Ok(entries.into_iter())
    }

    pub(crate) fn active_entity_hashes(&self, route: &str) -> Result<Vec<(String, String)>> {
        let Some(partition) = self.partitions.read().get(route).cloned() else {
            return Ok(Vec::new());
        };
        let hashes = partition.read().active_entity_hashes();
        Ok(hashes)
    }

    pub(crate) fn cluster_neighbors_within_route(
        &self,
        route: &str,
        similarity_threshold: f32,
    ) -> Result<Vec<VectorCluster>> {
        let entries = self.iter_active(route, None)?.collect::<Vec<_>>();
        Ok(cluster_entries(entries, similarity_threshold))
    }

    pub fn rebuild(&self, route: &str) -> Result<()> {
        let partition = self.partition(route)?;
        let result = partition.write().compact().map(|_| ());
        result
    }

    pub fn compact_partitions(
        &self,
        max_partitions: Option<usize>,
    ) -> Result<Vec<RouteCompactionStats>> {
        self.compact_partitions_with_policy(
            COMPACT_DELETED_RATIO,
            COMPACT_MIN_DELETED_ENTRIES,
            COMPACT_MIN_WAL_SURPLUS_RECORDS,
            max_partitions,
        )
    }

    fn compact_partitions_with_policy(
        &self,
        deleted_ratio_threshold: f32,
        min_deleted_entries: usize,
        min_wal_surplus_records: usize,
        max_partitions: Option<usize>,
    ) -> Result<Vec<RouteCompactionStats>> {
        let mut candidates = self
            .partitions
            .read()
            .iter()
            .filter_map(|(route, partition)| {
                let partition_read = partition.read();
                if !partition_read.needs_compaction_with_policy(
                    deleted_ratio_threshold,
                    min_deleted_entries,
                    min_wal_surplus_records,
                ) {
                    return None;
                }
                Some((
                    route.clone(),
                    partition_read.metrics().deleted_ratio,
                    partition.clone(),
                ))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let limit = max_partitions.unwrap_or(usize::MAX);
        let mut compacted = Vec::new();
        for (route, _, partition) in candidates.into_iter().take(limit) {
            let started = std::time::Instant::now();
            let Some(stats) = Self::compact_partition_from_snapshot(
                &partition,
                deleted_ratio_threshold,
                min_deleted_entries,
                min_wal_surplus_records,
            )?
            else {
                continue;
            };
            compacted.push(RouteCompactionStats {
                route,
                before_wal_records: stats.before_wal_records,
                after_wal_records: stats.after_wal_records,
                before_slab_entries: stats.before_slab_entries,
                after_slab_entries: stats.after_slab_entries,
                elapsed_ms: started.elapsed().as_millis(),
            });
        }
        Ok(compacted)
    }

    fn compact_partition_from_snapshot(
        partition: &Arc<RwLock<Partition>>,
        deleted_ratio_threshold: f32,
        min_deleted_entries: usize,
        min_wal_surplus_records: usize,
    ) -> Result<Option<CompactionStats>> {
        let prepared = {
            let partition = partition.read();
            if !partition.needs_compaction_with_policy(
                deleted_ratio_threshold,
                min_deleted_entries,
                min_wal_surplus_records,
            ) {
                return Ok(None);
            }
            partition.prepare_compaction()?
        };
        let mut partition = partition.write();
        if partition.wal_records != prepared.before_wal_records
            || partition.slab.len() != prepared.before_slab_entries
        {
            return Ok(None);
        }
        partition.apply_prepared_compaction(prepared).map(Some)
    }

    pub fn partition_count(&self) -> usize {
        self.partitions.read().len()
    }

    /// Lifecycle inventory of every partition present on disk, including
    /// ones the in-memory map has not loaded. `dims`/`active_count` come
    /// from loaded partition metrics (`None` while unloaded or under an
    /// active write-lock hold); `last_write`/`disk_bytes` come from file
    /// metadata so they never force a load of a cold multi-GB partition.
    pub fn partition_infos(&self) -> Result<Vec<PartitionInfo>> {
        let loaded = self.metrics_nonblocking();
        let mut infos = Vec::new();
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("reading vector store {}", self.root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let route = entry.file_name().to_string_lossy().to_string();
            let mut last_write: Option<chrono::DateTime<chrono::Utc>> = None;
            let mut disk_bytes = 0u64;
            for file in fs::read_dir(entry.path())? {
                let metadata = file?.metadata()?;
                if !metadata.is_file() {
                    continue;
                }
                disk_bytes += metadata.len();
                if let Ok(modified) = metadata.modified() {
                    let modified = chrono::DateTime::<chrono::Utc>::from(modified);
                    if last_write.is_none_or(|current| modified > current) {
                        last_write = Some(modified);
                    }
                }
            }
            let metrics = loaded.get(&route);
            infos.push(PartitionInfo {
                route,
                dims: metrics.map(|m| m.dims),
                active_count: metrics.map(|m| m.active_count),
                last_write,
                disk_bytes,
            });
        }
        infos.sort_by(|a, b| a.route.cmp(&b.route));
        Ok(infos)
    }

    /// Remove one partition: drop it from the in-memory map and delete its
    /// directory. Returns false when no such partition exists. Intended for
    /// orphaned partitions no route maps to — a concurrent writer targeting
    /// the same route can recreate it, so callers gate on unmapped routes.
    pub fn remove_partition(&self, route: &str) -> Result<bool> {
        if route.is_empty() || route.contains(['/', '\\']) || route == "." || route == ".." {
            anyhow::bail!("invalid partition route `{route}`");
        }
        let removed = self.partitions.write().remove(route);
        if let Some(partition) = removed {
            // Let any in-flight operation on the evicted handle finish
            // before the files disappear underneath it.
            let _quiesce = partition.write();
        }
        let path = self.root.join(route);
        if !path.is_dir() {
            return Ok(false);
        }
        fs::remove_dir_all(&path)
            .with_context(|| format!("removing vector partition {}", path.display()))?;
        Ok(true)
    }

    /// Sampled self-recall diagnostic for one route (gap-1168b0bd c).
    /// O(sample × search) — operator-invoked probe, never a metrics()-path
    /// stat. Uses `try_read` so a probe issued during a long write-lock
    /// rebuild errors with "busy" instead of hanging the caller; returns
    /// Ok(None) when the partition has no graph yet.
    pub fn self_recall_probe(
        &self,
        route: &str,
        sample_every: usize,
        k: usize,
    ) -> Result<Option<f64>> {
        let partition = self.partition(route)?;
        let guard = partition.try_read().ok_or_else(|| {
            anyhow::anyhow!("partition {route} is busy (rebuild/compaction in progress)")
        })?;
        Ok(guard
            .hnsw
            .as_ref()
            .map(|hnsw| hnsw.self_recall_probe(sample_every, k)))
    }

    pub fn metrics(&self) -> BTreeMap<String, PartitionMetrics> {
        self.partitions
            .read()
            .iter()
            .map(|(route, partition)| (route.clone(), partition.read().metrics()))
            .collect()
    }

    /// Like `metrics()` but skips partitions whose lock is held (e.g. a
    /// long write-lock rebuild). For surfaces that must never block behind
    /// compaction — the inbox attention layer reads through this.
    pub fn metrics_nonblocking(&self) -> BTreeMap<String, PartitionMetrics> {
        self.partitions
            .read()
            .iter()
            .filter_map(|(route, partition)| {
                partition
                    .try_read()
                    .map(|guard| (route.clone(), guard.metrics()))
            })
            .collect()
    }

    fn load_existing_partitions(&self) -> Result<()> {
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("reading vector store {}", self.root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let route = entry.file_name().to_string_lossy().to_string();
            let partition = Partition::open(route.clone(), entry.path())?;
            self.partitions
                .write()
                .insert(route, Arc::new(RwLock::new(partition)));
        }
        Ok(())
    }

    fn partition(&self, route: &str) -> Result<Arc<RwLock<Partition>>> {
        if let Some(partition) = self.partitions.read().get(route).cloned() {
            return Ok(partition);
        }
        let path = self.root.join(route);
        let mut partitions = self.partitions.write();
        if let Some(partition) = partitions.get(route).cloned() {
            return Ok(partition);
        }
        let partition = Arc::new(RwLock::new(Partition::open(route.to_string(), path)?));
        partitions.insert(route.to_string(), partition.clone());
        Ok(partition)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PartitionMetrics {
    pub route: String,
    pub state: PartitionState,
    pub dims: usize,
    pub wal_records: usize,
    pub active_count: usize,
    pub deleted_count: usize,
    pub deleted_ratio: f32,
    pub hnsw_rebuilds: usize,
    pub hnsw: Option<HnswMetricsSerde>,
}

impl PartitionMetrics {
    /// Fraction of active nodes unreachable by graph traversal
    /// (`zero_in_degree_nodes / active_nodes`) — vector-recall risk
    /// (gap-1168b0bd). 0.0 for empty or graph-less partitions. Below
    /// `MIN_CONNECTIVITY_GUARD_NODES` active nodes the ratio is noise;
    /// gate consumers must check that floor, this is the raw fraction.
    pub fn connectivity_risk_ratio(&self) -> f32 {
        let Some(hnsw) = &self.hnsw else { return 0.0 };
        if hnsw.active_nodes == 0 {
            return 0.0;
        }
        hnsw.zero_in_degree_nodes as f32 / hnsw.active_nodes as f32
    }

    /// True when this partition's connectivity degradation merits attention
    /// at `threshold` (and the partition is large enough for the ratio to
    /// be signal rather than noise).
    pub fn connectivity_breach(&self, threshold: f32) -> bool {
        self.hnsw
            .as_ref()
            .is_some_and(|hnsw| hnsw.active_nodes >= MIN_CONNECTIVITY_GUARD_NODES)
            && self.connectivity_risk_ratio() >= threshold
    }
}

/// Lifecycle-facing partition inventory row (`bbox_embed_partitions`).
/// Distinct from `PartitionMetrics`: this is disk-truthful (covers
/// unloaded partitions) and carries recency, not HNSW health.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PartitionInfo {
    pub route: String,
    pub dims: Option<usize>,
    pub active_count: Option<usize>,
    pub last_write: Option<chrono::DateTime<chrono::Utc>>,
    pub disk_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteCompactionStats {
    pub route: String,
    pub before_wal_records: usize,
    pub after_wal_records: usize,
    pub before_slab_entries: usize,
    pub after_slab_entries: usize,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PartitionState {
    Empty,
    Active { dims: usize },
}

#[derive(Debug, Clone)]
pub struct VectorUpsert {
    pub entity_id: String,
    pub content_hash: String,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VectorEntry {
    pub entity_id: String,
    pub content_hash: String,
    pub vector: Vec<f32>,
    pub upserted_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorCluster {
    pub id: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HnswMetricsSerde {
    pub total_nodes: usize,
    pub active_nodes: usize,
    pub deleted_nodes: usize,
    pub dimensions: usize,
    pub max_level: isize,
    pub entry_point: Option<usize>,
    pub neighbor_refs: usize,
    pub avg_neighbor_degree: f64,
    pub layer_distribution: Vec<usize>,
    pub disconnected_nodes: usize,
    /// Active nodes with no inbound active edge (gap-2eabd96d leading
    /// indicator). Defaults for snapshots written before the field existed.
    #[serde(default)]
    pub zero_in_degree_nodes: usize,
}

impl From<HnswMetrics> for HnswMetricsSerde {
    fn from(value: HnswMetrics) -> Self {
        Self {
            total_nodes: value.total_nodes,
            active_nodes: value.active_nodes,
            deleted_nodes: value.deleted_nodes,
            dimensions: value.dimensions,
            max_level: value.max_level,
            entry_point: value.entry_point,
            neighbor_refs: value.neighbor_refs,
            avg_neighbor_degree: value.avg_neighbor_degree,
            layer_distribution: value.layer_distribution,
            disconnected_nodes: value.disconnected_nodes,
            zero_in_degree_nodes: value.zero_in_degree_nodes,
        }
    }
}

#[derive(Debug)]
struct Partition {
    route: String,
    path: PathBuf,
    slab: VectorSlab,
    hnsw: Option<HnswIndex>,
    wal_records: usize,
    hnsw_rebuilds: usize,
    /// `wal_records` value at the most recent successful flush_derived_files.
    /// flush_derived_files_throttled compares this to current `wal_records` and
    /// skips the (expensive — full slab.bin rewrite) flush when too few new
    /// records have landed since last time. Force-flushes on shutdown +
    /// periodic timer ignore this gate.
    last_flushed_wal_records: usize,
    /// `wal_records` value captured by the current `snapshot.bin`.
    last_snapshot_wal_records: usize,
}

/// Minimum new WAL records required between non-forced flushes. Each flush
/// rewrites the entire slab.bin (~4 bytes × dims × active_count, often
/// hundreds of MB), so per-batch flushing during heavy ingest churned the
/// page cache and pinned disk I/O for hours during the agentic-corpus
/// backfill experiment. The WAL is sync'd on every batch independently of
/// derived-file flushing, so correctness survives crashes regardless.
///
/// Rationale for 8192: voyage batches at 128 docs, so an in-flight worker
/// fills 64 batches between flushes. At ~1.5s/batch that's roughly 90
/// seconds of work — comfortably longer than the 30-second periodic
/// flusher tick, so the periodic timer (not this threshold) is what
/// usually drives flushes during steady-state ingest. The threshold only
/// kicks in for very fast bursts where the periodic timer hasn't fired
/// yet, capping write amplification at ~slab_size_bytes per ~8k records.
const FLUSH_MIN_RECORDS: usize = 8192;
/// Background flusher cadence — every Partition with `wal_records >
/// last_flushed_wal_records` gets a force-flush at this interval, so users
/// who stop ingest mid-stream still get derived files reasonably fresh.
const FLUSH_INTERVAL_SECS: u64 = 30;
const COMPACT_INTERVAL_SECS: u64 = 300;
const COMPACT_DELETED_RATIO: f32 = 0.30;
const COMPACT_MIN_DELETED_ENTRIES: usize = 10_000;
const COMPACT_MIN_WAL_SURPLUS_RECORDS: usize = 100_000;

/// Connectivity thresholds (gap-1168b0bd). These gate the WORKFLOW
/// compaction lane (embed-compaction-arc: quiesce → rebuild → swap) and the
/// inbox attention layer — deliberately NOT the in-process periodic
/// compactor above, because a connectivity-triggered rebuild holds the
/// partition write lock for the full rebuild (~25 min at 399k×1024d) and
/// must not fire unquiesced on a 5-minute tick.
///
/// Fraction is `zero_in_degree_nodes / active_nodes` — the leading
/// indicator of reverse-edge orphaning. Calibration from the gap-2eabd96d
/// incident: 16.7% disconnected at detection; ~1.4% residual
/// (exact-duplicate degeneracy) after rebuild; healthy partitions sit
/// ≤0.3%.
pub const COMPACT_CONNECTIVITY_RATIO: f32 = 0.05;
pub const NOTIFY_CONNECTIVITY_RATIO: f32 = 0.02;
/// Partitions smaller than this have rebuilds cheap enough that the
/// deleted-ratio gate covers them; connectivity ratios on tiny graphs are
/// also noisy (one orphan in 50 nodes is 2%).
pub const MIN_CONNECTIVITY_GUARD_NODES: usize = 1_000;

#[derive(Debug, Clone, Copy)]
struct CompactionStats {
    before_wal_records: usize,
    after_wal_records: usize,
    before_slab_entries: usize,
    after_slab_entries: usize,
}

struct PreparedCompaction {
    before_wal_records: usize,
    before_slab_entries: usize,
    compacted_slab: VectorSlab,
    rebuilt_hnsw: Option<HnswIndex>,
    compacted_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct PartitionSnapshot {
    // Bincode 1.x is positional: changing field order/types in this struct,
    // VectorSlab, SlabEntry, or HnswIndex requires a VECTOR_SNAPSHOT_VERSION
    // bump so old caches fall back to WAL replay.
    schema_version: String,
    route: String,
    wal_records: usize,
    wal_len_bytes: u64,
    hnsw_rebuilds: usize,
    slab: VectorSlab,
    hnsw: Option<HnswIndex>,
}

impl Partition {
    fn open(route: String, path: PathBuf) -> Result<Self> {
        fs::create_dir_all(&path)
            .with_context(|| format!("creating vector partition {}", path.display()))?;
        let mut partition = Self {
            route,
            path,
            slab: VectorSlab::default(),
            hnsw: None,
            wal_records: 0,
            hnsw_rebuilds: 0,
            last_flushed_wal_records: 0,
            last_snapshot_wal_records: 0,
        };
        let restored_snapshot = match partition.restore_from_snapshot() {
            Ok(restored) => restored,
            Err(err) => {
                tracing::warn!(
                    route = %partition.route,
                    error = %err,
                    "vector snapshot restore failed; rebuilding partition from WAL"
                );
                false
            }
        };
        if !restored_snapshot {
            partition.rebuild_from_wal()?;
            partition.write_snapshot_best_effort("wal_rebuild");
        }
        Ok(partition)
    }

    fn upsert(&mut self, entity_id: &str, content_hash: &str, vector: Vec<f32>) -> Result<()> {
        let record = WalRecord::upsert(&self.route, entity_id, content_hash, vector.clone());
        if !self.apply_upsert(entity_id, content_hash, vector)? {
            return Ok(());
        }
        wal::append(&self.wal_path(), &record)?;
        self.wal_records += 1;
        Ok(())
    }

    fn upsert_batch(&mut self, records: Vec<VectorUpsert>) -> Result<()> {
        let mut wal_records = Vec::new();
        for record in records {
            let wal_record = WalRecord::upsert(
                &self.route,
                &record.entity_id,
                &record.content_hash,
                record.vector.clone(),
            );
            if self.apply_upsert(&record.entity_id, &record.content_hash, record.vector)? {
                wal_records.push(wal_record);
            }
        }
        wal::append_many(&self.wal_path(), &wal_records)?;
        self.wal_records += wal_records.len();
        Ok(())
    }

    fn apply_upsert(
        &mut self,
        entity_id: &str,
        content_hash: &str,
        vector: Vec<f32>,
    ) -> Result<bool> {
        if !self.slab.upsert(entity_id, content_hash, vector.clone())? {
            return Ok(false);
        }
        match self.hnsw.as_mut() {
            Some(hnsw) => hnsw
                .push(entity_id.to_string(), vector)
                .map_err(anyhow::Error::msg)?,
            None => self.rebuild_hnsw()?,
        }
        Ok(true)
    }

    fn delete(&mut self, entity_id: &str) -> Result<()> {
        let record = WalRecord::delete(&self.route, entity_id);
        wal::append(&self.wal_path(), &record)?;
        self.wal_records += 1;
        self.slab.delete(entity_id);
        if let Some(hnsw) = self.hnsw.as_mut() {
            hnsw.delete(entity_id);
        }
        // Deletion is rare and we want it durable + reflected in slab.bin
        // so a cold start doesn't resurrect the entity via stale slab.bin.
        self.flush_derived_full()
    }

    fn search(&self, query: &[f32], k: usize) -> Vec<SearchHit> {
        self.hnsw
            .as_ref()
            .map(|hnsw| hnsw.search(query, k))
            .unwrap_or_default()
    }

    fn contains_active(&self, entity_id: &str, content_hash: &str) -> bool {
        self.slab.contains_active(entity_id, content_hash)
    }

    fn active_entries_since(
        &self,
        since: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Vec<VectorEntry> {
        self.slab
            .active_entries()
            .filter(|entry| match since {
                Some(cutoff) => entry
                    .upserted_at
                    .as_deref()
                    .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                    .map(|ts| ts.with_timezone(&chrono::Utc) > cutoff)
                    .unwrap_or(false),
                None => true,
            })
            .map(|entry| VectorEntry {
                entity_id: entry.entity_id.clone(),
                content_hash: entry.content_hash.clone(),
                vector: entry.vector.clone(),
                upserted_at: entry.upserted_at.clone(),
            })
            .collect()
    }

    fn active_entity_hashes(&self) -> Vec<(String, String)> {
        self.slab
            .active_entries()
            .map(|entry| (entry.entity_id.clone(), entry.content_hash.clone()))
            .collect()
    }

    fn rebuild_from_wal(&mut self) -> Result<()> {
        let wal_path = self.wal_path();
        self.slab = VectorSlab::default();
        self.hnsw = None;
        let mut wal_records = 0usize;
        wal::for_each(&wal_path, |record| {
            wal_records += 1;
            if record.deleted_at.is_some() {
                self.slab.delete(&record.entity_id);
            } else {
                self.slab.upsert_at(
                    &record.entity_id,
                    &record.content_hash,
                    record.vector,
                    record.upserted_at,
                )?;
            }
            Ok(())
        })?;
        self.wal_records = wal_records;
        self.rebuild_hnsw()?;
        // After a full replay, checkpoint metadata and sync the WAL.
        self.flush_derived_full()
    }

    fn restore_from_snapshot(&mut self) -> Result<bool> {
        let snapshot_path = self.snapshot_path();
        if !snapshot_path.exists() {
            return Ok(false);
        }
        let snapshot = read_snapshot(&snapshot_path)?;
        if snapshot.schema_version != VECTOR_SNAPSHOT_VERSION {
            tracing::warn!(
                route = %self.route,
                snapshot_version = %snapshot.schema_version,
                expected_version = VECTOR_SNAPSHOT_VERSION,
                "vector snapshot version mismatch; rebuilding from WAL"
            );
            return Ok(false);
        }
        if snapshot.route != self.route {
            tracing::warn!(
                route = %self.route,
                snapshot_route = %snapshot.route,
                "vector snapshot route mismatch; rebuilding from WAL"
            );
            return Ok(false);
        }
        let wal_path = self.wal_path();
        let wal_len_bytes = wal_path.metadata().map(|meta| meta.len()).unwrap_or(0);
        if wal_len_bytes < snapshot.wal_len_bytes {
            tracing::warn!(
                route = %self.route,
                wal_len_bytes,
                snapshot_wal_len_bytes = snapshot.wal_len_bytes,
                "vector snapshot is ahead of WAL; rebuilding from WAL"
            );
            return Ok(false);
        }

        self.slab = snapshot.slab;
        self.slab.rebuild_active_index();
        self.hnsw = snapshot.hnsw;
        if let Some(hnsw) = &self.hnsw {
            let metrics = hnsw.metrics();
            if metrics.active_nodes != self.slab.active_count()
                || metrics.dimensions != self.slab.dims()
            {
                tracing::warn!(
                    route = %self.route,
                    snapshot_active_count = self.slab.active_count(),
                    hnsw_active_nodes = metrics.active_nodes,
                    snapshot_dims = self.slab.dims(),
                    hnsw_dims = metrics.dimensions,
                    "vector snapshot HNSW metrics mismatch; rebuilding from WAL"
                );
                return Ok(false);
            }
        } else if self.slab.active_count() > 0 {
            tracing::warn!(
                route = %self.route,
                active_count = self.slab.active_count(),
                "vector snapshot has active vectors but no HNSW index; rebuilding from WAL"
            );
            return Ok(false);
        }
        self.wal_records = snapshot.wal_records;
        self.hnsw_rebuilds = snapshot.hnsw_rebuilds;
        self.last_flushed_wal_records = snapshot.wal_records;
        self.last_snapshot_wal_records = snapshot.wal_records;

        let mut replayed_tail = 0usize;
        wal::for_each_from(&wal_path, snapshot.wal_len_bytes, |record| {
            self.apply_wal_record(record)?;
            replayed_tail += 1;
            Ok(())
        })?;

        if replayed_tail > 0 || wal_len_bytes > snapshot.wal_len_bytes {
            self.flush_derived_full()?;
            self.write_snapshot_best_effort("snapshot_tail_replay");
        }
        tracing::info!(
            route = %self.route,
            wal_records = self.wal_records,
            replayed_tail,
            "vector partition restored from snapshot"
        );
        Ok(true)
    }

    fn apply_wal_record(&mut self, record: WalRecord) -> Result<()> {
        self.wal_records += 1;
        if record.deleted_at.is_some() {
            self.slab.delete(&record.entity_id);
            if let Some(hnsw) = self.hnsw.as_mut() {
                hnsw.delete(&record.entity_id);
            }
            return Ok(());
        }
        if !self.slab.upsert_at(
            &record.entity_id,
            &record.content_hash,
            record.vector.clone(),
            record.upserted_at,
        )? {
            return Ok(());
        }
        match self.hnsw.as_mut() {
            Some(hnsw) => hnsw
                .push(record.entity_id, record.vector)
                .map_err(anyhow::Error::msg)?,
            None => self.rebuild_hnsw()?,
        }
        Ok(())
    }

    fn write_snapshot_best_effort(&mut self, reason: &'static str) {
        let started = std::time::Instant::now();
        match self.write_snapshot() {
            Ok(()) => tracing::info!(
                route = %self.route,
                reason,
                wal_records = self.wal_records,
                active_count = self.slab.active_count(),
                elapsed_ms = started.elapsed().as_millis(),
                "vector partition snapshot written"
            ),
            Err(err) => tracing::warn!(
                route = %self.route,
                reason,
                error = %err,
                "vector partition snapshot write failed; WAL rebuild remains available"
            ),
        }
    }

    fn write_snapshot(&mut self) -> Result<()> {
        fs::create_dir_all(&self.path)?;
        let wal_path = self.wal_path();
        crate::wal::sync_path(&wal_path)?;
        let wal_len_bytes = wal_path.metadata().map(|meta| meta.len()).unwrap_or(0);
        let snapshot = PartitionSnapshot {
            schema_version: VECTOR_SNAPSHOT_VERSION.to_string(),
            route: self.route.clone(),
            wal_records: self.wal_records,
            wal_len_bytes,
            hnsw_rebuilds: self.hnsw_rebuilds,
            slab: self.slab.clone(),
            hnsw: self.hnsw.clone(),
        };
        let tmp_path = self.path.join(SNAPSHOT_TMP_FILE);
        let snapshot_path = self.snapshot_path();
        let _ = fs::remove_file(&tmp_path);
        let result = (|| -> Result<()> {
            let file = fs::File::create(&tmp_path)
                .with_context(|| format!("creating vector snapshot {}", tmp_path.display()))?;
            let mut writer = BufWriter::new(file);
            writer
                .write_all(VECTOR_SNAPSHOT_MAGIC)
                .with_context(|| format!("writing vector snapshot magic {}", tmp_path.display()))?;
            bincode::serialize_into(&mut writer, &snapshot)
                .with_context(|| format!("serializing vector snapshot {}", tmp_path.display()))?;
            writer
                .flush()
                .with_context(|| format!("flushing vector snapshot {}", tmp_path.display()))?;
            writer
                .get_ref()
                .sync_data()
                .with_context(|| format!("fsync vector snapshot {}", tmp_path.display()))?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                fs::rename(&tmp_path, &snapshot_path).with_context(|| {
                    format!(
                        "renaming vector snapshot {} to {}",
                        tmp_path.display(),
                        snapshot_path.display()
                    )
                })?;
                sync_parent_dir(&snapshot_path)?;
                self.last_snapshot_wal_records = self.wal_records;
                Ok(())
            }
            Err(err) => {
                let _ = fs::remove_file(&tmp_path);
                Err(err)
            }
        }
    }

    fn compact(&mut self) -> Result<CompactionStats> {
        let prepared = self.prepare_compaction()?;
        self.apply_prepared_compaction(prepared)
    }

    fn prepare_compaction(&self) -> Result<PreparedCompaction> {
        let before_wal_records = self.wal_records;
        let before_slab_entries = self.slab.len();
        // Build the new slab from the old one's active entries.
        // Each entry's metadata and vector are cloned into the replacement
        // slab (unavoidable — the old slab is still the source of truth
        // until we swap). The HNSW rebuild further below clones vectors
        // again for the graph construction. Two copies is the floor for
        // compaction; the previous implementation kept four.
        let mut compacted_slab = VectorSlab::new(self.slab.dims());
        let mut compacted_count = 0usize;
        for entry in self.slab.active_entries() {
            compacted_slab.upsert_at(
                &entry.entity_id,
                &entry.content_hash,
                entry.vector.clone(),
                entry.upserted_at.clone(),
            )?;
            compacted_count += 1;
        }
        let items = compacted_slab
            .active_entries()
            .map(|entry| (entry.entity_id.clone(), entry.vector.clone()))
            .collect::<Vec<_>>();
        let rebuilt_hnsw = if items.is_empty() {
            None
        } else {
            Some(HnswIndex::build(items, HnswOptions::default()).map_err(anyhow::Error::msg)?)
        };
        Ok(PreparedCompaction {
            before_wal_records,
            before_slab_entries,
            compacted_slab,
            rebuilt_hnsw,
            compacted_count,
        })
    }

    fn apply_prepared_compaction(
        &mut self,
        prepared: PreparedCompaction,
    ) -> Result<CompactionStats> {
        // The WAL is about to be rewritten from scratch. Drop the old
        // snapshot first so a failed post-compaction snapshot write cannot
        // leave a cache keyed to the previous WAL byte offsets.
        let snapshot_path = self.snapshot_path();
        match fs::remove_file(&snapshot_path) {
            Ok(()) => sync_parent_dir(&snapshot_path)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).context("removing stale vector snapshot before compaction");
            }
        }
        // Stream the compacted slab directly to the WAL file — no
        // intermediate Vec<WalRecord>. This avoids doubling memory
        // during compaction for routes with millions of vectors.
        wal::rewrite(
            &self.wal_path(),
            prepared
                .compacted_slab
                .active_entries()
                .map(|entry| WalRecord {
                    entity_id: entry.entity_id.clone(),
                    content_hash: entry.content_hash.clone(),
                    model: self.route.clone(),
                    dims: entry.vector.len(),
                    vector: entry.vector.clone(),
                    upserted_at: entry.upserted_at.clone(),
                    deleted_at: None,
                    route: self.route.clone(),
                }),
        )?;
        self.slab = prepared.compacted_slab;
        self.wal_records = prepared.compacted_count;
        self.hnsw = prepared.rebuilt_hnsw;
        self.hnsw_rebuilds += 1;
        self.flush_derived_full()?;
        self.write_snapshot_best_effort("compaction");

        Ok(CompactionStats {
            before_wal_records: prepared.before_wal_records,
            after_wal_records: self.wal_records,
            before_slab_entries: prepared.before_slab_entries,
            after_slab_entries: self.slab.len(),
        })
    }

    fn rebuild_hnsw(&mut self) -> Result<()> {
        let items = self
            .slab
            .active_entries()
            .map(|entry| (entry.entity_id.clone(), entry.vector.clone()))
            .collect::<Vec<_>>();
        self.hnsw = if items.is_empty() {
            None
        } else {
            Some(HnswIndex::build(items, HnswOptions::default()).map_err(anyhow::Error::msg)?)
        };
        self.hnsw_rebuilds += 1;
        Ok(())
    }

    fn metrics(&self) -> PartitionMetrics {
        let dims = self.slab.dims();
        let active_count = self.slab.active_count();
        let hnsw_metrics = self.hnsw.as_ref().map(|hnsw| hnsw.metrics());
        let deleted_count = hnsw_metrics
            .as_ref()
            .map(|metrics| metrics.deleted_nodes)
            .unwrap_or(0);
        let denominator = active_count + deleted_count;
        let deleted_ratio = if denominator == 0 {
            0.0
        } else {
            deleted_count as f32 / denominator as f32
        };
        PartitionMetrics {
            route: self.route.clone(),
            state: if active_count == 0 {
                PartitionState::Empty
            } else {
                PartitionState::Active { dims }
            },
            dims,
            wal_records: self.wal_records,
            active_count,
            deleted_count,
            deleted_ratio,
            hnsw_rebuilds: self.hnsw_rebuilds,
            hnsw: hnsw_metrics.map(Into::into),
        }
    }

    /// Checkpoint the partition: write the small `meta.json` (operator
    /// visibility — `du -sh` / `cat meta.json` to see partition state)
    /// and fsync the WAL. That's the entire on-disk write per
    /// checkpoint — typically <2 KB per partition.
    ///
    /// Historical context: previous versions wrote `slab.bin` (full
    /// slab as f32), `ids.bin` (entity_id list), and `graph.bin` (HNSW
    /// metrics) on every flush — hundreds of MB per call when the
    /// partition was hot. NONE of those files were ever read; at the time,
    /// cold start always used `rebuild_from_wal`. We dropped them in the
    /// disk-hammer post-mortem (see thread-3e2a0cfa).
    ///
    /// `flush_derived_full` is the same operation today — kept as a
    /// distinct alias so future durability extensions can branch on
    /// checkpoint strength without churning callers.
    fn flush_derived_files(&mut self) -> Result<()> {
        let options = HnswOptions::default();
        let metrics = self.metrics();
        fs::create_dir_all(&self.path)?;
        fs::write(
            self.path.join("meta.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": VECTOR_SCHEMA_VERSION,
                "route": self.route,
                "dims": self.slab.dims(),
                "wal_records": self.wal_records,
                "active_count": self.slab.active_count(),
                "deleted_count": metrics.deleted_count,
                "deleted_ratio": metrics.deleted_ratio,
                "m": options.m,
                "ef_construction": options.ef_construction,
                "ef_search": options.ef_search,
                "max_layers": options.max_layers,
            }))?,
        )?;
        // Checkpoint the WAL — durability for everything written since
        // the last sync. The append path no longer fsyncs per batch
        // (see `wal::append_many`), so this is where durability lands.
        crate::wal::sync_path(&self.wal_path())?;
        self.last_flushed_wal_records = self.wal_records;
        Ok(())
    }

    /// Identical to `flush_derived_files` today. Kept as a distinct
    /// name so callers can keep expressing "full checkpoint" intent even
    /// though vector snapshots are written from the startup/compaction paths.
    fn flush_derived_full(&mut self) -> Result<()> {
        self.flush_derived_files()
    }

    /// Wraps flush_derived_files with a throttle: skip when the WAL has
    /// grown by fewer than FLUSH_MIN_RECORDS since the last flush. The
    /// per-batch upsert path uses this so heavy ingest doesn't rewrite
    /// hundreds of MB of slab.bin per voyage batch (the original bug
    /// that pinned a workstation for hours during the agentic-corpus
    /// backfill). The periodic flusher in `spawn_flush_thread` and
    /// graceful-shutdown handlers force-flush via `flush_derived_files`
    /// directly.
    fn flush_derived_files_throttled(&mut self) -> Result<()> {
        if self
            .wal_records
            .saturating_sub(self.last_flushed_wal_records)
            < FLUSH_MIN_RECORDS
        {
            return Ok(());
        }
        self.flush_derived_files()
    }

    /// True when the partition has uncommitted derived-file state — used
    /// by the periodic flusher to decide whether to force a flush at
    /// the next tick.
    fn needs_flush(&self) -> bool {
        self.wal_records > self.last_flushed_wal_records
    }

    fn needs_snapshot_refresh(&self) -> bool {
        self.wal_records
            .saturating_sub(self.last_snapshot_wal_records)
            >= SNAPSHOT_MIN_RECORDS
    }

    fn needs_compaction(&self) -> bool {
        self.needs_compaction_with_policy(
            COMPACT_DELETED_RATIO,
            COMPACT_MIN_DELETED_ENTRIES,
            COMPACT_MIN_WAL_SURPLUS_RECORDS,
        )
    }

    fn needs_compaction_with_policy(
        &self,
        deleted_ratio_threshold: f32,
        min_deleted_entries: usize,
        min_wal_surplus_records: usize,
    ) -> bool {
        let active_count = self.slab.active_count();
        let deleted_count = self.slab.deleted_count();
        if deleted_count == 0 {
            return false;
        }
        let total = active_count + deleted_count;
        let deleted_ratio = deleted_count as f32 / total as f32;
        let wal_surplus = self.wal_records.saturating_sub(active_count);
        deleted_ratio >= deleted_ratio_threshold
            && (deleted_count >= min_deleted_entries || wal_surplus >= min_wal_surplus_records)
    }

    fn wal_path(&self) -> PathBuf {
        self.path.join("records.wal")
    }

    fn snapshot_path(&self) -> PathBuf {
        self.path.join(SNAPSHOT_FILE)
    }
}

// `write_f32_file` was removed alongside the slab.bin / ids.bin / graph.bin
// per-flush writes. Those derived raw-f32 dumps had no consumer; the current
// startup cache is an explicit `snapshot.bin` of the in-memory structures.
// Kept the bulk-write helper notes in git history (commit d8cd57d).

fn cluster_entries(entries: Vec<VectorEntry>, similarity_threshold: f32) -> Vec<VectorCluster> {
    if entries.len() < 2 {
        return Vec::new();
    }
    let threshold = similarity_threshold.clamp(0.0, 1.0);
    let mut parent: Vec<usize> = (0..entries.len()).collect();
    for left in 0..entries.len() {
        for right in (left + 1)..entries.len() {
            if entries[left].vector.len() != entries[right].vector.len() {
                continue;
            }
            let similarity =
                1.0 - distance::cosine_distance(&entries[left].vector, &entries[right].vector);
            if similarity >= threshold {
                union(&mut parent, left, right);
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        let root = find(&mut parent, idx);
        groups
            .entry(root)
            .or_default()
            .push(entry.entity_id.clone());
    }
    let mut clusters = Vec::new();
    for (idx, mut members) in groups
        .into_values()
        .filter(|members| members.len() > 1)
        .enumerate()
    {
        members.sort();
        clusters.push(VectorCluster {
            id: format!("cluster:{idx}"),
            members,
        });
    }
    clusters
}

fn find(parent: &mut [usize], idx: usize) -> usize {
    if parent[idx] != idx {
        parent[idx] = find(parent, parent[idx]);
    }
    parent[idx]
}

fn union(parent: &mut [usize], left: usize, right: usize) {
    let left_root = find(parent, left);
    let right_root = find(parent, right);
    if left_root != right_root {
        parent[right_root] = left_root;
    }
}

fn read_snapshot(path: &Path) -> Result<PartitionSnapshot> {
    let file = fs::File::open(path)
        .with_context(|| format!("opening vector snapshot {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut magic = [0u8; VECTOR_SNAPSHOT_MAGIC.len()];
    reader
        .read_exact(&mut magic)
        .with_context(|| format!("reading vector snapshot magic {}", path.display()))?;
    if &magic != VECTOR_SNAPSHOT_MAGIC {
        anyhow::bail!("unsupported vector snapshot file header");
    }
    bincode::deserialize_from(&mut reader)
        .with_context(|| format!("decoding vector snapshot {}", path.display()))
}

fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("finding parent directory for {}", path.display()))?;
    fs::File::open(parent)
        .with_context(|| format!("opening vector snapshot parent {}", parent.display()))?
        .sync_data()
        .with_context(|| format!("fsync vector snapshot parent {}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_rebuild_restores_active_vectors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "a", "h1", vec![1.0, 0.0, 0.0, 0.0])
            .unwrap();
        store
            .upsert("voyage-1024", "b", "h2", vec![0.0, 1.0, 0.0, 0.0])
            .unwrap();
        store.delete("voyage-1024", "b").unwrap();
        drop(store);

        let restored = VectorStore::open(tmp.path()).unwrap();
        let metrics = restored.metrics();
        assert_eq!(metrics["voyage-1024"].wal_records, 3);
        assert_eq!(metrics["voyage-1024"].active_count, 1);
        let hits = restored
            .search("voyage-1024", &[1.0, 0.0, 0.0, 0.0], 5)
            .unwrap();
        assert_eq!(hits[0].id, "a");
    }

    #[test]
    fn snapshot_restore_preserves_active_vectors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "a", "h1", vec![1.0, 0.0, 0.0, 0.0])
            .unwrap();
        store
            .upsert("voyage-1024", "b", "h2", vec![0.0, 1.0, 0.0, 0.0])
            .unwrap();
        drop(store);

        let restored_from_wal = VectorStore::open(tmp.path()).unwrap();
        assert!(tmp.path().join("voyage-1024").join(SNAPSHOT_FILE).exists());
        drop(restored_from_wal);

        let restored_from_snapshot = VectorStore::open(tmp.path()).unwrap();
        let metrics = restored_from_snapshot.metrics();
        assert_eq!(metrics["voyage-1024"].wal_records, 2);
        assert_eq!(metrics["voyage-1024"].active_count, 2);
        let hits = restored_from_snapshot
            .search("voyage-1024", &[1.0, 0.0, 0.0, 0.0], 5)
            .unwrap();
        assert_eq!(hits[0].id, "a");
    }

    #[test]
    fn snapshot_restore_replays_wal_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "a", "h1", vec![1.0, 0.0, 0.0, 0.0])
            .unwrap();
        drop(store);

        let restored = VectorStore::open(tmp.path()).unwrap();
        assert!(tmp.path().join("voyage-1024").join(SNAPSHOT_FILE).exists());
        restored
            .upsert("voyage-1024", "b", "h2", vec![0.0, 1.0, 0.0, 0.0])
            .unwrap();
        drop(restored);

        let restored_with_tail = VectorStore::open(tmp.path()).unwrap();
        let metrics = restored_with_tail.metrics();
        assert_eq!(metrics["voyage-1024"].wal_records, 2);
        assert_eq!(metrics["voyage-1024"].active_count, 2);
        assert!(
            restored_with_tail
                .contains_active("voyage-1024", "a", "h1")
                .unwrap()
        );
        assert!(
            restored_with_tail
                .contains_active("voyage-1024", "b", "h2")
                .unwrap()
        );
    }

    #[test]
    fn snapshot_restore_replays_delete_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "a", "h1", vec![1.0, 0.0, 0.0, 0.0])
            .unwrap();
        store
            .upsert("voyage-1024", "b", "h2", vec![0.0, 1.0, 0.0, 0.0])
            .unwrap();
        drop(store);

        let restored = VectorStore::open(tmp.path()).unwrap();
        assert!(tmp.path().join("voyage-1024").join(SNAPSHOT_FILE).exists());
        restored.delete("voyage-1024", "a").unwrap();
        drop(restored);

        let restored_with_tail_delete = VectorStore::open(tmp.path()).unwrap();
        let metrics = restored_with_tail_delete.metrics();
        assert_eq!(metrics["voyage-1024"].wal_records, 3);
        assert_eq!(metrics["voyage-1024"].active_count, 1);
        assert!(
            !restored_with_tail_delete
                .contains_active("voyage-1024", "a", "h1")
                .unwrap()
        );
        assert!(
            restored_with_tail_delete
                .contains_active("voyage-1024", "b", "h2")
                .unwrap()
        );
    }

    #[test]
    fn invalid_snapshot_falls_back_to_wal_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "a", "h1", vec![1.0, 0.0, 0.0, 0.0])
            .unwrap();
        drop(store);

        let restored = VectorStore::open(tmp.path()).unwrap();
        drop(restored);

        let snapshot_path = tmp.path().join("voyage-1024").join(SNAPSHOT_FILE);
        let mut snapshot = read_snapshot(&snapshot_path).unwrap();
        snapshot.route = "wrong-route".to_string();
        let mut encoded = Vec::new();
        encoded.extend_from_slice(VECTOR_SNAPSHOT_MAGIC);
        bincode::serialize_into(&mut encoded, &snapshot).unwrap();
        fs::write(&snapshot_path, encoded).unwrap();

        let rebuilt = VectorStore::open(tmp.path()).unwrap();
        let metrics = rebuilt.metrics();
        assert_eq!(metrics["voyage-1024"].wal_records, 1);
        assert_eq!(metrics["voyage-1024"].active_count, 1);
        assert!(rebuilt.contains_active("voyage-1024", "a", "h1").unwrap());
    }

    #[test]
    fn old_snapshot_without_magic_falls_back_to_wal_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "a", "h1", vec![1.0, 0.0, 0.0, 0.0])
            .unwrap();
        drop(store);

        let restored = VectorStore::open(tmp.path()).unwrap();
        drop(restored);

        let snapshot_path = tmp.path().join("voyage-1024").join(SNAPSHOT_FILE);
        let snapshot = read_snapshot(&snapshot_path).unwrap();
        fs::write(&snapshot_path, bincode::serialize(&snapshot).unwrap()).unwrap();

        let rebuilt = VectorStore::open(tmp.path()).unwrap();
        let metrics = rebuilt.metrics();
        assert_eq!(metrics["voyage-1024"].wal_records, 1);
        assert_eq!(metrics["voyage-1024"].active_count, 1);
        assert!(rebuilt.contains_active("voyage-1024", "a", "h1").unwrap());
    }

    #[test]
    fn snapshot_roundtrips_entries_without_upsert_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let mut slab = VectorSlab::new(2);
        slab.upsert_at("legacy", "h1", vec![1.0, 0.0], None)
            .unwrap();
        let snapshot = PartitionSnapshot {
            schema_version: VECTOR_SNAPSHOT_VERSION.to_string(),
            route: "voyage-1024".to_string(),
            wal_records: 1,
            wal_len_bytes: 123,
            hnsw_rebuilds: 0,
            slab,
            hnsw: None,
        };
        let snapshot_path = tmp.path().join(SNAPSHOT_FILE);
        let mut encoded = Vec::new();
        encoded.extend_from_slice(VECTOR_SNAPSHOT_MAGIC);
        bincode::serialize_into(&mut encoded, &snapshot).unwrap();
        fs::write(&snapshot_path, encoded).unwrap();

        let restored = read_snapshot(&snapshot_path).unwrap();
        let entry = restored.slab.active_entries().next().unwrap();
        assert_eq!(entry.entity_id, "legacy");
        assert_eq!(entry.upserted_at, None);
    }

    #[test]
    fn contains_active_uses_entity_and_content_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "a", "h1", vec![1.0, 0.0])
            .unwrap();

        assert!(store.contains_active("voyage-1024", "a", "h1").unwrap());
        assert!(!store.contains_active("voyage-1024", "a", "h2").unwrap());
        store.delete("voyage-1024", "a").unwrap();
        assert!(!store.contains_active("voyage-1024", "a", "h1").unwrap());
    }

    #[test]
    fn separate_routes_create_separate_partitions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "a", "h1", vec![1.0, 0.0])
            .unwrap();
        store
            .upsert("ollama-768", "b", "h2", vec![0.0, 1.0, 0.0])
            .unwrap();
        assert_eq!(store.partition_count(), 2);
        assert_eq!(store.metrics()["voyage-1024"].dims, 2);
        assert_eq!(store.metrics()["ollama-768"].dims, 3);
    }

    #[test]
    fn delete_entity_all_routes_writes_tombstones() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "same", "h1", vec![1.0, 0.0])
            .unwrap();
        store
            .upsert("ollama-768", "same", "h1", vec![0.0, 1.0, 0.0])
            .unwrap();
        store.delete_entity_all_routes("same").unwrap();

        assert_eq!(store.metrics()["voyage-1024"].active_count, 0);
        assert_eq!(store.metrics()["ollama-768"].active_count, 0);
        let voyage_records = wal::read_all(&tmp.path().join("voyage-1024").join("records.wal"))
            .expect("voyage WAL should read");
        let ollama_records = wal::read_all(&tmp.path().join("ollama-768").join("records.wal"))
            .expect("ollama WAL should read");
        assert!(voyage_records.last().unwrap().deleted_at.is_some());
        assert!(ollama_records.last().unwrap().deleted_at.is_some());
    }

    #[test]
    fn partition_metrics_distinguish_empty_from_active_dimensions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "same", "h1", vec![1.0, 0.0])
            .unwrap();

        let active = store.metrics().remove("voyage-1024").unwrap();
        assert_eq!(active.dims, 2);
        assert_eq!(active.state, PartitionState::Active { dims: 2 });

        store.delete("voyage-1024", "same").unwrap();
        let empty = store.metrics().remove("voyage-1024").unwrap();
        assert_eq!(empty.dims, 2);
        assert_eq!(empty.active_count, 0);
        assert_eq!(empty.state, PartitionState::Empty);
    }

    fn metrics_with_connectivity(active_nodes: usize, zero_in: usize) -> PartitionMetrics {
        PartitionMetrics {
            route: "voyage-1024".into(),
            state: PartitionState::Active { dims: 2 },
            dims: 2,
            wal_records: active_nodes,
            active_count: active_nodes,
            deleted_count: 0,
            deleted_ratio: 0.0,
            hnsw_rebuilds: 0,
            hnsw: Some(HnswMetricsSerde {
                total_nodes: active_nodes,
                active_nodes,
                deleted_nodes: 0,
                dimensions: 2,
                max_level: 0,
                entry_point: Some(0),
                neighbor_refs: active_nodes * 4,
                avg_neighbor_degree: 4.0,
                layer_distribution: vec![active_nodes],
                disconnected_nodes: zero_in,
                zero_in_degree_nodes: zero_in,
            }),
        }
    }

    #[test]
    fn connectivity_risk_ratio_is_zero_in_over_active() {
        let metrics = metrics_with_connectivity(10_000, 600);
        assert!((metrics.connectivity_risk_ratio() - 0.06).abs() < 1e-6);

        let healthy = metrics_with_connectivity(10_000, 0);
        assert_eq!(healthy.connectivity_risk_ratio(), 0.0);

        let graphless = PartitionMetrics {
            hnsw: None,
            ..metrics_with_connectivity(10_000, 600)
        };
        assert_eq!(graphless.connectivity_risk_ratio(), 0.0);
    }

    #[test]
    fn connectivity_breach_requires_threshold_and_size_floor() {
        // Over threshold, over the size floor: breach.
        assert!(
            metrics_with_connectivity(10_000, 600).connectivity_breach(COMPACT_CONNECTIVITY_RATIO)
        );
        // Under threshold: no breach.
        assert!(
            !metrics_with_connectivity(10_000, 100).connectivity_breach(COMPACT_CONNECTIVITY_RATIO)
        );
        // Tiny partition: ratio is noise, never a breach regardless of value.
        assert!(!metrics_with_connectivity(50, 10).connectivity_breach(COMPACT_CONNECTIVITY_RATIO));
    }

    #[test]
    fn self_recall_probe_reports_healthy_partition_near_one() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        for idx in 0..32 {
            let theta = idx as f32 * 0.1;
            store
                .upsert(
                    "voyage-1024",
                    &format!("id-{idx}"),
                    &format!("hash-{idx}"),
                    vec![theta.cos(), theta.sin()],
                )
                .unwrap();
        }
        let recall = store
            .self_recall_probe("voyage-1024", 1, 5)
            .unwrap()
            .expect("partition has a graph");
        assert!(recall > 0.9, "healthy graph self-recall was {recall}");
    }

    #[test]
    fn metrics_nonblocking_skips_write_locked_partitions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store.upsert("route-a", "a", "h1", vec![1.0, 0.0]).unwrap();
        store.upsert("route-b", "b", "h2", vec![0.0, 1.0]).unwrap();

        let partition_a = store.partition("route-a").unwrap();
        let _write_hold = partition_a.write();
        let metrics = store.metrics_nonblocking();
        assert!(!metrics.contains_key("route-a"));
        assert!(metrics.contains_key("route-b"));
    }

    #[test]
    fn rebuild_compacts_deleted_ordinals() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        for idx in 0..10 {
            let theta = idx as f32 * 0.01;
            store
                .upsert(
                    "voyage-1024",
                    &format!("id-{idx}"),
                    &format!("hash-{idx}"),
                    vec![theta.cos(), theta.sin()],
                )
                .unwrap();
        }
        for idx in 0..4 {
            store.delete("voyage-1024", &format!("id-{idx}")).unwrap();
        }

        let before = store.metrics().remove("voyage-1024").unwrap();
        assert_eq!(before.active_count, 6);
        assert_eq!(before.deleted_count, 4);
        assert!(before.deleted_ratio > 0.3);

        store.rebuild("voyage-1024").unwrap();
        let after = store.metrics().remove("voyage-1024").unwrap();
        assert_eq!(after.active_count, 6);
        assert_eq!(after.deleted_count, 0);
        assert_eq!(after.deleted_ratio, 0.0);
    }

    #[test]
    fn compact_partitions_processes_multiple_over_threshold_routes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        for route in ["route-a", "route-b"] {
            for idx in 0..10 {
                let theta = idx as f32 * 0.01;
                store
                    .upsert(
                        route,
                        &format!("{route}-id-{idx}"),
                        &format!("{route}-hash-{idx}"),
                        vec![theta.cos(), theta.sin()],
                    )
                    .unwrap();
            }
            for idx in 0..4 {
                store.delete(route, &format!("{route}-id-{idx}")).unwrap();
            }
        }

        let stats = store
            .compact_partitions_with_policy(0.30, 1, usize::MAX, None)
            .unwrap();

        assert_eq!(stats.len(), 2);
        for route in ["route-a", "route-b"] {
            let metrics = store.metrics().remove(route).unwrap();
            assert_eq!(metrics.active_count, 6);
            assert_eq!(metrics.deleted_count, 0);
            assert_eq!(metrics.deleted_ratio, 0.0);
            assert_eq!(
                store.search(route, &[1.0, 0.0], 3).unwrap().len(),
                3,
                "search should be served from the rebuilt snapshot for {route}"
            );
        }
    }

    #[test]
    fn upsert_uses_incremental_hnsw_insertion() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        let start_rebuilds = store
            .metrics()
            .get("voyage-1024")
            .map(|metrics| metrics.hnsw_rebuilds)
            .unwrap_or_default();

        for idx in 0..100 {
            let theta = idx as f32 * 0.01;
            store
                .upsert(
                    "voyage-1024",
                    &format!("id-{idx}"),
                    &format!("hash-{idx}"),
                    vec![theta.cos(), theta.sin(), 0.0, 0.0],
                )
                .unwrap();
        }

        let metrics = store.metrics();
        let partition = &metrics["voyage-1024"];
        assert_eq!(partition.active_count, 100);
        assert_eq!(partition.hnsw.as_ref().unwrap().total_nodes, 100);
        assert_eq!(
            partition
                .hnsw
                .as_ref()
                .unwrap()
                .layer_distribution
                .iter()
                .sum::<usize>(),
            partition.active_count
        );
        assert_eq!(partition.hnsw.as_ref().unwrap().disconnected_nodes, 0,);
        assert!(partition.hnsw_rebuilds <= start_rebuilds + 2);
    }

    #[test]
    fn incrementally_inserted_hnsw_search_returns_nearest_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        for idx in 0..100 {
            let theta = idx as f32 * 0.01;
            store
                .upsert(
                    "voyage-1024",
                    &format!("id-{idx}"),
                    &format!("hash-{idx}"),
                    vec![theta.cos(), theta.sin(), 0.0, 0.0],
                )
                .unwrap();
        }

        let hits = store
            .search("voyage-1024", &[1.0, 0.0, 0.0, 0.0], 5)
            .unwrap();

        assert_eq!(hits[0].id, "id-0");
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn dimension_mismatch_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "a", "h1", vec![1.0, 0.0])
            .unwrap();
        assert!(store.upsert("voyage-1024", "b", "h2", vec![1.0]).is_err());
    }

    #[test]
    fn partition_infos_cover_loaded_and_unloaded_partitions() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let store = VectorStore::open(tmp.path()).unwrap();
            store.upsert("route-a", "a", "h1", vec![1.0, 0.0]).unwrap();
            store.upsert("route-b", "b", "h2", vec![0.0, 1.0]).unwrap();
            store.flush_all().unwrap();
        }
        // Loaded view: both partitions carry metrics.
        let store = VectorStore::open(tmp.path()).unwrap();
        let infos = store.partition_infos().unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].route, "route-a");
        assert_eq!(infos[0].dims, Some(2));
        assert_eq!(infos[0].active_count, Some(1));
        assert!(infos[0].last_write.is_some());
        assert!(infos[0].disk_bytes > 0);
        drop(store);

        // Unloaded view (open_unloaded): rows still appear, disk-truthful,
        // with metrics absent instead of forcing a partition load.
        let store = VectorStore::open_unloaded(tmp.path()).unwrap();
        let infos = store.partition_infos().unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].dims, None);
        assert_eq!(infos[0].active_count, None);
        assert!(infos[0].disk_bytes > 0);
    }

    #[test]
    fn remove_partition_deletes_dir_and_map_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store.upsert("route-a", "a", "h1", vec![1.0, 0.0]).unwrap();
        store.upsert("route-b", "b", "h2", vec![0.0, 1.0]).unwrap();
        store.flush_all().unwrap();

        assert!(store.remove_partition("route-a").unwrap());
        assert!(!tmp.path().join("route-a").exists());
        assert_eq!(store.partition_count(), 1);
        let infos = store.partition_infos().unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].route, "route-b");

        // Absent partition reports false; survivors keep working.
        assert!(!store.remove_partition("route-a").unwrap());
        assert!(store.search("route-b", &[0.0, 1.0], 1).is_ok());
    }

    #[test]
    fn remove_partition_rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        for bad in ["", ".", "..", "a/b", "a\\b"] {
            assert!(store.remove_partition(bad).is_err(), "accepted `{bad}`");
        }
    }
}
