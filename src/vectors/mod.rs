#![allow(dead_code)] // E3 lands vector API; H1 wires search callers.

pub mod distance;
pub mod hnsw;
pub mod slab;
pub mod wal;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use parking_lot::RwLock;
#[cfg(test)]
use parking_lot::{Mutex, MutexGuard};
use serde::{Deserialize, Serialize};

use self::hnsw::{HnswIndex, HnswMetrics, HnswOptions, SearchHit};
use self::slab::VectorSlab;
use self::wal::WalRecord;

const VECTOR_SCHEMA_VERSION: &str = "agentic-corpus-e3";

static GLOBAL_STORE: OnceLock<Arc<VectorStore>> = OnceLock::new();

#[cfg(test)]
static TEST_GLOBAL_STORE: OnceLock<RwLock<Option<Arc<VectorStore>>>> = OnceLock::new();
#[cfg(test)]
static TEST_GLOBAL_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(test)]
fn test_global_store() -> &'static RwLock<Option<Arc<VectorStore>>> {
    TEST_GLOBAL_STORE.get_or_init(|| RwLock::new(None))
}

#[cfg(test)]
pub struct TestGlobalStoreGuard {
    previous: Option<Arc<VectorStore>>,
    _lock: MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestGlobalStoreGuard {
    fn drop(&mut self) {
        *test_global_store().write() = self.previous.take();
    }
}

#[cfg(test)]
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
    spawn_periodic_flusher(store);
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

pub fn global() -> Arc<VectorStore> {
    #[cfg(test)]
    if let Some(store) = test_global_store().read().clone() {
        return store;
    }

    GLOBAL_STORE
        .get_or_init(|| {
            let store = Arc::new(
                VectorStore::open(default_vectors_dir()).expect("default vector store should open"),
            );
            spawn_periodic_flusher(store.clone());
            store
        })
        .clone()
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

pub fn search(route: &str, query: &[f32], k: usize) -> Result<Vec<SearchHit>> {
    global().search(route, query, k)
}

pub(crate) fn iter_active(
    route: &str,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<impl Iterator<Item = VectorEntry>> {
    global().iter_active(route, since)
}

pub(crate) fn active_entity_hashes(route: &str) -> Result<Vec<(String, String)>> {
    global().active_entity_hashes(route)
}

pub(crate) fn cluster_neighbors_within_route(
    route: &str,
    similarity_threshold: f32,
) -> Result<Vec<VectorCluster>> {
    global().cluster_neighbors_within_route(route, similarity_threshold)
}

pub fn rebuild(route: &str) -> Result<()> {
    global().rebuild(route)
}

pub fn metrics() -> BTreeMap<String, PartitionMetrics> {
    global().metrics()
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

    /// Force-flush every partition's derived files INCLUDING slab.bin.
    /// Called on graceful shutdown so the next start can mmap slab.bin
    /// and skip rebuild_from_wal. Also fsyncs every dirty WAL.
    pub fn flush_all(&self) -> Result<()> {
        let partitions: Vec<_> = self.partitions.read().values().cloned().collect();
        for partition in partitions {
            let mut partition = partition.write();
            partition.flush_derived_full()?;
        }
        crate::vectors::wal::sync_pending().ok();
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
    ) -> Result<impl Iterator<Item = VectorEntry>> {
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
        let result = partition.write().rebuild_from_wal();
        result
    }

    pub fn partition_count(&self) -> usize {
        self.partitions.read().len()
    }

    pub fn metrics(&self) -> BTreeMap<String, PartitionMetrics> {
        self.partitions
            .read()
            .iter()
            .map(|(route, partition)| (route.clone(), partition.read().metrics()))
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
        let partition = Arc::new(RwLock::new(Partition::open(route.to_string(), path)?));
        self.partitions
            .write()
            .insert(route.to_string(), partition.clone());
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
        };
        partition.rebuild_from_wal()?;
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
        let records = wal::read_all(&self.wal_path())?;
        self.slab = VectorSlab::default();
        for record in &records {
            if record.deleted_at.is_some() {
                self.slab.delete(&record.entity_id);
            } else {
                self.slab.upsert_at(
                    &record.entity_id,
                    &record.content_hash,
                    record.vector.clone(),
                    record.upserted_at.clone(),
                )?;
            }
        }
        self.wal_records = records.len();
        self.rebuild_hnsw()?;
        // After a full rebuild, materialize slab.bin so subsequent cold
        // starts can mmap it instead of replaying the WAL again.
        self.flush_derived_full()
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
    /// partition was hot. NONE of those files were ever read; the cold-
    /// start path is always `rebuild_from_wal`. We dropped them in the
    /// disk-hammer post-mortem (see thread-3e2a0cfa).
    ///
    /// `flush_derived_full` is the same operation today — kept as a
    /// distinct alias so future durability extensions can branch on
    /// "checkpoint vs full snapshot" without churning callers.
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
        crate::vectors::wal::sync_path(&self.wal_path()).ok();
        self.last_flushed_wal_records = self.wal_records;
        Ok(())
    }

    /// Identical to `flush_derived_files` today. Kept as a distinct
    /// name so a future change that adds a snapshot file (mmap'able
    /// slab cache to skip rebuild_from_wal on huge corpora) has a
    /// natural place to land without churning every caller.
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

    fn wal_path(&self) -> PathBuf {
        self.path.join("records.wal")
    }
}

// `write_f32_file` was removed alongside the slab.bin / ids.bin / graph.bin
// per-flush writes. Cold-start always rebuilds via WAL, so the derived
// raw-f32 dump had no consumer. Kept the bulk-write helper notes in git
// history (commit d8cd57d) in case a future snapshot path needs them.

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
    for idx in 0..entries.len() {
        let root = find(&mut parent, idx);
        groups
            .entry(root)
            .or_default()
            .push(entries[idx].entity_id.clone());
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
}
