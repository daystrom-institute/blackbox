use crate::packets::test_support::{compile_breaking_packet, tmp_packets};
use crate::packets::*;
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
fn companion_gap_note_created() {
    let dir = TempDir::new().unwrap();
    let packets = Packets::open(dir.path()).unwrap();
    let notes_path = dir.path().join("notes.json");
    let notes = crate::notes::Notes::open(&notes_path).unwrap();
    let notes_lock = parking_lot::RwLock::new(notes);

    let ev = packets
        .log_gap(
            "wanted regex matching on log messages",
            Some("auth"),
            Some("CountInWindow{...}"),
            Some("prose rubric"),
            Some("StringMatches"),
        )
        .unwrap();

    let params = GapParams {
        description: "wanted regex matching on log messages".into(),
        domain: Some("auth".into()),
        attempted_sketch: Some("CountInWindow{...}".into()),
        fallback_used: Some("prose rubric".into()),
        ast_feature_requested: Some("StringMatches".into()),
    };

    let warning = Packets::emit_companion_gap_note(&notes_lock, &ev, &params);
    assert!(warning.is_none(), "should succeed without warning");

    let notes = notes_lock.read();
    assert_eq!(notes.all().len(), 1);
    let note = &notes.all()[0];
    assert_eq!(note.kind, crate::notes::NoteKind::Followup);

    let parsed = crate::notes::GapNoteView::parse(note).unwrap();
    assert_eq!(parsed.gap_kind.as_deref(), Some("packet_ast"));
    assert_eq!(parsed.domain.as_deref(), Some("auth"));
    assert_eq!(
        parsed.dedupe_key.as_deref(),
        Some("packet_ast/auth/StringMatches")
    );
    assert_eq!(
        parsed.wanted_capability.as_deref(),
        Some("wanted regex matching on log messages")
    );
}

#[test]
fn companion_gap_note_deduplicates() {
    let dir = TempDir::new().unwrap();
    let packets = Packets::open(dir.path()).unwrap();
    let notes_path = dir.path().join("notes.json");
    let notes = crate::notes::Notes::open(&notes_path).unwrap();
    let notes_lock = parking_lot::RwLock::new(notes);

    let params = GapParams {
        description: "wanted regex".into(),
        domain: Some("auth".into()),
        attempted_sketch: None,
        fallback_used: None,
        ast_feature_requested: Some("StringMatches".into()),
    };

    let ev = packets
        .log_gap(
            "wanted regex",
            Some("auth"),
            None,
            None,
            Some("StringMatches"),
        )
        .unwrap();
    let _ = Packets::emit_companion_gap_note(&notes_lock, &ev, &params);
    assert_eq!(notes_lock.read().all().len(), 1);

    let ev2 = packets
        .log_gap(
            "wanted regex",
            Some("auth"),
            None,
            None,
            Some("StringMatches"),
        )
        .unwrap();
    let _ = Packets::emit_companion_gap_note(&notes_lock, &ev2, &params);
    assert_eq!(
        notes_lock.read().all().len(),
        1,
        "second call should not create a duplicate"
    );
}

#[test]
fn companion_gap_note_deduplicates_acknowledged() {
    let dir = TempDir::new().unwrap();
    let packets = Packets::open(dir.path()).unwrap();
    let notes_path = dir.path().join("notes.json");
    let notes = crate::notes::Notes::open(&notes_path).unwrap();
    let notes_lock = parking_lot::RwLock::new(notes);

    let params = GapParams {
        description: "no rate predicate".into(),
        domain: Some("rate-limit".into()),
        attempted_sketch: None,
        fallback_used: None,
        ast_feature_requested: Some("RateCmp".into()),
    };

    let ev = packets
        .log_gap(
            "no rate predicate",
            Some("rate-limit"),
            None,
            None,
            Some("RateCmp"),
        )
        .unwrap();
    let _ = Packets::emit_companion_gap_note(&notes_lock, &ev, &params);
    let note_id = notes_lock.read().all()[0].id.clone();

    notes_lock
        .write()
        .resolve(&crate::notes::NoteResolveParams {
            id: note_id,
            resolution: "acknowledged".into(),
            note: None,
        })
        .unwrap();

    let ev2 = packets
        .log_gap(
            "no rate predicate",
            Some("rate-limit"),
            None,
            None,
            Some("RateCmp"),
        )
        .unwrap();
    let _ = Packets::emit_companion_gap_note(&notes_lock, &ev2, &params);
    assert_eq!(
        notes_lock.read().all().len(),
        1,
        "acknowledged gap note should block new companion"
    );
}

#[test]
fn companion_gap_note_allows_after_addressed() {
    let dir = TempDir::new().unwrap();
    let packets = Packets::open(dir.path()).unwrap();
    let notes_path = dir.path().join("notes.json");
    let notes = crate::notes::Notes::open(&notes_path).unwrap();
    let notes_lock = parking_lot::RwLock::new(notes);

    let params = GapParams {
        description: "no temporal window".into(),
        domain: Some("retry".into()),
        attempted_sketch: None,
        fallback_used: None,
        ast_feature_requested: Some("Within{temporal}".into()),
    };

    let ev = packets
        .log_gap(
            "no temporal window",
            Some("retry"),
            None,
            None,
            Some("Within{temporal}"),
        )
        .unwrap();
    let _ = Packets::emit_companion_gap_note(&notes_lock, &ev, &params);
    let note_id = notes_lock.read().all()[0].id.clone();

    notes_lock
        .write()
        .resolve(&crate::notes::NoteResolveParams {
            id: note_id,
            resolution: "addressed".into(),
            note: Some("implemented RateCmp".into()),
        })
        .unwrap();

    let ev2 = packets
        .log_gap(
            "no temporal window",
            Some("retry"),
            None,
            None,
            Some("Within{temporal}"),
        )
        .unwrap();
    let _ = Packets::emit_companion_gap_note(&notes_lock, &ev2, &params);
    assert_eq!(
        notes_lock.read().all().len(),
        2,
        "addressed gap note should allow new companion"
    );
}

#[test]
fn packet_event_survives_note_failure() {
    let dir = TempDir::new().unwrap();
    let packets = Packets::open(dir.path()).unwrap();
    let broken_path = dir.path().join("notes.json");
    let notes = crate::notes::Notes::open(&broken_path).unwrap();
    std::fs::create_dir(&broken_path).unwrap();
    let notes_lock = parking_lot::RwLock::new(notes);

    let params = GapParams {
        description: "some gap".into(),
        domain: None,
        attempted_sketch: None,
        fallback_used: None,
        ast_feature_requested: Some("Foo".into()),
    };

    let ev = packets
        .log_gap("some gap", None, None, None, Some("Foo"))
        .unwrap();

    let warning = Packets::emit_companion_gap_note(&notes_lock, &ev, &params);
    assert!(
        warning.is_some(),
        "note creation should fail on unwritable path"
    );
    assert!(warning.unwrap().contains("companion gap note failed"));

    let events = packets
        .list_events(Some("gap"), None, None, None, 10)
        .unwrap();
    assert_eq!(events.len(), 1, "packet event must survive note failure");
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
