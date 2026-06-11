use std::collections::BTreeMap;

use serde_json::json;

use super::super::test_support::{compile_breaking_packet, tmp_packets};
use super::super::{
    CompileParams, MAX_COMPOSITION_DEPTH, NoopResolver, Packets, Predicate, Value, apply_with,
    eval_predicate,
};

#[test]
fn count_matches_composes_with_apply_for_subpacket_tally() {
    // End-to-end: compose three sub-packet Apply nodes under CountMatches.
    let (_d, packets) = tmp_packets();
    let red_lattice = vec!["red".into(), "green".into()];

    let make_red_sub = |packets: &Packets, domain: &str, field: &str| -> String {
        let out = packets
            .compile(&CompileParams {
                domain: domain.into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: Some(red_lattice.clone()),
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([
                    {
                        "id": "red_on_true",
                        "classification": "red",
                        "antecedent": {"op": "Eq", "field": field, "value": true},
                        "consequent": "RED"
                    },
                    {
                        "id": "green_default",
                        "classification": "green",
                        "emit": "fallback",
                        "antecedent": {"op": "True"},
                        "consequent": "GREEN"
                    }
                ]),
            })
            .unwrap();
        out.split_whitespace().nth(1).unwrap().to_string()
    };
    let a = make_red_sub(&packets, "a-sub", "a_red");
    let b = make_red_sub(&packets, "b-sub", "b_red");
    let c = make_red_sub(&packets, "c-sub", "c_red");

    let out = packets
        .compile(&CompileParams {
            domain: "tally".into(),
            scope: Some("global".into()),
            project: None,
            classification_lattice: Some(vec!["block".into(), "review".into(), "ok".into()]),
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([
                {
                    "id": "block_2plus_red",
                    "classification": "block",
                    "antecedent": {
                        "op": "CountMatches",
                        "args": [
                            {"op": "Apply", "packet_id": a, "expect": ["red"]},
                            {"op": "Apply", "packet_id": b, "expect": ["red"]},
                            {"op": "Apply", "packet_id": c, "expect": ["red"]}
                        ],
                        "compare": "ge",
                        "value": 2
                    },
                    "consequent": "BLOCK"
                },
                {
                    "id": "review_1_red",
                    "classification": "review",
                    "antecedent": {
                        "op": "CountMatches",
                        "args": [
                            {"op": "Apply", "packet_id": a, "expect": ["red"]},
                            {"op": "Apply", "packet_id": b, "expect": ["red"]},
                            {"op": "Apply", "packet_id": c, "expect": ["red"]}
                        ],
                        "compare": "eq",
                        "value": 1
                    },
                    "consequent": "REVIEW"
                },
                {
                    "id": "ok_default",
                    "classification": "ok",
                    "emit": "fallback",
                    "antecedent": {"op": "True"},
                    "consequent": "OK"
                }
            ]),
        })
        .unwrap();
    let master_id = out.split_whitespace().nth(1).unwrap().to_string();
    let master = packets.load(&master_id).unwrap();

    let all_green = json!({"a_red": false, "b_red": false, "c_red": false});
    assert_eq!(
        apply_with(&master, &all_green, &packets).unwrap().rule_id,
        "ok_default"
    );
    let one_red = json!({"a_red": true, "b_red": false, "c_red": false});
    assert_eq!(
        apply_with(&master, &one_red, &packets).unwrap().rule_id,
        "review_1_red"
    );
    let two_red = json!({"a_red": true, "b_red": true, "c_red": false});
    assert_eq!(
        apply_with(&master, &two_red, &packets).unwrap().rule_id,
        "block_2plus_red"
    );
    let three_red = json!({"a_red": true, "b_red": true, "c_red": true});
    assert_eq!(
        apply_with(&master, &three_red, &packets).unwrap().rule_id,
        "block_2plus_red"
    );
}

#[test]
fn count_matches_compile_validates_nested_apply_refs() {
    let (_d, packets) = tmp_packets();
    let err = packets
        .compile(&CompileParams {
            domain: "test".into(),
            scope: Some("global".into()),
            project: None,
            classification_lattice: None,
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([{
                "id": "fail_nested_ref",
                "antecedent": {
                    "op": "CountMatches",
                    "args": [
                        {"op": "Apply", "packet_id": "packet-missing1", "expect": ["red"]}
                    ],
                    "compare": "ge",
                    "value": 1
                },
                "consequent": "X"
            }]),
        })
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("packet-missing1"),
        "expected missing-packet error via nested CountMatches, got: {msg}"
    );
}

#[test]
fn apply_node_composes_sub_packet_verdict() {
    let (_d, packets) = tmp_packets();
    let sub_id = compile_breaking_packet(&packets);

    let outer = packets
        .compile(&CompileParams {
            domain: "pr-triage".into(),
            scope: Some("global".into()),
            project: None,
            classification_lattice: None,
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([
                {
                    "id": "fail_breaking",
                    "antecedent": {
                        "op": "Apply",
                        "packet_id": sub_id.clone(),
                        "expect": ["breaking"],
                    },
                    "consequent": "REJECT"
                },
                {
                    "id": "pass_default",
                    "emit": "fallback",
                    "antecedent": {"op": "True"},
                    "consequent": "ACCEPT"
                }
            ]),
        })
        .unwrap();
    let outer_id = outer.split_whitespace().nth(1).unwrap();
    let outer_pkt = packets.load(outer_id).unwrap();

    let breaking = json!({
        "api_surface_changed": true,
        "migration_note_present": false,
    });
    let pred = apply_with(&outer_pkt, &breaking, &packets).unwrap();
    assert_eq!(pred.rule_id, "fail_breaking");
    assert_eq!(pred.consequent, Value::String("REJECT".into()));

    let safe = json!({
        "api_surface_changed": true,
        "migration_note_present": true,
    });
    let pred = apply_with(&outer_pkt, &safe, &packets).unwrap();
    assert_eq!(pred.rule_id, "pass_default");
}

#[test]
fn apply_node_returns_false_when_resolver_cannot_find_packet() {
    let (_d, packets) = tmp_packets();
    let pred = Predicate::Apply {
        packet_id: "packet-deadbeef".into(),
        expect: vec!["breaking".into()],
        entity_map: BTreeMap::new(),
    };
    let entity = serde_json::Map::new();
    assert!(!eval_predicate(&pred, &entity, &packets, 0));
    assert!(!eval_predicate(&pred, &entity, &NoopResolver, 0));
}

#[test]
fn compile_rejects_apply_with_missing_sub_packet() {
    let (_d, packets) = tmp_packets();
    let err = packets
        .compile(&CompileParams {
            domain: "test".into(),
            scope: Some("global".into()),
            project: None,
            classification_lattice: None,
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([{
                "id": "fail_missing",
                "antecedent": {
                    "op": "Apply",
                    "packet_id": "packet-nonexistent",
                    "expect": ["breaking"]
                },
                "consequent": "REJECT"
            }]),
        })
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("packet-nonexistent") && msg.contains("not in the store"),
        "expected missing-packet error, got: {msg}"
    );
}

#[test]
fn compile_rejects_apply_with_expect_outside_sub_lattice() {
    let (_d, packets) = tmp_packets();
    let sub_id = compile_breaking_packet(&packets);
    let err = packets
        .compile(&CompileParams {
            domain: "test".into(),
            scope: Some("global".into()),
            project: None,
            classification_lattice: None,
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([{
                "id": "fail_typo",
                "antecedent": {
                    "op": "Apply",
                    "packet_id": sub_id,
                    "expect": ["brekaing"]
                },
                "consequent": "REJECT"
            }]),
        })
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("brekaing") && msg.contains("lattice"),
        "expected lattice-mismatch error, got: {msg}"
    );
}

#[test]
fn apply_node_entity_map_rebinds_fields() {
    let (_d, packets) = tmp_packets();
    let sub_id = compile_breaking_packet(&packets);

    let outer = packets
        .compile(&CompileParams {
            domain: "pr-triage-remap".into(),
            scope: Some("global".into()),
            project: None,
            classification_lattice: None,
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([
                {
                    "id": "fail_via_mapped",
                    "antecedent": {
                        "op": "Apply",
                        "packet_id": sub_id,
                        "expect": ["breaking"],
                        "entity_map": {
                            "api_surface_changed": "did_break",
                            "migration_note_present": "has_migration_doc"
                        }
                    },
                    "consequent": "REJECT"
                },
                {
                    "id": "pass_default",
                    "emit": "fallback",
                    "antecedent": {"op": "True"},
                    "consequent": "ACCEPT"
                }
            ]),
        })
        .unwrap();
    let outer_id = outer.split_whitespace().nth(1).unwrap();
    let outer_pkt = packets.load(outer_id).unwrap();

    let breaking = json!({
        "did_break": true,
        "has_migration_doc": false,
    });
    let pred = apply_with(&outer_pkt, &breaking, &packets).unwrap();
    assert_eq!(pred.rule_id, "fail_via_mapped");
}

#[test]
fn apply_node_respects_depth_limit() {
    let (_d, packets) = tmp_packets();

    let base_id = {
        let out = packets
            .compile(&CompileParams {
                domain: "chain-base".into(),
                scope: Some("global".into()),
                project: None,
                classification_lattice: Some(vec!["match".into(), "nomatch".into()]),
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([{
                    "id": "always_match",
                    "classification": "match",
                    "antecedent": {"op": "True"},
                    "consequent": "M"
                }]),
            })
            .unwrap();
        out.split_whitespace().nth(1).unwrap().to_string()
    };

    let mut current = base_id.clone();
    for i in 0..(MAX_COMPOSITION_DEPTH + 2) {
        let out = packets
            .compile(&CompileParams {
                domain: format!("chain-{i}"),
                scope: Some("global".into()),
                project: None,
                classification_lattice: Some(vec!["match".into(), "nomatch".into()]),
                prefix_inference: None,
                rank_table: None,
                threshold_table: None,
                rank_lookup_key: None,
                threshold_lookup_key: None,
                source_ids: None,
                rules: json!([
                    {
                        "id": "match_via_next",
                        "classification": "match",
                        "antecedent": {
                            "op": "Apply",
                            "packet_id": current.clone(),
                            "expect": ["match"]
                        },
                        "consequent": "M"
                    },
                    {
                        "id": "nomatch_default",
                        "classification": "nomatch",
                        "emit": "fallback",
                        "antecedent": {"op": "True"},
                        "consequent": "N"
                    }
                ]),
            })
            .unwrap();
        current = out.split_whitespace().nth(1).unwrap().to_string();
    }

    let outer = packets.load(&current).unwrap();
    let pred = apply_with(&outer, &json!({}), &packets).unwrap();
    assert_eq!(
        pred.classification, "nomatch",
        "depth limit should prevent the outer chain from resolving to 'match'"
    );
}
