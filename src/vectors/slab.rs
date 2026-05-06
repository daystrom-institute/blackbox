use std::collections::HashMap;

use anyhow::{bail, Result};
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
    /// Maps `entity_id` to the index of the currently-active entry in
    /// `entries`. Tombstoned entries are NOT in this map; the upsert path
    /// flips the prior entry's `active=false` and re-points the index to
    /// the new entry. Skipped during serde load so the JSON disk format
    /// stays unchanged; rebuilt by `rebuild_active_index` on first
    /// non-trivial use.
    #[serde(skip)]
    active_index: HashMap<String, usize>,
    /// Once `rebuild_active_index` has run, subsequent ops keep the index
    /// in sync. Set to false on deserialize so the first index-using op
    /// rebuilds.
    #[serde(skip)]
    index_built: bool,
}

impl VectorSlab {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            entries: Vec::new(),
            active_index: HashMap::new(),
            index_built: true,
        }
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn active_count(&self) -> usize {
        // The active_index size matches the count of active entries when
        // built; fall back to a scan when it hasn't been (post-deserialize
        // immutable callers).
        if self.index_built {
            self.active_index.len()
        } else {
            self.entries.iter().filter(|entry| entry.active).count()
        }
    }

    pub fn active_entries(&self) -> impl Iterator<Item = &SlabEntry> {
        self.entries.iter().filter(|entry| entry.active)
    }

    /// O(1) lookup: an entry is "contained active" when its entity_id maps
    /// to a live slot whose content_hash matches. Replaces the prior
    /// `entries.iter().any(...)` linear scan that was O(n) per call —
    /// during a backfill, called O(N) times with N = corpus size, costing
    /// O(N^2) CPU on every reembed.
    pub fn contains_active(&self, entity_id: &str, content_hash: &str) -> bool {
        if !self.index_built {
            // Defensive fallback for callers that hit us before any
            // mutation rebuilds the index.
            return self.entries.iter().any(|entry| {
                entry.active && entry.entity_id == entity_id && entry.content_hash == content_hash
            });
        }
        match self.active_index.get(entity_id) {
            Some(&idx) => {
                let entry = &self.entries[idx];
                entry.active && entry.content_hash == content_hash
            }
            None => false,
        }
    }

    fn ensure_index_built(&mut self) {
        if self.index_built {
            return;
        }
        self.active_index.clear();
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.active {
                self.active_index.insert(entry.entity_id.clone(), i);
            }
        }
        self.index_built = true;
    }

    pub fn upsert(
        &mut self,
        entity_id: &str,
        content_hash: &str,
        vector: Vec<f32>,
    ) -> Result<bool> {
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
        self.ensure_index_built();
        if let Some(&idx) = self.active_index.get(entity_id) {
            let existing = &self.entries[idx];
            if existing.active && existing.content_hash == content_hash {
                return Ok(false);
            }
            // Different content_hash: tombstone the prior entry so a
            // future contains_active for the OLD hash returns false.
            self.entries[idx].active = false;
        }
        let new_idx = self.entries.len();
        self.entries.push(SlabEntry {
            entity_id: entity_id.to_string(),
            content_hash: content_hash.to_string(),
            vector,
            active: true,
        });
        self.active_index.insert(entity_id.to_string(), new_idx);
        Ok(true)
    }

    pub fn delete(&mut self, entity_id: &str) -> bool {
        self.ensure_index_built();
        match self.active_index.remove(entity_id) {
            Some(idx) => {
                self.entries[idx].active = false;
                true
            }
            None => false,
        }
    }

    pub fn to_f32_slab(&self) -> Vec<f32> {
        self.active_entries()
            .flat_map(|entry| entry.vector.iter().copied())
            .collect()
    }
}
