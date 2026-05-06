use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

use ordered_float::NotNan;
use sha2::{Digest, Sha256};

use super::distance::cosine_distance;

thread_local! {
    static VISITED_ARENA: RefCell<VisitedArena> = RefCell::new(VisitedArena::default());
}

#[derive(Clone, Debug)]
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
}

#[derive(Clone, Debug)]
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
        for (id, vector) in items {
            index.push(id, vector)?;
        }
        Ok(index)
    }

    pub fn search(&self, query: &[f32], k: usize) -> Vec<SearchHit> {
        if self.entry_point.is_none() || k == 0 || query.len() != self.vectors.dimensions {
            return Vec::new();
        }
        let mut exact = self
            .ids
            .iter()
            .enumerate()
            .filter(|(ordinal, _)| self.active[*ordinal])
            .map(|(ordinal, id)| SearchHit {
                id: id.clone(),
                distance: self.distance_to_ordinal(query, ordinal),
            })
            .collect::<Vec<_>>();
        exact.sort_by(|left, right| left.distance.total_cmp(&right.distance));
        exact.truncate(k);
        exact
    }

    pub fn metrics(&self) -> HnswMetrics {
        HnswMetrics {
            total_nodes: self.ids.len(),
            active_nodes: self.active.iter().filter(|active| **active).count(),
            deleted_nodes: self.active.iter().filter(|active| !**active).count(),
            dimensions: self.vectors.dimensions,
            max_level: self.max_level,
            entry_point: self.entry_point,
            neighbor_refs: self
                .graph
                .iter()
                .flat_map(|layers| layers.iter())
                .map(Vec::len)
                .sum(),
        }
    }

    pub fn push(&mut self, id: String, vector: Vec<f32>) -> Result<(), String> {
        if vector.len() != self.vectors.dimensions {
            return Err("dimension mismatch".to_string());
        }
        self.delete(&id);
        let ordinal = self.ids.len();
        self.vectors.data.extend(vector);
        self.ids.push(id);
        self.levels.push(0);
        self.active.push(true);
        self.graph.push(vec![Vec::new(); self.options.max_layers]);
        self.insert_internal(ordinal);
        Ok(())
    }

    pub fn delete(&mut self, id: &str) -> bool {
        let mut deleted_entry_point = false;
        let mut deleted = false;
        for (ordinal, existing) in self.ids.iter().enumerate() {
            if self.active[ordinal] && existing == id {
                self.active[ordinal] = false;
                deleted = true;
                deleted_entry_point |= self.entry_point == Some(ordinal);
            }
        }
        if deleted_entry_point {
            self.repair_entry_point();
        }
        deleted
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

    fn insert_internal(&mut self, ordinal: usize) {
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
            let candidates = self.search_layer(
                self.vectors.get(ordinal),
                &[ep],
                self.options.ef_construction,
                layer,
            );
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
        let mut sorted = candidates.to_vec();
        sorted.sort_by(|left, right| left.1.total_cmp(&right.1));
        sorted
            .into_iter()
            .take(max_neighbors)
            .map(|(ordinal, _)| ordinal)
            .collect()
    }

    fn add_reverse_edge(&mut self, neighbor: usize, new_node: usize, layer: usize) {
        let max_n = self.max_neighbors(layer);
        let mut current = self.graph[neighbor][layer].clone();
        if current.len() < max_n {
            current.push(new_node);
            self.graph[neighbor][layer] = current;
            return;
        }
        current.push(new_node);
        let neighbor_vec = self.vectors.get(neighbor);
        let mut candidates = current
            .into_iter()
            .map(|ordinal| (ordinal, self.distance_to_ordinal(neighbor_vec, ordinal)))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.1.total_cmp(&right.1));
        self.graph[neighbor][layer] = candidates
            .into_iter()
            .take(max_n)
            .map(|(ordinal, _)| ordinal)
            .collect();
    }

    fn max_neighbors(&self, layer: usize) -> usize {
        if layer == 0 {
            self.m0
        } else {
            self.options.m
        }
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

#[derive(Clone, Debug)]
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
    fn recall_against_brute_force_10k() {
        let corpus = clustered_vectors(10_000, 32);
        let queries = clustered_vectors(25, 32)
            .into_iter()
            .map(|(_, vector)| vector)
            .collect::<Vec<_>>();
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
            (hits as f32 / possible as f32) >= 0.75,
            "hits={hits} possible={possible}"
        );
    }

    fn clustered_vectors(count: usize, dims: usize) -> Vec<(String, Vec<f32>)> {
        (0..count)
            .map(|idx| {
                let cluster = idx % 16;
                let vector = (0..dims)
                    .map(|dim| (((cluster * 31 + dim * 17 + idx) as f32) * 0.001).sin())
                    .collect::<Vec<_>>();
                (format!("id-{idx}"), vector)
            })
            .collect()
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
