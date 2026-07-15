//! Deterministic lexical skill/lens selection experiment.
//!
//! Selection is metrics-only. It never filters, loads, or renders a catalog.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use serde::Deserialize;
use serde_json::{Value, json};

const QUERY_BYTE_LIMIT: usize = 16 * 1024;
const RESULT_LIMIT: usize = 20;

#[derive(Debug, Clone, Deserialize)]
pub struct ShadowLensCandidate {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub prompt_visible: bool,
    #[serde(default)]
    pub invocation_tools: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct ShadowSelection {
    pub selected_ids: Vec<String>,
    ranks: BTreeMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct ShadowSelectionRun {
    pub selection: ShadowSelection,
    pub metrics: Value,
}

#[derive(Debug, Clone)]
pub struct ShadowLensSelector {
    catalog: Vec<ShadowLensCandidate>,
}

impl ShadowLensSelector {
    pub fn from_json(raw: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(raw)
            .map_err(|error| format!("parse shadow lens catalog: {error}"))?;
        let candidates = if value.is_array() {
            serde_json::from_value(value)
        } else {
            serde_json::from_value(value.get("lenses").cloned().unwrap_or(Value::Null))
        }
        .map_err(|error| format!("decode shadow lens catalog: {error}"))?;
        Ok(Self {
            catalog: candidates,
        })
    }

    pub fn select(&self, query: &str) -> ShadowSelectionRun {
        let started = Instant::now();
        let (query, truncated) = truncate_utf8(query, QUERY_BYTE_LIMIT);
        let query_terms = terms(query);
        let eligible: Vec<&ShadowLensCandidate> = self
            .catalog
            .iter()
            .filter(|candidate| {
                candidate.enabled && candidate.prompt_visible && !candidate.id.trim().is_empty()
            })
            .collect();

        let mut scored: Vec<(u64, &ShadowLensCandidate)> = eligible
            .iter()
            .filter_map(|candidate| {
                let score = lexical_score(candidate, &query_terms);
                (score > 0).then_some((score, *candidate))
            })
            .collect();
        scored.sort_by(|(score_a, candidate_a), (score_b, candidate_b)| {
            score_b
                .cmp(score_a)
                .then_with(|| candidate_a.id.cmp(&candidate_b.id))
        });
        scored.truncate(RESULT_LIMIT);

        let selected_ids: Vec<String> = scored
            .iter()
            .map(|(_, candidate)| candidate.id.clone())
            .collect();
        let ranks = selected_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index + 1))
            .collect();
        let reduction_bps = if eligible.is_empty() {
            0
        } else {
            10_000usize.saturating_sub(selected_ids.len() * 10_000 / eligible.len())
        };
        let status = if eligible.is_empty() {
            "empty_catalog"
        } else if query_terms.is_empty() {
            "empty_query"
        } else {
            "selected"
        };
        ShadowSelectionRun {
            selection: ShadowSelection {
                selected_ids: selected_ids.clone(),
                ranks,
            },
            metrics: json!({
                "status": status,
                "catalog_size": self.catalog.len(),
                "eligible_size": eligible.len(),
                "selected_size": selected_ids.len(),
                "selected_ids": selected_ids,
                "query_terms": query_terms.len(),
                "reduction_bps": reduction_bps,
                "latency_micros": started.elapsed().as_micros().min(u64::MAX as u128) as u64,
                "query_truncated": truncated,
                "shadow_only": true,
            }),
        }
    }

    /// Compare selected ranks with observable catalog-specific invocation
    /// tools. Empty means this catalog exposed no invocation on the step.
    pub fn observe_invocations(
        &self,
        selection: &ShadowSelection,
        tool_names: impl IntoIterator<Item = String>,
    ) -> Vec<Value> {
        let called: BTreeSet<String> = tool_names.into_iter().collect();
        let mut observations = Vec::new();
        for candidate in &self.catalog {
            if candidate
                .invocation_tools
                .iter()
                .any(|tool| called.contains(tool))
            {
                observations.push(json!({
                    "lens_id": candidate.id,
                    "selected": selection.ranks.contains_key(&candidate.id),
                    "rank": selection.ranks.get(&candidate.id),
                    "shadow_only": true,
                }));
            }
        }
        observations
    }
}

fn lexical_score(candidate: &ShadowLensCandidate, query_terms: &BTreeSet<String>) -> u64 {
    let id_terms = terms(&candidate.id);
    let title_terms = terms(&candidate.title);
    let description_terms = terms(&candidate.description);
    query_terms
        .iter()
        .map(|term| {
            u64::from(id_terms.contains(term)) * 8
                + u64::from(title_terms.contains(term)) * 4
                + u64::from(description_terms.contains(term))
        })
        .sum()
}

fn terms(text: &str) -> BTreeSet<String> {
    text.split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| term.len() >= 2)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn truncate_utf8(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut boundary = limit;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (&text[..boundary], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector() -> ShadowLensSelector {
        ShadowLensSelector::from_json(
            r#"[
                {"id":"rust-refactor","title":"Rust refactor","description":"Move and rename Rust items","invocation_tools":["skill_read"]},
                {"id":"docs","title":"Documentation","description":"Write user guides"},
                {"id":"hidden","title":"Rust","prompt_visible":false}
            ]"#,
        )
        .unwrap()
    }

    #[test]
    fn selection_is_deterministic_bounded_and_prompt_visible_only() {
        let selector = selector();
        let first = selector.select("refactor a Rust item");
        let second = selector.select("refactor a Rust item");
        assert_eq!(first.selection.selected_ids, second.selection.selected_ids);
        assert_eq!(first.selection.selected_ids, ["rust-refactor"]);
        assert_eq!(first.metrics["shadow_only"], true);
        assert_eq!(first.metrics["eligible_size"], 2);
    }

    #[test]
    fn observation_reports_false_negative_rank_without_changing_selection() {
        let selector = selector();
        let run = selector.select("write docs");
        let observations = selector.observe_invocations(&run.selection, ["skill_read".into()]);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0]["lens_id"], "rust-refactor");
        assert_eq!(observations[0]["selected"], false);
    }

    #[test]
    fn query_truncation_keeps_utf8_valid() {
        let query = format!("{}é", "x".repeat(QUERY_BYTE_LIMIT));
        let (_bounded, truncated) = truncate_utf8(&query, QUERY_BYTE_LIMIT + 1);
        assert!(truncated);
    }
}
