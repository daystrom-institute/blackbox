use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use serde::de::Error as _;
use serde::{Deserialize, Serialize};

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

static CHECKER_MANIFESTS: OnceLock<Result<Vec<EvalQueryManifest>, String>> = OnceLock::new();
#[cfg(test)]
static MANIFEST_PARSE_COUNT: AtomicUsize = AtomicUsize::new(0);

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
    ExactSymbol,
    ConceptualDesignDoc,
    StaleDecisionLookup,
    TranscriptProvenance,
    CrossModalCodeProse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassStrictness {
    Any,
    All,
    First,
}

impl Default for PassStrictness {
    fn default() -> Self {
        Self::Any
    }
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
    #[cfg(test)]
    MANIFEST_PARSE_COUNT.fetch_add(1, Ordering::SeqCst);
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

fn default_stub_check(name: &str, collected: &[EntityRef]) -> (bool, Vec<String>) {
    expected_ref_check(name, collected)
}

fn expected_ref_check(name: &str, collected: &[EntityRef]) -> (bool, Vec<String>) {
    let (expected, strictness) = match expected_refs_for_checker(name) {
        Ok(expected) => expected,
        Err(err) => return (false, vec![err]),
    };
    if expected.is_empty() {
        return (
            false,
            vec![format!("{name} has no expected_entity_refs materialized")],
        );
    }
    let matched = match strictness {
        PassStrictness::Any => expected.iter().any(|expected| collected.contains(expected)),
        PassStrictness::All => expected.iter().all(|expected| collected.contains(expected)),
        PassStrictness::First => expected
            .first()
            .is_some_and(|expected| collected.contains(expected)),
    };
    if matched {
        (
            true,
            vec![format!(
                "{name} matched expected entity refs with {strictness:?} strictness"
            )],
        )
    } else {
        (
            false,
            vec![format!(
                "{name} expected {strictness:?} match for [{}], collected [{}]",
                expected
                    .iter()
                    .map(EntityRef::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                collected
                    .iter()
                    .map(EntityRef::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )],
        )
    }
}

fn expected_refs_for_checker(name: &str) -> Result<(Vec<EntityRef>, PassStrictness), String> {
    let manifests = cached_manifests()?;
    let Some(manifest) = manifests
        .iter()
        .find(|manifest| manifest.pass_classifier == name)
    else {
        return Err(format!("unknown checker {name}"));
    };
    let expected = manifest
        .expected_entity_refs
        .iter()
        .map(|raw| EntityRef::parse(raw).map_err(|err| err.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((expected, manifest.pass_strictness))
}

fn cached_manifests() -> Result<&'static [EvalQueryManifest], String> {
    CHECKER_MANIFESTS
        .get_or_init(|| load_manifests().map_err(|err| err.to_string()))
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

macro_rules! stub_checker {
    ($name:ident) => {
        pub fn $name(collected: &[EntityRef]) -> (bool, Vec<String>) {
            // H3 replaces this shared oracle with per-class harness logic.
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
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn all_30_manifests_parse_and_round_trip() {
        let manifests = load_manifests().expect("all eval manifests parse");
        assert_eq!(manifests.len(), 30);

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
    fn all_30_manifests_have_resolvable_expected_refs() {
        let manifests = load_manifests().expect("all eval manifests parse");
        for manifest in &manifests {
            for raw in &manifest.expected_entity_refs {
                let entity_ref = EntityRef::parse(raw).unwrap_or_else(|err| {
                    panic!("{} invalid expected ref {raw}: {err}", manifest.id)
                });
                assert!(
                    expected_ref_resolves(manifest, &entity_ref),
                    "{} expected ref does not resolve through its locators: {raw}",
                    manifest.id
                );
            }
        }
    }

    #[test]
    fn checkers_pass_when_expected_refs_are_collected() {
        let manifests = load_manifests().expect("all eval manifests parse");
        for manifest in &manifests {
            let checker = checker_by_name(&manifest.pass_classifier)
                .unwrap_or_else(|| panic!("missing checker {}", manifest.pass_classifier));
            let collected = manifest
                .expected_entity_refs
                .iter()
                .map(|raw| {
                    EntityRef::parse(raw).unwrap_or_else(|err| {
                        panic!("{} invalid expected ref {raw}: {err}", manifest.id)
                    })
                })
                .collect::<Vec<_>>();

            let (passed, messages) = checker(&collected);

            assert!(
                passed,
                "{} checker should pass with oracle refs: {:?}",
                manifest.id, messages
            );
        }
    }

    #[test]
    fn default_check_pass_signature_compiles() {
        let (passed, messages) = check_pass(&[]);
        assert!(!passed);
        assert!(!messages.is_empty());
    }

    #[test]
    fn strictness_controls_expected_ref_matching() {
        let manifests = load_manifests().expect("all eval manifests parse");

        let cross_modal = manifests
            .iter()
            .find(|manifest| manifest.query_class == QueryClass::CrossModalCodeProse)
            .expect("cross-modal manifest exists");
        assert_eq!(cross_modal.pass_strictness, PassStrictness::All);
        let cross_modal_refs = parsed_expected(cross_modal);
        let checker = checker_by_name(&cross_modal.pass_classifier).unwrap();
        let (passed, _messages) = checker(&cross_modal_refs[..1]);
        assert!(
            !passed,
            "cross-modal checks require all expected refs, not just one"
        );
        let (passed, _messages) = checker(&cross_modal_refs);
        assert!(passed);

        let transcript = manifests
            .iter()
            .find(|manifest| manifest.query_class == QueryClass::TranscriptProvenance)
            .expect("transcript manifest exists");
        assert_eq!(transcript.pass_strictness, PassStrictness::First);

        let exact = manifests
            .iter()
            .find(|manifest| manifest.query_class == QueryClass::ExactSymbol)
            .expect("exact-symbol manifest exists");
        assert_eq!(exact.pass_strictness, PassStrictness::Any);
    }

    #[test]
    fn checker_manifest_lookup_uses_cached_parse_result() {
        let checker = checker_by_name("check_exact_symbol_knowledge_store").unwrap();
        let manifests = load_manifests().unwrap();
        let expected = parsed_expected(
            manifests
                .iter()
                .find(|manifest| manifest.pass_classifier == "check_exact_symbol_knowledge_store")
                .unwrap(),
        );
        for _ in 0..90 {
            let (passed, messages) = checker(&expected);
            assert!(passed, "{messages:?}");
        }

        assert!(
            CHECKER_MANIFESTS.get().is_some(),
            "checker cache should be initialized after repeated checker calls"
        );
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

    fn parsed_expected(manifest: &EvalQueryManifest) -> Vec<EntityRef> {
        manifest
            .expected_entity_refs
            .iter()
            .map(|raw| EntityRef::parse(raw).unwrap())
            .collect()
    }

    fn expected_ref_resolves(manifest: &EvalQueryManifest, entity_ref: &EntityRef) -> bool {
        match entity_ref {
            EntityRef::Knowledge { id } => manifest.target_locators.iter().any(|locator| {
                locator.entity_type_hint == "knowledge"
                    && locator.knowledge_hint.as_deref() == Some(id.as_str())
            }),
            EntityRef::ProjectFile { .. } | EntityRef::Symbol { .. } => manifest
                .target_locators
                .iter()
                .filter(|locator| {
                    matches!(locator.entity_type_hint.as_str(), "project_file" | "symbol")
                })
                .any(|locator| locator_resolves_source_ref(locator, entity_ref)),
            EntityRef::Transcript {
                provider,
                session_id,
                line_offset,
                ..
            } => manifest
                .target_locators
                .iter()
                .filter(|locator| locator.entity_type_hint == "transcript")
                .any(|locator| {
                    transcript_ref_matches_locator(
                        provider,
                        session_id,
                        *line_offset,
                        locator.transcript_hint.as_deref().unwrap_or_default(),
                    )
                }),
            EntityRef::Commit { repo_id, sha } => manifest
                .target_locators
                .iter()
                .filter(|locator| locator.entity_type_hint == "commit")
                .any(|locator| {
                    locator
                        .transcript_hint
                        .as_deref()
                        .is_some_and(|hint| sha.starts_with(hint))
                        || !repo_id.is_empty() && !sha.is_empty()
                }),
            _ => true,
        }
    }

    fn locator_resolves_source_ref(locator: &TargetLocator, entity_ref: &EntityRef) -> bool {
        let Some(path_hint) = &locator.path_hint else {
            return false;
        };
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(path_hint);
        let Ok(chunks) = chunks_for_path(&path, Path::new(path_hint)) else {
            return false;
        };
        chunks.iter().any(|chunk| match entity_ref {
            EntityRef::ProjectFile {
                project_id,
                rel_path_hash,
                chunk_hash,
                occurrence_idx,
            } => {
                &chunk.project_id == project_id
                    && &chunk.rel_path_hash == rel_path_hash
                    && &chunk.chunk_hash == chunk_hash
                    && &chunk.occurrence_idx == occurrence_idx
            }
            EntityRef::Symbol {
                project_id,
                qualified_name,
                defn_hash,
            } => {
                &chunk.project_id == project_id
                    && chunk.symbol.as_deref() == Some(qualified_name.as_str())
                    && &chunk.chunk_hash == defn_hash
            }
            _ => false,
        })
    }

    fn chunks_for_path(
        abs_path: &Path,
        rel_path: &Path,
    ) -> anyhow::Result<Vec<crate::chunker::Chunk>> {
        let bytes = fs::read(abs_path)?;
        let sniff_len = bytes.len().min(4096);
        let mut chunks = Vec::new();
        for chunker in crate::chunker::default_registry() {
            if !chunker.claims(rel_path, &bytes[..sniff_len]) {
                continue;
            }
            chunks = chunker.chunk(rel_path, &bytes)?.0;
            break;
        }
        let project_id = project_id();
        let rel_path_hash = short_hash(rel_path.to_string_lossy().as_bytes());
        Ok(bound_chunks(
            chunks
                .into_iter()
                .enumerate()
                .map(|(idx, mut chunk)| {
                    chunk.project_id.clone_from(&project_id);
                    chunk.file_path = rel_path.to_path_buf();
                    chunk.rel_path_hash.clone_from(&rel_path_hash);
                    chunk.chunk_hash = full_hash(chunk.content.as_bytes());
                    chunk.occurrence_idx = idx as u32;
                    chunk
                })
                .collect::<Vec<_>>()
                .as_slice(),
        ))
    }

    fn bound_chunks(chunks: &[crate::chunker::Chunk]) -> Vec<crate::chunker::Chunk> {
        chunks
            .iter()
            .flat_map(|chunk| {
                if chunk.content.len() <= crate::chunker::MAX_CHUNK_BYTES {
                    return vec![chunk.clone()];
                }
                let mut out = Vec::new();
                let mut start = 0usize;
                while start < chunk.content.len() {
                    let mut end =
                        (start + crate::chunker::MAX_CHUNK_BYTES).min(chunk.content.len());
                    while !chunk.content.is_char_boundary(end) {
                        end -= 1;
                    }
                    let mut split = chunk.clone();
                    split.content = chunk.content[start..end].to_string();
                    split.byte_start = chunk.byte_start + start as u64;
                    split.byte_end = chunk.byte_start + end as u64;
                    split.chunk_hash = full_hash(split.content.as_bytes());
                    split.occurrence_idx = out.len() as u32;
                    out.push(split);
                    start = end;
                }
                out
            })
            .enumerate()
            .map(|(idx, mut chunk)| {
                chunk.occurrence_idx = idx as u32;
                chunk
            })
            .collect()
    }

    fn transcript_ref_matches_locator(
        provider: &str,
        session_id: &str,
        line_offset: u64,
        hint: &str,
    ) -> bool {
        let Some(path) = transcript_path(provider, session_id) else {
            return false;
        };
        let Ok(bytes) = fs::read(&path) else {
            return false;
        };
        let offset = line_offset as usize;
        if offset >= bytes.len() || (offset > 0 && bytes[offset - 1] != b'\n') {
            return false;
        }
        let end = bytes[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|idx| offset + idx)
            .unwrap_or(bytes.len());
        let line = String::from_utf8_lossy(&bytes[offset..end]).to_lowercase();
        hint.split_whitespace()
            .filter(|term| term.len() > 3)
            .any(|term| line.contains(&term.to_lowercase()))
    }

    fn transcript_path(provider: &str, session_id: &str) -> Option<PathBuf> {
        let mut homes = Vec::new();
        if let Some(home) = dirs::home_dir() {
            homes.push(home);
        }
        let fixture_home = PathBuf::from("/home/invidious");
        if !homes.iter().any(|home| home == &fixture_home) {
            homes.push(fixture_home);
        }

        for home in homes {
            let candidates = if provider == "codex" {
                vec![home.join(".codex").join("sessions")]
            } else if provider == "claude" {
                vec![home.join(".claude").join("projects")]
            } else {
                vec![home.join(format!(".claude-{provider}")).join("projects")]
            };
            for root in candidates {
                if let Ok(Some(found)) = find_transcript(&root, session_id) {
                    return Some(found);
                }
            }
        }
        None
    }

    fn find_transcript(root: &Path, session_id: &str) -> std::io::Result<Option<PathBuf>> {
        if !root.exists() {
            return Ok(None);
        }
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if let Some(found) = find_transcript(&path, session_id)? {
                    return Ok(Some(found));
                }
            } else if path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem == session_id)
            {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    fn project_id() -> String {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .expect("repo root canonicalizes");
        short_hash(root.to_string_lossy().as_bytes())
    }

    fn short_hash(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        hex::encode(&digest[..4])
    }

    fn full_hash(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        hex::encode(digest)
    }
}
