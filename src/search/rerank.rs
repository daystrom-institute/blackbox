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
    let recall_boost = (1.0 + features.recall_count as f32).ln_1p().min(0.25);
    let recency_boost = parse_time(features.last_recalled.as_deref())
        .map(|last| {
            let days = (now - last).num_seconds().max(0) as f32 / 86_400.0;
            if days <= 30.0 { 0.10 } else { 0.0 }
        })
        .unwrap_or(0.0);
    (base + recall_boost + recency_boost).clamp(0.50, 1.25)
}

pub fn apply_rerank(base_score: f32, features: &RerankFeatures, now: DateTime<Utc>) -> f32 {
    base_score * type_multiplier(features) * temporal_decay(features, now)
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
}
