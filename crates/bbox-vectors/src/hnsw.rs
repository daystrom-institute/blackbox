use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::time::Instant;

use ordered_float::NotNan;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::distance::cosine_distance;

thread_local! {
    static VISITED_ARENA: RefCell<VisitedArena> = RefCell::new(VisitedArena::default());
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HnswOptions {
    pub m: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
    pub max_layers: usize,
}

impl Default for HnswOptions {
    fn default() -> Self {
        Self {
            m: 32,
            ef_construction: 200,
            ef_search: 200,
            max_layers: 6,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub id: String,
    pub distance: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HnswMetrics {
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
    /// Active nodes with no inbound edge from any active node. The leading
    /// indicator of reverse-edge-prune orphaning (gap-2eabd96d): out-degree
    /// stats cannot see it, and a zero-in-degree node is unreachable by
    /// graph traversal at any ef.
    pub zero_in_degree_nodes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HnswIndex {
    options: HnswOptions,
    m0: usize,
    ml: f64,
    vectors: VectorSlab,
    ids: Vec<String>,
    levels: Vec<usize>,
    active: Vec<bool>,
    graph: Vec<Vec<Vec<usize>>>,
    entry_point: Option<usize>,
    max_level: isize,
    /// Derived active-only lookup. Historical tombstoned duplicate ids stay in
    /// `ids`, but at most one active ordinal may exist for an id.
    #[serde(skip)]
    active_ordinal_by_id: HashMap<String, usize>,
    /// False after snapshot deserialization; rebuilt and validated before the
    /// first id-based mutation. Skipped so the snapshot wire shape is stable.
    #[serde(skip)]
    active_index_built: bool,
}

impl HnswIndex {
    pub fn empty(dimensions: usize, options: HnswOptions) -> Result<Self, String> {
        if dimensions == 0 {
            return Err("embedding dimensions must be positive".to_string());
        }
        let m0 = options.m * 2;
        let ml = 1.0 / (options.m as f64).ln();
        Ok(Self {
            options,
            m0,
            ml,
            vectors: VectorSlab {
                dimensions,
                data: Vec::new(),
            },
            ids: Vec::new(),
            levels: Vec::new(),
            active: Vec::new(),
            graph: Vec::new(),
            entry_point: None,
            max_level: -1,
            active_ordinal_by_id: HashMap::new(),
            active_index_built: true,
        })
    }

    pub fn build(items: Vec<(String, Vec<f32>)>, options: HnswOptions) -> Result<Self, String> {
        let Some((_, first)) = items.first() else {
            return Self::empty(1, options);
        };
        let dimensions = first.len();
        if dimensions == 0 || !items.iter().all(|(_, vector)| vector.len() == dimensions) {
            return Err("dimension mismatch".to_string());
        }
        let mut index = Self::empty(dimensions, options)?;
        let count = items.len();
        index.ids = items.iter().map(|(id, _)| id.clone()).collect();
        index.vectors.data = items
            .into_iter()
            .flat_map(|(_, vector)| vector)
            .collect::<Vec<_>>();
        index.levels = vec![0; count];
        index.active = vec![true; count];
        index.graph = vec![vec![Vec::new(); index.options.max_layers]; count];
        for (ordinal, id) in index.ids.iter().enumerate() {
            if index
                .active_ordinal_by_id
                .insert(id.clone(), ordinal)
                .is_some()
            {
                return Err(format!("duplicate active HNSW id: {id}"));
            }
        }

        let mut ordered = (0..count)
            .map(|ordinal| (ordinal, index.deterministic_level(&index.ids[ordinal])))
            .collect::<Vec<_>>();
        ordered.sort_by_key(|(ordinal, level)| (Reverse(*level), *ordinal));

        for (idx, (ordinal, _)) in ordered.into_iter().enumerate() {
            let ramped_ef = index
                .options
                .ef_construction
                .min(50.max(index.options.ef_construction * idx.min(1000) / 1000));
            index.insert_internal_with_ef(ordinal, ramped_ef);
        }
        Ok(index)
    }

    pub fn search(&self, query: &[f32], k: usize) -> Vec<SearchHit> {
        if self.entry_point.is_none() || k == 0 || query.len() != self.vectors.dimensions {
            return Vec::new();
        }
        let mut entry = self.entry_point.unwrap();
        for layer in (1..=self.max_level.max(1) as usize).rev() {
            entry = self.greedy_closest(query, entry, layer);
        }

        let ef_search = self
            .options
            .ef_search
            .max(k)
            .max((self.ids.len() / 4).min(2_000));
        self.search_layer(query, &[entry], ef_search, 0)
            .into_iter()
            .filter(|(ordinal, _)| self.active[*ordinal])
            .take(k)
            .map(|(ordinal, distance)| SearchHit {
                id: self.ids[ordinal].clone(),
                distance,
            })
            .collect()
    }

    /// Full graph diagnostics. This walks graph connectivity and must never be
    /// used for mutation bookkeeping, checkpointing, or cheap status.
    pub fn diagnostics(&self) -> HnswMetrics {
        self.diagnostics_checked(None)
            .expect("unbounded HNSW diagnostics cannot time out")
    }

    /// Full graph diagnostics with a hard cooperative deadline. Each linear
    /// graph pass checks the deadline at bounded intervals so an explicit
    /// diagnostic request cannot monopolize the daemon indefinitely.
    pub fn diagnostics_before(&self, deadline: Instant) -> Result<HnswMetrics, String> {
        self.diagnostics_checked(Some(deadline))
    }

    fn diagnostics_checked(&self, deadline: Option<Instant>) -> Result<HnswMetrics, String> {
        let mut active_nodes = 0usize;
        let mut deleted_nodes = 0usize;
        let mut neighbor_refs = 0usize;
        for (ordinal, layers) in self.graph.iter().enumerate() {
            Self::check_diagnostic_deadline(deadline, ordinal)?;
            if !self.active[ordinal] {
                deleted_nodes += 1;
                continue;
            }
            active_nodes += 1;
            for neighbors in layers {
                neighbor_refs += neighbors
                    .iter()
                    .filter(|neighbor| self.active[**neighbor])
                    .count();
            }
        }
        Ok(HnswMetrics {
            total_nodes: self.ids.len(),
            active_nodes,
            deleted_nodes,
            dimensions: self.vectors.dimensions,
            max_level: self.max_level,
            entry_point: self.entry_point,
            neighbor_refs,
            avg_neighbor_degree: if active_nodes == 0 {
                0.0
            } else {
                neighbor_refs as f64 / active_nodes as f64
            },
            layer_distribution: self.layer_distribution(deadline)?,
            disconnected_nodes: self.disconnected_nodes(deadline)?,
            zero_in_degree_nodes: self.zero_in_degree_nodes(deadline)?,
        })
    }

    fn check_diagnostic_deadline(deadline: Option<Instant>, progress: usize) -> Result<(), String> {
        if progress.is_multiple_of(1024)
            && deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err("HNSW diagnostic deadline exceeded".to_string());
        }
        Ok(())
    }

    fn zero_in_degree_nodes(&self, deadline: Option<Instant>) -> Result<usize, String> {
        let mut has_inbound = vec![false; self.ids.len()];
        if let Some(entry_point) = self.entry_point {
            // The entry point is reachable by definition.
            has_inbound[entry_point] = true;
        }
        for (ordinal, layers) in self.graph.iter().enumerate() {
            Self::check_diagnostic_deadline(deadline, ordinal)?;
            if !self.active[ordinal] {
                continue;
            }
            for neighbors in layers {
                for &neighbor in neighbors {
                    has_inbound[neighbor] = true;
                }
            }
        }
        Ok(self
            .active
            .iter()
            .zip(has_inbound.iter())
            .filter(|(active, inbound)| **active && !**inbound)
            .count())
    }

    /// Sampled self-recall: search every `sample_every`-th active vector and
    /// report the fraction whose own id appears in its top-`k`. A healthy
    /// graph scores ~1.0; reverse-edge orphaning (gap-2eabd96d) drags this
    /// down because orphaned vectors cannot be reached for any query.
    /// O(sample * search) — a diagnostic probe, not a metrics()-path stat.
    pub fn self_recall_probe(&self, sample_every: usize, k: usize) -> f64 {
        let step = sample_every.max(1);
        let mut sampled = 0usize;
        let mut hits = 0usize;
        for ordinal in (0..self.ids.len()).step_by(step) {
            if !self.active[ordinal] {
                continue;
            }
            sampled += 1;
            let query = self.vectors.get(ordinal).to_vec();
            if self
                .search(&query, k)
                .iter()
                .any(|hit| hit.id == self.ids[ordinal])
            {
                hits += 1;
            }
        }
        if sampled == 0 {
            return 1.0;
        }
        hits as f64 / sampled as f64
    }

    pub fn push(&mut self, id: String, vector: Vec<f32>) -> Result<(), String> {
        if vector.len() != self.vectors.dimensions {
            return Err("dimension mismatch".to_string());
        }
        self.ensure_active_index()?;
        let deleted_entry_point = self
            .active_ordinal_by_id
            .remove(&id)
            .map(|ordinal| {
                self.active[ordinal] = false;
                self.entry_point == Some(ordinal)
            })
            .unwrap_or(false);
        if deleted_entry_point {
            self.repair_entry_point();
        }
        let ordinal = self.ids.len();
        self.vectors.data.extend(vector);
        self.ids.push(id.clone());
        self.levels.push(0);
        self.active.push(true);
        self.graph.push(vec![Vec::new(); self.options.max_layers]);
        self.insert_internal(ordinal);
        self.active_ordinal_by_id.insert(id, ordinal);
        Ok(())
    }

    pub fn delete(&mut self, id: &str) -> Result<bool, String> {
        self.ensure_active_index()?;
        let Some(ordinal) = self.active_ordinal_by_id.remove(id) else {
            return Ok(false);
        };
        self.active[ordinal] = false;
        let deleted_entry_point = self.entry_point == Some(ordinal);
        if deleted_entry_point {
            self.repair_entry_point();
        }
        Ok(true)
    }

    /// Tombstone active ids in O(requested ids), repairing the graph entry
    /// point once after the batch rather than once per entity.
    pub fn delete_many(&mut self, ids: &[String]) -> Result<usize, String> {
        self.ensure_active_index()?;
        let mut deleted = 0usize;
        let mut deleted_entry_point = false;
        let mut seen = HashSet::with_capacity(ids.len());
        for id in ids {
            if !seen.insert(id.as_str()) {
                continue;
            }
            let Some(ordinal) = self.active_ordinal_by_id.remove(id) else {
                continue;
            };
            self.active[ordinal] = false;
            deleted += 1;
            deleted_entry_point |= self.entry_point == Some(ordinal);
        }
        if deleted_entry_point {
            self.repair_entry_point();
        }
        Ok(deleted)
    }

    pub fn active_count(&self) -> usize {
        self.active.iter().filter(|active| **active).count()
    }

    pub fn deleted_count(&self) -> usize {
        self.active.len().saturating_sub(self.active_count())
    }

    pub fn dimensions(&self) -> usize {
        self.vectors.dimensions
    }

    /// Rebuild derived id lookup after snapshot deserialization and refuse an
    /// impossible snapshot containing more than one active ordinal per id.
    pub fn rebuild_active_index(&mut self) -> Result<(), String> {
        if self.ids.len() != self.active.len()
            || self.ids.len() != self.levels.len()
            || self.ids.len() != self.graph.len()
            || self.ids.len().checked_mul(self.vectors.dimensions) != Some(self.vectors.data.len())
        {
            self.active_index_built = false;
            return Err("inconsistent HNSW snapshot vector lengths".to_string());
        }
        self.active_ordinal_by_id.clear();
        for (ordinal, id) in self.ids.iter().enumerate() {
            if !self.active[ordinal] {
                continue;
            }
            if self
                .active_ordinal_by_id
                .insert(id.clone(), ordinal)
                .is_some()
            {
                self.active_index_built = false;
                return Err(format!("duplicate active HNSW id: {id}"));
            }
        }
        self.active_index_built = true;
        Ok(())
    }

    fn ensure_active_index(&mut self) -> Result<(), String> {
        if !self.active_index_built {
            self.rebuild_active_index()?;
        }
        Ok(())
    }

    fn repair_entry_point(&mut self) {
        let Some((ordinal, level)) = self
            .active
            .iter()
            .enumerate()
            .filter(|(_, active)| **active)
            .map(|(ordinal, _)| (ordinal, self.levels[ordinal]))
            .max_by_key(|(ordinal, level)| (*level, std::cmp::Reverse(*ordinal)))
        else {
            self.entry_point = None;
            self.max_level = -1;
            return;
        };
        self.entry_point = Some(ordinal);
        self.max_level = level as isize;
    }

    fn layer_distribution(&self, deadline: Option<Instant>) -> Result<Vec<usize>, String> {
        let mut by_layer = vec![0usize; self.options.max_layers];
        for (ordinal, level) in self.levels.iter().copied().enumerate() {
            Self::check_diagnostic_deadline(deadline, ordinal)?;
            if !self.active[ordinal] {
                continue;
            }
            by_layer[level.min(self.options.max_layers - 1)] += 1;
        }
        Ok(by_layer)
    }

    fn disconnected_nodes(&self, deadline: Option<Instant>) -> Result<usize, String> {
        let Some(entry_point) = self.entry_point else {
            return Ok(0);
        };
        let mut frontier = VecDeque::new();
        let mut visited = vec![false; self.ids.len()];
        visited[entry_point] = true;
        frontier.push_back(entry_point);

        let mut visited_count = 0usize;
        while let Some(current) = frontier.pop_front() {
            Self::check_diagnostic_deadline(deadline, visited_count)?;
            visited_count += 1;
            for neighbors in &self.graph[current] {
                for &neighbor in neighbors {
                    if !self.active[neighbor] || visited[neighbor] {
                        continue;
                    }
                    visited[neighbor] = true;
                    frontier.push_back(neighbor);
                }
            }
        }
        Ok(self
            .active
            .iter()
            .zip(visited.iter())
            .filter(|(active, visited)| **active && !**visited)
            .count())
    }

    fn insert_internal(&mut self, ordinal: usize) {
        self.insert_internal_with_ef(ordinal, self.options.ef_construction);
    }

    fn insert_internal_with_ef(&mut self, ordinal: usize, ef_construction: usize) {
        let level = self.deterministic_level(&self.ids[ordinal]);
        self.levels[ordinal] = level;
        let Some(entry_point) = self.entry_point else {
            self.entry_point = Some(ordinal);
            self.max_level = level as isize;
            return;
        };
        let mut ep = entry_point;
        if self.max_level as usize > level {
            for layer in ((level + 1)..=self.max_level as usize).rev() {
                ep = self.greedy_closest(self.vectors.get(ordinal), ep, layer);
            }
        }
        for layer in (0..=level.min(self.max_level.max(0) as usize)).rev() {
            let candidates =
                self.search_layer(self.vectors.get(ordinal), &[ep], ef_construction, layer);
            let selected = self.select_neighbors(&candidates, self.max_neighbors(layer));
            self.graph[ordinal][layer] = selected.clone();
            for neighbor in selected {
                self.add_reverse_edge(neighbor, ordinal, layer);
            }
            if let Some((next_ep, _)) = candidates.first() {
                ep = *next_ep;
            }
        }
        if level as isize > self.max_level {
            self.entry_point = Some(ordinal);
            self.max_level = level as isize;
        }
    }

    fn greedy_closest(&self, query: &[f32], ep: usize, layer: usize) -> usize {
        let mut best = ep;
        let mut best_dist = self.distance_to_ordinal(query, ep);
        loop {
            let mut changed = false;
            for neighbor in self.graph[best][layer].iter().copied() {
                if !self.active[neighbor] {
                    continue;
                }
                let dist = self.distance_to_ordinal(query, neighbor);
                if dist < best_dist {
                    best = neighbor;
                    best_dist = dist;
                    changed = true;
                }
            }
            if !changed {
                return best;
            }
        }
    }

    fn search_layer(
        &self,
        query: &[f32],
        entry_points: &[usize],
        ef: usize,
        layer: usize,
    ) -> Vec<(usize, f32)> {
        VISITED_ARENA.with_borrow_mut(|visited| {
            visited.prepare(self.ids.len());
            let mut candidates = MinCandidateQueue::default();
            let mut results = MaxResultQueue::new(ef);
            for ep in entry_points.iter().copied() {
                if visited.mark(ep) {
                    let distance = self.distance_to_ordinal(query, ep);
                    candidates.push(ep, distance);
                    results.push(ep, distance);
                }
            }
            while let Some((current, current_dist)) = candidates.pop() {
                if results.is_full() && current_dist > results.worst_distance() {
                    break;
                }
                for neighbor in self.graph[current][layer].iter().copied() {
                    if !self.active[neighbor] {
                        continue;
                    }
                    if !visited.mark(neighbor) {
                        continue;
                    }
                    let dist = self.distance_to_ordinal(query, neighbor);
                    if !results.is_full() || dist < results.worst_distance() {
                        candidates.push(neighbor, dist);
                        results.push(neighbor, dist);
                    }
                }
            }
            results.into_sorted_vec()
        })
    }

    fn select_neighbors(&self, candidates: &[(usize, f32)], max_neighbors: usize) -> Vec<usize> {
        if candidates.len() <= max_neighbors {
            return candidates.iter().map(|(ordinal, _)| *ordinal).collect();
        }

        let mut sorted = candidates.to_vec();
        sorted.sort_by(|left, right| left.1.total_cmp(&right.1));

        let mut selected = Vec::with_capacity(max_neighbors);
        for (candidate, candidate_dist) in &sorted {
            if selected.len() >= max_neighbors {
                break;
            }
            let candidate_vec = self.vectors.get(*candidate);
            let too_close = selected.iter().any(|selected| {
                cosine_distance(candidate_vec, self.vectors.get(*selected)) < *candidate_dist
            });
            if !too_close {
                selected.push(*candidate);
            }
        }

        if selected.len() < max_neighbors {
            for (candidate, _) in sorted {
                if selected.len() >= max_neighbors {
                    break;
                }
                if !selected.contains(&candidate) {
                    selected.push(candidate);
                }
            }
        }

        selected
    }

    /// Add `new_node` to `neighbor`'s adjacency, shrinking a saturated list
    /// with the same diversity heuristic as forward selection (the HNSW
    /// paper's SHRINK-CONNECTIONS) instead of distance-sort-truncate.
    ///
    /// Pure-distance truncation mass-orphans members of near-duplicate
    /// clusters larger than max_neighbors: every saturated list in the
    /// cluster evicts the same global losers, their in-degree hits zero,
    /// and search (forward-edge traversal from the entry point) can never
    /// reach them again (gap-2eabd96d: ~17% of the prod partition
    /// disconnected; 35% self-recall loss in the cluster repro). The
    /// diversity rule decorrelates evictions — a member pruned from one
    /// list because it sits closer to a kept neighbor stays reachable
    /// through that neighbor — and `select_neighbors`' distance backfill
    /// doubles as keep-pruned-connections.
    fn add_reverse_edge(&mut self, neighbor: usize, new_node: usize, layer: usize) {
        let max_n = self.max_neighbors(layer);
        if self.graph[neighbor][layer].len() < max_n {
            self.graph[neighbor][layer].push(new_node);
            return;
        }
        let neighbor_vec = self.vectors.get(neighbor);
        let mut candidates = self.graph[neighbor][layer]
            .iter()
            .copied()
            .chain(std::iter::once(new_node))
            .map(|ordinal| (ordinal, self.distance_to_ordinal(neighbor_vec, ordinal)))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.1.total_cmp(&right.1));
        self.graph[neighbor][layer] = self.select_neighbors(&candidates, max_n);
    }

    fn max_neighbors(&self, layer: usize) -> usize {
        if layer == 0 { self.m0 } else { self.options.m }
    }

    fn deterministic_level(&self, id: &str) -> usize {
        let mut hasher = Sha256::new();
        hasher.update(id.as_bytes());
        let hash = hasher.finalize();
        let hash_val = u64::from_le_bytes(hash[0..8].try_into().expect("sha256 prefix"));
        let unit = ((hash_val as f64) / (u64::MAX as f64)).max(1.0e-15);
        ((-unit.ln() * self.ml).floor() as usize).min(self.options.max_layers - 1)
    }

    fn distance_to_ordinal(&self, query: &[f32], ordinal: usize) -> f32 {
        cosine_distance(query, self.vectors.get(ordinal))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct VectorSlab {
    dimensions: usize,
    data: Vec<f32>,
}

impl VectorSlab {
    fn get(&self, ordinal: usize) -> &[f32] {
        let start = ordinal * self.dimensions;
        &self.data[start..start + self.dimensions]
    }
}

#[derive(Default)]
struct VisitedArena {
    epochs: Vec<u32>,
    current_epoch: u32,
}

impl VisitedArena {
    fn prepare(&mut self, len: usize) {
        if self.epochs.len() < len {
            self.epochs.resize(len, 0);
        }
        self.current_epoch = self.current_epoch.wrapping_add(1);
        if self.current_epoch == 0 {
            self.epochs.fill(0);
            self.current_epoch = 1;
        }
    }

    fn mark(&mut self, ordinal: usize) -> bool {
        if self.epochs[ordinal] == self.current_epoch {
            return false;
        }
        self.epochs[ordinal] = self.current_epoch;
        true
    }
}

#[derive(Default)]
struct MinCandidateQueue {
    heap: BinaryHeap<Reverse<(NotNan<f32>, usize)>>,
}

impl MinCandidateQueue {
    fn push(&mut self, ordinal: usize, distance: f32) {
        self.heap.push(Reverse((not_nan(distance), ordinal)));
    }

    fn pop(&mut self) -> Option<(usize, f32)> {
        self.heap
            .pop()
            .map(|Reverse((distance, ordinal))| (ordinal, distance.into_inner()))
    }
}

struct MaxResultQueue {
    ef: usize,
    heap: BinaryHeap<(NotNan<f32>, usize)>,
}

impl MaxResultQueue {
    fn new(ef: usize) -> Self {
        Self {
            ef,
            heap: BinaryHeap::with_capacity(ef + 1),
        }
    }

    fn push(&mut self, ordinal: usize, distance: f32) {
        self.heap.push((not_nan(distance), ordinal));
        if self.heap.len() > self.ef {
            self.heap.pop();
        }
    }

    fn is_full(&self) -> bool {
        self.heap.len() >= self.ef
    }

    fn worst_distance(&self) -> f32 {
        self.heap
            .peek()
            .map(|(distance, _)| distance.into_inner())
            .unwrap_or(f32::INFINITY)
    }

    fn into_sorted_vec(self) -> Vec<(usize, f32)> {
        let mut results = self
            .heap
            .into_iter()
            .map(|(distance, ordinal)| (ordinal, distance.into_inner()))
            .collect::<Vec<_>>();
        results.sort_by(|left, right| left.1.total_cmp(&right.1));
        results
    }
}

fn not_nan(distance: f32) -> NotNan<f32> {
    NotNan::new(distance).expect("cosine distance should not be NaN")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_neighbor_roundtrip() {
        let index = HnswIndex::build(
            vec![
                ("x".to_string(), vec![1.0, 0.0, 0.0, 0.0]),
                ("y".to_string(), vec![0.0, 1.0, 0.0, 0.0]),
                ("near-x".to_string(), vec![0.9, 0.1, 0.0, 0.0]),
            ],
            HnswOptions {
                m: 4,
                ef_construction: 20,
                ef_search: 20,
                max_layers: 6,
            },
        )
        .unwrap();

        let hits = index.search(&[1.0, 0.0, 0.0, 0.0], 2);
        assert_eq!(hits[0].id, "x");
        assert!(hits[0].distance <= 0.001);
    }

    #[test]
    fn health_metrics_report_layer_and_neighbor_summary() {
        let index = HnswIndex::build(
            vec![
                ("x".to_string(), vec![1.0, 0.0, 0.0, 0.0]),
                ("y".to_string(), vec![0.0, 1.0, 0.0, 0.0]),
                ("z".to_string(), vec![0.0, 0.0, 1.0, 0.0]),
            ],
            HnswOptions {
                m: 4,
                ef_construction: 20,
                ef_search: 20,
                max_layers: 4,
            },
        )
        .unwrap();

        let metrics = index.diagnostics();
        assert_eq!(metrics.total_nodes, 3);
        assert_eq!(metrics.active_nodes, 3);
        assert_eq!(metrics.deleted_nodes, 0);
        assert_eq!(metrics.layer_distribution.iter().sum::<usize>(), 3);
        assert!(metrics.avg_neighbor_degree >= 0.0);
        assert_eq!(metrics.disconnected_nodes, 0);
    }

    #[test]
    fn disconnected_nodes_detects_orphaned_active_vectors() {
        let mut index = HnswIndex::build(
            vec![
                ("x".to_string(), vec![1.0, 0.0, 0.0, 0.0]),
                ("y".to_string(), vec![0.0, 1.0, 0.0, 0.0]),
            ],
            HnswOptions {
                m: 4,
                ef_construction: 20,
                ef_search: 20,
                max_layers: 4,
            },
        )
        .unwrap();

        for level in &mut index.graph {
            for edges in level {
                edges.clear();
            }
        }
        index.entry_point = Some(0);
        index.max_level = 0;
        assert_eq!(index.diagnostics().disconnected_nodes, 1);
    }

    /// gap-2eabd96d regression: near-duplicate clusters much larger than
    /// m0, inserted cluster-consecutively (prod's incremental arrival
    /// order). Distance-truncate reverse-edge pruning orphaned ~28% of
    /// nodes here (zero in-degree, 35% self-recall loss); the diversity
    /// shrink must keep the graph connected and self-recall near-perfect.
    #[test]
    fn large_near_duplicate_clusters_stay_connected_under_push_order() {
        let dims = 32;
        let mut rng = SplitMix64::new(7);
        let mut corpus = Vec::new();
        for c in 0..6 {
            let centroid = gaussian_unit_vector(&mut rng, dims);
            for i in 0..200 {
                let mut vector = centroid
                    .iter()
                    .map(|value| value + gaussian(&mut rng) * 0.05)
                    .collect::<Vec<f32>>();
                normalize(&mut vector);
                corpus.push((format!("c{c}-n{i}"), vector));
            }
        }

        let mut index = HnswIndex::empty(dims, HnswOptions::default()).unwrap();
        for (id, vector) in &corpus {
            index.push(id.clone(), vector.clone()).unwrap();
        }

        let metrics = index.diagnostics();
        let disconnected_ratio = metrics.disconnected_nodes as f64 / metrics.active_nodes as f64;
        assert!(
            disconnected_ratio < 0.01,
            "disconnected {}/{} (zero_in {})",
            metrics.disconnected_nodes,
            metrics.active_nodes,
            metrics.zero_in_degree_nodes,
        );
        let self_recall = index.self_recall_probe(5, 10);
        assert!(self_recall >= 0.98, "self-recall {self_recall}");
    }

    #[test]
    fn duplicate_active_ids_are_refused_at_build() {
        let error = HnswIndex::build(
            vec![
                ("same".to_string(), vec![1.0, 0.0]),
                ("same".to_string(), vec![0.0, 1.0]),
            ],
            HnswOptions::default(),
        )
        .unwrap_err();
        assert!(error.contains("duplicate active HNSW id"));
    }

    #[test]
    fn batch_delete_uses_active_lookup_and_repairs_entry_once() {
        let mut index = HnswIndex::build(
            vec![
                ("a".to_string(), vec![1.0, 0.0]),
                ("b".to_string(), vec![0.0, 1.0]),
                ("c".to_string(), vec![0.7, 0.3]),
            ],
            HnswOptions::default(),
        )
        .unwrap();
        let deleted = index
            .delete_many(&[
                "a".to_string(),
                "missing".to_string(),
                "a".to_string(),
                "c".to_string(),
            ])
            .unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(index.active_count(), 1);
        assert_eq!(index.deleted_count(), 2);
        assert!(!index.delete("a").unwrap());
        assert!(index.delete("b").unwrap());
        assert_eq!(index.entry_point, None);
    }

    #[test]
    fn large_partition_delete_resolves_only_requested_active_ids() {
        const TOTAL: usize = 50_000;
        let mut index = HnswIndex::empty(1, HnswOptions::default()).unwrap();
        index.ids = (0..TOTAL).map(|ordinal| format!("id-{ordinal}")).collect();
        index.vectors.data = vec![1.0; TOTAL];
        index.levels = vec![0; TOTAL];
        index.active = vec![true; TOTAL];
        index.graph = vec![vec![Vec::new(); index.options.max_layers]; TOTAL];
        index.active_ordinal_by_id = index
            .ids
            .iter()
            .enumerate()
            .map(|(ordinal, id)| (id.clone(), ordinal))
            .collect();
        index.entry_point = Some(0);
        index.max_level = 0;

        let deleted = index
            .delete_many(&[
                "id-1".to_string(),
                "id-25000".to_string(),
                "id-49999".to_string(),
                "missing".to_string(),
            ])
            .unwrap();
        assert_eq!(deleted, 3);
        assert_eq!(index.active_ordinal_by_id.len(), TOTAL - 3);
        assert!(!index.active[1]);
        assert!(!index.active[25_000]);
        assert!(!index.active[49_999]);
        assert!(index.active[2]);
    }

    #[test]
    fn deserialized_index_rebuilds_active_lookup_before_mutation() {
        let index = HnswIndex::build(
            vec![
                ("a".to_string(), vec![1.0, 0.0]),
                ("b".to_string(), vec![0.0, 1.0]),
            ],
            HnswOptions::default(),
        )
        .unwrap();
        let bytes = bincode::serialize(&index).unwrap();
        let mut restored: HnswIndex = bincode::deserialize(&bytes).unwrap();
        assert!(!restored.active_index_built);
        assert!(restored.delete("a").unwrap());
        assert_eq!(restored.active_count(), 1);
        assert_eq!(restored.active_ordinal_by_id.get("b"), Some(&1));
    }

    #[test]
    fn donor_recall_parity_1000() {
        let corpus = clustered_vectors(1_000, 32, 20, 42);
        let queries = daystrom_queries(25, 32, 99);
        let index = HnswIndex::build(corpus.clone(), HnswOptions::default()).unwrap();
        let mut hits = 0usize;
        let mut possible = 0usize;
        for query in queries {
            let expected = brute_force(&corpus, &query, 10);
            let actual = index.search(&query, 10);
            hits += actual
                .iter()
                .filter(|hit| expected.iter().any(|id| id == &hit.id))
                .count();
            possible += 10;
        }
        assert!(
            (hits as f32 / possible as f32) >= 0.95,
            "hits={hits} possible={possible}"
        );
    }

    fn clustered_vectors(
        count: usize,
        dims: usize,
        clusters: usize,
        seed: u64,
    ) -> Vec<(String, Vec<f32>)> {
        let mut rng = SplitMix64::new(seed);
        let centroids = (0..clusters)
            .map(|_| gaussian_unit_vector(&mut rng, dims))
            .collect::<Vec<_>>();
        (0..count)
            .map(|idx| {
                let centroid = &centroids[rng.next_usize(clusters)];
                let mut vector = centroid
                    .iter()
                    .map(|value| value + gaussian(&mut rng) * 0.1)
                    .collect::<Vec<_>>();
                normalize(&mut vector);
                (format!("id-{idx}"), vector)
            })
            .collect()
    }

    fn daystrom_queries(count: usize, dims: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut rng = SplitMix64::new(seed);
        (0..count)
            .map(|_| gaussian_unit_vector(&mut rng, dims))
            .collect()
    }

    fn gaussian_unit_vector(rng: &mut SplitMix64, dims: usize) -> Vec<f32> {
        loop {
            let mut vector = (0..dims).map(|_| gaussian(rng)).collect::<Vec<_>>();
            if normalize(&mut vector) {
                return vector;
            }
        }
    }

    fn normalize(vector: &mut [f32]) -> bool {
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm == 0.0 {
            return false;
        }
        for value in vector {
            *value /= norm;
        }
        true
    }

    fn gaussian(rng: &mut SplitMix64) -> f32 {
        let u1 = rng.next_f32().max(f32::MIN_POSITIVE);
        let u2 = rng.next_f32();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
    }

    struct SplitMix64 {
        state: u64,
    }

    impl SplitMix64 {
        fn new(seed: u64) -> Self {
            Self { state: seed }
        }

        fn next_u64(&mut self) -> u64 {
            self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = self.state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }

        fn next_f32(&mut self) -> f32 {
            let value = self.next_u64() >> 40;
            (value as f32) / ((1_u64 << 24) as f32)
        }

        fn next_usize(&mut self, upper: usize) -> usize {
            (self.next_u64() as usize) % upper
        }
    }

    fn brute_force(corpus: &[(String, Vec<f32>)], query: &[f32], k: usize) -> Vec<String> {
        let mut hits = corpus
            .iter()
            .map(|(id, vector)| (id.clone(), cosine_distance(query, vector)))
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| left.1.total_cmp(&right.1));
        hits.into_iter().take(k).map(|(id, _)| id).collect()
    }
}
