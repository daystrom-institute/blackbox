//! Parse and shape gate for the retrieval eval manifests in `eval/queries`.
//! The manifests are data for the scripts in `eval/`; compiling them in here
//! makes a manifest that no longer parses fail the test build.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::Error as _;
use serde::{Deserialize, Serialize};

use crate::entity_ref::{EntityRef, EntityType};

pub const MANIFEST_SOURCES: &[(&str, &str)] = &[
    (
        "conceptual-recursion-guard",
        include_str!("queries/conceptual-recursion-guard.json"),
    ),
    (
        "conceptual-entity-ref-stability",
        include_str!("queries/conceptual-entity-ref-stability.json"),
    ),
    (
        "conceptual-embedding-routing",
        include_str!("queries/conceptual-embedding-routing.json"),
    ),
    (
        "conceptual-no-sync-llm",
        include_str!("queries/conceptual-no-sync-llm.json"),
    ),
    (
        "decision-deep-docs-system-memory",
        include_str!("queries/decision-deep-docs-system-memory.json"),
    ),
    (
        "decision-render-pipeline-unidirectional",
        include_str!("queries/decision-render-pipeline-unidirectional.json"),
    ),
    (
        "transcript-nextest-workspace-adoption",
        include_str!("queries/transcript-nextest-workspace-adoption.json"),
    ),
    (
        "transcript-mechanical-recursion-guard",
        include_str!("queries/transcript-mechanical-recursion-guard.json"),
    ),
    (
        "transcript-clippy-disallowed-methods",
        include_str!("queries/transcript-clippy-disallowed-methods.json"),
    ),
    (
        "transcript-worktree-containment-removal",
        include_str!("queries/transcript-worktree-containment-removal.json"),
    ),
    (
        "transcript-codesign-launchd-redeploy",
        include_str!("queries/transcript-codesign-launchd-redeploy.json"),
    ),
    (
        "transcript-harness-in-process-provider",
        include_str!("queries/transcript-harness-in-process-provider.json"),
    ),
    (
        "cross-modal-knowledge-store",
        include_str!("queries/cross-modal-knowledge-store.json"),
    ),
    (
        "cross-modal-recursion-guard",
        include_str!("queries/cross-modal-recursion-guard.json"),
    ),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalQueryManifest {
    pub id: String,
    pub query_class: QueryClass,
    pub query: String,
    pub target_locators: Vec<TargetLocator>,
    #[serde(default)]
    pub expected_entity_refs: Vec<String>,
    #[serde(default)]
    pub pass_strictness: PassStrictness,
    pub required_evidence: RequiredEvidence,
    pub forbidden_stale_answers: Vec<String>,
    pub pass_classifier: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryClass {
    ConceptualDesignDoc,
    StaleDecisionLookup,
    TranscriptProvenance,
    CrossModalCodeProse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PassStrictness {
    #[default]
    Any,
    All,
    First,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetLocator {
    pub description: String,
    pub entity_type_hint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum RequiredEvidence {
    EdgeFamily(String),
    EntitySet(Vec<EntityType>),
    Path {
        from: String,
        to: String,
        edge_types: Vec<String>,
    },
}

pub fn load_manifests() -> Result<Vec<EvalQueryManifest>, serde_json::Error> {
    MANIFEST_SOURCES
        .iter()
        .map(|(name, raw)| load_manifest(name, raw))
        .collect()
}

fn load_manifest(name: &str, raw: &str) -> Result<EvalQueryManifest, serde_json::Error> {
    let manifest = serde_json::from_str::<EvalQueryManifest>(raw)?;
    for locator in &manifest.target_locators {
        if EntityType::from_prefix(&locator.entity_type_hint).is_none() {
            return Err(serde_json::Error::custom(format!(
                "{name}: invalid entity_type_hint `{}` in locator `{}`",
                locator.entity_type_hint,
                truncate_locator_description(&locator.description)
            )));
        }
    }
    Ok(manifest)
}

fn truncate_locator_description(description: &str) -> String {
    const LIMIT: usize = 80;
    let mut chars = description.chars();
    let truncated: String = chars.by_ref().take(LIMIT).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_manifests_parse_and_round_trip() {
        let manifests = load_manifests().expect("all eval manifests parse");
        assert_eq!(manifests.len(), 14);

        let mut ids = BTreeSet::new();
        let mut class_counts = BTreeMap::<QueryClass, usize>::new();
        for ((name, _), manifest) in MANIFEST_SOURCES.iter().zip(&manifests) {
            assert_eq!(&manifest.id, name, "manifest id must match filename stem");
            assert!(
                ids.insert(manifest.id.clone()),
                "duplicate id {}",
                manifest.id
            );
            assert!(
                !manifest.query.trim().is_empty(),
                "empty query in {}",
                manifest.id
            );
            assert!(
                !manifest.target_locators.is_empty(),
                "missing target locators in {}",
                manifest.id
            );
            assert!(
                !manifest.expected_entity_refs.is_empty(),
                "missing expected_entity_refs in {}",
                manifest.id
            );
            for raw in &manifest.expected_entity_refs {
                EntityRef::parse(raw).unwrap_or_else(|err| {
                    panic!("{} invalid expected ref {raw}: {err}", manifest.id)
                });
            }
            *class_counts.entry(manifest.query_class).or_default() += 1;

            let encoded = serde_json::to_string(manifest).unwrap();
            let decoded: EvalQueryManifest = serde_json::from_str(&encoded).unwrap();
            assert_eq!(&decoded, manifest);
        }

        for (class, count) in [
            (QueryClass::ConceptualDesignDoc, 4),
            (QueryClass::StaleDecisionLookup, 2),
            (QueryClass::TranscriptProvenance, 6),
            (QueryClass::CrossModalCodeProse, 2),
        ] {
            assert_eq!(class_counts.get(&class).copied(), Some(count), "{class:?}");
        }
    }

    #[test]
    fn load_manifest_rejects_invalid_entity_type_hint() {
        let mut value: serde_json::Value = serde_json::from_str(MANIFEST_SOURCES[0].1).unwrap();
        value["target_locators"][0]["entity_type_hint"] = "smbol".into();
        let raw = serde_json::to_string(&value).unwrap();

        let err = load_manifest("bogus-hint", &raw).unwrap_err();

        assert!(err.to_string().contains("invalid entity_type_hint"));
        assert!(err.to_string().contains("smbol"));
    }

    #[test]
    fn invalid_hint_error_truncates_long_locator_description() {
        let mut value: serde_json::Value = serde_json::from_str(MANIFEST_SOURCES[0].1).unwrap();
        value["target_locators"][0]["entity_type_hint"] = "smbol".into();
        value["target_locators"][0]["description"] = "x".repeat(120).into();
        let raw = serde_json::to_string(&value).unwrap();

        let err = load_manifest("bogus-hint", &raw).unwrap_err().to_string();

        assert!(err.contains(&"x".repeat(80)));
        assert!(!err.contains(&"x".repeat(120)));
    }
}
