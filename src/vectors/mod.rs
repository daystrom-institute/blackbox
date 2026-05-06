#![allow(dead_code)] // E3 lands vector API; H1 wires search callers.

pub mod distance;
pub mod hnsw;
pub mod slab;
pub mod wal;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use self::hnsw::{HnswIndex, HnswMetrics, HnswOptions, SearchHit};
use self::slab::VectorSlab;
use self::wal::WalRecord;

const VECTOR_SCHEMA_VERSION: &str = "agentic-corpus-e3";

static GLOBAL_STORE: OnceLock<Arc<VectorStore>> = OnceLock::new();

pub fn install_global(store: Arc<VectorStore>) {
    let _ = GLOBAL_STORE.set(store);
}

pub fn global() -> Arc<VectorStore> {
    GLOBAL_STORE
        .get_or_init(|| {
            Arc::new(
                VectorStore::open(default_vectors_dir()).expect("default vector store should open"),
            )
        })
        .clone()
}

pub fn upsert(route: &str, entity_id: &str, content_hash: &str, vector: Vec<f32>) -> Result<()> {
    global().upsert(route, entity_id, content_hash, vector)
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

pub fn rebuild(route: &str) -> Result<()> {
    global().rebuild(route)
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
        let result = partition
            .write()
            .upsert(entity_id, content_hash, vector)
            .with_context(|| format!("upserting vector entity {entity_id} into {route}"));
        result
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
    pub dims: usize,
    pub wal_records: usize,
    pub active_count: usize,
    pub hnsw: Option<HnswMetricsSerde>,
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
        };
        partition.rebuild_from_wal()?;
        Ok(partition)
    }

    fn upsert(&mut self, entity_id: &str, content_hash: &str, vector: Vec<f32>) -> Result<()> {
        let record = WalRecord::upsert(&self.route, entity_id, content_hash, vector.clone());
        if !self.slab.upsert(entity_id, content_hash, vector)? {
            return Ok(());
        }
        wal::append(&self.wal_path(), &record)?;
        self.wal_records += 1;
        self.rebuild_hnsw()?;
        self.write_derived_files()
    }

    fn delete(&mut self, entity_id: &str) -> Result<()> {
        let record = WalRecord::delete(&self.route, entity_id);
        wal::append(&self.wal_path(), &record)?;
        self.wal_records += 1;
        self.slab.delete(entity_id);
        self.rebuild_hnsw()?;
        self.write_derived_files()
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

    fn rebuild_from_wal(&mut self) -> Result<()> {
        let records = wal::read_all(&self.wal_path())?;
        self.slab = VectorSlab::default();
        for record in &records {
            if record.deleted_at.is_some() {
                self.slab.delete(&record.entity_id);
            } else {
                self.slab.upsert(
                    &record.entity_id,
                    &record.content_hash,
                    record.vector.clone(),
                )?;
            }
        }
        self.wal_records = records.len();
        self.rebuild_hnsw()?;
        self.write_derived_files()
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
        Ok(())
    }

    fn metrics(&self) -> PartitionMetrics {
        PartitionMetrics {
            route: self.route.clone(),
            dims: self.slab.dims(),
            wal_records: self.wal_records,
            active_count: self.slab.active_count(),
            hnsw: self.hnsw.as_ref().map(|hnsw| hnsw.metrics().into()),
        }
    }

    fn write_derived_files(&self) -> Result<()> {
        fs::create_dir_all(&self.path)?;
        fs::write(
            self.path.join("meta.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": VECTOR_SCHEMA_VERSION,
                "route": self.route,
                "dims": self.slab.dims(),
                "wal_records": self.wal_records,
                "active_count": self.slab.active_count(),
                "m": HnswOptions::default().m,
                "ef_construction": HnswOptions::default().ef_construction,
                "ef_search": HnswOptions::default().ef_search,
                "max_layers": HnswOptions::default().max_layers,
            }))?,
        )?;
        write_f32_file(&self.path.join("slab.bin"), &self.slab.to_f32_slab())?;
        fs::write(
            self.path.join("ids.bin"),
            self.slab
                .active_entries()
                .map(|entry| entry.entity_id.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )?;
        fs::write(
            self.path.join("graph.bin"),
            serde_json::to_vec(&self.metrics().hnsw)?,
        )?;
        Ok(())
    }

    fn wal_path(&self) -> PathBuf {
        self.path.join("records.wal")
    }
}

fn write_f32_file(path: &Path, values: &[f32]) -> Result<()> {
    let mut file =
        fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    for value in values {
        file.write_all(&value.to_le_bytes())?;
    }
    file.sync_data()?;
    Ok(())
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
    fn dimension_mismatch_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VectorStore::open(tmp.path()).unwrap();
        store
            .upsert("voyage-1024", "a", "h1", vec![1.0, 0.0])
            .unwrap();
        assert!(store.upsert("voyage-1024", "b", "h2", vec![1.0]).is_err());
    }
}
