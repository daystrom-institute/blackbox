use std::collections::BTreeMap;

use serde_json::json;

use super::test_support::tmp_packets;
use super::{
    ApplyParams, AuditParams, Emit, Packet, Packets, Predicate, Rule, Value, apply, review_lattice,
    review_prefix_inference,
};

/// Build the E8 authorization matrix packet (the merged Gemini-style
/// encoding from thread-0b20e854) as a typed Rust value. This is the
/// definitional round-trip: if this packet evaluates faithfully over
/// the matrix, the primitive works end-to-end.
fn e8_auth_packet() -> Packet {
    let now = Packets::now_iso();

    // Rank table: role -> rank (from the E8 merged packet)
    let rank_table: BTreeMap<String, i64> = [
        ("auditor", 0),
        ("reader", 1),
        ("editor", 2),
        ("owner", 3),
        ("admin", 4),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    // Threshold table: resource -> threshold
    let threshold_table: BTreeMap<String, i64> = [
        ("public", 1),
        ("team", 2),
        ("private", 3),
        ("billing", 3),
        ("archived", 4),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    // Rules: anomalies first, then generals.
    let rules = vec![
        // Anomalies (read)
        Rule {
            id: "anom_reader_get_team".into(),
            antecedent: Predicate::All {
                args: vec![
                    Predicate::Eq {
                        field: "role".into(),
                        value: Value::String("reader".into()),
                    },
                    Predicate::Eq {
                        field: "method".into(),
                        value: Value::String("GET".into()),
                    },
                    Predicate::Eq {
                        field: "resource".into(),
                        value: Value::String("team".into()),
                    },
                ],
            },
            consequent: Value::String("DENY".into()),
            classification: "info".to_string(),
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        },
        Rule {
            id: "anom_auditor_get_private".into(),
            antecedent: Predicate::All {
                args: vec![
                    Predicate::Eq {
                        field: "role".into(),
                        value: Value::String("auditor".into()),
                    },
                    Predicate::Eq {
                        field: "method".into(),
                        value: Value::String("GET".into()),
                    },
                    Predicate::Eq {
                        field: "resource".into(),
                        value: Value::String("private".into()),
                    },
                ],
            },
            consequent: Value::String("DENY".into()),
            classification: "info".to_string(),
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        },
        // Anomalies (write)
        Rule {
            id: "anom_admin_delete_billing".into(),
            antecedent: Predicate::All {
                args: vec![
                    Predicate::Eq {
                        field: "role".into(),
                        value: Value::String("admin".into()),
                    },
                    Predicate::Eq {
                        field: "method".into(),
                        value: Value::String("DELETE".into()),
                    },
                    Predicate::Eq {
                        field: "resource".into(),
                        value: Value::String("billing".into()),
                    },
                ],
            },
            consequent: Value::String("DENY".into()),
            classification: "info".to_string(),
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        },
        Rule {
            id: "anom_owner_patch_public".into(),
            antecedent: Predicate::All {
                args: vec![
                    Predicate::Eq {
                        field: "role".into(),
                        value: Value::String("owner".into()),
                    },
                    Predicate::Eq {
                        field: "method".into(),
                        value: Value::String("PATCH".into()),
                    },
                    Predicate::Eq {
                        field: "resource".into(),
                        value: Value::String("public".into()),
                    },
                ],
            },
            consequent: Value::String("DENY".into()),
            classification: "info".to_string(),
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        },
        Rule {
            id: "anom_editor_post_archived".into(),
            antecedent: Predicate::All {
                args: vec![
                    Predicate::Eq {
                        field: "role".into(),
                        value: Value::String("editor".into()),
                    },
                    Predicate::Eq {
                        field: "method".into(),
                        value: Value::String("POST".into()),
                    },
                    Predicate::Eq {
                        field: "resource".into(),
                        value: Value::String("archived".into()),
                    },
                ],
            },
            consequent: Value::String("ALLOW".into()),
            classification: "info".to_string(),
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        },
        // GET default allow (after GET exceptions above)
        Rule {
            id: "get_default_allow".into(),
            antecedent: Predicate::Eq {
                field: "method".into(),
                value: Value::String("GET".into()),
            },
            consequent: Value::String("ALLOW".into()),
            classification: "info".to_string(),
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        },
        // Write default: allow iff role_rank >= res_threshold
        Rule {
            id: "write_rank_ge_threshold".into(),
            antecedent: Predicate::RankGeFieldThreshold {
                rank_field: "role_rank".into(),
                threshold_field: "res_threshold".into(),
            },
            consequent: Value::String("ALLOW".into()),
            classification: "info".to_string(),
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        },
        // Catch-all deny
        Rule {
            id: "default_deny".into(),
            antecedent: Predicate::AlwaysTrue {},
            consequent: Value::String("DENY".into()),
            classification: "info".to_string(),
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        },
    ];

    Packet {
        id: "packet-e8test01".into(),
        domain: "e8-auth-matrix".into(),
        scope: "global".into(),
        project: None,
        project_id: None,
        rank_table,
        threshold_table,
        rank_lookup_key: "role".into(),
        threshold_lookup_key: "resource".into(),
        classification_lattice: review_lattice(),
        prefix_inference: review_prefix_inference(),
        rules,
        source_ids: vec!["thread-0b20e854".into()],
        self_audit_fidelity: None,
        created_at: now.clone(),
        updated_at: now,
        superseded_by: None,
        merged_from: vec![],
    }
}

/// Ground truth: the 5 hidden anomalies used in the E8 matrix.
fn ground_truth_allow(role: &str, method: &str, resource: &str) -> Value {
    // Anomaly lookup
    let anom = [
        ("reader", "GET", "team", "DENY"),
        ("auditor", "GET", "private", "DENY"),
        ("admin", "DELETE", "billing", "DENY"),
        ("owner", "PATCH", "public", "DENY"),
        ("editor", "POST", "archived", "ALLOW"),
    ];
    for (r, m, res, v) in anom {
        if role == r && method == m && resource == res {
            return Value::String(v.to_string());
        }
    }

    // Otherwise: GET default ALLOW, write -> rank gate
    if method == "GET" {
        return Value::String("ALLOW".into());
    }

    let rank = match role {
        "auditor" => 0,
        "reader" => 1,
        "editor" => 2,
        "owner" => 3,
        "admin" => 4,
        _ => unreachable!(),
    };
    let threshold = match resource {
        "public" => 1,
        "team" => 2,
        "private" | "billing" => 3,
        "archived" => 4,
        _ => unreachable!(),
    };
    if rank >= threshold {
        Value::String("ALLOW".into())
    } else {
        Value::String("DENY".into())
    }
}

#[test]
fn e8_packet_round_trips_full_matrix() {
    let packet = e8_auth_packet();
    let roles = ["reader", "editor", "auditor", "admin", "owner"];
    let methods = ["GET", "POST", "PUT", "DELETE", "PATCH"];
    let resources = ["public", "team", "private", "archived", "billing"];

    let mut correct = 0;
    let mut total = 0;
    let mut mismatches: Vec<String> = Vec::new();

    for role in &roles {
        for method in &methods {
            for resource in &resources {
                let entity = json!({
                    "role": role,
                    "method": method,
                    "resource": resource,
                });
                let expected = ground_truth_allow(role, method, resource);
                total += 1;
                match apply(&packet, &entity) {
                    Some(p) if p.consequent == expected => correct += 1,
                    Some(p) => mismatches.push(format!(
                        "({role},{method},{resource}) expected={:?} got={:?} rule={}",
                        expected, p.consequent, p.rule_id
                    )),
                    None => mismatches.push(format!(
                        "({role},{method},{resource}) expected={:?} got=UNMATCHED",
                        expected
                    )),
                }
            }
        }
    }

    assert_eq!(total, 125, "125 cells total");
    assert_eq!(
        correct,
        125,
        "Expected 125/125, got {correct}/125. Mismatches:\n{}",
        mismatches.join("\n")
    );
}

#[test]
fn e8_packet_extrapolates_to_new_role_and_new_resource() {
    // Same packet, but we add "contributor" and "staging" to the
    // lookup tables. The rules themselves DO NOT mention these
    // names - if they still produce correct answers, the packet
    // genuinely encoded laws rather than per-role tables.
    let mut packet = e8_auth_packet();
    packet.rank_table.insert("contributor".into(), 2);
    packet.threshold_table.insert("staging".into(), 2);

    // 15 cells mirroring the experiment's extrapolation set.
    let cases: &[(&str, &str, &str, &str)] = &[
        ("contributor", "GET", "public", "ALLOW"),
        ("contributor", "GET", "team", "ALLOW"),
        ("contributor", "POST", "team", "ALLOW"),
        ("contributor", "POST", "private", "DENY"),
        ("contributor", "DELETE", "archived", "DENY"),
        ("contributor", "POST", "billing", "DENY"),
        ("contributor", "PATCH", "public", "ALLOW"),
        ("contributor", "PUT", "team", "ALLOW"),
        ("contributor", "DELETE", "private", "DENY"),
        ("contributor", "GET", "billing", "ALLOW"),
        ("editor", "POST", "staging", "ALLOW"),
        ("reader", "DELETE", "staging", "DENY"),
        ("auditor", "GET", "staging", "ALLOW"),
        ("admin", "PATCH", "staging", "ALLOW"),
        ("contributor", "POST", "staging", "ALLOW"),
    ];

    let mut misses = Vec::new();
    for (role, method, resource, expected) in cases {
        let entity = json!({
            "role": role,
            "method": method,
            "resource": resource,
        });
        let expected_val = Value::String(expected.to_string());
        match apply(&packet, &entity) {
            Some(p) if p.consequent == expected_val => {}
            other => misses.push(format!(
                "({role},{method},{resource}) expected={expected} got={:?}",
                other
            )),
        }
    }

    assert!(
        misses.is_empty(),
        "Expected 15/15, misses: \n{}",
        misses.join("\n")
    );
}

#[test]
fn save_load_round_trip() {
    let (_dir, store) = tmp_packets();
    let packet = e8_auth_packet();
    store.save_packet(&packet).unwrap();

    let loaded = store.load(&packet.id).unwrap();
    assert_eq!(loaded.id, packet.id);
    assert_eq!(loaded.rules.len(), packet.rules.len());
    // Evaluate a representative cell after round-trip
    let entity = json!({
        "role": "admin",
        "method": "DELETE",
        "resource": "billing",
    });
    let prediction = apply(&loaded, &entity).unwrap();
    assert_eq!(prediction.consequent, Value::String("DENY".into()));
    assert_eq!(prediction.rule_id, "anom_admin_delete_billing");
}

#[test]
fn apply_tool_and_audit_tool() {
    let (_dir, store) = tmp_packets();
    let packet = e8_auth_packet();
    store.save_packet(&packet).unwrap();

    // apply
    let apply_params = ApplyParams {
        packet_id: packet.id.clone(),
        entity: json!({
            "role": "reader",
            "method": "GET",
            "resource": "team",
        }),
        mode: None,
    };
    let out = store.apply_tool(&apply_params).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["consequent"],
        "DENY"
    );
    assert!(out.contains("anom_reader_get_team"));

    // audit
    let dataset = json!([
        {"entity": {"role": "reader", "method": "GET", "resource": "public"}, "expected": "ALLOW"},
        {"entity": {"role": "reader", "method": "GET", "resource": "team"}, "expected": "DENY"},
        {"entity": {"role": "editor", "method": "POST", "resource": "archived"}, "expected": "ALLOW"},
        {"entity": {"role": "owner", "method": "DELETE", "resource": "billing"}, "expected": "ALLOW"},
        {"entity": {"role": "auditor", "method": "DELETE", "resource": "team"}, "expected": "DENY"},
    ]);
    let audit_params = AuditParams {
        packet_id: packet.id.clone(),
        dataset,
        mode: None,
    };
    let report_text = store.audit_tool(&audit_params).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&report_text).unwrap()["total"],
        5
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&report_text).unwrap()["correct"],
        5
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&report_text).unwrap()["fidelity"],
        1.0
    );
}

#[test]
fn audit_flags_mismatches() {
    let (_dir, store) = tmp_packets();
    let packet = e8_auth_packet();
    store.save_packet(&packet).unwrap();

    // One entry has a deliberately wrong expected value to exercise
    // the mismatch path.
    let dataset = json!([
        {"entity": {"role": "reader", "method": "GET", "resource": "public"}, "expected": "ALLOW"},
        {"entity": {"role": "reader", "method": "GET", "resource": "team"}, "expected": "ALLOW"},
    ]);
    let report_text = store
        .audit_tool(&AuditParams {
            packet_id: packet.id.clone(),
            dataset,
            mode: None,
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&report_text).unwrap()["total"],
        2
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&report_text).unwrap()["correct"],
        1
    );
}

#[test]
fn missing_packet_errors_clearly() {
    let (_dir, store) = tmp_packets();
    let err = store
        .apply_tool(&ApplyParams {
            packet_id: "packet-deadbeef".into(),
            entity: json!({}),
            mode: None,
        })
        .unwrap_err()
        .to_string();
    assert!(err.contains("not found"));
}
