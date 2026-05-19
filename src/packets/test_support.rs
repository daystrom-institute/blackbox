use super::{CompileParams, Packets};

use serde_json::json;
use tempfile::TempDir;

pub(super) fn tmp_packets() -> (TempDir, Packets) {
    let dir = TempDir::new().unwrap();
    let packets = Packets::open(dir.path()).unwrap();
    (dir, packets)
}

/// Compile a minimal "is_breaking" sub-packet: breaks if api_surface
/// changed AND no migration note. Lattice: [breaking, safe].
pub(super) fn compile_breaking_packet(packets: &Packets) -> String {
    let out = packets
        .compile(&CompileParams {
            domain: "pr-breakingness".into(),
            scope: Some("global".into()),
            project: None,
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
