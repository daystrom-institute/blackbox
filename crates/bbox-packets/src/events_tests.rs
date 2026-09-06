use crate::test_support::{compile_breaking_packet, tmp_packets};
use crate::*;
use serde_json::json;
use tempfile::TempDir;

// ── Event logging tests ────────────────────────────────────────

#[test]
fn compile_ok_records_event_with_rules_count_and_refs() {
    let (_d, packets) = tmp_packets();
    let sub_id = compile_breaking_packet(&packets);
    // Outer packet composes the sub-packet — events should capture
    // the reference.
    let _ = packets
        .compile(&CompileParams {
            domain: "pr-triage-with-events".into(),
            scope: Some("global".into()),
            project: None,
            project_id: None,
            classification_lattice: None,
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([
                {
                    "id": "fail_if_breaking",
                    "antecedent": {
                        "op": "Apply",
                        "packet_id": sub_id.clone(),
                        "expect": ["breaking"]
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

    let events = packets
        .list_events(Some("compile"), None, None, None, 50)
        .unwrap();
    // Two compile events: sub + outer
    assert_eq!(events.len(), 2);
    // Newest-first: outer is index 0
    let outer = &events[0];
    assert_eq!(outer.op, "compile");
    assert_eq!(outer.outcome, "ok");
    assert_eq!(outer.domain.as_deref(), Some("pr-triage-with-events"));
    let refs = outer
        .details
        .get("referenced_packets")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].as_str().unwrap(), sub_id);
}

#[test]
fn compile_error_records_event_with_error_message() {
    let (_d, packets) = tmp_packets();
    let _ = packets
        .compile(&CompileParams {
            domain: "broken-compile".into(),
            scope: Some("global".into()),
            project: None,
            project_id: None,
            classification_lattice: None,
            prefix_inference: None,
            rank_table: None,
            threshold_table: None,
            rank_lookup_key: None,
            threshold_lookup_key: None,
            source_ids: None,
            rules: json!([{
                "id": "fail_bad_ref",
                "antecedent": {
                    "op": "Apply",
                    "packet_id": "packet-nonexistent",
                    "expect": ["breaking"]
                },
                "consequent": "REJECT"
            }]),
        })
        .unwrap_err();

    let events = packets
        .list_events(Some("compile"), None, Some("error"), None, 50)
        .unwrap();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.outcome, "error");
    assert_eq!(ev.domain.as_deref(), Some("broken-compile"));
    let err = ev.details.get("error").unwrap().as_str().unwrap();
    assert!(
        err.contains("packet-nonexistent"),
        "error detail missing id: {err}"
    );
}

#[test]
fn apply_tool_records_ok_and_no_match_events() {
    let (_d, packets) = tmp_packets();
    let id = compile_breaking_packet(&packets);

    // Breaking entity — should match.
    let _ = packets
        .apply_tool(&ApplyParams {
            packet_id: id.clone(),
            entity: json!({
                "api_surface_changed": true,
                "migration_note_present": false,
            }),
            mode: Some(ApplyMode::First),
        })
        .unwrap();

    // Safe entity — breaking_api rule won't fire; safe_default
    // (fallback) doesn't fire in mode=first either way because
    // first-match and fallback interact differently; either way
    // we record an event.
    let _ = packets
        .apply_tool(&ApplyParams {
            packet_id: id.clone(),
            entity: json!({
                "api_surface_changed": true,
                "migration_note_present": true,
            }),
            mode: Some(ApplyMode::First),
        })
        .unwrap();

    let events = packets
        .list_events(Some("apply"), Some(&id), None, None, 50)
        .unwrap();
    // Two apply events, one per call.
    assert_eq!(events.len(), 2);
    // At least one ok (the breaking entity fired a rule).
    assert!(events.iter().any(|e| e.outcome == "ok"));
}

#[test]
fn audit_tool_records_fidelity_and_mismatch_count() {
    let (_d, packets) = tmp_packets();
    let id = compile_breaking_packet(&packets);

    let _ = packets
        .audit_tool(&AuditParams {
            packet_id: id.clone(),
            dataset: json!([
                {
                    "entity": {"api_surface_changed": true, "migration_note_present": false},
                    "expected": "BREAKING"
                }
            ]),
            mode: Some(ApplyMode::First),
        })
        .unwrap();

    let events = packets
        .list_events(Some("audit"), Some(&id), None, None, 50)
        .unwrap();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.outcome, "ok");
    let fidelity = ev.details.get("fidelity").unwrap().as_f64().unwrap();
    assert!((fidelity - 1.0).abs() < 1e-6);
}

#[test]
fn log_gap_records_event_with_details() {
    let (_d, packets) = tmp_packets();
    let _ = packets
        .log_gap(
            "wanted to flag requests exceeding 10 per minute per user",
            Some("rate-limit"),
            Some("CountInWindow{path: 'requests[*]', window_seconds: 60, gt: 10}"),
            Some("prose rubric in reviewer instructions"),
            Some("RateCmp or Within{temporal}"),
        )
        .unwrap();

    let events = packets
        .list_events(Some("gap"), None, None, None, 10)
        .unwrap();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.op, "gap");
    assert_eq!(ev.outcome, "logged");
    assert_eq!(ev.domain.as_deref(), Some("rate-limit"));
    let desc = ev.details.get("description").unwrap().as_str().unwrap();
    assert!(desc.contains("10 per minute"));
    assert_eq!(
        ev.details
            .get("ast_feature_requested")
            .unwrap()
            .as_str()
            .unwrap(),
        "RateCmp or Within{temporal}"
    );
}

#[test]
fn log_gap_rejects_empty_description() {
    let (_d, packets) = tmp_packets();
    let err = packets.log_gap("", None, None, None, None).unwrap_err();
    assert!(format!("{err:#}").contains("description"));
}

#[test]
fn gap_dedupe_key_uses_ast_feature_over_description() {
    let key = Packets::gap_dedupe_key(Some("auth"), Some("StringMatches"), "fallback text");
    assert_eq!(key, "packet_ast/auth/StringMatches");
}

#[test]
fn gap_dedupe_key_slugifies_description_when_no_feature() {
    let key = Packets::gap_dedupe_key(None, None, "wanted rate limiting!!");
    assert_eq!(key, "packet_ast/unknown/wanted-rate-limiting");
}

#[test]
fn build_gap_note_body_is_valid_json() {
    let dir = TempDir::new().unwrap();
    let packets = Packets::open(dir.path()).unwrap();
    let ev = packets
        .log_gap("test gap", Some("x"), None, None, Some("Y"))
        .unwrap();
    let params = GapParams {
        description: "test gap".into(),
        domain: Some("x".into()),
        attempted_sketch: Some("sketch".into()),
        fallback_used: Some("fallback".into()),
        ast_feature_requested: Some("Y".into()),
    };
    let body = Packets::build_gap_note_body(&ev, &params, "packet_ast/x/Y");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["type"], "blackbox.gap_note.v1");
    assert_eq!(v["gap_kind"], "packet_ast");
    assert_eq!(v["domain"], "x");
    assert_eq!(v["missing_primitive"], "Y");
    assert_eq!(v["wanted_capability"], "test gap");
    assert_eq!(v["notes"], "sketch");
    assert_eq!(v["fallback_used"], "fallback");
    assert_eq!(v["impact"], "medium");
    assert_eq!(v["blocking_level"], "workaround_available");
    assert!(v["evidence"].as_array().unwrap().len() == 1);
}

#[test]
fn list_events_filters_and_newest_first() {
    let (_d, packets) = tmp_packets();
    // Fire a mix of operations.
    let id = compile_breaking_packet(&packets);
    let _ = packets.log_gap("gap A", None, None, None, None).unwrap();
    let _ = packets
        .apply_tool(&ApplyParams {
            packet_id: id.clone(),
            entity: json!({"api_surface_changed": false}),
            mode: Some(ApplyMode::First),
        })
        .unwrap();
    let _ = packets.log_gap("gap B", None, None, None, None).unwrap();

    // All events, default ordering (newest-first).
    let all = packets.list_events(None, None, None, None, 100).unwrap();
    assert!(!all.is_empty());
    // Newest first — last logged gap ("gap B") should be first.
    let first_gap = all
        .iter()
        .find(|e| e.op == "gap")
        .expect("at least one gap event");
    assert!(
        first_gap
            .details
            .get("description")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("gap B")
    );

    // Filter by op=gap — should be two.
    let gaps = packets
        .list_events(Some("gap"), None, None, None, 100)
        .unwrap();
    assert_eq!(gaps.len(), 2);

    // Limit honored.
    let limited = packets.list_events(None, None, None, None, 1).unwrap();
    assert_eq!(limited.len(), 1);
}
#[test]
fn events_page_continues_with_cursors_and_reports_live_semantics() {
    let (_d, packets) = tmp_packets();
    packets.log_gap("gap A", None, None, None, None).unwrap();
    packets.log_gap("gap B", None, None, None, None).unwrap();

    let params: EventsParams = serde_json::from_value(json!({"op": "gap", "limit": 1})).unwrap();
    let first = packets.events_page(&params).unwrap();
    assert_eq!(first["count"], 1);
    assert_eq!(first["total"], 2);
    assert_eq!(first["offset"], 0);
    assert_eq!(first["order"], "timestamp_file_order_desc");
    assert_eq!(
        first["events"][0]["details"]["description"], "gap B",
        "newest event must come first"
    );
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    packets
        .log_gap("appended gap C", None, None, None, None)
        .unwrap();

    let mut continuation: EventsParams =
        serde_json::from_value(json!({"op": "gap", "limit": 1})).unwrap();
    continuation.cursor = Some(cursor);
    let second = packets.events_page(&continuation).unwrap();
    assert_eq!(second["total"], 3);
    assert_eq!(second["offset"], 2);
    assert_eq!(
        second["events"][0]["details"]["description"], "gap A",
        "continuation must advance without overlap"
    );
    assert_eq!(second["next_cursor"], serde_json::Value::Null);
}

#[test]
fn events_page_counts_malformed_lines_and_rejects_shrunk_cursor() {
    let (directory, packets) = tmp_packets();
    packets.log_gap("gap A", None, None, None, None).unwrap();
    packets.log_gap("gap B", None, None, None, None).unwrap();

    let params: EventsParams = serde_json::from_value(json!({"op": "gap", "limit": 1})).unwrap();
    let first = packets.events_page(&params).unwrap();
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();

    let event_log = directory.path().join("events.jsonl");
    let raw = std::fs::read_to_string(&event_log).unwrap();
    std::fs::write(&event_log, format!("{raw}not-json\n")).unwrap();
    let malformed = packets.events_page(&params).unwrap();
    assert_eq!(malformed["malformed_lines_omitted"], 1);
    assert_eq!(malformed["total"], 2);

    let mut continuation: EventsParams =
        serde_json::from_value(json!({"op": "gap", "limit": 1})).unwrap();
    continuation.cursor = Some(cursor);
    let shrunk = format!("{}\n", raw.lines().next().unwrap());
    std::fs::write(&event_log, shrunk).unwrap();
    let error = packets.events_page(&continuation).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("error.stale_event_cursor"), "{message}");
}
#[test]
fn large_event_pages_remain_bounded_and_recover_the_tail() {
    let (directory, packets) = tmp_packets();
    let event_log = directory.path().join("events.jsonl");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&event_log).unwrap();
        for sequence in 0..501 {
            let event =
                PacketEvent::now("gap", "logged").with_details(json!({"sequence": sequence}));
            writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        }
    }

    let params: EventsParams = serde_json::from_value(json!({"op": "gap", "limit": 1000})).unwrap();
    let first = packets.events_page(&params).unwrap();
    assert_eq!(first["limit"], 500);
    assert_eq!(first["total"], 501);
    assert_eq!(first["events"][0]["details"]["sequence"], 500);
    let mut first_count = first["count"].as_u64().unwrap();
    assert!(
        first_count > 0 && first_count < 500,
        "byte budget must bound the requested 500 rows"
    );
    assert_eq!(first["next_offset"], first_count);
    let mut cursor = first["next_cursor"].as_str().map(str::to_owned);
    let mut newest_remaining = 500u64;

    let mut continuation: EventsParams =
        serde_json::from_value(json!({"op": "gap", "limit": 1000})).unwrap();
    while let Some(next_cursor) = cursor.clone() {
        continuation.cursor = Some(next_cursor);
        let tail = packets.events_page(&continuation).unwrap();
        let count = tail["count"].as_u64().unwrap();
        assert!(count > 0);
        let sequence = tail["events"][0]["details"]["sequence"].as_u64().unwrap();
        assert!(sequence < newest_remaining);
        newest_remaining =
            tail["events"].as_array().unwrap().last().unwrap()["details"]["sequence"]
                .as_u64()
                .unwrap();
        first_count += count;
        cursor = tail["next_cursor"].as_str().map(str::to_owned);
    }
    assert_eq!(first_count, 501);
    assert_eq!(newest_remaining, 0);
}

#[test]
fn event_cursors_reject_filter_changes_and_same_size_rewrites() {
    let (directory, packets) = tmp_packets();
    packets.log_gap("gap A", None, None, None, None).unwrap();
    packets.log_gap("gap B", None, None, None, None).unwrap();
    let params: EventsParams = serde_json::from_value(json!({"op":"gap", "limit":1})).unwrap();
    let first = packets.events_page(&params).unwrap();
    for extra in [json!({"outcome":"logged"}), json!({"detail":true})] {
        let mut next = json!({"op":"gap", "limit":1, "cursor":first["next_cursor"]});
        next.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let error = packets
            .events_page(&serde_json::from_value(next).unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("stale_event_cursor"));
    }
    let event_log = directory.path().join("events.jsonl");
    let raw = std::fs::read_to_string(&event_log).unwrap();
    std::fs::write(&event_log, raw.replace("gap A", "gap Z")).unwrap();
    let next =
        serde_json::from_value(json!({"op":"gap", "limit":1, "cursor":first["next_cursor"]}))
            .unwrap();
    assert!(
        packets
            .events_page(&next)
            .unwrap_err()
            .to_string()
            .contains("stale_event_cursor")
    );
}
