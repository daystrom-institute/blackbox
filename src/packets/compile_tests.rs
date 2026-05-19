use std::collections::BTreeMap;

use serde_json::json;

use super::super::test_support::tmp_packets;
use super::super::{CompileParams, apply, apply_all};

fn compile_params(domain: &str, rules: serde_json::Value) -> CompileParams {
    CompileParams {
        domain: domain.into(),
        rules,
        classification_lattice: None,
        prefix_inference: None,
        rank_table: None,
        threshold_table: None,
        rank_lookup_key: None,
        threshold_lookup_key: None,
        source_ids: None,
        scope: Some("global".into()),
        project: None,
    }
}

#[test]
fn compile_tool_happy_path() {
    let (_dir, store) = tmp_packets();
    let params = compile_params(
        "minimal-test",
        json!([
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
    );
    let msg = store.compile(&params).unwrap();
    assert!(msg.contains("Packet packet-"));
    assert!(msg.contains("minimal-test"));

    let packets = store.list_all().unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].rules.len(), 2);
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
            // Explicit classification survives even though the id prefix would say fail.
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
    assert_eq!(classes, vec!["fail", "flag", "manual", "pass", "info",]);
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

    let result = apply_all(packet, &json!({"sensitive": true, "role": "admin"}));
    assert_eq!(
        result.verdict,
        Some("deny".to_string()),
        "DENY precedes ALLOW in auth lattice"
    );
}

#[test]
fn compile_accepts_dotted_paths() {
    let (_dir, store) = tmp_packets();
    let ok_params = compile_params(
        "dotted-path-accepted",
        json!([{
            "id": "flag_x",
            "antecedent": {"op": "ForAll", "path": "config.rules[*]", "pred": {"op": "True"}},
            "consequent": "X"
        }]),
    );
    store
        .compile(&ok_params)
        .expect("dotted-path antecedent must compile");

    let bad_params = compile_params(
        "dotted-path-empty-seg",
        json!([{
            "id": "flag_x",
            "antecedent": {"op": "ForAll", "path": "config..rules[*]", "pred": {"op": "True"}},
            "consequent": "X"
        }]),
    );
    let err = format!("{:#}", store.compile(&bad_params).unwrap_err());
    assert!(
        err.contains("empty segment"),
        "empty-segment rejection missing: got {err}"
    );
}

#[test]
fn compile_rejects_missing_bracket_suffix() {
    let (_dir, store) = tmp_packets();
    let params = compile_params(
        "no-bracket",
        json!([{
            "id": "flag_x",
            "antecedent": {"op": "ForAll", "path": "tools", "pred": {"op": "True"}},
            "consequent": "X"
        }]),
    );
    let err = format!("{:#}", store.compile(&params).unwrap_err());
    assert!(
        err.contains("[*]"),
        "missing [*] rejection unclear: got {err}"
    );
}

#[test]
fn compile_rejects_nested_forall() {
    let (_dir, store) = tmp_packets();
    let params = compile_params(
        "nested",
        json!([{
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
    );
    let err = format!("{:#}", store.compile(&params).unwrap_err());
    assert!(
        err.contains("nested inside ForAll"),
        "nested-ForAll rejection unclear: got {err}"
    );
}

#[test]
fn compile_allows_exists_inside_forall_inside_exists() {
    let (_dir, store) = tmp_packets();
    let params = compile_params(
        "mixed-quantifiers",
        json!([{
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
    );
    store
        .compile(&params)
        .expect("Exists over ForAll should compile");
}

#[test]
fn compile_accepts_stringified_rules_array() {
    let (_d, packets) = tmp_packets();
    let rules_as_string = serde_json::Value::String(
        r#"[{"id":"r1","antecedent":{"op":"True"},"consequent":"X","classification":"pass","emit":"fallback"}]"#
            .into(),
    );
    let out = packets
        .compile(&compile_params("coerce-test", rules_as_string))
        .unwrap();
    assert!(out.contains("compiled"));
}

#[test]
fn compile_accepts_new_predicates_end_to_end() {
    let (_d, packets) = tmp_packets();
    let mut params = compile_params(
        "log-triage",
        json!([
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
    );
    params.classification_lattice =
        Some(vec!["critical".into(), "observe".into(), "ignore".into()]);
    let out = packets.compile(&params).unwrap();
    let id = out.split_whitespace().nth(1).unwrap().to_string();
    let pkt = packets.load(&id).unwrap();

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

#[test]
fn classification_mismatch_error_names_inferred_prefix() {
    let (_d, packets) = tmp_packets();
    let mut params = compile_params(
        "prefix-trap",
        json!([{
            "id": "review_one_red",
            "antecedent": {"op": "True"},
            "consequent": "REVIEW"
        }]),
    );
    params.classification_lattice =
        Some(vec!["BLOCK".into(), "REVIEW".into(), "AUTO_APPROVE".into()]);
    let err = packets.compile(&params).unwrap_err();
    let msg = format!("{err:#}");
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
    let (_d, packets) = tmp_packets();
    let mut params = compile_params(
        "explicit-mismatch",
        json!([{
            "id": "bad_rule",
            "classification": "purple",
            "antecedent": {"op": "True"},
            "consequent": "X"
        }]),
    );
    params.classification_lattice = Some(vec!["red".into(), "green".into()]);
    let err = packets.compile(&params).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("'purple'"));
    assert!(
        !msg.contains("INFERRED"),
        "explicit classification should not trigger inference hint, got: {msg}"
    );
}
