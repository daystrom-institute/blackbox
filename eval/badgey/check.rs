use serde::{Deserialize, Serialize};

pub const BADGEY_QUERY_SOURCES: &[(&str, &str)] = &[
    (
        "answer/graph-navigation",
        include_str!("queries/answer/graph-navigation.toml"),
    ),
    (
        "answer/proposal-gate",
        include_str!("queries/answer/proposal-gate.toml"),
    ),
    (
        "answer/scout-wrapper",
        include_str!("queries/answer/scout-wrapper.toml"),
    ),
    (
        "teach/inspect-entity",
        include_str!("queries/teach/inspect-entity.toml"),
    ),
    (
        "teach/find-path",
        include_str!("queries/teach/find-path.toml"),
    ),
    (
        "teach/skip-badgey",
        include_str!("queries/teach/skip-badgey.toml"),
    ),
    (
        "scout/compare-paths",
        include_str!("queries/scout/compare-paths.toml"),
    ),
    (
        "scout/proposal-evidence",
        include_str!("queries/scout/proposal-evidence.toml"),
    ),
    ("scout/dispute", include_str!("queries/scout/dispute.toml")),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BadgeyMode {
    Answer,
    Teach,
    Scout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BadgeyEvalCase {
    pub id: String,
    pub mode: BadgeyMode,
    pub query: String,
    #[serde(default)]
    pub required_entity_refs: Vec<String>,
    #[serde(default)]
    pub required_citation_kinds: Vec<String>,
    #[serde(default)]
    pub required_path_edges: Vec<GoldPathEdge>,
    #[serde(default)]
    pub narrative_must_contain: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldPathEdge {
    pub from: String,
    pub edge: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BadgeyBundle {
    #[serde(default)]
    pub entity_refs: Vec<String>,
    #[serde(default)]
    pub citations: Vec<BadgeyCitation>,
    #[serde(default)]
    pub path_edges: Vec<GoldPathEdge>,
    #[serde(default)]
    pub narrative: String,
    #[serde(default)]
    pub teach_steps: Vec<String>,
    #[serde(default)]
    pub scout_results: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BadgeyCitation {
    pub entity_ref: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BadgeyCheckResult {
    pub pass: bool,
    pub stage: String,
    pub failures: Vec<String>,
}

pub fn check_pass(case: &BadgeyEvalCase, bundle: &BadgeyBundle) -> BadgeyCheckResult {
    let failures = missing_required_refs(case, bundle);
    if !failures.is_empty() {
        return fail("required_refs", failures);
    }
    let failures = missing_citation_kinds(case, bundle);
    if !failures.is_empty() {
        return fail("citation_kinds", failures);
    }
    let failures = missing_path_edges(case, bundle);
    if !failures.is_empty() {
        return fail("path_coverage", failures);
    }
    let failures = missing_narrative_terms(case, bundle);
    if !failures.is_empty() {
        return fail("narrative", failures);
    }
    let failures = mode_specific_failures(case, bundle);
    if !failures.is_empty() {
        return fail("mode_specific", failures);
    }
    BadgeyCheckResult {
        pass: true,
        stage: "pass".to_string(),
        failures: Vec::new(),
    }
}

fn fail(stage: &str, failures: Vec<String>) -> BadgeyCheckResult {
    BadgeyCheckResult {
        pass: false,
        stage: stage.to_string(),
        failures,
    }
}

fn missing_required_refs(case: &BadgeyEvalCase, bundle: &BadgeyBundle) -> Vec<String> {
    case.required_entity_refs
        .iter()
        .filter(|required| !bundle.entity_refs.contains(required))
        .map(|required| format!("missing_entity_ref:{required}"))
        .collect()
}

fn missing_citation_kinds(case: &BadgeyEvalCase, bundle: &BadgeyBundle) -> Vec<String> {
    case.required_citation_kinds
        .iter()
        .filter(|required| !bundle.citations.iter().any(|citation| &citation.kind == *required))
        .map(|required| format!("missing_citation_kind:{required}"))
        .collect()
}

fn missing_path_edges(case: &BadgeyEvalCase, bundle: &BadgeyBundle) -> Vec<String> {
    case.required_path_edges
        .iter()
        .filter(|required| !bundle.path_edges.contains(required))
        .map(|required| format!("missing_path_edge:{}:{}:{}", required.from, required.edge, required.to))
        .collect()
}

fn missing_narrative_terms(case: &BadgeyEvalCase, bundle: &BadgeyBundle) -> Vec<String> {
    let narrative = bundle.narrative.to_ascii_lowercase();
    case.narrative_must_contain
        .iter()
        .filter(|term| !narrative.contains(&term.to_ascii_lowercase()))
        .map(|term| format!("missing_narrative_term:{term}"))
        .collect()
}

fn mode_specific_failures(case: &BadgeyEvalCase, bundle: &BadgeyBundle) -> Vec<String> {
    match case.mode {
        BadgeyMode::Answer => Vec::new(),
        BadgeyMode::Teach if bundle.teach_steps.len() >= 2 => Vec::new(),
        BadgeyMode::Teach => vec!["teach_steps_missing".to_string()],
        BadgeyMode::Scout if !bundle.scout_results.is_empty() => Vec::new(),
        BadgeyMode::Scout => vec!["scout_results_missing".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(mode: BadgeyMode) -> BadgeyEvalCase {
        BadgeyEvalCase {
            id: "badgey-answer-001".to_string(),
            mode,
            query: "How does Badgey cite graph evidence?".to_string(),
            required_entity_refs: vec!["knowledge:abc12345".to_string()],
            required_citation_kinds: vec!["knowledge".to_string()],
            required_path_edges: vec![GoldPathEdge {
                from: "knowledge:abc12345".to_string(),
                edge: "DERIVED_FROM".to_string(),
                to: "thread:def67890".to_string(),
            }],
            narrative_must_contain: vec!["evidence".to_string()],
        }
    }

    fn good_bundle() -> BadgeyBundle {
        BadgeyBundle {
            entity_refs: vec!["knowledge:abc12345".to_string()],
            citations: vec![BadgeyCitation {
                entity_ref: "knowledge:abc12345".to_string(),
                kind: "knowledge".to_string(),
            }],
            path_edges: vec![GoldPathEdge {
                from: "knowledge:abc12345".to_string(),
                edge: "DERIVED_FROM".to_string(),
                to: "thread:def67890".to_string(),
            }],
            narrative: "The evidence path is materialized.".to_string(),
            teach_steps: vec!["search".to_string(), "inspect".to_string()],
            scout_results: vec!["scout-a".to_string()],
        }
    }

    #[test]
    fn structural_checks_short_circuit_before_narrative() {
        let mut bundle = good_bundle();
        bundle.entity_refs.clear();
        bundle.narrative.clear();
        let result = check_pass(&case(BadgeyMode::Answer), &bundle);
        assert!(!result.pass);
        assert_eq!(result.stage, "required_refs");
        assert_eq!(result.failures, vec!["missing_entity_ref:knowledge:abc12345"]);
    }

    #[test]
    fn good_answer_bundle_passes() {
        let result = check_pass(&case(BadgeyMode::Answer), &good_bundle());
        assert!(result.pass);
    }

    #[test]
    fn teach_mode_requires_steps() {
        let mut bundle = good_bundle();
        bundle.teach_steps.clear();
        let result = check_pass(&case(BadgeyMode::Teach), &bundle);
        assert_eq!(result.stage, "mode_specific");
        assert_eq!(result.failures, vec!["teach_steps_missing"]);
    }

    #[test]
    fn shipped_query_manifests_parse() {
        for (name, raw) in BADGEY_QUERY_SOURCES {
            let parsed: BadgeyEvalCase =
                toml::from_str(raw).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert!(!parsed.id.is_empty(), "{name}");
            assert!(!parsed.query.is_empty(), "{name}");
            assert!(!parsed.required_entity_refs.is_empty(), "{name}");
        }
    }
}
