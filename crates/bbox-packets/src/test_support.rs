use std::collections::BTreeMap;

use super::{
    CompileParams, Emit, Packet, Packets, Predicate, Rule, Value, review_lattice,
    review_prefix_inference,
};

use serde_json::json;
use tempfile::TempDir;

pub(super) fn tmp_packets() -> (TempDir, Packets) {
    let dir = TempDir::new().unwrap();
    let packets = Packets::open(dir.path()).unwrap();
    (dir, packets)
}

pub(super) fn bare_packet(rules: Vec<Rule>) -> Packet {
    let now = Packets::now_iso();
    Packet {
        id: "packet-phase2t".into(),
        domain: "phase2-test".into(),
        scope: "global".into(),
        project: None,
        project_id: None,
        rank_table: BTreeMap::new(),
        threshold_table: BTreeMap::new(),
        rank_lookup_key: "role".into(),
        threshold_lookup_key: "resource".into(),
        classification_lattice: review_lattice(),
        prefix_inference: review_prefix_inference(),
        rules,
        source_ids: vec![],
        self_audit_fidelity: None,
        created_at: now.clone(),
        updated_at: now,
        superseded_by: None,
        merged_from: vec![],
    }
}

pub(super) fn rule(id: &str, antecedent: Predicate, consequent: &str, class: &str) -> Rule {
    Rule {
        id: id.into(),
        antecedent,
        consequent: Value::String(consequent.into()),
        classification: class.into(),
        emit: Emit::Independent,
        confidence: 1.0,
        provenance: vec![],
    }
}

pub(super) fn fallback_rule(
    id: &str,
    antecedent: Predicate,
    consequent: &str,
    class: &str,
) -> Rule {
    Rule {
        id: id.into(),
        antecedent,
        consequent: Value::String(consequent.into()),
        classification: class.into(),
        emit: Emit::Fallback,
        confidence: 1.0,
        provenance: vec![],
    }
}

/// Compile a minimal "is_breaking" sub-packet: breaks if api_surface
/// changed AND no migration note. Lattice: [breaking, safe].
pub(super) fn compile_breaking_packet(packets: &Packets) -> String {
    let out = packets
        .compile(&CompileParams {
            domain: "pr-breakingness".into(),
            scope: Some("global".into()),
            project: None,
            project_id: None,
            classification_lattice: Some(vec!["breaking".into(), "safe".into()]),
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([
                {
                    "id": "breaking_api_no_migration",
                    "classification": "breaking",
                    "antecedent": {"op": "All", "args": [
                        {"op": "Eq", "field": "api_surface_changed", "value": true},
                        {"op": "Eq", "field": "migration_note_present", "value": false}
                    ]},
                    "consequent": "BREAKING"
                },
                {
                    "id": "safe_default",
                    "classification": "safe",
                    "emit": "fallback",
                    "antecedent": {"op": "True"},
                    "consequent": "SAFE"
                }
            ]),
        })
        .unwrap();
    // compile returns "Packet packet-xxxxxxxx compiled (...)"
    out.split_whitespace().nth(1).unwrap().to_string()
}
