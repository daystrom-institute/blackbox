use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlabEntry {
    pub entity_id: String,
    pub content_hash: String,
    pub vector: Vec<f32>,
    pub active: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VectorSlab {
    dims: usize,
    entries: Vec<SlabEntry>,
}

impl VectorSlab {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            entries: Vec::new(),
        }
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn active_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.active).count()
    }

    pub fn active_entries(&self) -> impl Iterator<Item = &SlabEntry> {
        self.entries.iter().filter(|entry| entry.active)
    }

    pub fn upsert(&mut self, entity_id: &str, content_hash: &str, vector: Vec<f32>) -> Result<bool> {
        if self.dims == 0 {
            self.dims = vector.len();
        }
        if vector.len() != self.dims {
            bail!(
                "vector dimension mismatch for {entity_id}: got {}, expected {}",
                vector.len(),
                self.dims
            );
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.active && entry.entity_id == entity_id && entry.content_hash == content_hash)
        {
            return Ok(false);
        }
        for entry in &mut self.entries {
            if entry.active && entry.entity_id == entity_id {
                entry.active = false;
            }
        }
        self.entries.push(SlabEntry {
            entity_id: entity_id.to_string(),
            content_hash: content_hash.to_string(),
            vector,
            active: true,
        });
        Ok(true)
    }

    pub fn delete(&mut self, entity_id: &str) -> bool {
        let mut deleted = false;
        for entry in &mut self.entries {
            if entry.active && entry.entity_id == entity_id {
                entry.active = false;
                deleted = true;
            }
        }
        deleted
    }

    pub fn to_f32_slab(&self) -> Vec<f32> {
        self.active_entries()
            .flat_map(|entry| entry.vector.iter().copied())
            .collect()
    }
}
