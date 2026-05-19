use super::test_support::tmp_packets;
use std::collections::BTreeMap;

use super::{
    ApplyMode, ApplyParams, AuditParams, CmpOp, CompileParams, Emit, MAX_COMPOSITION_DEPTH,
    NoopResolver, Packet, Packets, Predicate, Rule, RuleInput, Value, apply, apply_all, apply_with,
    default_rank_lookup_key, default_threshold_lookup_key, eval_predicate, infer_classification,
    packet_matches_query, packet_summary, review_lattice, review_prefix_inference, verify_all,
};
use serde_json::json;

/// Build the E8 authorization matrix packet (the merged Gemini-style
/// encoding from thread-0b20e854) as a typed Rust value. This is the
/// definitional round-trip: if this packet evaluates faithfully over
/// the matrix, the primitive works end-to-end.
fn e8_auth_packet() -> Packet {
    let now = Packets::now_iso();

    // Rank table: role → rank (from the E8 merged packet)
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

    // Threshold table: resource → threshold
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

    // Otherwise: GET default ALLOW, write → rank gate
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
    // names — if they still produce correct answers, the packet
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
fn compile_tool_happy_path() {
    let (_dir, store) = tmp_packets();
    // Minimal 2-rule packet via the public MCP surface.
    let params = CompileParams {
        domain: "minimal-test".to_string(),
        rules: json!([
            {
                "id": "always_allow_get",
                "antecedent": {"op": "Eq", "field": "method", "value": "GET"},
                "consequent": "ALLOW",
                "confidence": 1.0
            },
            {
                "id": "default_deny",
                "antecedent": {"op": "True"},
                "consequent": "DENY",
                "confidence": 1.0
            }
        ]),
        classification_lattice: None,
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    let msg = store.compile(&params).unwrap();
    assert!(msg.contains("Packet packet-"));
    assert!(msg.contains("minimal-test"));

    // Verify listing works
    let packets = store.list_all().unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].rules.len(), 2);
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
    assert!(out.contains("\"consequent\": \"DENY\""));
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
    assert!(report_text.contains("\"total\": 5"));
    assert!(report_text.contains("\"correct\": 5"));
    assert!(report_text.contains("\"fidelity\": 1.0"));
}

#[test]
fn packet_list_helpers_match_and_summarize() {
    // Exercises the helpers bbox_packet_list / bbox_knowledge share:
    // packet_matches_query and packet_summary. Covers substring match
    // across id / domain / rule ids / classifications, the classification
    // histogram, the rule-id preview, and the latest-per-domain dedup
    // semantics (consumers rely on list_all being newest-first).

    fn mk_rule(id: &str, cls: &str) -> Rule {
        Rule {
            id: id.to_string(),
            antecedent: Predicate::AlwaysTrue {},
            consequent: Value::String(cls.to_uppercase()),
            classification: cls.to_string(),
            emit: Emit::Independent,
            confidence: 1.0,
            provenance: vec![],
        }
    }

    fn mk_packet(id: &str, domain: &str, created_at: &str, rules: Vec<Rule>) -> Packet {
        Packet {
            id: id.to_string(),
            domain: domain.to_string(),
            scope: "global".to_string(),
            project: None,
            rank_table: BTreeMap::new(),
            threshold_table: BTreeMap::new(),
            rank_lookup_key: default_rank_lookup_key(),
            threshold_lookup_key: default_threshold_lookup_key(),
            classification_lattice: vec![
                "fail".to_string(),
                "flag".to_string(),
                "pass".to_string(),
            ],
            prefix_inference: BTreeMap::new(),
            rules,
            source_ids: vec![],
            self_audit_fidelity: None,
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            superseded_by: None,
            merged_from: vec![],
        }
    }

    let pr_triage_old = mk_packet(
        "packet-11111111",
        "pr-triage",
        "2026-01-01T00:00:00Z",
        vec![
            mk_rule("breaking_api_change", "fail"),
            mk_rule("missing_tests", "flag"),
        ],
    );
    let pr_triage_new = mk_packet(
        "packet-22222222",
        "pr-triage",
        "2026-04-19T00:00:00Z",
        vec![
            mk_rule("breaking_api_change", "fail"),
            mk_rule("missing_tests", "flag"),
            mk_rule("all_clean", "pass"),
        ],
    );
    let auth_matrix = mk_packet(
        "packet-33333333",
        "auth-matrix",
        "2026-02-14T00:00:00Z",
        vec![
            mk_rule("deny_reader_team", "fail"),
            mk_rule("allow_owner_any", "pass"),
        ],
    );

    // --- packet_matches_query ---
    // domain hit (case-insensitive)
    assert!(packet_matches_query(&pr_triage_new, "PR-triage"));
    // rule id hit
    assert!(packet_matches_query(&pr_triage_new, "breaking"));
    // classification lattice hit
    assert!(packet_matches_query(&pr_triage_new, "FAIL"));
    // id hit
    assert!(packet_matches_query(&auth_matrix, "33333333"));
    // miss
    assert!(!packet_matches_query(&auth_matrix, "retry"));
    // empty query degenerates to false — caller is responsible for
    // skipping the filter on empty queries; the helper just answers.
    assert!(!packet_matches_query(&auth_matrix, "zzzzz"));

    // --- packet_summary ---
    let summary = packet_summary(&pr_triage_new);
    assert_eq!(summary["id"], "packet-22222222");
    assert_eq!(summary["domain"], "pr-triage");
    assert_eq!(summary["rules_count"], 3);
    // Histogram counts by classification
    let hist = &summary["classification_histogram"];
    assert_eq!(hist["fail"], 1);
    assert_eq!(hist["flag"], 1);
    assert_eq!(hist["pass"], 1);
    // Rule-id preview capped at 3
    let preview = summary["rule_ids_preview"].as_array().unwrap();
    assert_eq!(preview.len(), 3);
    assert_eq!(preview[0], "breaking_api_change");

    // --- list_all ordering + latest_per_domain dedup ---
    // Save via a tmp store and confirm list_all returns newest-first.
    let (_dir, store) = tmp_packets();
    store.save_packet(&pr_triage_old).unwrap();
    store.save_packet(&pr_triage_new).unwrap();
    store.save_packet(&auth_matrix).unwrap();

    let listed = store.list_all().unwrap();
    assert_eq!(listed.len(), 3);
    // Newest first: pr_triage_new (Apr), auth_matrix (Feb), pr_triage_old (Jan)
    assert_eq!(listed[0].id, "packet-22222222");
    assert_eq!(listed[1].id, "packet-33333333");
    assert_eq!(listed[2].id, "packet-11111111");

    // Simulate the latest_per_domain filter used by bbox_packet_list.
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<_> = listed
        .into_iter()
        .filter(|pkt| seen.insert(pkt.domain.clone()))
        .collect();
    assert_eq!(deduped.len(), 2);
    assert_eq!(deduped[0].id, "packet-22222222");
    assert_eq!(deduped[1].id, "packet-33333333");
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
    assert!(report_text.contains("\"total\": 2"));
    assert!(report_text.contains("\"correct\": 1"));
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

#[test]
fn predicate_serde_matches_e11_format() {
    // The E11 experiment output rules in this exact JSON shape.
    // If this round-trips, our AST is compatible with what LLMs
    // actually produce.
    let json_rule = json!({
        "op": "All",
        "args": [
            {"op": "Eq", "field": "role", "value": "admin"},
            {"op": "Eq", "field": "method", "value": "DELETE"},
            {"op": "Eq", "field": "resource", "value": "billing"}
        ]
    });
    let p: Predicate = serde_json::from_value(json_rule.clone()).unwrap();
    let back = serde_json::to_value(&p).unwrap();
    assert_eq!(back, json_rule);
}

// ── Phase 2 tests: applicability, field-vs-field, float, severity, evaluate-all ──

fn bare_packet(rules: Vec<Rule>) -> Packet {
    let now = Packets::now_iso();
    Packet {
        id: "packet-phase2t".into(),
        domain: "phase2-test".into(),
        scope: "global".into(),
        project: None,
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

fn rule(id: &str, antecedent: Predicate, consequent: &str, class: &str) -> Rule {
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

fn fallback_rule(id: &str, antecedent: Predicate, consequent: &str, class: &str) -> Rule {
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

#[test]
fn applicability_gate_discriminates_from_zero() {
    // The decisive phase-2 bug: rule said `Lt(tool_docs, 3)` fires on
    // a "clean" entity where no docs were added AND no tools were
    // added. The tri-state predicates let rules gate on applicability.
    let p = bare_packet(vec![rule(
        "fail_undocumented",
        Predicate::All {
            args: vec![
                Predicate::IsNonNull {
                    field: "mcp_tools_added".into(),
                },
                Predicate::Gt {
                    field: "mcp_tools_added".into(),
                    value: 0,
                },
                Predicate::FieldLt {
                    lhs_field: "tool_docs_stanzas_added".into(),
                    rhs_field: "mcp_tools_added".into(),
                },
            ],
        },
        "FAIL",
        "fail",
    )]);

    // No tools added — rule must NOT fire even though
    // tool_docs_stanzas_added=0 < some-constant.
    let clean = json!({
        "mcp_tools_added": 0,
        "tool_docs_stanzas_added": 0,
    });
    assert!(apply(&p, &clean).is_none(), "clean entity must not fire");

    // Tools added with too few docs — rule fires.
    let undoc = json!({
        "mcp_tools_added": 3,
        "tool_docs_stanzas_added": 1,
    });
    let pred = apply(&p, &undoc).expect("should fire on undoc");
    assert_eq!(pred.rule_id, "fail_undocumented");

    // Tools added, docs match — no fire.
    let ok = json!({
        "mcp_tools_added": 3,
        "tool_docs_stanzas_added": 3,
    });
    assert!(apply(&p, &ok).is_none(), "fully documented must not fire");
}

// The old IsPresent/IsAbsent pair has been removed; tri-state
// applicability is tested by `tri_state_applicability_discriminates_null_vs_missing`.

#[test]
fn field_comparisons_work_across_all_ops() {
    let cases: &[(Predicate, &str, serde_json::Value, bool)] = &[
        (
            Predicate::FieldEq {
                lhs_field: "a".into(),
                rhs_field: "b".into(),
            },
            "eq-hit",
            json!({"a": 5, "b": 5}),
            true,
        ),
        (
            Predicate::FieldEq {
                lhs_field: "a".into(),
                rhs_field: "b".into(),
            },
            "eq-miss",
            json!({"a": 5, "b": 6}),
            false,
        ),
        (
            Predicate::FieldGt {
                lhs_field: "a".into(),
                rhs_field: "b".into(),
            },
            "gt-hit",
            json!({"a": 10, "b": 5}),
            true,
        ),
        (
            Predicate::FieldGt {
                lhs_field: "a".into(),
                rhs_field: "b".into(),
            },
            "gt-eq",
            json!({"a": 5, "b": 5}),
            false,
        ),
        (
            Predicate::FieldGe {
                lhs_field: "a".into(),
                rhs_field: "b".into(),
            },
            "ge-eq",
            json!({"a": 5, "b": 5}),
            true,
        ),
        (
            Predicate::FieldLt {
                lhs_field: "a".into(),
                rhs_field: "b".into(),
            },
            "lt-hit",
            json!({"a": 1, "b": 5}),
            true,
        ),
        (
            Predicate::FieldLe {
                lhs_field: "a".into(),
                rhs_field: "b".into(),
            },
            "le-eq",
            json!({"a": 5, "b": 5}),
            true,
        ),
        // Missing field → false (no panic)
        (
            Predicate::FieldGt {
                lhs_field: "a".into(),
                rhs_field: "b".into(),
            },
            "missing-a",
            json!({"b": 5}),
            false,
        ),
    ];

    for (pred, label, entity, expect_hit) in cases {
        let p = bare_packet(vec![rule(label, pred.clone(), "HIT", "info")]);
        let fired = apply(&p, entity).is_some();
        assert_eq!(
            fired, *expect_hit,
            "case `{label}` failed; pred={pred:?}, entity={entity}"
        );
    }
}

#[test]
fn float_comparisons_work() {
    let p = bare_packet(vec![
        rule(
            "fail_low_coverage",
            Predicate::LtF {
                field: "coverage_pct".into(),
                value: 80.0,
            },
            "FAIL: coverage below 80%",
            "fail",
        ),
        rule(
            "pass_high_coverage",
            Predicate::GeF {
                field: "coverage_pct".into(),
                value: 95.0,
            },
            "PASS: coverage above 95%",
            "pass",
        ),
    ]);

    let low = apply(&p, &json!({"coverage_pct": 73.5})).unwrap();
    assert_eq!(low.rule_id, "fail_low_coverage");

    let mid = apply(&p, &json!({"coverage_pct": 85.0}));
    assert!(mid.is_none(), "mid coverage should match neither rule");

    let high = apply(&p, &json!({"coverage_pct": 96.0})).unwrap();
    assert_eq!(high.rule_id, "pass_high_coverage");
}

#[test]
fn classification_infers_from_id_prefix() {
    let map = review_prefix_inference();
    assert_eq!(
        infer_classification("fail_warnings", &map).as_deref(),
        Some("fail")
    );
    assert_eq!(
        infer_classification("flag_readonly_fs", &map).as_deref(),
        Some("flag")
    );
    assert_eq!(
        infer_classification("manual_review_security", &map).as_deref(),
        Some("manual")
    );
    assert_eq!(
        infer_classification("review_contract", &map).as_deref(),
        Some("manual")
    );
    assert_eq!(
        infer_classification("pass_all_clean", &map).as_deref(),
        Some("pass")
    );
    assert_eq!(infer_classification("miscellaneous", &map).as_deref(), None);
}

#[test]
fn compile_infers_classification_from_id_prefix() {
    let (_dir, store) = tmp_packets();
    let params = CompileParams {
        domain: "classification-infer".into(),
        rules: json!([
            {"id": "fail_a", "antecedent": {"op": "True"}, "consequent": "FAIL"},
            {"id": "flag_b", "antecedent": {"op": "True"}, "consequent": "FLAG"},
            {"id": "manual_c", "antecedent": {"op": "True"}, "consequent": "MANUAL"},
            {"id": "pass_d", "antecedent": {"op": "True"}, "consequent": "PASS"},
            // Explicit classification survives — even though id prefix would say Fail
            {"id": "fail_e", "classification": "info", "antecedent": {"op": "True"}, "consequent": "EXPLICIT"},
        ]),
        classification_lattice: None,
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    store.compile(&params).unwrap();

    let all = store.list_all().unwrap();
    let packet = &all[0];
    let classes: Vec<&str> = packet
        .rules
        .iter()
        .map(|r| r.classification.as_str())
        .collect();
    assert_eq!(
        classes,
        vec![
            "fail",   // inferred from fail_
            "flag",   // inferred from flag_
            "manual", // inferred from manual_
            "pass",   // inferred from pass_
            "info",   // explicit "info" preserved even though fail_ prefix would infer "fail"
        ]
    );
}

#[test]
fn compile_rejects_classification_not_in_lattice() {
    let (_dir, store) = tmp_packets();
    let params = CompileParams {
        domain: "bad-class".into(),
        rules: json!([
            {"id": "r1", "classification": "blocker", "antecedent": {"op": "True"}, "consequent": "X"},
        ]),
        classification_lattice: Some(vec!["fail".into(), "pass".into()]),
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    let err = store.compile(&params).unwrap_err().to_string();
    assert!(err.contains("not in packet lattice"), "got: {err}");
}

#[test]
fn prefix_inference_uses_longest_match() {
    // Overlapping prefixes — longer one wins (not BTreeMap iteration order).
    let mut map = BTreeMap::new();
    map.insert("fail_".into(), "fail".into());
    map.insert("fail_critical_".into(), "blocker".into());
    map.insert("flag_".into(), "flag".into());

    assert_eq!(
        infer_classification("fail_critical_foo", &map).as_deref(),
        Some("blocker"),
        "longer prefix `fail_critical_` beats shorter `fail_`"
    );
    assert_eq!(
        infer_classification("fail_normal", &map).as_deref(),
        Some("fail"),
        "only `fail_` matches — picks that"
    );
    assert_eq!(
        infer_classification("flag_readonly", &map).as_deref(),
        Some("flag"),
        "different prefix — picks the matching one"
    );
    assert_eq!(
        infer_classification("unknown_rule", &map).as_deref(),
        None,
        "no prefix match returns None"
    );
}

// ── Phase 4A: quantified collection predicates ──

#[test]
fn forall_vacuous_truth_on_empty_and_missing() {
    let pred = Predicate::ForAll {
        path: "items[*]".into(),
        pred: Box::new(Predicate::AlwaysFalse {}),
    };
    let p = bare_packet(vec![rule("flag_x", pred, "HIT", "flag")]);
    // Missing collection → vacuous true → rule fires even though inner is False.
    assert!(
        apply(&p, &json!({})).is_some(),
        "missing collection: ForAll is true vacuously"
    );
    // Empty collection → also vacuous true.
    assert!(
        apply(&p, &json!({"items": []})).is_some(),
        "empty collection: vacuous true"
    );
}

#[test]
fn forall_fires_when_all_elements_satisfy() {
    // Rule: every tool must have a non-null description.
    let pred = Predicate::ForAll {
        path: "tools[*]".into(),
        pred: Box::new(Predicate::IsNonNull {
            field: "description".into(),
        }),
    };
    let p = bare_packet(vec![rule("ok_all_documented", pred, "ALL_OK", "flag")]);

    let good = json!({"tools": [
        {"name": "a", "description": "does A"},
        {"name": "b", "description": "does B"},
    ]});
    assert!(apply(&p, &good).is_some(), "all documented → rule fires");

    let bad = json!({"tools": [
        {"name": "a", "description": "does A"},
        {"name": "b"},  // missing description
    ]});
    assert!(
        apply(&p, &bad).is_none(),
        "one undocumented → rule does not fire"
    );
}

#[test]
fn exists_false_on_empty_true_on_witness() {
    let pred = Predicate::Exists {
        path: "tools[*]".into(),
        pred: Box::new(Predicate::IsNonNull {
            field: "critical".into(),
        }),
    };
    let p = bare_packet(vec![rule(
        "flag_has_critical",
        pred,
        "HAS_CRITICAL",
        "flag",
    )]);

    // Empty → Exists is false → rule doesn't fire.
    assert!(apply(&p, &json!({"tools": []})).is_none());
    // No witness → false.
    assert!(apply(&p, &json!({"tools": [{"name": "a"}]})).is_none());
    // Witness present → true.
    assert!(apply(&p, &json!({"tools": [{"name": "a", "critical": true}]})).is_some());
}

#[test]
fn forall_primitive_elements_wrapped_as_dollar() {
    // Scalars in the array get wrapped as {"$": value}. Predicate
    // references "$" to read them.
    let pred = Predicate::ForAll {
        path: "tags[*]".into(),
        pred: Box::new(Predicate::IsNonNull { field: "$".into() }),
    };
    let p = bare_packet(vec![rule("flag_tags_present", pred, "OK", "flag")]);

    assert!(
        apply(&p, &json!({"tags": ["a", "b", "c"]})).is_some(),
        "all non-null strings"
    );
    // Primitive null in array → IsNonNull("$") is false → ForAll fails.
    let with_null = json!({"tags": ["a", null, "c"]});
    assert!(
        apply(&p, &with_null).is_none(),
        "any null element breaks ForAll"
    );
}

#[test]
fn forall_vacuous_true_when_runtime_data_not_an_array() {
    // Authoring error (dotted path, bad shape) is rejected at compile.
    // Runtime shape mismatch (entity has non-array where packet expected
    // array) is NOT an authoring error — the packet was correctly shaped,
    // the data just isn't what was expected. ForAll treats this as
    // "no elements to quantify over" → vacuous true, matching math
    // convention. Callers who want loud runtime failure should guard
    // with `IsNonNull{field}` and a separate rule.
    let pred = Predicate::ForAll {
        path: "count[*]".into(),
        pred: Box::new(Predicate::AlwaysTrue {}),
    };
    let p = bare_packet(vec![rule("flag_x", pred, "X", "flag")]);
    assert!(
        apply(&p, &json!({"count": 42})).is_some(),
        "non-array at runtime → vacuous true (not an authoring error)"
    );
}

#[test]
fn count_cmp_all_ops() {
    fn probe(op: CmpOp, value: usize, arr_len: usize) -> bool {
        let pred = Predicate::CountCmp {
            path: "items[*]".into(),
            compare: op,
            value,
        };
        let p = bare_packet(vec![rule("flag_x", pred, "X", "flag")]);
        let arr: Vec<serde_json::Value> = (0..arr_len).map(|i| json!(i)).collect();
        apply(&p, &json!({"items": arr})).is_some()
    }

    // Eq
    assert!(probe(CmpOp::Eq, 3, 3));
    assert!(!probe(CmpOp::Eq, 3, 2));
    // Lt
    assert!(probe(CmpOp::Lt, 5, 3));
    assert!(!probe(CmpOp::Lt, 3, 3));
    // Le
    assert!(probe(CmpOp::Le, 3, 3));
    assert!(probe(CmpOp::Le, 5, 3));
    // Gt
    assert!(probe(CmpOp::Gt, 2, 3));
    assert!(!probe(CmpOp::Gt, 3, 3));
    // Ge
    assert!(probe(CmpOp::Ge, 3, 3));
    assert!(probe(CmpOp::Ge, 2, 3));

    // Missing path → length 0
    let pred = Predicate::CountCmp {
        path: "missing[*]".into(),
        compare: CmpOp::Eq,
        value: 0,
    };
    let p = bare_packet(vec![rule("flag_zero", pred, "X", "flag")]);
    assert!(apply(&p, &json!({})).is_some(), "missing path → count 0");
}

#[test]
fn quantified_predicate_serde_round_trips() {
    // Canonical JSON shape for ForAll.
    let forall_json = json!({
        "op": "ForAll",
        "path": "tools[*]",
        "pred": {"op": "IsNonNull", "field": "description"}
    });
    let p: Predicate = serde_json::from_value(forall_json.clone()).unwrap();
    let back = serde_json::to_value(&p).unwrap();
    assert_eq!(back, forall_json);

    let count_json = json!({
        "op": "CountCmp",
        "path": "tools[*]",
        "compare": "ge",
        "value": 1
    });
    let p: Predicate = serde_json::from_value(count_json.clone()).unwrap();
    let back = serde_json::to_value(&p).unwrap();
    assert_eq!(back, count_json);
}

#[test]
fn compile_accepts_dotted_paths() {
    // Workflow-engine integration accepts dotted paths in
    // quantified-predicate path expressions because ArcContext
    // entities are deeply structured (`vars.labels[*]`,
    // `outputs.Plan.findings[*]`). Empty segments still rejected.
    let (_dir, store) = tmp_packets();
    let ok_params = CompileParams {
        domain: "dotted-path-accepted".into(),
        rules: json!([{
            "id": "flag_x",
            "antecedent": {"op": "ForAll", "path": "config.rules[*]", "pred": {"op": "True"}},
            "consequent": "X"
        }]),
        classification_lattice: None,
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    store
        .compile(&ok_params)
        .expect("dotted-path antecedent must compile");

    let bad_params = CompileParams {
        domain: "dotted-path-empty-seg".into(),
        rules: json!([{
            "id": "flag_x",
            "antecedent": {"op": "ForAll", "path": "config..rules[*]", "pred": {"op": "True"}},
            "consequent": "X"
        }]),
        classification_lattice: None,
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    let err = format!("{:#}", store.compile(&bad_params).unwrap_err());
    assert!(
        err.contains("empty segment"),
        "empty-segment rejection missing: got {err}"
    );
}

#[test]
fn compile_rejects_missing_bracket_suffix() {
    let (_dir, store) = tmp_packets();
    let params = CompileParams {
        domain: "no-bracket".into(),
        rules: json!([{
            "id": "flag_x",
            "antecedent": {"op": "ForAll", "path": "tools", "pred": {"op": "True"}},
            "consequent": "X"
        }]),
        classification_lattice: None,
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    let err = format!("{:#}", store.compile(&params).unwrap_err());
    assert!(
        err.contains("[*]"),
        "missing [*] rejection unclear: got {err}"
    );
}

#[test]
fn compile_rejects_nested_forall() {
    let (_dir, store) = tmp_packets();
    let params = CompileParams {
        domain: "nested".into(),
        rules: json!([{
            "id": "flag_x",
            "antecedent": {
                "op": "ForAll",
                "path": "groups[*]",
                "pred": {
                    "op": "ForAll",
                    "path": "items[*]",
                    "pred": {"op": "True"}
                }
            },
            "consequent": "X"
        }]),
        classification_lattice: None,
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    let err = format!("{:#}", store.compile(&params).unwrap_err());
    assert!(
        err.contains("nested inside ForAll"),
        "nested-ForAll rejection unclear: got {err}"
    );
}

#[test]
fn compile_allows_exists_inside_forall_inside_exists() {
    // The nested-ban is specifically ForAll-inside-ForAll.
    // Exists-inside-ForAll and ForAll-inside-Exists are fine.
    let (_dir, store) = tmp_packets();
    let params = CompileParams {
        domain: "mixed-quantifiers".into(),
        rules: json!([{
            "id": "flag_x",
            "antecedent": {
                "op": "Exists",
                "path": "groups[*]",
                "pred": {
                    "op": "ForAll",
                    "path": "items[*]",
                    "pred": {"op": "IsNonNull", "field": "id"}
                }
            },
            "consequent": "X"
        }]),
        classification_lattice: None,
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    store
        .compile(&params)
        .expect("Exists over ForAll should compile");
}

#[test]
fn verdict_is_highest_priority_not_firing_order() {
    // Lattice: fail > flag > pass. Findings fire in order [flag, pass, fail].
    // Verdict should be "fail" (highest priority), not "flag" (first fired).
    let p = bare_packet(vec![
        rule("flag_first", Predicate::AlwaysTrue {}, "FLAG", "flag"),
        rule("pass_second", Predicate::AlwaysTrue {}, "PASS", "pass"),
        rule("fail_third", Predicate::AlwaysTrue {}, "FAIL", "fail"),
    ]);
    let result = apply_all(&p, &json!({}));
    assert_eq!(result.findings.len(), 3);
    assert_eq!(
        result.verdict,
        Some("fail".to_string()),
        "verdict = highest-priority classification, not firing order"
    );
}

#[test]
fn compile_auth_domain_lattice() {
    // Auth domain: deny wins; anomalies denoted deny_* or anom_*.
    let (_dir, store) = tmp_packets();
    let mut prefix = BTreeMap::new();
    prefix.insert("deny_".into(), "deny".into());
    prefix.insert("allow_".into(), "allow".into());
    prefix.insert("anom_".into(), "deny".into());

    let params = CompileParams {
        domain: "auth".into(),
        rules: json!([
            {"id": "anom_sensitive_resource", "antecedent": {"op": "Eq", "field": "sensitive", "value": true}, "consequent": "DENY"},
            {"id": "allow_admin", "antecedent": {"op": "Eq", "field": "role", "value": "admin"}, "consequent": "ALLOW"},
        ]),
        classification_lattice: Some(vec!["deny".into(), "allow".into()]),
        prefix_inference: Some(prefix),
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    store.compile(&params).unwrap();

    let packet = &store.list_all().unwrap()[0];
    let classes: Vec<&str> = packet
        .rules
        .iter()
        .map(|r| r.classification.as_str())
        .collect();
    assert_eq!(
        classes,
        vec!["deny", "allow"],
        "auth prefixes inferred correctly"
    );

    // Apply in mode=all with a sensitive resource — deny wins.
    let result = apply_all(packet, &json!({"sensitive": true, "role": "admin"}));
    assert_eq!(
        result.verdict,
        Some("deny".to_string()),
        "DENY precedes ALLOW in auth lattice"
    );
}

#[test]
fn apply_all_returns_every_matching_rule() {
    // The critical phase-2 semantic: apply_all evaluates every rule,
    // returns all findings, computes aggregate verdict. This is what
    // the bros called for in thread-0b20e854.
    let p = bare_packet(vec![
        rule("fail_a", Predicate::AlwaysTrue {}, "FAIL: always", "fail"),
        rule("flag_b", Predicate::AlwaysTrue {}, "FLAG: always", "flag"),
        rule(
            "flag_c",
            Predicate::Eq {
                field: "x".into(),
                value: Value::Int(1),
            },
            "FLAG: on x=1",
            "flag",
        ),
        rule("pass_d", Predicate::AlwaysFalse {}, "PASS: never", "pass"),
    ]);

    let result = apply_all(&p, &json!({"x": 1}));
    let fired: Vec<&str> = result.findings.iter().map(|f| f.rule_id.as_str()).collect();
    assert_eq!(
        fired,
        vec!["fail_a", "flag_b", "flag_c"],
        "every matching rule should appear"
    );
    assert_eq!(
        result.verdict,
        Some("fail".to_string()),
        "verdict = highest severity that fired"
    );

    // Entity where only the false rule fires → no findings, no verdict
    let empty = apply_all(&p, &json!({"x": 99}));
    let fired2: Vec<&str> = empty.findings.iter().map(|f| f.rule_id.as_str()).collect();
    assert_eq!(fired2, vec!["fail_a", "flag_b"]); // unconditional rules still fire
}

#[test]
fn apply_all_verdict_follows_severity_precedence() {
    // Fail > Flag > Manual > Pass > Info
    let fail_p = bare_packet(vec![
        rule("pass_x", Predicate::AlwaysTrue {}, "PASS", "pass"),
        rule("manual_y", Predicate::AlwaysTrue {}, "MANUAL", "manual"),
        rule("flag_z", Predicate::AlwaysTrue {}, "FLAG", "flag"),
    ]);
    assert_eq!(
        apply_all(&fail_p, &json!({})).verdict,
        Some("flag".to_string())
    );

    let with_fail = bare_packet(vec![
        rule("pass_x", Predicate::AlwaysTrue {}, "PASS", "pass"),
        rule("fail_y", Predicate::AlwaysTrue {}, "FAIL", "fail"),
        rule("info_z", Predicate::AlwaysTrue {}, "INFO", "info"),
    ]);
    assert_eq!(
        apply_all(&with_fail, &json!({})).verdict,
        Some("fail".to_string())
    );

    // Nothing fires
    let nothing = bare_packet(vec![rule(
        "fail_never",
        Predicate::AlwaysFalse {},
        "NOPE",
        "fail",
    )]);
    assert_eq!(apply_all(&nothing, &json!({})).verdict, None);
}

#[test]
fn apply_tool_all_mode_returns_aggregate() {
    let (_dir, store) = tmp_packets();
    let params = CompileParams {
        domain: "all-mode-test".into(),
        rules: json!([
            {"id": "flag_a", "antecedent": {"op": "True"}, "consequent": "FLAG_A"},
            {"id": "flag_b", "antecedent": {"op": "True"}, "consequent": "FLAG_B"},
            {"id": "pass_c", "antecedent": {"op": "True"}, "consequent": "PASS"},
        ]),
        classification_lattice: None,
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    store.compile(&params).unwrap();
    let all = store.list_all().unwrap();
    let packet = &all[0];

    let out = store
        .apply_tool(&ApplyParams {
            packet_id: packet.id.clone(),
            entity: json!({}),
            mode: Some(ApplyMode::All),
        })
        .unwrap();
    assert!(out.contains("\"mode\": \"all\""));
    assert!(out.contains("\"verdict\": \"flag\""));
    assert!(out.contains("\"finding_count\": 3"));
    assert!(out.contains("flag_a"));
    assert!(out.contains("flag_b"));
    assert!(out.contains("pass_c"));
}

#[test]
fn apply_mode_deserializes_invalid_string_as_error() {
    // Phase-2.5: mode is now a typed enum, so invalid mode strings
    // fail at JSON deserialization rather than reaching apply_tool.
    let bad = json!({"packet_id": "packet-deadbeef", "entity": {}, "mode": "nonsense"});
    let res: std::result::Result<ApplyParams, _> = serde_json::from_value(bad);
    assert!(
        res.is_err(),
        "invalid mode string should fail deserialization"
    );
}

#[test]
fn value_eq_across_int_and_float() {
    // JSON serde can widen ints to floats on round-trip. Rules
    // authored as `Eq{value: 5}` must still match `entity.x = 5.0`.
    assert_eq!(Value::Int(5), Value::Float(5.0));
    assert_eq!(Value::Float(5.0), Value::Int(5));
    assert_ne!(Value::Int(5), Value::Int(6));
    assert_ne!(Value::Float(5.0), Value::Float(6.0));
}

// ── Phase 2.5 tests (convergent adversarial-review fixes) ──

#[test]
fn tri_state_applicability_discriminates_null_vs_missing() {
    let missing = json!({}); // no key
    let nulled = json!({"x": serde_json::Value::Null}); // key present, value null
    let real = json!({"x": 42}); // key present, real value

    let key_exists = bare_packet(vec![rule(
        "flag_ke",
        Predicate::KeyExists { field: "x".into() },
        "KE",
        "flag",
    )]);
    let is_null = bare_packet(vec![rule(
        "flag_null",
        Predicate::IsNull { field: "x".into() },
        "NULL",
        "flag",
    )]);
    let is_non_null = bare_packet(vec![rule(
        "flag_nn",
        Predicate::IsNonNull { field: "x".into() },
        "NN",
        "flag",
    )]);
    let is_missing = bare_packet(vec![rule(
        "flag_miss",
        Predicate::IsMissing { field: "x".into() },
        "M",
        "flag",
    )]);

    // KeyExists: fires when key exists regardless of value
    assert!(apply(&key_exists, &missing).is_none());
    assert!(apply(&key_exists, &nulled).is_some());
    assert!(apply(&key_exists, &real).is_some());

    // IsNull: ONLY when key exists AND value is null
    assert!(apply(&is_null, &missing).is_none());
    assert!(apply(&is_null, &nulled).is_some());
    assert!(apply(&is_null, &real).is_none());

    // IsNonNull: fires when key exists with a non-null value
    assert!(apply(&is_non_null, &missing).is_none());
    assert!(apply(&is_non_null, &nulled).is_none());
    assert!(apply(&is_non_null, &real).is_some());

    // IsMissing: fires ONLY when key absent
    assert!(apply(&is_missing, &missing).is_some());
    assert!(apply(&is_missing, &nulled).is_none());
    assert!(apply(&is_missing, &real).is_none());
}

#[test]
fn classification_info_explicitly_preserved_over_prefix_inference() {
    // The phase-2 bug Codex caught: compile loop upgraded every Info
    // from the id prefix, so explicit `classification: "info"` was erased.
    // Post-phase-3: the field is `classification`, and explicit values
    // still beat id-prefix inference.
    let (_dir, store) = tmp_packets();
    let params = CompileParams {
        domain: "classification-preserve".into(),
        rules: json!([
            // Prefix says FAIL, but caller EXPLICITLY says Info — must preserve.
            {"id": "fail_x", "classification": "info", "antecedent": {"op": "True"}, "consequent": "X"},
            // No classification declared — infer from prefix.
            {"id": "fail_y", "antecedent": {"op": "True"}, "consequent": "Y"},
        ]),
        classification_lattice: None,
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    store.compile(&params).unwrap();
    let packet = &store.list_all().unwrap()[0];
    assert_eq!(
        packet.rules[0].classification, "info",
        "explicit info must survive prefix inference"
    );
    assert_eq!(
        packet.rules[1].classification, "fail",
        "no classification declared → infer from prefix"
    );
}

#[test]
fn fallback_rules_suppressed_when_independent_fires() {
    // Phase-2.5d: Fallback rules fire ONLY when no Independent rule fired.
    // This is how pass_all_clean ought to behave: disappear when real
    // findings exist, present when nothing else has anything to say.
    let p = bare_packet(vec![
        rule(
            "flag_x",
            Predicate::Eq {
                field: "trigger".into(),
                value: Value::Bool(true),
            },
            "FLAG",
            "flag",
        ),
        fallback_rule("pass_catchall", Predicate::AlwaysTrue {}, "PASS", "pass"),
    ]);

    // Trigger fires — fallback is suppressed
    let result = apply_all(&p, &json!({"trigger": true}));
    let fired: Vec<&str> = result.findings.iter().map(|f| f.rule_id.as_str()).collect();
    assert_eq!(
        fired,
        vec!["flag_x"],
        "fallback must be suppressed when Independent fires"
    );
    assert_eq!(result.verdict, Some("flag".to_string()));

    // No trigger — fallback fires
    let result = apply_all(&p, &json!({"trigger": false}));
    let fired: Vec<&str> = result.findings.iter().map(|f| f.rule_id.as_str()).collect();
    assert_eq!(
        fired,
        vec!["pass_catchall"],
        "fallback fires when no Independent matched"
    );
    assert_eq!(result.verdict, Some("pass".to_string()));
}

#[test]
fn fallback_ignored_in_first_mode() {
    // In apply (mode="first"), emit is irrelevant — first-match-wins
    // applies regardless. Fallback rules can still fire.
    let p = bare_packet(vec![
        rule(
            "flag_x",
            Predicate::Eq {
                field: "a".into(),
                value: Value::Int(1),
            },
            "FLAG_X",
            "flag",
        ),
        fallback_rule("pass_catchall", Predicate::AlwaysTrue {}, "PASS", "pass"),
    ]);

    // When flag_x matches, first-match-wins picks it
    let pred = apply(&p, &json!({"a": 1})).unwrap();
    assert_eq!(pred.rule_id, "flag_x");

    // When flag_x doesn't match, pass_catchall (fallback) fires since we
    // still walk the rule list top-to-bottom.
    let pred = apply(&p, &json!({"a": 99})).unwrap();
    assert_eq!(pred.rule_id, "pass_catchall");
}

#[test]
fn apply_mode_enum_serde_lowercase() {
    assert_eq!(
        serde_json::to_value(ApplyMode::First).unwrap(),
        json!("first")
    );
    assert_eq!(serde_json::to_value(ApplyMode::All).unwrap(), json!("all"));
    let m: ApplyMode = serde_json::from_value(json!("all")).unwrap();
    assert_eq!(m, ApplyMode::All);
}

#[test]
fn emit_default_is_independent() {
    // Rule authored without `emit:` field gets Independent.
    let rule_json = json!({
        "id": "fail_x",
        "antecedent": {"op": "True"},
        "consequent": "X",
    });
    let ri: RuleInput = serde_json::from_value(rule_json).unwrap();
    let r = ri
        .materialize(&review_lattice(), &review_prefix_inference())
        .unwrap();
    assert_eq!(r.emit, Emit::Independent);
}

// ── Phase 4B: multi-finding audit ──

fn multi_find_packet() -> Packet {
    bare_packet(vec![
        rule("fail_always", Predicate::AlwaysTrue {}, "FAIL", "fail"),
        rule(
            "flag_on_x",
            Predicate::Eq {
                field: "x".into(),
                value: Value::Int(1),
            },
            "FLAG_X",
            "flag",
        ),
        fallback_rule("pass_catchall", Predicate::AlwaysTrue {}, "PASS", "pass"),
    ])
}

#[test]
fn verify_all_matches_verdict_and_rule_ids() {
    let p = multi_find_packet();
    // Entity with x=1: both fail_always and flag_on_x fire; verdict = fail.
    let dataset = json!([{
        "entity": {"x": 1},
        "expected_verdict": "fail",
        "expected_rule_ids": ["fail_always", "flag_on_x"]
    }]);
    let report = verify_all(&p, &dataset).unwrap();
    assert_eq!(report.total, 1);
    assert_eq!(report.correct, 1);
    assert!(report.mismatches.is_empty());
}

#[test]
fn verify_all_flags_verdict_mismatch() {
    let p = multi_find_packet();
    let dataset = json!([{
        "entity": {"x": 1},
        "expected_verdict": "flag",  // wrong — actual is "fail"
        "expected_rule_ids": ["fail_always", "flag_on_x"]
    }]);
    let report = verify_all(&p, &dataset).unwrap();
    assert_eq!(report.total, 1);
    assert_eq!(report.correct, 0);
    assert_eq!(report.mismatches.len(), 1);
    assert_eq!(report.mismatches[0].check, "verdict");
    assert_eq!(
        report.mismatches[0].expected_verdict.as_deref(),
        Some("flag")
    );
    assert_eq!(report.mismatches[0].actual_verdict.as_deref(), Some("fail"));
}

#[test]
fn verify_all_flags_rule_ids_mismatch() {
    let p = multi_find_packet();
    let dataset = json!([{
        "entity": {"x": 1},
        "expected_verdict": "fail",
        "expected_rule_ids": ["fail_always"]  // missing flag_on_x
    }]);
    let report = verify_all(&p, &dataset).unwrap();
    assert_eq!(report.correct, 0);
    assert_eq!(report.mismatches[0].check, "rule_ids");
}

#[test]
fn verify_all_flags_both_mismatches() {
    let p = multi_find_packet();
    let dataset = json!([{
        "entity": {"x": 1},
        "expected_verdict": "pass",
        "expected_rule_ids": ["nonexistent_rule"]
    }]);
    let report = verify_all(&p, &dataset).unwrap();
    assert_eq!(report.mismatches[0].check, "both");
}

#[test]
fn verify_all_rule_ids_order_invariant() {
    let p = multi_find_packet();
    // Order of expected_rule_ids differs from firing order — still matches.
    let dataset = json!([{
        "entity": {"x": 1},
        "expected_rule_ids": ["flag_on_x", "fail_always"]  // reversed
    }]);
    let report = verify_all(&p, &dataset).unwrap();
    assert_eq!(
        report.correct, 1,
        "rule_ids comparison is a set, not a list"
    );
}

#[test]
fn verify_all_partial_expectations_ok() {
    let p = multi_find_packet();
    // Only expected_verdict set → only verdict checked.
    let dataset = json!([
        {"entity": {"x": 1}, "expected_verdict": "fail"},
        {"entity": {"x": 99}, "expected_verdict": "fail"}  // fail_always still fires
    ]);
    let report = verify_all(&p, &dataset).unwrap();
    assert_eq!(report.correct, 2);
}

#[test]
fn audit_tool_all_mode_via_mcp_surface() {
    let (_dir, store) = tmp_packets();
    store.save_packet(&multi_find_packet()).unwrap();
    let packet_id = multi_find_packet().id;

    let report = store
        .audit_tool(&AuditParams {
            packet_id: packet_id.clone(),
            dataset: json!([{
                "entity": {"x": 1},
                "expected_verdict": "fail",
                "expected_rule_ids": ["fail_always", "flag_on_x"]
            }]),
            mode: Some(ApplyMode::All),
        })
        .unwrap();
    assert!(report.contains("\"mode\": \"all\""));
    assert!(report.contains("\"correct\": 1"));
    assert!(report.contains("\"fidelity\": 1.0"));
}

#[test]
fn emit_fallback_deserializes() {
    let rule_json = json!({
        "id": "pass_clean",
        "antecedent": {"op": "True"},
        "consequent": "PASS",
        "emit": "fallback",
    });
    let ri: RuleInput = serde_json::from_value(rule_json).unwrap();
    let r = ri
        .materialize(&review_lattice(), &review_prefix_inference())
        .unwrap();
    assert_eq!(r.emit, Emit::Fallback);
}

// ── E12-refinement: permissive JSON on tool params ─────────────

#[test]
fn compile_accepts_stringified_rules_array() {
    // Simulates the Codex first-attempt shape: rules passed as a
    // JSON-encoded string instead of a structured array. Compile
    // should succeed without a retry.
    let (_d, packets) = tmp_packets();
    let rules_as_string = serde_json::Value::String(
        r#"[{"id":"r1","antecedent":{"op":"True"},"consequent":"X","classification":"pass","emit":"fallback"}]"#
            .into(),
    );
    let out = packets
        .compile(&CompileParams {
            domain: "coerce-test".into(),
            scope: Some("global".into()),
            project: None,
            classification_lattice: None,
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: rules_as_string,
        })
        .unwrap();
    assert!(out.contains("compiled"));
}

#[test]
fn apply_tool_accepts_stringified_entity() {
    let (_d, packets) = tmp_packets();
    let id = compile_breaking_packet(&packets);
    // Entity passed as string
    let report = packets
        .apply_tool(&ApplyParams {
            packet_id: id,
            entity: serde_json::Value::String(
                r#"{"api_surface_changed": true, "migration_note_present": false}"#.into(),
            ),
            mode: Some(ApplyMode::First),
        })
        .unwrap();
    assert!(report.contains("\"match\": true"));
    assert!(report.contains("breaking_api_no_migration"));
}

// ── Phase 6: StringContains / InRange tests ────────────────────

fn noop_entity() -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::new()
}

#[test]
fn string_contains_matches_case_sensitive() {
    let p = Predicate::StringContains {
        field: "message".into(),
        needle: "OOM".into(),
        case_insensitive: false,
    };
    let yes = serde_json::json!({"message": "worker OOMKilled"})
        .as_object()
        .unwrap()
        .clone();
    let mixed = serde_json::json!({"message": "worker oom event"})
        .as_object()
        .unwrap()
        .clone();
    let no_field = noop_entity();
    let non_string = serde_json::json!({"message": 42})
        .as_object()
        .unwrap()
        .clone();
    assert!(eval_predicate(&p, &yes, &NoopResolver, 0));
    assert!(!eval_predicate(&p, &mixed, &NoopResolver, 0));
    assert!(!eval_predicate(&p, &no_field, &NoopResolver, 0));
    assert!(!eval_predicate(&p, &non_string, &NoopResolver, 0));
}

#[test]
fn string_contains_matches_case_insensitive() {
    let p = Predicate::StringContains {
        field: "message".into(),
        needle: "out of memory".into(),
        case_insensitive: true,
    };
    let yes1 = serde_json::json!({"message": "Out Of Memory allocating"})
        .as_object()
        .unwrap()
        .clone();
    let yes2 = serde_json::json!({"message": "OUT OF MEMORY"})
        .as_object()
        .unwrap()
        .clone();
    let no = serde_json::json!({"message": "disk full"})
        .as_object()
        .unwrap()
        .clone();
    assert!(eval_predicate(&p, &yes1, &NoopResolver, 0));
    assert!(eval_predicate(&p, &yes2, &NoopResolver, 0));
    assert!(!eval_predicate(&p, &no, &NoopResolver, 0));
}

#[test]
fn string_contains_supports_dotted_fields() {
    let p = Predicate::StringContains {
        field: "vars.cheap_output.result".into(),
        needle: "confidence-low".into(),
        case_insensitive: true,
    };
    let yes = serde_json::json!({
        "vars": {
            "cheap_output": {
                "result": "Confidence-Low: needs a reviewer"
            }
        }
    })
    .as_object()
    .unwrap()
    .clone();
    let no = serde_json::json!({
        "vars": {
            "cheap_output": {
                "result": "looks sufficient"
            }
        }
    })
    .as_object()
    .unwrap()
    .clone();
    assert!(eval_predicate(&p, &yes, &NoopResolver, 0));
    assert!(!eval_predicate(&p, &no, &NoopResolver, 0));
}

#[test]
fn string_contains_composes_via_any_for_multi_needle() {
    // The regex-alternation idiom: Any[Contains{a}, Contains{b}].
    let p = Predicate::Any {
        args: vec![
            Predicate::StringContains {
                field: "message".into(),
                needle: "OOM".into(),
                case_insensitive: true,
            },
            Predicate::StringContains {
                field: "message".into(),
                needle: "out of memory".into(),
                case_insensitive: true,
            },
        ],
    };
    let oom = serde_json::json!({"message": "ooMkilled"})
        .as_object()
        .unwrap()
        .clone();
    let prose = serde_json::json!({"message": "Process Ran Out Of Memory"})
        .as_object()
        .unwrap()
        .clone();
    let neither = serde_json::json!({"message": "disk full"})
        .as_object()
        .unwrap()
        .clone();
    assert!(eval_predicate(&p, &oom, &NoopResolver, 0));
    assert!(eval_predicate(&p, &prose, &NoopResolver, 0));
    assert!(!eval_predicate(&p, &neither, &NoopResolver, 0));
}

#[test]
fn in_range_inclusive_both_ends() {
    let p = Predicate::InRange {
        field: "perf_delta_ms".into(),
        min: 1,
        max: 5,
    };
    for (v, want) in [(0, false), (1, true), (3, true), (5, true), (6, false)] {
        let e = serde_json::json!({"perf_delta_ms": v})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            eval_predicate(&p, &e, &NoopResolver, 0),
            want,
            "v={v} expected {want}"
        );
    }
}

#[test]
fn in_range_missing_or_non_int_is_false() {
    let p = Predicate::InRange {
        field: "x".into(),
        min: 0,
        max: 10,
    };
    let missing = noop_entity();
    let str_field = serde_json::json!({"x": "five"})
        .as_object()
        .unwrap()
        .clone();
    assert!(!eval_predicate(&p, &missing, &NoopResolver, 0));
    assert!(!eval_predicate(&p, &str_field, &NoopResolver, 0));
}

#[test]
fn in_range_f_inclusive_and_rejects_non_numeric() {
    let p = Predicate::InRangeF {
        field: "coverage".into(),
        min: 0.8,
        max: 0.95,
    };
    for (v, want) in [
        (0.79, false),
        (0.80, true),
        (0.90, true),
        (0.95, true),
        (0.96, false),
    ] {
        let e = serde_json::json!({"coverage": v})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            eval_predicate(&p, &e, &NoopResolver, 0),
            want,
            "v={v} expected {want}"
        );
    }
    let str_field = serde_json::json!({"coverage": "high"})
        .as_object()
        .unwrap()
        .clone();
    assert!(!eval_predicate(&p, &str_field, &NoopResolver, 0));
}

#[test]
fn compile_accepts_new_predicates_end_to_end() {
    let (_d, packets) = tmp_packets();
    let out = packets
        .compile(&CompileParams {
            domain: "log-triage".into(),
            scope: Some("global".into()),
            project: None,
            classification_lattice: Some(vec![
                "critical".into(),
                "observe".into(),
                "ignore".into(),
            ]),
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([
                {
                    "id": "critical_oom",
                    "classification": "critical",
                    "antecedent": {
                        "op": "Any",
                        "args": [
                            {"op": "StringContains", "field": "message", "needle": "OOM", "case_insensitive": true},
                            {"op": "StringContains", "field": "message", "needle": "out of memory", "case_insensitive": true}
                        ]
                    },
                    "consequent": "CRIT"
                },
                {
                    "id": "observe_elevated_latency",
                    "classification": "observe",
                    "antecedent": {
                        "op": "InRangeF", "field": "p99_ms", "min": 500.0, "max": 2000.0
                    },
                    "consequent": "OBS"
                },
                {
                    "id": "ignore_default",
                    "classification": "ignore",
                    "emit": "fallback",
                    "antecedent": {"op": "True"},
                    "consequent": "IGN"
                }
            ]),
        })
        .unwrap();
    let id = out.split_whitespace().nth(1).unwrap().to_string();
    let pkt = packets.load(&id).unwrap();

    // Apply + multi-finding via bbox_audit dataset.
    assert_eq!(
        apply(&pkt, &json!({"message": "worker OOMKilled at 0x1234"}))
            .unwrap()
            .rule_id,
        "critical_oom"
    );
    assert_eq!(
        apply(&pkt, &json!({"message": "disk full", "p99_ms": 800.0}))
            .unwrap()
            .rule_id,
        "observe_elevated_latency"
    );
    assert_eq!(
        apply(&pkt, &json!({"message": "ok", "p99_ms": 50.0}))
            .unwrap()
            .rule_id,
        "ignore_default"
    );
}

// ── Prefix-inference error clarity test (E14 gotcha) ───────────

#[test]
fn classification_mismatch_error_names_inferred_prefix() {
    // Reproduces the E14/Gemini gotcha: rule id has prefix `review_`
    // which auto-classifies as `manual` via default prefix_inference,
    // but the packet lattice doesn't include `manual`. Error should
    // explicitly name the inference path so the author knows where
    // `manual` came from.
    let (_d, packets) = tmp_packets();
    let err = packets
        .compile(&CompileParams {
            domain: "prefix-trap".into(),
            scope: Some("global".into()),
            project: None,
            classification_lattice: Some(vec![
                "BLOCK".into(),
                "REVIEW".into(),
                "AUTO_APPROVE".into(),
            ]),
            prefix_inference: None, // uses default review_prefix_inference
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([{
                "id": "review_one_red",
                // No explicit classification → inferred from `review_` prefix → "manual"
                "antecedent": {"op": "True"},
                "consequent": "REVIEW"
            }]),
        })
        .unwrap_err();
    let msg = format!("{err:#}");
    // Old error: "rule 'review_one_red' classification 'manual' is not in packet lattice [...]"
    // New error: adds "INFERRED from id prefix 'review_' via the packet's prefix_inference map"
    assert!(
        msg.contains("INFERRED from id prefix 'review_'"),
        "expected error to name inferred prefix path, got: {msg}"
    );
    assert!(
        msg.contains("prefix_inference map"),
        "expected error to mention the inference map, got: {msg}"
    );
}

#[test]
fn classification_mismatch_error_no_inference_hint_when_explicit() {
    // When classification is explicit, error should NOT add the
    // inference hint (it would be misleading).
    let (_d, packets) = tmp_packets();
    let err = packets
        .compile(&CompileParams {
            domain: "explicit-mismatch".into(),
            scope: Some("global".into()),
            project: None,
            classification_lattice: Some(vec!["red".into(), "green".into()]),
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([{
                "id": "bad_rule",
                "classification": "purple", // explicit, not in lattice
                "antecedent": {"op": "True"},
                "consequent": "X"
            }]),
        })
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("'purple'"));
    assert!(
        !msg.contains("INFERRED"),
        "explicit classification should not trigger inference hint, got: {msg}"
    );
}

// ── CountMatches predicate tests ───────────────────────────────

#[test]
fn count_matches_counts_true_subpredicates() {
    // 3 predicates, 2 of which match → count=2
    let p = Predicate::CountMatches {
        args: vec![
            Predicate::Eq {
                field: "a".into(),
                value: Value::Bool(true),
            },
            Predicate::Eq {
                field: "b".into(),
                value: Value::Bool(true),
            },
            Predicate::Eq {
                field: "c".into(),
                value: Value::Bool(true),
            },
        ],
        compare: CmpOp::Ge,
        value: 2,
    };
    let two_true = serde_json::json!({"a": true, "b": true, "c": false})
        .as_object()
        .unwrap()
        .clone();
    let one_true = serde_json::json!({"a": true, "b": false, "c": false})
        .as_object()
        .unwrap()
        .clone();
    let all_true = serde_json::json!({"a": true, "b": true, "c": true})
        .as_object()
        .unwrap()
        .clone();
    assert!(eval_predicate(&p, &two_true, &NoopResolver, 0));
    assert!(!eval_predicate(&p, &one_true, &NoopResolver, 0));
    assert!(eval_predicate(&p, &all_true, &NoopResolver, 0));
}

#[test]
fn count_matches_exactly_k_shape() {
    // "exactly 1 of N" uses CmpOp::Eq
    let p = Predicate::CountMatches {
        args: vec![
            Predicate::Eq {
                field: "a".into(),
                value: Value::Bool(true),
            },
            Predicate::Eq {
                field: "b".into(),
                value: Value::Bool(true),
            },
            Predicate::Eq {
                field: "c".into(),
                value: Value::Bool(true),
            },
        ],
        compare: CmpOp::Eq,
        value: 1,
    };
    for (a, b, c, want) in [
        (false, false, false, false), // 0 true
        (true, false, false, true),   // 1 true
        (true, true, false, false),   // 2 true
        (true, true, true, false),    // 3 true
    ] {
        let e = serde_json::json!({"a": a, "b": b, "c": c})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            eval_predicate(&p, &e, &NoopResolver, 0),
            want,
            "a={a},b={b},c={c}"
        );
    }
}

#[test]
fn count_matches_empty_args_is_zero_count() {
    let p = Predicate::CountMatches {
        args: vec![],
        compare: CmpOp::Eq,
        value: 0,
    };
    let e = noop_entity();
    assert!(eval_predicate(&p, &e, &NoopResolver, 0));

    let p_ge_1 = Predicate::CountMatches {
        args: vec![],
        compare: CmpOp::Ge,
        value: 1,
    };
    assert!(!eval_predicate(&p_ge_1, &e, &NoopResolver, 0));
}

#[test]
fn count_matches_composes_with_apply_for_subpacket_tally() {
    // End-to-end: compose three sub-packet Apply nodes under
    // CountMatches to express "≥ 2 of these 3 sub-packets say red".
    // This is the E14 use case — collapses pairwise enumeration to
    // a single rule.
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

    // Master uses CountMatches over three Apply nodes.
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

    // All green
    let all_green = json!({"a_red": false, "b_red": false, "c_red": false});
    assert_eq!(
        apply_with(&master, &all_green, &packets).unwrap().rule_id,
        "ok_default"
    );
    // One red
    let one_red = json!({"a_red": true, "b_red": false, "c_red": false});
    assert_eq!(
        apply_with(&master, &one_red, &packets).unwrap().rule_id,
        "review_1_red"
    );
    // Two red
    let two_red = json!({"a_red": true, "b_red": true, "c_red": false});
    assert_eq!(
        apply_with(&master, &two_red, &packets).unwrap().rule_id,
        "block_2plus_red"
    );
    // Three red
    let three_red = json!({"a_red": true, "b_red": true, "c_red": true});
    assert_eq!(
        apply_with(&master, &three_red, &packets).unwrap().rule_id,
        "block_2plus_red"
    );
}

#[test]
fn count_matches_compile_validates_nested_apply_refs() {
    // CountMatches containing Apply with a missing packet_id should
    // fail compile (same invariant as top-level Apply).
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

// ── Composition (Apply predicate) tests ────────────────────────

/// Compile a minimal "is_breaking" sub-packet: breaks if api_surface
/// changed AND no migration note. Lattice: [breaking, safe].
fn compile_breaking_packet(packets: &Packets) -> String {
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

#[test]
fn apply_node_composes_sub_packet_verdict() {
    let (_d, packets) = tmp_packets();
    let sub_id = compile_breaking_packet(&packets);

    // Outer packet: REJECT if sub says breaking; else PASS.
    let outer = packets
        .compile(&CompileParams {
            domain: "pr-triage".into(),
            scope: Some("global".into()),
            project: None,
            classification_lattice: None, // use review lattice
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

    // Breaking entity → outer fires fail_breaking via Apply.
    let breaking = json!({
        "api_surface_changed": true,
        "migration_note_present": false,
    });
    let pred = apply_with(&outer_pkt, &breaking, &packets).unwrap();
    assert_eq!(pred.rule_id, "fail_breaking");
    assert_eq!(pred.consequent, Value::String("REJECT".into()));

    // Safe entity → outer falls through to pass_default.
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
    // With a real resolver that doesn't have the packet, eval returns false.
    assert!(!eval_predicate(&pred, &entity, &packets, 0));
    // With NoopResolver, also false.
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
                    "expect": ["brekaing"]  // typo
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

    // Outer's entity schema uses DIFFERENT field names.
    // Map outer's `did_break` → sub's `api_surface_changed`, etc.
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

    // Entity with outer schema → mapping rebinds to sub schema.
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

    // Build a chain: p0 references p1 references p2 ... up to limit.
    // Each packet has one rule: classification "match" fires iff the
    // NEXT packet in the chain says "match".
    //
    // We can construct this by (a) compiling a base packet that
    // always says "match", then (b) wrapping N times. With N >
    // MAX_COMPOSITION_DEPTH, the outermost call should return false
    // because depth exceeded.
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
    // Build a chain longer than the depth limit.
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

    // The outermost packet's eval should trip the depth limit
    // before reaching the base. That means `match_via_next` returns
    // false, the fallback `nomatch_default` fires, and the outer
    // verdict is "nomatch" — NOT "match".
    let outer = packets.load(&current).unwrap();
    let pred = apply_with(&outer, &json!({}), &packets).unwrap();
    assert_eq!(
        pred.classification, "nomatch",
        "depth limit should prevent the outer chain from resolving to 'match'"
    );
}

#[test]
fn load_resolves_domain_prefix_to_latest() {
    let (_dir, store) = tmp_packets();
    // Compile two packets in the same domain.
    let p1 = CompileParams {
        domain: "demo/routing".into(),
        rules: json!([{
            "id": "fail_default",
            "antecedent": {"op": "True"},
            "consequent": "REJECT"
        }]),
        classification_lattice: None,
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    };
    let msg1 = store.compile(&p1).unwrap();
    // Slight delay so created_at differs reliably.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let msg2 = store.compile(&p1).unwrap();
    let id1 = msg1.split_whitespace().nth(1).unwrap();
    let id2 = msg2.split_whitespace().nth(1).unwrap();
    assert_ne!(id1, id2, "two compiles of same domain → distinct ids");
    // domain: prefix resolves to the latest.
    let resolved = store.load("domain:demo/routing").unwrap();
    assert_eq!(resolved.id, id2);
    // Bare id still works.
    let resolved_old = store.load(id1).unwrap();
    assert_eq!(resolved_old.id, id1);
    // Unknown domain errors clearly.
    let err = store.load("domain:does-not-exist").unwrap_err().to_string();
    assert!(err.contains("no packet found for domain"), "got: {err}");
}
