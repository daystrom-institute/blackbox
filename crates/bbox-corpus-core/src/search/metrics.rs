//! Ranking-quality metrics for search evaluation.
//!
//! The eval oracle (eval/check.rs) measures coverage — whether an expected
//! entity appears in the collected set at all. That cannot compare one
//! ranking against another, which is what tuning knobs like the combined
//! rerank cap need (gap-39b3ce16). These metrics measure *where* expected
//! entities land.
//!
//! Sweep protocol for the rerank cap: for each candidate cap, run every
//! eval-suite query through `bbox_hybrid_search` with `rerank_cap` set and a
//! deep `limit`, collect the ranked `entity_id` lists, and `aggregate` them
//! against each manifest's `expected_entity_refs`. Compare MRR and recall@k
//! across caps; the metrics are generic over the item type so raw entity-ref
//! strings work directly.

use std::collections::BTreeMap;

/// 1 / rank of the first expected item in `ranked`, or 0.0 when none appear.
pub fn reciprocal_rank<T: PartialEq>(ranked: &[T], expected: &[T]) -> f64 {
    ranked
        .iter()
        .position(|item| expected.contains(item))
        .map(|idx| 1.0 / (idx as f64 + 1.0))
        .unwrap_or(0.0)
}

/// Fraction of `expected` present in the top `k` of `ranked`.
/// Returns 0.0 when `expected` is empty (no signal to measure).
pub fn recall_at_k<T: PartialEq>(ranked: &[T], expected: &[T], k: usize) -> f64 {
    if expected.is_empty() {
        return 0.0;
    }
    let top = &ranked[..k.min(ranked.len())];
    let hits = expected.iter().filter(|item| top.contains(item)).count();
    hits as f64 / expected.len() as f64
}

/// Aggregated ranking quality across a query set.
#[derive(Debug, Clone, PartialEq)]
pub struct RankingReport {
    /// Queries that contributed (non-empty expected set).
    pub queries: usize,
    /// Queries skipped because their expected set was empty.
    pub skipped: usize,
    /// Mean reciprocal rank over contributing queries.
    pub mrr: f64,
    /// Mean recall@k over contributing queries, per requested k.
    pub recall_at: BTreeMap<usize, f64>,
}

/// Aggregate MRR and recall@k over `(ranked, expected)` pairs, one per query.
pub fn aggregate<T: PartialEq>(per_query: &[(Vec<T>, Vec<T>)], ks: &[usize]) -> RankingReport {
    let mut queries = 0usize;
    let mut skipped = 0usize;
    let mut rr_sum = 0.0;
    let mut recall_sums: BTreeMap<usize, f64> = ks.iter().map(|k| (*k, 0.0)).collect();
    for (ranked, expected) in per_query {
        if expected.is_empty() {
            skipped += 1;
            continue;
        }
        queries += 1;
        rr_sum += reciprocal_rank(ranked, expected);
        for k in ks {
            *recall_sums.get_mut(k).expect("k seeded above") += recall_at_k(ranked, expected, *k);
        }
    }
    let denom = queries.max(1) as f64;
    RankingReport {
        queries,
        skipped,
        mrr: rr_sum / denom,
        recall_at: recall_sums
            .into_iter()
            .map(|(k, sum)| (k, sum / denom))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reciprocal_rank_finds_first_expected_hit() {
        let ranked = vec!["a", "b", "c", "d"];
        assert_eq!(reciprocal_rank(&ranked, &["a"]), 1.0);
        assert_eq!(reciprocal_rank(&ranked, &["c", "d"]), 1.0 / 3.0);
        assert_eq!(reciprocal_rank(&ranked, &["z"]), 0.0);
        assert_eq!(reciprocal_rank::<&str>(&[], &["a"]), 0.0);
    }

    #[test]
    fn recall_at_k_counts_expected_in_window() {
        let ranked = vec!["a", "b", "c", "d"];
        assert_eq!(recall_at_k(&ranked, &["a", "c"], 1), 0.5);
        assert_eq!(recall_at_k(&ranked, &["a", "c"], 3), 1.0);
        assert_eq!(recall_at_k(&ranked, &["a", "z"], 4), 0.5);
        assert_eq!(recall_at_k(&ranked, &[], 4), 0.0);
        // k beyond ranked length is not an error.
        assert_eq!(recall_at_k(&ranked, &["d"], 50), 1.0);
    }

    #[test]
    fn aggregate_means_over_contributing_queries_only() {
        let per_query = vec![
            (vec!["a", "b"], vec!["a"]),          // rr 1.0, recall@1 1.0
            (vec!["x", "a"], vec!["a"]),          // rr 0.5, recall@1 0.0
            (vec!["x", "y"], Vec::<&str>::new()), // skipped
        ];
        let report = aggregate(&per_query, &[1, 2]);
        assert_eq!(report.queries, 2);
        assert_eq!(report.skipped, 1);
        assert!((report.mrr - 0.75).abs() < f64::EPSILON);
        assert_eq!(report.recall_at[&1], 0.5);
        assert_eq!(report.recall_at[&2], 1.0);
    }

    #[test]
    fn aggregate_of_empty_input_is_zeroed() {
        let report = aggregate::<&str>(&[], &[5]);
        assert_eq!(report.queries, 0);
        assert_eq!(report.mrr, 0.0);
        assert_eq!(report.recall_at[&5], 0.0);
    }
}
