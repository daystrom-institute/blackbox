use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, PartialEq)]
pub struct RankedHit {
    pub entity_id: String,
    pub rank: usize,
    pub score: f32,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedList {
    pub source: String,
    pub weight: f32,
    pub hits: Vec<RankedHit>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FusedHit {
    pub entity_id: String,
    pub score: f32,
    pub sources: BTreeMap<String, f32>,
}

pub fn fuse_rrf(lists: &[RankedList], k: f32, limit: usize) -> Vec<FusedHit> {
    let mut scores: HashMap<String, FusedHit> = HashMap::new();
    for list in lists {
        for (idx, hit) in list.hits.iter().enumerate() {
            let rank = if hit.rank == 0 { idx + 1 } else { hit.rank };
            let contribution = list.weight / (k + rank as f32);
            let fused = scores
                .entry(hit.entity_id.clone())
                .or_insert_with(|| FusedHit {
                    entity_id: hit.entity_id.clone(),
                    score: 0.0,
                    sources: BTreeMap::new(),
                });
            fused.score += contribution;
            *fused.sources.entry(list.source.clone()).or_insert(0.0) += contribution;
        }
    }

    let mut fused = scores.into_values().collect::<Vec<_>>();
    fused.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.entity_id.cmp(&b.entity_id))
    });
    fused.truncate(limit);
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(entity_id: &str, rank: usize) -> RankedHit {
        RankedHit {
            entity_id: entity_id.into(),
            rank,
            score: 1.0 / rank as f32,
            source: "fixture".into(),
        }
    }

    #[test]
    fn fuses_bm25_and_vector_by_entity_id() {
        let fused = fuse_rrf(
            &[
                RankedList {
                    source: "bm25".into(),
                    weight: 0.4,
                    hits: vec![hit("knowledge:a", 1), hit("project_file:b", 2)],
                },
                RankedList {
                    source: "vector:voyage".into(),
                    weight: 0.6,
                    hits: vec![hit("knowledge:a", 1), hit("commit:c", 2)],
                },
            ],
            60.0,
            10,
        );

        assert_eq!(fused[0].entity_id, "knowledge:a");
        assert_eq!(
            fused
                .iter()
                .filter(|hit| hit.entity_id == "knowledge:a")
                .count(),
            1
        );
        assert!(fused[0].sources.contains_key("bm25"));
        assert!(fused[0].sources.contains_key("vector:voyage"));
    }
}
