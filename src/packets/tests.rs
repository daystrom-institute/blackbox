use super::test_support::{bare_packet, compile_breaking_packet, fallback_rule, rule, tmp_packets};

use super::{ApplyMode, ApplyParams, CmpOp, CompileParams, Predicate, Value, apply, apply_all};
use serde_json::json;

// ── Phase 2 tests: applicability, field-vs-field, float, severity, evaluate-all ──

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

// ── E12-refinement: permissive JSON on tool params ─────────────

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
