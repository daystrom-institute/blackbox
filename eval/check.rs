use std::collections::{BTreeMap, BTreeSet};

use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::entity_ref::{EntityRef, EntityType};

pub const MANIFEST_SOURCES: &[(&str, &str)] = &[
    (
        "exact-symbol-knowledge-store",
        include_str!("queries/exact-symbol-knowledge-store.json"),
    ),
    (
        "exact-symbol-routing-verdict",
        include_str!("queries/exact-symbol-routing-verdict.json"),
    ),
    (
        "exact-symbol-wait-store",
        include_str!("queries/exact-symbol-wait-store.json"),
    ),
    (
        "exact-symbol-mcp-store",
        include_str!("queries/exact-symbol-mcp-store.json"),
    ),
    (
        "exact-symbol-cron-registry",
        include_str!("queries/exact-symbol-cron-registry.json"),
    ),
    (
        "exact-symbol-entity-ref",
        include_str!("queries/exact-symbol-entity-ref.json"),
    ),
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
        "conceptual-edge-index-authored",
        include_str!("queries/conceptual-edge-index-authored.json"),
    ),
    (
        "conceptual-no-sync-llm",
        include_str!("queries/conceptual-no-sync-llm.json"),
    ),
    (
        "conceptual-workflow-foreach",
        include_str!("queries/conceptual-workflow-foreach.json"),
    ),
    (
        "decision-rule-packet-primitive",
        include_str!("queries/decision-rule-packet-primitive.json"),
    ),
    (
        "decision-deep-docs-system-memory",
        include_str!("queries/decision-deep-docs-system-memory.json"),
    ),
    (
        "decision-distinct-daemon-paths",
        include_str!("queries/decision-distinct-daemon-paths.json"),
    ),
    (
        "decision-bro-account-env",
        include_str!("queries/decision-bro-account-env.json"),
    ),
    (
        "decision-render-pipeline-unidirectional",
        include_str!("queries/decision-render-pipeline-unidirectional.json"),
    ),
    (
        "decision-rule-packet-validation",
        include_str!("queries/decision-rule-packet-validation.json"),
    ),
    (
        "transcript-voyage-embeddings",
        include_str!("queries/transcript-voyage-embeddings.json"),
    ),
    (
        "transcript-mechanical-recursion-guard",
        include_str!("queries/transcript-mechanical-recursion-guard.json"),
    ),
    (
        "transcript-postgres-snake-case",
        include_str!("queries/transcript-postgres-snake-case.json"),
    ),
    (
        "transcript-rule-packet-validation",
        include_str!("queries/transcript-rule-packet-validation.json"),
    ),
    (
        "transcript-workflow-foreach",
        include_str!("queries/transcript-workflow-foreach.json"),
    ),
    (
        "transcript-entity-ref-phase-f1",
        include_str!("queries/transcript-entity-ref-phase-f1.json"),
    ),
    (
        "cross-modal-knowledge-store",
        include_str!("queries/cross-modal-knowledge-store.json"),
    ),
    (
        "cross-modal-recursion-guard",
        include_str!("queries/cross-modal-recursion-guard.json"),
    ),
    (
        "cross-modal-workflow-engine",
        include_str!("queries/cross-modal-workflow-engine.json"),
    ),
    (
        "cross-modal-rule-packets",
        include_str!("queries/cross-modal-rule-packets.json"),
    ),
    (
        "cross-modal-entity-ref-parser",
        include_str!("queries/cross-modal-entity-ref-parser.json"),
    ),
    (
        "cross-modal-notes-side-channel",
        include_str!("queries/cross-modal-notes-side-channel.json"),
    ),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalQueryManifest {
    pub id: String,
    pub query_class: QueryClass,
    pub query: String,
    pub target_locators: Vec<TargetLocator>,
    pub required_evidence: RequiredEvidence,
    pub forbidden_stale_answers: Vec<String>,
    pub pass_classifier: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryClass {
    ExactSymbol,
    ConceptualDesignDoc,
    StaleDecisionLookup,
    TranscriptProvenance,
    CrossModalCodeProse,
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
pub struct RequiredEvidence {
    pub kind: RequiredEvidenceKind,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredEvidenceKind {
    EdgeFamily,
    Path,
    EntitySet,
}

pub type CheckPassFn = fn(&[EntityRef]) -> (bool, Vec<String>);

pub fn check_pass(collected: &[EntityRef]) -> (bool, Vec<String>) {
    default_stub_check("check_pass", collected)
}

pub fn checker_by_name(name: &str) -> Option<CheckPassFn> {
    Some(match name {
        "check_exact_symbol_knowledge_store" => check_exact_symbol_knowledge_store,
        "check_exact_symbol_routing_verdict" => check_exact_symbol_routing_verdict,
        "check_exact_symbol_wait_store" => check_exact_symbol_wait_store,
        "check_exact_symbol_mcp_store" => check_exact_symbol_mcp_store,
        "check_exact_symbol_cron_registry" => check_exact_symbol_cron_registry,
        "check_exact_symbol_entity_ref" => check_exact_symbol_entity_ref,
        "check_conceptual_recursion_guard" => check_conceptual_recursion_guard,
        "check_conceptual_entity_ref_stability" => check_conceptual_entity_ref_stability,
        "check_conceptual_embedding_routing" => check_conceptual_embedding_routing,
        "check_conceptual_edge_index_authored" => check_conceptual_edge_index_authored,
        "check_conceptual_no_sync_llm" => check_conceptual_no_sync_llm,
        "check_conceptual_workflow_foreach" => check_conceptual_workflow_foreach,
        "check_decision_rule_packet_primitive" => check_decision_rule_packet_primitive,
        "check_decision_deep_docs_system_memory" => check_decision_deep_docs_system_memory,
        "check_decision_distinct_daemon_paths" => check_decision_distinct_daemon_paths,
        "check_decision_bro_account_env" => check_decision_bro_account_env,
        "check_decision_render_pipeline_unidirectional" => {
            check_decision_render_pipeline_unidirectional
        }
        "check_decision_rule_packet_validation" => check_decision_rule_packet_validation,
        "check_transcript_voyage_embeddings" => check_transcript_voyage_embeddings,
        "check_transcript_mechanical_recursion_guard" => {
            check_transcript_mechanical_recursion_guard
        }
        "check_transcript_postgres_snake_case" => check_transcript_postgres_snake_case,
        "check_transcript_rule_packet_validation" => check_transcript_rule_packet_validation,
        "check_transcript_workflow_foreach" => check_transcript_workflow_foreach,
        "check_transcript_entity_ref_phase_f1" => check_transcript_entity_ref_phase_f1,
        "check_cross_modal_knowledge_store" => check_cross_modal_knowledge_store,
        "check_cross_modal_recursion_guard" => check_cross_modal_recursion_guard,
        "check_cross_modal_workflow_engine" => check_cross_modal_workflow_engine,
        "check_cross_modal_rule_packets" => check_cross_modal_rule_packets,
        "check_cross_modal_entity_ref_parser" => check_cross_modal_entity_ref_parser,
        "check_cross_modal_notes_side_channel" => check_cross_modal_notes_side_channel,
        _ => return None,
    })
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
                locator.entity_type_hint, locator.description
            )));
        }
    }
    Ok(manifest)
}

fn default_stub_check(name: &str, collected: &[EntityRef]) -> (bool, Vec<String>) {
    (
        false,
        vec![format!(
            "{name} is an F2a stub; F2b will resolve expected entity refs and implement assertions (collected={})",
            collected.len()
        )],
    )
}

macro_rules! stub_checker {
    ($name:ident) => {
        pub fn $name(collected: &[EntityRef]) -> (bool, Vec<String>) {
            default_stub_check(stringify!($name), collected)
        }
    };
}

stub_checker!(check_exact_symbol_knowledge_store);
stub_checker!(check_exact_symbol_routing_verdict);
stub_checker!(check_exact_symbol_wait_store);
stub_checker!(check_exact_symbol_mcp_store);
stub_checker!(check_exact_symbol_cron_registry);
stub_checker!(check_exact_symbol_entity_ref);
stub_checker!(check_conceptual_recursion_guard);
stub_checker!(check_conceptual_entity_ref_stability);
stub_checker!(check_conceptual_embedding_routing);
stub_checker!(check_conceptual_edge_index_authored);
stub_checker!(check_conceptual_no_sync_llm);
stub_checker!(check_conceptual_workflow_foreach);
stub_checker!(check_decision_rule_packet_primitive);
stub_checker!(check_decision_deep_docs_system_memory);
stub_checker!(check_decision_distinct_daemon_paths);
stub_checker!(check_decision_bro_account_env);
stub_checker!(check_decision_render_pipeline_unidirectional);
stub_checker!(check_decision_rule_packet_validation);
stub_checker!(check_transcript_voyage_embeddings);
stub_checker!(check_transcript_mechanical_recursion_guard);
stub_checker!(check_transcript_postgres_snake_case);
stub_checker!(check_transcript_rule_packet_validation);
stub_checker!(check_transcript_workflow_foreach);
stub_checker!(check_transcript_entity_ref_phase_f1);
stub_checker!(check_cross_modal_knowledge_store);
stub_checker!(check_cross_modal_recursion_guard);
stub_checker!(check_cross_modal_workflow_engine);
stub_checker!(check_cross_modal_rule_packets);
stub_checker!(check_cross_modal_entity_ref_parser);
stub_checker!(check_cross_modal_notes_side_channel);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_30_manifests_parse_and_round_trip() {
        let manifests = load_manifests().expect("all eval manifests parse");
        assert_eq!(manifests.len(), 30);

        let mut ids = BTreeSet::new();
        let mut class_counts = BTreeMap::<QueryClass, usize>::new();
        for manifest in &manifests {
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
                checker_by_name(&manifest.pass_classifier).is_some(),
                "unknown checker {} in {}",
                manifest.pass_classifier,
                manifest.id
            );
            *class_counts.entry(manifest.query_class).or_default() += 1;

            let encoded = serde_json::to_string(manifest).unwrap();
            let decoded: EvalQueryManifest = serde_json::from_str(&encoded).unwrap();
            assert_eq!(&decoded, manifest);
        }

        for class in [
            QueryClass::ExactSymbol,
            QueryClass::ConceptualDesignDoc,
            QueryClass::StaleDecisionLookup,
            QueryClass::TranscriptProvenance,
            QueryClass::CrossModalCodeProse,
        ] {
            assert_eq!(class_counts.get(&class).copied(), Some(6), "{class:?}");
        }
    }

    #[test]
    fn default_check_pass_signature_compiles() {
        let (passed, messages) = check_pass(&[]);
        assert!(!passed);
        assert!(!messages.is_empty());
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
}
