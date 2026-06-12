use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RerankFeatures {
    pub doc_type: Option<String>,
    pub chunk_kind: Option<String>,
    pub role: Option<String>,
    pub approval: Option<String>,
    pub created_at: Option<String>,
    pub last_recalled: Option<String>,
    pub recall_count: u32,
}

pub fn type_multiplier(features: &RerankFeatures) -> f32 {
    match features.doc_type.as_deref() {
        Some("knowledge") => match features.approval.as_deref() {
            Some("UserConfirmed") | Some("user_confirmed") => 1.35,
            Some("Imported") | Some("imported") => 0.85,
            _ => 1.0,
        },
        Some("project_file") => match features.chunk_kind.as_deref() {
            Some("doc_section") => 1.20,
            Some("code_block") => 1.0,
            _ => 1.0,
        },
        Some("commit") => 1.05,
        Some("transcript") => match features.role.as_deref() {
            Some("user") => 1.10,
            Some("assistant") => 0.95,
            _ => 1.0,
        },
        _ => 1.0,
    }
}

pub fn temporal_decay(features: &RerankFeatures, now: DateTime<Utc>) -> f32 {
    if features.doc_type.as_deref() != Some("knowledge") {
        return 1.0;
    }
    let Some(created_at) = parse_time(features.created_at.as_deref()) else {
        return 1.0;
    };
    let age_days = (now - created_at).num_seconds().max(0) as f32 / 86_400.0;
    let base = 1.0 / (1.0 + age_days / 365.0);
    // Graded by recall frequency: zero at recall_count=0, ~0.06 at 1, ~0.19 at
    // 10, saturating at the 0.25 cap near 30 recalls. The previous form
    // ln_1p(1 + count) was >= ln(2) at count=0, so the cap saturated for every
    // entry and the recall signal was dead.
    let recall_boost = ((features.recall_count as f32).ln_1p() * 0.08).min(0.25);
    let recency_boost = parse_time(features.last_recalled.as_deref())
        .map(|last| {
            let days = (now - last).num_seconds().max(0) as f32 / 86_400.0;
            if days <= 30.0 { 0.10 } else { 0.0 }
        })
        .unwrap_or(0.0);
    (base + recall_boost + recency_boost).clamp(0.50, 1.25)
}

/// Default ceiling on the combined type x temporal multiplier. Tuned
/// empirically (gap-39b3ce16, 2026-06-12): sweeping caps
/// {1.0, 1.25, 1.5, 1.75, 2.0, 2.5} over the 30-query eval suite against
/// the live corpus, MRR rose monotonically to 1.75 (0.066 → 0.175,
/// recall@1 0 → 0.13) and plateaued exactly from there — today's maximum
/// legitimate boost product is UserConfirmed 1.35 × temporal 1.25 =
/// 1.6875, so 1.5 truncated real knowledge promotions while any cap
/// ≥ 1.6875 never binds. 1.75 is the smallest plateau value: it frees the
/// current signals and still backstops future boost stacking.
pub const DEFAULT_COMBINED_CAP: f32 = 1.75;

pub fn apply_rerank(base_score: f32, features: &RerankFeatures, now: DateTime<Utc>) -> f32 {
    apply_rerank_with_cap(base_score, features, now, DEFAULT_COMBINED_CAP)
}

/// `apply_rerank` with an explicit combined-multiplier cap, for eval sweeps.
pub fn apply_rerank_with_cap(
    base_score: f32,
    features: &RerankFeatures,
    now: DateTime<Utc>,
    cap: f32,
) -> f32 {
    let uncapped = base_score * type_multiplier(features) * temporal_decay(features, now);
    // Keep independent type and temporal boosts from compounding into a
    // runaway promotion; one result can gain at most (cap - 1) over its
    // base RRF rank.
    uncapped.min(base_score * cap)
}

fn parse_time(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let raw = raw?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|ts| ts.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_multiplier_prefers_confirmed_knowledge_over_code() {
        let knowledge = RerankFeatures {
            doc_type: Some("knowledge".into()),
            approval: Some("UserConfirmed".into()),
            ..RerankFeatures::default()
        };
        let code = RerankFeatures {
            doc_type: Some("project_file".into()),
            chunk_kind: Some("code_block".into()),
            ..RerankFeatures::default()
        };
        assert!(type_multiplier(&knowledge) > type_multiplier(&code));
    }

    #[test]
    fn temporal_decay_only_applies_to_knowledge() {
        let now = DateTime::parse_from_rfc3339("2026-05-05T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let stale = RerankFeatures {
            doc_type: Some("knowledge".into()),
            created_at: Some("2020-01-01T00:00:00Z".into()),
            ..RerankFeatures::default()
        };
        let code = RerankFeatures {
            doc_type: Some("project_file".into()),
            created_at: Some("2020-01-01T00:00:00Z".into()),
            ..RerankFeatures::default()
        };
        assert!(temporal_decay(&stale, now) < 1.0);
        assert_eq!(temporal_decay(&code, now), 1.0);
    }

    #[test]
    fn recall_boost_is_zero_when_never_recalled_and_grades_with_count() {
        let now = DateTime::parse_from_rfc3339("2026-05-05T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let knowledge = |recall_count: u32| RerankFeatures {
            doc_type: Some("knowledge".into()),
            created_at: Some("2026-05-05T00:00:00Z".into()),
            recall_count,
            ..RerankFeatures::default()
        };
        let cold = temporal_decay(&knowledge(0), now);
        let warm = temporal_decay(&knowledge(5), now);
        let hot = temporal_decay(&knowledge(100), now);
        // Fresh entry never recalled gets no boost at all.
        assert_eq!(cold, 1.0);
        assert!(warm > cold);
        assert!(hot > warm);
        // Recall boost saturates at the 0.25 cap, then the overall clamp holds.
        assert_eq!(hot, 1.25);
    }

    #[test]
    fn default_cap_passes_max_legitimate_boost_and_explicit_cap_truncates() {
        let now = DateTime::parse_from_rfc3339("2026-05-05T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let fresh_confirmed = RerankFeatures {
            doc_type: Some("knowledge".into()),
            approval: Some("UserConfirmed".into()),
            created_at: Some("2026-05-05T00:00:00Z".into()),
            last_recalled: Some("2026-05-05T00:00:00Z".into()),
            recall_count: 10,
            ..RerankFeatures::default()
        };

        // Max legitimate product today: type 1.35 x temporal clamp 1.25 =
        // 1.6875 — below the tuned 1.75 default, so the cap does not bind
        // (the gap-39b3ce16 sweep showed a binding cap costs MRR).
        let scored = apply_rerank(0.2, &fresh_confirmed, now);
        assert!((scored - 0.2 * 1.6875).abs() < 1e-6, "{scored}");

        // An explicit lower cap still truncates stacked boosts.
        let capped = apply_rerank_with_cap(0.2, &fresh_confirmed, now, 1.5);
        assert!((capped - 0.3).abs() < 1e-6, "{capped}");
    }
}
