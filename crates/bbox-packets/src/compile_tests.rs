use std::collections::BTreeMap;

use serde_json::json;

use super::super::test_support::tmp_packets;
use super::super::{
    CompileParams, Emit, Packet, Predicate, Rule, Value, apply, apply_all, default_rank_lookup_key,
    default_threshold_lookup_key, packet_matches_query, packet_summary,
};

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
    assert!(packet_matches_query(&pr_triage_new, "PR-triage")); // domain hit (case-insensitive)
    assert!(packet_matches_query(&pr_triage_new, "breaking")); // rule id hit
    assert!(packet_matches_query(&pr_triage_new, "FAIL")); // classification lattice hit
    assert!(packet_matches_query(&auth_matrix, "33333333")); // id hit
    assert!(!packet_matches_query(&auth_matrix, "retry")); // miss
    // empty query degenerates to false - caller is responsible for
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
    let preview = summary["rule_ids_preview"].as_array().unwrap();
    assert_eq!(preview.len(), 3);
    assert_eq!(preview[0], "breaking_api_change");

    let (_dir, store) = tmp_packets();
    store.save_packet(&pr_triage_old).unwrap();
    store.save_packet(&pr_triage_new).unwrap();
    store.save_packet(&auth_matrix).unwrap();

    // list_all is newest-first across all packets, and callers can do
    // latest-per-domain by keeping the first domain they see.
    let listed = store.list_all().unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].id, "packet-22222222");
    assert_eq!(listed[1].id, "packet-33333333");
    assert_eq!(listed[2].id, "packet-11111111");

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

#[test]
fn classification_info_explicitly_preserved_over_prefix_inference() {
    // The phase-2 bug Codex caught: compile loop upgraded every Info
    // from the id prefix, so explicit `classification: "info"` was erased.
    // Post-phase-3: the field is `classification`, and explicit values
    // still beat id-prefix inference.
    let (_dir, store) = tmp_packets();
    let params = compile_params(
        "classification-preserve",
        json!([
            // Prefix says FAIL, but caller EXPLICITLY says Info - must preserve.
            {"id": "fail_x", "classification": "info", "antecedent": {"op": "True"}, "consequent": "X"},
            // No classification declared - infer from prefix.
            {"id": "fail_y", "antecedent": {"op": "True"}, "consequent": "Y"},
        ]),
    );
    store.compile(&params).unwrap();
    let packet = &store.list_all().unwrap()[0];
    assert_eq!(
        packet.rules[0].classification, "info",
        "explicit info must survive prefix inference"
    );
    assert_eq!(
        packet.rules[1].classification, "fail",
        "no classification declared -> infer from prefix"
    );
}

// ── compile_idempotent / generation / duplicate GC ───────────────────

use super::super::CompileOutcome;

fn extract_packet_id(compile_msg: &str) -> String {
    // "Packet packet-xxxxxxxx compiled (...)"
    compile_msg
        .split_whitespace()
        .nth(1)
        .expect("compile message carries the packet id")
        .to_string()
}

#[test]
fn compile_idempotent_skips_identical_content() {
    let (_dir, store) = tmp_packets();
    let params = compile_params(
        "idem-test",
        json!([
            {"id": "pass_ok", "antecedent": {"op": "True"}, "consequent": "OK"}
        ]),
    );

    let first = store.compile_idempotent(&params).unwrap();
    let CompileOutcome::Created(first_id) = first else {
        panic!("first compile must create");
    };
    assert_eq!(
        store.compile_idempotent(&params).unwrap(),
        CompileOutcome::UnchangedExisting(first_id),
        "identical re-compile must reuse the existing packet"
    );
    assert_eq!(store.list_all().unwrap().len(), 1);

    // Changed content must still write a new packet.
    let changed = compile_params(
        "idem-test",
        json!([
            {"id": "pass_ok", "antecedent": {"op": "True"}, "consequent": "CHANGED"}
        ]),
    );
    assert!(matches!(
        store.compile_idempotent(&changed).unwrap(),
        CompileOutcome::Created(_)
    ));
    assert_eq!(store.list_all().unwrap().len(), 2);
}

#[test]
fn generation_bumps_on_writes_not_on_idempotent_skip() {
    let (_dir, store) = tmp_packets();
    let params = compile_params(
        "gen-test",
        json!([
            {"id": "pass_ok", "antecedent": {"op": "True"}, "consequent": "OK"}
        ]),
    );
    let g0 = store.generation();
    store.compile(&params).unwrap();
    let g1 = store.generation();
    assert!(g1 > g0, "save must bump the generation");

    store.compile_idempotent(&params).unwrap();
    assert_eq!(
        store.generation(),
        g1,
        "idempotent skip must not bump the generation"
    );

    store.remove_domain("gen-test").unwrap();
    assert!(store.generation() > g1, "remove must bump the generation");
}

#[test]
fn gc_duplicate_packets_keeps_newest_copy() {
    let (_dir, store) = tmp_packets();
    let params = compile_params(
        "dup-test",
        json!([
            {"id": "pass_ok", "antecedent": {"op": "True"}, "consequent": "OK"}
        ]),
    );
    // created_at has millisecond precision; space the copies out so
    // "newest" is well-defined.
    store.compile(&params).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    store.compile(&params).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let newest_id = extract_packet_id(&store.compile(&params).unwrap());

    let dry = store.gc_duplicate_packets(false).unwrap();
    assert!(!dry.applied);
    assert_eq!(dry.scanned, 3);
    assert_eq!(dry.deleted, 2);
    assert_eq!(
        store.list_all().unwrap().len(),
        3,
        "dry-run must not delete"
    );

    let report = store.gc_duplicate_packets(true).unwrap();
    assert!(report.applied);
    assert_eq!(report.deleted, 2);
    assert_eq!(report.per_domain.get("dup-test"), Some(&2));
    let rest = store.list_all().unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].id, newest_id, "the newest copy survives");
}

#[test]
fn gc_protects_apply_referenced_duplicates() {
    let (_dir, store) = tmp_packets();
    let sub_params = compile_params(
        "gc-sub",
        json!([
            {"id": "fail_bad", "antecedent": {"op": "Eq", "field": "x", "value": "bad"}, "consequent": "NO"}
        ]),
    );
    let old_id = extract_packet_id(&store.compile(&sub_params).unwrap());
    std::thread::sleep(std::time::Duration::from_millis(5));
    store.compile(&sub_params).unwrap();

    // Reference the OLD duplicate from another packet's Apply antecedent.
    let referrer = compile_params(
        "gc-referrer",
        json!([
            {
                "id": "fail_sub_failed",
                "antecedent": {"op": "Apply", "packet_id": old_id, "expect": ["fail"]},
                "consequent": "STOP"
            }
        ]),
    );
    store.compile(&referrer).unwrap();

    let report = store.gc_duplicate_packets(true).unwrap();
    assert_eq!(
        report.deleted, 0,
        "the only duplicate candidate is Apply-referenced and must survive"
    );
    assert_eq!(report.protected_by_refs, 1);
    // The referenced packet still resolves.
    assert!(store.load(&old_id).is_ok());
}

#[test]
fn gc_sweeps_orphaned_lock_files() {
    let (dir, store) = tmp_packets();
    let params = compile_params(
        "lock-test",
        json!([
            {"id": "pass_ok", "antecedent": {"op": "True"}, "consequent": "OK"}
        ]),
    );
    store.compile(&params).unwrap();
    // remove_domain deletes packet jsons but leaves .json.lock siblings.
    store.remove_domain("lock-test").unwrap();
    let global = dir.path().join("global");
    let orphans = std::fs::read_dir(&global)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".json.lock")
        })
        .count();
    assert!(orphans > 0, "precondition: remove_domain leaves lock files");

    let report = store.gc_duplicate_packets(true).unwrap();
    assert_eq!(report.orphan_locks_removed, orphans);
    let remaining = std::fs::read_dir(&global)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".json.lock")
        })
        .count();
    assert_eq!(remaining, 0);
}
